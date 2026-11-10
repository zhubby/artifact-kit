package client

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/cookiejar"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"

	pb "github.com/artifact-kit/agent/gen/agentv1"
	"github.com/artifact-kit/agent/internal/config"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// The independent Go client holds a real PostgreSQL row lock, observes the
// heartbeat waiting on it, then changes node authorization before releasing it.
// A renewal must use the authorization that is current when it writes the lease.
func TestLiveRustGoProtocolLeaseFencing(test *testing.T) {
	if os.Getenv("AK_LIVE_TESTS") != "1" {
		test.Skip("requires live Rust control, PostgreSQL, psql and acceptance fixtures")
	}
	for _, change := range []string{"unchanged", "revoked", "certificate-expired", "session-superseded"} {
		test.Run(change, func(test *testing.T) {
			probe := newLeaseFencingProbe(test)
			assignment, err := probe.stream.Recv()
			if err != nil || assignment.Kind != "assign" {
				test.Fatalf("expected a real assignment: %v", err)
			}
			if err := probe.stream.Send(&pb.AgentEvent{AgentId: probe.config.AgentID, Kind: "accept", AttemptId: assignment.AttemptId, LeaseToken: assignment.LeaseToken, CommandId: "lease-fence-accept"}); err != nil {
				test.Fatal(err)
			}
			grant, err := probe.stream.Recv()
			if err != nil || grant.Kind != "start" {
				test.Fatalf("expected a real start grant: %v", err)
			}
			attempt := assignment.AttemptId
			node := probe.config.AgentID
			before := probe.query("SELECT lease_expires_at::text FROM attempts WHERE id='" + attempt + "'")
			session := probe.query("SELECT active_session::text FROM nodes WHERE id='" + node + "'")
			if before == "" || session == "" {
				test.Fatal("DATABASE_URL does not contain this probe's newly created attempt and node")
			}
			blocker, release := probe.lockAttempt(attempt)
			payload, err := json.Marshal(map[string]any{"capabilities": []string{"git", "bash", "sh", "cc", "codesign"}, "active_attempts": []map[string]string{{"attempt_id": attempt, "lease_token": assignment.LeaseToken}}})
			if err != nil {
				test.Fatal(err)
			}
			requestID := "blocked-heartbeat-" + change
			if err := probe.stream.Send(&pb.AgentEvent{AgentId: node, Kind: "heartbeat", CommandId: requestID, PayloadJson: string(payload)}); err != nil {
				test.Fatal(err)
			}
			var waiting string
			probe.await("heartbeat actually waiting on the owned attempt lock", func() bool {
				waiting = probe.query(fmt.Sprintf("SELECT pid FROM pg_stat_activity WHERE %d=ANY(pg_blocking_pids(pid)) AND wait_event_type='Lock'", blocker))
				return waiting != ""
			})
			backend, err := strconv.Atoi(waiting)
			if err != nil {
				test.Fatal("expected one blocked heartbeat backend")
			}
			test.Log("real heartbeat is blocked after its initial node authorization check")
			switch change {
			case "revoked":
				probe.settings["revoked"] = true
				probe.api("PATCH", "/nodes/"+node, probe.settings)
			case "certificate-expired":
				// Move only this disposable node's database authorization expiry.
				// This does not claim that the TLS certificate itself aged in time.
				probe.query("UPDATE nodes SET certificate_expires_at=clock_timestamp()-interval '1 second' WHERE id='" + node + "'")
			case "session-superseded":
				fresh, err := probe.control.Connect(probe.context)
				if err != nil {
					test.Fatal(err)
				}
				defer fresh.CloseSend()
				if err := fresh.Send(probe.hello()); err != nil {
					test.Fatal(err)
				}
				probe.await("fresh authenticated session supersedes the old one", func() bool {
					return probe.query("SELECT active_session::text FROM nodes WHERE id='"+node+"'") != session
				})
			}
			release()
			// The same transaction owns the blocked SELECT and subsequent UPDATE.
			// Idle (not idle-in-transaction) proves its commit/rollback completed.
			probe.await("blocked heartbeat transaction completes", func() bool {
				return probe.query(fmt.Sprintf("SELECT NOT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=%d AND state<>'idle')", backend)) == "t"
			})
			after := probe.query("SELECT lease_expires_at::text FROM attempts WHERE id='" + attempt + "'")
			response, receiveError := probe.stream.Recv()
			if change == "unchanged" {
				if receiveError != nil || response.Kind != "renew" || response.RequestId != requestID || response.LeaseSeconds != 90 || before == after {
					test.Fatalf("valid blocked heartbeat did not renew its lease: %v", receiveError)
				}
				test.Log("unchanged authorization still renews after a real row-lock wait")
				return
			}
			if before != after {
				test.Errorf("lease was extended after %s committed during the row-lock wait: before=%s after=%s", change, before, after)
			}
			if change == "session-superseded" {
				if code := status.Code(receiveError); code != codes.Aborted && code != codes.Unauthenticated {
					test.Errorf("superseded stream was not rejected: %v", receiveError)
				}
			} else if receiveError == nil && (response.Kind != "cancel" || response.RequestId != requestID || response.LeaseSeconds != 0) {
				test.Errorf("invalid node authorization received a grant instead of cancellation")
			} else if receiveError != nil && status.Code(receiveError) != codes.Unauthenticated {
				test.Errorf("unexpected authorization failure: %v", receiveError)
			}
			if !test.Failed() {
				test.Log(change + " fenced the waiting heartbeat without extending its persisted lease")
			}
		})
	}
}

func TestLiveRustGoProtocolEventFencing(test *testing.T) {
	if os.Getenv("AK_LIVE_TESTS") != "1" {
		test.Skip("requires live Rust control, PostgreSQL, psql and acceptance fixtures")
	}
	for _, kind := range []string{"accept", "finished"} {
		for _, change := range []string{"unchanged", "revoked", "certificate-expired", "session-superseded"} {
			test.Run(kind+"/"+change, func(test *testing.T) {
				probe := newLeaseFencingProbe(test)
				assignment, err := probe.stream.Recv()
				if err != nil || assignment.Kind != "assign" {
					test.Fatalf("expected a real assignment: %v", err)
				}
				if kind == "finished" {
					if err := probe.stream.Send(&pb.AgentEvent{AgentId: probe.config.AgentID, Kind: "accept", AttemptId: assignment.AttemptId, LeaseToken: assignment.LeaseToken, CommandId: "event-fence-start"}); err != nil {
						test.Fatal(err)
					}
					grant, err := probe.stream.Recv()
					if err != nil || grant.Kind != "start" {
						test.Fatalf("expected a real start grant: %v", err)
					}
				}
				attempt := assignment.AttemptId
				node := probe.config.AgentID
				snapshot := func() string {
					return probe.query("SELECT json_build_object('status',a.status,'job',j.status,'lease',a.lease_expires_at,'sequence',a.last_sequence,'events',(SELECT count(*) FROM attempt_events WHERE attempt_id=a.id))::text FROM attempts a JOIN jobs j ON j.id=a.job_id WHERE a.id='" + attempt + "'")
				}
				before := snapshot()
				session := probe.query("SELECT active_session::text FROM nodes WHERE id='" + node + "'")
				if before == "" || session == "" {
					test.Fatal("DATABASE_URL does not contain this probe's newly created attempt and node")
				}
				blocker, release := probe.lockAttempt(attempt)
				event := &pb.AgentEvent{AgentId: node, Kind: kind, AttemptId: attempt, LeaseToken: assignment.LeaseToken, CommandId: "blocked-event-" + kind + "-" + change}
				if kind == "finished" {
					event.Sequence = 1
					event.PayloadJson = `{"status":"failed","error":"owned authorization probe"}`
				}
				if err := probe.stream.Send(event); err != nil {
					test.Fatal(err)
				}
				var waiting string
				probe.await("control event actually waiting on the owned attempt lock", func() bool {
					waiting = probe.query(fmt.Sprintf("SELECT pid FROM pg_stat_activity WHERE %d=ANY(pg_blocking_pids(pid)) AND wait_event_type='Lock'", blocker))
					return waiting != ""
				})
				backend, err := strconv.Atoi(waiting)
				if err != nil {
					test.Fatal("expected one blocked control-event backend")
				}
				test.Log("real " + kind + " event is blocked after its initial node authorization check")
				switch change {
				case "revoked":
					probe.settings["revoked"] = true
					probe.api("PATCH", "/nodes/"+node, probe.settings)
				case "certificate-expired":
					probe.query("UPDATE nodes SET certificate_expires_at=clock_timestamp()-interval '1 second' WHERE id='" + node + "'")
				case "session-superseded":
					fresh, err := probe.control.Connect(probe.context)
					if err != nil {
						test.Fatal(err)
					}
					defer fresh.CloseSend()
					if err := fresh.Send(probe.hello()); err != nil {
						test.Fatal(err)
					}
					probe.await("fresh authenticated session supersedes the old one", func() bool {
						return probe.query("SELECT active_session::text FROM nodes WHERE id='"+node+"'") != session
					})
				}
				release()
				probe.await("blocked control-event transaction completes", func() bool {
					return probe.query(fmt.Sprintf("SELECT NOT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=%d AND state<>'idle')", backend)) == "t"
				})
				after := snapshot()
				response, receiveError := probe.stream.Recv()
				if change == "unchanged" {
					if receiveError != nil || before == after {
						test.Fatalf("authorized event did not update its attempt: %v", receiveError)
					}
					if kind == "accept" {
						if response.Kind != "start" || response.RequestId != event.CommandId || probe.query("SELECT status FROM attempts WHERE id='"+attempt+"'") != "running" {
							test.Fatal("authorized acceptance did not start its task")
						}
					} else if response.Kind != "event_ack" || response.AckSequence != 1 || probe.query("SELECT status FROM attempts WHERE id='"+attempt+"'") != "failed" {
						test.Fatal("authorized completion was not persisted and acknowledged")
					}
					test.Log("unchanged authorization still accepts the " + kind + " event after a real row-lock wait")
					return
				}
				if before != after {
					test.Errorf("%s event changed persisted state after %s committed during the row-lock wait: before=%s after=%s", kind, change, before, after)
				}
				if change == "session-superseded" {
					if code := status.Code(receiveError); code != codes.Aborted && code != codes.Unauthenticated {
						test.Errorf("superseded stream was not rejected: %v", receiveError)
					}
				} else if status.Code(receiveError) != codes.Unauthenticated {
					test.Errorf("invalid node authorization was not rejected: %v", receiveError)
				}
				if !test.Failed() {
					test.Log(change + " fenced the waiting " + kind + " event without changing its persisted state")
				}
			})
		}
	}
}

func TestLiveRustGoProtocolReplayedCompletion(test *testing.T) {
	if os.Getenv("AK_LIVE_TESTS") != "1" {
		test.Skip("requires live Rust control, PostgreSQL, psql and acceptance fixtures")
	}
	probe := newLeaseFencingProbe(test)
	assignment, err := probe.stream.Recv()
	if err != nil || assignment.Kind != "assign" {
		test.Fatalf("expected a real assignment: %v", err)
	}
	node := probe.config.AgentID
	if err := probe.stream.Send(&pb.AgentEvent{AgentId: node, Kind: "accept", AttemptId: assignment.AttemptId, LeaseToken: assignment.LeaseToken}); err != nil {
		test.Fatal(err)
	}
	grant, err := probe.stream.Recv()
	if err != nil || grant.Kind != "start" {
		test.Fatalf("expected a real start grant: %v", err)
	}
	// Prevent automatic assignment while the failed job is explicitly retried.
	probe.settings["draining"] = true
	probe.api("PATCH", "/nodes/"+node, probe.settings)
	finished := &pb.AgentEvent{AgentId: node, Kind: "finished", AttemptId: assignment.AttemptId, LeaseToken: assignment.LeaseToken, Sequence: 1, PayloadJson: `{"status":"failed","error":"owned replay probe"}`}
	if err := probe.stream.Send(finished); err != nil {
		test.Fatal(err)
	}
	acknowledgment, err := probe.stream.Recv()
	if err != nil || acknowledgment.Kind != "event_ack" || acknowledgment.AckSequence != 1 {
		test.Fatalf("expected persisted completion acknowledgment: %v", err)
	}
	job := probe.query("SELECT job_id::text FROM attempts WHERE id='" + assignment.AttemptId + "'")
	probe.api("POST", "/jobs/"+job+"/retry", map[string]any{})
	blocker, release := probe.lockRow("nodes", node)
	if err := probe.stream.Send(finished); err != nil {
		test.Fatal(err)
	}
	var waiting string
	probe.await("replayed completion actually waiting on the owned node lock", func() bool {
		waiting = probe.query(fmt.Sprintf("SELECT pid FROM pg_stat_activity WHERE %d=ANY(pg_blocking_pids(pid)) AND wait_event_type='Lock'", blocker))
		return waiting != ""
	})
	backend, err := strconv.Atoi(waiting)
	if err != nil {
		test.Fatal("expected one blocked replay backend")
	}
	test.Log("old completion is blocked on its node after an explicit retry")
	release()
	acknowledgment, err = probe.stream.Recv()
	if err != nil || acknowledgment.Kind != "event_ack" || acknowledgment.AckSequence != 1 {
		test.Fatalf("replayed completion was not acknowledged: %v", err)
	}
	probe.await("replayed completion transaction completes", func() bool {
		return probe.query(fmt.Sprintf("SELECT NOT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=%d AND state<>'idle')", backend)) == "t"
	})
	if probe.query("SELECT status FROM jobs WHERE id='"+job+"'") != "queued" || probe.query("SELECT status||':'||last_sequence::text||':'||(SELECT count(*) FROM attempt_events WHERE attempt_id=a.id)::text FROM attempts a WHERE id='"+assignment.AttemptId+"'") != "failed:1:1" {
		test.Fatal("replayed completion changed the retried job or terminal attempt")
	}
	probe.settings["draining"] = false
	probe.api("PATCH", "/nodes/"+node, probe.settings)
	retry, err := probe.stream.Recv()
	if err != nil || retry.Kind != "assign" || retry.AttemptId == assignment.AttemptId {
		test.Fatalf("expected a distinct retry assignment after the replay: %v", err)
	}
	if err := probe.stream.Send(&pb.AgentEvent{AgentId: node, Kind: "accept", AttemptId: retry.AttemptId, LeaseToken: retry.LeaseToken}); err != nil {
		test.Fatal(err)
	}
	grant, err = probe.stream.Recv()
	if err != nil || grant.Kind != "start" || grant.AttemptId != retry.AttemptId {
		test.Fatalf("retry did not receive a valid start grant: %v", err)
	}
	test.Log("old completion preserved the queued retry; a distinct attempt received its start grant")
}

type leaseFencingProbe struct {
	test     *testing.T
	context  context.Context
	config   config.Config
	control  pb.ControlClient
	stream   pb.Control_ConnectClient
	settings map[string]any
	api      func(string, string, any) map[string]any
}

func newLeaseFencingProbe(test *testing.T) *leaseFencingProbe {
	test.Helper()
	if os.Getenv("DATABASE_URL") == "" {
		test.Fatal("DATABASE_URL must identify the live acceptance control plane's database")
	}
	if _, err := exec.LookPath("psql"); err != nil {
		test.Fatal("psql is required for controlled live database lock waits")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 35*time.Second)
	test.Cleanup(cancel)
	probe := &leaseFencingProbe{test: test, context: ctx}
	base := os.Getenv("AK_LIVE_HTTP_URL")
	if base == "" {
		base = "http://127.0.0.1:8080"
	}
	jar, err := cookiejar.New(nil)
	if err != nil {
		test.Fatal(err)
	}
	browser := &http.Client{Jar: jar, Timeout: 10 * time.Second}
	probe.api = func(method, path string, body any) map[string]any {
		test.Helper()
		encoded, err := json.Marshal(body)
		if err != nil {
			test.Fatal(err)
		}
		request, err := http.NewRequestWithContext(ctx, method, base+"/api/v1"+path, bytes.NewReader(encoded))
		if err != nil {
			test.Fatal(err)
		}
		request.Header.Set("Content-Type", "application/json")
		request.Header.Set("x-artifact-csrf", "1")
		response, err := browser.Do(request)
		if err != nil {
			test.Fatal(err)
		}
		defer response.Body.Close()
		if response.StatusCode != 200 {
			test.Fatalf("%s %s: HTTP %d", method, path, response.StatusCode)
		}
		var result map[string]any
		if err := json.NewDecoder(response.Body).Decode(&result); err != nil {
			test.Fatal(err)
		}
		return result
	}
	probe.api("POST", "/auth/login", map[string]string{"username": os.Getenv("AK_BOOTSTRAP_USERNAME"), "password": os.Getenv("AK_BOOTSTRAP_PASSWORD")})
	existing, err := config.Load(os.Getenv("AK_LIVE_AGENT_CONFIG"))
	if err != nil {
		test.Fatal(err)
	}
	pool := probe.api("POST", "/pools", map[string]string{"name": fmt.Sprintf("lease-fence-%d", time.Now().UnixNano())})["id"].(string)
	token := probe.api("POST", "/enrollment-tokens", map[string]string{"pool_id": pool})["token"].(string)
	directory := test.TempDir()
	path := filepath.Join(directory, "agent.json")
	if err := Enroll(ctx, path, config.Config{Endpoint: existing.Endpoint, CAFile: existing.CAFile, Root: directory, Name: "lease-fencing-probe"}, token); err != nil {
		test.Fatal(err)
	}
	probe.config, err = config.Load(path)
	if err != nil {
		test.Fatal(err)
	}
	source := probe.api("GET", "/projects/"+os.Getenv("AK_LIVE_PROJECT_ID"), nil)
	probe.settings = map[string]any{"draining": false, "revoked": false, "slots": 1, "labels": map[string]string{}, "signing_profiles": source["signing_profiles"], "pool_id": pool}
	probe.api("PATCH", "/nodes/"+probe.config.AgentID, probe.settings)
	project := probe.api("POST", "/projects", map[string]any{"name": "Lease fencing acceptance", "repository_url": source["repository_url"], "credential_id": source["credential_id"], "signing_profiles": source["signing_profiles"], "pool_ids": []string{pool}})["id"].(string)
	connection, err := Dial(probe.config, false)
	if err != nil {
		test.Fatal(err)
	}
	test.Cleanup(func() { connection.Close() })
	probe.control = pb.NewControlClient(connection)
	probe.stream, err = probe.control.Connect(ctx)
	if err != nil {
		test.Fatal(err)
	}
	test.Cleanup(func() { probe.stream.CloseSend() })
	if err := probe.stream.Send(probe.hello()); err != nil {
		test.Fatal(err)
	}
	build := probe.api("POST", "/projects/"+project+"/builds", map[string]any{"ref": "main", "targets": []string{"native"}})["id"].(string)
	test.Cleanup(func() {
		jobStatus := probe.query("SELECT status FROM jobs WHERE build_id='" + build + "'")
		if jobStatus == "queued" || jobStatus == "leased" || jobStatus == "running" {
			probe.api("POST", "/builds/"+build+"/cancel", map[string]any{})
			// This protocol-only peer has no Worker to acknowledge cancellation.
			// Expire only its own leases; let the real scheduler finish cancellation.
			probe.query("UPDATE attempts a SET lease_expires_at=clock_timestamp()-interval '1 second' FROM jobs j WHERE j.id=a.job_id AND j.build_id='" + build + "' AND a.status IN ('leased','running')")
		}
		probe.settings["revoked"] = true
		probe.api("PATCH", "/nodes/"+probe.config.AgentID, probe.settings)
		probe.await("owned protocol probe has no active build or jobs after cleanup", func() bool {
			return probe.query("SELECT b.status IN ('succeeded','failed','lost','canceled') AND NOT EXISTS(SELECT 1 FROM jobs j WHERE j.build_id=b.id AND j.status IN ('queued','leased','running')) FROM builds b WHERE b.id='"+build+"'") == "t"
		})
	})
	return probe
}

func (probe *leaseFencingProbe) hello() *pb.AgentEvent {
	return &pb.AgentEvent{AgentId: probe.config.AgentID, Kind: "hello", ProtocolMajor: 1, PayloadJson: `{"capabilities":["git","bash","sh","cc","codesign"]}`}
}

func (probe *leaseFencingProbe) psql() *exec.Cmd {
	database, err := url.Parse(os.Getenv("DATABASE_URL"))
	if err != nil || (database.Scheme != "postgres" && database.Scheme != "postgresql") || database.Host == "" {
		probe.test.Fatal("DATABASE_URL must be a PostgreSQL connection URL")
	}
	password, _ := database.User.Password()
	database.User = url.User(database.User.Username())
	options := database.Query()
	if options.Has("password") {
		password = options.Get("password")
		options.Del("password")
		database.RawQuery = options.Encode()
	}
	command := exec.CommandContext(probe.context, "psql", "--no-psqlrc", "--no-password", "--dbname", database.String(), "--quiet", "--no-align", "--tuples-only", "--set", "ON_ERROR_STOP=1")
	// Keep the database password out of process arguments and test diagnostics.
	command.Env = append(os.Environ(), "PGPASSWORD="+password, "PGCONNECT_TIMEOUT=5", "PGOPTIONS=-c statement_timeout=10000 -c idle_in_transaction_session_timeout=20000")
	return command
}

func (probe *leaseFencingProbe) query(query string) string {
	probe.test.Helper()
	command := probe.psql()
	command.Stdin = strings.NewReader(query + ";\n")
	output, err := command.Output()
	if err != nil {
		probe.test.Fatalf("owned acceptance database query failed: %v", err)
	}
	return strings.TrimSpace(string(output))
}

func (probe *leaseFencingProbe) lockAttempt(attempt string) (int, func()) {
	probe.test.Helper()
	return probe.lockRow("attempts", attempt)
}

func (probe *leaseFencingProbe) lockRow(table, id string) (int, func()) {
	probe.test.Helper()
	if table != "attempts" && table != "nodes" {
		probe.test.Fatal("only owned attempt/node locks are supported")
	}
	command := probe.psql()
	input, err := command.StdinPipe()
	if err != nil {
		probe.test.Fatal(err)
	}
	output, err := command.StdoutPipe()
	if err != nil {
		probe.test.Fatal(err)
	}
	if err := command.Start(); err != nil {
		probe.test.Fatal(err)
	}
	released := false
	release := func() {
		if released {
			return
		}
		released = true
		_, writeError := io.WriteString(input, "COMMIT;\n\\q\n")
		input.Close()
		if err := command.Wait(); err != nil || writeError != nil {
			probe.test.Errorf("release owned row lock: process=%v write=%v", err, writeError)
		}
	}
	probe.test.Cleanup(release)
	if _, err := fmt.Fprintf(input, "BEGIN;\nSELECT pg_backend_pid();\nSELECT id FROM %s WHERE id='%s' FOR UPDATE;\n\\echo LEASE_LOCK_READY\n", table, id); err != nil {
		probe.test.Fatal(err)
	}
	scanner := bufio.NewScanner(output)
	if !scanner.Scan() {
		probe.test.Fatal("lock connection did not report its backend PID")
	}
	pid, err := strconv.Atoi(scanner.Text())
	if err != nil {
		probe.test.Fatal(err)
	}
	for scanner.Scan() {
		if scanner.Text() == "LEASE_LOCK_READY" {
			return pid, release
		}
	}
	probe.test.Fatalf("owned row lock was not acquired: %v", scanner.Err())
	return 0, release
}

func (probe *leaseFencingProbe) await(name string, ready func() bool) {
	probe.test.Helper()
	deadline := time.Now().Add(8 * time.Second)
	for time.Now().Before(deadline) {
		if ready() {
			return
		}
		time.Sleep(30 * time.Millisecond)
	}
	probe.test.Fatal(name + " timed out")
}
