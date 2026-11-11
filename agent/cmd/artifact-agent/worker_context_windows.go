//go:build windows

package main

import (
	"context"
	"os"
	"os/signal"
)

func agentSignalContext() (context.Context, context.CancelFunc) {
	return workerSignalContext()
}

func workerSignalContext() (context.Context, context.CancelFunc) {
	return signal.NotifyContext(context.Background(), os.Interrupt)
}
