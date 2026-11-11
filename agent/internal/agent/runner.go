package agent

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"sync"
	"time"

	pb "github.com/zhubby/artifact-kit/agent/gen/agentv1"
	"github.com/zhubby/artifact-kit/agent/internal/client"
	"github.com/zhubby/artifact-kit/agent/internal/config"
	"github.com/zhubby/artifact-kit/agent/internal/ota"
	"github.com/zhubby/artifact-kit/agent/internal/worker"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

type task struct {
	id            string
	token         string
	assignment    *pb.Assignment
	deadline      time.Time
	started       bool
	encoder       *json.Encoder
	pipe          io.WriteCloser
	pipeMutex     sync.Mutex
	pending       []*pb.AgentEvent
	sequence      uint64
	logSequence   uint64
	finished      bool
	finishedAt    time.Time
	bufferError   string
	logsAbandoned bool
}

func (task *task) control(input worker.Input) {
	task.pipeMutex.Lock()
	defer task.pipeMutex.Unlock()
	if task.encoder != nil {
		_ = task.encoder.Encode(input)
	}
}

type Runner struct {
	config          config.Config
	mutex           sync.Mutex
	tasks           map[string]*task
	anchors         map[string]time.Time
	notify          chan struct{}
	spoolMutex      sync.Mutex
	spoolBytes      int64
	diagnostics     map[string]any
	capabilities    []string
	workers         int
	workerExit      sync.WaitGroup
	stopping        bool
	otaSession      string
	otaHealthAt     time.Time
	otaDrainRequest string
	otaDrainUpdate  string
}

func Run(ctx context.Context, cfg config.Config) error {
	if cfg.AgentID == "" {
		return errors.New("agent must be registered first")
	}
	if err := os.MkdirAll(filepath.Join(cfg.Root, "spool"), 0700); err != nil {
		return err
	}
	diagnostics, capabilities := Doctor(cfg)
	if os.Getenv("ARTIFACTORY_OTA_DIRECTORY") != "" {
		capabilities = append(capabilities, "agent-ota-v1")
	}
	runner := &Runner{config: cfg, tasks: map[string]*task{}, anchors: map[string]time.Time{}, notify: make(chan struct{}, 1), diagnostics: diagnostics, capabilities: capabilities}
	_ = filepath.WalkDir(filepath.Join(cfg.Root, "spool"), func(_ string, entry os.DirEntry, err error) error {
		if err == nil && !entry.IsDir() {
			if info, err := entry.Info(); err == nil {
				runner.spoolBytes += info.Size()
			}
		}
		return nil
	})
	logConnection, err := client.Dial(cfg, false)
	if err != nil {
		return err
	}
	defer logConnection.Close()
	logContext, stopLogs := context.WithCancel(context.WithoutCancel(ctx))
	defer stopLogs()
	go runner.logLoop(logContext, pb.NewControlClient(logConnection))
	go runner.leaseLoop(ctx)
	defer runner.stop()
	delay := time.Second
	for ctx.Err() == nil {
		connection, err := client.Dial(cfg, false)
		if err == nil {
			err = runner.session(ctx, pb.NewControlClient(connection))
			connection.Close()
		}
		if ctx.Err() != nil {
			break
		}
		fmt.Fprintf(os.Stderr, "control connection interrupted (%s); reconnecting\n", status.Code(err))
		select {
		case <-ctx.Done():
			return nil
		case <-time.After(delay):
		}
		if delay < 30*time.Second {
			delay = min(delay*2, 30*time.Second)
		}
	}
	return nil
}

func (runner *Runner) wake() {
	select {
	case runner.notify <- struct{}{}:
	default:
	}
}
func identifier() string {
	var bytes [16]byte
	_, _ = rand.Read(bytes[:])
	return hex.EncodeToString(bytes[:])
}

func (runner *Runner) heartbeat(kind string) *pb.AgentEvent {
	runner.mutex.Lock()
	defer runner.mutex.Unlock()
	active := []map[string]string{}
	for _, task := range runner.tasks {
		active = append(active, map[string]string{"attempt_id": task.id, "lease_token": task.token})
	}
	diagnostics := map[string]any{}
	for key, value := range runner.diagnostics {
		diagnostics[key] = value
	}
	diagnostics["metrics"] = metrics(runner.config)
	runner.spoolMutex.Lock()
	diagnostics["buffered_log_bytes"] = runner.spoolBytes
	runner.spoolMutex.Unlock()
	diagnostics["agent_commit"] = Commit
	request := identifier()
	data, _ := json.Marshal(map[string]any{"active_attempts": active, "capabilities": runner.capabilities, "diagnostics": diagnostics, "ota": runner.otaSnapshot(request)})
	runner.anchors[request] = time.Now()
	for key, value := range runner.anchors {
		if time.Since(value) > 2*time.Minute {
			delete(runner.anchors, key)
		}
	}
	return &pb.AgentEvent{AgentId: runner.config.AgentID, Kind: kind, PayloadJson: string(data), ProtocolMajor: 1, CommandId: request}
}

func (runner *Runner) session(ctx context.Context, control pb.ControlClient) error {
	stopRequested := ctx.Done()
	ctx, cancel := context.WithCancel(context.WithoutCancel(ctx))
	defer cancel()
	stream, err := control.Connect(ctx)
	if err != nil {
		return err
	}
	runner.otaSession = identifier()
	if directory := os.Getenv("ARTIFACTORY_OTA_DIRECTORY"); directory != "" {
		_ = ota.Confirm(directory, runner.otaSession, "")
	}
	if err = stream.Send(runner.heartbeat("hello")); err != nil {
		return err
	}
	commands := make(chan *pb.Command, 32)
	failures := make(chan error, 1)
	go func() {
		for {
			command, err := stream.Recv()
			if err != nil {
				failures <- err
				return
			}
			select {
			case commands <- command:
			case <-ctx.Done():
				return
			}
		}
	}()
	beat := time.NewTicker(10 * time.Second)
	defer beat.Stop()
	runner.wake()
	var drainTimeout <-chan time.Time
	draining := false
	for {
		if draining && runner.settled() {
			return nil
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-stopRequested:
			// Keep this authenticated stream and log transport alive while
			// Workers cancel and their final events are acknowledged.
			runner.stop()
			stopRequested = nil
			draining = true
			drainTimeout = time.After(5 * time.Second)
			runner.wake()
		case <-drainTimeout:
			return nil
		case err := <-failures:
			return err
		case command := <-commands:
			if err = runner.command(command); err != nil {
				return err
			}
			runner.wake()
		case <-beat.C:
			if err = stream.Send(runner.heartbeat("heartbeat")); err != nil {
				return err
			}
			if err = runner.sendPending(stream); err != nil {
				return err
			}
		case <-runner.notify:
			if err = runner.sendPending(stream); err != nil {
				return err
			}
		}
	}
}

func (runner *Runner) sendPending(stream pb.Control_ConnectClient) error {
	runner.mutex.Lock()
	tasks := make([]*task, 0, len(runner.tasks))
	for _, task := range runner.tasks {
		tasks = append(tasks, task)
	}
	runner.mutex.Unlock()
	for _, current := range tasks {
		runner.mutex.Lock()
		if !current.started {
			if runner.stopping {
				runner.mutex.Unlock()
				continue
			}
			request := identifier()
			runner.anchors[request] = time.Now()
			event := &pb.AgentEvent{AgentId: runner.config.AgentID, Kind: "accept", AttemptId: current.id, LeaseToken: current.token, CommandId: request}
			runner.mutex.Unlock()
			if err := stream.Send(event); err != nil {
				return err
			}
			continue
		}
		events := append([]*pb.AgentEvent(nil), current.pending...)
		runner.mutex.Unlock()
		for _, event := range events {
			runner.mutex.Lock()
			abandoned := current.logsAbandoned
			runner.mutex.Unlock()
			if event.Kind == "finished" && !abandoned && !runner.spoolEmpty(current.id) {
				break
			}
			if err := stream.Send(event); err != nil {
				return err
			}
		}
	}
	return nil
}

func (runner *Runner) command(command *pb.Command) error {
	if handled, err := runner.otaCommand(command); handled {
		return err
	}
	runner.mutex.Lock()
	current := runner.tasks[command.AttemptId]
	if command.Kind == "assign" {
		if !runner.stopping && current == nil && command.Assignment != nil && len(runner.tasks) < 32 {
			runner.tasks[command.AttemptId] = &task{id: command.AttemptId, token: command.LeaseToken, assignment: command.Assignment, deadline: time.Now().Add(time.Duration(command.LeaseSeconds) * time.Second)}
		}
		runner.mutex.Unlock()
		return nil
	}
	if current == nil {
		runner.mutex.Unlock()
		return nil
	}
	if command.Kind == "event_ack" {
		remaining := current.pending[:0]
		terminal := false
		for _, event := range current.pending {
			if event.Sequence > command.AckSequence {
				remaining = append(remaining, event)
			} else if event.Kind == "finished" {
				terminal = true
			}
		}
		current.pending = remaining
		if terminal {
			delete(runner.tasks, current.id)
		}
		runner.mutex.Unlock()
		if terminal {
			current.pipeMutex.Lock()
			if current.pipe != nil {
				current.pipe.Close()
			}
			current.pipeMutex.Unlock()
		}
		return nil
	}
	if command.LeaseToken != current.token {
		runner.mutex.Unlock()
		return errors.New("lease token mismatch")
	}
	switch command.Kind {
	case "start", "renew":
		if runner.stopping && !current.started {
			runner.mutex.Unlock()
			return nil
		}
		anchor, known := runner.anchors[command.RequestId]
		if !known {
			runner.mutex.Unlock()
			return nil
		}
		deadline := anchor.Add(time.Duration(command.LeaseSeconds) * time.Second)
		if !time.Now().Before(current.deadline) || !time.Now().Before(deadline) {
			runner.mutex.Unlock()
			runner.drop(current, "execution lease expired")
			return nil
		}
		current.deadline = deadline
		remaining := uint32(time.Until(deadline) / time.Second)
		start := command.Kind == "start" && !current.started
		if start {
			current.started = true
		}
		runner.mutex.Unlock()
		if start {
			runner.launch(current, remaining)
		} else {
			current.control(worker.Input{Kind: "renew", LeaseSeconds: remaining})
		}
	case "cancel":
		runner.mutex.Unlock()
		if command.Message == "lease_invalid" {
			runner.drop(current, "execution lease expired or revoked")
		} else {
			current.control(worker.Input{Kind: "cancel", Reason: "cancel requested"})
		}
	default:
		runner.mutex.Unlock()
	}
	return nil
}

func (runner *Runner) launch(current *task, leaseSeconds uint32) {
	executable, err := os.Executable()
	if err != nil {
		runner.event(current, worker.Event{Kind: "finished", Payload: map[string]any{"status": "failed", "error": "worker executable unavailable", "failure_kind": "agent"}})
		return
	}
	command := exec.Command(executable, "worker")
	input, err := command.StdinPipe()
	if err != nil {
		runner.failedStart(current)
		return
	}
	output, err := command.StdoutPipe()
	if err != nil {
		input.Close()
		runner.failedStart(current)
		return
	}
	command.Stderr = os.Stderr
	if err = command.Start(); err != nil {
		input.Close()
		runner.failedStart(current)
		return
	}
	runner.mutex.Lock()
	runner.workers++
	runner.mutex.Unlock()
	runner.workerExit.Add(1)
	current.pipeMutex.Lock()
	current.pipe = input
	current.encoder = json.NewEncoder(input)
	err = current.encoder.Encode(worker.Input{Kind: "start", Config: runner.config, AttemptID: current.id, LeaseToken: current.token, LeaseSeconds: leaseSeconds, Assignment: current.assignment})
	current.pipeMutex.Unlock()
	if err != nil {
		input.Close()
	}
	go func() {
		defer runner.workerExit.Done()
		decoder := json.NewDecoder(output)
		for {
			var event worker.Event
			if err := decoder.Decode(&event); err != nil {
				break
			}
			if event.Kind == "log" {
				if err := runner.spool(current, event); err != nil {
					runner.mutex.Lock()
					current.bufferError = "log buffer exhausted or unavailable"
					runner.mutex.Unlock()
					current.control(worker.Input{Kind: "cancel", Reason: "log buffer exhausted or unavailable"})
				}
			} else {
				runner.event(current, event)
			}
		}
		_ = command.Wait()
		runner.mutex.Lock()
		runner.workers--
		finished := current.finished
		runner.mutex.Unlock()
		if !finished {
			runner.event(current, worker.Event{Kind: "finished", Payload: map[string]any{"status": "failed", "error": "worker exited without a terminal result", "failure_kind": "agent"}})
		}
	}()
}

func (runner *Runner) failedStart(current *task) {
	runner.event(current, worker.Event{Kind: "finished", Payload: map[string]any{"status": "failed", "error": "unable to start isolated worker", "failure_kind": "agent"}})
}

func (runner *Runner) event(current *task, event worker.Event) {
	runner.mutex.Lock()
	defer runner.mutex.Unlock()
	if runner.tasks[current.id] != current || current.finished {
		return
	}
	if event.Kind == "finished" {
		current.finished = true
		current.finishedAt = time.Now()
		if current.bufferError != "" {
			event.Payload = map[string]any{"status": "failed", "error": current.bufferError}
		}
	}
	current.sequence++
	data, _ := json.Marshal(event.Payload)
	current.pending = append(current.pending, &pb.AgentEvent{AgentId: runner.config.AgentID, Kind: event.Kind, AttemptId: current.id, LeaseToken: current.token, Sequence: current.sequence, PayloadJson: string(data)})
	runner.wake()
}

func (runner *Runner) drop(current *task, reason string) {
	runner.mutex.Lock()
	delete(runner.tasks, current.id)
	runner.mutex.Unlock()
	current.control(worker.Input{Kind: "cancel", Reason: reason})
	current.pipeMutex.Lock()
	if current.pipe != nil {
		current.pipe.Close()
	}
	current.pipeMutex.Unlock()
	path := filepath.Join(runner.config.Root, "lost")
	_ = os.MkdirAll(path, 0700)
	data, _ := json.Marshal(map[string]any{"attempt_id": current.id, "reason": reason, "time": time.Now().UTC()})
	_ = os.WriteFile(filepath.Join(path, current.id+".json"), data, 0600)
}

func (runner *Runner) leaseLoop(ctx context.Context) {
	ticker := time.NewTicker(time.Second)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			runner.mutex.Lock()
			var expired []*task
			for _, current := range runner.tasks {
				if !time.Now().Before(current.deadline) {
					expired = append(expired, current)
				}
				if current.finished && time.Since(current.finishedAt) > 120*time.Second && !current.logsAbandoned {
					current.logsAbandoned = true
					for _, event := range current.pending {
						if event.Kind == "finished" {
							event.PayloadJson = `{"status":"failed","error":"log delivery timed out; unacknowledged chunks retained on agent"}`
						}
					}
					runner.wake()
				}
			}
			runner.mutex.Unlock()
			for _, current := range expired {
				runner.drop(current, "execution lease expired")
			}
		}
	}
}

func (runner *Runner) stop() {
	runner.mutex.Lock()
	runner.stopping = true
	tasks := make([]*task, 0, len(runner.tasks))
	for _, current := range runner.tasks {
		tasks = append(tasks, current)
	}
	runner.mutex.Unlock()
	for _, current := range tasks {
		current.control(worker.Input{Kind: "cancel", Reason: "agent stopping"})
		current.pipeMutex.Lock()
		if current.pipe != nil {
			current.pipe.Close()
		}
		current.pipeMutex.Unlock()
	}
	// Workers finish cancellation, process-tree cleanup and workspace removal
	// before the runtime exits. The service launcher bounds a stuck shutdown.
	runner.workerExit.Wait()
}

func (runner *Runner) settled() bool {
	runner.mutex.Lock()
	defer runner.mutex.Unlock()
	for _, current := range runner.tasks {
		if current.started {
			return false
		}
	}
	runner.spoolMutex.Lock()
	defer runner.spoolMutex.Unlock()
	return runner.workers == 0 && runner.spoolBytes == 0
}

type logRecord struct {
	Sequence uint64 `json:"sequence"`
	Step     string `json:"step"`
	Data     []byte `json:"data"`
}

func (runner *Runner) spool(current *task, event worker.Event) error {
	runner.spoolMutex.Lock()
	defer runner.spoolMutex.Unlock()
	record := logRecord{Sequence: current.logSequence + 1, Step: event.Step, Data: event.Data}
	data, err := json.Marshal(record)
	if err != nil {
		return err
	}
	if runner.spoolBytes+int64(len(data)) > runner.config.LogBufferBytes {
		return errors.New("log buffer exhausted")
	}
	directory := filepath.Join(runner.config.Root, "spool", current.id)
	if err = os.MkdirAll(directory, 0700); err != nil {
		return err
	}
	if err = os.WriteFile(filepath.Join(directory, fmt.Sprintf("%020d.json", record.Sequence)), data, 0600); err != nil {
		return err
	}
	current.logSequence = record.Sequence
	runner.spoolBytes += int64(len(data))
	return nil
}

func (runner *Runner) spoolEmpty(id string) bool {
	runner.spoolMutex.Lock()
	defer runner.spoolMutex.Unlock()
	files, err := os.ReadDir(filepath.Join(runner.config.Root, "spool", id))
	return os.IsNotExist(err) || (err == nil && len(files) == 0)
}

func (runner *Runner) logLoop(ctx context.Context, control pb.ControlClient) {
	ticker := time.NewTicker(250 * time.Millisecond)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			runner.mutex.Lock()
			tasks := make([]*task, 0, len(runner.tasks))
			for _, current := range runner.tasks {
				tasks = append(tasks, current)
			}
			runner.mutex.Unlock()
			for _, current := range tasks {
				if err := runner.flush(ctx, control, current); err != nil && status.Code(err) != codes.Unavailable && status.Code(err) != codes.DeadlineExceeded && status.Code(err) != codes.Canceled {
					fmt.Fprintf(os.Stderr, "log upload pending (%s)\n", status.Code(err))
				}
			}
		}
	}
}

func (runner *Runner) flush(ctx context.Context, control pb.ControlClient, current *task) error {
	directory := filepath.Join(runner.config.Root, "spool", current.id)
	runner.spoolMutex.Lock()
	files, err := os.ReadDir(directory)
	if os.IsNotExist(err) {
		runner.spoolMutex.Unlock()
		return nil
	}
	if err != nil {
		runner.spoolMutex.Unlock()
		return err
	}
	sort.Slice(files, func(first, second int) bool { return files[first].Name() < files[second].Name() })
	if len(files) > 32 {
		files = files[:32]
	}
	var records []logRecord
	for _, file := range files {
		data, err := os.ReadFile(filepath.Join(directory, file.Name()))
		if err != nil {
			runner.spoolMutex.Unlock()
			return err
		}
		var record logRecord
		if err = json.Unmarshal(data, &record); err != nil {
			runner.spoolMutex.Unlock()
			return err
		}
		records = append(records, record)
	}
	runner.spoolMutex.Unlock()
	if len(records) == 0 {
		return nil
	}
	ctx, cancel := context.WithTimeout(ctx, 20*time.Second)
	defer cancel()
	stream, err := control.UploadLogs(ctx)
	if err != nil {
		return err
	}
	for _, record := range records {
		if err = stream.Send(&pb.LogChunk{AttemptId: current.id, LeaseToken: current.token, Sequence: record.Sequence, Step: record.Step, Data: record.Data}); err != nil {
			return err
		}
	}
	ack, err := stream.CloseAndRecv()
	if err != nil {
		return err
	}
	runner.spoolMutex.Lock()
	for index, record := range records {
		if record.Sequence <= ack.ThroughSequence {
			path := filepath.Join(directory, files[index].Name())
			if info, err := os.Stat(path); err == nil {
				if os.Remove(path) == nil {
					runner.spoolBytes -= info.Size()
				}
			}
		}
	}
	runner.spoolMutex.Unlock()
	runner.wake()
	return nil
}
