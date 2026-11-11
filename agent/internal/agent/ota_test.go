package agent

import (
	pb "github.com/artifact-kit/agent/gen/agentv1"
	"github.com/artifact-kit/agent/internal/ota"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestOTADrainWaitsForTerminalAckWorkersAndLogs(t *testing.T) {
	directory := t.TempDir()
	t.Setenv("AK_OTA_DIRECTORY", directory)
	if err := ota.Atomic(filepath.Join(directory, "state.json"), ota.State{UpdateID: "u", Phase: "draining"}); err != nil {
		t.Fatal(err)
	}
	r := &Runner{tasks: map[string]*task{"attempt": {finished: true}}, workers: 1, spoolBytes: 5}
	assertReady := func(expected bool) {
		t.Helper()
		r.otaSnapshot()
		_, err := os.Stat(filepath.Join(directory, "drained.json"))
		if (err == nil) != expected {
			t.Fatalf("ready=%t, expected=%t", err == nil, expected)
		}
	}
	assertReady(false)
	delete(r.tasks, "attempt")
	assertReady(false)
	r.workers = 0
	assertReady(false)
	r.spoolBytes = 0
	assertReady(true)
}

func TestOTAHealthRequiresCurrentOutstandingHeartbeat(t *testing.T) {
	directory := t.TempDir()
	t.Setenv("AK_OTA_DIRECTORY", directory)
	r := &Runner{otaSession: "session", anchors: map[string]time.Time{"fresh": time.Now(), "expired": time.Now().Add(-time.Minute)}}
	for _, id := range []string{"forged", "expired"} {
		if err := r.command(&pb.Command{Kind: "agent_health", RequestId: id}); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := os.Stat(filepath.Join(directory, "health.json")); !os.IsNotExist(err) {
		t.Fatal("unauthorized health acknowledgment accepted")
	}
	if err := r.command(&pb.Command{Kind: "agent_health", RequestId: "fresh"}); err != nil {
		t.Fatal(err)
	}
	var h ota.Health
	if err := ota.Read(filepath.Join(directory, "health.json"), &h); err != nil || h.ID != "fresh" {
		t.Fatal("fresh acknowledgment lost", err)
	}
	if r.otaHealthAt != r.anchors["fresh"] {
		t.Fatal("health window did not retain replay boundary")
	}
}
