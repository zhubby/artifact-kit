//go:build !windows

package worker

import (
	"encoding/json"
	"io"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	pb "github.com/artifact-kit/agent/gen/agentv1"
	"github.com/artifact-kit/agent/internal/config"
)

func TestWorkerKillsProcessTreeOnParentDeathAndLeaseExpiry(test *testing.T) {
	for _, scenario := range []string{"parent-pipe", "lease-expiry"} {
		test.Run(scenario, func(test *testing.T) {
			directory := test.TempDir()
			marker := filepath.Join(directory, "child.pid")
			script := "#!/bin/sh\nsleep 60 &\nprintf '%s\\n' \"$!\" > \"$AK_TEST_PID_FILE\"\nwait\n"
			if err := os.WriteFile(filepath.Join(directory, "git"), []byte(script), 0700); err != nil {
				test.Fatal(err)
			}
			test.Setenv("PATH", directory+string(os.PathListSeparator)+os.Getenv("PATH"))
			test.Setenv("AK_TEST_PID_FILE", marker)
			reader, writer := io.Pipe()
			defer reader.Close()
			defer writer.Close()
			done := make(chan error, 1)
			go func() { done <- Run(reader, io.Discard) }()
			lease := uint32(30)
			if scenario == "lease-expiry" {
				lease = 1
			}
			initial := Input{Kind: "start", AttemptID: "00000000-0000-4000-8000-000000000001", LeaseSeconds: lease, Config: config.Config{Root: filepath.Join(directory, "managed")}, Assignment: &pb.Assignment{CommitSha: strings.Repeat("0", 40), RepositoryUrl: "https://invalid.example/repo.git", TimeoutSeconds: 30}}
			if err := json.NewEncoder(writer).Encode(initial); err != nil {
				test.Fatal(err)
			}
			var child int
			deadline := time.Now().Add(5 * time.Second)
			for time.Now().Before(deadline) {
				if data, err := os.ReadFile(marker); err == nil {
					child, _ = strconv.Atoi(strings.TrimSpace(string(data)))
					if child > 0 {
						break
					}
				}
				time.Sleep(10 * time.Millisecond)
			}
			if child == 0 {
				test.Fatal("fixture child did not start")
			}
			if scenario == "parent-pipe" {
				writer.Close()
			}
			select {
			case err := <-done:
				if err == nil {
					test.Fatal("unexpected success")
				}
			case <-time.After(5 * time.Second):
				test.Fatal("worker did not stop")
			}
			deadline = time.Now().Add(3 * time.Second)
			for time.Now().Before(deadline) {
				if syscall.Kill(child, 0) != nil {
					return
				}
				time.Sleep(10 * time.Millisecond)
			}
			test.Fatal("child process survived worker cancellation")
		})
	}
}
