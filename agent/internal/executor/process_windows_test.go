//go:build windows

package executor

import (
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"

	"golang.org/x/sys/windows"
)

func TestJobObjectTerminatesDescendants(test *testing.T) {
	marker := filepath.Join(test.TempDir(), "child.pid")
	script := `$child = Start-Process powershell -ArgumentList '-NoProfile','-Command','Start-Sleep -Seconds 60' -PassThru; [IO.File]::WriteAllText($env:ARTIFACTORY_TEST_PID_FILE, [string]$child.Id); Start-Sleep -Seconds 60`
	command := exec.Command("powershell", "-NoProfile", "-NonInteractive", "-Command", script)
	command.Env = append(os.Environ(), "ARTIFACTORY_TEST_PID_FILE="+marker)
	tree, err := StartProcess(command)
	if err != nil {
		test.Fatal(err)
	}
	defer tree.Close()
	deadline := time.Now().Add(15 * time.Second)
	var child int
	for time.Now().Before(deadline) {
		if data, err := os.ReadFile(marker); err == nil {
			child, _ = strconv.Atoi(strings.TrimSpace(strings.TrimPrefix(string(data), "\ufeff")))
			if child > 0 {
				break
			}
		}
		time.Sleep(20 * time.Millisecond)
	}
	if child == 0 {
		tree.Kill()
		command.Wait()
		test.Fatal("child did not start")
	}
	handle, err := windows.OpenProcess(windows.SYNCHRONIZE, false, uint32(child))
	if err != nil {
		tree.Kill()
		command.Wait()
		test.Fatal(err)
	}
	defer windows.CloseHandle(handle)
	tree.Kill()
	_ = command.Wait()
	result, err := windows.WaitForSingleObject(handle, 5000)
	if err != nil || result != windows.WAIT_OBJECT_0 {
		test.Fatalf("descendant survived Job Object cancellation: %v %v", result, err)
	}
}
