package main

// Keep the installer inputs supported by kardianos/service's launchd template,
// with an explicit deadline longer than agentService.Stop and child.stop.
const launchdServiceConfig = `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Disabled</key><false/>
	<key>ExitTimeOut</key><integer>15</integer>
	{{- if .EnvVars}}
	<key>EnvironmentVariables</key>
	<dict>
		{{- range $k, $v := .EnvVars}}
		<key>{{html $k}}</key><string>{{html $v}}</string>
		{{- end}}
	</dict>
	{{- end}}
	<key>KeepAlive</key><{{bool .KeepAlive}}/>
	<key>Label</key><string>{{html .Name}}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{{html .Path}}</string>
		{{- range .Config.Arguments}}
		<string>{{html .}}</string>
		{{- end}}
	</array>
	{{- if .ChRoot}}
	<key>RootDirectory</key><string>{{html .ChRoot}}</string>
	{{- end}}
	<key>RunAtLoad</key><{{bool .RunAtLoad}}/>
	<key>SessionCreate</key><{{bool .SessionCreate}}/>
	{{- if .StandardErrorPath}}
	<key>StandardErrorPath</key><string>{{html .StandardErrorPath}}</string>
	{{- end}}
	{{- if .StandardOutPath}}
	<key>StandardOutPath</key><string>{{html .StandardOutPath}}</string>
	{{- end}}
	{{- if .UserName}}
	<key>UserName</key><string>{{html .UserName}}</string>
	{{- end}}
	{{- if .WorkingDirectory}}
	<key>WorkingDirectory</key><string>{{html .WorkingDirectory}}</string>
	{{- end}}
</dict>
</plist>
`
