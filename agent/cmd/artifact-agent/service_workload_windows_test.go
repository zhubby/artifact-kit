//go:build windows

package main

import (
	"testing"

	"golang.org/x/sys/windows"
)

func watchWorkloadProcess(test *testing.T, pid int) func() bool {
	test.Helper()
	handle, err := windows.OpenProcess(windows.SYNCHRONIZE, false, uint32(pid))
	if err != nil {
		test.Fatalf("open live workload process %d: %v", pid, err)
	}
	test.Cleanup(func() { windows.CloseHandle(handle) })
	if result, err := windows.WaitForSingleObject(handle, 0); err != nil || result != uint32(windows.WAIT_TIMEOUT) {
		test.Fatalf("workload process %d is not alive at stop checkpoint: %v %v", pid, result, err)
	}
	return func() bool {
		result, err := windows.WaitForSingleObject(handle, 0)
		return err == nil && result == windows.WAIT_OBJECT_0
	}
}
