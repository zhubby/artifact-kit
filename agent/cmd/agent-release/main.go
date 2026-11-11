// agent-release creates the exact-byte signed release envelope in CI. Only this
// command reads the signing seed; the control plane and Agents need public keys.
package main

import (
	"crypto/ed25519"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"path/filepath"

	"github.com/zhubby/artifact-kit/agent/internal/ota"
)

func run() error {
	directory := flag.String("directory", "bin", "release binary directory")
	version := flag.String("version", "", "GitHub release tag")
	commit := flag.String("commit", "", "full Git commit")
	flag.Parse()
	seed, err := base64.StdEncoding.DecodeString(os.Getenv("ARTIFACTORY_AGENT_RELEASE_SIGNING_SEED"))
	if err != nil || len(seed) != ed25519.SeedSize {
		return fmt.Errorf("ARTIFACTORY_AGENT_RELEASE_SIGNING_SEED must contain a base64 Ed25519 seed (32 bytes)")
	}
	key := ed25519.NewKeyFromSeed(seed)
	m := ota.Manifest{Version: *version, Commit: *commit, ProtocolMin: 1, ProtocolMax: 1}
	for _, platform := range [][2]string{{"linux", "amd64"}, {"linux", "arm64"}, {"darwin", "amd64"}, {"darwin", "arm64"}, {"windows", "amd64"}} {
		a := ota.Asset{OS: platform[0], Arch: platform[1], Name: "artifactory-agent-" + platform[0] + "-" + platform[1]}
		if a.OS == "windows" {
			a.Name += ".exe"
		}
		data, err := os.ReadFile(filepath.Join(*directory, a.Name))
		if err != nil {
			return err
		}
		hash := sha256.Sum256(data)
		a.Size = int64(len(data))
		a.SHA256 = hex.EncodeToString(hash[:])
		m.Assets = append(m.Assets, a)
	}
	raw, err := json.MarshalIndent(m, "", "  ")
	if err != nil {
		return err
	}
	raw = append(raw, '\n')
	signature := base64.StdEncoding.EncodeToString(ed25519.Sign(key, raw))
	if _, err = ota.Verify(raw, signature, []string{base64.StdEncoding.EncodeToString(key.Public().(ed25519.PublicKey))}, "linux", "amd64"); err != nil {
		return err
	}
	if err = os.WriteFile(filepath.Join(*directory, "agent-manifest.json"), raw, 0644); err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(*directory, "agent-manifest.sig"), []byte(signature+"\n"), 0644)
}
func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
