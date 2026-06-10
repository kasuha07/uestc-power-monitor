use crate::{Result, UestcClientError};
use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockEncrypt, KeyInit};
use aes::{Aes128, Aes192, Aes256};
use rand::Rng;

const AES_CHARS: &[u8] = b"ABCDEFGHJKMNPQRSTWXYZabcdefhijkmnprstwxyz2345678";

fn random_string(len: usize) -> String {
    let mut rng = rand::rng();
    (0..len)
        .map(|_| {
            let idx = rng.random_range(0..AES_CHARS.len());
            AES_CHARS[idx] as char
        })
        .collect()
}

pub fn encrypt_password(password: &str, pwd_encrypt_salt: &str) -> Result<String> {
    log::debug!(
        "Starting password encryption (password length: {}, salt length: {})",
        password.len(),
        pwd_encrypt_salt.len()
    );

    let salt = pwd_encrypt_salt.trim();
    let key = salt.as_bytes();

    let iv_str = random_string(16);
    let iv = iv_str.as_bytes();

    let prefix = random_string(64);
    let plaintext = format!("{}{}", prefix, password);
    let plaintext_bytes = plaintext.as_bytes();

    // PKCS7 Padding
    let padding_len = 16 - (plaintext_bytes.len() % 16);
    let mut padded_input = plaintext_bytes.to_vec();
    padded_input.extend(std::iter::repeat(padding_len as u8).take(padding_len));

    let mut ciphertext = Vec::with_capacity(padded_input.len());
    let mut current_iv = GenericArray::clone_from_slice(iv);

    match key.len() {
        16 => {
            log::debug!("Using AES-128 encryption");
            let cipher =
                Aes128::new_from_slice(key).map_err(|e| UestcClientError::CryptoError {
                    message: format!("Failed to create AES-128 cipher: {}", e),
                    key_length: Some(key.len()),
                    source: None,
                })?;
            for chunk in padded_input.chunks(16) {
                let mut block = GenericArray::clone_from_slice(chunk);
                for (b, v) in block.iter_mut().zip(current_iv.iter()) {
                    *b ^= *v;
                }
                cipher.encrypt_block(&mut block);
                ciphertext.extend_from_slice(&block);
                current_iv = block;
            }
        }
        24 => {
            log::debug!("Using AES-192 encryption");
            let cipher =
                Aes192::new_from_slice(key).map_err(|e| UestcClientError::CryptoError {
                    message: format!("Failed to create AES-192 cipher: {}", e),
                    key_length: Some(key.len()),
                    source: None,
                })?;
            for chunk in padded_input.chunks(16) {
                let mut block = GenericArray::clone_from_slice(chunk);
                for (b, v) in block.iter_mut().zip(current_iv.iter()) {
                    *b ^= *v;
                }
                cipher.encrypt_block(&mut block);
                ciphertext.extend_from_slice(&block);
                current_iv = block;
            }
        }
        32 => {
            log::debug!("Using AES-256 encryption");
            let cipher =
                Aes256::new_from_slice(key).map_err(|e| UestcClientError::CryptoError {
                    message: format!("Failed to create AES-256 cipher: {}", e),
                    key_length: Some(key.len()),
                    source: None,
                })?;
            for chunk in padded_input.chunks(16) {
                let mut block = GenericArray::clone_from_slice(chunk);
                for (b, v) in block.iter_mut().zip(current_iv.iter()) {
                    *b ^= *v;
                }
                cipher.encrypt_block(&mut block);
                ciphertext.extend_from_slice(&block);
                current_iv = block;
            }
        }
        _ => {
            log::warn!("Invalid key length for AES encryption: {} bytes", key.len());
            return Err(UestcClientError::CryptoError {
                message: format!(
                    "Invalid key length: {} bytes (expected 16, 24, or 32)",
                    key.len()
                ),
                key_length: Some(key.len()),
                source: None,
            });
        }
    }

    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(ciphertext);
    log::debug!(
        "Password encryption successful (output length: {} bytes)",
        encoded.len()
    );
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn test_encrypt_password_aes128() {
        let password = "password123";
        // 16 bytes key
        let salt = "1234567890123456";
        let result = encrypt_password(password, salt);
        assert!(result.is_ok());

        let encrypted = result.unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&encrypted)
            .expect("Should decode base64");

        // Output length should be a multiple of 16 (block size)
        assert_eq!(decoded.len() % 16, 0);
        // Length check: 64 (prefix) + 11 (password) + padding.
        // 75 bytes -> padding 5 bytes -> 80 bytes total.
        assert_eq!(decoded.len(), 80);
    }

    #[test]
    fn test_encrypt_password_aes192() {
        let password = "password123";
        // 24 bytes key
        let salt = "123456789012345678901234";
        let result = encrypt_password(password, salt);
        assert!(result.is_ok());
    }

    #[test]
    fn test_encrypt_password_aes256() {
        let password = "password123";
        // 32 bytes key
        let salt = "12345678901234567890123456789012";
        let result = encrypt_password(password, salt);
        assert!(result.is_ok());
    }

    #[test]
    fn test_encrypt_password_invalid_key_length() {
        let password = "password123";
        // Invalid length (not 16, 24, or 32)
        let salt = "short";
        let result = encrypt_password(password, salt);
        assert!(result.is_err());

        match result {
            Err(UestcClientError::CryptoError {
                message,
                key_length,
                ..
            }) => {
                assert!(message.contains("Invalid key length"));
                assert_eq!(key_length, Some(5));
            }
            _ => panic!("Expected CryptoError"),
        }
    }

    #[test]
    fn test_randomness() {
        let password = "password123";
        let salt = "1234567890123456";

        let result1 = encrypt_password(password, salt).unwrap();
        let result2 = encrypt_password(password, salt).unwrap();

        // Should be different because of random IV and prefix
        assert_ne!(result1, result2);
    }
}
