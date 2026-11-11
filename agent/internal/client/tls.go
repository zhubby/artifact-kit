package client

import (
	"crypto/tls"
	"crypto/x509"
	"errors"
	"os"

	"github.com/zhubby/artifact-kit/agent/internal/config"
	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials"
)

func Dial(config config.Config, enrollment bool) (*grpc.ClientConn, error) {
	ca, err := os.ReadFile(config.CAFile)
	if err != nil {
		return nil, err
	}
	roots := x509.NewCertPool()
	if !roots.AppendCertsFromPEM(ca) {
		return nil, errors.New("invalid pinned CA")
	}
	tlsConfig := &tls.Config{MinVersion: tls.VersionTLS13, RootCAs: roots}
	if !enrollment {
		certificate, err := tls.LoadX509KeyPair(config.CertificateFile, config.KeyFile)
		if err != nil {
			return nil, err
		}
		tlsConfig.Certificates = []tls.Certificate{certificate}
	}
	return grpc.NewClient(config.Endpoint, grpc.WithTransportCredentials(credentials.NewTLS(tlsConfig)), grpc.WithDefaultCallOptions(grpc.MaxCallRecvMsgSize(512*1024)))
}
