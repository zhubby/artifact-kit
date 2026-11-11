package ota

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
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"testing"
)

func TestInterruptedDownloadNeverReplacesCurrentBinary(t *testing.T) {
	pub, key, _ := ed25519.GenerateKey(rand.Reader)
	body := []byte("complete release binary")
	hash := sha256.Sum256(body)
	name := "artifact-agent-" + runtime.GOOS + "-" + runtime.GOARCH
	if runtime.GOOS == "windows" {
		name += ".exe"
	}
	m := Manifest{Version: "v-test", Commit: strings.Repeat("a", 40), ProtocolMin: 1, ProtocolMax: 1, Assets: []Asset{{OS: runtime.GOOS, Arch: runtime.GOARCH, Name: name, Size: int64(len(body)), SHA256: hex.EncodeToString(hash[:])}}}
	raw, _ := json.Marshal(m)
	signature := base64.StdEncoding.EncodeToString(ed25519.Sign(key, raw))
	for _, kind := range []string{"truncated", "corrupt", "oversized"} {
		t.Run(kind, func(t *testing.T) {
			directory := t.TempDir()
			current := filepath.Join(directory, "current")
			if err := os.WriteFile(current, []byte("known good"), 0700); err != nil {
				t.Fatal(err)
			}
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				switch kind {
				case "truncated":
					w.Header().Set("Content-Length", strconv.Itoa(len(body)))
					_, _ = w.Write(body[:3])
				case "corrupt":
					_, _ = w.Write([]byte(strings.Repeat("x", len(body))))
				case "oversized":
					_, _ = w.Write(append(body, 1))
				}
			}))
			defer server.Close()
			_, err := download(context.Background(), Config{Directory: directory, AllowHTTP: true, PublicKeys: []string{base64.StdEncoding.EncodeToString(pub)}}, Request{Manifest: string(raw), Signature: signature, URL: server.URL})
			if err == nil {
				t.Fatal("unsafe download succeeded")
			}
			old, err := os.ReadFile(current)
			if err != nil || string(old) != "known good" {
				t.Fatal("current runtime was changed")
			}
			candidate := filepath.Join(directory, "versions", m.Assets[0].SHA256, "artifact-agent")
			if runtime.GOOS == "windows" {
				candidate += ".exe"
			}
			if _, err = os.Stat(candidate); !os.IsNotExist(err) {
				t.Fatal("unverified candidate was published")
			}
		})
	}
}
