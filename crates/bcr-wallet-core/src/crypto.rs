use bitcoin::secp256k1::{PublicKey, SecretKey};
use thiserror::Error;

#[derive(Debug, Error, Eq, PartialEq)]
pub enum EncryptionError {
    #[error("Ecies encryption error {0}")]
    Ecies(String),
}

/// Encrypt the given bytes with the given Secp256k1 key via ECIES
pub fn encrypt_ecies(bytes: &[u8], public_key: &PublicKey) -> Result<Vec<u8>, EncryptionError> {
    let pub_key_bytes = public_key.serialize();
    let encrypted = ecies::encrypt(pub_key_bytes.as_slice(), bytes)
        .map_err(|e| EncryptionError::Ecies(e.to_string()))?;
    Ok(encrypted)
}

/// Decrypt the given bytes with the given Secp256k1 key via ECIES
pub fn decrypt_ecies(bytes: &[u8], private_key: &SecretKey) -> Result<Vec<u8>, EncryptionError> {
    let decrypted = ecies::decrypt(private_key.secret_bytes().as_slice(), bytes)
        .map_err(|e| EncryptionError::Ecies(e.to_string()))?;
    Ok(decrypted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_ecies_basic() {
        let msg = "Hello, this is a very important message!"
            .to_string()
            .into_bytes();
        let keypair =
            bitcoin::secp256k1::Keypair::new_global(&mut bitcoin::secp256k1::rand::thread_rng());

        let encrypted = encrypt_ecies(&msg, &keypair.public_key());
        assert!(encrypted.is_ok());
        let decrypted = decrypt_ecies(encrypted.as_ref().unwrap(), &keypair.secret_key());
        assert!(decrypted.is_ok());

        assert_eq!(&msg, decrypted.as_ref().unwrap());
    }
}
