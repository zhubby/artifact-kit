use anyhow::{Context, Result, ensure};
use artifactory_emissary::ota::{self, Asset, Manifest};
use base64::{Engine, engine::general_purpose::STANDARD};
use clap::Parser;
use ed25519_dalek::{Signer, SigningKey};
use std::{fs, path::PathBuf};
#[derive(Parser)]
struct Args {
    #[arg(long)]
    directory: PathBuf,
    #[arg(long)]
    version: String,
    #[arg(long)]
    commit: String,
}
fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        ota::safe_version(&args.version) && args.commit.len() == 40,
        "invalid release metadata"
    );
    let secret = std::env::var("ARTIFACTORY_EMISSARY_RELEASE_SIGNING_SEED")
        .context("release signing seed environment variable required")?;
    let bytes = STANDARD.decode(secret)?;
    let seed: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("release seed must be 32 bytes"))?;
    let key = SigningKey::from_bytes(&seed);
    let mut assets = Vec::new();
    for (os, arch) in [
        ("linux", "amd64"),
        ("linux", "arm64"),
        ("darwin", "amd64"),
        ("darwin", "arm64"),
        ("windows", "amd64"),
        ("windows", "arm64"),
    ] {
        let bytes = fs::read(args.directory.join(ota::asset_name(os, arch)))?;
        let asset = Asset::from_bytes(os, arch, &bytes);
        ensure!(
            asset.size > 0 && asset.size <= ota::MAX_BINARY_SIZE,
            "invalid release size"
        );
        assets.push(asset);
    }
    let manifest = Manifest {
        version: args.version,
        commit: args.commit,
        protocol_min: 1,
        protocol_max: 1,
        assets,
    };
    let raw = serde_json::to_vec_pretty(&manifest)?;
    fs::write(args.directory.join("emissary-manifest.json"), &raw)?;
    fs::write(
        args.directory.join("emissary-manifest.sig"),
        STANDARD.encode(key.sign(&raw).to_bytes()),
    )?;
    println!("signed Emissary manifest for six platforms");
    Ok(())
}
