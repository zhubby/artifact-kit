package main

import (
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/json"
	"encoding/pem"
	"errors"
	"fmt"
	"math/big"
	"net"
	"os"
	"os/exec"
	"os/user"
	"path/filepath"
	"regexp"
	"runtime"
	"strconv"
	"strings"
	"testing"
	"time"

	pb "github.com/artifact-kit/agent/gen/agentv1"
	"github.com/artifact-kit/agent/internal/config"
	"github.com/kardianos/service"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials"
	"google.golang.org/grpc/status"
)

const serviceTestUser = "ak-service-ci"

type serviceConnection struct {
	hello  *pb.AgentEvent
	closed chan struct{}
}

// This loopback peer exercises the installed Agent and real mTLS transport.
// It is not the Rust control plane or an application-build acceptance test.
type servicePeer struct {
	pb.UnimplementedControlServer
	connected chan serviceConnection
}

func (peer *servicePeer) Connect(stream pb.Control_ConnectServer) error {
	event, err := stream.Recv()
	if err != nil {
		return err
	}
	if event.Kind != "hello" || event.AgentId != "native-service-test" || event.ProtocolMajor != 1 {
		return status.Error(codes.PermissionDenied, "unexpected test Agent identity")
	}
	connection := serviceConnection{hello: event, closed: make(chan struct{})}
	defer close(connection.closed)
	select {
	case peer.connected <- connection:
	case <-stream.Context().Done():
		return stream.Context().Err()
	}
	for {
		if _, err := stream.Recv(); err != nil {
			return err
		}
	}
}

type nativeServiceState struct {
	Installed bool   `json:"installed"`
	Enabled   bool   `json:"enabled"`
	PID       int    `json:"pid"`
	User      string `json:"user"`
}

func TestServiceStopsAuthenticatedAgent(test *testing.T) {
	private := test.TempDir()
	endpoint, peer := startServicePeer(test, private)
	path := filepath.Join(private, "agent.json")
	if err := config.Save(path, config.Config{
		Endpoint: endpoint, CAFile: filepath.Join(private, "ca.pem"),
		CertificateFile: filepath.Join(private, "client.pem"), KeyFile: filepath.Join(private, "client-key.pem"),
		AgentID: "native-service-test", Root: filepath.Join(private, "managed"),
	}); err != nil {
		test.Fatal(err)
	}
	program := &agentService{configPath: path}
	instance, err := service.New(program, &service.Config{Name: "artifact-kit-agent"})
	if err != nil {
		test.Fatal(err)
	}
	if err := program.Start(instance); err != nil {
		test.Fatal(err)
	}
	test.Cleanup(func() {
		if err := program.Stop(instance); err != nil {
			test.Error(err)
		}
	})
	connection := awaitServiceConnection(test, peer)
	if err := program.Stop(instance); err != nil {
		test.Fatal(err)
	}
	awaitServiceDisconnect(test, connection)
}

// This test changes system services and creates a local account. It is opt-in
// and restricted to disposable GitHub-hosted VMs, never the developer's host.
func TestNativeServiceLifecycle(test *testing.T) {
	if os.Getenv("AK_NATIVE_SERVICE_TESTS") != "1" {
		test.Skip("requires an explicitly enabled disposable native CI VM")
	}
	if os.Getenv("GITHUB_ACTIONS") != "true" || os.Getenv("RUNNER_ENVIRONMENT") != "github-hosted" {
		test.Fatal("refusing service/account changes outside a GitHub-hosted ephemeral runner")
	}
	if state := inspectNativeService(test); state.Installed {
		test.Fatal("refusing to replace an existing artifact-kit-agent service")
	}
	// Windows reports an absent local account as ERROR_NONE_MAPPED, not
	// user.UnknownUserError. Its account script checks Get-LocalUser directly
	// before creation, with SID-bound cleanup if creation only partly succeeds.
	if runtime.GOOS != "windows" {
		if _, err := user.Lookup(serviceTestUser); err == nil {
			test.Fatal("refusing to reuse an existing service test account")
		} else {
			var unknown user.UnknownUserError
			if !errors.As(err, &unknown) {
				test.Fatalf("cannot verify test account absence: %v", err)
			}
		}
	}
	base := "/tmp"
	if runtime.GOOS == "windows" {
		base = os.Getenv("ProgramData")
		if base == "" {
			test.Fatal("ProgramData is required")
		}
	}
	directory, err := os.MkdirTemp(base, "artifact-kit-service-")
	if err != nil {
		test.Fatal(err)
	}
	// The executable path and private configuration both contain spaces.
	binaryDirectory := filepath.Join(directory, "agent bin %i $HOME {{literal}}")
	private := filepath.Join(directory, "private data %n $HOME {{literal}}")
	for _, path := range []string{directory, binaryDirectory} {
		if err := os.MkdirAll(path, 0755); err != nil {
			test.Fatal(err)
		}
		if err := os.Chmod(path, 0755); err != nil {
			test.Fatal(err)
		}
	}
	if err := os.Mkdir(private, 0700); err != nil {
		test.Fatal(err)
	}
	test.Cleanup(func() {
		if runtime.GOOS == "windows" {
			if err := os.RemoveAll(directory); err != nil {
				test.Errorf("remove owned test directory: %v", err)
			}
		} else if _, err := nativeCommand(nil, true, "rm", "-rf", directory); err != nil {
			test.Errorf("remove owned test directory: %v", err)
		}
	})
	binary := filepath.Join(binaryDirectory, "artifact-agent")
	if runtime.GOOS == "windows" {
		binary += ".exe"
	}
	requireNativeCommand(test, nil, false, "go", "build", "-o", binary, ".")
	endpoint, peer := startServicePeer(test, private)
	configuration := config.Config{
		Endpoint: endpoint, CAFile: filepath.Join(private, "ca.pem"),
		CertificateFile: filepath.Join(private, "client.pem"), KeyFile: filepath.Join(private, "client-key.pem"),
		AgentID: "native-service-test", Name: "native-service-test", Root: filepath.Join(private, "managed root"),
	}
	configPath := filepath.Join(private, "agent config.json")
	if err := config.Save(configPath, configuration); err != nil {
		test.Fatal(err)
	}
	password := fmt.Sprintf("Ak-CI-%x!", makeServiceRandom(test, 20))
	accountEnvironment := append(os.Environ(), "AK_TEST_SERVICE_PASSWORD="+password)
	createServiceTestUser(test, accountEnvironment, directory, private)
	if runtime.GOOS != "windows" {
		for path, mode := range map[string]string{private: "700", configPath: "600", configuration.KeyFile: "600"} {
			arguments := []string{"stat", "-c", "%U %a", path}
			if runtime.GOOS == "darwin" {
				arguments = []string{"stat", "-f", "%Su %Lp", path}
			}
			if actual := strings.TrimSpace(requireNativeCommand(test, nil, true, arguments...)); actual != serviceTestUser+" "+mode {
				test.Fatalf("private configuration ownership/mode mismatch: %s: %s", path, actual)
			}
		}
	}
	serviceUser := serviceTestUser
	if runtime.GOOS == "windows" {
		serviceUser = `.\` + serviceUser
	}
	action := func(name string) {
		environment := os.Environ()
		arguments := []string{binary, "service", "--action", name, "--config", configPath}
		if name == "install" {
			arguments = append(arguments, "--user", serviceUser)
			environment = append(environment, "AK_SERVICE_PASSWORD="+password)
		}
		requireNativeCommand(test, environment, true, arguments...)
	}
	// Preflight proved the name absent; clean up a partially successful install
	// as well, while preserving test failures and cleanup diagnostics.
	test.Cleanup(func() {
		if test.Failed() {
			logNativeServiceFailure(test, private)
		}
		if state := inspectNativeService(test); state.Installed {
			_, _ = nativeCommand(nil, true, binary, "service", "--action", "stop", "--config", configPath)
			if output, err := nativeCommand(nil, true, binary, "service", "--action", "uninstall", "--config", configPath); err != nil {
				test.Errorf("uninstall owned test service: %v: %s", err, output)
			}
		}
	})
	action("install")
	state := inspectNativeService(test)
	if !state.Installed || !state.Enabled {
		test.Fatalf("service was not installed for automatic startup: %+v", state)
	}
	action("start")
	first := awaitServiceConnection(test, peer)
	state = awaitNativeService(test, true)
	firstPID := state.PID
	if state.User != serviceTestUser {
		test.Fatalf("service process did not use its dedicated unprivileged account: %+v", state)
	}
	test.Logf("installed service connected over mTLS as %s (pid=%d)", state.User, firstPID)
	if runtime.GOOS == "darwin" {
		for _, name := range []string{"artifact-kit-agent.out.log", "artifact-kit-agent.err.log"} {
			requireNativeCommand(test, nil, true, "test", "-f", filepath.Join(private, name))
		}
	}
	action("restart")
	awaitServiceDisconnect(test, first)
	second := awaitServiceConnection(test, peer)
	state = awaitNativeService(test, true)
	if state.PID == firstPID || state.User != serviceTestUser {
		test.Fatalf("restart failed to replace the service process under its dedicated account: %+v", state)
	}
	test.Logf("restart created a fresh authenticated Agent process (pid=%d)", state.PID)
	action("stop")
	awaitServiceDisconnect(test, second)
	awaitNativeService(test, false)
	action("uninstall")
	if state := inspectNativeService(test); state.Installed {
		test.Fatalf("service still installed after uninstall: %+v", state)
	}
	test.Log("native install, automatic-start configuration, account identity, mTLS, restart, stop and uninstall passed")
}

func makeServiceRandom(test *testing.T, size int) []byte {
	test.Helper()
	value := make([]byte, size)
	if _, err := rand.Read(value); err != nil {
		test.Fatal(err)
	}
	return value
}

func startServicePeer(test *testing.T, private string) (string, *servicePeer) {
	test.Helper()
	public, key, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		test.Fatal(err)
	}
	ca := &x509.Certificate{SerialNumber: big.NewInt(1), Subject: pkix.Name{CommonName: "Artifact Kit service test CA"}, NotBefore: time.Now().Add(-time.Minute), NotAfter: time.Now().Add(time.Hour), IsCA: true, BasicConstraintsValid: true, KeyUsage: x509.KeyUsageCertSign}
	caDER, err := x509.CreateCertificate(rand.Reader, ca, ca, public, key)
	if err != nil {
		test.Fatal(err)
	}
	caPEM := pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: caDER})
	if err := os.WriteFile(filepath.Join(private, "ca.pem"), caPEM, 0600); err != nil {
		test.Fatal(err)
	}
	certificates := make([]tls.Certificate, 0, 2)
	for index, name := range []string{"server", "client"} {
		public, privateKey, err := ed25519.GenerateKey(rand.Reader)
		if err != nil {
			test.Fatal(err)
		}
		template := &x509.Certificate{SerialNumber: big.NewInt(int64(index + 2)), Subject: pkix.Name{CommonName: "native-service-test"}, NotBefore: ca.NotBefore, NotAfter: ca.NotAfter, KeyUsage: x509.KeyUsageDigitalSignature}
		if name == "server" {
			template.IPAddresses = []net.IP{net.ParseIP("127.0.0.1")}
			template.ExtKeyUsage = []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth}
		} else {
			template.ExtKeyUsage = []x509.ExtKeyUsage{x509.ExtKeyUsageClientAuth}
		}
		der, err := x509.CreateCertificate(rand.Reader, template, ca, public, key)
		if err != nil {
			test.Fatal(err)
		}
		keyDER, err := x509.MarshalPKCS8PrivateKey(privateKey)
		if err != nil {
			test.Fatal(err)
		}
		certificatePEM := pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der})
		keyPEM := pem.EncodeToMemory(&pem.Block{Type: "PRIVATE KEY", Bytes: keyDER})
		certificate, err := tls.X509KeyPair(certificatePEM, keyPEM)
		if err != nil {
			test.Fatal(err)
		}
		certificates = append(certificates, certificate)
		if name == "client" {
			for path, data := range map[string][]byte{"client.pem": certificatePEM, "client-key.pem": keyPEM} {
				if err := os.WriteFile(filepath.Join(private, path), data, 0600); err != nil {
					test.Fatal(err)
				}
			}
		}
	}
	roots := x509.NewCertPool()
	roots.AppendCertsFromPEM(caPEM)
	server := grpc.NewServer(grpc.Creds(credentials.NewTLS(&tls.Config{MinVersion: tls.VersionTLS13, Certificates: certificates[:1], ClientCAs: roots, ClientAuth: tls.RequireAndVerifyClientCert})))
	peer := &servicePeer{connected: make(chan serviceConnection, 8)}
	pb.RegisterControlServer(server, peer)
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		test.Fatal(err)
	}
	test.Cleanup(server.Stop)
	go func() { _ = server.Serve(listener) }()
	return listener.Addr().String(), peer
}

func awaitServiceConnection(test *testing.T, peer *servicePeer) serviceConnection {
	test.Helper()
	select {
	case connection := <-peer.connected:
		var payload struct {
			Diagnostics struct {
				OS   string `json:"os"`
				Arch string `json:"arch"`
			} `json:"diagnostics"`
		}
		if err := json.Unmarshal([]byte(connection.hello.PayloadJson), &payload); err != nil || payload.Diagnostics.OS != runtime.GOOS || payload.Diagnostics.Arch != runtime.GOARCH {
			test.Fatalf("service did not report native capabilities: %v, %+v", err, payload)
		}
		return connection
	case <-time.After(90 * time.Second):
		test.Fatalf("native service never connected; manager state: %+v", inspectNativeService(test))
		return serviceConnection{}
	}
}

func awaitServiceDisconnect(test *testing.T, connection serviceConnection) {
	test.Helper()
	select {
	case <-connection.closed:
	case <-time.After(30 * time.Second):
		test.Fatal("old service process retained its control stream")
	}
}

func awaitNativeService(test *testing.T, running bool) nativeServiceState {
	test.Helper()
	var state nativeServiceState
	deadline := time.Now().Add(30 * time.Second)
	for time.Now().Before(deadline) {
		state = inspectNativeService(test)
		if (state.PID > 0) == running {
			return state
		}
		time.Sleep(100 * time.Millisecond)
	}
	test.Fatalf("service did not reach running=%t: %+v", running, state)
	return state
}

func nativeCommand(environment []string, administrative bool, arguments ...string) (string, error) {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	if administrative && runtime.GOOS != "windows" {
		arguments = append([]string{"sudo", "-n"}, arguments...)
	}
	command := exec.CommandContext(ctx, arguments[0], arguments[1:]...)
	command.Env = environment
	output, err := command.CombinedOutput()
	return string(output), err
}

func requireNativeCommand(test *testing.T, environment []string, administrative bool, arguments ...string) string {
	test.Helper()
	output, err := nativeCommand(environment, administrative, arguments...)
	if err != nil {
		test.Fatalf("%s failed: %v: %s", arguments[0], err, output)
	}
	return output
}

func createServiceTestUser(test *testing.T, environment []string, directory, private string) {
	test.Helper()
	switch runtime.GOOS {
	case "linux":
		requireNativeCommand(test, nil, true, "useradd", "--system", "--home-dir", private, "--shell", "/usr/sbin/nologin", serviceTestUser)
		test.Cleanup(func() { requireNativeCommand(test, nil, true, "userdel", serviceTestUser) })
		requireNativeCommand(test, nil, true, "chown", "-R", serviceTestUser, private)
	case "darwin":
		// Find an unused UID instead of colliding with a runner account.
		entries := requireNativeCommand(test, nil, true, "dscl", ".", "-list", "/Users", "UniqueID")
		uid := 6001
		for strings.Contains(entries, strconv.Itoa(uid)) {
			uid++
		}
		record := "/Users/" + serviceTestUser
		requireNativeCommand(test, nil, true, "dscl", ".", "-create", record)
		test.Cleanup(func() { requireNativeCommand(test, nil, true, "dscl", ".", "-delete", record) })
		for _, entry := range [][2]string{{"UniqueID", strconv.Itoa(uid)}, {"PrimaryGroupID", "20"}, {"NFSHomeDirectory", private}, {"UserShell", "/usr/bin/false"}, {"IsHidden", "1"}} {
			requireNativeCommand(test, nil, true, "dscl", ".", "-create", record, entry[0], entry[1])
		}
		requireNativeCommand(test, nil, true, "chown", "-R", serviceTestUser, private)
	case "windows":
		script, err := filepath.Abs("../../../scripts/native-service-account.ps1")
		if err != nil {
			test.Fatal(err)
		}
		test.Cleanup(func() {
			requireNativeCommand(test, nil, true, "pwsh.exe", "-NoProfile", "-NonInteractive", "-File", script, "-Action", "remove", "-Directory", directory)
		})
		requireNativeCommand(test, environment, true, "pwsh.exe", "-NoProfile", "-NonInteractive", "-File", script, "-Action", "create", "-Directory", directory)
	default:
		test.Fatal("unsupported service test OS")
	}
}

func inspectNativeService(test *testing.T) nativeServiceState {
	test.Helper()
	var state nativeServiceState
	switch runtime.GOOS {
	case "linux":
		output, err := nativeCommand(nil, false, "systemctl", "show", "artifact-kit-agent.service", "--property=LoadState,UnitFileState,MainPID")
		if err != nil && !strings.Contains(output, "LoadState=not-found") {
			test.Fatalf("inspect systemd: %v: %s", err, output)
		}
		values := map[string]string{}
		for _, line := range strings.Split(output, "\n") {
			if key, value, ok := strings.Cut(line, "="); ok {
				values[key] = value
			}
		}
		state.Installed = values["LoadState"] != "not-found"
		state.Enabled = values["UnitFileState"] == "enabled"
		state.PID, _ = strconv.Atoi(values["MainPID"])
	case "darwin":
		path := "/Library/LaunchDaemons/artifact-kit-agent.plist"
		if _, err := os.Lstat(path); err == nil {
			state.Installed = true
			// An unconditional KeepAlive also starts the job when launchd
			// loads it, even with RunAtLoad=false.
			for _, key := range []string{"RunAtLoad", "KeepAlive"} {
				if strings.TrimSpace(requireNativeCommand(test, nil, false, "plutil", "-extract", key, "raw", "-o", "-", path)) == "true" {
					state.Enabled = true
				}
			}
		} else if !errors.Is(err, os.ErrNotExist) {
			test.Fatal(err)
		}
		output, err := nativeCommand(nil, true, "launchctl", "print", "system/artifact-kit-agent")
		if err == nil {
			state.Installed = true
			if match := regexp.MustCompile(`(?m)^\s*pid = (\d+)`).FindStringSubmatch(output); len(match) == 2 {
				state.PID, _ = strconv.Atoi(match[1])
			}
		} else if !strings.Contains(output, "Could not find service") {
			test.Fatalf("inspect launchd: %v: %s", err, output)
		}
	case "windows":
		output := requireNativeCommand(test, nil, false, "pwsh.exe", "-NoProfile", "-NonInteractive", "-Command", `$ErrorActionPreference='Stop'; $s=Get-CimInstance Win32_Service -Filter "Name='artifact-kit-agent'"; if (!$s) { '{"installed":false}' } else { $owner=''; if ($s.ProcessId) { $p=Get-CimInstance Win32_Process -Filter "ProcessId=$($s.ProcessId)"; if ($p) { $owner=(Invoke-CimMethod -InputObject $p -MethodName GetOwner).User } }; @{installed=$true; enabled=($s.StartMode -eq 'Auto'); pid=[int]$s.ProcessId; user=$owner} | ConvertTo-Json -Compress }`)
		if err := json.Unmarshal([]byte(strings.TrimPrefix(output, "\ufeff")), &state); err != nil {
			test.Fatalf("inspect SCM: %v: %s", err, output)
		}
	default:
		test.Fatal("unsupported service test OS")
	}
	if state.PID > 0 && runtime.GOOS != "windows" {
		output, err := nativeCommand(nil, false, "ps", "-o", "uid=", "-p", strconv.Itoa(state.PID))
		if err == nil {
			owner, err := user.LookupId(strings.TrimSpace(output))
			if err == nil {
				state.User = owner.Username
			}
		}
	}
	return state
}

func logNativeServiceFailure(test *testing.T, private string) {
	test.Helper()
	var commands [][]string
	switch runtime.GOOS {
	case "linux":
		commands = [][]string{{"systemctl", "status", "artifact-kit-agent.service", "--no-pager"}, {"journalctl", "-u", "artifact-kit-agent.service", "-n", "80", "--no-pager"}}
	case "darwin":
		commands = [][]string{{"launchctl", "print", "system/artifact-kit-agent"}, {"tail", "-n", "80", filepath.Join(private, "artifact-kit-agent.err.log")}, {"log", "show", "--last", "3m", "--style", "compact", "--predicate", `(process == "launchd" OR process == "xpcproxy") AND eventMessage CONTAINS "artifact-kit-agent"`}}
	case "windows":
		commands = [][]string{{"pwsh.exe", "-NoProfile", "-NonInteractive", "-Command", `Get-WinEvent -FilterHashtable @{LogName='Application'; ProviderName='artifact-kit-agent'} -MaxEvents 15 -ErrorAction SilentlyContinue | Select-Object TimeCreated,Id,Message | Format-List`}}
	}
	for _, arguments := range commands {
		output, err := nativeCommand(nil, true, arguments...)
		test.Logf("native service failure diagnostics (%s, %v):\n%s", arguments[0], err, output)
	}
}
