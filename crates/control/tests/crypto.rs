use artifactory_control::crypto::{open, seal};

#[test]
fn authenticated_encryption_rejects_tampering_and_wrong_keys() {
    let key = [7_u8; 32];
    let encrypted = seal(&key, b"private deploy key").unwrap();
    assert_eq!(open(&key, &encrypted).unwrap(), b"private deploy key");
    assert!(open(&[8_u8; 32], &encrypted).is_err());
    let mut altered = encrypted.into_bytes();
    altered[20] = if altered[20] == b'A' { b'B' } else { b'A' };
    assert!(open(&key, std::str::from_utf8(&altered).unwrap()).is_err());
}
