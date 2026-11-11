package worker

import (
	"bytes"
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"sync/atomic"
	"testing"

	pb "github.com/zhubby/artifact-kit/agent/gen/agentv1"
	"github.com/zhubby/artifact-kit/agent/internal/config"
)

func gitTest(t *testing.T, directory string, args ...string) string {
	t.Helper()
	args = append([]string{"-C", directory, "-c", "user.name=Source test", "-c", "user.email=test@localhost", "-c", "commit.gpgsign=false"}, args...)
	command := exec.Command("git", args...)
	command.Env = append(os.Environ(), "GIT_CONFIG_NOSYSTEM=1", "GIT_CONFIG_GLOBAL="+os.DevNull)
	output, err := command.CombinedOutput()
	if err != nil {
		t.Fatalf("Git %v: %v: %s", args, err, output)
	}
	return strings.TrimSpace(string(output))
}

func TestPlatformCheckoutReusesObjectsAndIsolatesAttempts(t *testing.T) {
	root := t.TempDir()
	upstream := filepath.Join(root, "repo.git")
	if err := os.Mkdir(upstream, 0700); err != nil {
		t.Fatal(err)
	}
	gitTest(t, upstream, "init", "-b", "source")
	if err := os.WriteFile(filepath.Join(upstream, "main.c"), []byte("int main(void) { return 0; }\n"), 0644); err != nil {
		t.Fatal(err)
	}
	gitTest(t, upstream, "add", ".")
	gitTest(t, upstream, "commit", "-m", "immutable fixture")
	commit := gitTest(t, upstream, "rev-parse", "HEAD")
	gitTest(t, upstream, "tag", "-a", "v1", "-m", "frozen tag")
	tag := gitTest(t, upstream, "rev-parse", "refs/tags/v1")
	var requests atomic.Int64
	server := httptest.NewServer(http.HandlerFunc(func(output http.ResponseWriter, request *http.Request) {
		requests.Add(1)
		command := exec.Command("git", "http-backend")
		command.Env = append(os.Environ(), "GIT_PROJECT_ROOT="+root, "GIT_HTTP_EXPORT_ALL=1", "PATH_INFO=/repo.git/"+strings.TrimPrefix(request.URL.Path, "/git/"), "REQUEST_METHOD="+request.Method, "QUERY_STRING="+request.URL.RawQuery, "CONTENT_TYPE="+request.Header.Get("Content-Type"))
		command.Stdin = request.Body
		raw, err := command.Output()
		if err != nil {
			t.Errorf("backend: %v", err)
			output.WriteHeader(500)
			return
		}
		headers, body, ok := bytes.Cut(raw, []byte("\r\n\r\n"))
		if !ok {
			t.Error("invalid CGI output")
			output.WriteHeader(500)
			return
		}
		for _, line := range strings.Split(string(headers), "\r\n") {
			key, value, ok := strings.Cut(line, ":")
			if ok && strings.EqualFold(key, "content-type") {
				output.Header().Set(key, strings.TrimSpace(value))
			}
		}
		output.Write(body)
	}))
	defer server.Close()
	input := Input{Config: config.Config{Root: filepath.Join(root, "managed"), AllowHTTPUploads: true}, Assignment: &pb.Assignment{CommitSha: commit, Source: &pb.SourceDescriptor{
		SnapshotId: "00000000-0000-4000-8000-000000000001", AccessToken: strings.Repeat("a", 64), Repositories: []*pb.SourceRepository{{RepositoryId: "00000000-0000-4000-8000-000000000002", CacheId: "00000000-0000-4000-8000-000000000003", CommitSha: commit, GitUrl: server.URL + "/git", Tags: map[string]string{"v1": tag}}},
	}}}
	emit := &emitter{encoder: json.NewEncoder(io.Discard)}
	first := filepath.Join(root, "first")
	second := filepath.Join(root, "second")
	if err := checkoutPlatform(context.Background(), input, root, first, emit); err != nil {
		t.Fatal(err)
	}
	initial := requests.Load()
	if initial == 0 {
		t.Fatal("source was not downloaded")
	}
	if err := checkoutPlatform(context.Background(), input, root, second, emit); err != nil {
		t.Fatal(err)
	}
	if requests.Load() != initial {
		t.Fatal("cached objects were downloaded again")
	}
	if got := gitTest(t, second, "describe", "--tags"); got != "v1" {
		t.Fatal(got)
	}
	if err := os.WriteFile(filepath.Join(first, "main.c"), []byte("changed\n"), 0644); err != nil {
		t.Fatal(err)
	}
	if err := verifySource(context.Background(), input, first); err == nil || !strings.Contains(err.Error(), "source_modified") {
		t.Fatalf("mutation passed: %v", err)
	}
	if err := verifySource(context.Background(), input, second); err != nil {
		t.Fatal("another attempt changed", err)
	}
	gitTest(t, first, "reset", "--hard", "HEAD")
	if runtime.GOOS != "windows" {
		if err := os.Chmod(filepath.Join(first, "main.c"), 0755); err != nil {
			t.Fatal(err)
		}
		if err := verifySource(context.Background(), input, first); err == nil {
			t.Fatal("tracked mode change passed")
		}
		gitTest(t, first, "reset", "--hard", "HEAD")
	}
	if err := os.WriteFile(filepath.Join(second, "generated.tmp"), []byte("build output"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := verifySource(context.Background(), input, second); err != nil {
		t.Fatal("ordinary generated file rejected", err)
	}
	for _, relative := range []string{"../x", "a/../x", ".git/x", "/x", "C:/x", "a\\x"} {
		if _, err := managedSourcePath(second, relative); err == nil {
			t.Fatal("unsafe path accepted", relative)
		}
	}
}

func TestBuildEnvironmentAndLFSFilesAreIndependent(t *testing.T) {
	t.Setenv("ANTHROPIC_API_KEY", "must-not-leak")
	t.Setenv("ARTIFACTORY_MASTER_KEY", "must-not-leak")
	for _, value := range buildEnvironment() {
		if strings.Contains(value, "must-not-leak") {
			t.Fatal("daemon key inherited")
		}
	}
	root := t.TempDir()
	source := filepath.Join(root, "cache")
	destination := filepath.Join(root, "attempt")
	if err := os.MkdirAll(source, 0700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(source, "object"), []byte("original"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := copyLFS(source, destination); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(destination, "object"), []byte("changed"), 0600); err != nil {
		t.Fatal(err)
	}
	data, err := os.ReadFile(filepath.Join(source, "object"))
	if err != nil || string(data) != "original" {
		t.Fatal("attempt modified LFS cache", err)
	}
}
