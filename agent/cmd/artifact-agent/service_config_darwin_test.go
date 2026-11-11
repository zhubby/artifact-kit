package main

import (
	"bytes"
	"encoding/json"
	"os/exec"
	"path/filepath"
	"testing"
	"text/template"

	"github.com/kardianos/service"
)

func TestLaunchdAllowsSupervisorCleanup(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config & data", "agent.json")
	settings := &service.Config{
		Name: "artifact-kit-agent", Arguments: []string{"service-run", "--config", path},
		UserName: "ordinary-build-user", WorkingDirectory: filepath.Dir(path),
		EnvVars: map[string]string{"HOME": filepath.Dir(path)}, Option: service.KeyValue{},
	}
	if err := configureService(settings, path); err != nil {
		t.Fatal(err)
	}
	config, ok := settings.Option["LaunchdConfig"].(string)
	if !ok {
		t.Fatal("launchd has no explicit exit deadline for the supervisor's eight second cleanup")
	}
	functions := template.FuncMap{"bool": func(v bool) string {
		if v {
			return "true"
		}
		return "false"
	}}
	tpl, err := template.New("").Funcs(functions).Parse(config)
	if err != nil {
		t.Fatal(err)
	}
	// Use the same template input as kardianos/service's launchd installer.
	view := struct {
		*service.Config
		Path                                string
		KeepAlive, RunAtLoad, SessionCreate bool
		StandardOutPath, StandardErrorPath  string
	}{settings, "/usr/local/libexec/artifact & kit/agent", true, false, false,
		filepath.Join(filepath.Dir(path), "out.log"), filepath.Join(filepath.Dir(path), "err.log")}
	var xml bytes.Buffer
	if err := tpl.Execute(&xml, view); err != nil {
		t.Fatal(err)
	}
	command := exec.Command("/usr/bin/plutil", "-convert", "json", "-o", "-", "--", "-")
	command.Stdin = &xml
	data, err := command.CombinedOutput()
	if err != nil {
		t.Fatalf("invalid service plist: %v: %s", err, data)
	}
	var plist map[string]any
	if err := json.Unmarshal(data, &plist); err != nil {
		t.Fatal(err)
	}
	if deadline, _ := plist["ExitTimeOut"].(float64); deadline < 15 {
		t.Fatalf("launchd will stop before supervisor cleanup: %v", plist["ExitTimeOut"])
	}
	arguments := plist["ProgramArguments"].([]any)
	if arguments[0] != view.Path || arguments[3] != path || plist["UserName"] != settings.UserName ||
		plist["WorkingDirectory"] != settings.WorkingDirectory || plist["KeepAlive"] != true ||
		plist["EnvironmentVariables"].(map[string]any)["HOME"] != filepath.Dir(path) {
		t.Fatalf("service identity, paths or environment changed: %v", plist)
	}
}
