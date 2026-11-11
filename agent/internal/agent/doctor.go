package agent

import (
	"context"
	"os"
	"os/exec"
	"runtime"
	"sort"
	"strings"
	"time"

	"github.com/zhubby/artifact-kit/agent/internal/config"
)

var Version = "0.1.0"
var Commit = "development"

func Doctor(cfg config.Config) (map[string]any, []string) {
	names := []string{"git", "git-lfs", "bash", "sh", "powershell", "pwsh", "cc", "cargo", "go", "node", "npm", "xcodebuild", "java", "gradle", "cmake", "ninja", "dotnet", "msbuild", "signtool", "codesign", "security", "apksigner", "docker"}
	for name := range cfg.Tools {
		found := false
		for _, existing := range names {
			if existing == name {
				found = true
			}
		}
		if !found {
			names = append(names, name)
		}
	}
	tools := map[string]any{}
	capabilities := []string{"source-cache-v1", "external-build-plan-v1"}
	for _, name := range names {
		executable := name
		if override := cfg.Tools[name]; override != "" {
			executable = override
		}
		path, err := exec.LookPath(executable)
		if err != nil {
			tools[name] = map[string]any{"available": false}
			continue
		}
		arguments := []string{"--version"}
		switch name {
		case "go", "apksigner":
			arguments = []string{"version"}
		case "xcodebuild", "java", "msbuild":
			arguments = []string{"-version"}
		case "codesign":
			arguments = []string{"--help"}
		case "security":
			arguments = []string{"help"}
		case "signtool":
			arguments = []string{"/?"}
		case "powershell", "pwsh":
			arguments = []string{"-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "$PSVersionTable.PSVersion.ToString()"}
		}
		ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
		output, versionError := exec.CommandContext(ctx, path, arguments...).CombinedOutput()
		cancel()
		version := strings.TrimSpace(string(output))
		if len(version) > 512 {
			version = version[:512]
		}
		exitError, exited := versionError.(*exec.ExitError)
		probeSucceeded := versionError == nil || (name == "codesign" && exited && exitError.ExitCode() == 2 && strings.HasPrefix(version, "Usage: codesign"))
		tools[name] = map[string]any{"available": probeSucceeded, "path": path, "version": version, "version_check_succeeded": versionError == nil, "probe_succeeded": probeSucceeded, "probe_arguments": arguments}
		if probeSucceeded {
			capabilities = append(capabilities, name)
		}
	}
	sort.Strings(capabilities)
	hostname, _ := os.Hostname()
	profiles := []string{}
	for profile := range cfg.SigningProfiles {
		profiles = append(profiles, profile)
	}
	sort.Strings(profiles)
	return map[string]any{"agent_version": Version, "protocol_major": 1, "hostname": hostname, "os": runtime.GOOS, "arch": runtime.GOARCH, "tools": tools, "configured_signing_profiles": profiles}, capabilities
}

func metrics(cfg config.Config) map[string]any {
	var memory runtime.MemStats
	runtime.ReadMemStats(&memory)
	return map[string]any{"logical_cpus": runtime.NumCPU(), "agent_heap_bytes": memory.Alloc, "managed_free_bytes": freeDisk(cfg.Root), "managed_root": cfg.Root}
}
