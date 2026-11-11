package worker

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"time"

	pb "github.com/zhubby/artifact-kit/agent/gen/agentv1"
	"github.com/zhubby/artifact-kit/agent/internal/client"
	"github.com/zhubby/artifact-kit/agent/internal/config"
	"github.com/zhubby/artifact-kit/agent/internal/executor"
)

type Input struct {
	Kind         string         `json:"kind"`
	Config       config.Config  `json:"config,omitempty"`
	AttemptID    string         `json:"attempt_id,omitempty"`
	LeaseToken   string         `json:"lease_token,omitempty"`
	LeaseSeconds uint32         `json:"lease_seconds,omitempty"`
	Assignment   *pb.Assignment `json:"assignment,omitempty"`
	Reason       string         `json:"reason,omitempty"`
}

type Event struct {
	Kind    string         `json:"kind"`
	Payload map[string]any `json:"payload,omitempty"`
	Step    string         `json:"step,omitempty"`
	Data    []byte         `json:"data,omitempty"`
}

type emitter struct {
	mutex   sync.Mutex
	encoder *json.Encoder
}

func (output *emitter) emit(event Event) {
	output.mutex.Lock()
	defer output.mutex.Unlock()
	_ = output.encoder.Encode(event)
}

type logWriter struct {
	output *emitter
	step   string
}

func (writer logWriter) Write(data []byte) (int, error) {
	length := len(data)
	for len(data) > 0 {
		size := min(len(data), 32*1024)
		writer.output.emit(Event{Kind: "log", Step: writer.step, Data: append([]byte(nil), data[:size]...)})
		data = data[size:]
	}
	return length, nil
}

func Run(processContext context.Context, input io.Reader, output io.Writer) error {
	decoder := json.NewDecoder(input)
	var initial Input
	if err := decoder.Decode(&initial); err != nil {
		return err
	}
	if initial.Kind != "start" || initial.Assignment == nil || !regexp.MustCompile(`^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$`).MatchString(initial.AttemptID) || initial.LeaseSeconds == 0 {
		return errors.New("invalid worker assignment")
	}
	emit := &emitter{encoder: json.NewEncoder(output)}
	parent, cancel := context.WithCancelCause(processContext)
	defer cancel(nil)
	ctx, stop := context.WithTimeout(parent, time.Duration(initial.Assignment.TimeoutSeconds)*time.Second)
	defer stop()
	lease := executor.NewLease(time.Duration(initial.LeaseSeconds) * time.Second)
	go func() {
		for {
			var command Input
			if err := decoder.Decode(&command); err != nil {
				cancel(errors.New("agent control pipe closed"))
				return
			}
			switch command.Kind {
			case "renew":
				if !lease.Renew(time.Duration(command.LeaseSeconds) * time.Second) {
					cancel(errors.New("execution lease expired"))
					return
				}
			case "cancel":
				cancel(fmt.Errorf("%s", command.Reason))
				return
			}
		}
	}()
	go func() {
		ticker := time.NewTicker(100 * time.Millisecond)
		defer ticker.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
				if lease.Expired() {
					cancel(errors.New("execution lease expired"))
					return
				}
			}
		}
	}()
	err := execute(ctx, initial, emit)
	status := "succeeded"
	message := ""
	failureKind := "execution"
	if err != nil {
		var failure *executionFailure
		if errors.As(err, &failure) {
			failureKind = failure.kind
		}
		status = "failed"
		message = err.Error()
		if cause := context.Cause(ctx); cause != nil {
			message = cause.Error()
			if message == "cancel requested" {
				status = "canceled"
			}
		}
	}
	emit.emit(Event{Kind: "finished", Payload: map[string]any{"status": status, "error": message, "failure_kind": failureKind}})
	return err
}

type executionFailure struct {
	kind  string
	cause error
}

func (failure *executionFailure) Error() string { return failure.cause.Error() }
func (failure *executionFailure) Unwrap() error { return failure.cause }

func execute(ctx context.Context, input Input, emit *emitter) error {
	assignment := input.Assignment
	base := filepath.Join(input.Config.Root, "work", input.AttemptID)
	if err := os.MkdirAll(filepath.Dir(base), 0700); err != nil {
		return err
	}
	if err := os.Mkdir(base, 0700); err != nil {
		return errors.New("attempt workspace already exists; refusing duplicate execution")
	}
	defer os.RemoveAll(base)
	workspace := filepath.Join(base, "source")
	if err := checkout(ctx, input, base, workspace, emit); err != nil {
		return &executionFailure{"source", err}
	}
	scripts := filepath.Join(base, "scripts")
	if err := os.Mkdir(scripts, 0700); err != nil {
		return err
	}
	for _, script := range assignment.Scripts {
		if !regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9._-]{0,119}$`).MatchString(script.Name) {
			return errors.New("invalid external script name")
		}
		digest := sha256.Sum256([]byte(script.Content))
		if hex.EncodeToString(digest[:]) != script.Sha256 {
			return errors.New("external script digest mismatch")
		}
		if err := os.WriteFile(filepath.Join(scripts, script.Name), []byte(script.Content), 0700); err != nil {
			return err
		}
	}
	for index, step := range assignment.Steps {
		if err := ctx.Err(); err != nil {
			return err
		}
		environment := append([]string(nil), os.Environ()...)
		if assignment.Source != nil {
			environment = buildEnvironment()
		}
		for key, value := range assignment.Environment {
			environment = append(environment, key+"="+value)
		}
		environment = append(environment, "ARTIFACTORY_SCRIPT_DIR="+scripts, "ARTIFACTORY_WORK_DIR="+base, "GIT_TERMINAL_PROMPT=0", "GIT_CONFIG_NOSYSTEM=1", "GIT_CONFIG_GLOBAL="+os.DevNull)
		if step.Phase == "sign" || step.Phase == "verify" {
			for _, name := range assignment.SigningProfiles {
				profile, exists := input.Config.SigningProfiles[name]
				if !exists {
					return &executionFailure{"authorization", fmt.Errorf("local signing profile unavailable: %s", name)}
				}
				for key, value := range profile {
					environment = append(environment, key+"="+value)
				}
			}
		}
		emit.emit(Event{Kind: "step_started", Payload: map[string]any{"index": index}})
		command, err := shell(step.Shell, step.Run, base, index)
		if err != nil {
			return err
		}
		command.Dir = workspace
		command.Env = environment
		command.Stdout = logWriter{emit, step.Name}
		command.Stderr = command.Stdout
		stepContext, cancel := context.WithTimeout(ctx, time.Duration(step.TimeoutSeconds)*time.Second)
		err = runCommand(stepContext, command)
		if sourceError := verifySource(stepContext, input, workspace); sourceError != nil {
			err = sourceError
		}
		cancel()
		exit := 0
		if err != nil {
			exit = -1
			var processError *exec.ExitError
			if errors.As(err, &processError) {
				exit = processError.ExitCode()
			}
		}
		emit.emit(Event{Kind: "step_finished", Payload: map[string]any{"index": index, "exit_code": exit}})
		if err != nil {
			return fmt.Errorf("step %q failed: %w", step.Name, err)
		}
	}
	files, err := executor.Collect(workspace, assignment.Artifacts)
	if err != nil {
		return err
	}
	connection, err := client.Dial(input.Config, false)
	if err != nil {
		return err
	}
	defer connection.Close()
	control := pb.NewControlClient(connection)
	for _, path := range files {
		if err = upload(ctx, input, control, workspace, path); err != nil {
			return &executionFailure{"artifact_transport", err}
		}
	}
	return nil
}

func shell(name, script, directory string, index int) (*exec.Cmd, error) {
	switch name {
	case "bash":
		return exec.Command("bash", "--noprofile", "--norc", "-e", "-o", "pipefail", "-c", script), nil
	case "sh":
		return exec.Command("sh", "-e", "-c", script), nil
	case "powershell", "pwsh":
		path := filepath.Join(directory, fmt.Sprintf("step-%d.ps1", index))
		contents := "$ErrorActionPreference = 'Stop'\r\n" + script + "\r\nif ($LASTEXITCODE -and $LASTEXITCODE -ne 0) { exit $LASTEXITCODE }\r\n"
		if err := os.WriteFile(path, []byte(contents), 0600); err != nil {
			return nil, err
		}
		return exec.Command(name, "-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", path), nil
	default:
		return nil, errors.New("unsupported shell")
	}
}

func runCommand(ctx context.Context, command *exec.Cmd) error {
	command.WaitDelay = 2 * time.Second
	tree, err := executor.StartProcess(command)
	if err != nil {
		return err
	}
	defer tree.Close()
	done := make(chan error, 1)
	go func() { done <- command.Wait() }()
	select {
	case err := <-done:
		return err
	case <-ctx.Done():
		tree.Kill()
		<-done
		return ctx.Err()
	}
}

func checkout(ctx context.Context, input Input, base, workspace string, emit *emitter) error {
	if input.Assignment.Source != nil {
		return checkoutPlatform(ctx, input, base, workspace, emit)
	}
	authDir := filepath.Join(base, "auth")
	if err := os.Mkdir(authDir, 0700); err != nil {
		return err
	}
	defer os.RemoveAll(authDir)
	environment := append(os.Environ(), "GIT_TERMINAL_PROMPT=0", "GIT_CONFIG_NOSYSTEM=1", "GIT_CONFIG_GLOBAL="+os.DevNull)
	if input.Config.GitCAFile != "" {
		environment = append(environment, "GIT_SSL_CAINFO="+input.Config.GitCAFile)
	}
	if auth := input.Assignment.GitCredential; auth != nil {
		switch auth.Kind {
		case "https":
			executable, err := os.Executable()
			if err != nil {
				return err
			}
			environment = append(environment, "GIT_ASKPASS="+executable, "ARTIFACTORY_GIT_ASKPASS=1", "ARTIFACTORY_GIT_USERNAME="+auth.Username, "ARTIFACTORY_GIT_PASSWORD="+auth.Secret)
		case "ssh":
			if auth.KnownHosts == "" {
				return errors.New("SSH host trust is required")
			}
			key := filepath.Join(authDir, "identity")
			hosts := filepath.Join(authDir, "known_hosts")
			if err := os.WriteFile(key, []byte(auth.Secret), 0600); err != nil {
				return err
			}
			if err := os.WriteFile(hosts, []byte(auth.KnownHosts), 0600); err != nil {
				return err
			}
			// Preserve quotes for OpenSSH's second parse of the -o option value.
			trustedHosts := `UserKnownHostsFile="` + strings.NewReplacer(`\`, `\\`, `"`, `\"`, `%`, `%%`).Replace(filepath.ToSlash(hosts)) + `"`
			environment = append(environment, "GIT_SSH_COMMAND=ssh -F "+quote(os.DevNull)+" -i "+quote(filepath.ToSlash(key))+" -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes -o "+quote(trustedHosts))
		default:
			return errors.New("unsupported Git credential")
		}
	}
	commit := input.Assignment.CommitSha
	if !regexp.MustCompile(`^[0-9a-f]{40}([0-9a-f]{24})?$`).MatchString(commit) {
		return errors.New("invalid pinned commit")
	}
	commands := [][]string{{"init", workspace}, {"-C", workspace, "fetch", "--depth=1", "--no-tags", "--", input.Assignment.RepositoryUrl, commit}, {"-C", workspace, "checkout", "--detach", commit}}
	for _, arguments := range commands {
		command := exec.Command("git", arguments...)
		command.Env = environment
		command.Stdout = logWriter{emit, "checkout"}
		command.Stderr = command.Stdout
		checkoutContext, cancel := context.WithTimeout(ctx, 120*time.Second)
		err := runCommand(checkoutContext, command)
		cancel()
		if err != nil {
			return errors.New("Git checkout failed; check repository credentials, pinned commit and host trust")
		}
	}
	return nil
}

func quote(value string) string { return "'" + strings.ReplaceAll(value, "'", "'\\''") + "'" }

func upload(ctx context.Context, input Input, control pb.ControlClient, root, path string) error {
	file, err := executor.OpenArtifact(root, path)
	if err != nil {
		return err
	}
	defer file.Close()
	digest := sha256.New()
	size, err := io.Copy(digest, io.LimitReader(file, 20*1024*1024*1024+1))
	if err != nil {
		return err
	}
	if size == 0 || size > 20*1024*1024*1024 {
		return errors.New("artifact must contain 1 byte..20 GiB")
	}
	if _, err = file.Seek(0, io.SeekStart); err != nil {
		return err
	}
	ticket, err := control.PrepareArtifact(ctx, &pb.ArtifactRequest{AttemptId: input.AttemptID, LeaseToken: input.LeaseToken, Path: path, Size: uint64(size), Sha256: hex.EncodeToString(digest.Sum(nil))})
	if err != nil {
		return err
	}
	if ticket.Url != "" {
		endpoint, err := url.Parse(ticket.Url)
		if err != nil || (endpoint.Scheme != "https" && !(input.Config.AllowHTTPUploads && endpoint.Scheme == "http")) {
			return errors.New("refusing insecure artifact upload URL")
		}
		request, err := http.NewRequestWithContext(ctx, http.MethodPut, ticket.Url, file)
		if err != nil {
			return err
		}
		request.ContentLength = size
		httpClient := http.Client{CheckRedirect: func(_ *http.Request, _ []*http.Request) error { return http.ErrUseLastResponse }}
		response, err := httpClient.Do(request)
		if err != nil {
			return errors.New("artifact upload failed")
		}
		io.Copy(io.Discard, io.LimitReader(response.Body, 65536))
		response.Body.Close()
		if response.StatusCode < 200 || response.StatusCode >= 300 {
			return fmt.Errorf("artifact upload rejected: %d", response.StatusCode)
		}
	}
	_, err = control.CompleteArtifact(ctx, &pb.CompleteArtifactRequest{AttemptId: input.AttemptID, LeaseToken: input.LeaseToken, UploadId: ticket.UploadId})
	return err
}

func Askpass(arguments []string) {
	query := strings.Join(arguments, " ")
	if strings.Contains(strings.ToLower(query), "username") {
		fmt.Println(os.Getenv("ARTIFACTORY_GIT_USERNAME"))
	} else {
		fmt.Println(os.Getenv("ARTIFACTORY_GIT_PASSWORD"))
	}
}
