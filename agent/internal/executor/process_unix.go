//go:build !windows

package executor

import (
	"os/exec"
	"syscall"
)

type ProcessTree struct{ command *exec.Cmd }

func StartProcess(command *exec.Cmd) (*ProcessTree, error) {
	command.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	if err := command.Start(); err != nil {
		return nil, err
	}
	return &ProcessTree{command: command}, nil
}

func (tree *ProcessTree) Kill() {
	if tree.command.Process != nil {
		_ = syscall.Kill(-tree.command.Process.Pid, syscall.SIGKILL)
	}
}

func (tree *ProcessTree) Close() { tree.Kill() }
