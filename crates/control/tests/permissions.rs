use artifact_control::auth::Auth;
use uuid::Uuid;

#[test]
fn token_scope_cannot_escape_project_or_escalate_role() {
    let project = Uuid::new_v4();
    let actor = Auth::scoped(
        Uuid::new_v4(),
        "operator",
        vec![project],
        vec!["read".into(), "build".into()],
    );
    assert!(actor.project(project, "build").is_ok());
    assert!(actor.project(Uuid::new_v4(), "read").is_err());
    assert!(actor.project(project, "release").is_err());
    assert!(actor.session_role("viewer").is_err());
    let viewer = Auth::scoped(
        Uuid::new_v4(),
        "viewer",
        vec![project],
        vec!["build".into()],
    );
    assert!(viewer.project(project, "build").is_err());
}
