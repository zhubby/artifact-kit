// Deterministic model and read-only Git upstream for the isolated Kubernetes test.
package main

import (
	"crypto/sha256"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"net/http/cgi"
	"net/http/httputil"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync/atomic"
	"time"
)

func git(dir string, args ...string) {
	c := exec.Command("git", append([]string{"-C", dir, "-c", "user.name=Operator Acceptance", "-c", "user.email=operator@localhost", "-c", "commit.gpgsign=false"}, args...)...)
	if out, e := c.CombinedOutput(); e != nil {
		log.Fatalf("git fixture failed: %s: %s", e, out)
	}
}
func main() {
	root := "/tmp/upstream"
	repo := filepath.Join(root, "repo.git")
	_ = os.MkdirAll(repo, 0700)
	git(repo, "init", "-b", "main")
	_ = os.WriteFile(filepath.Join(repo, "README.md"), []byte("Build with sh build.sh. The artifact is dist/result.txt and contains operator-native-ok.\n"), 0600)
	lfsPayload := []byte("operator-source-lfs\n")
	lfsOID := fmt.Sprintf("%x", sha256.Sum256(lfsPayload))
	git(repo, "lfs", "install", "--local")
	_ = os.WriteFile(filepath.Join(repo, ".gitattributes"), []byte("payload.dat filter=lfs diff=lfs merge=lfs -text\n"), 0600)
	_ = os.WriteFile(filepath.Join(repo, "payload.dat"), lfsPayload, 0600)
	_ = os.WriteFile(filepath.Join(repo, "build.sh"), []byte("#!/bin/sh\nset -eu\ntest \"$(cat payload.dat)\" = operator-source-lfs\nmkdir -p dist\nprintf 'operator-native-ok\\n' > dist/result.txt\n"), 0700)
	git(repo, "add", ".")
	git(repo, "commit", "-m", "immutable source fixture")
	git(repo, "tag", "-a", "v-fixture", "-m", "frozen source")
	var calls atomic.Int64
	var hold atomic.Bool
	var gitDown atomic.Bool
	var dropAck atomic.Int64
	var dropped atomic.Int64
	var buildSeconds atomic.Int64
	dropAck.Store(1)
	mux := http.NewServeMux()
	mountLiveProvider(mux)
	mux.HandleFunc("/metrics", func(w http.ResponseWriter, r *http.Request) {
		fmt.Fprintf(w, "fixture_model_requests %d\nfixture_dropped_accept_acks %d\n", calls.Load(), dropped.Load())
	})
	mux.HandleFunc("/test/mode", func(w http.ResponseWriter, r *http.Request) {
		var value struct {
			Hold         bool  `json:"hold"`
			GitDown      bool  `json:"git_down"`
			ResetAck     bool  `json:"reset_ack"`
			BuildSeconds int64 `json:"build_seconds"`
		}
		if json.NewDecoder(http.MaxBytesReader(w, r.Body, 1024)).Decode(&value) != nil {
			http.Error(w, "bad mode", 400)
			return
		}
		hold.Store(value.Hold)
		if value.BuildSeconds < 0 || value.BuildSeconds > 30 {
			http.Error(w, "invalid build delay", 400)
			return
		}
		buildSeconds.Store(value.BuildSeconds)
		gitDown.Store(value.GitDown)
		if value.ResetAck {
			dropAck.Store(1)
			dropped.Store(0)
		}
		fmt.Fprint(w, "{}")
	})
	mux.HandleFunc("/v1/chat/completions", func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") != "Bearer fixture-provider-key" {
			http.Error(w, "provider identity rejected", 401)
			return
		}
		calls.Add(1)
		for hold.Load() {
			select {
			case <-r.Context().Done():
				return
			case <-time.After(100 * time.Millisecond):
			}
		}
		var request map[string]any
		if json.NewDecoder(http.MaxBytesReader(w, r.Body, 2<<20)).Decode(&request) != nil {
			http.Error(w, "bad input", 400)
			return
		}
		messages, _ := request["messages"].([]any)
		toolCount := 0
		for _, m := range messages {
			if o, ok := m.(map[string]any); ok && o["role"] == "tool" {
				toolCount++
			}
		}
		name := "list_tree"
		arguments := map[string]any{}
		if toolCount == 1 {
			name = "read_file"
			arguments = map[string]any{"path": "README.md"}
		}
		if toolCount >= 2 {
			name = "submit_plan"
			arch := "arm64"
			if os.Getenv("FIXTURE_ARCH") != "" {
				arch = os.Getenv("FIXTURE_ARCH")
			}
			targetOS := os.Getenv("FIXTURE_OS")
			if targetOS == "" {
				targetOS = "linux"
			}
			run := "sh build.sh"
			if seconds := buildSeconds.Load(); seconds > 0 {
				run = fmt.Sprintf("sleep %d; sh build.sh", seconds)
			}
			target := map[string]any{"os": targetOS, "arch": arch, "executor": "native", "timeout_seconds": 90, "steps": []any{map[string]any{"name": "build", "run": run, "timeout_seconds": 45}}, "artifacts": []string{"dist/result.txt"}}
			if toolCount == 2 {
				target["os"] = "invalid-platform"
			} // Must return validation feedback, then correct in the same run.
			arguments = map[string]any{"plan": map[string]any{"summary": "Build the fixed repository on a device Agent", "evidence": []any{map[string]any{"path": "README.md", "reason": "Documents the build command and output"}}, "recipe": map[string]any{"version": 1, "targets": map[string]any{"app": target}}}}
		}
		text, _ := json.Marshal(arguments)
		w.Header().Set("Content-Type", "text/event-stream")
		w.WriteHeader(200)
		chunk := func(delta any, reason any) {
			b, _ := json.Marshal(map[string]any{"id": "fixture", "object": "chat.completion.chunk", "model": "fixture", "created": 1, "choices": []any{map[string]any{"index": 0, "delta": delta, "finish_reason": reason}}})
			fmt.Fprintf(w, "data: %s\n\n", b)
		}
		chunk(map[string]any{"role": "assistant", "tool_calls": []any{map[string]any{"index": 0, "id": fmt.Sprintf("call-%d", toolCount), "type": "function", "function": map[string]any{"name": name, "arguments": string(text)}}}}, nil)
		chunk(map[string]any{}, "tool_calls")
		fmt.Fprint(w, "data: [DONE]\n\n")
	})
	gitPath, _ := exec.LookPath("git")
	mux.HandleFunc("/repo.git/info/lfs/objects/batch", func(w http.ResponseWriter, r *http.Request) {
		var request struct {
			Objects []struct {
				OID  string `json:"oid"`
				Size int64  `json:"size"`
			} `json:"objects"`
		}
		if gitDown.Load() || json.NewDecoder(http.MaxBytesReader(w, r.Body, 64<<10)).Decode(&request) != nil {
			http.Error(w, "upstream LFS unavailable", 503)
			return
		}
		objects := []any{}
		for _, object := range request.Objects {
			if object.OID == lfsOID && object.Size == int64(len(lfsPayload)) {
				objects = append(objects, map[string]any{"oid": lfsOID, "size": len(lfsPayload), "actions": map[string]any{"download": map[string]any{"href": "https://upstream:9443/repo.git/lfs/objects/" + lfsOID}}})
			} else {
				objects = append(objects, map[string]any{"oid": object.OID, "error": map[string]any{"code": 404, "message": "unknown fixture object"}})
			}
		}
		w.Header().Set("Content-Type", "application/vnd.git-lfs+json")
		_ = json.NewEncoder(w).Encode(map[string]any{"transfer": "basic", "objects": objects})
	})
	mux.HandleFunc("/repo.git/lfs/objects/"+lfsOID, func(w http.ResponseWriter, r *http.Request) {
		if gitDown.Load() {
			http.Error(w, "upstream LFS unavailable", 503)
			return
		}
		_, _ = w.Write(lfsPayload)
	})
	backend := &cgi.Handler{Path: gitPath, Args: []string{"http-backend"}, Root: "/", Env: []string{"GIT_PROJECT_ROOT=" + root, "GIT_HTTP_EXPORT_ALL=1", "GIT_CONFIG_COUNT=1", "GIT_CONFIG_KEY_0=http.receivepack", "GIT_CONFIG_VALUE_0=false"}}
	mux.HandleFunc("/repo.git/", func(w http.ResponseWriter, r *http.Request) {
		if gitDown.Load() {
			http.Error(w, "Git upstream unavailable", 503)
			return
		}
		if strings.Contains(r.URL.String(), "receive-pack") {
			http.Error(w, "read only", 403)
			return
		}
		backend.ServeHTTP(w, r)
	})
	control, _ := url.Parse("http://control-plane:8080")
	proxy := httputil.NewSingleHostReverseProxy(control)
	proxy.ModifyResponse = func(response *http.Response) error {
		if response.Request.URL.Path != "/internal/runtime/event" {
			return nil
		}
		body, e := io.ReadAll(io.LimitReader(response.Body, 2<<20))
		response.Body.Close()
		if e != nil {
			return e
		}
		var value map[string]any
		_ = json.Unmarshal(body, &value)
		if value["accepted"] == true && dropAck.CompareAndSwap(1, 0) {
			dropped.Add(1)
			response.StatusCode = 503
			body = []byte("ack intentionally unavailable after business commit")
		}
		response.Body = io.NopCloser(strings.NewReader(string(body)))
		response.ContentLength = int64(len(body))
		response.Header.Set("Content-Length", fmt.Sprint(len(body)))
		return nil
	}
	mux.Handle("/", proxy)
	log.Fatal(http.ListenAndServeTLS(":9443", "/run/pki/server.pem", "/run/pki/server.key", mux))
}
