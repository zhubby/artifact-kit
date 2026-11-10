package agent

import (
	"os"
	"path/filepath"
	"testing"
	"time"

	pb "github.com/artifact-kit/agent/gen/agentv1"
	"github.com/artifact-kit/agent/internal/config"
	"github.com/artifact-kit/agent/internal/worker"
)

func TestBoundedSpoolDoesNotSkipSequenceOnOverflow(test *testing.T) {
	runner := &Runner{config: config.Config{Root: test.TempDir(), LogBufferBytes: 1}}
	current := &task{id: "task"}
	if err := runner.spool(current, worker.Event{Step: "test", Data: []byte("data")}); err == nil {
		test.Fatal("overflow accepted")
	}
	if current.logSequence != 0 || runner.spoolBytes != 0 {
		test.Fatal("overflow advanced the log cursor")
	}
	runner.config.LogBufferBytes = 4096
	if err := runner.spool(current, worker.Event{Step: "test", Data: []byte("data")}); err != nil {
		test.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(runner.config.Root, "spool", "task", "00000000000000000001.json")); err != nil {
		test.Fatal(err)
	}
}

func TestDuplicateAssignmentAndDelayedRenewalDoNotExtendLease(test *testing.T) {
	runner := &Runner{tasks: map[string]*task{}, anchors: map[string]time.Time{}}
	assignment := &pb.Command{Kind: "assign", AttemptId: "attempt", LeaseToken: "lease", LeaseSeconds: 90, Assignment: &pb.Assignment{}}
	if err := runner.command(assignment); err != nil {
		test.Fatal(err)
	}
	current := runner.tasks["attempt"]
	if err := runner.command(assignment); err != nil {
		test.Fatal(err)
	}
	if runner.tasks["attempt"] != current {
		test.Fatal("duplicate assignment recreated the task")
	}
	current.started = true
	anchor := time.Now().Add(-15 * time.Second)
	runner.anchors["request"] = anchor
	if err := runner.command(&pb.Command{Kind: "renew", AttemptId: current.id, LeaseToken: current.token, RequestId: "request", LeaseSeconds: 30}); err != nil {
		test.Fatal(err)
	}
	if !current.deadline.Equal(anchor.Add(30 * time.Second)) {
		test.Fatal("renewal was anchored to receive time instead of request time")
	}
}
