fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .build_client(false)
        .compile_protos(
            &["../../proto/artifactkit/agent/v1/agent.proto"],
            &["../../proto"],
        )?;
    println!("cargo:rerun-if-changed=../../proto/artifactkit/agent/v1/agent.proto");
    Ok(())
}
