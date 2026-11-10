package agent

import (
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"testing"

	"github.com/artifact-kit/agent/internal/config"
)

func TestDoctorOnlyAdvertisesWorkingToolchains(test *testing.T) {
	if runtime.GOOS == "windows" {
		test.Skip("uses Unix executable fixtures")
	}
	directory := test.TempDir()
	working := filepath.Join(directory, "go")
	if err := os.WriteFile(working, []byte("#!/bin/sh\nif [ \"$1\" = version ]; then echo 'go version go-test'; exit 0; fi\nexit 2\n"), 0700); err != nil {
		test.Fatal(err)
	}
	broken := filepath.Join(directory, "stub")
	if err := os.WriteFile(broken, []byte("#!/bin/sh\necho 'SDK is not installed' >&2\nexit 1\n"), 0700); err != nil {
		test.Fatal(err)
	}
	report, capabilities := Doctor(config.Config{Tools: map[string]string{"go": working, "xcodebuild": broken, "broken-toolchain": broken}})
	tools := report["tools"].(map[string]any)
	if !tools["go"].(map[string]any)["version_check_succeeded"].(bool) {
		test.Error("Go must be probed using its version subcommand")
	}
	if !slices.Contains(capabilities, "go") {
		test.Error("working Go toolchain is not advertised")
	}
	for _, name := range []string{"xcodebuild", "broken-toolchain"} {
		if slices.Contains(capabilities, name) || tools[name].(map[string]any)["available"].(bool) {
			test.Errorf("nonfunctional %s stub is advertised as available", name)
		}
	}
}
