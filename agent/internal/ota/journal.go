package ota

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"time"
)

type Config struct {
	Directory  string   `json:"directory"`
	PublicKeys []string `json:"public_keys"`
	AllowHTTP  bool     `json:"allow_http,omitempty"`
}
type Image struct {
	Version string `json:"version"`
	Path    string `json:"path"`
}
type State struct {
	ActivationPending bool      `json:"activation_pending,omitempty"`
	Generation        int64     `json:"generation"`
	UpdateID          string    `json:"update_id"`
	Target            string    `json:"target"`
	Phase             string    `json:"phase"`
	Error             string    `json:"error,omitempty"`
	Current           Image     `json:"current"`
	Previous          Image     `json:"previous"`
	Candidate         Image     `json:"candidate"`
	LauncherVersion   string    `json:"launcher_version"`
	StartedAt         time.Time `json:"started_at"`
}
type Request struct {
	Generation int64  `json:"generation"`
	ID         string `json:"id"`
	Action     string `json:"action"`
	Manifest   string `json:"manifest"`
	Signature  string `json:"signature"`
	URL        string `json:"url"`
}
type Health struct {
	Session string    `json:"session"`
	ID      string    `json:"id"`
	Time    time.Time `json:"time"`
}
type HealthWindow struct {
	session string
	last    string
	count   int
	at      time.Time
}

func (h *HealthWindow) Ack(session, id string) bool {
	if session != h.session || time.Since(h.at) > 25*time.Second || id == "" {
		*h = HealthWindow{session: session}
	}
	if id != "" && id != h.last {
		h.last = id
		h.count++
		h.at = time.Now()
	}
	return h.count >= 3
}
func (s *State) Recover() bool {
	switch s.Phase {
	case "activating":
		s.Phase = "rolled_back"
		s.Error = "launcher interrupted during activation; restored previous runtime"
	case "downloading", "draining":
		s.Phase = "failed"
		s.Error = "launcher interrupted before activation; current runtime preserved"
	default:
		return false
	}
	s.Candidate = Image{}
	return true
}
func Atomic(path string, value any) error {
	data, err := json.Marshal(value)
	if err != nil {
		return err
	}
	f, err := os.CreateTemp(filepath.Dir(path), ".ota-*")
	if err != nil {
		return err
	}
	defer os.Remove(f.Name())
	if err = f.Chmod(0600); err == nil {
		_, err = f.Write(data)
	}
	if err == nil {
		err = f.Sync()
	}
	closeErr := f.Close()
	if err == nil {
		err = closeErr
	}
	if err != nil {
		return err
	}
	if err = os.Rename(f.Name(), path); err != nil {
		return err
	}
	if dir, err := os.Open(filepath.Dir(path)); err == nil {
		_ = dir.Sync()
		_ = dir.Close()
	}
	return nil
}
func Read(path string, target any) error {
	f, err := os.Open(path)
	if err != nil {
		return err
	}
	defer f.Close()
	info, err := f.Stat()
	if err != nil {
		return err
	}
	if info.Size() > 131072 {
		return errors.New("oversized OTA record")
	}
	return json.NewDecoder(f).Decode(target)
}
func Load(configPath string) (Config, error) {
	var c Config
	err := Read(filepath.Join(filepath.Dir(configPath), "ota.json"), &c)
	if err == nil && (!filepath.IsAbs(c.Directory) || len(c.PublicKeys) == 0) {
		err = errors.New("invalid OTA configuration")
	}
	return c, err
}
func Snapshot(directory string) (State, error) {
	var s State
	err := Read(filepath.Join(directory, "state.json"), &s)
	return s, err
}
func Submit(directory string, request Request) error {
	return Atomic(filepath.Join(directory, "request.json"), request)
}
func Confirm(directory, session, id string) error {
	return Atomic(filepath.Join(directory, "health.json"), Health{Session: session, ID: id, Time: time.Now()})
}
