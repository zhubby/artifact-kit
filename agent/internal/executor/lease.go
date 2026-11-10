package executor

import (
	"sync"
	"time"
)

type Lease struct {
	mutex    sync.Mutex
	deadline time.Time
}

func NewLease(duration time.Duration) *Lease { return &Lease{deadline: time.Now().Add(duration)} }

func (lease *Lease) Renew(duration time.Duration) bool {
	lease.mutex.Lock()
	defer lease.mutex.Unlock()
	if !time.Now().Before(lease.deadline) {
		return false
	}
	lease.deadline = time.Now().Add(duration)
	return true
}

func (lease *Lease) Expired() bool {
	lease.mutex.Lock()
	defer lease.mutex.Unlock()
	return !time.Now().Before(lease.deadline)
}
