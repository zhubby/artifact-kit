//go:build !windows

package main

import (
	"context"
	"os"
	"os/signal"
	"syscall"
)

func agentSignalContext() (context.Context, context.CancelFunc) {
	// Network socket failures can generate SIGPIPE. A runtime reconnects
	// through those failures; only process stop signals should stop it.
	return signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
}

func workerSignalContext() (context.Context, context.CancelFunc) {
	// A service manager may terminate the whole group; an exiting Agent also
	// closes stdout. Convert both into cancellation so the Worker can kill
	// its task tree and remove its workspace before returning.
	return signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM, syscall.SIGPIPE)
}
