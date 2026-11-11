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
	"strconv"
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
		state, err := serviceOTASnapshot(directory)
		if err == nil && state.Phase == phase {
			return state
		}
		time.Sleep(100 * time.Millisecond)
	}
	state, err := serviceOTASnapshot(directory)
	t.Fatalf("OTA did not reach %s: %+v (%v)", phase, state, err)
	return state
}

func serviceOTASnapshot(directory string) (ota.State, error) {
	if os.Getenv("AK_NATIVE_SERVICE_TESTS") != "1" || runtime.GOOS == "windows" {
		return ota.Snapshot(directory)
	}
	// The installed service's 0700 directory belongs to its own account.
	// Read only the journal through the VM's test administrator, preserving
	// the production ownership and permissions being exercised.
	data, err := nativeCommand(nil, true, "cat", filepath.Join(directory, "state.json"))
	var state ota.State
	if err == nil {
		err = json.Unmarshal([]byte(data), &state)
	}
	return state, err
}

func exerciseServiceOTA(t *testing.T, key ed25519.PrivateKey, directory string, peer *servicePeer, connection serviceConnection) serviceConnection {
	t.Helper()
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
	state := waitOTA(t, directory, "succeeded")
	if state.Current.Version != "v-ota-native-b" || state.Previous.Version != "0.1.0" {
		t.Fatalf("wrong committed versions: %+v", state)
	}
	awaitServiceDisconnect(t, connection)
	candidate := awaitServiceConnection(t, peer)
	send(candidate, "native-bad", "v-ota-native-bad", 2, bad)
	state = waitOTA(t, directory, "rolled_back")
	if state.Current.Version != "v-ota-native-b" || state.Error == "" {
		t.Fatalf("failed to restore last good binary: %+v", state)
	}
	restored := awaitServiceConnection(t, peer)
	// A delayed replay of an older command cannot reapply an old release.
	send(restored, "native-good", "v-ota-native-b", 1, good)
	time.Sleep(time.Second)
	state, err = serviceOTASnapshot(directory)
	if err != nil || state.UpdateID != "native-bad" || state.Phase != "rolled_back" {
		t.Fatalf("stale command restarted an update: %+v (%v)", state, err)
	}
	t.Log("native child executable A -> B, three mTLS confirmations, invalid executable rollback and command replay fencing passed")
	return restored
}

func exerciseUnresponsiveCandidateStop(t *testing.T, key ed25519.PrivateKey, directory string, peer *servicePeer, connection serviceConnection, action func(string)) {
	t.Helper()
	if runtime.GOOS != "darwin" {
		return
	}
	// This deliberately ignores the launcher's EOF. launchd must allow enough
	// time for the supervisor's forced cleanup before terminating the launcher.
	private := t.TempDir()
	source, binary := filepath.Join(private, "hung.go"), filepath.Join(private, "hung")
	if err := os.WriteFile(source, []byte(`package main
import ("os"; "path/filepath"; "strconv"; "time")
func main() {
 if err := os.WriteFile(filepath.Join(os.Getenv("AK_OTA_DIRECTORY"), "unresponsive.pid"), []byte(strconv.Itoa(os.Getpid())), 0600); err != nil { panic(err) }
 time.Sleep(time.Hour)
}
`), 0600); err != nil {
		t.Fatal(err)
	}
	requireNativeCommand(t, nil, false, "go", "build", "-o", binary, source)
	data, err := os.ReadFile(binary)
	if err != nil {
		t.Fatal(err)
	}
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) { _, _ = w.Write(data) }))
	defer server.Close()
	sum := sha256.Sum256(data)
	manifest, _ := json.Marshal(ota.Manifest{Version: "v-ota-hung", Commit: strings.Repeat("a", 40), ProtocolMin: 1, ProtocolMax: 1,
		Assets: []ota.Asset{{OS: runtime.GOOS, Arch: runtime.GOARCH, Name: "artifact-agent-" + runtime.GOOS + "-" + runtime.GOARCH, Size: int64(len(data)), SHA256: hex.EncodeToString(sum[:])}}})
	request, _ := json.Marshal(ota.Request{ID: "native-hung", Generation: 3, Action: "apply", Manifest: string(manifest),
		Signature: base64.StdEncoding.EncodeToString(ed25519.Sign(key, manifest)), URL: server.URL})
	select {
	case connection.commands <- &pb.Command{Kind: "agent_update", PayloadJson: string(request)}:
	case <-time.After(10 * time.Second):
		t.Fatal("unresponsive candidate command timed out")
	}
	waitOTA(t, directory, "activating")
	pid := 0
	deadline := time.Now().Add(10 * time.Second)
	for time.Now().Before(deadline) {
		value, err := nativeCommand(nil, true, "cat", filepath.Join(directory, "unresponsive.pid"))
		if err == nil {
			pid, _ = strconv.Atoi(strings.TrimSpace(value))
		}
		if pid > 0 {
			break
		}
		time.Sleep(100 * time.Millisecond)
	}
	if pid <= 0 {
		t.Fatal("unresponsive candidate did not start")
	}
	exited := watchWorkloadProcess(t, pid)
	action("stop")
	awaitNativeService(t, false)
	deadline = time.Now().Add(15 * time.Second)
	for !exited() && time.Now().Before(deadline) {
		time.Sleep(100 * time.Millisecond)
	}
	if !exited() {
		t.Fatal("launchd terminated the supervisor before it reaped the nonresponsive candidate")
	}
	action("start")
	awaitServiceConnection(t, peer)
	state := waitOTA(t, directory, "rolled_back")
	if state.Current.Version != "v-ota-native-b" || state.UpdateID != "native-hung" {
		t.Fatalf("interrupted candidate did not restore the previous runtime: %+v", state)
	}
	t.Log("launchd reaped a nonresponsive candidate before restart and recovered the previous runtime")
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
	exerciseServiceOTA(t, key, filepath.Join(root, "ota"), peer, first)
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
	select {
	case event := <-connection.finished:
		if event.AttemptId != id {
			t.Fatalf("shutdown acknowledged a different task: %s", event.AttemptId)
		}
	default:
		t.Fatal("service stopped before its Worker terminal event was acknowledged")
	}
}
