//! Opt-in system service acceptance on disposable GitHub-hosted machines only.
mod support;
use artifactory_emissary::{ota, platform, protocol as pb, service};
use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::Duration,
};

const USER: &str = "artifactory-ci";
struct Machine {
    directory: PathBuf,
    binary: PathBuf,
    config: PathBuf,
    account: bool,
    installed: bool,
    password: String,
}
impl Machine {
    fn command(&self, admin: bool, program: impl AsRef<OsStr>, args: &[&OsStr]) -> Output {
        let mut command = if admin && cfg!(unix) {
            let mut cmd = Command::new("sudo");
            cmd.args(["-n", "--"]).arg(program);
            cmd
        } else {
            Command::new(program)
        };
        command
            .args(args)
            .env("ARTIFACTORY_SERVICE_PASSWORD", &self.password)
            .env("ARTIFACTORY_TEST_SERVICE_PASSWORD", &self.password)
            .env("EMISSARY_SERVICE_DIRECTORY", &self.directory);
        command
            .output()
            .expect("native test command could not start")
    }
    fn require(&self, program: &str, args: &[&str]) -> String {
        let args = args.iter().map(OsStr::new).collect::<Vec<_>>();
        let output = self.command(true, program, &args);
        assert!(
            output.status.success(),
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
    fn powershell(&self, script: &str) -> String {
        self.require(
            "pwsh",
            &["-NoProfile", "-NonInteractive", "-Command", script],
        )
    }
    fn action(&self, action: &str) -> Output {
        let mut args = vec![
            OsStr::new("service"),
            OsStr::new("--action"),
            OsStr::new(action),
            OsStr::new("--config"),
            self.config.as_os_str(),
        ];
        let user = if cfg!(windows) {
            format!(".\\{USER}")
        } else {
            USER.into()
        };
        if action == "install" {
            args.extend([OsStr::new("--user"), OsStr::new(&user)])
        }
        self.command(true, &self.binary, &args)
    }
    fn act(&self, action: &str) {
        let output = self.action(action);
        assert!(
            output.status.success(),
            "service {action} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fn state(&self) -> (bool, bool, u32, String) {
        if cfg!(target_os = "linux") {
            let output = self.command(
                false,
                "systemctl",
                &[
                    OsStr::new("show"),
                    OsStr::new(service::NAME),
                    OsStr::new("--property=LoadState,UnitFileState,MainPID,User"),
                ],
            );
            let text = String::from_utf8(output.stdout).unwrap();
            let fields = text
                .lines()
                .filter_map(|line| line.split_once('='))
                .collect::<std::collections::HashMap<_, _>>();
            (
                fields.get("LoadState").is_some_and(|v| *v != "not-found"),
                fields.get("UnitFileState") == Some(&"enabled"),
                fields.get("MainPID").unwrap_or(&"0").parse().unwrap(),
                fields.get("User").unwrap_or(&"").to_string(),
            )
        } else if cfg!(target_os = "macos") {
            let output = self.command(
                false,
                "sudo",
                &[
                    OsStr::new("-n"),
                    OsStr::new("launchctl"),
                    OsStr::new("print"),
                    OsStr::new("system/artifactory-emissary"),
                ],
            );
            let text = String::from_utf8(output.stdout).unwrap();
            let pid = text
                .lines()
                .find_map(|line| {
                    line.trim()
                        .strip_prefix("pid = ")
                        .and_then(|v| v.parse().ok())
                })
                .unwrap_or(0);
            (
                Path::new("/Library/LaunchDaemons/artifactory-emissary.plist").exists(),
                output.status.success(),
                pid,
                if pid > 0 {
                    self.require("ps", &["-o", "user=", "-p", &pid.to_string()])
                        .trim()
                        .into()
                } else {
                    String::new()
                },
            )
        } else {
            let text=self.powershell("$s=Get-CimInstance Win32_Service -Filter \"Name='artifactory-emissary'\"; if(!$s){'{\"installed\":false}'}else{@{installed=$true;enabled=($s.StartMode -eq 'Auto');pid=[int]$s.ProcessId;user=$s.StartName}|ConvertTo-Json -Compress}");
            let state: serde_json::Value = serde_json::from_str(&text).unwrap();
            (
                state["installed"] == true,
                state["enabled"] == true,
                state["pid"].as_u64().unwrap_or(0) as u32,
                state["user"]
                    .as_str()
                    .unwrap_or("")
                    .trim_start_matches(".\\")
                    .into(),
            )
        }
    }
    async fn running(&self) -> u32 {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let (installed, enabled, pid, user) = self.state();
                if pid > 0 {
                    assert!(installed && enabled);
                    assert_eq!(user, USER);
                    return pid;
                }
                tokio::time::sleep(Duration::from_millis(100)).await
            }
        })
        .await
        .expect("native service did not become ready")
    }
}
impl Drop for Machine {
    fn drop(&mut self) {
        if self.installed {
            let _ = self.action("stop");
            let _ = self.action("uninstall");
        }
        if self.account {
            if cfg!(target_os = "linux") {
                self.command(true, "userdel", &[OsStr::new(USER)]);
            } else if cfg!(target_os = "macos") {
                self.command(
                    true,
                    "dscl",
                    &[
                        OsStr::new("."),
                        OsStr::new("-delete"),
                        OsStr::new(&format!("/Users/{USER}")),
                    ],
                );
            } else {
                let script = Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../scripts/native-service-account.ps1");
                self.command(
                    true,
                    "pwsh",
                    &[
                        OsStr::new("-NoProfile"),
                        OsStr::new("-File"),
                        script.as_os_str(),
                        OsStr::new("-Action"),
                        OsStr::new("remove"),
                        OsStr::new("-Directory"),
                        self.directory.as_os_str(),
                    ],
                );
            }
        }
        if cfg!(unix) {
            self.command(true, "rm", &[OsStr::new("-rf"), self.directory.as_os_str()]);
        } else {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
}

#[tokio::test]
#[ignore = "changes services and accounts; requires explicitly enabled disposable GitHub VM"]
async fn native_service_lifecycle() {
    assert_eq!(
        std::env::var("ARTIFACTORY_NATIVE_SERVICE_TESTS").as_deref(),
        Ok("1")
    );
    assert_eq!(std::env::var("GITHUB_ACTIONS").as_deref(), Ok("true"));
    assert_eq!(
        std::env::var("RUNNER_ENVIRONMENT").as_deref(),
        Ok("github-hosted")
    );
    let (os, arch) = platform();
    assert_eq!(
        std::env::var("ARTIFACTORY_NATIVE_EXPECTED_PLATFORM").unwrap(),
        format!("{os}/{arch}")
    );
    assert_eq!(
        std::env::var("RUNNER_ARCH").unwrap(),
        if arch == "amd64" { "X64" } else { "ARM64" }
    );
    assert_eq!(
        std::env::var("RUNNER_OS").unwrap(),
        match os {
            "linux" => "Linux",
            "darwin" => "macOS",
            _ => "Windows",
        }
    );
    let base = if cfg!(windows) {
        PathBuf::from(std::env::var_os("ProgramData").expect("ProgramData required"))
    } else {
        PathBuf::from("/tmp")
    };
    let directory = tempfile::Builder::new()
        .prefix("emissary-service-")
        .tempdir_in(base)
        .unwrap()
        .keep()
        .canonicalize()
        .unwrap();
    let private = directory.join("private data %n $HOME");
    let bin = directory.join("emissary bin %i $HOME");
    fs::create_dir(&private).unwrap();
    fs::create_dir(&bin).unwrap();
    let mut machine = Machine {
        binary: bin.join(ota::binary_name()),
        config: private.join("emissary config.json"),
        directory,
        account: false,
        installed: false,
        password: format!("Emissary-{}!", uuid::Uuid::new_v4()),
    };
    assert!(
        !machine.state().0,
        "refusing to replace an existing service"
    );
    fs::copy(env!("CARGO_BIN_EXE_artifactory-emissary"), &machine.binary).unwrap();
    let mut fixture = support::Fixture::start(&private, &[&rustls::version::TLS13]).await;
    fixture.cfg.save(&machine.config).unwrap();
    let repo = private.join("repository");
    fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@localhost",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    git(&["init"]);
    fs::write(repo.join("README"), "fixture").unwrap();
    git(&["add", "."]);
    git(&["commit", "-m", "fixture"]);
    let commit = git(&["rev-parse", "HEAD"]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&machine.directory, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&machine.binary, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        for name in ["ca.pem", "client.pem", "client.key"] {
            fs::set_permissions(private.join(name), fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(
            !machine
                .command(false, "id", &[OsStr::new(USER)])
                .status
                .success(),
            "refusing existing test account"
        );
        if os == "linux" {
            machine.require(
                "useradd",
                &[
                    "--system",
                    "--home-dir",
                    private.to_str().unwrap(),
                    "--shell",
                    "/usr/sbin/nologin",
                    USER,
                ],
            );
            machine.account = true;
        } else {
            let users = machine.require("dscl", &[".", "-list", "/Users", "UniqueID"]);
            let mut uid = 6001;
            while users.split_whitespace().any(|word| word == uid.to_string()) {
                uid += 1;
            }
            let record = format!("/Users/{USER}");
            machine.require("dscl", &[".", "-create", &record]);
            machine.account = true;
            for (key, value) in [
                ("UniqueID", uid.to_string()),
                ("PrimaryGroupID", "20".into()),
                ("NFSHomeDirectory", private.to_str().unwrap().into()),
                ("UserShell", "/usr/bin/false".into()),
                ("IsHidden", "1".into()),
            ] {
                machine.require("dscl", &[".", "-create", &record, key, &value]);
            }
        }
        machine.require("chown", &["-R", USER, private.to_str().unwrap()]);
    }
    if cfg!(windows) {
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/native-service-account.ps1");
        machine.account = true;
        machine.require(
            "pwsh",
            &[
                "-NoProfile",
                "-File",
                script.to_str().unwrap(),
                "-Action",
                "create",
                "-Directory",
                machine.directory.to_str().unwrap(),
            ],
        );
        machine.powershell("$ErrorActionPreference='Stop'; $sid=(Get-LocalUser artifactory-ci).SID; $repo=Join-Path $env:EMISSARY_SERVICE_DIRECTORY 'private data %n $HOME/repository'; foreach($p in @((Get-Item -LiteralPath $repo))+@(Get-ChildItem -LiteralPath $repo -Recurse -Force)){$acl=Get-Acl -LiteralPath $p.FullName;$acl.SetOwner($sid);Set-Acl -LiteralPath $p.FullName -AclObject $acl}");
    }
    machine.installed = true;
    machine.act("install");
    machine.act("start");
    let mut first = fixture.connection().await;
    assert_eq!(first.hello.emissary_id, fixture.cfg.emissary_id);
    let first_pid = machine.running().await;
    let long = if cfg!(windows) {
        "$p=Start-Process pwsh -ArgumentList '-NoProfile','-Command','Start-Sleep 120' -PassThru; [IO.File]::WriteAllText('child.pid',$p.Id.ToString()); Write-Output tree-started; Wait-Process -Id $p.Id"
    } else {
        "sh -c 'sleep 120 & wait' & echo $! > child.pid; echo tree-started; wait"
    };
    for action in ["restart", "stop"] {
        let attempt = uuid::Uuid::new_v4().to_string();
        first
            .commands
            .send(pb::Command {
                kind: "assign".into(),
                attempt_id: attempt.clone(),
                lease_token: "native-fixture".into(),
                lease_seconds: 90,
                assignment: Some(pb::Assignment {
                    repository_url: repo.to_string_lossy().into(),
                    commit_sha: commit.clone(),
                    timeout_seconds: 120,
                    steps: vec![pb::Step {
                        name: "tree".into(),
                        shell: if cfg!(windows) { "pwsh" } else { "sh" }.into(),
                        run: long.into(),
                        timeout_seconds: 120,
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        let marker = fixture
            .cfg
            .root
            .join("work")
            .join(&attempt)
            .join("source/child.pid");
        let pid = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let output = if cfg!(unix) {
                    machine.command(true, "cat", &[marker.as_os_str()])
                } else {
                    Command::new("cmd")
                        .args([OsStr::new("/c"), OsStr::new("type"), marker.as_os_str()])
                        .output()
                        .unwrap()
                };
                if output.status.success()
                    && let Ok(pid) = String::from_utf8_lossy(&output.stdout)
                        .trim()
                        .parse::<u32>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("native Worker never started");
        machine.act(action);
        first.event("finished").await;
        first.closed().await;
        let output = if cfg!(unix) {
            machine.command(
                true,
                "ps",
                &[
                    OsStr::new("-o"),
                    OsStr::new("stat="),
                    OsStr::new("-p"),
                    OsStr::new(&pid.to_string()),
                ],
            )
        } else {
            machine.command(
                true,
                "pwsh",
                &[
                    OsStr::new("-Command"),
                    OsStr::new(&format!(
                        "if(Get-Process -Id {pid} -ErrorAction SilentlyContinue){{exit 1}}"
                    )),
                ],
            )
        };
        if cfg!(unix) {
            assert!(
                !output.status.success()
                    || String::from_utf8_lossy(&output.stdout)
                        .trim()
                        .starts_with('Z'),
                "descendant survived service {action}"
            );
        } else {
            assert!(
                output.status.success(),
                "descendant survived service {action}"
            );
        }
        #[cfg(unix)]
        assert!(String::from_utf8_lossy(&fixture.logs(&attempt)).contains("tree-started"));
        if action == "stop" {
            assert_eq!(machine.state().2, 0);
            machine.act("start");
        }
        first = fixture.connection().await;
        assert_ne!(machine.running().await, first_pid);
        let task = uuid::Uuid::new_v4().to_string();
        first
            .commands
            .send(pb::Command {
                kind: "assign".into(),
                attempt_id: task,
                lease_token: "followup".into(),
                lease_seconds: 90,
                assignment: Some(pb::Assignment {
                    repository_url: repo.to_string_lossy().into(),
                    commit_sha: commit.clone(),
                    timeout_seconds: 60,
                    steps: vec![pb::Step {
                        name: "followup".into(),
                        shell: if cfg!(windows) { "pwsh" } else { "sh" }.into(),
                        run: "echo followup".into(),
                        timeout_seconds: 30,
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        let finished = first.event("finished").await;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&finished.payload_json).unwrap()["status"],
            "succeeded"
        );
    }
    machine.act("stop");
    first.closed().await;
    machine.act("uninstall");
    machine.installed = false;
    assert!(!machine.state().0);
}
