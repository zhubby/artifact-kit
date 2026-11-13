use crate::{COMMIT, VERSION, config::Config, platform, process};
use serde_json::{Value, json};
use std::{collections::BTreeSet, time::Duration};
use tokio::process::Command;

pub async fn doctor(cfg: &Config) -> (Value, Vec<String>) {
    let mut names: BTreeSet<String> = [
        "git",
        "git-lfs",
        "bash",
        "sh",
        "powershell",
        "pwsh",
        "cc",
        "cargo",
        "go",
        "node",
        "npm",
        "xcodebuild",
        "java",
        "gradle",
        "cmake",
        "ninja",
        "dotnet",
        "msbuild",
        "signtool",
        "codesign",
        "security",
        "apksigner",
        "docker",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    names.extend(cfg.tools.keys().cloned());
    let mut tools = serde_json::Map::new();
    let mut capabilities = vec!["source-cache-v1".into(), "external-build-plan-v1".into()];
    for name in names {
        let binary = cfg.tool(&name);
        let Ok(Ok(path)) = tokio::task::spawn_blocking(move || which::which(binary)).await else {
            tools.insert(name, json!({"available":false}));
            continue;
        };
        let args = match name.as_str() {
            // POSIX sh need not accept --version (for example, Debian's dash).
            "sh" => vec!["-c", "exit 0"],
            "go" | "apksigner" => vec!["version"],
            "xcodebuild" | "java" | "msbuild" => vec!["-version"],
            "codesign" => vec!["--help"],
            "security" => vec!["help"],
            "signtool" => vec!["/?"],
            "powershell" | "pwsh" => vec![
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$PSVersionTable.PSVersion.ToString()",
            ],
            _ => vec!["--version"],
        };
        let mut command = Command::new(&path);
        command.args(&args);
        // Some tools print their version on stderr. Merge through separate bounded
        // pipes rather than invoking a shell with paths supplied by configuration.
        let result = process::probe(command, Duration::from_secs(3)).await;
        let (success, code, output) = match result {
            Ok((status, output)) => (status.success(), status.code(), output),
            Err(_) => (false, None, Vec::new()),
        };
        let output = String::from_utf8_lossy(&output);
        let version = output.trim();
        let version = version[..version.floor_char_boundary(version.len().min(512))].to_owned();
        let working = success
            || (name == "codesign" && code == Some(2) && version.starts_with("Usage: codesign"));
        tools.insert(name.clone(), json!({"available":working,"path":path,"version":version,"version_check_succeeded":success,"probe_succeeded":working,"probe_arguments":args}));
        if working {
            capabilities.push(name);
        }
    }
    capabilities.sort();
    let (os, arch) = platform();
    (
        json!({"emissary_version":VERSION,"emissary_commit":COMMIT,"protocol_major":1,"hostname":sysinfo::System::host_name().unwrap_or_default(),"os":os,"arch":arch,"tools":tools,"configured_signing_profiles":cfg.signing_profiles.keys().collect::<Vec<_>>()}),
        capabilities,
    )
}
pub fn metrics(cfg: &Config) -> Value {
    let mut system = sysinfo::System::new();
    let pid = sysinfo::Pid::from_u32(std::process::id());
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    json!({"logical_cpus":std::thread::available_parallelism().ok().map(|n| n.get()),"emissary_rss_bytes":system.process(pid).map(|p|p.memory()),"managed_free_bytes":fs2::available_space(&cfg.root).ok(),"managed_root":cfg.root})
}
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn advertises_posix_sh_without_a_version_option() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("posix-sh");
        crate::config::private_write(
            &path,
            b"#!/bin/sh\nif [ \"$1\" = --version ]; then exit 2; fi\nexec /bin/sh \"$@\"\n",
            true,
            true,
        )
        .unwrap();
        let mut cfg = Config::default();
        cfg.tools.insert("sh".into(), path.to_string_lossy().into());

        let (diagnostics, capabilities) = doctor(&cfg).await;

        assert!(capabilities.iter().any(|name| name == "sh"));
        assert_eq!(diagnostics["tools"]["sh"]["available"], true);
    }

    #[tokio::test]
    async fn only_advertises_working_toolchains() {
        let directory = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        for (name, contents) in [
            ("working", "#!/bin/sh\necho v1\n"),
            ("broken", "#!/bin/sh\nexit 1\n"),
            ("hung", "#!/bin/sh\nsleep 30\n"),
        ] {
            let path = directory.path().join(name);
            crate::config::private_write(&path, contents.as_bytes(), true, true).unwrap();
            cfg.tools.insert(name.into(), path.to_string_lossy().into());
        }
        let (_, capabilities) = doctor(&cfg).await;
        assert!(capabilities.iter().any(|s| s == "working"));
        assert!(!capabilities.iter().any(|s| s == "broken" || s == "hung"));
    }
}
