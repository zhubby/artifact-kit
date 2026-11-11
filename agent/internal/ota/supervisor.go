package ota

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
	"runtime"
	"strings"
	"time"

	"github.com/zhubby/artifact-kit/agent/internal/executor"
)

// Initialize is an administrator bootstrap operation. The service account must
// own directory; future releases never replace the fixed service executable.
func Initialize(configPath, directory, binary, version string, keys []string) error {
	if !filepath.IsAbs(directory) || !safeVersion.MatchString(version) || len(keys) == 0 {
		return errors.New("invalid bootstrap options")
	}
	if _, err := os.Stat(filepath.Join(filepath.Dir(configPath), "ota.json")); !os.IsNotExist(err) {
		return errors.New("OTA is already configured or configuration is inaccessible")
	}
	if err := os.MkdirAll(directory, 0700); err != nil {
		return err
	}
	body, err := os.ReadFile(binary)
	if err != nil {
		return err
	}
	sum := sha256.Sum256(body)
	directoryPath := filepath.Join(directory, "versions", hex.EncodeToString(sum[:]))
	if err = os.MkdirAll(directoryPath, 0700); err != nil {
		return err
	}
	name := "artifactory-agent"
	if runtime.GOOS == "windows" {
		name += ".exe"
	}
	image := filepath.Join(directoryPath, name)
	if err = os.WriteFile(image, body, 0700); err != nil {
		return err
	}
	if err = Atomic(filepath.Join(directory, "state.json"), State{Current: Image{Version: version, Path: image}, Phase: "idle", LauncherVersion: version}); err != nil {
		return err
	}
	return Atomic(filepath.Join(filepath.Dir(configPath), "ota.json"), Config{Directory: directory, PublicKeys: keys})
}

type downloadResult struct {
	id    string
	image Image
	err   error
}
type child struct {
	command *exec.Cmd
	tree    *executor.ProcessTree
	control io.WriteCloser
	done    chan error
}

func startChild(configPath, directory, path, launcher string) (*child, error) {
	command := exec.Command(path, "run", "--config", configPath)
	command.Env = append(os.Environ(), "ARTIFACTORY_OTA_DIRECTORY="+directory, "ARTIFACTORY_OTA_LAUNCHER_VERSION="+launcher)
	command.Stdout = os.Stdout
	command.Stderr = os.Stderr
	control, err := command.StdinPipe()
	if err != nil {
		return nil, err
	}
	tree, err := executor.StartProcess(command)
	if err != nil {
		_ = control.Close()
		return nil, err
	}
	c := &child{command: command, tree: tree, control: control, done: make(chan error, 1)}
	go func() { c.done <- command.Wait() }()
	return c, nil
}
func (c *child) stop() {
	if c == nil {
		return
	}
	// EOF is a private cross-platform shutdown signal. Let the runtime cancel
	// and reap its Workers so their task groups and workspaces are cleaned up
	// before closing the outer Windows Job Object or Unix process group.
	_ = c.control.Close()
	select {
	case <-c.done:
	case <-time.After(8 * time.Second):
		c.tree.Kill()
		<-c.done
	}
	c.tree.Close()
}

func download(ctx context.Context, cfg Config, request Request) (Image, error) {
	var image Image
	asset, err := Verify([]byte(request.Manifest), request.Signature, cfg.PublicKeys, runtime.GOOS, runtime.GOARCH)
	if err != nil {
		return image, err
	}
	var manifest Manifest
	_ = json.Unmarshal([]byte(request.Manifest), &manifest)
	u, err := url.Parse(request.URL)
	if err != nil || u.Host == "" || u.User != nil || (u.Scheme != "https" && !(cfg.AllowHTTP && u.Scheme == "http")) {
		return image, errors.New("HTTPS release download required")
	}
	client := &http.Client{Timeout: 5 * time.Minute, CheckRedirect: func(req *http.Request, via []*http.Request) error {
		return errors.New("release redirects are not allowed")
	}}
	req, err := http.NewRequestWithContext(ctx, "GET", request.URL, nil)
	if err != nil {
		return image, err
	}
	response, err := client.Do(req)
	if err != nil {
		return image, errors.New("release download interrupted")
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusOK {
		return image, fmt.Errorf("release download returned HTTP %d", response.StatusCode)
	}
	directory := filepath.Join(cfg.Directory, "versions", asset.SHA256)
	if err = os.MkdirAll(directory, 0700); err != nil {
		return image, err
	}
	f, err := os.CreateTemp(directory, ".download-*")
	if err != nil {
		return image, err
	}
	defer os.Remove(f.Name())
	hash := sha256.New()
	size, err := io.Copy(io.MultiWriter(f, hash), io.LimitReader(response.Body, asset.Size+1))
	if err == nil && (size != asset.Size || hex.EncodeToString(hash.Sum(nil)) != asset.SHA256) {
		err = errors.New("binary size or SHA-256 mismatch")
	}
	if err == nil {
		err = f.Chmod(0700)
	}
	if err == nil {
		err = f.Sync()
	}
	_ = f.Close()
	if err != nil {
		return image, err
	}
	name := "artifactory-agent"
	if runtime.GOOS == "windows" {
		name += ".exe"
	}
	destination := filepath.Join(directory, name)
	// A historical executable may still be locked on Windows. Reuse it only
	// after checking its full content, never overwrite a running executable.
	if existing, readErr := os.ReadFile(destination); readErr == nil {
		if err = VerifyBytes(existing, asset); err != nil {
			return image, err
		}
	} else if err = os.Rename(f.Name(), destination); err != nil {
		return image, err
	}
	return Image{Version: manifest.Version, Path: destination}, nil
}

func Supervise(ctx context.Context, configPath, launcherVersion string) error {
	cfg, err := Load(configPath)
	if err != nil {
		return err
	}
	release, err := lock(filepath.Join(cfg.Directory, "launcher.lock"))
	if err != nil {
		return err
	}
	defer release()
	state, err := Snapshot(cfg.Directory)
	if err != nil {
		return err
	}
	state.Recover()
	state.LauncherVersion = launcherVersion
	save := func() error { return Atomic(filepath.Join(cfg.Directory, "state.json"), state) }
	if err = save(); err != nil {
		return err
	}
	current, err := startChild(configPath, cfg.Directory, state.Current.Path, launcherVersion)
	if err != nil {
		return err
	}
	defer func() { current.stop() }()
	results := make(chan downloadResult, 1)
	var cancelDownload context.CancelFunc
	defer func() {
		if cancelDownload != nil {
			cancelDownload()
		}
	}()
	var health HealthWindow
	tick := time.NewTicker(250 * time.Millisecond)
	defer tick.Stop()
	rollback := func(reason string) error {
		current.stop()
		current = nil
		state.Phase = "rolled_back"
		state.Error = reason
		state.Candidate = Image{}
		if err := save(); err != nil {
			return err
		}
		var err error
		current, err = startChild(configPath, cfg.Directory, state.Current.Path, launcherVersion)
		return err
	}
	for {
		select {
		case <-ctx.Done():
			return nil
		case result := <-results:
			if state.UpdateID != result.id || state.Phase != "downloading" {
				continue
			}
			if result.err != nil {
				state.Phase = "failed"
				state.Error = result.err.Error()
			} else {
				state.Candidate = result.image
				state.Phase = "draining"
			}
			if err = save(); err != nil {
				return err
			}
		case <-current.done:
			current.tree.Close()
			current = nil
			if state.Phase == "activating" {
				if err = rollback("candidate exited before health confirmation"); err != nil {
					return err
				}
			} else {
				return errors.New("agent runtime exited; service restart required")
			}
		case <-tick.C:
			var request Request
			path := filepath.Join(cfg.Directory, "request.json")
			if Read(path, &request) == nil {
				_ = os.Remove(path)
				if request.ID != "" && len(request.ID) <= 100 {
					// Cancellation can overtake the first apply command in the
					// private mailbox. Persist its generation so a delayed apply
					// cannot revive an operation the administrator canceled.
					if request.Action == "cancel" && request.Generation > state.Generation && !active(state.Phase) {
						state.Generation = request.Generation
						state.UpdateID = request.ID
						state.Phase = "canceled"
						state.Error = ""
						state.Candidate = Image{}
						if err = save(); err != nil {
							return err
						}
					}
					if request.Action == "cancel" && request.ID == state.UpdateID && (state.Phase == "downloading" || state.Phase == "draining") {
						if cancelDownload != nil {
							cancelDownload()
						}
						state.Phase = "canceled"
						state.Candidate = Image{}
						state.Error = ""
						if err = save(); err != nil {
							return err
						}
					} else if request.Action == "apply" && request.Generation > state.Generation && !active(state.Phase) {
						state.Generation = request.Generation
						state.UpdateID = request.ID
						state.Error = ""
						state.Phase = "downloading"
						state.StartedAt = time.Now()
						state.Candidate = Image{}
						var m Manifest
						_ = json.Unmarshal([]byte(request.Manifest), &m)
						state.Target = m.Version
						if err = save(); err != nil {
							return err
						}
						downloadCtx, cancel := context.WithCancel(ctx)
						cancelDownload = cancel
						go func(r Request) {
							image, err := download(downloadCtx, cfg, r)
							results <- downloadResult{id: r.ID, image: image, err: err}
						}(request)
					}
				}
			}
			if state.Phase == "draining" {
				var ready struct {
					ID string `json:"id"`
				}
				if Read(filepath.Join(cfg.Directory, "drained.json"), &ready) == nil && ready.ID == state.UpdateID {
					// The runtime has acknowledged every terminal event and all workers exited.
					current.stop()
					current = nil
					state.Phase = "activating"
					state.StartedAt = time.Now()
					health = HealthWindow{}
					_ = os.Remove(filepath.Join(cfg.Directory, "health.json"))
					if err = save(); err != nil {
						return err
					}
					current, err = startChild(configPath, cfg.Directory, state.Candidate.Path, launcherVersion)
					if err != nil {
						if err = rollback("candidate failed to start"); err != nil {
							return err
						}
					}
				}
			}
			if state.Phase == "activating" {
				var ack Health
				if Read(filepath.Join(cfg.Directory, "health.json"), &ack) == nil && time.Since(ack.Time) < 25*time.Second && health.Ack(ack.Session, ack.ID) {
					state.Previous = state.Current
					state.Current = state.Candidate
					state.Candidate = Image{}
					state.Phase = "succeeded"
					if err = save(); err != nil {
						return err
					}
				} else if time.Since(state.StartedAt) >= 120*time.Second {
					if err = rollback("candidate health confirmation timed out after 120 seconds"); err != nil {
						return err
					}
				}
			}
		}
	}
}
func active(phase string) bool {
	return strings.Contains("|downloading|draining|activating|", "|"+phase+"|")
}
