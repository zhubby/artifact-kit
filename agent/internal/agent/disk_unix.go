//go:build !windows

package agent

import "syscall"

func freeDisk(path string) uint64 {
	var statistics syscall.Statfs_t
	if syscall.Statfs(path, &statistics) != nil {
		return 0
	}
	return uint64(statistics.Bavail) * uint64(statistics.Bsize)
}
