package executor

import (
	"testing"
	"time"
)

func TestLeaseUsesLocalMonotonicDeadline(test *testing.T) {
	lease := NewLease(25 * time.Millisecond)
	if lease.Expired() {
		test.Fatal("fresh lease expired")
	}
	time.Sleep(15 * time.Millisecond)
	lease.Renew(40 * time.Millisecond)
	time.Sleep(20 * time.Millisecond)
	if lease.Expired() {
		test.Fatal("renewal ignored")
	}
	time.Sleep(30 * time.Millisecond)
	if !lease.Expired() {
		test.Fatal("expired lease accepted")
	}
}
