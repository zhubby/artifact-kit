package main

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"

	"github.com/kardianos/service"
)

func configureService(settings *service.Config, configPath string) error {
	if runtime.GOOS == "darwin" {
		// launchd opens stdout/stderr after dropping to the dedicated user.
		// That account cannot create the dependency's default /var/log files;
		// its existing private configuration directory is writable by design.
		settings.Option["LogDirectory"] = filepath.Dir(configPath)
	}
	if service.Platform() != "linux-systemd" {
		return nil
	}
	executable, err := os.Executable()
	if err != nil {
		return err
	}
	// The dependency's ConditionFileIsExecutable uses an ExecStart-specific
	// space escape that systemd treats literally in conditions. Use a unit
	// with quoted arguments and let ExecStart report missing executables.
	values := append([]string{executable}, settings.Arguments...)
	for index, value := range values {
		if strings.ContainsAny(value, "\x00\r\n") {
			return errors.New("service paths must not contain control characters")
		}
		// Percent specifiers apply to every token. Dollar expansion only
		// applies to arguments; the executable itself is always literal.
		value = strings.ReplaceAll(value, "%", "%%")
		if index > 0 {
			value = strings.ReplaceAll(value, "$", "$$")
		}
		values[index] = strconv.Quote(value)
	}
	if strings.ContainsAny(settings.UserName, "\x00\r\n") {
		return errors.New("service account must not contain control characters")
	}
	unit := fmt.Sprintf(`[Unit]
Description=Outbound authenticated native build worker
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=%s
User=%s
Restart=always
RestartSec=120
KillMode=control-group
EnvironmentFile=-/etc/sysconfig/artifact-kit-agent

[Install]
WantedBy=multi-user.target
`, strings.Join(values, " "), strings.ReplaceAll(settings.UserName, "%", "%%"))
	// kardianos/service parses SystemdScript as a Go template. Emit a literal
	// string so braces in filesystem paths cannot become template syntax.
	settings.Option["SystemdScript"] = "{{" + strconv.Quote(unit) + "}}"
	return nil
}
