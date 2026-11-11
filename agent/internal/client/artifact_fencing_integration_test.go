package client

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"net/http"
	"os"
	"strconv"
	"testing"

	pb "github.com/zhubby/artifact-kit/agent/gen/agentv1"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// Observe actual database lock waits after the unary artifact RPC's initial
// authorization. Invalidating the owned attempt must prevent a committed ticket
// or artifact record when that wait ends.
func TestLiveRustGoProtocolArtifactFencing(test *testing.T) {
	if os.Getenv("ARTIFACTORY_LIVE_TESTS") != "1" {
		test.Skip("requires live Rust control, PostgreSQL, psql and acceptance fixtures")
	}
	for _, stage := range []string{"prepare", "complete"} {
		for _, change := range []string{"unchanged", "revoked", "certificate-expired", "canceled", "lease-expired"} {
			test.Run(stage+"/"+change, func(test *testing.T) {
				probe := newLeaseFencingProbe(test)
				assignment, err := probe.stream.Recv()
				if err != nil || assignment.Kind != "assign" {
					test.Fatalf("expected a real assignment: %v", err)
				}
				if err := probe.stream.Send(&pb.AgentEvent{AgentId: probe.config.AgentID, Kind: "accept", AttemptId: assignment.AttemptId, LeaseToken: assignment.LeaseToken}); err != nil {
					test.Fatal(err)
				}
				grant, err := probe.stream.Recv()
				if err != nil || grant.Kind != "start" {
					test.Fatalf("expected a real start grant: %v", err)
				}
				attempt := assignment.AttemptId
				build := probe.query("SELECT j.build_id::text FROM attempts a JOIN jobs j ON j.id=a.job_id WHERE a.id='" + attempt + "'")
				data := []byte("owned artifact authorization probe")
				digest := sha256.Sum256(data)
				declaration := &pb.ArtifactRequest{AttemptId: attempt, LeaseToken: assignment.LeaseToken, Path: "dist/fenced-artifact", Size: uint64(len(data)), Sha256: hex.EncodeToString(digest[:])}
				var ticket *pb.UploadTicket
				if stage == "complete" {
					ticket, err = probe.control.PrepareArtifact(probe.context, declaration)
					if err != nil {
						test.Fatal(err)
					}
					request, err := http.NewRequestWithContext(probe.context, "PUT", ticket.Url, bytes.NewReader(data))
					if err != nil {
						test.Fatal(err)
					}
					response, err := http.DefaultClient.Do(request)
					if err != nil {
						test.Fatal(err)
					}
					response.Body.Close()
					if response.StatusCode/100 != 2 {
						test.Fatalf("owned staging upload: HTTP %d", response.StatusCode)
					}
				}
				snapshot := func() string {
					return probe.query("SELECT (SELECT count(*) FROM uploads WHERE attempt_id='" + attempt + "')::text||':'||(SELECT count(*) FROM artifacts WHERE attempt_id='" + attempt + "')::text")
				}
				before := snapshot()
				if change == "lease-expired" {
					// Let time really pass while the attempt row is locked; no other
					// connection can change that row until this test releases it.
					probe.query("UPDATE attempts SET lease_expires_at=clock_timestamp()+interval '5 seconds' WHERE id='" + attempt + "'")
				}
				blocker, release := probe.lockAttempt(attempt)
				type result struct {
					id  string
					err error
				}
				completed := make(chan result, 1)
				go func() {
					if stage == "prepare" {
						ticket, err := probe.control.PrepareArtifact(probe.context, declaration)
						if err != nil {
							completed <- result{err: err}
							return
						}
						completed <- result{id: ticket.UploadId}
						return
					}
					artifact, err := probe.control.CompleteArtifact(probe.context, &pb.CompleteArtifactRequest{AttemptId: attempt, LeaseToken: assignment.LeaseToken, UploadId: ticket.UploadId})
					if err != nil {
						completed <- result{err: err}
						return
					}
					completed <- result{id: artifact.ArtifactId}
				}()
				var waiting string
				probe.await("artifact RPC actually waiting on the owned attempt row", func() bool {
					waiting = probe.query(fmt.Sprintf("SELECT pid FROM pg_stat_activity WHERE %d=ANY(pg_blocking_pids(pid)) AND wait_event_type='Lock'", blocker))
					return waiting != ""
				})
				backend, err := strconv.Atoi(waiting)
				if err != nil {
					test.Fatal("expected one blocked artifact RPC backend")
				}
				test.Log("real " + stage + " RPC is blocked after its initial attempt authorization")
				switch change {
				case "revoked":
					probe.settings["revoked"] = true
					probe.api("PATCH", "/nodes/"+probe.config.AgentID, probe.settings)
				case "certificate-expired":
					probe.query("UPDATE nodes SET certificate_expires_at=clock_timestamp()-interval '1 second' WHERE id='" + probe.config.AgentID + "'")
				case "canceled":
					probe.api("POST", "/builds/"+build+"/cancel", map[string]any{})
				case "lease-expired":
					probe.await("owned lease really expires during the lock wait", func() bool {
						return probe.query("SELECT lease_expires_at<=clock_timestamp() FROM attempts WHERE id='"+attempt+"'") == "t"
					})
				}
				release()
				var outcome result
				select {
				case outcome = <-completed:
				case <-probe.context.Done():
					test.Fatal("artifact RPC did not finish after releasing its attempt lock")
				}
				probe.await("artifact RPC transaction completes", func() bool {
					return probe.query(fmt.Sprintf("SELECT NOT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=%d AND state<>'idle')", backend)) == "t"
				})
				after := snapshot()
				if change == "unchanged" {
					expected := "1:0"
					if stage == "complete" {
						expected = "1:1"
					}
					if outcome.err != nil || outcome.id == "" || after != expected {
						test.Fatalf("valid waiting %s RPC failed: %v records=%s", stage, outcome.err, after)
					}
					test.Log("unchanged authorization still completes " + stage + " after a real attempt-lock wait")
					return
				}
				if before != after {
					test.Errorf("%s committed new records after %s during its lock wait: before=%s after=%s", stage, change, before, after)
				}
				if code := status.Code(outcome.err); code != codes.FailedPrecondition && code != codes.Unauthenticated {
					test.Errorf("invalid %s was not fenced: %v", stage, outcome.err)
				}
				if !test.Failed() {
					test.Log(change + " fenced the waiting " + stage + " without committing new records")
				}
			})
		}
	}
}

func TestLiveRustGoProtocolArtifactConcurrency(test *testing.T) {
	if os.Getenv("ARTIFACTORY_LIVE_TESTS") != "1" {
		test.Skip("requires live Rust control, PostgreSQL, psql and acceptance fixtures")
	}
	probe := newLeaseFencingProbe(test)
	assignment, err := probe.stream.Recv()
	if err != nil || assignment.Kind != "assign" {
		test.Fatalf("expected a real assignment: %v", err)
	}
	if err := probe.stream.Send(&pb.AgentEvent{AgentId: probe.config.AgentID, Kind: "accept", AttemptId: assignment.AttemptId, LeaseToken: assignment.LeaseToken}); err != nil {
		test.Fatal(err)
	}
	grant, err := probe.stream.Recv()
	if err != nil || grant.Kind != "start" {
		test.Fatalf("expected a real start grant: %v", err)
	}
	data := []byte("concurrent immutable artifact")
	digest := sha256.Sum256(data)
	declaration := &pb.ArtifactRequest{AttemptId: assignment.AttemptId, LeaseToken: assignment.LeaseToken, Path: "dist/concurrent", Size: uint64(len(data)), Sha256: hex.EncodeToString(digest[:])}
	runConcurrent := func(count int, operation func(int) (string, error)) []string {
		test.Helper()
		type result struct {
			id  string
			err error
		}
		ready := make(chan struct{})
		completed := make(chan result, count)
		for index := 0; index < count; index++ {
			go func(index int) {
				<-ready
				id, err := operation(index)
				completed <- result{id: id, err: err}
			}(index)
		}
		close(ready)
		var ids []string
		for range count {
			select {
			case outcome := <-completed:
				if outcome.err != nil || outcome.id == "" {
					test.Fatalf("concurrent artifact operation failed: %v", outcome.err)
				}
				ids = append(ids, outcome.id)
			case <-probe.context.Done():
				test.Fatal("concurrent artifact operations did not finish")
			}
		}
		return ids
	}
	ids := runConcurrent(8, func(int) (string, error) {
		ticket, err := probe.control.PrepareArtifact(probe.context, declaration)
		if err != nil {
			return "", err
		}
		return ticket.UploadId, nil
	})
	ticket, err := probe.control.PrepareArtifact(probe.context, declaration)
	if err != nil {
		test.Fatal(err)
	}
	for _, id := range ids {
		if id != ticket.UploadId {
			test.Fatal("concurrent declarations created different upload identities")
		}
	}
	request, err := http.NewRequestWithContext(probe.context, "PUT", ticket.Url, bytes.NewReader(data))
	if err != nil {
		test.Fatal(err)
	}
	response, err := http.DefaultClient.Do(request)
	if err != nil {
		test.Fatal(err)
	}
	response.Body.Close()
	if response.StatusCode/100 != 2 {
		test.Fatalf("owned concurrent staging upload: HTTP %d", response.StatusCode)
	}
	payload, err := json.Marshal(map[string]any{"active_attempts": []map[string]string{{"attempt_id": assignment.AttemptId, "lease_token": assignment.LeaseToken}}})
	if err != nil {
		test.Fatal(err)
	}
	ids = runConcurrent(17, func(index int) (string, error) {
		if index == 16 {
			return ticket.UploadId, probe.stream.Send(&pb.AgentEvent{AgentId: probe.config.AgentID, Kind: "heartbeat", CommandId: "artifact-concurrency-heartbeat", PayloadJson: string(payload)})
		}
		if index%2 == 0 {
			ticket, err := probe.control.PrepareArtifact(probe.context, declaration)
			if err != nil {
				return "", err
			}
			return ticket.UploadId, nil
		}
		artifact, err := probe.control.CompleteArtifact(probe.context, &pb.CompleteArtifactRequest{AttemptId: assignment.AttemptId, LeaseToken: assignment.LeaseToken, UploadId: ticket.UploadId})
		if err != nil {
			return "", err
		}
		if artifact.Sha256 != declaration.Sha256 || artifact.Size != declaration.Size {
			return "", fmt.Errorf("concurrent completion changed declared artifact content")
		}
		return artifact.ArtifactId, nil
	})
	for _, id := range ids {
		if id != ticket.UploadId {
			test.Fatal("concurrent completion changed artifact identity")
		}
	}
	renewal, err := probe.stream.Recv()
	if err != nil || renewal.Kind != "renew" || renewal.RequestId != "artifact-concurrency-heartbeat" || renewal.AttemptId != assignment.AttemptId {
		test.Fatalf("artifact concurrency prevented heartbeat renewal: %v", err)
	}
	if probe.query("SELECT (SELECT count(*) FROM uploads WHERE attempt_id='"+assignment.AttemptId+"')::text||':'||(SELECT count(*) FROM artifacts WHERE attempt_id='"+assignment.AttemptId+"')::text") != "1:1" {
		test.Fatal("concurrent requests did not commit exactly one upload and artifact")
	}
	test.Log("eight concurrent preparations and eight mixed duplicate completions preserved one artifact while heartbeat renewal succeeded")
}
