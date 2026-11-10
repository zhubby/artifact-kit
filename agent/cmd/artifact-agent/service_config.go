package main

import (
	"errors"
	"fmt"
	"os"
	"strconv"
	"strings"

	"github.com/kardianos/service"
)

func configureService(settings *service.Config) error {
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
		// systemd expands percent specifiers and command-line dollar variables
		// even inside quotes. Escape both before its C-style string parsing.
		values[index] = strconv.Quote(strings.NewReplacer("%", "%%", "$", "$$").Replace(value))
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
