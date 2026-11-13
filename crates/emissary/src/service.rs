use anyhow::{Context, Result, ensure};
use std::path::Path;
pub const NAME: &str = "artifactory-emissary";
fn checked(value: &str) -> Result<&str> {
    ensure!(
        !value.contains(['\0', '\r', '\n']),
        "service inputs must not contain control characters"
    );
    Ok(value)
}
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
pub fn launchd(executable: &Path, config: &Path, user: &str) -> Result<String> {
    let executable = checked(executable.to_str().context("service path must be UTF-8")?)?;
    let path = checked(
        config
            .to_str()
            .context("configuration path must be UTF-8")?,
    )?;
    checked(user)?;
    let log = config
        .parent()
        .context("configuration parent missing")?
        .join("emissary.log");
    let home = config.parent().context("configuration parent missing")?;
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{NAME}</string>
<key>ProgramArguments</key><array><string>{}</string><string>service-run</string><string>--config</string><string>{}</string></array>
<key>UserName</key><string>{}</string>
<key>WorkingDirectory</key><string>{}</string>
<key>EnvironmentVariables</key><dict><key>HOME</key><string>{}</string></dict>
<key>KeepAlive</key><true/><key>RunAtLoad</key><true/>
<key>ExitTimeOut</key><integer>15</integer><key>ThrottleInterval</key><integer>120</integer>
<key>StandardOutPath</key><string>{}</string><key>StandardErrorPath</key><string>{}</string>
</dict></plist>
"#,
        xml(executable),
        xml(path),
        xml(user),
        xml(&home.to_string_lossy()),
        xml(&home.to_string_lossy()),
        xml(&log.to_string_lossy()),
        xml(&log.to_string_lossy())
    ))
}
pub fn systemd(executable: &Path, config: &Path, user: &str) -> Result<String> {
    checked(user)?;
    let quote = |value: &str, argument: bool| -> Result<String> {
        let value = checked(value)?.replace('%', "%%");
        let value = if argument {
            value.replace('$', "$$")
        } else {
            value
        };
        Ok(format!(
            "\"{}\"",
            value.replace('\\', "\\\\").replace('"', "\\\"")
        ))
    };
    Ok(format!(
        "[Unit]\nDescription=Artifactory Emissary native build runtime\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nExecStart={} service-run --config {}\nUser={}\nRestart=always\nRestartSec=120\nKillMode=mixed\nTimeoutStopSec=15\nEnvironmentFile=-/etc/sysconfig/artifactory-emissary\n\n[Install]\nWantedBy=multi-user.target\n",
        quote(
            executable.to_str().context("invalid executable path")?,
            false
        )?,
        quote(config.to_str().context("invalid config path")?, true)?,
        user.replace('%', "%%")
    ))
}
pub fn manage(action: &str, config: &Path, user: Option<&str>) -> Result<()> {
    ensure!(
        ["install", "uninstall", "start", "stop", "restart"].contains(&action),
        "invalid service action"
    );
    if action == "install" {
        let user = user.context(
            "installation requires --user with a dedicated unprivileged service account",
        )?;
        ensure!(
            !user.is_empty()
                && !["root", "localsystem", "nt authority\\system"]
                    .iter()
                    .any(|s| user.eq_ignore_ascii_case(s)),
            "installation requires a dedicated unprivileged service account"
        );
        checked(user)?;
    }
    platform_manage(action, config, user)
}
#[cfg(unix)]
fn run(program: &str, args: &[&std::ffi::OsStr]) -> Result<()> {
    let status = std::process::Command::new(program).args(args).status()?;
    ensure!(
        status.success(),
        "native service operation failed: {program}"
    );
    Ok(())
}
#[cfg(unix)]
fn platform_manage(action: &str, config: &Path, user: Option<&str>) -> Result<()> {
    use std::{ffi::OsStr, fs};
    let executable = std::env::current_exe()?;
    if action == "install" {
        let uid = std::process::Command::new("id")
            .args(["-u", user.context("missing service user")?])
            .output()?;
        ensure!(
            uid.status.success() && std::str::from_utf8(&uid.stdout)?.trim() != "0",
            "service account must exist and be unprivileged"
        );
        crate::config::Config::load(config)?;
    }
    if cfg!(target_os = "macos") {
        let path = std::path::PathBuf::from(format!("/Library/LaunchDaemons/{NAME}.plist"));
        let target = format!("system/{NAME}");
        let loaded = || -> Result<bool> {
            Ok(std::process::Command::new("launchctl")
                .args(["print", &target])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()?
                .success())
        };
        let stop = || -> Result<()> {
            if loaded()? {
                // Removing the loaded job prevents KeepAlive from respawning it.
                // bootout gives the supervisor ExitTimeOut to stop its workers.
                run("launchctl", &[OsStr::new("bootout"), OsStr::new(&target)])?;
                // bootout returns before launchd removes the terminating job.
                // Starting it during that interval can kick the old instance,
                // leaving no job after it exits. Wait beyond ExitTimeOut (15s).
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
                while loaded()? {
                    ensure!(
                        std::time::Instant::now() < deadline,
                        "launchd service did not finish stopping: {target}"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
            Ok(())
        };
        let start = || -> Result<()> {
            if loaded()? {
                run("launchctl", &[OsStr::new("kickstart"), OsStr::new(&target)])
            } else {
                run(
                    "launchctl",
                    &[
                        OsStr::new("bootstrap"),
                        OsStr::new("system"),
                        path.as_os_str(),
                    ],
                )
            }
        };
        match action {
            "install" => {
                fs::write(
                    &path,
                    launchd(&executable, config, user.context("missing service user")?)?,
                )?;
                Ok(())
            }
            "uninstall" => {
                stop()?;
                fs::remove_file(path)?;
                Ok(())
            }
            "start" => start(),
            "stop" => stop(),
            "restart" => {
                stop()?;
                start()
            }
            _ => unreachable!("validated service action"),
        }
    } else {
        let path = std::path::PathBuf::from(format!("/etc/systemd/system/{NAME}.service"));
        match action {
            "install" => {
                fs::write(
                    &path,
                    systemd(&executable, config, user.context("missing service user")?)?,
                )?;
                run("systemctl", &[OsStr::new("daemon-reload")])?;
                run("systemctl", &[OsStr::new("enable"), OsStr::new(NAME)])
            }
            "uninstall" => {
                let _ = run(
                    "systemctl",
                    &[OsStr::new("disable"), OsStr::new("--now"), OsStr::new(NAME)],
                );
                fs::remove_file(path)?;
                run("systemctl", &[OsStr::new("daemon-reload")])
            }
            _ => run("systemctl", &[OsStr::new(action), OsStr::new(NAME)]),
        }
    }
}
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::dispatch;
#[cfg(windows)]
use windows::platform_manage;
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launchd_allows_supervisor_cleanup() {
        let text = launchd(
            Path::new("/opt/bin/emissary & runtime"),
            Path::new("/private/user/emissary.json"),
            "builder",
        )
        .unwrap();
        assert!(text.contains("<key>ExitTimeOut</key><integer>15</integer>"));
        assert!(text.contains("emissary &amp; runtime"));
        let log = Path::new("/private/user").join("emissary.log");
        assert!(text.contains(log.to_str().unwrap()));
        assert!(text.contains("<key>WorkingDirectory</key><string>/private/user</string>"));
        assert!(text.contains("<key>HOME</key><string>/private/user</string>"));
    }
    #[test]
    fn systemd_quotes_paths_and_rejects_control_characters() {
        let text = systemd(
            Path::new("/opt/emissary worker%"),
            Path::new("/var/$builder/with space/emissary.json"),
            "builder",
        )
        .unwrap();
        assert!(text.contains("\"/opt/emissary worker%%\""));
        assert!(text.contains("/var/$$builder/with space/emissary.json\""));
        assert!(systemd(Path::new("/bin/runtime"), Path::new("/tmp/a\nb"), "builder").is_err());
        assert!(manage("install", Path::new("missing"), Some("root")).is_err());
    }
}
