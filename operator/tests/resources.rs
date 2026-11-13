use artifactory_operator::{api::*, controller::job};
use kube::ResourceExt;
#[test]
fn jobs_have_single_attempt_and_bound_pod_identity_without_provider_secrets() {
    let mut run = AgentRun::new(
        "run-a",
        AgentRunSpec {
            run_id: "00000000-0000-0000-0000-000000000001".into(),
            generation: 2,
            pool: "default".into(),
            snapshot_id: "snapshot".into(),
            snapshot_digest: "a".repeat(64),
        },
    );
    run.metadata.uid = Some("cr-uid".into());
    run.metadata.namespace = Some("test".into());
    let pool = AgentPool::new(
        "default",
        AgentPoolSpec {
            max_active: 8,
            max_per_project: 2,
            image: "pi@sha256:fixed".into(),
            image_pull_policy: "IfNotPresent".into(),
            runtime_url: "https://runtime".into(),
            source_pool: "source".into(),
            cpu: "250m".into(),
            memory: "512Mi".into(),
            tls_secret_name: String::new(),
        },
    );
    let first = job(&run, &pool).unwrap();
    let second = job(&run, &pool).unwrap();
    assert_eq!(first, second);
    assert!(first.name_any().ends_with("-g2"));
    let value = serde_json::to_value(first).unwrap();
    assert_eq!(value["spec"]["backoffLimit"], 0);
    assert_eq!(
        value["spec"]["template"]["spec"]["automountServiceAccountToken"],
        false
    );
    assert_eq!(value["metadata"]["ownerReferences"][0]["uid"], "cr-uid");
    assert!(!value.to_string().contains("api_key"));
    assert!(value.to_string().contains("artifactory-runtime"));
}
#[test]
fn immutable_crd_context_is_enforced_by_the_api_server_schema() {
    let definitions = crds();
    assert_eq!(definitions.len(), 4);
    for definition in definitions {
        let value = serde_json::to_value(definition).unwrap();
        let kind = value["spec"]["names"]["kind"].as_str().unwrap();
        if kind == "AgentRun" || kind == "SourceSnapshot" {
            assert_eq!(
                value["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]["x-kubernetes-validations"]
                    [0]["rule"],
                "self == oldSelf"
            );
        }
    }
}

#[test]
fn source_download_addresses_are_shard_specific_and_separate_from_internal_rpc() {
    let mut spec: SourcePoolSpec = serde_json::from_value(serde_json::json!({
        "shards": 2, "image": "source:fixed", "runtimeUrl": "https://runtime",
        "controlUrl": "https://control", "secretName": "source",
        "storageClass": "standard", "storageSize": "1Gi", "tlsSecretName": "tls"
    }))
    .unwrap();
    assert_eq!(
        spec.download_url("sources", "builds").unwrap(),
        "https://$(POD_NAME).sources.builds.svc:8090"
    );
    spec.public_url_template = Some("https://{pod}.source.example.com/{namespace}/".into());
    assert_eq!(
        spec.download_url("sources", "builds").unwrap(),
        "https://$(POD_NAME).source.example.com/builds"
    );
    for invalid in [
        "https://user:secret@example.com",
        "https://example.com/?token=secret",
        "https://example.com/#fragment",
        "https://{unknown}.example.com",
        "https://$(SECRET).example.com",
        "https://example.com/a\nb",
    ] {
        spec.public_url_template = Some(invalid.into());
        assert!(
            spec.download_url("sources", "builds").is_err(),
            "accepted {invalid}"
        );
    }
}
