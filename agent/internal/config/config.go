package config

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
)

type Config struct {
	Endpoint         string                       `json:"endpoint"`
	CAFile           string                       `json:"ca_file"`
	CertificateFile  string                       `json:"certificate_file"`
	KeyFile          string                       `json:"key_file"`
	AgentID          string                       `json:"agent_id"`
	Name             string                       `json:"name"`
	Root             string                       `json:"root"`
	GitCAFile        string                       `json:"git_ca_file,omitempty"`
	Tools            map[string]string            `json:"tools,omitempty"`
	SigningProfiles  map[string]map[string]string `json:"signing_profiles,omitempty"`
	LogBufferBytes   int64                        `json:"log_buffer_bytes,omitempty"`
	AllowHTTPUploads bool                         `json:"allow_http_uploads,omitempty"`
	SourceCacheBytes int64                        `json:"source_cache_bytes,omitempty"`
}

func Load(path string) (Config, error) {
	var config Config
	file, err := os.Open(path)
	if err != nil {
		return config, err
	}
	defer file.Close()
	decoder := json.NewDecoder(file)
	decoder.DisallowUnknownFields()
	if err = decoder.Decode(&config); err != nil {
		return config, err
	}
	if config.Endpoint == "" || config.CAFile == "" || config.Root == "" {
		return config, errors.New("endpoint, CA file and managed root are required")
	}
	config.Root, err = filepath.Abs(config.Root)
	if config.LogBufferBytes == 0 {
		config.LogBufferBytes = 256 * 1024 * 1024
	}
	if config.LogBufferBytes < 1024*1024 {
		return config, errors.New("log buffer must be at least 1 MiB")
	}
	if config.SourceCacheBytes == 0 {
		config.SourceCacheBytes = 20 * 1024 * 1024 * 1024
	}
	if config.SourceCacheBytes < 1024*1024 {
		return config, errors.New("source cache quota must be at least 1 MiB")
	}
	return config, err
}

func Save(path string, config Config) error {
	if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
		return err
	}
	data, err := json.MarshalIndent(config, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, data, 0600)
}
