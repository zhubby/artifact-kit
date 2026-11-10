use artifact_control::recipe::Recipe;

const VALID: &str = r#"
version: 1
targets:
  linux:
    os: linux
    arch: amd64
    steps:
      - name: build
        run: mkdir -p dist && printf hello > dist/app
    artifacts: [dist/app]
"#;

#[test]
fn parses_native_recipe() {
    let recipe = Recipe::parse(VALID).unwrap();
    assert_eq!(recipe.targets["linux"].executor, "native");
    assert_eq!(recipe.targets["linux"].timeout_seconds, 7200);
}

#[test]
fn rejects_unknown_fields_and_unsupported_execution() {
    assert!(
        Recipe::parse(&VALID.replace("version: 1", "version: 1\nworkflow: arbitrary")).is_err()
    );
    assert!(
        Recipe::parse(&VALID.replace("os: linux", "executor: container\n    os: linux")).is_err()
    );
}

#[test]
fn rejects_artifact_escape_and_empty_steps() {
    for path in [
        "../private",
        "/etc/passwd",
        "C:\\keys\\secret",
        "dist/../../key",
    ] {
        let recipe = VALID.replace("[dist/app]", &format!("['{path}']"));
        assert!(Recipe::parse(&recipe).is_err(), "accepted {path}");
    }
    assert!(
        Recipe::parse(&VALID.replace(
            "- name: build\n        run: mkdir -p dist && printf hello > dist/app",
            "[]"
        ))
        .is_err()
    );
}

#[test]
fn signing_requires_a_verification_stage() {
    let recipe = VALID.replace(
        "artifacts: [dist/app]",
        "signing_profiles: [apple]\n    artifacts: [dist/app]",
    );
    assert!(Recipe::parse(&recipe).is_err());
}

#[test]
fn validates_parameters_without_evaluating_expressions() {
    let recipe = Recipe::parse(VALID).unwrap();
    assert!(
        recipe
            .environment(&serde_json::json!({"UNDECLARED": "oops"}))
            .is_err()
    );
}

#[test]
fn all_platform_examples_obey_the_real_parser() {
    for input in [
        include_str!("../../../examples/linux-rust.yaml"),
        include_str!("../../../examples/macos-xcode.yaml"),
        include_str!("../../../examples/windows-rust.yaml"),
        include_str!("../../../examples/android-gradle.yaml"),
        include_str!("../../../examples/ios-xcode.yaml"),
    ] {
        Recipe::parse(input).unwrap();
    }
}
