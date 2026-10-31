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
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const COOKIE_FILE_VERSION: u8 = 1;
const COOKIE_AAD: &[u8] = b"uestc-client:encrypted-cookies:v1";
const CIPHER_NAME: &str = "AES-256-GCM";
const KDF_NAME: &str = "PBKDF2-HMAC-SHA256";
/// v1 信封固定使用该迭代次数；读取时必须严格相等，防止篡改文件指定超大迭代数耗尽 CPU。
const KDF_ITERATIONS: u32 = 210_000;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

/// cookie 文件大小上限。正常会话文件只有几 KB，超过即视为异常文件拒绝读取。
const MAX_COOKIE_FILE_BYTES: u64 = 1024 * 1024;
/// 单个文件内 cookie 数量上限。
const MAX_COOKIES: usize = 1024;
/// 单个 cookie 名/值/domain/path 的长度上限（字节）。
const MAX_COOKIE_FIELD_BYTES: usize = 8 * 1024;

/// 显式迁移旧版明文 cookie 时只接受这些域（及其子域）的 cookie。
const LEGACY_IMPORT_ALLOWED_DOMAINS: [&str; 1] = ["uestc.edu.cn"];

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

/// 加密信封内的明文载荷。`principal` 记录这份会话所属的账号（已验证的登录用户名），
/// 用于防止把 A 账号的会话当作 B 账号的登录结果复用。
#[derive(Serialize, Deserialize, Debug)]
struct CookiePayload {
    #[serde(default)]
    principal: Option<String>,
    cookies: Vec<SerializableCookie>,
}

/// 兼容旧版信封：密文内可能直接是 cookie 数组（无 principal）。
/// 两种形态都在 GCM 认证之后才解析，不影响完整性。
#[derive(Deserialize)]
#[serde(untagged)]
enum DecryptedPayload {
    Current(CookiePayload),
    LegacyArray(Vec<SerializableCookie>),
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

/// 从加密文件加载出的会话。
#[derive(Debug)]
pub struct LoadedCookies {
    pub store: CookieStore,
    pub principal: Option<String>,
}

/// 加载加密 cookie 文件。只接受通过 AES-GCM 认证的信封；明文或损坏文件一律报错，
/// 不会回退为明文解析（旧版明文文件请使用 [`migrate_plaintext_cookie_file`] 显式迁移）。
pub fn load_encrypted_cookie_store(path: &Path, encryption_secret: &[u8]) -> Result<LoadedCookies> {
    if encryption_secret.is_empty() {
        return Err(cookie_error(
            "decrypt",
            path,
            "Cookie encryption secret is empty",
            None,
        ));
    }

    let content = read_bounded_regular_file(path)?;
    load_encrypted_envelope(&content, path, encryption_secret)
}

/// 显式把旧版（0.3.0 及更早，未加密）明文 cookie 文件迁移为加密格式。
///
/// 只有调用方明确调用时才会执行；常规加载路径不再接受明文文件。迁移时只保留
/// `uestc.edu.cn` 及其子域的 cookie。迁移后的会话不绑定账号，首次 `login`
/// 会重新走密码登录。返回迁移的 cookie 数量。
pub fn migrate_plaintext_cookie_file<P, S>(path: P, encryption_secret: S) -> Result<usize>
where
    P: AsRef<Path>,
    S: AsRef<[u8]>,
{
    let path = path.as_ref();
    let encryption_secret = encryption_secret.as_ref();
    if encryption_secret.is_empty() {
        return Err(cookie_error(
            "migrate",
            path,
            "Cookie encryption secret is empty",
            None,
        ));
    }

    let content = read_bounded_regular_file(path)?;
    let cookies: Vec<SerializableCookie> = serde_json::from_str(&content).map_err(|e| {
        cookie_error(
            "migrate",
            path,
            format!("File is not a legacy plaintext cookie array: {}", e),
            Some(Box::new(e)),
        )
    })?;
    check_cookie_count(path, cookies.len())?;

    let cookies: Vec<SerializableCookie> = cookies
        .into_iter()
        .filter(|c| {
            let allowed = is_allowed_legacy_domain(&c.domain);
            if !allowed {
                log::warn!(
                    "迁移旧版 cookie 时跳过非学校域 cookie（domain: {}）",
                    c.domain
                );
            }
            allowed
        })
        .collect();

    let store = Arc::new(CookieStoreMutex::new(store_from_cookies(cookies)));
    let count = cookie_count(&store);
    save_encrypted_cookie_store(path, &store, None, encryption_secret)?;
    log::info!("已将旧版明文 cookie 文件迁移为加密存储: {:?}", path);
    Ok(count)
}

fn is_allowed_legacy_domain(domain: &str) -> bool {
    let domain = domain.trim_start_matches('.').to_ascii_lowercase();
    LEGACY_IMPORT_ALLOWED_DOMAINS
        .iter()
        .any(|allowed| domain == *allowed || domain.ends_with(&format!(".{}", allowed)))
}

/// 只读取普通文件，且大小不超过 [`MAX_COOKIE_FILE_BYTES`]。
fn read_bounded_regular_file(path: &Path) -> Result<String> {
    let read_err = |message: String, source: std::io::Error| {
        cookie_error("read", path, message, Some(Box::new(source)))
    };

    let file = fs::File::open(path)
        .map_err(|e| read_err(format!("Failed to open cookie file: {}", e), e))?;
    let metadata = file
        .metadata()
        .map_err(|e| read_err(format!("Failed to stat cookie file: {}", e), e))?;
    if !metadata.is_file() {
        return Err(cookie_error(
            "read",
            path,
            "Cookie path is not a regular file",
            None,
        ));
    }
    if metadata.len() > MAX_COOKIE_FILE_BYTES {
        return Err(cookie_error(
            "read",
            path,
            format!(
                "Cookie file is too large ({} bytes, limit {})",
                metadata.len(),
                MAX_COOKIE_FILE_BYTES
            ),
            None,
        ));
    }

    // 再用 take 兜底，防止 stat 与读取之间文件被追加。
    let mut content = String::new();
    file.take(MAX_COOKIE_FILE_BYTES + 1)
        .read_to_string(&mut content)
        .map_err(|e| read_err(format!("Failed to read cookie file: {}", e), e))?;
    if content.len() as u64 > MAX_COOKIE_FILE_BYTES {
        return Err(cookie_error(
            "read",
            path,
            "Cookie file grew beyond the size limit while reading",
            None,
        ));
    }
    Ok(content)
}

/// 按加密信封格式解析并解密 cookie。
fn load_encrypted_envelope(
    content: &str,
    path: &Path,
    encryption_secret: &[u8],
) -> Result<LoadedCookies> {
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

    let payload: DecryptedPayload = serde_json::from_slice(&plaintext).map_err(|e| {
        cookie_error(
            "deserialize",
            path,
            format!("Failed to deserialize decrypted cookies: {}", e),
            Some(Box::new(e)),
        )
    })?;
    let (principal, cookies) = match payload {
        DecryptedPayload::Current(p) => (p.principal, p.cookies),
        DecryptedPayload::LegacyArray(cookies) => (None, cookies),
    };
    check_cookie_count(path, cookies.len())?;

    Ok(LoadedCookies {
        store: store_from_cookies(cookies),
        principal,
    })
}

fn check_cookie_count(path: &Path, count: usize) -> Result<()> {
    if count > MAX_COOKIES {
        return Err(cookie_error(
            "deserialize",
            path,
            format!(
                "Too many cookies in file ({}, limit {})",
                count, MAX_COOKIES
            ),
            None,
        ));
    }
    Ok(())
}

pub fn save_encrypted_cookie_store(
    path: &Path,
    cookie_store: &Arc<CookieStoreMutex>,
    principal: Option<&str>,
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
        let store = cookie_store.lock().unwrap_or_else(|p| p.into_inner());
        cookies_from_store(&store)
    };
    let payload = CookiePayload {
        principal: principal.map(str::to_string),
        cookies,
    };

    let plaintext = serde_json::to_vec(&payload).map_err(|e| {
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
    cookie_store
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter_any()
        .count()
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
    // v1 写入方固定使用 KDF_ITERATIONS；在认证之前就要求严格相等，
    // 避免被篡改的文件用超大迭代数拖垮 CPU。
    if encrypted.iterations != KDF_ITERATIONS {
        return Err(cookie_error(
            "decrypt",
            path,
            format!(
                "Invalid encrypted cookie KDF iteration count: {} (expected {})",
                encrypted.iterations, KDF_ITERATIONS
            ),
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

    for sc in cookies.into_iter().take(MAX_COOKIES) {
        if sc.domain.is_empty() {
            log::debug!("跳过空 domain 的 cookie: {}", sc.name);
            continue;
        }
        if [&sc.name, &sc.value, &sc.domain, &sc.path]
            .iter()
            .any(|f| f.len() > MAX_COOKIE_FIELD_BYTES)
        {
            log::debug!("跳过超长 cookie 字段（domain: {}）", sc.domain);
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

/// 原子写入 cookie 文件：在同目录创建随机名的临时文件（`create_new`，即
/// O_CREAT|O_EXCL，不会跟随已存在的符号链接），写入并 fsync 后 rename 覆盖目标。
fn write_secure_cookie_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let tmp_path = temp_path_for(path);

    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&tmp_path)?;

        #[cfg(unix)]
        {
            // create_new 保证这是我们刚创建的普通文件；仍显式收紧权限以防 umask 影响。
            let mut permissions = file.metadata()?.permissions();
            permissions.set_mode(0o600);
            file.set_permissions(permissions)?;
        }

        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp_path, path)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

fn temp_path_for(path: &Path) -> PathBuf {
    let suffix: u128 = rand::random();
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cookies".to_string());
    path.with_file_name(format!(".{}.{:032x}.tmp", file_name, suffix))
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
        let rnd: u64 = rand::random();
        std::env::temp_dir().join(format!("uestc-client-cookie-test-{uniq}-{rnd:x}.json"))
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

    const LEGACY_PLAINTEXT: &str = r#"[
        {
            "name": "SESSION",
            "value": "plaintext-secret-value",
            "domain": "idas.uestc.edu.cn",
            "path": "/",
            "expires": null,
            "secure": true,
            "http_only": true
        },
        {
            "name": "EVIL",
            "value": "attacker",
            "domain": "evil.example.com",
            "path": "/",
            "expires": null,
            "secure": true,
            "http_only": true
        }
    ]"#;

    #[test]
    fn encrypted_cookie_file_round_trips_without_plaintext() {
        let path = unique_cookie_path();
        let store = sample_cookie_store();

        save_encrypted_cookie_store(&path, &store, Some("2025000000001"), b"test-secret")
            .expect("save encrypted cookies");

        let contents = fs::read_to_string(&path).expect("read encrypted file");
        assert!(contents.contains("\"cipher\": \"AES-256-GCM\""));
        assert!(!contents.contains("SESSION"));
        assert!(!contents.contains("secret-cookie-value"));
        assert!(!contents.contains("2025000000001"));

        #[cfg(unix)]
        {
            let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let loaded =
            load_encrypted_cookie_store(&path, b"test-secret").expect("load encrypted cookies");
        assert_eq!(loaded.principal.as_deref(), Some("2025000000001"));
        assert!(
            loaded
                .store
                .iter_any()
                .any(|cookie| cookie.name() == "SESSION"
                    && cookie.value() == "secret-cookie-value")
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn legacy_array_payload_inside_envelope_still_loads() {
        // 旧版信封：密文内直接是 cookie 数组（无 principal）
        let path = unique_cookie_path();
        let salt: [u8; SALT_LEN] = rand::random();
        let nonce: [u8; NONCE_LEN] = rand::random();
        let key = derive_key(b"test-secret", &salt, KDF_ITERATIONS);
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let ct = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: br#"[{"name":"SESSION","value":"v","domain":"idas.uestc.edu.cn","path":"/","expires":null,"secure":true,"http_only":true}]"#,
                    aad: COOKIE_AAD,
                },
            )
            .unwrap();
        let env = EncryptedCookieFile {
            version: COOKIE_FILE_VERSION,
            cipher: CIPHER_NAME.into(),
            kdf: KDF_NAME.into(),
            iterations: KDF_ITERATIONS,
            salt: BASE64.encode(salt),
            nonce: BASE64.encode(nonce),
            ciphertext: BASE64.encode(ct),
        };
        fs::write(&path, serde_json::to_string(&env).unwrap()).unwrap();

        let loaded = load_encrypted_cookie_store(&path, b"test-secret").expect("load legacy");
        assert_eq!(loaded.principal, None);
        assert_eq!(loaded.store.iter_any().count(), 1);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn encrypted_cookie_file_rejects_wrong_secret() {
        let path = unique_cookie_path();
        let store = sample_cookie_store();

        save_encrypted_cookie_store(&path, &store, None, b"right-secret")
            .expect("save encrypted cookies");

        let err = load_encrypted_cookie_store(&path, b"wrong-secret")
            .expect_err("wrong secret must not decrypt");
        assert!(err.to_string().contains("Failed to decrypt cookies"));
        assert!(path.exists(), "load failure must not delete the file");

        let _ = fs::remove_file(path);
    }

    #[test]
    fn plaintext_cookie_file_is_rejected_by_encrypted_load() {
        let path = unique_cookie_path();
        fs::write(&path, LEGACY_PLAINTEXT).expect("write plaintext cookie file");

        load_encrypted_cookie_store(&path, b"test-secret")
            .expect_err("plaintext must never be installed by the encrypted loader");

        // 文件保持原样，不被静默改写
        assert_eq!(fs::read_to_string(&path).unwrap(), LEGACY_PLAINTEXT);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn explicit_migration_encrypts_and_filters_foreign_domains() {
        let path = unique_cookie_path();
        fs::write(&path, LEGACY_PLAINTEXT).expect("write plaintext cookie file");

        let migrated = migrate_plaintext_cookie_file(&path, b"test-secret").expect("migrate");
        assert_eq!(migrated, 1, "foreign-domain cookie must be dropped");

        let contents = fs::read_to_string(&path).expect("read migrated file");
        assert!(contents.contains("\"cipher\": \"AES-256-GCM\""));
        assert!(!contents.contains("plaintext-secret-value"));

        let loaded = load_encrypted_cookie_store(&path, b"test-secret").expect("reload");
        assert_eq!(loaded.principal, None);
        let names: Vec<_> = loaded
            .store
            .iter_any()
            .map(|c| c.name().to_string())
            .collect();
        assert_eq!(names, vec!["SESSION".to_string()]);

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

    #[test]
    fn tampered_iteration_count_is_rejected_before_kdf() {
        let path = unique_cookie_path();
        save_encrypted_cookie_store(&path, &sample_cookie_store(), None, b"test-secret")
            .expect("save");
        let tampered = fs::read_to_string(&path)
            .unwrap()
            .replace("\"iterations\": 210000", "\"iterations\": 4294967295");
        fs::write(&path, tampered).unwrap();

        let start = std::time::Instant::now();
        let err = load_encrypted_cookie_store(&path, b"test-secret").expect_err("must reject");
        assert!(err.to_string().contains("iteration count"));
        assert!(start.elapsed() < std::time::Duration::from_secs(5));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn oversized_cookie_file_is_rejected() {
        let path = unique_cookie_path();
        fs::write(&path, vec![b' '; (MAX_COOKIE_FILE_BYTES + 1) as usize]).unwrap();
        let err = load_encrypted_cookie_store(&path, b"test-secret").expect_err("must reject");
        assert!(err.to_string().contains("too large"), "{}", err);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn non_regular_file_is_rejected() {
        let dir = unique_cookie_path();
        fs::create_dir(&dir).unwrap();
        load_encrypted_cookie_store(&dir, b"test-secret").expect_err("directory must be rejected");
        let _ = fs::remove_dir(dir);
    }

    #[cfg(unix)]
    #[test]
    fn save_does_not_follow_symlinks_at_legacy_tmp_path() {
        let path = unique_cookie_path();
        let victim = unique_cookie_path();
        fs::write(&victim, "victim-content").unwrap();
        // 旧实现使用的可预测临时路径
        let legacy_tmp = path.with_extension("tmp");
        std::os::unix::fs::symlink(&victim, &legacy_tmp).unwrap();

        save_encrypted_cookie_store(&path, &sample_cookie_store(), None, b"test-secret")
            .expect("save");

        assert_eq!(fs::read_to_string(&victim).unwrap(), "victim-content");
        assert!(
            fs::symlink_metadata(&legacy_tmp)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            load_encrypted_cookie_store(&path, b"test-secret").is_ok(),
            "destination must hold the new envelope"
        );

        let _ = fs::remove_file(path);
        let _ = fs::remove_file(victim);
        let _ = fs::remove_file(legacy_tmp);
    }

    #[test]
    fn temp_paths_are_unique_per_save() {
        let path = unique_cookie_path();
        assert_ne!(temp_path_for(&path), temp_path_for(&path));
        assert_eq!(temp_path_for(&path).parent(), path.parent());
    }

    #[test]
    fn legacy_domain_allowlist() {
        assert!(is_allowed_legacy_domain("idas.uestc.edu.cn"));
        assert!(is_allowed_legacy_domain(".uestc.edu.cn"));
        assert!(is_allowed_legacy_domain("uestc.edu.cn"));
        assert!(!is_allowed_legacy_domain("evil-uestc.edu.cn"));
        assert!(!is_allowed_legacy_domain("uestc.edu.cn.evil.com"));
    }
}
