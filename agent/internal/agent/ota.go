package agent

import (
	"encoding/json"
	"os"
	"path/filepath"
	"time"

	pb "github.com/artifact-kit/agent/gen/agentv1"
	"github.com/artifact-kit/agent/internal/ota"
)

func (runner *Runner) otaCommand(command *pb.Command) (bool, error) {
	directory := os.Getenv("AK_OTA_DIRECTORY")
	if directory == "" {
		return false, nil
	}
	switch command.Kind {
	case "agent_update":
		var request ota.Request
		if err := json.Unmarshal([]byte(command.PayloadJson), &request); err != nil {
			return true, err
		}
		return true, ota.Submit(directory, request)
	case "agent_health":
		runner.mutex.Lock()
		anchor, known := runner.anchors[command.RequestId]
		fresh := known && time.Since(anchor) < 30*time.Second && anchor.After(runner.otaHealthAt)
		if fresh {
			runner.otaHealthAt = anchor
		}
		runner.mutex.Unlock()
		if fresh {
			return true, ota.Confirm(directory, runner.otaSession, command.RequestId)
		}
		return true, nil
	}
	return false, nil
}

// Caller holds the task mutex. finished events remain in tasks until their
// terminal ack, which itself is withheld until artifact and log delivery.
func (runner *Runner) otaSnapshot() any {
	directory := os.Getenv("AK_OTA_DIRECTORY")
	if directory == "" {
		return nil
	}
	state, err := ota.Snapshot(directory)
	if err != nil {
		return nil
	}
	if state.Phase == "draining" && len(runner.tasks) == 0 && runner.workers == 0 {
		runner.spoolMutex.Lock()
		empty := runner.spoolBytes == 0
		runner.spoolMutex.Unlock()
		if empty {
			_ = ota.Atomic(filepath.Join(directory, "drained.json"), map[string]string{"id": state.UpdateID})
		}
	}
	return state
}
