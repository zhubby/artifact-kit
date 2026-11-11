package ota

import (
	"crypto/ed25519"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"strings"
	"testing"
)

func TestSignedPackageVerification(t *testing.T) {
	pub, priv, _ := ed25519.GenerateKey(rand.Reader)
	data := []byte("native agent")
	digest := sha256.Sum256(data)
	m := Manifest{Version: "v1.2.3", Commit: strings.Repeat("a", 40), ProtocolMin: 1, ProtocolMax: 1, Assets: []Asset{{OS: "darwin", Arch: "arm64", Name: "artifact-agent-darwin-arm64", Size: int64(len(data)), SHA256: hex.EncodeToString(digest[:])}}}
	raw, _ := json.Marshal(m)
	sig := base64.StdEncoding.EncodeToString(ed25519.Sign(priv, raw))
	keys := []string{base64.StdEncoding.EncodeToString(pub)}
	if _, err := Verify(raw, sig, keys, "darwin", "arm64"); err != nil {
		t.Fatal(err)
	}
	for _, test := range []struct {
		name          string
		raw           []byte
		sig, os, arch string
	}{
		{"signature", raw, base64.StdEncoding.EncodeToString(make([]byte, 64)), "darwin", "arm64"},
		{"changed", append(append([]byte{}, raw...), ' '), sig, "darwin", "arm64"},
		{"platform", raw, sig, "windows", "amd64"},
	} {
		t.Run(test.name, func(t *testing.T) {
			if _, err := Verify(test.raw, test.sig, keys, test.os, test.arch); err == nil {
				t.Fatal("unsafe package accepted")
			}
		})
	}
	for _, body := range [][]byte{data[:3], append(append([]byte{}, data...), 0), []byte("native agenX")} {
		if err := VerifyBytes(body, m.Assets[0]); err == nil {
			t.Fatal("damaged binary accepted")
		}
	}
	m.ProtocolMin = 2
	m.ProtocolMax = 2
	raw, _ = json.Marshal(m)
	sig = base64.StdEncoding.EncodeToString(ed25519.Sign(priv, raw))
	if _, err := Verify(raw, sig, keys, "darwin", "arm64"); err == nil {
		t.Fatal("incompatible protocol accepted")
	}
}

func TestInterruptedActivationRollsBackOnce(t *testing.T) {
	s := State{UpdateID: "update-1", Phase: "activating", Current: Image{Version: "A", Path: "A"}, Candidate: Image{Version: "B", Path: "B"}}
	if !s.Recover() || s.Phase != "rolled_back" || s.Current.Version != "A" || s.Candidate.Path != "" {
		t.Fatalf("unsafe recovery: %+v", s)
	}
	if s.Recover() {
		t.Fatal("recovery repeatedly retries failed candidate")
	}
}

func TestHealthRequiresConsecutiveSessionConfirmations(t *testing.T) {
	var h HealthWindow
	if h.Ack("s1", "1") || h.Ack("s1", "1") || h.Ack("s1", "2") {
		t.Fatal("confirmed too early or replay accepted")
	}
	if h.Ack("s2", "3") || h.Ack("s2", "4") {
		t.Fatal("session reset ignored")
	}
	if !h.Ack("s2", "5") {
		t.Fatal("three acknowledgments did not confirm")
	}
}
