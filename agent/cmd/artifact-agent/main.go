package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/signal"
	"path/filepath"
	"runtime"
	"strings"
	"time"

	"github.com/artifact-kit/agent/internal/agent"
	"github.com/artifact-kit/agent/internal/client"
	"github.com/artifact-kit/agent/internal/config"
	"github.com/artifact-kit/agent/internal/ota"
	"github.com/artifact-kit/agent/internal/worker"
	"github.com/kardianos/service"
)

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func run() error {
	if os.Getenv("AK_GIT_ASKPASS") == "1" {
		worker.Askpass(os.Args[1:])
		return nil
	}
	if len(os.Args) < 2 {
		return errors.New("usage: artifact-agent register|run|doctor|service|version")
	}
	if os.Args[1] == "worker" {
		ctx, stop := workerSignalContext()
		defer stop()
		return worker.Run(ctx, os.Stdin, os.Stdout)
	}
	if os.Args[1] == "git-askpass" {
		worker.Askpass(os.Args[2:])
		return nil
	}
	if os.Args[1] == "version" {
		fmt.Printf("artifact-agent %s %s/%s protocol=1 commit=%s\n", agent.Version, runtime.GOOS, runtime.GOARCH, agent.Commit)
		return nil
	}
	base, err := os.UserConfigDir()
	if err != nil {
		// System service accounts may have no interactive profile environment.
		// An explicit configuration still supplies every path the Agent needs.
		// Leave its default empty and validate it after parsing the flags.
		base = ""
	}
	defaultConfig := ""
	defaultRoot := ""
	if base != "" {
		defaultConfig = filepath.Join(base, "artifact-kit", "agent.json")
		defaultRoot = filepath.Join(base, "artifact-kit", "data")
	}
	flags := flag.NewFlagSet(os.Args[1], flag.ContinueOnError)
	configPath := flags.String("config", defaultConfig, "agent configuration path")
	endpoint := flags.String("endpoint", "", "gRPC host:port")
	ca := flags.String("ca", "", "pinned control-plane CA file")
	root := flags.String("root", defaultRoot, "managed data root")
	name := flags.String("name", "", "node display name")
	tokenEnvironment := flags.String("token-env", "AK_ENROLLMENT_TOKEN", "environment variable containing one-time enrollment token")
	allowHTTP := flags.Bool("allow-http-uploads", false, "allow HTTP object-store uploads for explicit local development only")
	serviceAction := flags.String("action", "", "install, uninstall, start, stop or restart")
	user := flags.String("user", "", "dedicated unprivileged service account; required for installation")
	publicKeys := flags.String("public-keys", "", "comma-separated base64 Ed25519 verification keys for ota-init")
	if err = flags.Parse(os.Args[2:]); err != nil {
		return err
	}
	if *configPath == "" {
		return errors.New("--config is required when the account has no configuration directory")
	}
	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt)
	defer cancel()
	switch os.Args[1] {
	case "ota-init":
		cfg, err := config.Load(*configPath)
		if err != nil {
			return err
		}
		executable, err := os.Executable()
		if err != nil {
			return err
		}
		if *publicKeys == "" {
			return errors.New("--public-keys is required")
		}
		return ota.Initialize(*configPath, filepath.Join(cfg.Root, "ota"), executable, agent.Version, strings.Split(*publicKeys, ","))
	case "register":
		if *endpoint == "" || *ca == "" {
			return errors.New("--endpoint and --ca are required")
		}
		if *root == "" {
			return errors.New("--root is required when the account has no configuration directory")
		}
		if *name == "" {
			*name, _ = os.Hostname()
		}
		rootPath, err := filepath.Abs(*root)
		if err != nil {
			return err
		}
		caPath, err := filepath.Abs(*ca)
		if err != nil {
			return err
		}
		cfg := config.Config{Endpoint: *endpoint, CAFile: caPath, Name: *name, Root: rootPath, AllowHTTPUploads: *allowHTTP}
		if err = client.Enroll(ctx, *configPath, cfg, os.Getenv(*tokenEnvironment)); err != nil {
			return err
		}
		fmt.Println("node enrolled; configuration saved to", *configPath)
		return nil
	case "run":
		cfg, err := config.Load(*configPath)
		if err != nil {
			return err
		}
		if os.Getenv("AK_OTA_DIRECTORY") == "" {
			if _, err := ota.Load(*configPath); err == nil {
				return ota.Supervise(ctx, *configPath, agent.Version)
			} else if !os.IsNotExist(err) {
				return err
			}
		}
		return agent.Run(ctx, cfg)
	case "doctor":
		cfg, err := config.Load(*configPath)
		if err != nil {
			cfg = config.Config{Root: *root}
		}
		diagnostics, capabilities := agent.Doctor(cfg)
		diagnostics["capabilities"] = capabilities
		diagnostics["registered"] = cfg.AgentID != ""
		diagnostics["managed_root"] = cfg.Root
		diagnostics["configuration_loaded"] = err == nil
		encoder := json.NewEncoder(os.Stdout)
		encoder.SetIndent("", "  ")
		return encoder.Encode(diagnostics)
	case "service":
		if !map[string]bool{"install": true, "uninstall": true, "start": true, "stop": true, "restart": true}[*serviceAction] {
			return errors.New("--action must be install, uninstall, start, stop or restart")
		}
		if *serviceAction == "install" && (*user == "" || *user == "root" || *user == "LocalSystem") {
			return errors.New("installation requires --user with a dedicated unprivileged service account")
		}
		absolute, err := filepath.Abs(*configPath)
		if err != nil {
			return err
		}
		program := &agentService{configPath: absolute}
		settings := &service.Config{Name: "artifact-kit-agent", DisplayName: "Artifact Kit Agent", Description: "Outbound authenticated native build worker", Arguments: []string{"service-run", "--config", absolute}, UserName: *user, Option: service.KeyValue{"Password": os.Getenv("AK_SERVICE_PASSWORD")}}
		if *serviceAction == "install" {
			if err := configureService(settings, absolute); err != nil {
				return err
			}
		}
		instance, err := service.New(program, settings)
		if err != nil {
			return err
		}
		return service.Control(instance, *serviceAction)
	case "service-run":
		absolute, err := filepath.Abs(*configPath)
		if err != nil {
			return err
		}
		instance, err := service.New(&agentService{configPath: absolute}, &service.Config{Name: "artifact-kit-agent"})
		if err != nil {
			return err
		}
		return instance.Run()
	default:
		return errors.New("unknown command")
	}
}

type agentService struct {
	configPath string
	cancel     context.CancelFunc
	done       chan struct{}
}

func (program *agentService) Start(instance service.Service) error {
	cfg, err := config.Load(program.configPath)
	if err != nil {
		return err
	}
	ctx, cancel := context.WithCancel(context.Background())
	program.cancel = cancel
	program.done = make(chan struct{})
	go func() {
		defer close(program.done)
		var runError error
		if _, err := ota.Load(program.configPath); err == nil {
			runError = ota.Supervise(ctx, program.configPath, agent.Version)
		} else if os.IsNotExist(err) {
			runError = agent.Run(ctx, cfg)
		} else {
			runError = err
		}
		if err := runError; err != nil {
			if logger, logErr := instance.Logger(nil); logErr == nil {
				_ = logger.Error(err)
			} else {
				fmt.Fprintln(os.Stderr, err)
			}
			// Returning only from this goroutine leaves the service manager
			// reporting a running service with no Agent. A fatal Runner error
			// must terminate the process so native failure policy can apply.
			os.Exit(1)
		}
	}()
	return nil
}
func (program *agentService) Stop(_ service.Service) error {
	if program.cancel != nil {
		program.cancel()
	}
	if program.done != nil {
		select {
		case <-program.done:
		case <-time.After(5 * time.Second):
		}
	}
	return nil
}
