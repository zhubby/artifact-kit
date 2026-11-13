fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::compile_protos("../../proto/artifactory/runtime/v1/runtime.proto")?;
    println!("cargo:rerun-if-changed=../../proto/artifactory/runtime/v1/runtime.proto");
    Ok(())
}
