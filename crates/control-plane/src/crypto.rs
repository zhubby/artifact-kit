use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use rand::RngCore;
use sha2::{Digest, Sha256};

pub fn hash(value: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(value.as_ref()))
}

pub fn token() -> String {
    let mut bytes = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub fn seal(key: &[u8; 32], value: &[u8]) -> Result<String> {
    let mut nonce = [0_u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce);
    let encrypted = Aes256Gcm::new_from_slice(key)
        .unwrap()
        .encrypt(Nonce::from_slice(&nonce), value)
        .map_err(|_| anyhow::anyhow!("credential encryption failed"))?;
    Ok(STANDARD.encode([nonce.to_vec(), encrypted].concat()))
}

pub fn open(key: &[u8; 32], value: &str) -> Result<Vec<u8>> {
    let bytes = STANDARD.decode(value)?;
    ensure!(bytes.len() >= 28, "invalid encrypted credential");
    Aes256Gcm::new_from_slice(key)
        .unwrap()
        .decrypt(Nonce::from_slice(&bytes[..12]), &bytes[12..])
        .map_err(|_| anyhow::anyhow!("credential authentication failed"))
}
