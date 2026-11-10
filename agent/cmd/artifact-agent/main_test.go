package main

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/artifact-kit/agent/internal/config"
)

func TestServiceRunHelper(test *testing.T) {
	if os.Getenv("AK_TEST_SERVICE_RUN_HELPER") != "1" {
		return
	}
	os.Args = []string{"artifact-agent", "service-run", "--config", os.Getenv("AK_TEST_SERVICE_CONFIG")}
	main()
}

func TestServiceRunExitsWhenAgentCannotStart(test *testing.T) {
	directory := test.TempDir()
	path := filepath.Join(directory, "agent.json")
	// This is a syntactically valid configuration that was never enrolled.
	// The real Runner must fail before opening any network connection.
	if err := config.Save(path, config.Config{Endpoint: "127.0.0.1:1", CAFile: "unused.pem", Root: directory}); err != nil {
		test.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	command := exec.CommandContext(ctx, os.Args[0], "-test.run=^TestServiceRunHelper$")
	command.Env = append(os.Environ(), "AK_TEST_SERVICE_RUN_HELPER=1", "AK_TEST_SERVICE_CONFIG="+path)
	output, err := command.CombinedOutput()
	if ctx.Err() != nil {
		test.Fatalf("service stayed alive after its Agent failed: %s", output)
	}
	if err == nil {
		test.Fatalf("service reported successful startup: %s", output)
	}
	if !strings.Contains(string(output), "agent must be registered first") {
		test.Fatalf("expected the real Agent startup error, got %v: %s", err, output)
	}
}
