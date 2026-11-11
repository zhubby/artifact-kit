package client

import (
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/pem"
	"errors"
	"os"
	"path/filepath"
	"runtime"
	"time"

	pb "github.com/zhubby/artifact-kit/agent/gen/agentv1"
	"github.com/zhubby/artifact-kit/agent/internal/config"
)

func Enroll(ctx context.Context, path string, cfg config.Config, token string) error {
	if _, err := os.Stat(path); err == nil {
		return errors.New("config already exists; revoke the old node before re-registering with a new configuration")
	} else if !os.IsNotExist(err) {
		return err
	}
	for _, name := range []string{"node.pem", "node.key"} {
		if _, err := os.Stat(filepath.Join(cfg.Root, name)); err == nil {
			return errors.New("managed root already has a node identity; use a new root for re-registration")
		} else if !os.IsNotExist(err) {
			return err
		}
	}
	if err := os.MkdirAll(cfg.Root, 0700); err != nil {
		return err
	}
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		return err
	}
	request, err := x509.CreateCertificateRequest(rand.Reader, &x509.CertificateRequest{Subject: pkix.Name{CommonName: cfg.Name}}, key)
	if err != nil {
		return err
	}
	connection, err := Dial(cfg, true)
	if err != nil {
		return err
	}
	defer connection.Close()
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	reply, err := pb.NewEnrollmentClient(connection).Enroll(ctx, &pb.EnrollRequest{Token: token, Name: cfg.Name, Os: runtime.GOOS, Arch: runtime.GOARCH, CsrPem: string(pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE REQUEST", Bytes: request})), ProtocolMajor: 1})
	if err != nil {
		return err
	}
	privateKey, err := x509.MarshalPKCS8PrivateKey(key)
	if err != nil {
		return err
	}
	cfg.AgentID = reply.AgentId
	cfg.CertificateFile = filepath.Join(cfg.Root, "node.pem")
	cfg.KeyFile = filepath.Join(cfg.Root, "node.key")
	if err = os.WriteFile(cfg.KeyFile, pem.EncodeToMemory(&pem.Block{Type: "PRIVATE KEY", Bytes: privateKey}), 0600); err != nil {
		return err
	}
	if err = os.WriteFile(cfg.CertificateFile, []byte(reply.CertificatePem), 0600); err != nil {
		return err
	}
	return config.Save(path, cfg)
}
