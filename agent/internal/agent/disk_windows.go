//go:build windows

package agent

import "golang.org/x/sys/windows"

func freeDisk(path string) uint64 {
	name, err := windows.UTF16PtrFromString(path)
	if err != nil {
		return 0
	}
	var available uint64
	if windows.GetDiskFreeSpaceEx(name, &available, nil, nil) != nil {
		return 0
	}
	return available
}
