package client

import (
	"fmt"
	"os"
	"strconv"
	"testing"

	pb "github.com/artifact-kit/agent/gen/agentv1"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// A committed build cancellation during either authorization lock wait must
// prevent a start grant and override a late failure result to canceled.
func TestLiveRustGoProtocolCancellationFencing(test *testing.T) {
	if os.Getenv("AK_LIVE_TESTS") != "1" {
		test.Skip("requires live Rust control, PostgreSQL, psql and acceptance fixtures")
	}
	for _, kind := range []string{"accept", "finished"} {
		for _, table := range []string{"attempts", "nodes"} {
			for _, change := range []string{"unchanged", "canceled"} {
				test.Run(kind+"/"+table+"/"+change, func(test *testing.T) {
					probe := newLeaseFencingProbe(test)
					assignment, err := probe.stream.Recv()
					if err != nil || assignment.Kind != "assign" {
						test.Fatalf("expected a real assignment: %v", err)
					}
					attempt := assignment.AttemptId
					node := probe.config.AgentID
					if kind == "finished" {
						if err := probe.stream.Send(&pb.AgentEvent{AgentId: node, Kind: "accept", AttemptId: attempt, LeaseToken: assignment.LeaseToken}); err != nil {
							test.Fatal(err)
						}
						grant, err := probe.stream.Recv()
						if err != nil || grant.Kind != "start" {
							test.Fatalf("expected a real start grant: %v", err)
						}
					}
					build := probe.query("SELECT j.build_id::text FROM attempts a JOIN jobs j ON j.id=a.job_id WHERE a.id='" + attempt + "'")
					snapshot := func() string {
						return probe.query("SELECT a.status||':'||j.status||':'||a.last_sequence::text||':'||(SELECT count(*) FROM attempt_events WHERE attempt_id=a.id)::text FROM attempts a JOIN jobs j ON j.id=a.job_id WHERE a.id='" + attempt + "'")
					}
					before := snapshot()
					lockedID := attempt
					if table == "nodes" {
						lockedID = node
					}
					blocker, release := probe.lockRow(table, lockedID)
					event := &pb.AgentEvent{AgentId: node, Kind: kind, AttemptId: attempt, LeaseToken: assignment.LeaseToken, CommandId: "cancellation-fence-" + kind}
					if kind == "finished" {
						event.Sequence = 1
						event.PayloadJson = `{"status":"failed","error":"owned cancellation probe"}`
					}
					if err := probe.stream.Send(event); err != nil {
						test.Fatal(err)
					}
					var waiting string
					probe.await("control event actually waiting on the owned "+table+" row", func() bool {
						waiting = probe.query(fmt.Sprintf("SELECT pid FROM pg_stat_activity WHERE %d=ANY(pg_blocking_pids(pid)) AND wait_event_type='Lock'", blocker))
						return waiting != ""
					})
					backend, err := strconv.Atoi(waiting)
					if err != nil {
						test.Fatal("expected one blocked cancellation-probe backend")
					}
					test.Log("real " + kind + " event is waiting on its owned " + table + " row")
					if change == "canceled" {
						probe.api("POST", "/builds/"+build+"/cancel", map[string]any{})
						if probe.query("SELECT cancel_requested FROM builds WHERE id='"+build+"'") != "t" {
							test.Fatal("build cancellation did not commit before releasing the event")
						}
					}
					release()
					probe.await("blocked event transaction completes", func() bool {
						return probe.query(fmt.Sprintf("SELECT NOT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=%d AND state<>'idle')", backend)) == "t"
					})
					response, receiveError := probe.stream.Recv()
					// The real scheduler may enqueue cancellation while the event
					// waits. Observe the handler's grant/ack/error after those notices.
					for receiveError == nil && response.Kind == "cancel" && response.AttemptId == attempt {
						response, receiveError = probe.stream.Recv()
					}
					after := snapshot()
					if kind == "accept" {
						if change == "canceled" {
							if status.Code(receiveError) != codes.FailedPrecondition || before != after {
								test.Fatalf("cancellation during %s wait did not fence start: response=%v before=%s after=%s", table, receiveError, before, after)
							}
							test.Log("committed cancellation prevented a start grant after the " + table + " wait")
							return
						}
						if receiveError != nil || response.Kind != "start" || response.RequestId != event.CommandId || after != "running:running:0:0" {
							test.Fatalf("valid waiting acceptance did not start: %v state=%s", receiveError, after)
						}
						test.Log("uncanceled acceptance still starts after the " + table + " wait")
						return
					}
					want := "failed:failed:1:1"
					if change == "canceled" {
						want = "canceled:canceled:1:1"
					}
					if receiveError != nil || response.Kind != "event_ack" || response.AckSequence != 1 || after != want {
						test.Fatalf("waiting completion used stale cancellation after %s wait: %v state=%s want=%s", table, receiveError, after, want)
					}
					test.Log(change + " completion persisted the expected terminal status after the " + table + " wait")
				})
			}
		}
	}
}
