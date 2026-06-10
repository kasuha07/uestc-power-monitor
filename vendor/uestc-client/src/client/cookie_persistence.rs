use crate::{Result, UestcClientError};
use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use cookie_store::CookieStore;
use pbkdf2::pbkdf2_hmac;
use reqwest_cookie_store::CookieStoreMutex;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;

const COOKIE_FILE_VERSION: u8 = 1;
const COOKIE_AAD: &[u8] = b"uestc-client:encrypted-cookies:v1";
const CIPHER_NAME: &str = "AES-256-GCM";
const KDF_NAME: &str = "PBKDF2-HMAC-SHA256";
const KDF_ITERATIONS: u32 = 210_000;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

#[derive(Serialize, Deserialize, Debug)]
struct SerializableCookie {
    name: String,
    value: String,
    domain: String,
    path: String,
    expires: Option<i64>,
    secure: bool,
    http_only: bool,
}

#[derive(Serialize, Deserialize, Debug)]
struct EncryptedCookieFile {
    version: u8,
    cipher: String,
    kdf: String,
    iterations: u32,
    salt: String,
    nonce: String,
    ciphertext: String,
}

pub fn load_encrypted_cookie_store(
    path: &Path,
    encryption_secret: &[u8],
) -> Result<Arc<CookieStoreMutex>> {
    if encryption_secret.is_empty() {
        return Err(cookie_error(
            "decrypt",
            path,
            "Cookie encryption secret is empty",
            None,
        ));
    }

    let encrypted_json = fs::read_to_string(path).map_err(|e| {
        cookie_error(
            "read",
            path,
            format!("Failed to read encrypted cookie file: {}", e),
            Some(Box::new(e)),
        )
    })?;

    let encrypted: EncryptedCookieFile = serde_json::from_str(&encrypted_json).map_err(|e| {
        cookie_error(
            "deserialize_envelope",
            path,
            format!("Failed to deserialize encrypted cookie envelope: {}", e),
            Some(Box::new(e)),
        )
    })?;

    validate_envelope(path, &encrypted)?;

    let salt = decode_b64(path, "salt", &encrypted.salt)?;
    let nonce = decode_b64(path, "nonce", &encrypted.nonce)?;
    let ciphertext = decode_b64(path, "ciphertext", &encrypted.ciphertext)?;

    if salt.len() != SALT_LEN {
        return Err(cookie_error(
            "decrypt",
            path,
            format!("Invalid cookie salt length: {}", salt.len()),
            None,
        ));
    }
    if nonce.len() != NONCE_LEN {
        return Err(cookie_error(
            "decrypt",
            path,
            format!("Invalid cookie nonce length: {}", nonce.len()),
            None,
        ));
    }

    let key = derive_key(encryption_secret, &salt, encrypted.iterations);
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| {
        cookie_error(
            "decrypt",
            path,
            "Failed to initialize cookie decryptor",
            None,
        )
    })?;
    let plaintext = cipher
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &ciphertext,
                aad: COOKIE_AAD,
            },
        )
        .map_err(|_| {
            cookie_error(
                "decrypt",
                path,
                "Failed to decrypt cookies; encryption key or file contents are invalid",
                None,
            )
        })?;

    let cookies: Vec<SerializableCookie> = serde_json::from_slice(&plaintext).map_err(|e| {
        cookie_error(
            "deserialize",
            path,
            format!("Failed to deserialize decrypted cookies: {}", e),
            Some(Box::new(e)),
        )
    })?;

    Ok(Arc::new(CookieStoreMutex::new(store_from_cookies(cookies))))
}

pub fn save_encrypted_cookie_store(
    path: &Path,
    cookie_store: &Arc<CookieStoreMutex>,
    encryption_secret: &[u8],
) -> Result<()> {
    if encryption_secret.is_empty() {
        return Err(cookie_error(
            "encrypt",
            path,
            "Cookie encryption secret is empty; refusing to write plaintext cookies",
            None,
        ));
    }

    let cookies = {
        let store = cookie_store.lock().unwrap();
        cookies_from_store(&store)
    };

    let plaintext = serde_json::to_vec_pretty(&cookies).map_err(|e| {
        cookie_error(
            "serialize",
            path,
            format!("Failed to serialize cookies: {}", e),
            Some(Box::new(e)),
        )
    })?;

    let salt: [u8; SALT_LEN] = rand::random();
    let nonce: [u8; NONCE_LEN] = rand::random();
    let key = derive_key(encryption_secret, &salt, KDF_ITERATIONS);
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| {
        cookie_error(
            "encrypt",
            path,
            "Failed to initialize cookie encryptor",
            None,
        )
    })?;
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &plaintext,
                aad: COOKIE_AAD,
            },
        )
        .map_err(|_| cookie_error("encrypt", path, "Failed to encrypt cookies", None))?;

    let encrypted = EncryptedCookieFile {
        version: COOKIE_FILE_VERSION,
        cipher: CIPHER_NAME.to_string(),
        kdf: KDF_NAME.to_string(),
        iterations: KDF_ITERATIONS,
        salt: BASE64.encode(salt),
        nonce: BASE64.encode(nonce),
        ciphertext: BASE64.encode(ciphertext),
    };

    let encrypted_json = serde_json::to_string_pretty(&encrypted).map_err(|e| {
        cookie_error(
            "serialize_envelope",
            path,
            format!("Failed to serialize encrypted cookie envelope: {}", e),
            Some(Box::new(e)),
        )
    })?;

    write_secure_cookie_file(path, encrypted_json.as_bytes()).map_err(|e| {
        cookie_error(
            "write",
            path,
            format!("Failed to write encrypted cookie file: {}", e),
            Some(Box::new(e)),
        )
    })?;

    Ok(())
}

pub fn cookie_count(cookie_store: &Arc<CookieStoreMutex>) -> usize {
    cookie_store.lock().unwrap().iter_any().count()
}

pub fn remove_cookie_file(path: &Path, reason: &str) {
    match fs::remove_file(path) {
        Ok(()) => log::debug!("已删除 cookie 文件 {:?}: {}", path, reason),
        Err(e) => log::warn!("删除 cookie 文件 {:?} 失败: {}", path, e),
    }
}

fn validate_envelope(path: &Path, encrypted: &EncryptedCookieFile) -> Result<()> {
    if encrypted.version != COOKIE_FILE_VERSION {
        return Err(cookie_error(
            "decrypt",
            path,
            format!(
                "Unsupported encrypted cookie file version: {}",
                encrypted.version
            ),
            None,
        ));
    }
    if encrypted.cipher != CIPHER_NAME {
        return Err(cookie_error(
            "decrypt",
            path,
            format!("Unsupported encrypted cookie cipher: {}", encrypted.cipher),
            None,
        ));
    }
    if encrypted.kdf != KDF_NAME {
        return Err(cookie_error(
            "decrypt",
            path,
            format!("Unsupported encrypted cookie KDF: {}", encrypted.kdf),
            None,
        ));
    }
    if encrypted.iterations == 0 {
        return Err(cookie_error(
            "decrypt",
            path,
            "Invalid encrypted cookie KDF iteration count: 0",
            None,
        ));
    }

    Ok(())
}

fn decode_b64(path: &Path, field: &str, value: &str) -> Result<Vec<u8>> {
    BASE64.decode(value).map_err(|e| {
        cookie_error(
            "decode",
            path,
            format!("Failed to base64-decode encrypted cookie {}: {}", field, e),
            Some(Box::new(e)),
        )
    })
}

fn derive_key(secret: &[u8], salt: &[u8], iterations: u32) -> [u8; KEY_LEN] {
    let mut key = [0_u8; KEY_LEN];
    pbkdf2_hmac::<Sha256>(secret, salt, iterations, &mut key);
    key
}

fn cookies_from_store(store: &CookieStore) -> Vec<SerializableCookie> {
    store
        .iter_any()
        .map(|c| {
            // Use a default domain if the cookie doesn't have one.
            let domain = c
                .domain()
                .filter(|d| !d.is_empty())
                .unwrap_or("idas.uestc.edu.cn");

            SerializableCookie {
                name: c.name().to_string(),
                value: c.value().to_string(),
                domain: domain.to_string(),
                path: c.path().unwrap_or("/").to_string(),
                expires: None, // Treat all as session cookies for simplicity.
                secure: c.secure().unwrap_or(false),
                http_only: c.http_only().unwrap_or(false),
            }
        })
        .collect()
}

fn store_from_cookies(cookies: Vec<SerializableCookie>) -> CookieStore {
    let mut store = CookieStore::default();

    for sc in cookies {
        if sc.domain.is_empty() {
            log::debug!("跳过空 domain 的 cookie: {}", sc.name);
            continue;
        }

        let mut cookie_str = format!("{}={}", sc.name, sc.value);
        cookie_str.push_str(&format!("; Domain={}", sc.domain));
        cookie_str.push_str(&format!("; Path={}", sc.path));

        if sc.secure {
            cookie_str.push_str("; Secure");
        }
        if sc.http_only {
            cookie_str.push_str("; HttpOnly");
        }
        if let Some(expires) = sc.expires {
            cookie_str.push_str(&format!("; Max-Age={}", expires));
        }

        if let Ok(cookie) = cookie_str.parse::<cookie_store::RawCookie>() {
            if let Ok(url) = url::Url::parse(&format!("https://{}", sc.domain)) {
                if let Err(e) = store.insert_raw(&cookie, &url) {
                    log::debug!("插入 cookie 失败: {:?}", e);
                }
            } else {
                log::debug!("无法解析 domain: {}", sc.domain);
            }
        }
    }

    store
}

fn write_secure_cookie_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(contents)?;

        let mut permissions = file.metadata()?.permissions();
        permissions.set_mode(0o600);
        file.set_permissions(permissions)?;
        file.flush()?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        fs::write(path, contents)
    }
}

fn cookie_error(
    operation: &str,
    path: &Path,
    message: impl Into<String>,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
) -> UestcClientError {
    UestcClientError::CookieError {
        operation: operation.to_string(),
        file_path: Some(path.display().to_string()),
        message: message.into(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_cookie_path() -> std::path::PathBuf {
        let uniq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("uestc-client-cookie-test-{uniq}.json"))
    }

    fn sample_cookie_store() -> Arc<CookieStoreMutex> {
        let mut store = CookieStore::default();
        let cookie =
            "SESSION=secret-cookie-value; Domain=idas.uestc.edu.cn; Path=/; Secure; HttpOnly"
                .parse::<cookie_store::RawCookie>()
                .expect("parse cookie");
        let url = url::Url::parse("https://idas.uestc.edu.cn").expect("parse url");
        store.insert_raw(&cookie, &url).expect("insert cookie");
        Arc::new(CookieStoreMutex::new(store))
    }

    #[test]
    fn encrypted_cookie_file_round_trips_without_plaintext() {
        let path = unique_cookie_path();
        let store = sample_cookie_store();

        save_encrypted_cookie_store(&path, &store, b"test-secret").expect("save encrypted cookies");

        let contents = fs::read_to_string(&path).expect("read encrypted file");
        assert!(contents.contains("\"cipher\": \"AES-256-GCM\""));
        assert!(!contents.contains("SESSION"));
        assert!(!contents.contains("secret-cookie-value"));

        #[cfg(unix)]
        {
            let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let loaded =
            load_encrypted_cookie_store(&path, b"test-secret").expect("load encrypted cookies");
        let loaded_store = loaded.lock().expect("lock loaded cookies");
        assert!(
            loaded_store
                .iter_any()
                .any(|cookie| cookie.name() == "SESSION"
                    && cookie.value() == "secret-cookie-value")
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn encrypted_cookie_file_rejects_wrong_secret() {
        let path = unique_cookie_path();
        let store = sample_cookie_store();

        save_encrypted_cookie_store(&path, &store, b"right-secret")
            .expect("save encrypted cookies");

        let err = load_encrypted_cookie_store(&path, b"wrong-secret")
            .expect_err("wrong secret must not decrypt");
        assert!(err.to_string().contains("Failed to decrypt cookies"));

        let _ = fs::remove_file(path);
    }
}
