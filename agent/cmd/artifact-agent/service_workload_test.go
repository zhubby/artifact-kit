package main

import (
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"os/user"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	pb "github.com/artifact-kit/agent/gen/agentv1"
)

type workloadReport struct {
	Mode      string `json:"mode"`
	Key       string `json:"key"`
	PID       int    `json:"pid"`
	ParentPID int    `json:"parent_pid"`
	User      string `json:"user"`
	Directory string `json:"directory"`
}

type serviceWorkload struct {
	repository string
	commit     string
	endpoint   string
	mutex      sync.Mutex
	stopping   bool
	reports    map[string]map[string]workloadReport
}

func newServiceWorkload(test *testing.T, private string) *serviceWorkload {
	test.Helper()
	repository := filepath.Join(private, "task fixture")
	if err := os.Mkdir(repository, 0755); err != nil {
		test.Fatal(err)
	}
	name := "workload"
	if runtime.GOOS == "windows" {
		name += ".exe"
	}
	requireNativeCommand(test, nil, false, "go", "build", "-o", filepath.Join(repository, name), "./testdata/service-workload")
	environment := append(os.Environ(), "GIT_CONFIG_NOSYSTEM=1", "GIT_CONFIG_GLOBAL="+os.DevNull)
	requireNativeCommand(test, environment, false, "git", "init", "--template=", repository)
	requireNativeCommand(test, environment, false, "git", "-C", repository, "add", name)
	requireNativeCommand(test, environment, false, "git", "-C", repository, "update-index", "--chmod=+x", name)
	requireNativeCommand(test, environment, false, "git", "-C", repository, "-c", "user.name=Artifact Kit CI", "-c", "user.email=ci@artifact-kit.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "finite native service workload")
	commit := strings.TrimSpace(requireNativeCommand(test, environment, false, "git", "-C", repository, "rev-parse", "HEAD"))
	workload := &serviceWorkload{repository: repository, commit: commit, reports: map[string]map[string]workloadReport{}}
	server := httptest.NewServer(http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		defer request.Body.Close()
		var report workloadReport
		if request.Method != http.MethodPost || json.NewDecoder(http.MaxBytesReader(response, request.Body, 4096)).Decode(&report) != nil {
			http.Error(response, "invalid workload report", http.StatusBadRequest)
			return
		}
		workload.mutex.Lock()
		defer workload.mutex.Unlock()
		if workload.stopping {
			response.WriteHeader(http.StatusGone)
			return
		}
		reports, allowed := workload.reports[report.Key]
		if !allowed || report.PID <= 0 || report.ParentPID <= 0 || (report.Mode != "parent" && report.Mode != "descendant" && report.Mode != "success") {
			http.Error(response, "unknown workload", http.StatusForbidden)
			return
		}
		reports[report.Mode] = report
		response.WriteHeader(http.StatusNoContent)
	}))
	workload.endpoint = server.URL
	test.Cleanup(server.Close)
	return workload
}

func (workload *serviceWorkload) stop() {
	workload.mutex.Lock()
	workload.stopping = true
	workload.mutex.Unlock()
}

func (workload *serviceWorkload) assign(test *testing.T, connection serviceConnection, number int, mode string) string {
	test.Helper()
	id := fmt.Sprintf("00000000-0000-4000-8000-%012d", number)
	workload.mutex.Lock()
	workload.reports[id] = map[string]workloadReport{}
	workload.mutex.Unlock()
	shell, script := "sh", "./workload "+mode
	if runtime.GOOS == "windows" {
		shell, script = "powershell", ".\\workload.exe "+mode
	}
	command := &pb.Command{Kind: "assign", AttemptId: id, LeaseToken: "native-service-" + id, LeaseSeconds: 90, Assignment: &pb.Assignment{
		RepositoryUrl: filepath.ToSlash(workload.repository), CommitSha: workload.commit, TimeoutSeconds: 180,
		Steps:       []*pb.Step{{Name: "native-service-workload", Phase: "build", Shell: shell, Run: script, TimeoutSeconds: 180}},
		Environment: map[string]string{"AK_WORKLOAD_ENDPOINT": workload.endpoint, "AK_WORKLOAD_KEY": id},
	}}
	select {
	case connection.commands <- command:
	case <-connection.closed:
		test.Fatal("service disconnected before task assignment")
	case <-time.After(10 * time.Second):
		test.Fatal("service did not receive task assignment")
	}
	return id
}

func (workload *serviceWorkload) awaitTree(test *testing.T, connection serviceConnection, id string, agentPID int, owner, root string) (func(), int) {
	test.Helper()
	deadline := time.Now().Add(45 * time.Second)
	var parent, child workloadReport
	for time.Now().Before(deadline) {
		workload.mutex.Lock()
		parent, child = workload.reports[id]["parent"], workload.reports[id]["descendant"]
		workload.mutex.Unlock()
		if parent.PID > 0 && child.PID > 0 {
			break
		}
		select {
		case event := <-connection.finished:
			if event.AttemptId == id {
				test.Fatalf("task %s ended before a live descendant: %s\nWorker logs:\n%s", id, event.PayloadJson, connection.peer.taskLogs(id))
			}
		default:
		}
		time.Sleep(50 * time.Millisecond)
	}
	workspace := filepath.Join(root, "work", id, "source")
	if child.PID == 0 || child.ParentPID != parent.PID || parent.Directory != workspace || child.Directory != workspace {
		test.Fatalf("actual pinned Git workload did not start in its managed workspace: parent=%+v child=%+v expected=%s", parent, child, workspace)
	}
	if !sameServiceUser(parent.User, owner) || !sameServiceUser(child.User, owner) {
		test.Fatalf("workload did not use service account %s: parent=%s child=%s", owner, parent.User, child.User)
	}
	type watchedProcess struct {
		pid    int
		exited func() bool
	}
	var processes []watchedProcess
	pid, workerPID := child.PID, 0
	for depth := 0; pid != agentPID && depth < 8; depth++ {
		process := inspectWorkloadProcess(test, pid)
		if process.pid != pid || !sameServiceUser(process.owner, owner) || process.parent <= 0 {
			test.Fatalf("unexpected process ancestry for native workload: %+v", process)
		}
		if strings.Contains(process.command, " worker") {
			workerPID = pid
		}
		processes = append(processes, watchedProcess{pid: pid, exited: watchWorkloadProcess(test, pid)})
		pid = process.parent
	}
	if pid != agentPID || workerPID == 0 || len(processes) < 3 {
		test.Fatalf("workload is not descended from the expected real Agent/Worker: agent=%d ancestor=%d processes=%+v worker=%d", agentPID, pid, processes, workerPID)
	}
	test.Logf("pinned Git task %s started a real Worker/process tree as %s; descendant pid=%d", id, owner, child.PID)
	return func() {
		test.Helper()
		deadline := time.Now().Add(15 * time.Second)
		var alive []int
		for time.Now().Before(deadline) {
			alive = nil
			for _, process := range processes {
				if !process.exited() {
					alive = append(alive, process.pid)
				}
			}
			if len(alive) == 0 {
				path := filepath.Join(root, "work", id)
				if runtime.GOOS == "windows" {
					if _, err := os.Lstat(path); !os.IsNotExist(err) {
						test.Fatalf("Worker left its managed workspace after stopping: %s: %v", path, err)
					}
				} else {
					if output, err := nativeCommand(nil, os.Getenv("AK_NATIVE_SERVICE_TESTS") == "1", "test", "!", "-e", path); err != nil {
						test.Fatalf("all task processes exited but Worker left its managed workspace: %s: %v %s", path, err, output)
					}
				}
				test.Logf("task %s Worker, step and descendant exited; managed workspace removed", id)
				return
			}
			time.Sleep(50 * time.Millisecond)
		}
		test.Fatalf("native task process tree survived service stop/restart: %v", alive)
	}, workerPID
}

func (workload *serviceWorkload) awaitSuccess(test *testing.T, connection serviceConnection, id string) {
	test.Helper()
	timer := time.NewTimer(45 * time.Second)
	defer timer.Stop()
	for {
		select {
		case event := <-connection.finished:
			if event.AttemptId != id {
				continue
			}
			var result struct{ Status string }
			if json.Unmarshal([]byte(event.PayloadJson), &result) != nil || result.Status != "succeeded" {
				test.Fatalf("task after service restart did not succeed: %+v\nWorker logs:\n%s", event, connection.peer.taskLogs(id))
			}
			workload.mutex.Lock()
			report := workload.reports[id]["success"]
			workload.mutex.Unlock()
			if report.PID <= 0 {
				test.Fatal("successful terminal event has no real workload execution")
			}
			test.Log("fresh Agent/Worker completed another pinned Git task after service restart")
			return
		case <-timer.C:
			test.Fatal("task after restart never completed")
		}
	}
}

type workloadProcess struct {
	pid, parent    int
	owner, command string
}

func sameServiceUser(actual, expected string) bool {
	if runtime.GOOS == "windows" {
		_, actual, _ = strings.Cut(actual, `\`)
		if actual == "" {
			return false
		}
		if _, local, qualified := strings.Cut(expected, `\`); qualified {
			expected = local
		}
	}
	return strings.EqualFold(actual, expected)
}

func inspectWorkloadProcess(test *testing.T, pid int) workloadProcess {
	test.Helper()
	if runtime.GOOS == "windows" {
		output := requireNativeCommand(test, nil, false, "pwsh.exe", "-NoProfile", "-NonInteractive", "-Command", fmt.Sprintf(`$ErrorActionPreference='Stop'; $p=Get-CimInstance Win32_Process -Filter 'ProcessId=%d'; if (!$p) { throw 'workload process exited' }; @{pid=[int]$p.ProcessId; parent=[int]$p.ParentProcessId; owner=(Invoke-CimMethod -InputObject $p -MethodName GetOwner).User; command=$p.CommandLine} | ConvertTo-Json -Compress`, pid))
		var result struct {
			PID, Parent    int
			Owner, Command string
		}
		if err := json.Unmarshal([]byte(strings.TrimPrefix(output, "\ufeff")), &result); err != nil {
			test.Fatal(err)
		}
		// Win32_Process.GetOwner returns the local name; report the same form
		// as os/user so the account comparison remains platform-independent.
		return workloadProcess{result.PID, result.Parent, `.\` + result.Owner, result.Command}
	}
	output := requireNativeCommand(test, nil, false, "ps", "-o", "pid=,ppid=,uid=,args=", "-p", strconv.Itoa(pid))
	fields := strings.Fields(output)
	if len(fields) < 4 {
		test.Fatalf("invalid native process inspection: %s", output)
	}
	actualPID, _ := strconv.Atoi(fields[0])
	parent, _ := strconv.Atoi(fields[1])
	owner, err := user.LookupId(fields[2])
	if err != nil {
		test.Fatal(err)
	}
	return workloadProcess{actualPID, parent, owner.Username, strings.Join(fields[3:], " ")}
}
