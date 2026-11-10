//go:build !windows

package main

import (
	"os"
	"os/exec"
	"os/user"
	"path/filepath"
	"syscall"
	"testing"
	"time"

	pb "github.com/artifact-kit/agent/gen/agentv1"
	"github.com/artifact-kit/agent/internal/config"
)

// Run real Agent/Worker binaries, without installing a local system service.
func TestForegroundAgentStopsActiveProcessTree(test *testing.T) {
	binary := filepath.Join(test.TempDir(), "artifact-agent")
	requireNativeCommand(test, nil, false, "go", "build", "-o", binary, ".")
	owner, err := user.Current()
	if err != nil {
		test.Fatal(err)
	}
	for _, scenario := range []string{"interrupt", "agent-crash", "worker-sigterm", "replayed-before-tree", "replayed-before-success"} {
		test.Run(scenario, func(test *testing.T) {
			private, err := filepath.EvalSymlinks(test.TempDir())
			if err != nil {
				test.Fatal(err)
			}
			endpoint, peer := startServicePeer(test, private)
			workload := newServiceWorkload(test, private)
			test.Cleanup(workload.stop)
			root, path := filepath.Join(private, "managed root"), filepath.Join(private, "agent.json")
			if err := config.Save(path, config.Config{Endpoint: endpoint, CAFile: filepath.Join(private, "ca.pem"), CertificateFile: filepath.Join(private, "client.pem"), KeyFile: filepath.Join(private, "client-key.pem"), AgentID: "native-service-test", Root: root}); err != nil {
				test.Fatal(err)
			}
			log, err := os.Create(filepath.Join(private, "agent.log"))
			if err != nil {
				test.Fatal(err)
			}
			test.Cleanup(func() { log.Close() })
			command := exec.Command(binary, "run", "--config", path)
			command.Stdout, command.Stderr = log, log
			if err := command.Start(); err != nil {
				test.Fatal(err)
			}
			done := make(chan error, 1)
			go func() { done <- command.Wait() }()
			waited := false
			test.Cleanup(func() {
				if !waited {
					command.Process.Kill()
					<-done
				}
			})
			connection := awaitServiceConnection(test, peer)
			if scenario == "replayed-before-tree" || scenario == "replayed-before-success" {
				completed := workload.assign(test, connection, 2, "success")
				workload.awaitSuccess(test, connection, completed)
				// Terminal events may be replayed before their acknowledgment reaches
				// the Agent. Deliver a duplicate at the observer boundary deterministically.
				connection.finished <- &pb.AgentEvent{Kind: "finished", AttemptId: completed, PayloadJson: `{"status":"succeeded"}`}
				if scenario == "replayed-before-success" {
					completed = workload.assign(test, connection, 3, "success")
					workload.awaitSuccess(test, connection, completed)
				}
			}
			id := workload.assign(test, connection, 1, "parent")
			treeStopped, workerPID := workload.awaitTree(test, connection, id, command.Process.Pid, owner.Username, root)
			if scenario == "worker-sigterm" {
				if err := syscall.Kill(workerPID, syscall.SIGTERM); err != nil {
					test.Fatal(err)
				}
				select {
				case event := <-connection.finished:
					test.Logf("Worker SIGTERM terminal event: %s", event.PayloadJson)
				case <-time.After(10 * time.Second):
					test.Fatal("Agent did not observe Worker termination")
				}
			}
			if scenario == "agent-crash" {
				err = command.Process.Kill()
			} else {
				err = command.Process.Signal(os.Interrupt)
			}
			if err != nil {
				test.Fatal(err)
			}
			select {
			case err := <-done:
				waited = true
				if (err == nil) != (scenario != "agent-crash") {
					test.Fatalf("unexpected Agent exit after %s: %v", scenario, err)
				}
			case <-time.After(10 * time.Second):
				test.Fatal("foreground Agent did not exit")
			}
			awaitServiceDisconnect(test, connection)
			treeStopped()
		})
	}
}

func watchWorkloadProcess(test *testing.T, pid int) func() bool {
	test.Helper()
	if err := syscall.Kill(pid, 0); err != nil && err != syscall.EPERM {
		test.Fatalf("workload process %d is not alive at stop checkpoint: %v", pid, err)
	}
	return func() bool { return syscall.Kill(pid, 0) == syscall.ESRCH }
}
