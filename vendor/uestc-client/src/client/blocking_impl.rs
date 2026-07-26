use super::AUTH_SERVER_URL;
use super::cookie_persistence::{
    cookie_count, load_encrypted_cookie_store, remove_cookie_file, save_encrypted_cookie_store,
};
use crate::{Result, UestcClientError, core};
use cookie_store::CookieStore;
use reqwest::blocking::{Client, RequestBuilder};
use reqwest::{IntoUrl, Method};
use reqwest_cookie_store::CookieStoreMutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const DEFAULT_COOKIE_FILE: &str = "uestc_cookies.json";

pub struct UestcBlockingClient {
    client: Client,
    cookie_store: Arc<CookieStoreMutex>,
    cookie_file: PathBuf,
    cookie_encryption_secret: Option<Vec<u8>>,
}

impl UestcBlockingClient {
    pub fn new() -> Self {
        Self::with_cookie_file(DEFAULT_COOKIE_FILE)
    }

    pub fn with_cookie_file<P: AsRef<Path>>(path: P) -> Self {
        Self::with_cookie_file_and_secret(path, None)
    }

    pub fn with_encrypted_cookie_file<P, S>(path: P, encryption_secret: S) -> Self
    where
        P: AsRef<Path>,
        S: AsRef<[u8]>,
    {
        Self::with_cookie_file_and_secret(path, Some(encryption_secret.as_ref().to_vec()))
    }

    fn with_cookie_file_and_secret<P: AsRef<Path>>(
        path: P,
        cookie_encryption_secret: Option<Vec<u8>>,
    ) -> Self {
        let cookie_file = path.as_ref().to_path_buf();

        // Try to load existing cookies
        let cookie_store = if cookie_file.exists() {
            log::debug!("发现 cookie 文件: {:?}", cookie_file);
            match cookie_encryption_secret.as_deref() {
                Some(secret) if !secret.is_empty() => {
                    match load_encrypted_cookie_store(&cookie_file, secret) {
                        Ok(store) => {
                            log::debug!("成功加载 {} 个加密 cookies", cookie_count(&store));
                            store
                        }
                        Err(e) => {
                            log::warn!("加载加密 cookie 失败: {}", e);
                            remove_cookie_file(
                                &cookie_file,
                                "无法按加密格式读取，不保留旧格式或损坏的 cookie",
                            );
                            Arc::new(CookieStoreMutex::new(CookieStore::default()))
                        }
                    }
                }
                _ => {
                    log::warn!(
                        "cookie 文件存在但未配置加密密钥，已忽略该文件以避免明文 cookie 落盘"
                    );
                    remove_cookie_file(&cookie_file, "未配置加密密钥，不保留现有 cookie 文件");
                    Arc::new(CookieStoreMutex::new(CookieStore::default()))
                }
            }
        } else {
            log::debug!("cookie 文件不存在: {:?}", cookie_file);
            Arc::new(CookieStoreMutex::new(CookieStore::default()))
        };

        let client = Client::builder()
            .default_headers(super::default_headers())
            .cookie_provider(cookie_store.clone())
            .connect_timeout(super::CONNECT_TIMEOUT)
            .timeout(super::REQUEST_TIMEOUT)
            .build()
            .expect("Failed to build client");

        Self {
            client,
            cookie_store,
            cookie_file,
            cookie_encryption_secret,
        }
    }

    pub fn with_client(client: Client) -> Self {
        let cookie_store = Arc::new(CookieStoreMutex::new(CookieStore::default()));
        Self {
            client,
            cookie_store,
            cookie_file: PathBuf::from(DEFAULT_COOKIE_FILE),
            cookie_encryption_secret: None,
        }
    }

    fn save_cookie_store(&self) -> Result<()> {
        let secret = self.cookie_encryption_secret.as_deref().ok_or_else(|| {
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
        save_encrypted_cookie_store(&self.cookie_file, &self.cookie_store, secret)?;
        log::debug!("cookies 已成功加密保存");
        Ok(())
    }

    pub fn login(&self, username: &str, password: &str) -> Result<()> {
        log::info!("Starting login for user: {}", username);

        // Check if session is already active
        if self.is_session_active() {
            log::info!("Session already active, skipping login");
            return Ok(());
        }

        // Perform password login
        let login_url = format!("{}/login", AUTH_SERVER_URL);

        log::debug!("Fetching login page");
        // Get login page without service parameter
        let resp = self.client.get(&login_url).send()?;
        let html = resp.text()?;

        log::debug!("Parsing login page");
        // Parse login page
        let info = core::parser::parse_login_page(&html)?;

        log::debug!("Encrypting password");
        // Encrypt password
        let encrypted_password = core::crypto::encrypt_password(password, &info.pwd_encrypt_salt)?;

        // Prepare form data
        let mut form_data = info.form_data;
        form_data
            .entry("username".to_string())
            .and_modify(|v| *v = username.to_string())
            .or_insert(username.to_string());
        form_data
            .entry("password".to_string())
            .and_modify(|v| *v = encrypted_password.to_string())
            .or_insert(encrypted_password.to_string());

        log::debug!("Submitting login form");
        // Submit login form
        let resp = self.client.post(&login_url).form(&form_data).send()?;

        // Check for redirect (302) or success status
        let status = resp.status();
        let final_url = resp.url().to_string();

        log::debug!("Login response status: {}, URL: {}", status, final_url);

        // Login is successful if we're not on the login page
        if status.is_redirection() || status.is_success() {
            if !final_url.contains("/authserver/login") {
                log::info!("Login successful for user: {}", username);
                // Save cookies after successful login
                if let Err(e) = self.save_cookie_store() {
                    log::warn!("Failed to save cookies after login: {}", e);
                }
                return Ok(());
            }
        }

        // If we're still on login page, extract error message
        let html = resp.text()?;
        let error_msg = core::parser::extract_error_message(&html)
            .unwrap_or_else(|| format!("Login failed with status: {}", status));

        log::error!("Login failed for user {}: {}", username, error_msg);

        Err(UestcClientError::LoginFailed {
            message: error_msg,
            username: Some(username.to_string()),
        })
    }

    pub fn logout(&self) -> Result<()> {
        log::info!("Attempting to logout");

        let logout_url = format!("{}/logout", AUTH_SERVER_URL);
        let resp = self.client.get(&logout_url).send()?;

        if resp.status().is_success() {
            log::info!("Logout successful");
            // Clear cookies after logout
            if let Err(e) = std::fs::remove_file(&self.cookie_file) {
                log::warn!("Failed to delete cookie file after logout: {}", e);
            }
            return Ok(());
        }

        let error_msg = format!("Logout failed with status: {}", resp.status());
        log::error!("{}", error_msg);

        Err(UestcClientError::LogoutFailed { message: error_msg })
    }

    /// Login using WeChat QR code
    /// This will display a QR code in the terminal for scanning
    pub fn wechat_login(&self) -> Result<()> {
        use crate::core::wechat;

        // Check if session is already active
        log::debug!("检查已存储的会话");
        if self.is_session_active() {
            log::info!("已经登录，无需重新登录");
            return Ok(());
        }
        log::debug!("未检测到有效会话，开始微信登录流程");

        log::debug!("正在连接 CAS 初始化参数");

        // Step 1: Get WeChat OAuth parameters
        let cas_login_url = format!("{}/combinedLogin.do?type=weixin", AUTH_SERVER_URL);
        let resp = self.client.get(&cas_login_url).send()?;

        // Extract WeChat OAuth parameters from the final URL
        let wechat_auth_url = resp.url().to_string();

        // Verify we got redirected to WeChat OAuth page
        if !wechat_auth_url.contains("open.weixin.qq.com") {
            return Err(UestcClientError::WeChatError {
                message: format!(
                    "Failed to redirect to WeChat login page, current URL: {}",
                    wechat_auth_url
                ),
            });
        }

        let params = wechat::WechatAuthParams::from_url(&wechat_auth_url)?;
        log::debug!("Target AppID: {}", params.appid);

        // Step 2: Get QR code UUID
        log::debug!("正在获取二维码 UUID");
        let xml_url = params.build_qr_xml_url();
        let resp = self.client.get(&xml_url).send()?;
        let xml_text = resp.text()?;
        let uuid = wechat::parse_qr_uuid_from_xml(&xml_text)?;

        // Step 3: Display QR code in terminal
        wechat::display_qr_in_terminal(&uuid)?;

        // Step 4: Poll for scan status
        log::debug!("等待扫码");
        let mut last_code: Option<String> = None;
        let mut guard = wechat::ScanPollGuard::new();
        let wx_code = loop {
            guard.check_deadline()?;

            let poll_url = wechat::build_poll_url(&uuid, last_code.as_deref());
            let polled = (|| {
                let resp = self
                    .client
                    .get(&poll_url)
                    .timeout(wechat::POLL_REQUEST_TIMEOUT)
                    .send()?;
                let text = resp.text()?;
                wechat::parse_scan_status(&text)
            })();

            // A single blip must not invalidate a QR code the user is mid-scan on.
            let result = match polled {
                Ok(result) => result,
                Err(e) => {
                    guard.record_failure(e)?;
                    std::thread::sleep(wechat::POLL_INTERVAL);
                    continue;
                }
            };

            guard.record_status(&result.status)?;

            match result.status {
                wechat::ScanStatus::Confirmed => {
                    log::debug!("登录成功 (405)");
                    if let Some(code) = result.wx_code {
                        log::debug!("获取到 wx_code");
                        break code;
                    } else {
                        return Err(UestcClientError::WeChatError {
                            message: "Received 405 status but wx_code not found".to_string(),
                        });
                    }
                }
                wechat::ScanStatus::Scanned => {
                    log::info!("已扫码，请在手机上点击确认");
                    last_code = Some("404".to_string());
                }
                wechat::ScanStatus::Expired => {
                    return Err(UestcClientError::WeChatError {
                        message: "QR code expired, please run again".to_string(),
                    });
                }
                wechat::ScanStatus::Waiting => {
                    // Keep waiting silently
                }
                wechat::ScanStatus::Unknown(_) => {
                    // Counted and logged by the guard above.
                }
            }

            std::thread::sleep(wechat::POLL_INTERVAL);
        };

        // Step 5: Complete login
        log::debug!("正在验证登录");
        let callback_url = params.build_callback_url(&wx_code);
        let resp = self.client.get(&callback_url).send()?;
        let final_url = resp.url().to_string();

        // Consume the response body to ensure cookies are properly captured
        let _ = resp.bytes()?;

        // Check if login succeeded by examining the final URL
        if !final_url.contains("/authserver/login") {
            // Save cookies after successful login
            if let Err(e) = self.save_cookie_store() {
                log::warn!("Failed to save cookies after WeChat login: {}", e);
            }
            log::info!("微信登录成功");
            Ok(())
        } else {
            Err(UestcClientError::WeChatError {
                message: "WeChat login failed, still on login page".to_string(),
            })
        }
    }

    /// Check if the current session is still active
    /// Returns true if logged in, false otherwise
    pub fn is_session_active(&self) -> bool {
        let login_url = format!("{}/login", AUTH_SERVER_URL);
        let expected_redirect = "https://idas.uestc.edu.cn/personalInfo/personCenter/index.html";

        log::debug!("Checking session status");

        match self.client.get(&login_url).send() {
            Ok(resp) => {
                let final_url = resp.url().to_string();
                // If we're redirected to personal center, session is active
                if final_url == expected_redirect {
                    log::debug!("Session is active");
                    // Save cookies when session is confirmed active
                    if let Err(e) = self.save_cookie_store() {
                        log::warn!("Failed to save cookies during session check: {}", e);
                    }
                    true
                } else {
                    log::debug!("Session is not active (URL: {})", final_url);
                    false
                }
            }
            Err(e) => {
                log::debug!("Session check failed: {}", e);
                false
            }
        }
    }

    pub fn request<U: IntoUrl>(&self, method: Method, url: U) -> RequestBuilder {
        self.client.request(method, url)
    }

    pub fn get<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::GET, url)
    }

    pub fn post<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::POST, url)
    }

    pub fn put<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::PUT, url)
    }

    pub fn patch<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::PATCH, url)
    }

    pub fn delete<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::DELETE, url)
    }

    pub fn head<U: IntoUrl>(&self, url: U) -> RequestBuilder {
        self.request(Method::HEAD, url)
    }
}

impl Default for UestcBlockingClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let _client = UestcBlockingClient::new();
        assert!(true);
    }

    #[test]
    fn test_with_client() {
        use reqwest::blocking::Client;
        let req_client = Client::new();
        let _client = UestcBlockingClient::with_client(req_client);
        assert!(true);
    }

    #[test]
    fn test_login_failed() {
        let client = UestcBlockingClient::new();
        let result = client.login("1234567890", "password123");
        assert!(result.is_err());
    }
}
