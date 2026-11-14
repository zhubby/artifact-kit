use crate::{crypto, recipe::Recipe};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub path: String,
    pub reason: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalScript {
    pub name: String,
    pub content: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPlan {
    pub summary: String,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    #[serde(default)]
    pub scripts: Vec<ExternalScript>,
    pub recipe: Recipe,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub placements:
        std::collections::BTreeMap<String, artifactory_telemetry_model::nodes::Placement>,
}

impl ExecutionPlan {
    pub fn parse(value: serde_json::Value) -> Result<Self> {
        ensure!(
            value.to_string().len() <= 384 * 1024,
            "plan exceeds 384 KiB"
        );
        let mut plan: Self = serde_json::from_value(value)?;
        plan.recipe = Recipe::parse(&serde_yaml::to_string(&plan.recipe)?)?;
        ensure!(
            plan.placements
                .keys()
                .all(|key| plan.recipe.targets.contains_key(key)),
            "placement references unknown target"
        );
        for placement in plan.placements.values() {
            placement.validate()?;
        }
        ensure!(
            !plan.summary.trim().is_empty() && plan.summary.len() <= 8000,
            "invalid plan summary"
        );
        ensure!(
            !plan.evidence.is_empty() && plan.evidence.len() <= 64 && plan.scripts.len() <= 32,
            "too many plan resources"
        );
        for item in &plan.evidence {
            validate_path(&item.path)?;
            ensure!(
                !item.reason.is_empty() && item.reason.len() <= 2000,
                "invalid evidence"
            );
        }
        let mut names = std::collections::HashSet::new();
        let mut total = 0;
        for script in &plan.scripts {
            ensure!(
                valid_script_name(&script.name) && names.insert(&script.name),
                "invalid or duplicate script name"
            );
            total += script.content.len();
            ensure!(
                !script.content.is_empty() && !script.content.contains('\0') && total <= 256 * 1024,
                "invalid external script"
            );
        }
        Ok(plan)
    }

    pub fn scripts(&self) -> Vec<crate::protocol::ExternalScript> {
        self.scripts
            .iter()
            .map(|item| crate::protocol::ExternalScript {
                name: item.name.clone(),
                content: item.content.clone(),
                sha256: crypto::hash(&item.content),
            })
            .collect()
    }
}

pub fn valid_script_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 120
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}

pub fn validate_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && path.len() <= 2048
            && !path.starts_with('/')
            && !path.contains(['\\', '\0', '\n', '\r', ':'])
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "invalid source path"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn external_scripts_cannot_escape_task_directory() {
        for name in ["../x", "/x", "a/b", "a\\b", "..", ".env", "x:y"] {
            assert!(!valid_script_name(name));
        }
        assert!(valid_script_name("build.ps1"));
        for path in ["../key", "a/../../x", "/etc/passwd", "a\\b", "C:/x"] {
            assert!(validate_path(path).is_err());
        }
    }
}
