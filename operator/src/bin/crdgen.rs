fn main() -> anyhow::Result<()> {
    for crd in artifactory_operator::api::crds() {
        print!("---\n{}", serde_yaml::to_string(&crd)?);
    }
    Ok(())
}
