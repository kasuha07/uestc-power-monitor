//! async / blocking 两套客户端共用的本地会话状态：cookie store、加密持久化、
//! 会话所属账号（principal）与多因子设备指纹。

use super::cookie_persistence::{
    cookie_count, load_encrypted_cookie_store, save_encrypted_cookie_store,
};
use crate::{Result, UestcClientError, core};
use cookie_store::CookieStore;
use reqwest_cookie_store::CookieStoreMutex;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// 设备指纹 cookie 名：服务端种下的 HttpOnly 持久 cookie，
/// 作为多因子风控的"可信设备"标识（见 `core::bfp`）。
pub(crate) const MULTIFACTOR_FINGERPRINT_COOKIE: &str = "MULTIFACTOR_BROWSER_FINGERPRINT";

pub(crate) struct SessionState {
    pub(crate) cookie_store: Arc<CookieStoreMutex>,
    cookie_file: PathBuf,
    encryption_secret: Option<Vec<u8>>,
    /// 当前会话所属账号。只有密码登录 / reauth 成功后才会写入；
    /// 未知（微信登录、旧版文件、空会话）时为 `None`。
    principal: Mutex<Option<String>>,
    /// 多因子设备指纹（32 位大写 hex，服务端只当字符串比对；cookie 持久化复用）
    bfp_fingerprint: Mutex<String>,
}

impl SessionState {
    /// 严格模式：cookie 文件存在但无法读取/解密时返回错误，不修改文件。
    pub(crate) fn open(path: &Path, encryption_secret: Option<Vec<u8>>) -> Result<Self> {
        let cookie_file = path.to_path_buf();
        let secret = encryption_secret.filter(|s| !s.is_empty());

        let (store, principal) = match secret.as_deref() {
            Some(secret) if cookie_file.exists() => {
                log::debug!("发现 cookie 文件: {:?}", cookie_file);
                let loaded = load_encrypted_cookie_store(&cookie_file, secret)?;
                (loaded.store, loaded.principal)
            }
            None if cookie_file.exists() => {
                // 不删除调用方指定的文件：未配置密钥时只是不读取、也不写入。
                log::warn!(
                    "cookie 文件 {:?} 存在但未配置加密密钥：本实例不读取也不写入该文件",
                    cookie_file
                );
                (CookieStore::default(), None)
            }
            _ => {
                log::debug!("cookie 文件不存在: {:?}", cookie_file);
                (CookieStore::default(), None)
            }
        };

        Ok(Self::from_parts(cookie_file, secret, store, principal))
    }

    /// 宽松模式（供不可失败的构造函数使用）：加载失败时记录警告并以空会话启动，
    /// **保留**原文件不动，由调用方决定如何处理。
    pub(crate) fn open_lenient(path: &Path, encryption_secret: Option<Vec<u8>>) -> Self {
        match Self::open(path, encryption_secret.clone()) {
            Ok(state) => state,
            Err(e) => {
                log::warn!(
                    "加载加密 cookie 失败，以空会话启动（原文件已保留，未删除）: {}",
                    e
                );
                Self::from_parts(
                    path.to_path_buf(),
                    encryption_secret.filter(|s| !s.is_empty()),
                    CookieStore::default(),
                    None,
                )
            }
        }
    }

    /// 无持久化（`with_client`）。
    pub(crate) fn in_memory(cookie_file: PathBuf) -> Self {
        Self::from_parts(cookie_file, None, CookieStore::default(), None)
    }

    fn from_parts(
        cookie_file: PathBuf,
        encryption_secret: Option<Vec<u8>>,
        store: CookieStore,
        principal: Option<String>,
    ) -> Self {
        let cookie_store = Arc::new(CookieStoreMutex::new(store));
        log::debug!("已加载 {} 个 cookies", cookie_count(&cookie_store));
        let bfp_fingerprint = fingerprint_from_cookie_store(&cookie_store);
        Self {
            cookie_store,
            cookie_file,
            encryption_secret,
            principal: Mutex::new(principal),
            bfp_fingerprint: Mutex::new(bfp_fingerprint),
        }
    }

    pub(crate) fn principal(&self) -> Option<String> {
        self.principal
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub(crate) fn set_principal(&self, principal: Option<String>) {
        *self.principal.lock().unwrap_or_else(|p| p.into_inner()) = principal;
    }

    pub(crate) fn fingerprint(&self) -> String {
        self.bfp_fingerprint
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// 加密保存当前 cookie store（未配置密钥时报错，绝不写明文）。
    pub(crate) fn save(&self) -> Result<()> {
        let secret = self.encryption_secret.as_deref().ok_or_else(|| {
            UestcClientError::CookieError {
                operation: "encrypt".to_string(),
                file_path: Some(self.cookie_file.display().to_string()),
                message: "Cookie encryption secret is not configured; refusing to write plaintext cookies".to_string(),
                source: None,
            }
        })?;

        log::debug!(
            "加密保存 {} 个 cookies 到: {:?}",
            cookie_count(&self.cookie_store),
            self.cookie_file
        );
        let principal = self.principal();
        save_encrypted_cookie_store(
            &self.cookie_file,
            &self.cookie_store,
            principal.as_deref(),
            secret,
        )?;
        log::debug!("cookies 已成功加密保存");
        Ok(())
    }

    /// 配置了加密密钥时保存；未配置时什么也不做（与"不持久化"语义一致）。
    pub(crate) fn save_if_persistent(&self) -> Result<()> {
        if self.encryption_secret.is_some() {
            self.save()
        } else {
            Ok(())
        }
    }

    /// 从磁盘重新加载加密 cookie 文件（含 principal），替换内存中的会话。
    pub(crate) fn reload(&self) -> Result<()> {
        let secret =
            self.encryption_secret
                .as_deref()
                .ok_or_else(|| UestcClientError::CookieError {
                    operation: "load".to_string(),
                    file_path: Some(self.cookie_file.display().to_string()),
                    message: "Cookie encryption secret is not configured; cannot reload cookies"
                        .to_string(),
                    source: None,
                })?;

        if !self.cookie_file.exists() {
            log::warn!("cookie 文件不存在，无法重载: {:?}", self.cookie_file);
            return Ok(());
        }

        let loaded = load_encrypted_cookie_store(&self.cookie_file, secret)?;
        *self.cookie_store.lock().unwrap_or_else(|p| p.into_inner()) = loaded.store;
        self.set_principal(loaded.principal);
        log::debug!(
            "已从磁盘重载 {} 个 cookies: {:?}",
            cookie_count(&self.cookie_store),
            self.cookie_file
        );
        Ok(())
    }

    /// 清空本地会话（内存 + 账号绑定）。
    ///
    /// `keep_device_fingerprint = true` 时只保留设备指纹 cookie（可信设备标识），
    /// 其余会话/业务 cookie 全部丢弃；`false` 时连指纹一起清除并重新生成随机指纹。
    /// 不负责落盘，调用方按需调用 `save_if_persistent`。
    pub(crate) fn clear(&self, keep_device_fingerprint: bool) {
        {
            let mut store = self.cookie_store.lock().unwrap_or_else(|p| p.into_inner());
            if keep_device_fingerprint {
                let kept: Vec<cookie_store::Cookie<'static>> = store
                    .iter_any()
                    .filter(|c| c.name() == MULTIFACTOR_FINGERPRINT_COOKIE)
                    .cloned()
                    .collect();
                store.clear();
                for cookie in kept {
                    if let Some(url) = cookie_url(&cookie)
                        && let Err(e) = store.insert(cookie, &url)
                    {
                        log::debug!("恢复设备指纹 cookie 失败: {:?}", e);
                    }
                }
            } else {
                store.clear();
            }
        }
        self.set_principal(None);
        if !keep_device_fingerprint {
            *self
                .bfp_fingerprint
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = core::bfp::random_fingerprint();
        }
    }
}

/// 复用 cookie store 中已持久化的设备指纹（无则随机生成）。
/// 指纹是"可信设备"标识：只有每次上报同值，服务端才能持续识别本机为可信设备。
pub(crate) fn fingerprint_from_cookie_store(cookie_store: &Arc<CookieStoreMutex>) -> String {
    cookie_store
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter_any()
        .find(|c| c.name() == MULTIFACTOR_FINGERPRINT_COOKIE)
        .map(|c| c.value())
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .unwrap_or_else(core::bfp::random_fingerprint)
}

fn cookie_url(cookie: &cookie_store::Cookie<'_>) -> Option<url::Url> {
    let domain = cookie.domain()?.trim_start_matches('.');
    let path = cookie.path().unwrap_or("/");
    url::Url::parse(&format!("https://{}{}", domain, path)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert(state: &SessionState, raw: &'static str) {
        let url = url::Url::parse("https://idas.uestc.edu.cn/authserver/").unwrap();
        let cookie = cookie_store::Cookie::parse(raw, &url).expect("parse cookie");
        state
            .cookie_store
            .lock()
            .unwrap()
            .insert(cookie, &url)
            .expect("insert cookie");
    }

    fn names(state: &SessionState) -> Vec<String> {
        let mut v: Vec<String> = state
            .cookie_store
            .lock()
            .unwrap()
            .iter_any()
            .map(|c| c.name().to_string())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn fingerprint_reused_from_store() {
        let store = Arc::new(CookieStoreMutex::new(CookieStore::default()));
        let url = url::Url::parse("https://idas.uestc.edu.cn/").expect("parse url");
        let cookie = cookie_store::Cookie::parse(
            "MULTIFACTOR_BROWSER_FINGERPRINT=ABCDEF0123456789ABCDEF0123456789; Path=/; HttpOnly",
            &url,
        )
        .expect("parse cookie");
        store.lock().unwrap().insert(cookie, &url).unwrap();

        assert_eq!(
            fingerprint_from_cookie_store(&store),
            "ABCDEF0123456789ABCDEF0123456789"
        );
    }

    #[test]
    fn fingerprint_generated_when_store_has_none() {
        let store = Arc::new(CookieStoreMutex::new(CookieStore::default()));
        let fp = fingerprint_from_cookie_store(&store);
        assert_eq!(fp.len(), 32);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn clear_keeping_device_retains_only_fingerprint() {
        let state = SessionState::in_memory(PathBuf::from("unused.json"));
        insert(
            &state,
            "CASTGC=TGT-secret; Domain=idas.uestc.edu.cn; Path=/authserver; Secure; HttpOnly",
        );
        insert(&state, "JSESSIONID=abc; Domain=idas.uestc.edu.cn; Path=/");
        insert(
            &state,
            "MULTIFACTOR_BROWSER_FINGERPRINT=ABCDEF0123456789ABCDEF0123456789; Domain=idas.uestc.edu.cn; Path=/; HttpOnly",
        );
        state.set_principal(Some("user-a".into()));

        state.clear(true);

        assert_eq!(
            names(&state),
            vec![MULTIFACTOR_FINGERPRINT_COOKIE.to_string()]
        );
        assert_eq!(state.principal(), None);
    }

    #[test]
    fn full_clear_removes_everything_and_rotates_fingerprint() {
        let state = SessionState::in_memory(PathBuf::from("unused.json"));
        insert(
            &state,
            "MULTIFACTOR_BROWSER_FINGERPRINT=ABCDEF0123456789ABCDEF0123456789; Domain=idas.uestc.edu.cn; Path=/; HttpOnly",
        );
        insert(
            &state,
            "CASTGC=TGT-secret; Domain=idas.uestc.edu.cn; Path=/authserver",
        );
        let before = state.fingerprint();

        state.clear(false);

        assert!(names(&state).is_empty());
        assert_ne!(state.fingerprint(), before);
    }

    #[test]
    fn open_preserves_unreadable_file() {
        let path = std::env::temp_dir().join(format!(
            "uestc-client-session-test-{:x}.json",
            rand::random::<u64>()
        ));
        std::fs::write(&path, "not a cookie file").unwrap();

        assert!(SessionState::open(&path, Some(b"secret".to_vec())).is_err());
        let _lenient = SessionState::open_lenient(&path, Some(b"secret".to_vec()));
        let _no_secret = SessionState::open_lenient(&path, None);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not a cookie file");

        let _ = std::fs::remove_file(path);
    }
}
