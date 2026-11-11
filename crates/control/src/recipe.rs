use std::collections::BTreeMap;

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub version: u32,
    #[serde(default)]
    pub parameters: BTreeMap<String, String>,
    pub targets: BTreeMap<String, Target>,
    #[serde(default)]
    pub release: ReleaseRule,
}

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ReleaseRule {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub os: String,
    pub arch: String,
    #[serde(default = "native")]
    pub executor: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub signing_profiles: Vec<String>,
    #[serde(default = "job_timeout")]
    pub timeout_seconds: u32,
    pub steps: Vec<Step>,
    pub artifacts: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub name: String,
    #[serde(default = "build_phase")]
    pub phase: String,
    #[serde(default)]
    pub shell: Option<String>,
    pub run: String,
    #[serde(default = "step_timeout")]
    pub timeout_seconds: u32,
}

fn native() -> String {
    "native".into()
}
fn build_phase() -> String {
    "build".into()
}
fn job_timeout() -> u32 {
    7200
}
fn step_timeout() -> u32 {
    3600
}

pub fn safe_relative_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && !path.starts_with(['/', '\\'])
        && !path.contains([':', '\0', '\\'])
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != ".." && part != ".")
}

impl Recipe {
    pub fn parse(input: &str) -> Result<Self> {
        ensure!(input.len() <= 256 * 1024, "configuration exceeds 256 KiB");
        let recipe: Self = serde_yaml::from_str(input)?;
        ensure!(recipe.version == 1, "unsupported configuration version");
        ensure!(
            !recipe.targets.is_empty() && recipe.targets.len() <= 32,
            "expected 1..32 targets"
        );
        for (name, target) in &recipe.targets {
            ensure!(!name.is_empty() && name.len() <= 80, "invalid target name");
            ensure!(
                target.executor == "native",
                "container executor is not implemented"
            );
            ensure!(
                ["linux", "darwin", "windows"].contains(&target.os.as_str()),
                "invalid host OS"
            );
            ensure!(
                ["amd64", "arm64"].contains(&target.arch.as_str()),
                "invalid host architecture"
            );
            ensure!(
                !target.steps.is_empty() && target.steps.len() <= 64,
                "expected 1..64 steps"
            );
            ensure!(
                target.timeout_seconds > 0 && target.timeout_seconds <= 86400,
                "invalid job timeout"
            );
            ensure!(
                !target.artifacts.is_empty() && target.artifacts.len() <= 32,
                "expected artifact patterns"
            );
            ensure!(
                target.artifacts.iter().all(|path| safe_relative_path(path)),
                "unsafe artifact pattern"
            );
            let mut previous_phase = 0;
            let mut phases = Vec::new();
            for step in &target.steps {
                ensure!(
                    !step.name.is_empty() && step.name.len() <= 120,
                    "invalid step name"
                );
                ensure!(
                    !step.run.trim().is_empty() && step.run.len() <= 65536,
                    "invalid command"
                );
                ensure!(
                    step.timeout_seconds > 0 && step.timeout_seconds <= target.timeout_seconds,
                    "invalid step timeout"
                );
                if let Some(shell) = &step.shell {
                    ensure!(
                        ["bash", "sh", "pwsh", "powershell"].contains(&shell.as_str()),
                        "unsupported shell"
                    );
                }
                let phase = match step.phase.as_str() {
                    "build" => 0,
                    "sign" => 1,
                    "verify" => 2,
                    _ => bail!("unsupported step phase"),
                };
                ensure!(
                    phase >= previous_phase,
                    "stages must be ordered build, sign, verify"
                );
                previous_phase = phase;
                phases.push(phase);
            }
            if !target.signing_profiles.is_empty() || phases.contains(&1) {
                ensure!(
                    !target.signing_profiles.is_empty()
                        && phases.contains(&1)
                        && phases.contains(&2),
                    "signing requires authorized profiles, sign and verify stages"
                );
            }
        }
        for (key, value) in &recipe.parameters {
            ensure!(valid_parameter(key), "invalid parameter name");
            ensure!(
                value.len() <= 4096 && !value.contains('\0'),
                "invalid parameter default"
            );
        }
        Ok(recipe)
    }

    pub fn environment(&self, parameters: &Value) -> Result<BTreeMap<String, String>> {
        let supplied = parameters
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("parameters must be an object"))?;
        let mut environment = self.parameters.clone();
        for (name, value) in supplied {
            ensure!(
                environment.contains_key(name),
                "undeclared parameter {name}"
            );
            let value = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("parameter values must be strings"))?;
            ensure!(
                value.len() <= 4096 && !value.contains('\0'),
                "invalid parameter value"
            );
            environment.insert(name.clone(), value.into());
        }
        Ok(environment
            .into_iter()
            .map(|(name, value)| (format!("ARTIFACTORY_PARAM_{name}"), value))
            .collect())
    }
}

fn valid_parameter(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|value| value.is_ascii_uppercase() || value.is_ascii_digit() || value == b'_')
        && name.as_bytes()[0].is_ascii_uppercase()
}
