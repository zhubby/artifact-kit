use artifactory_emissary::process;
use std::{fs, time::Duration};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn process_tree_terminates_descendants() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("descendant.pid");
    #[cfg(unix)]
    let mut command = {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "sh -c 'sleep 120 & wait' & echo $! > \"$MARKER\"; wait",
        ]);
        command
    };
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("pwsh");
        command.args(["-NoProfile","-Command","$p=Start-Process pwsh -ArgumentList '-NoProfile','-Command','Start-Sleep 120' -PassThru; [IO.File]::WriteAllText($env:MARKER,$p.Id.ToString()); Wait-Process -Id $p.Id"]);
        command
    };
    command.env("MARKER", &marker);
    let mut child = process::spawn(command).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    let pid = fs::read_to_string(&marker)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        process::wait(&mut *child, &cancel, Duration::from_secs(1))
            .await
            .is_err()
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut system = sysinfo::System::new();
            system.refresh_processes(
                sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
                true,
            );
            if system
                .process(sysinfo::Pid::from_u32(pid))
                .is_none_or(|p| p.status() == sysinfo::ProcessStatus::Zombie)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn worker_kills_process_tree_on_parent_death_and_lease_expiry() {
    use artifactory_emissary::{
        config::Config,
        protocol::{Assignment, Step},
        worker::{Event, Input},
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    for cause in ["parent-death", "lease-expiry", "sigterm"] {
        let directory = tempfile::tempdir().unwrap();
        let repo = directory.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@localhost",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        git(&["init"]);
        fs::write(repo.join("README"), "fixture").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "fixture"]);
        let commit = git(&["rev-parse", "HEAD"]);
        let id = uuid::Uuid::new_v4().to_string();
        let root = directory.path().join("managed");
        let input = Input {
            kind: "start".into(),
            config: Config {
                root: root.clone(),
                ..Default::default()
            },
            attempt_id: id.clone(),
            lease_token: "test".into(),
            lease_seconds: if cause == "lease-expiry" { 2 } else { 30 },
            assignment: Some(Assignment {
                repository_url: repo.to_string_lossy().into(),
                commit_sha: commit,
                timeout_seconds: 60,
                steps: vec![Step {
                    name: "long".into(),
                    shell: "sh".into(),
                    run: "sh -c 'sleep 120 & wait' & echo $! > child.pid; wait".into(),
                    timeout_seconds: 60,
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut child = Command::new(env!("CARGO_BIN_EXE_artifactory-emissary"))
            .arg("worker")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut line = serde_json::to_vec(&input).unwrap();
        line.push(b'\n');
        stdin.write_all(&line).await.unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let drain = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(line) = lines.next_line().await.unwrap() {
                events.push(serde_json::from_str::<Event>(&line).unwrap());
            }
            events
        });
        let marker = root.join("work").join(&id).join("source/child.pid");
        tokio::time::timeout(Duration::from_secs(10), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        let pid = fs::read_to_string(marker)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        if cause == "parent-death" {
            drop(stdin);
        } else if cause == "sigterm" {
            assert!(
                std::process::Command::new("kill")
                    .args(["-TERM", &child.id().unwrap().to_string()])
                    .status()
                    .unwrap()
                    .success()
            );
        }
        tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap();
        let events = drain.await.unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.kind == "finished" && e.payload["status"] == "failed"),
            "{cause}"
        );
        assert!(
            !root.join("work").join(&id).exists(),
            "workspace remains: {cause}"
        );
        let mut system = sysinfo::System::new();
        system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        assert!(
            system
                .process(sysinfo::Pid::from_u32(pid))
                .is_none_or(|p| p.status() == sysinfo::ProcessStatus::Zombie),
            "descendant remains: {cause}"
        );
    }
}

#[test]
fn service_run_exits_when_emissary_cannot_start() {
    let directory = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_artifactory-emissary"))
        .args(["run", "--config"])
        .arg(directory.path().join("missing.json"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    #[cfg(unix)]
    {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_artifactory-emissary"))
            .args(["service-run", "--config"])
            .arg(directory.path().join("missing.json"))
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
}
