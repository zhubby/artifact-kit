// A finite process-tree fixture for service lifecycle tests. This is compiled
// into a private Git fixture; it is not installed as a service or shipped.
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"os"
	"os/exec"
	"os/user"
	"time"
)

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func run() error {
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Minute)
	defer cancel()
	mode, endpoint, key := os.Args[1], os.Getenv("ARTIFACTORY_WORKLOAD_ENDPOINT"), os.Getenv("ARTIFACTORY_WORKLOAD_KEY")
	owner, err := user.Current()
	if err != nil {
		return err
	}
	directory, err := os.Getwd()
	if err != nil {
		return err
	}
	data, err := json.Marshal(map[string]any{"mode": mode, "key": key, "pid": os.Getpid(), "parent_pid": os.Getppid(), "user": owner.Username, "directory": directory})
	if err != nil {
		return err
	}
	client := &http.Client{Timeout: time.Second}
	report := func() bool {
		request, err := http.NewRequestWithContext(ctx, http.MethodPost, endpoint, bytes.NewReader(data))
		if err != nil {
			return false
		}
		response, err := client.Do(request)
		if err != nil {
			return false
		}
		response.Body.Close()
		return response.StatusCode == http.StatusNoContent
	}
	if !report() {
		return fmt.Errorf("workload observer unavailable")
	}
	if mode == "success" {
		fmt.Println("native service task completed")
		return nil
	}
	if mode == "parent" {
		executable, err := os.Executable()
		if err != nil {
			return err
		}
		child := exec.CommandContext(ctx, executable, "descendant")
		child.Stdout, child.Stderr = os.Stdout, os.Stderr
		return child.Run()
	}
	if mode != "descendant" {
		return fmt.Errorf("unknown workload mode %q", mode)
	}
	ticker := time.NewTicker(100 * time.Millisecond)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-ticker.C:
			// Failure cleanup closes the observer, allowing only this test's
			// workload to exit without killing an arbitrary PID.
			if !report() {
				return nil
			}
		}
	}
}
