//go:build windows

package executor

import (
	"os/exec"
	"syscall"
	"unsafe"

	"golang.org/x/sys/windows"
)

type ProcessTree struct {
	job     windows.Handle
	command *exec.Cmd
}

func StartProcess(command *exec.Cmd) (*ProcessTree, error) {
	job, err := windows.CreateJobObject(nil, nil)
	if err != nil {
		return nil, err
	}
	limits := windows.JOBOBJECT_EXTENDED_LIMIT_INFORMATION{}
	limits.BasicLimitInformation.LimitFlags = windows.JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
	if _, err = windows.SetInformationJobObject(job, windows.JobObjectExtendedLimitInformation, uintptr(unsafe.Pointer(&limits)), uint32(unsafe.Sizeof(limits))); err != nil {
		windows.CloseHandle(job)
		return nil, err
	}
	command.SysProcAttr = &syscall.SysProcAttr{CreationFlags: windows.CREATE_SUSPENDED}
	if err = command.Start(); err != nil {
		windows.CloseHandle(job)
		return nil, err
	}
	process, err := windows.OpenProcess(windows.PROCESS_SET_QUOTA|windows.PROCESS_TERMINATE|windows.PROCESS_QUERY_INFORMATION|windows.PROCESS_SUSPEND_RESUME, false, uint32(command.Process.Pid))
	if err != nil {
		command.Process.Kill()
		command.Wait()
		windows.CloseHandle(job)
		return nil, err
	}
	defer windows.CloseHandle(process)
	if err = windows.AssignProcessToJobObject(job, process); err != nil {
		command.Process.Kill()
		command.Wait()
		windows.CloseHandle(job)
		return nil, err
	}
	resume := windows.NewLazySystemDLL("ntdll.dll").NewProc("NtResumeProcess")
	status, _, _ := resume.Call(uintptr(process))
	if status != 0 {
		windows.TerminateJobObject(job, 1)
		command.Wait()
		windows.CloseHandle(job)
		return nil, syscall.Errno(status)
	}
	return &ProcessTree{job: job, command: command}, nil
}

func (tree *ProcessTree) Kill()  { _ = windows.TerminateJobObject(tree.job, 1) }
func (tree *ProcessTree) Close() { tree.Kill(); _ = windows.CloseHandle(tree.job) }
