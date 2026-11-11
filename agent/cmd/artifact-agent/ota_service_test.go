package main

import (
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"os/user"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"

	pb "github.com/artifact-kit/agent/gen/agentv1"
	"github.com/artifact-kit/agent/internal/config"
	"github.com/artifact-kit/agent/internal/ota"
)

func bootstrapTestOTA(t *testing.T, binary, path, root string) ed25519.PrivateKey {
	t.Helper()
	pub, key, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	if err = ota.Initialize(path, filepath.Join(root, "ota"), binary, "0.1.0", []string{base64.StdEncoding.EncodeToString(pub)}); err != nil {
		t.Fatal(err)
	}
	cfg, err := ota.Load(path)
	if err != nil {
		t.Fatal(err)
	}
	cfg.AllowHTTP = true
	if err = ota.Atomic(filepath.Join(filepath.Dir(path), "ota.json"), cfg); err != nil {
		t.Fatal(err)
	}
	return key
}
func waitOTA(t *testing.T, directory, phase string) ota.State {
	t.Helper()
	deadline := time.Now().Add(100 * time.Second)
	for time.Now().Before(deadline) {
		state, err := ota.Snapshot(directory)
		if err == nil && state.Phase == phase {
			return state
		}
		time.Sleep(100 * time.Millisecond)
	}
	state, err := ota.Snapshot(directory)
	t.Fatalf("OTA did not reach %s: %+v (%v)", phase, state, err)
	return state
}

func exerciseServiceOTA(t *testing.T, key ed25519.PrivateKey, configPath string, peer *servicePeer, connection serviceConnection) {
	t.Helper()
	cfg, err := ota.Load(configPath)
	if err != nil {
		t.Fatal(err)
	}
	binary := filepath.Join(t.TempDir(), "candidate")
	if runtime.GOOS == "windows" {
		binary += ".exe"
	}
	requireNativeCommand(t, nil, false, "go", "build", "-ldflags", "-X github.com/artifact-kit/agent/internal/agent.Version=v-ota-native-b", "-o", binary, ".")
	good, err := os.ReadFile(binary)
	if err != nil {
		t.Fatal(err)
	}
	bad := []byte("invalid native executable\n")
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/native-bad" {
			_, _ = w.Write(bad)
		} else {
			_, _ = w.Write(good)
		}
	}))
	defer server.Close()
	send := func(c serviceConnection, id, version string, generation int64, data []byte) {
		sum := sha256.Sum256(data)
		name := "artifact-agent-" + runtime.GOOS + "-" + runtime.GOARCH
		if runtime.GOOS == "windows" {
			name += ".exe"
		}
		m := ota.Manifest{Version: version, Commit: strings.Repeat("a", 40), ProtocolMin: 1, ProtocolMax: 1, Assets: []ota.Asset{{OS: runtime.GOOS, Arch: runtime.GOARCH, Name: name, Size: int64(len(data)), SHA256: hex.EncodeToString(sum[:])}}}
		raw, _ := json.Marshal(m)
		request, _ := json.Marshal(ota.Request{ID: id, Generation: generation, Action: "apply", Manifest: string(raw), Signature: base64.StdEncoding.EncodeToString(ed25519.Sign(key, raw)), URL: server.URL + "/" + id})
		select {
		case c.commands <- &pb.Command{Kind: "agent_update", PayloadJson: string(request)}:
		case <-time.After(10 * time.Second):
			t.Fatal("OTA command delivery timed out")
		}
	}
	send(connection, "native-good", "v-ota-native-b", 1, good)
	state := waitOTA(t, cfg.Directory, "succeeded")
	if state.Current.Version != "v-ota-native-b" || state.Previous.Version != "0.1.0" {
		t.Fatalf("wrong committed versions: %+v", state)
	}
	awaitServiceDisconnect(t, connection)
	candidate := awaitServiceConnection(t, peer)
	send(candidate, "native-bad", "v-ota-native-bad", 2, bad)
	state = waitOTA(t, cfg.Directory, "rolled_back")
	if state.Current.Version != "v-ota-native-b" || state.Error == "" {
		t.Fatalf("failed to restore last good binary: %+v", state)
	}
	restored := awaitServiceConnection(t, peer)
	// A delayed replay of an older command cannot reapply an old release.
	send(restored, "native-good", "v-ota-native-b", 1, good)
	time.Sleep(time.Second)
	state, err = ota.Snapshot(cfg.Directory)
	if err != nil || state.UpdateID != "native-bad" || state.Phase != "rolled_back" {
		t.Fatalf("stale command restarted an update: %+v (%v)", state, err)
	}
	t.Log("native child executable A -> B, three mTLS confirmations, invalid executable rollback and command replay fencing passed")
}

func TestOTASupervisorMTLSUpgradeAndRollback(t *testing.T) {
	private := t.TempDir()
	endpoint, peer := startServicePeer(t, private)
	path := filepath.Join(private, "agent.json")
	root := filepath.Join(private, "managed")
	if err := config.Save(path, config.Config{Endpoint: endpoint, CAFile: filepath.Join(private, "ca.pem"), CertificateFile: filepath.Join(private, "client.pem"), KeyFile: filepath.Join(private, "client-key.pem"), AgentID: "native-service-test", Root: root}); err != nil {
		t.Fatal(err)
	}
	binary := filepath.Join(private, "launcher")
	if runtime.GOOS == "windows" {
		binary += ".exe"
	}
	requireNativeCommand(t, nil, false, "go", "build", "-o", binary, ".")
	key := bootstrapTestOTA(t, binary, path, root)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	done := make(chan error, 1)
	go func() { done <- ota.Supervise(ctx, path, "0.1.0") }()
	t.Cleanup(func() {
		cancel()
		select {
		case err := <-done:
			if err != nil {
				t.Error(err)
			}
		case <-time.After(10 * time.Second):
			t.Error("supervisor did not stop")
		}
	})
	first := awaitServiceConnection(t, peer)
	exerciseServiceOTA(t, key, path, peer, first)
}

func TestOTASupervisorStopsActiveWorker(t *testing.T) {
	private, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	owner, err := user.Current()
	if err != nil {
		t.Fatal(err)
	}
	endpoint, peer := startServicePeer(t, private)
	workload := newServiceWorkload(t, private)
	t.Cleanup(workload.stop)
	path, root := filepath.Join(private, "agent.json"), filepath.Join(private, "managed")
	if err := config.Save(path, config.Config{Endpoint: endpoint, CAFile: filepath.Join(private, "ca.pem"), CertificateFile: filepath.Join(private, "client.pem"), KeyFile: filepath.Join(private, "client-key.pem"), AgentID: "native-service-test", Root: root}); err != nil {
		t.Fatal(err)
	}
	binary := filepath.Join(private, "launcher")
	if runtime.GOOS == "windows" {
		binary += ".exe"
	}
	requireNativeCommand(t, nil, false, "go", "build", "-o", binary, ".")
	bootstrapTestOTA(t, binary, path, root)
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan error, 1)
	go func() { done <- ota.Supervise(ctx, path, "0.1.0") }()
	waited := false
	t.Cleanup(func() {
		cancel()
		if !waited {
			select {
			case <-done:
			case <-time.After(15 * time.Second):
				t.Error("supervisor did not stop")
			}
		}
	})
	connection := awaitServiceConnection(t, peer)
	id := workload.assign(t, connection, 1, "parent")
	treeStopped, _ := workload.awaitTree(t, connection, id, os.Getpid(), owner.Username, root)
	cancel()
	select {
	case err := <-done:
		waited = true
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(15 * time.Second):
		t.Fatal("supervisor did not stop")
	}
	awaitServiceDisconnect(t, connection)
	treeStopped()
}
