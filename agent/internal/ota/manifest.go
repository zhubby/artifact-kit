// Package ota implements the fixed launcher and its private runtime mailbox.
package ota

import (
	"crypto/ed25519"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"regexp"
)

const MaxBinarySize int64 = 128 << 20

var safeVersion = regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$`)

type Asset struct {
	OS     string `json:"os"`
	Arch   string `json:"arch"`
	Name   string `json:"name"`
	Size   int64  `json:"size"`
	SHA256 string `json:"sha256"`
}
type Manifest struct {
	Version     string  `json:"version"`
	Commit      string  `json:"commit"`
	ProtocolMin int     `json:"protocol_min"`
	ProtocolMax int     `json:"protocol_max"`
	Assets      []Asset `json:"assets"`
}

func Verify(raw []byte, signature string, keys []string, osName, arch string) (Asset, error) {
	var result Asset
	if len(raw) > 65536 {
		return result, errors.New("oversized manifest")
	}
	sig, err := base64.StdEncoding.DecodeString(signature)
	if err != nil {
		return result, errors.New("invalid signature encoding")
	}
	valid := false
	for _, key := range keys {
		pub, err := base64.StdEncoding.DecodeString(key)
		if err == nil && len(pub) == ed25519.PublicKeySize && ed25519.Verify(pub, raw, sig) {
			valid = true
			break
		}
	}
	if !valid {
		return result, errors.New("manifest signature rejected")
	}
	var m Manifest
	if err = json.Unmarshal(raw, &m); err != nil {
		return result, err
	}
	if !safeVersion.MatchString(m.Version) || len(m.Commit) != 40 || m.ProtocolMin > 1 || m.ProtocolMax < 1 || m.ProtocolMin < 1 || len(m.Assets) == 0 || len(m.Assets) > 5 {
		return result, errors.New("incompatible release manifest")
	}
	seen := map[string]bool{}
	for _, a := range m.Assets {
		platform := a.OS + "/" + a.Arch
		name := "artifactory-agent-" + a.OS + "-" + a.Arch
		if a.OS == "windows" {
			name += ".exe"
		}
		allowed := platform == "linux/amd64" || platform == "linux/arm64" || platform == "darwin/amd64" || platform == "darwin/arm64" || platform == "windows/amd64"
		hash, err := hex.DecodeString(a.SHA256)
		if !allowed || seen[platform] || a.Name != name || a.Size <= 0 || a.Size > MaxBinarySize || err != nil || len(hash) != 32 || hex.EncodeToString(hash) != a.SHA256 {
			return result, errors.New("invalid release asset")
		}
		seen[platform] = true
		if a.OS == osName && a.Arch == arch {
			result = a
		}
	}
	if result.Name == "" {
		return result, fmt.Errorf("release has no compatible %s/%s binary", osName, arch)
	}
	return result, nil
}

func VerifyBytes(body []byte, a Asset) error {
	sum := sha256.Sum256(body)
	if int64(len(body)) != a.Size || hex.EncodeToString(sum[:]) != a.SHA256 {
		return errors.New("binary size or SHA-256 mismatch")
	}
	return nil
}
