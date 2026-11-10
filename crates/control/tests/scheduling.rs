use artifact_control::{recipe::Recipe, scheduler::matches};
use serde_json::json;

#[test]
fn requires_real_capabilities_and_authorized_signing_bindings() {
    let recipe=Recipe::parse("version: 1\ntargets:\n  app:\n    os: darwin\n    arch: arm64\n    capabilities: [xcodebuild]\n    labels: {purpose: signing}\n    signing_profiles: [apple]\n    steps:\n      - {name: build, run: make}\n      - {name: sign, phase: sign, run: sign}\n      - {name: verify, phase: verify, run: verify}\n    artifacts: [dist/app.zip]\n").unwrap();
    let target = &recipe.targets["app"];
    let labels = json!({"purpose":"signing"});
    let capabilities = json!(["git", "bash", "xcodebuild"]);
    assert!(matches(
        target,
        "darwin",
        "arm64",
        &labels,
        &capabilities,
        &["apple".into()]
    ));
    assert!(!matches(
        target,
        "darwin",
        "arm64",
        &labels,
        &capabilities,
        &[]
    ));
    assert!(!matches(
        target,
        "linux",
        "arm64",
        &labels,
        &capabilities,
        &["apple".into()]
    ));
    assert!(!matches(
        target,
        "darwin",
        "arm64",
        &labels,
        &json!(["git", "bash"]),
        &["apple".into()]
    ));
}
