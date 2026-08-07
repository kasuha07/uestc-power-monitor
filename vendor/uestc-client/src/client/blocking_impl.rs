use super::AUTH_SERVER_URL;
use super::cookie_persistence::{
    cookie_count, load_encrypted_cookie_store, remove_cookie_file, save_encrypted_cookie_store,
};
use crate::core::reauth::{
    self, REAUTH_URL_MARKERS, ReauthContext, ReauthMethod, ReauthMethodKind,
};
use crate::{Result, UestcClientError, core};
use cookie_store::CookieStore;
use reqwest::blocking::{Client, RequestBuilder};
use reqwest::header;
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
    /// 多因子设备指纹（32 位大写 hex，服务端只当字符串比对；随实例生成，cookie 持久化复用）
    bfp_fingerprint: String,
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
            .pool_idle_timeout(super::POOL_IDLE_TIMEOUT)
            .tcp_keepalive(super::TCP_KEEPALIVE)
            .build()
            .expect("Failed to build client");

        let bfp_fingerprint = Self::fingerprint_from_cookie_store(&cookie_store);

        Self {
            client,
            cookie_store,
            cookie_file,
            cookie_encryption_secret,
            bfp_fingerprint,
        }
    }

    pub fn with_client(client: Client) -> Self {
        let cookie_store = Arc::new(CookieStoreMutex::new(CookieStore::default()));
        Self {
            client,
            cookie_store,
            cookie_file: PathBuf::from(DEFAULT_COOKIE_FILE),
            cookie_encryption_secret: None,
            bfp_fingerprint: core::bfp::random_fingerprint(),
        }
    }

    /// 复用 cookie store 中已持久化的设备指纹（无则随机生成）。
    /// 与 async_impl 保持一致：指纹是"可信设备"标识，只有每次上报同值，
    /// 服务端才能持续识别本机为可信设备——退出登录后保留的指纹 cookie
    /// 因此继续有效。
    fn fingerprint_from_cookie_store(cookie_store: &Arc<CookieStoreMutex>) -> String {
        cookie_store
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter_any()
            .find(|c| c.name() == super::async_impl::MULTIFACTOR_FINGERPRINT_COOKIE)
            .map(|c| c.value())
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .unwrap_or_else(core::bfp::random_fingerprint)
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

        // 带锁定 TGT（上次 reauth 未完成）时 GET /login 会被 302 到 reauth 页
        let login_page_url = resp.url().to_string();
        if is_reauth_url(&login_page_url) {
            let html = resp.text()?;
            let context = reauth::parse_reauth_page(&html)?;
            log::warn!("发现未完成的 reauth 会话（TGT 已发但被锁定），返回 ReauthRequired");
            return Err(UestcClientError::ReauthRequired {
                context: Box::new(context),
            });
        }
        let html = resp.text()?;

        // 上报 bfp 设备指纹（多因子风控；服务端只当字符串比对，失败不阻断）
        self.report_bfp();

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
        let html = resp.text()?;

        log::debug!("Login response status: {}, URL: {}", status, final_url);

        // 多因子策略：密码校验通过（TGT 已发）但被 302 到 reauth 页锁定
        if is_reauth_url(&final_url) {
            let context = reauth::parse_reauth_page(&html)?;
            log::warn!(
                "账号需要多因子认证（reauth）：TGT 已发但被锁定，可用方式 {:?}",
                context
                    .available_methods
                    .iter()
                    .map(|m| (m.id, m.name.as_str()))
                    .collect::<Vec<_>>()
            );
            return Err(UestcClientError::ReauthRequired {
                context: Box::new(context),
            });
        }

        // Login is successful if we're not on the login page
        if (status.is_redirection() || status.is_success())
            && !final_url.contains("/authserver/login")
        {
            log::info!("Login successful for user: {}", username);
            // Save cookies after successful login
            if let Err(e) = self.save_cookie_store() {
                log::warn!("Failed to save cookies after login: {}", e);
            }
            return Ok(());
        }

        // If we're still on login page, extract error message
        let error_msg = core::parser::extract_error_message(&html)
            .unwrap_or_else(|| format!("Login failed with status: {}", status));

        log::error!("Login failed for user {}: {}", username, error_msg);

        Err(UestcClientError::LoginFailed {
            message: error_msg,
            username: Some(username.to_string()),
        })
    }

    /// 上报 bfp 设备指纹（GET /bfp/info?bfp=<32hex>，换 HttpOnly 持久 cookie）。
    /// 失败只记日志，不阻断登录（live 实测证明流程不依赖它）。
    fn report_bfp(&self) {
        let url = format!("{}/bfp/info?bfp={}", AUTH_SERVER_URL, self.bfp_fingerprint);
        match self.client.get(&url).send() {
            Ok(resp) => log::debug!("bfp 指纹上报完成 (HTTP {})", resp.status()),
            Err(e) => log::warn!("bfp 指纹上报失败（不影响登录）: {}", e),
        }
    }

    pub fn logout(&self) -> Result<()> {
        log::info!("Attempting to logout");

        let logout_url = format!("{}/logout", AUTH_SERVER_URL);
        let resp = self.client.get(&logout_url).send()?;

        if resp.status().is_success() {
            log::info!("Logout successful");
            // 只结束服务端会话，**不清理本地 cookie 文件**：设备指纹 cookie
            // （MULTIFACTOR_BROWSER_FINGERPRINT）保留后，下次登录复用同值上报，
            // 服务端仍识别本机为可信设备（免 reauth 弹窗）。需要彻底清除本地
            // cookie（含指纹）时，由调用方自行删除 cookie 文件。
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

        // 按约定（2026-08-07 决策）：微信扫码视为不触发 reauth。
        // 若服务端实际把扫码后的 TGT 锁了（落回 reauth 页），这里只记警告不报错——
        // 业务请求会失败并走调用方的会话恢复路径。
        if is_reauth_url(&final_url) {
            log::warn!(
                "微信扫码后进入 reauth 页（TGT 被多因子锁定）。当前实现按'扫码不触发 reauth'处理，返回成功；若后续业务请求失败，请改用密码登录并完成 reauth"
            );
        }

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

    /// 从磁盘重新加载加密 cookie 文件，替换内存中的 cookie store。
    ///
    /// 供无人值守调用方（如 power-monitor）在会话失效后、由外部进程
    /// （`--reauth` 子命令）写入了新 cookie 文件时恢复会话。
    pub fn reload_cookie_file(&self) -> Result<()> {
        let secret = self.cookie_encryption_secret.as_deref().ok_or_else(|| {
            UestcClientError::CookieError {
                operation: "load".to_string(),
                file_path: Some(self.cookie_file.display().to_string()),
                message: "Cookie encryption secret is not configured; cannot reload cookies"
                    .to_string(),
                source: None,
            }
        })?;

        if !self.cookie_file.exists() {
            log::warn!("cookie 文件不存在，无法重载: {:?}", self.cookie_file);
            return Ok(());
        }

        let loaded = load_encrypted_cookie_store(&self.cookie_file, secret)?;
        let new_store = loaded.lock().unwrap_or_else(|p| p.into_inner()).clone();
        *self.cookie_store.lock().unwrap_or_else(|p| p.into_inner()) = new_store;
        log::debug!(
            "已从磁盘重载 {} 个 cookies: {:?}",
            cookie_count(&self.cookie_store),
            self.cookie_file
        );
        Ok(())
    }

    /// 切换 reauth 方式（POST /reAuthCheck/changeReAuthType.do）。
    ///
    /// 服务端只接受本账号已开通的方式（未开通 → code:0 拒绝）；
    /// 切换成功后 `ctx` 的当前方式随之更新。
    pub fn change_reauth_type(&self, ctx: &mut ReauthContext, method: &ReauthMethod) -> Result<()> {
        let url = format!("{}/reAuthCheck/changeReAuthType.do", AUTH_SERVER_URL);
        let resp = self
            .client
            .post(&url)
            .header(header::REFERER, reauth::REAUTH_REFERER)
            .header("X-Requested-With", "XMLHttpRequest")
            .form(&reauth::change_form(ctx, method.id))
            .send()?;
        let body = resp.text()?;
        reauth::parse_change_response(&body)?;
        ctx.re_auth_type = method.id.to_string();
        log::info!("reauth 方式已切换: {}={}", method.id, method.name);
        Ok(())
    }

    /// 发送动态码（POST /dynamicCode/getDynamicCodeByReauth.do）。
    /// 仅动态码方式（短信/企微/邮箱/钉钉等）可用；发送成功后等待用户输入验证码。
    pub fn send_reauth_code(&self, ctx: &ReauthContext, method: &ReauthMethod) -> Result<()> {
        if method.kind() != ReauthMethodKind::DynamicCode {
            return Err(UestcClientError::ReauthFailed {
                message: format!(
                    "方式 {}={} 不是动态码方式，无需发码",
                    method.id, method.name
                ),
            });
        }
        let url = format!("{}/dynamicCode/getDynamicCodeByReauth.do", AUTH_SERVER_URL);
        let resp = self
            .client
            .post(&url)
            .header(header::REFERER, reauth::REAUTH_REFERER)
            .form(&reauth::send_code_form(ctx, method.id)?)
            .send()?;
        let body = resp.text()?;
        reauth::parse_send_code_response(&body)?;
        log::info!("验证码已发送（{}={}）", method.id, method.name);
        Ok(())
    }

    /// 提交 reauth 第二因素（POST /reAuthCheck/reAuthSubmit.do），成功后跟随 /login 链路拿 ST。
    ///
    /// 按方式填充凭证：
    /// - 动态码：`code` 为收到的验证码；
    /// - 密码：`password` 为明文密码（用 reauth 页自有 salt 加密）；
    /// - 微信扫码：忽略 `code`/`password`，内部走 `combinedLogin reAuth=2` 外部 OAuth
    ///   （终端二维码 + 轮询 + 回调），扫码动作需人工手机微信。
    ///
    /// `skip_tmp_reauth`：可信设备弹窗的值，false=仅本次 / true=信任此设备（服务端持久化指纹）。
    pub fn submit_reauth(
        &self,
        ctx: &ReauthContext,
        method: &ReauthMethod,
        code: Option<&str>,
        password: Option<&str>,
        skip_tmp_reauth: bool,
    ) -> Result<()> {
        // 服务端按 reAuthType 字段校验凭证：提交非当前方式必失败（reAuth_failed）。
        // 微信走外部 OAuth 不提交 reAuthType，无需先切换。
        if method.kind() != ReauthMethodKind::Wechat && ctx.re_auth_type != method.id.to_string() {
            return Err(UestcClientError::ReauthFailed {
                message: format!(
                    "方式 {}={} 不是当前 reauth 方式（当前 {}），请先调用 change_reauth_type",
                    method.id, method.name, ctx.re_auth_type
                ),
            });
        }

        // 成功后保存 cookie（`--reauth` 等调用方依赖落盘会话，供其他进程重载）
        let result = match method.kind() {
            ReauthMethodKind::Wechat => self.submit_wechat_reauth(ctx, skip_tmp_reauth),
            ReauthMethodKind::DynamicCode => {
                let form = reauth::submit_form(ctx, code, None, skip_tmp_reauth);
                self.submit_reauth_form(ctx, form)
            }
            ReauthMethodKind::Password => {
                let salt = ctx.pwd_encrypt_salt.as_deref().ok_or_else(|| {
                    UestcClientError::ReauthFailed {
                        message: "reauth 页未提供 pwdEncryptSalt，无法进行密码 reauth".to_string(),
                    }
                })?;
                let plain = password.ok_or_else(|| UestcClientError::ReauthFailed {
                    message: "密码 reauth 需要提供明文密码".to_string(),
                })?;
                let encrypted = core::crypto::encrypt_password(plain, salt)?;
                let form = reauth::submit_form(ctx, None, Some(encrypted), skip_tmp_reauth);
                self.submit_reauth_form(ctx, form)
            }
            ReauthMethodKind::Unsupported => Err(UestcClientError::ReauthFailed {
                message: format!("方式 {}={} 未实现", method.id, method.name),
            }),
        };
        if result.is_ok()
            && let Err(e) = self.save_cookie_store()
        {
            log::warn!("reauth 成功后保存 cookie 失败: {}", e);
        }
        result
    }

    /// reAuthSubmit.do 提交 + reauth 成功后跟随 /login 链路拿 ST。
    fn submit_reauth_form(
        &self,
        ctx: &ReauthContext,
        form: Vec<(&'static str, String)>,
    ) -> Result<()> {
        let url = format!("{}/reAuthCheck/reAuthSubmit.do", AUTH_SERVER_URL);
        let resp = self
            .client
            .post(&url)
            .header(header::REFERER, reauth::REAUTH_REFERER)
            .header("X-Requested-With", "XMLHttpRequest")
            .form(&form)
            .send()?;
        let body = resp.text()?;
        reauth::parse_submit_response(&body)?;
        log::info!("reauth 提交成功，跟随 /login 链路拿 ST");
        self.finish_to_service(ctx)
    }

    /// 微信扫码 reauth（type 8/16）：combinedLogin reAuth=2 → 微信 OAuth → 终端二维码
    /// → 轮询 → callback → /login 链路直接发 ST。扫码动作需人工手机微信。
    fn submit_wechat_reauth(&self, ctx: &ReauthContext, skip_tmp_reauth: bool) -> Result<()> {
        use crate::core::wechat;

        // Step 1: combinedLogin（reauth 版）→ 302 微信 OAuth
        let combined_url = format!(
            "{}/combinedLogin.do?type=weixin&reAuth=2&success={}&skipTmpReAuth={}",
            AUTH_SERVER_URL,
            urlencoding::encode(&ctx.service),
            skip_tmp_reauth
        );
        let resp = self
            .client
            .get(&combined_url)
            .header(header::REFERER, reauth::REAUTH_REFERER)
            .send()?;
        let wechat_auth_url = resp.url().to_string();
        if !wechat_auth_url.contains("open.weixin.qq.com") {
            return Err(UestcClientError::WeChatError {
                message: format!(
                    "combinedLogin(reauth) 未跳转到微信登录页，当前 URL: {}",
                    wechat_auth_url
                ),
            });
        }
        let params = wechat::WechatAuthParams::from_url(&wechat_auth_url)?;
        log::debug!("Target AppID: {}", params.appid);

        // Step 2: 二维码 uuid（f=xml 取 uuid，失败回退 HTML 提取）
        let xml_url = params.build_qr_xml_url();
        let resp = self.client.get(&xml_url).send()?;
        let xml_text = resp.text()?;
        let uuid = match wechat::parse_qr_uuid_from_xml(&xml_text) {
            Ok(uuid) => uuid,
            Err(xml_err) => {
                log::warn!("XML 取 uuid 失败，回退 HTML 提取: {}", xml_err);
                let html_url = params.build_qr_html_url();
                let resp = self.client.get(&html_url).send()?;
                let html = resp.text()?;
                wechat::parse_qr_uuid_from_html(&html).map_err(|_| xml_err)?
            }
        };

        // Step 3: 终端二维码（与登录版一致）
        wechat::display_qr_in_terminal(&uuid)?;

        // Step 4: 轮询扫码状态（同一套健壮性守卫）
        log::debug!("等待扫码（reauth）");
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
                    log::debug!("扫码确认 (405)");
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
                wechat::ScanStatus::Waiting => {}
                wechat::ScanStatus::Unknown(_) => {}
            }

            std::thread::sleep(wechat::POLL_INTERVAL);
        };

        // Step 5: callback → /login 链路直接发 ST
        log::debug!("正在回调验证");
        let callback_url = params.build_callback_url(&wx_code);
        let resp = self.client.get(&callback_url).send()?;
        let final_url = resp.url().to_string();
        let _ = resp.bytes()?;

        if is_reauth_url(&final_url) {
            log::warn!("微信 reauth 回调后仍落在 reauth 页，尝试跟随 /login 链路");
        }
        if final_url.contains("ticket=ST-")
            || (!final_url.contains("/authserver/login") && !is_reauth_url(&final_url))
        {
            log::info!("微信 reauth 完成（ST 已发放或已进业务页）");
            return Ok(());
        }
        self.finish_to_service(ctx)
    }

    /// reauth 完成后跟随 /login 链路（镜像分析仓库 finishToService）：
    /// - 跳转链直接发 ST（URL 含 ticket=ST-）或进入业务页 → 完成；
    /// - 停在登录页渲染（CAS 5.x 由 JS 自动提交近空表单）→ 手动 POST 近空表单换 ST。
    fn finish_to_service(&self, ctx: &ReauthContext) -> Result<()> {
        let mut login_url = format!("{}/login", AUTH_SERVER_URL);
        if !ctx.service.is_empty() {
            login_url.push_str("?service=");
            login_url.push_str(&urlencoding::encode(&ctx.service));
        }

        let resp = self.client.get(&login_url).send()?;
        let status = resp.status();
        let final_url = resp.url().to_string();
        let body = resp.text()?;

        if final_url.contains("ticket=ST-") {
            log::info!("reauth 完成，ST 已在跳转链中发放");
            return Ok(());
        }
        if status.is_success() && !final_url.contains("/authserver/login") {
            log::info!("reauth 完成，已进入业务系统: {}", final_url);
            return Ok(());
        }

        // 停在登录页渲染：CAS 5.x 用 JS 自动提交近空表单换 ST（wire 实测）
        log::debug!("reauth 后 /login 停在渲染页，POST 近空表单换取 ST");
        let execution = core::parser::extract_execution(&body).ok_or_else(|| {
            UestcClientError::ReauthFailed {
                message: "reauth 后 /login 链路未解析到 execution（页面结构可能已变）".to_string(),
            }
        })?;
        let form = reauth::near_empty_login_form(&execution);
        let resp = self.client.post(&login_url).form(&form).send()?;
        let final_url = resp.url().to_string();
        if (resp.status().is_redirection() || resp.status().is_success())
            && (final_url.contains("ticket=ST-") || !final_url.contains("/authserver/login"))
        {
            log::info!("reauth 完成，近空表单重登后进入业务系统");
            return Ok(());
        }
        Err(UestcClientError::ReauthFailed {
            message: "reauth 提交成功但 /login 链路未能发放 ST".to_string(),
        })
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

/// 判断 URL 是否落在 reauth 页（登录 POST 后 TGT 被锁的 302 落点）。
fn is_reauth_url(url: &str) -> bool {
    REAUTH_URL_MARKERS.iter().any(|m| url.contains(m))
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
