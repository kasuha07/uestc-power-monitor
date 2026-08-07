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

    let content = fs::read_to_string(path).map_err(|e| {
        cookie_error(
            "read",
            path,
            format!("Failed to read encrypted cookie file: {}", e),
            Some(Box::new(e)),
        )
    })?;

    match load_encrypted_envelope(&content, path, encryption_secret) {
        Ok(store) => Ok(Arc::new(CookieStoreMutex::new(store))),
        Err(encrypted_err) => match parse_plaintext_cookies(&content, path) {
            Some(cookies) => {
                // 旧版（未加密）cookie 文件：解析成功后立即迁移为加密格式。
                let store = Arc::new(CookieStoreMutex::new(store_from_cookies(cookies)));
                log::info!("检测到旧版明文 cookie 文件，自动迁移为加密存储: {:?}", path);
                if let Err(e) = save_encrypted_cookie_store(path, &store, encryption_secret) {
                    // 写回失败时保留原明文文件，待下次启动重试；内存中的 cookie 仍可使用。
                    log::warn!(
                        "迁移 cookie 文件为加密格式失败，保留原文件待下次启动重试: {}",
                        e
                    );
                }
                Ok(store)
            }
            None => Err(encrypted_err),
        },
    }
}

/// 按加密信封格式解析并解密 cookie。失败返回 Err（由调用方决定是否回退明文）。
fn load_encrypted_envelope(
    content: &str,
    path: &Path,
    encryption_secret: &[u8],
) -> Result<CookieStore> {
    let encrypted: EncryptedCookieFile = serde_json::from_str(content).map_err(|e| {
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

    Ok(store_from_cookies(cookies))
}

/// 尝试按旧版明文格式（加密前的 cookie 文件）解析。失败返回 None。
fn parse_plaintext_cookies(content: &str, path: &Path) -> Option<Vec<SerializableCookie>> {
    match serde_json::from_str::<Vec<SerializableCookie>>(content) {
        Ok(cookies) => Some(cookies),
        Err(e) => {
            log::debug!(
                "cookie 文件既非加密信封也非旧版明文格式: {:?} ({})",
                path,
                e
            );
            None
        }
    }
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
        // 先写临时文件再原子重命名，避免写入中途失败留下截断的 cookie 文件。
        let tmp_path = path.with_extension("tmp");
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp_path)?;
        file.write_all(contents)?;

        let mut permissions = file.metadata()?.permissions();
        permissions.set_mode(0o600);
        file.set_permissions(permissions)?;
        file.flush()?;
        fs::rename(&tmp_path, path)?;
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

    #[test]
    fn plaintext_cookie_file_is_migrated_to_encrypted_on_load() {
        let path = unique_cookie_path();
        // 模拟旧版明文格式（uestc-client 0.3.0 加密前的 cookie 文件）。
        let plaintext_json = r#"[
            {
                "name": "SESSION",
                "value": "plaintext-secret-value",
                "domain": "idas.uestc.edu.cn",
                "path": "/",
                "expires": null,
                "secure": true,
                "http_only": true
            }
        ]"#;
        fs::write(&path, plaintext_json).expect("write plaintext cookie file");

        let loaded = load_encrypted_cookie_store(&path, b"test-secret")
            .expect("migrate and load plaintext cookies");
        assert!(
            loaded
                .lock()
                .expect("lock migrated cookies")
                .iter_any()
                .any(|cookie| cookie.name() == "SESSION"
                    && cookie.value() == "plaintext-secret-value")
        );

        // 文件应立即变为加密信封格式，且不含任何明文内容。
        let contents = fs::read_to_string(&path).expect("read migrated file");
        assert!(contents.contains("\"cipher\": \"AES-256-GCM\""));
        assert!(!contents.contains("SESSION"));
        assert!(!contents.contains("plaintext-secret-value"));

        #[cfg(unix)]
        {
            let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        // 迁移后的加密文件可再次正常加载（round-trip）。
        let reloaded =
            load_encrypted_cookie_store(&path, b"test-secret").expect("reload migrated file");
        assert!(
            reloaded
                .lock()
                .expect("lock reloaded cookies")
                .iter_any()
                .any(|cookie| cookie.name() == "SESSION")
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn empty_plaintext_file_migrates_to_empty_encrypted_file() {
        let path = unique_cookie_path();
        fs::write(&path, "[]").expect("write empty plaintext cookie file");

        let loaded = load_encrypted_cookie_store(&path, b"test-secret")
            .expect("migrate empty plaintext file");
        assert_eq!(cookie_count(&loaded), 0);

        let contents = fs::read_to_string(&path).expect("read migrated file");
        assert!(contents.contains("\"cipher\": \"AES-256-GCM\""));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn garbage_cookie_file_still_fails_to_load() {
        let path = unique_cookie_path();
        fs::write(
            &path,
            r#"{"this": "is", "neither": "encrypted", "nor": "plaintext"}"#,
        )
        .expect("write garbage cookie file");

        let err = load_encrypted_cookie_store(&path, b"test-secret")
            .expect_err("garbage file must fail to load");
        assert!(
            err.to_string()
                .contains("Failed to deserialize encrypted cookie envelope"),
            "unexpected error: {}",
            err
        );

        let _ = fs::remove_file(path);
    }
}
