fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .build_transport(false)
        .type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]")
        .compile_protos(
            &["../../proto/artifactory/emissary/v1/emissary.proto"],
            &["../../proto"],
        )?;
    println!("cargo:rerun-if-changed=../../proto/artifactory/emissary/v1/emissary.proto");
    Ok(())
}
