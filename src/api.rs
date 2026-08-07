use crate::config::{AppConfig, LoginType};
use reqwest::StatusCode;
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use uestc_client::{ReauthContext, ReauthMethod, ReauthMethodKind, UestcClient, UestcClientError};

const BASE_URL: &str = "https://online.uestc.edu.cn/site";

/// 记录 JSON 响应体片段时的截断长度。够看清上游返回了什么，又不会淹没日志。
const MAX_LOGGED_BODY: usize = 512;

/// 单个上游字段进日志的长度上限。
pub(crate) const MAX_LOGGED_FIELD: usize = 200;

/// 列出 `d` 的键名时最多列几个。
const MAX_LOGGED_KEYS: usize = 32;

/// 响应体读入内存的上限。正常载荷不到 1 KB；设上限是为了让上游返回异常
/// 巨大的响应时，诊断路径不会把自己变成内存放大器。
const MAX_BODY_BYTES: usize = 64 * 1024;

/// 判断响应体形态时只嗅探开头这么多字符，避免复制整个响应体。
const MAX_SNIFFED_CHARS: usize = 1024;

/// 需要人工完成 reauth（多因子二次认证）——无人值守环境（stdin 非终端）
/// 无法交互完成第二因素。`relogin` 用它区分"需要人工"与"登录失败"：
/// 前者进入等待模式（ReauthPending 通知 + 定期探测恢复），后者走既有
/// `LoginRetryFailure` 路径。
#[derive(Debug)]
pub(crate) struct ReauthPendingError;

impl fmt::Display for ReauthPendingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "需要人工完成二次认证（reauth），请在终端运行 `uestc-power-monitor --reauth`"
        )
    }
}

impl std::error::Error for ReauthPendingError {}

/// 读取一行终端输入（供 reauth 交互选择/输入验证码；config.rs 的凭据输入
/// 有同款实现，此处复用同一风格）。
fn prompt_line(prompt: &str) -> Result<String, String> {
    print!("{prompt}");
    io::stdout()
        .flush()
        .map_err(|e| format!("刷新终端输出失败: {e}"))?;
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("读取输入失败: {e}"))?;
    Ok(line.trim().to_string())
}

pub struct ApiService {
    client: UestcClient,
    config: AppConfig,
    login_throttle: LoginThrottle,
    /// 本轮取数周期内是否发生过重新登录失败（含被节流拒绝）。
    /// 由 `lib.rs` 在取数失败后读取并清除，用于区分"登录重试失败"
    /// 与"普通网络失败"——前者走 `LoginRetryFailure` 通知（一天一次），
    /// 后者仍计入 `ConsecutiveFetchFailures`。
    login_retry_failure: Mutex<bool>,
    /// 本轮是否命中"需要人工完成 reauth"（无人值守环境无法交互）。
    /// 由 `lib.rs` 读取并清除：命中后进入等待模式（ReauthPending 通知 +
    /// 定期重载 cookie 探测会话），而不是继续拿账号撞锁。
    reauth_pending: Mutex<bool>,
}

impl ApiService {
    pub async fn new(config: &AppConfig) -> Result<Self, Box<dyn std::error::Error>> {
        let user_display = config.username.as_deref().unwrap_or("unknown");
        debug!("Creating new API service for user: {}", user_display);
        let cookie_encryption_secret = config
            .cookie_encryption_secret()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let client = UestcClient::with_encrypted_cookie_file(
            &config.cookie_file,
            cookie_encryption_secret.as_bytes(),
        );

        let service = Self {
            client,
            config: config.clone(),
            login_throttle: LoginThrottle::new(),
            login_retry_failure: Mutex::new(false),
            reauth_pending: Mutex::new(false),
        };

        // 首次登录不受冷却限制，但要记进节流器，免得刚启动就立刻重登一次。
        service.login_throttle.claim(Instant::now(), LOGIN_COOLDOWN);
        service.login().await?;
        Ok(service)
    }

    async fn login(&self) -> Result<(), Box<dyn std::error::Error>> {
        debug!("Attempting login via {:?}", self.config.login_type);

        // cookie 会话仍有效时直接跳过登录——此时账号密码根本用不到
        // （`client.login()` 内部才会探测会话，本层若先取凭据会白白拦截）。
        if self.client.is_session_active().await {
            debug!("Cookie session is active, skipping login");
        } else {
            match self.config.login_type {
                LoginType::Password => {
                    let username = self.config.username.as_ref().ok_or_else(|| {
                        "Username required for password login; cookie 会话已失效且未配置账号，\
                         请设置 UPM_USERNAME / UPM_PASSWORD 或运行 `uestc-power-monitor --reauth` 恢复会话"
                            .to_string()
                    })?;
                    let password = self.config.password.as_ref().ok_or_else(|| {
                        "Password required for password login; cookie 会话已失效且未配置密码，\
                         请设置 UPM_USERNAME / UPM_PASSWORD 或运行 `uestc-power-monitor --reauth` 恢复会话"
                            .to_string()
                    })?;
                    match self.client.login(username, password).await {
                        Ok(()) => {}
                        Err(UestcClientError::ReauthRequired { context }) => {
                            warn!("账号需要二次认证（reauth），进入交互流程");
                            self.complete_reauth_interactive(*context).await?;
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
                LoginType::Wechat => {
                    self.client.wechat_login().await?;
                }
            }
            debug!("Login successful");
            self.clear_login_retry_failure();
        }

        // Initialize session with forced CAS authentication
        let init_url = "https://online.uestc.edu.cn/common/actionCasLogin?redirect_url=https://online.uestc.edu.cn/page/";
        debug!("Initializing session with CAS authentication...");
        self.client.get(init_url).send().await?;
        debug!("Session initialized");

        Ok(())
    }

    /// 交互式完成 reauth（多因子二次认证），仅适用于终端环境。
    ///
    /// 流程：列出服务端渲染的可用方式 → 用户选择 →
    /// 微信扫码（终端二维码，需手机微信）/ 动态码（发码后输入验证码）/ 密码。
    /// 标准输入不是终端时直接报错（无人值守场景由 `relogin` 识别为
    /// `ReauthPendingError`，进入等待人工模式）。
    async fn complete_reauth_interactive(
        &self,
        mut ctx: ReauthContext,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !io::stdin().is_terminal() {
            return Err(Box::new(ReauthPendingError));
        }

        let supported: Vec<ReauthMethod> = ctx
            .available_methods
            .iter()
            .filter(|m| m.is_supported())
            .cloned()
            .collect();
        if supported.is_empty() {
            return Err(format!(
                "账号的 reauth 方式均不受支持: {:?}",
                ctx.available_methods
                    .iter()
                    .map(|m| (m.id, m.name.as_str()))
                    .collect::<Vec<_>>()
            )
            .into());
        }

        let default_idx = supported.iter().position(|m| m.current).unwrap_or(0);
        println!("\n账号需要二次认证，可用方式：");
        for (i, m) in supported.iter().enumerate() {
            let mark = if i == default_idx { "（默认）" } else { "" };
            println!("  [{}] {}{}", i + 1, m.name, mark);
        }
        let choice = prompt_line(&format!(
            "选择编号(1-{}，回车=默认 {}): ",
            supported.len(),
            default_idx + 1
        ))?;
        let idx = if choice.trim().is_empty() {
            default_idx
        } else {
            let parsed = choice
                .trim()
                .parse::<usize>()
                .map_err(|_| "无效的选择编号".to_string())?;
            parsed
                .checked_sub(1)
                .filter(|i| *i < supported.len())
                .ok_or_else(|| "选择编号超出范围".to_string())?
        };
        let method = supported[idx].clone();
        let trust = self.config.reauth_trust_device;

        match method.kind() {
            ReauthMethodKind::Wechat => {
                info!("微信扫码 reauth：二维码将显示在终端，请用手机微信扫一扫");
                self.client
                    .submit_reauth(&ctx, &method, None, None, trust)
                    .await?;
            }
            ReauthMethodKind::DynamicCode => {
                if ctx.current_type_id() != method.id {
                    self.client.change_reauth_type(&mut ctx, &method).await?;
                }
                self.client.send_reauth_code(&ctx, &method).await?;
                let code = prompt_line("请输入收到的验证码: ")?;
                self.client
                    .submit_reauth(&ctx, &method, Some(&code), None, trust)
                    .await?;
            }
            ReauthMethodKind::Password => {
                let password = rpassword::prompt_password("请输入登录密码（用于 reauth 验证）: ")
                    .map_err(|e| format!("读取密码输入失败: {e}"))?;
                self.client
                    .submit_reauth(&ctx, &method, None, Some(&password), trust)
                    .await?;
            }
            ReauthMethodKind::Unsupported => unreachable!("已过滤不支持的方式"),
        }
        info!("reauth 完成，会话已就绪");
        Ok(())
    }

    /// 等待人工 reauth 期间的探测：从磁盘重载 cookie 文件（`--reauth`
    /// 子命令写入的新会话），再探测业务会话。恢复后调用方继续监控。
    pub(crate) async fn probe_recovered_session(&self) -> bool {
        if let Err(e) = self.client.reload_cookie_file() {
            warn!("重载 cookie 文件失败: {}", e);
            return false;
        }
        self.check_session().await
    }

    /// 受冷却保护的重新登录。冷却期内直接报错，让这一轮取数失败，
    /// 而不是继续往统一身份认证上撞。
    async fn relogin(&self) -> Result<(), Box<dyn std::error::Error>> {
        if !self.login_throttle.claim(Instant::now(), LOGIN_COOLDOWN) {
            self.note_login_retry_failure();
            return Err(format!(
                "skipping re-login: another login attempt happened less than {:?} ago",
                LOGIN_COOLDOWN
            )
            .into());
        }
        match self.login().await {
            Ok(()) => Ok(()),
            Err(e) => {
                // 登录成功会由 `login()` 清除标志；失败则在此置位，
                // 供 `lib.rs` 区分本轮失败是否由登录引起。
                if e.is::<ReauthPendingError>() {
                    self.note_reauth_pending();
                } else {
                    self.note_login_retry_failure();
                }
                Err(e)
            }
        }
    }

    fn note_reauth_pending(&self) {
        *self
            .reauth_pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
    }

    /// 读取并清除"本轮命中 reauth 等待人工"标志。
    ///
    /// `lib.rs` 在取数失败后调用：返回 `true` 说明需要进入等待人工模式
    /// （`ReauthPending` 通知 + 定期重载 cookie 探测会话恢复），
    /// 而不是继续计入 `LoginRetryFailure`。
    pub fn take_reauth_pending(&self) -> bool {
        let mut flag = self
            .reauth_pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::take(&mut *flag)
    }

    fn note_login_retry_failure(&self) {
        *self
            .login_retry_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
    }

    fn clear_login_retry_failure(&self) {
        *self
            .login_retry_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = false;
    }

    /// 读取并清除"本轮发生过重新登录失败"标志。
    ///
    /// `lib.rs` 在取数失败（`retry` 耗尽）后调用：返回 `true` 说明这轮失败
    /// 由会话失效后的重登失败引起，应计入 `LoginRetryFailure` 通知；
    /// 返回 `false` 则是普通网络/上游失败，仍计入 `ConsecutiveFetchFailures`。
    pub fn take_login_retry_failure(&self) -> bool {
        let mut flag = self
            .login_retry_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::take(&mut *flag)
    }

    async fn check_session(&self) -> bool {
        debug!("Checking session validity...");
        let url = "https://online.uestc.edu.cn/common/getLanguageTypes.htl";
        match self.client.post(url).send().await {
            Ok(resp) => match resp.json::<SessionCheckResponse>().await {
                Ok(data) => {
                    let is_valid = data.success;
                    debug!("Session check result: valid={}", is_valid);
                    is_valid
                }
                Err(e) => {
                    // 同样经 `FetchError::transport` 剥掉 URL：这个请求也可能被
                    // CAS 重定向，最终 URL 会带上 ticket。
                    debug!(
                        "Failed to parse session check response: {}",
                        FetchError::transport(e)
                    );
                    false
                }
            },
            Err(e) => {
                debug!("Session check request failed: {}", FetchError::transport(e));
                false
            }
        }
    }

    fn power_data_request(&self, url: &str) -> reqwest::RequestBuilder {
        self.client
            .get(url)
            .header("Referer", "https://online.uestc.edu.cn/page/")
            .header("Accept", "application/json, text/plain, */*")
    }

    /// 发一次请求并解析响应体。
    ///
    /// 关键点是先把响应体读出来再解析，而不是直接用 `resp.json()`：后者失败时
    /// 只会给出 "error decoding response body"，上游到底返回了什么全部丢失。
    async fn fetch_power_response(&self, url: &str) -> Result<PowerResponse, FetchError> {
        let mut resp = self
            .power_data_request(url)
            .send()
            .await
            .map_err(FetchError::transport)?;

        let status = resp.status();
        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();

        // 读取响应体本身也可能失败（连接中断、超时），那属于传输问题。
        let body = match read_body_capped(&mut resp, MAX_BODY_BYTES)
            .await
            .map_err(FetchError::transport)?
        {
            CappedBody::Complete(body) => body,
            CappedBody::TooLarge { read } => {
                return Err(FetchError::TooLarge {
                    status,
                    content_type,
                    read,
                });
            }
        };

        let text = String::from_utf8_lossy(&body);
        classify_body(status, &content_type, body.len(), &text)
    }

    pub async fn fetch_data(&self) -> Result<Option<PowerInfo>, Box<dyn std::error::Error>> {
        let url = format!("{}/bedroom", BASE_URL);
        debug!("Fetching power data from: {}", url);

        // 请求失败时先探测会话，必要时重新登录再试一次
        let mut resp = match self.fetch_power_response(&url).await {
            Ok(resp) => resp,
            Err(e) if e.session_may_be_stale() => {
                warn!("Power data request failed: {}. Checking session...", e);
                if self.check_session().await {
                    return Err(e.into());
                }
                debug!("Session invalid, re-login and retry...");
                self.relogin()
                    .await
                    .inspect_err(|e| warn!("Re-login failed: {}", e))?;
                self.fetch_power_response(&url)
                    .await
                    .inspect_err(|e| warn!("Power data request failed after re-login: {}", e))?
            }
            Err(e) => {
                // 每次尝试都记一条，而且是 warn 级别：默认日志过滤器是 info，
                // 而 `retry` 只会把最后一次错误交给调用方，中间尝试的失败原因
                // 否则会彻底丢失——而间歇性故障恰恰要看各次尝试的差异。
                // `FetchError` 的 Display 本身已按脱敏规则构造，可安全落盘。
                warn!("Power data request failed: {}", e);
                return Err(e.into());
            }
        };

        debug!(
            "API response: error={}, message={}",
            resp.body.error,
            truncate_for_log(&resp.body.message, MAX_LOGGED_FIELD)
        );

        // 会话失效后重新登录并重试一次。
        if should_relogin(&resp) {
            warn!(
                "Session expired (HTTP {}, error={}, message='{}'). Re-logging in...",
                resp.status,
                resp.body.error,
                truncate_for_log(&resp.body.message, MAX_LOGGED_FIELD)
            );
            self.relogin()
                .await
                .inspect_err(|e| warn!("Re-login failed: {}", e))?;
            resp = self
                .fetch_power_response(&url)
                .await
                .inspect_err(|e| warn!("Power data request failed after re-login: {}", e))?;
            debug!(
                "Retry API response: error={}, message={}",
                resp.body.error,
                truncate_for_log(&resp.body.message, MAX_LOGGED_FIELD)
            );
        }

        if let Some(ref data) = resp.body.data {
            info!(
                "Power info received: room={}, money={:.2}, energy={:.2}",
                truncate_for_log(&data.room_display_name, MAX_LOGGED_FIELD),
                data.remaining_money,
                data.remaining_energy
            );
        } else {
            warn!(
                "API returned no data - error_code={}, message='{}', url='{}'. This usually means: 1) No room is bound to your account, 2) Session expired, or 3) API service issue",
                resp.body.error,
                truncate_for_log(&resp.body.message, MAX_LOGGED_FIELD),
                url
            );
        }

        Ok(resp.body.data)
    }
}

/// 两次登录尝试之间的最小间隔。
///
/// 会话失效时每个取数尝试都可能触发重登，而取数本身还被 `retry` 包了 3 次，
/// 于是一个轮询周期最多能打出 6 次统一身份认证登录。校园 SSO 普遍会对反复
/// 认证限流甚至锁定账号——宁可这一轮取不到数，也不该拿账号去撞锁。
///
/// 两点边界要清楚：
/// - 失败的登录同样占用配额（`claim` 在 `login()` 之前）。这是刻意的：锁定
///   正是由反复失败的认证触发的，代价是 CAS 偶发抖动会让恢复推迟到下一轮。
/// - 配额是每个 `ApiService` 实例的，不是每进程的。启动时 `lib.rs` 会重建
///   实例重试，因此启动阶段仍可能连续登录数次——那是需要的行为。
/// - 冷却固定 60s，与可配置的 `interval_seconds` 无关：轮询间隔远小于 60s 时，
///   会话失效后的恢复最多仍要等 60s。
const LOGIN_COOLDOWN: Duration = Duration::from_secs(60);

/// 登录节流器：记录上次尝试时间，冷却期内拒绝再次登录。
struct LoginThrottle {
    last_attempt: Mutex<Option<Instant>>,
}

impl LoginThrottle {
    fn new() -> Self {
        Self {
            last_attempt: Mutex::new(None),
        }
    }

    /// 申请一次登录机会。允许则记下时间并返回 true。
    fn claim(&self, now: Instant, cooldown: Duration) -> bool {
        // 锁只在这里短暂持有，不跨 await，避免阻塞其他任务。
        let mut last_attempt = self
            .last_attempt
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match *last_attempt {
            Some(previous) if now.duration_since(previous) < cooldown => false,
            _ => {
                *last_attempt = Some(now);
                true
            }
        }
    }
}

/// 拿到一个能解析的响应后，是否还需要重新登录。
///
/// 两个信号都要看：信封里的 `e=401`，以及 HTTP 层的 401/403——后者可能配着
/// 一个能正常解析的信封，只认前者会让监控一直卡在死会话上。
///
/// 但 HTTP 层的信号只在**没拿到数据**时才算数：状态码异常而信封里确实带着
/// 读数时，为了重登把这条读数丢掉，比直接用它更糟。
fn should_relogin(resp: &PowerResponse) -> bool {
    resp.body.error == 401 || (session_expired_status(resp.status) && resp.body.data.is_none())
}

/// HTTP 层是否在说"你没登录"。3xx 不会出现在这里：reqwest 默认跟随重定向，
/// 拿到的状态码总是最后一跳。
fn session_expired_status(status: StatusCode) -> bool {
    matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
}

/// 一次成功解析出的响应：业务信封，外加它到达时的 HTTP 状态码。
///
/// 状态码要一路带到调用方：网关可能用 HTTP 401 配一个能正常解析的信封，
/// 只看信封里的 `e` 会漏掉这种会话失效，监控就会一直卡在死会话上。
#[derive(Debug)]
struct PowerResponse {
    status: StatusCode,
    body: ApiResponse<PowerInfo>,
}

enum CappedBody {
    Complete(Vec<u8>),
    /// 超过上限，已停止读取；`read` 是放弃之前累计看到的字节数。
    TooLarge {
        read: usize,
    },
}

/// 分块读取响应体并设上限。
///
/// 不用 `bytes()`：它会把整个响应体无条件收进内存，上游一旦返回异常巨大的
/// 响应（例如错误页），诊断路径本身就会变成内存放大器。超限就地放弃，
/// 剩余数据连同这条连接一起丢弃。
async fn read_body_capped(
    resp: &mut reqwest::Response,
    cap: usize,
) -> Result<CappedBody, reqwest::Error> {
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if body.len() + chunk.len() > cap {
            return Ok(CappedBody::TooLarge {
                read: body.len() + chunk.len(),
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(CappedBody::Complete(body))
}

/// 按响应体的**实际可解析性**（而不是 Content-Type）分类。
///
/// 这条区分是安全边界：只有真正能解析成 JSON 的内容才允许进日志。CAS 登录页
/// 带着 `execution` ticket 和 CSRF token，而上游/网关完全可能在返回登录页时
/// 仍然声明 `Content-Type: application/json`——若以声明的类型作为脱敏依据，
/// 一个伪造的头部就能把 ticket 送进日志。能解析成 JSON 的 HTML 并不存在。
fn classify_body(
    status: StatusCode,
    content_type: &str,
    byte_len: usize,
    text: &str,
) -> Result<PowerResponse, FetchError> {
    // 先确定响应体是不是 JSON（决定 Shape 与 Status/NotJson 的边界）。
    let whole: serde_json::Value = match serde_json::from_str(text) {
        Ok(value) => value,
        // 根本不是 JSON：只报形态，绝不 dump 内容。
        Err(_) if !status.is_success() => {
            return Err(FetchError::Status {
                status,
                body: describe_opaque_body(content_type, byte_len, text),
            });
        }
        Err(_) => {
            return Err(FetchError::NotJson {
                status,
                body: describe_opaque_body(content_type, byte_len, text),
            });
        }
    };

    // 再按宽松信封解析（`d` 暂不校验类型），再按 `d` 的形态分流：对象 →
    // PowerInfo；null → 无数据；字符串 → 上游业务失败（如 "失败{...}"）。
    // 状态码非 2xx 也先按业务信封解析：上游会用 5xx 包着 {"e":401,...}，
    // 那种情况必须走到 `resp.error == 401` 的重新登录逻辑上去。
    let ApiResponse {
        error,
        message,
        data,
    } = match serde_json::from_value::<ApiResponse<serde_json::Value>>(whole.clone()) {
        Ok(envelope) => envelope,
        // 结构不符但确实是 JSON：内容可安全入日志，且重新序列化会转义控制字符，
        // 顺带挡掉借响应体伪造日志行（CRLF 注入）的可能。
        Err(detail) => {
            return Err(FetchError::Shape {
                status,
                envelope: describe_envelope(&whole),
                detail: detail.to_string(),
                snippet: json_snippet(&whole),
            });
        }
    };

    match data.as_ref() {
        // 无数据（如未绑定房间），交给调用方决定。
        None => Ok(PowerResponse {
            status,
            body: ApiResponse {
                error,
                message,
                data: None,
            },
        }),
        // 对象 → 按 PowerInfo 严格解析（核心电量字段缺失宁可报错也不编造）。
        Some(serde_json::Value::Object(_)) => {
            match serde_json::from_value::<PowerInfo>(data.unwrap()) {
                Ok(info) => Ok(PowerResponse {
                    status,
                    body: ApiResponse {
                        error,
                        message,
                        data: Some(info),
                    },
                }),
                Err(detail) => Err(FetchError::Shape {
                    status,
                    envelope: describe_envelope(&whole),
                    detail: detail.to_string(),
                    snippet: json_snippet(&whole),
                }),
            }
        }
        // 字符串 → 上游业务失败（见 `describe_business_payload`）。
        Some(serde_json::Value::String(payload)) => Err(FetchError::Business {
            status,
            message,
            payload: describe_business_payload(&payload),
        }),
        // 其它类型（数字/数组/bool）→ 结构不符。
        Some(other) => Err(FetchError::Shape {
            status,
            envelope: describe_envelope(&whole),
            detail: format!("d is {} instead of an object", json_type_name(&other)),
            snippet: json_snippet(&whole),
        }),
    }
}

/// 取电数据失败的分类。分类的意义在于区分"值得重新登录"和"上游返回了坏数据"。
#[derive(Debug)]
enum FetchError {
    /// 传输层失败：建连、超时、连接中断。
    Transport {
        host: Option<String>,
        source: reqwest::Error,
    },
    /// HTTP 状态码非 2xx，且响应体不是可用的业务信封。
    Status { status: StatusCode, body: String },
    /// 响应体不是 JSON，通常是被 CAS 重定向到了登录页。
    NotJson { status: StatusCode, body: String },
    /// 是 JSON，但结构与预期不符（上游降级返回、字段缺失或类型变化）。
    Shape {
        status: StatusCode,
        envelope: String,
        detail: String,
        snippet: String,
    },
    /// 信封正常（e/m 可解析），但业务层明确返回了失败：`d` 是字符串而非
    /// 数据对象。UESTC 电费网关业务失败时返回 `"d":"失败{...}"` —— "失败"
    /// 前缀后跟着一段携带诊断信息的 JSON（房间号、防重放时间戳、内网直连
    /// 重试地址、res_hash）。不是会话问题（信封 `e=0`），重登无济于事；
    /// 缺失的读数照常计入连续抓取失败，恢复依赖下一轮轮询。
    Business {
        status: StatusCode,
        /// 信封的 `m`（业务消息，反序列化时已清理控制字符）。
        message: String,
        /// `d` 字符串的脱敏诊断摘要（房间号/重试地址等，不含密文与签名材料）。
        payload: String,
    },
    /// 响应体超过 `MAX_BODY_BYTES`，已放弃读取。
    TooLarge {
        status: StatusCode,
        content_type: String,
        read: usize,
    },
}

impl FetchError {
    /// 构造传输错误，并在此处剥掉 URL。
    ///
    /// `reqwest::Error` 的 Display 会把完整 URL（含 query）打出来，而重定向后
    /// 的失败 URL 可能带着 CAS 的 `?ticket=ST-...`。日志是长期留存的，凭据不能
    /// 进去；主机名足够定位问题，就只留主机名。
    fn transport(source: reqwest::Error) -> Self {
        let host = source
            .url()
            .and_then(|url| url.host_str())
            .map(str::to_string);
        FetchError::Transport {
            host,
            source: source.without_url(),
        }
    }

    /// 是否可能是会话失效导致的，值得探测会话并重新登录。
    fn session_may_be_stale(&self) -> bool {
        match self {
            // `TooLarge` 也算：正常载荷不到 1 KB，超过 64 KiB 几乎只可能是
            // 带内联脚本样式的登录页/错误页，也就是 `NotJson` 那类会话信号。
            // 有节流兜底，猜错的代价上限是每 60s 多一次登录；猜对则避免了
            // 监控在超大登录页上永远卡死。
            FetchError::Transport { .. }
            | FetchError::NotJson { .. }
            | FetchError::TooLarge { .. } => true,
            // 只认 401/403：reqwest 默认跟随重定向，`status()` 拿到的总是
            // 最后一跳，因此 3xx 不会出现在这里；5xx 是上游故障而非会话问题。
            //
            // `Shape` 同样看状态码：200 下结构不符说明请求到达了业务接口，
            // 重新登录改变不了上游返回的内容，只会白跑一次登录流程；但 401/403
            // 时状态码本身就是鉴权信号，不该因为错误页恰好是 JSON（而不是
            // HTML）就走上相反的恢复路径。
            FetchError::Status { status, .. } | FetchError::Shape { status, .. } => {
                session_expired_status(*status)
            }
            // `Business` 是明确的业务失败语义，但 HTTP 401/403 配业务失败时
            // 状态码本身就是鉴权信号，与 `Shape` 同规则处理。
            FetchError::Business { status, .. } => session_expired_status(*status),
        }
    }
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Transport { host, source } => write!(
                f,
                "transport error contacting {}: {}",
                host.as_deref().unwrap_or("unknown host"),
                source
            ),
            FetchError::Status { status, body } => {
                write!(f, "unexpected HTTP status {} ({})", status, body)
            }
            FetchError::NotJson { status, body } => write!(
                f,
                "response body is not JSON (HTTP {}, {}) — session was probably redirected to the login page",
                status, body
            ),
            FetchError::Shape {
                status,
                envelope,
                detail,
                snippet,
            } => write!(
                f,
                "response JSON did not match the expected shape (HTTP {}): {} [{}] body={}",
                status, detail, envelope, snippet
            ),
            FetchError::Business {
                status,
                message,
                payload,
            } => write!(
                f,
                "electricity service reported a business failure (HTTP {}, m=\"{}\"): d is a string ({})",
                status,
                truncate_for_log(message, MAX_LOGGED_FIELD),
                payload
            ),
            FetchError::TooLarge {
                status,
                content_type,
                read,
            } => write!(
                f,
                "response body exceeded {} bytes (HTTP {}, content_type='{}', read {} bytes before giving up, content not logged)",
                MAX_BODY_BYTES,
                status,
                truncate_for_log(content_type, MAX_LOGGED_FIELD),
                read
            ),
        }
    }
}

impl std::error::Error for FetchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FetchError::Transport { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// 已解析 JSON 的截断片段。
///
/// 只接受 `Value`（而不是原始文本）有两个用处：调用点无法绕过"必须先解析成功"
/// 这一前提，且重新序列化会把控制字符转义，日志行不可能被响应体拆断。
fn json_snippet(value: &serde_json::Value) -> String {
    let rendered = redact_sensitive(value).to_string();
    truncate_for_log(&rendered, MAX_LOGGED_BODY)
}

pub(crate) fn truncate_for_log(rendered: &str, limit: usize) -> String {
    let mut out: String = rendered.chars().take(limit).collect();
    if rendered.chars().count() > limit {
        out.push_str("...<truncated>");
    }
    out
}

/// 纵深防御：日志是长期留存的，遇到疑似凭据的键名或值一律抹掉再落盘，免得
/// 上游哪天在错误响应里回显了 token。
///
/// 这不是完备的脱敏——键名匹配挡不住藏在任意键下的凭据，值匹配也只认几种
/// 已知格式。真正的边界是"只有能解析成 JSON 的内容才允许进日志"，这里只是
/// 在那之上多加一层。
fn redact_sensitive(value: &serde_json::Value) -> serde_json::Value {
    const SENSITIVE_KEY_PARTS: [&str; 10] = [
        "token",
        "ticket",
        "execution",
        "csrf",
        "secret",
        "password",
        "passwd",
        "cookie",
        "session",
        "authorization",
    ];

    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(key, val)| {
                    let lowered = key.to_ascii_lowercase();
                    if SENSITIVE_KEY_PARTS
                        .iter()
                        .any(|needle| lowered.contains(needle))
                    {
                        (key.clone(), serde_json::Value::String("<redacted>".into()))
                    } else {
                        (key.clone(), redact_sensitive(val))
                    }
                })
                .collect(),
        ),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(redact_sensitive).collect())
        }
        serde_json::Value::String(text) if value_looks_secret(text) => {
            serde_json::Value::String("<redacted>".into())
        }
        other => other.clone(),
    }
}

/// 按形态识别几种常见凭据，兜住藏在无害键名下的值（例如 `{"redirect": "...?ticket=ST-..."}`）。
fn value_looks_secret(text: &str) -> bool {
    const SECRET_VALUE_MARKERS: [&str; 7] = [
        "ticket=",
        "jsessionid=",
        "access_token=",
        "execution=",
        "ST-",
        "TGT-",
        "Bearer ",
    ];
    let lowered = text.to_ascii_lowercase();
    SECRET_VALUE_MARKERS
        .iter()
        .any(|marker| lowered.contains(&marker.to_ascii_lowercase()))
        || text.starts_with("eyJ")
}

/// 描述无法解析的响应体：只给体积和形态判断，绝不 dump 内容。
///
/// 非 JSON 的响应体最可能就是登录页，里面有 `execution` ticket 和 CSRF token。
fn describe_opaque_body(content_type: &str, byte_len: usize, text: &str) -> String {
    // 只在开头一小段上做形态判断，避免为了认出一个 HTML 页面而复制整个响应体。
    let head: String = text
        .chars()
        .take(MAX_SNIFFED_CHARS)
        .flat_map(char::to_lowercase)
        .collect();
    let hint = if head.contains("authserver") || head.contains("casloginform") {
        "looks like the CAS login page"
    } else if head.trim_start().starts_with("<!doctype") || head.contains("<html") {
        "looks like an HTML page"
    } else if text.trim().is_empty() {
        "empty body"
    } else {
        "unrecognized non-JSON body"
    };
    format!(
        "content_type='{}', {} bytes, {} (content not logged)",
        truncate_for_log(content_type, MAX_LOGGED_FIELD),
        byte_len,
        hint
    )
}

/// 从结构不符的 JSON 里尽力挖出上游给的错误信息。
///
/// 上游降级时会返回形如 `{"e":0,"m":"操作成功","d":{"retcode":-1,"msg":"查询失败"}}`
/// 的载荷，`d` 里缺少电量字段。这个函数把 `e`/`m`/`d.retcode`/`d.msg` 以及 `d`
/// 的实际键名提取出来，这才是判断"上游坏了"还是"字段改名了"的依据。
///
/// 所有取自上游的字符串都经 `Value` 序列化转义并单独限长：只给 `json_snippet`
/// 限长是不够的，否则一个超长的 `m` 就能把整行日志撑爆。
fn describe_envelope(value: &serde_json::Value) -> String {
    let serde_json::Value::Object(envelope) = value else {
        return format!(
            "unrecognized envelope, top level is {}",
            json_type_name(value)
        );
    };

    let mut parts = Vec::new();
    if let Some(e) = envelope.get("e") {
        parts.push(format!("e={}", bounded_field(e)));
    }
    if let Some(m) = envelope.get("m") {
        parts.push(format!("m={}", bounded_field(m)));
    }

    match envelope.get("d") {
        None | Some(serde_json::Value::Null) => parts.push("d=null".to_string()),
        Some(serde_json::Value::Object(data)) => {
            if let Some(retcode) = data.get("retcode") {
                parts.push(format!("d.retcode={}", bounded_field(retcode)));
            }
            if let Some(msg) = data.get("msg") {
                parts.push(format!("d.msg={}", bounded_field(msg)));
            }
            // 键名同样是上游可控的字符串，必须转义后再拼接。
            let keys: Vec<String> = data
                .keys()
                .take(MAX_LOGGED_KEYS)
                .map(|key| bounded_field(&serde_json::Value::String(key.clone())))
                .collect();
            let elided = data.len().saturating_sub(keys.len());
            if elided == 0 {
                parts.push(format!("d.keys=[{}]", keys.join(",")));
            } else {
                parts.push(format!("d.keys=[{},+{} more]", keys.join(","), elided));
            }
        }
        Some(other) => parts.push(format!("d is {}", json_type_name(other))),
    }

    parts.join(", ")
}

/// 单个字段的日志表示：JSON 序列化（转义控制字符）后限长。
fn bounded_field(value: &serde_json::Value) -> String {
    truncate_for_log(&value.to_string(), MAX_LOGGED_FIELD)
}

fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a bool",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// `d` 为字符串时的脱敏诊断摘要。
///
/// 识别 `"失败{...}"` 形态：剥离前缀后按 JSON 解析，只提取诊断字段（房间号、
/// 重试地址的主机与端口、res_hash）。str3/str4/sign/data 是服务端生成的一次性
/// 密文与签名材料，不进日志。不是该形态的字符串只报形态与长度，不 dump 内容。
fn describe_business_payload(raw: &str) -> String {
    let inner = raw.strip_prefix("失败").unwrap_or(raw);
    let value: serde_json::Value = match serde_json::from_str(inner) {
        Ok(value) => value,
        // 不是 "失败{json}" 形态：只报形态与长度，不 dump 内容。
        Err(_) => return format!("{} chars of non-JSON text", raw.chars().count()),
    };

    let serde_json::Value::Object(map) = &value else {
        return format!("non-object payload ({})", json_type_name(&value));
    };

    let mut parts = Vec::new();
    // 房间标识：fjh=门牌号，dffjbh=控电房间编号。
    if let (Some(fjh), Some(dffjbh)) = (map.get("fjh"), map.get("dffjbh")) {
        parts.push(format!(
            "room {}/{}",
            bounded_field(fjh),
            bounded_field(dffjbh)
        ));
    }
    // 重试地址只留主机[:端口] —— query 里的 sign/data 是签名材料，不进日志。
    if let Some(url) = map.get("url").and_then(|v| v.as_str()) {
        let authority = url
            .split("://")
            .nth(1)
            .and_then(|rest| rest.split(['/', '?', '#']).next())
            .filter(|a| !a.is_empty());
        if let Some(authority) = authority {
            parts.push(format!(
                "retry endpoint {}",
                truncate_for_log(authority, MAX_LOGGED_FIELD)
            ));
        }
    }
    if let Some(res_hash) = map.get("res_hash") {
        parts.push(format!("res_hash={}", bounded_field(res_hash)));
    }

    if parts.is_empty() {
        "no diagnostic fields".to_string()
    } else {
        parts.join(", ")
    }
}

/// 房间信息字段缺失时的占位值，避免因为附属字段缺失丢掉一次有效读数。
const UNKNOWN_FIELD: &str = "unknown";

fn unknown_field() -> String {
    UNKNOWN_FIELD.to_string()
}

/// 电量载荷。
///
/// 只有 `sydl`/`syje`/`roomName` 是必需的：前两者是监控的核心数据，缺失时宁可
/// 报错重试也不能编造；其余字段是附属元数据，缺一个不该让整条读数作废。
#[derive(Debug, Deserialize, Serialize)]
pub struct PowerInfo {
    /// retcode: 返回代码
    #[serde(rename = "retcode", default)]
    pub code: i32,

    /// msg: 消息提示
    #[serde(
        rename = "msg",
        default,
        deserialize_with = "deserialize_sanitized_string"
    )]
    pub message: String,

    /// sydl: 剩余电量 (Remaining Energy - kWh)
    /// 兼容字符串和数字类型（例如 "26.91" 或 26.91）
    #[serde(rename = "sydl", deserialize_with = "deserialize_f64_lossy")]
    pub remaining_energy: f64,

    /// syje: 剩余金额 (Remaining Money - CNY)
    /// 兼容字符串和数字类型（例如 "14.44" 或 14.44）
    #[serde(rename = "syje", deserialize_with = "deserialize_f64_lossy")]
    pub remaining_money: f64,

    /// dffjbh: 控电房间编号 (Meter Room ID for Utility System)
    #[serde(rename = "dffjbh", default = "unknown_field")]
    pub meter_room_id: String,

    /// roomName: 房间显示名称 (e.g., "220407")
    /// 通知与入库都以它标识房间，因此保持必需。
    #[serde(rename = "roomName", deserialize_with = "deserialize_sanitized_string")]
    pub room_display_name: String,

    /// roomId: 房间逻辑ID (Database ID)
    #[serde(rename = "roomId", default = "unknown_field")]
    pub room_id: String,

    /// buiId: 楼栋ID (Building ID)
    #[serde(rename = "buiId", default = "unknown_field")]
    pub building_id: String,

    /// areaid: 校区ID (Campus/Area ID)
    #[serde(rename = "areaid", default = "unknown_field")]
    pub campus_id: String,

    /// fjh: 门牌号 (e.g., "407")
    #[serde(rename = "fjh", default = "unknown_field")]
    pub room_number: String,
}

/// 去掉上游字符串里的控制字符。
///
/// 这些字段会进日志、通知和数据库。一个裸换行就足以在日志里伪造出一条完整的
/// 记录（日志伪造），而房间名和提示语里的控制字符没有任何正当含义。在反序列化
/// 处统一清理，比要求每个使用点各自记得转义要可靠——使用点只需再限长。
fn deserialize_sanitized_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(sanitize_control_chars(&raw))
}

fn sanitize_control_chars(raw: &str) -> String {
    raw.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn deserialize_f64_lossy<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum NumberOrString {
        Number(f64),
        String(String),
    }

    match NumberOrString::deserialize(deserializer)? {
        NumberOrString::Number(n) => Ok(n),
        NumberOrString::String(s) => s.trim().parse::<f64>().map_err(serde::de::Error::custom),
    }
}

#[derive(Debug, Deserialize)]
pub struct ApiResponse<T> {
    #[serde(rename = "e")]
    pub error: i32,

    #[serde(rename = "m", deserialize_with = "deserialize_sanitized_string")]
    pub message: String,

    #[serde(rename = "d")]
    pub data: Option<T>,
}

#[derive(Debug, Deserialize)]
struct SessionCheckResponse {
    success: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_json_with_values(sydl: &str, syje: &str) -> String {
        format!(
            r#"{{
                "e": 0,
                "m": "ok",
                "d": {{
                    "retcode": 0,
                    "msg": "ok",
                    "sydl": {},
                    "syje": {},
                    "dffjbh": "meter-room-id",
                    "roomName": "220407",
                    "roomId": "room-id",
                    "buiId": "building-id",
                    "areaid": "campus-id",
                    "fjh": "407"
                }}
            }}"#,
            sydl, syje
        )
    }

    #[test]
    fn power_info_deserializes_numeric_strings() {
        let json = sample_json_with_values(r#""26.91""#, r#""14.44""#);
        let resp: ApiResponse<PowerInfo> = serde_json::from_str(&json).expect("parse response");
        let data = resp.data.expect("response data should exist");
        assert!((data.remaining_energy - 26.91).abs() < f64::EPSILON);
        assert!((data.remaining_money - 14.44).abs() < f64::EPSILON);
    }

    #[test]
    fn power_info_deserializes_numeric_values() {
        let json = sample_json_with_values("26.91", "14.44");
        let resp: ApiResponse<PowerInfo> = serde_json::from_str(&json).expect("parse response");
        let data = resp.data.expect("response data should exist");
        assert!((data.remaining_energy - 26.91).abs() < f64::EPSILON);
        assert!((data.remaining_money - 14.44).abs() < f64::EPSILON);
    }

    #[test]
    fn power_info_rejects_non_numeric_values() {
        let json = sample_json_with_values(r#""abc""#, r#""14.44""#);
        let err = serde_json::from_str::<ApiResponse<PowerInfo>>(&json).expect_err("should fail");
        assert!(
            err.to_string().contains("invalid float literal"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn power_info_survives_missing_incidental_metadata() {
        // 只缺附属字段时，读数仍应保留 —— 否则一次电量数据会因为无关字段作废。
        let json = r#"{
            "e": 0,
            "m": "ok",
            "d": {"sydl": "26.91", "syje": "14.44", "roomName": "220407"}
        }"#;
        let resp: ApiResponse<PowerInfo> = serde_json::from_str(json).expect("parse response");
        let data = resp.data.expect("response data should exist");
        assert!((data.remaining_energy - 26.91).abs() < f64::EPSILON);
        assert_eq!(data.room_display_name, "220407");
        assert_eq!(data.room_number, UNKNOWN_FIELD);
        assert_eq!(data.meter_room_id, UNKNOWN_FIELD);
        assert_eq!(data.code, 0);
    }

    #[test]
    fn power_info_still_requires_the_core_readings() {
        // 核心电量字段缺失必须报错，不能默认成 0 触发假的低电量告警。
        let json = r#"{"e": 0, "m": "ok", "d": {"roomName": "220407", "syje": "14.44"}}"#;
        let err = serde_json::from_str::<ApiResponse<PowerInfo>>(json).expect_err("should fail");
        assert!(
            err.to_string().contains("sydl"),
            "error should name the missing field: {}",
            err
        );
    }

    /// 一个带凭据的 CAS 登录页，用来验证它永远不会进日志。
    const LOGIN_PAGE: &str = r#"<!DOCTYPE html><html><head><meta name="csrf-token" content="CSRF-abc123"></head>
        <form id="casLoginForm"><input name="execution" value="e1s1-SECRET-TICKET"></form></html>"#;

    fn parse(text: &str) -> serde_json::Value {
        serde_json::from_str(text).expect("valid json")
    }

    #[test]
    fn json_snippet_escapes_and_truncates() {
        assert_eq!(json_snippet(&parse("{\n  \"e\": 0\n}")), r#"{"e":0}"#);

        let long = parse(&format!(r#"{{"m":"{}"}}"#, "x".repeat(MAX_LOGGED_BODY * 2)));
        let snippet = json_snippet(&long);
        assert!(snippet.ends_with("...<truncated>"));
        assert_eq!(
            snippet.chars().count(),
            MAX_LOGGED_BODY + "...<truncated>".chars().count()
        );
    }

    #[test]
    fn json_snippet_redacts_credential_shaped_fields() {
        // 上游哪天在错误响应里回显了 token，也不该留在日志里。
        let snippet = json_snippet(&parse(
            r#"{"e":1,"access_token":"AT-secret","d":{"JSESSIONID":"sess-secret","roomName":"220407"}}"#,
        ));
        assert!(!snippet.contains("AT-secret"), "leaked: {}", snippet);
        assert!(!snippet.contains("sess-secret"), "leaked: {}", snippet);
        assert!(snippet.contains("<redacted>"), "got: {}", snippet);
        // 非敏感字段仍应保留，否则诊断价值就没了。
        assert!(snippet.contains("220407"), "got: {}", snippet);
    }

    #[test]
    fn opaque_bodies_are_never_dumped() {
        // 登录页带 CSRF token / CAS ticket，只报形态不报内容。
        let described =
            describe_opaque_body("text/html; charset=utf-8", LOGIN_PAGE.len(), LOGIN_PAGE);
        assert!(
            !described.contains("SECRET-TICKET"),
            "leaked: {}",
            described
        );
        assert!(!described.contains("CSRF-abc123"), "leaked: {}", described);
        assert!(described.contains("CAS login page"), "got: {}", described);
        assert!(described.contains(&format!("{} bytes", LOGIN_PAGE.len())));
    }

    #[test]
    fn a_login_page_declared_as_json_still_never_reaches_the_log() {
        // 回归测试：曾经用声明的 Content-Type 决定能否 dump 内容，于是上游
        // （或前置网关）只要把登录页标成 application/json，ticket 就会进日志。
        let err = classify_body(
            StatusCode::OK,
            "application/json",
            LOGIN_PAGE.len(),
            LOGIN_PAGE,
        )
        .expect_err("a login page is not a valid envelope");

        let rendered = err.to_string();
        assert!(!rendered.contains("SECRET-TICKET"), "leaked: {}", rendered);
        assert!(!rendered.contains("CSRF-abc123"), "leaked: {}", rendered);
        assert!(matches!(err, FetchError::NotJson { .. }), "got: {:?}", err);
        // 归类为 NotJson 才会去探测会话并重新登录，否则会话过期后一直卡住。
        assert!(err.session_may_be_stale());
    }

    #[test]
    fn upstream_strings_cannot_forge_log_lines() {
        // 键名和字段值都是上游可控的，若原样拼接就能靠换行伪造一条日志记录。
        let forged = "x\n2026-07-26T00:00:00 INFO Power info received: room=FORGED";
        let body = serde_json::json!({"e": 0, "m": forged, "d": {forged: 1}});
        let described = describe_envelope(&body);
        assert!(!described.contains('\n'), "forged newline: {}", described);
        assert!(
            described.contains("\\n"),
            "should be escaped: {}",
            described
        );

        let snippet = json_snippet(&body);
        assert!(!snippet.contains('\n'), "forged newline: {}", snippet);
    }

    #[test]
    fn envelope_fields_and_key_lists_are_bounded() {
        let long = "x".repeat(MAX_LOGGED_FIELD * 4);
        let described = describe_envelope(&serde_json::json!({"e": 0, "m": long}));
        assert!(described.contains("...<truncated>"), "got: {}", described);
        assert!(
            described.chars().count() < MAX_LOGGED_FIELD * 2,
            "unbounded log line: {} chars",
            described.chars().count()
        );

        let many: serde_json::Map<String, serde_json::Value> = (0..MAX_LOGGED_KEYS + 10)
            .map(|i| (format!("k{i}"), serde_json::json!(1)))
            .collect();
        let described = describe_envelope(&serde_json::json!({"d": many}));
        assert!(described.contains("+10 more"), "got: {}", described);
    }

    #[test]
    fn describe_envelope_extracts_upstream_error_details() {
        // 上游降级返回：外层 e=0 "成功"，真正的失败信息藏在 d.retcode / d.msg。
        let body = parse(
            r#"{"e":0,"m":"操作成功","d":{"retcode":-1,"msg":"查询失败","roomName":"220407"}}"#,
        );
        let described = describe_envelope(&body);
        assert!(described.contains("e=0"), "got: {}", described);
        assert!(described.contains("d.retcode=-1"), "got: {}", described);
        assert!(described.contains("查询失败"), "got: {}", described);
        // serde_json 默认用 BTreeMap，键名按字典序输出；值经 JSON 转义带引号。
        assert!(
            described.contains(r#"d.keys=["msg","retcode","roomName"]"#),
            "got: {}",
            described
        );
    }

    #[test]
    fn describe_envelope_reports_unexpected_shapes() {
        assert!(describe_envelope(&parse(r#"{"e":1,"d":""}"#)).contains("d is a string"));
        assert!(describe_envelope(&parse(r#"{"e":1,"m":"err"}"#)).contains("d=null"));
        assert!(describe_envelope(&parse("[]")).contains("top level is an array"));
        assert!(describe_envelope(&parse("3")).contains("top level is a number"));
    }

    #[test]
    fn classify_body_accepts_a_valid_envelope() {
        let text = sample_json_with_values("26.91", "14.44");
        let resp = classify_body(StatusCode::OK, "application/json", text.len(), &text)
            .expect("valid envelope");
        assert_eq!(resp.status, StatusCode::OK);
        assert!((resp.body.data.expect("data").remaining_money - 14.44).abs() < f64::EPSILON);
    }

    #[test]
    fn classify_body_uses_a_valid_envelope_even_on_a_failing_status() {
        // 上游会用 5xx 包着 {"e":401,...}；必须解析出来，才能走重新登录逻辑。
        let text = r#"{"e":401,"m":"未登录","d":null}"#;
        let resp = classify_body(
            StatusCode::INTERNAL_SERVER_ERROR,
            "application/json",
            text.len(),
            text,
        )
        .expect("envelope should still be used");
        assert_eq!(resp.body.error, 401);
        assert_eq!(resp.status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn classify_body_reports_degraded_payloads_as_shape_errors() {
        // 这正是线上症状：响应体完整、是 JSON，但 d 里没有电量字段。
        let text = r#"{"e":0,"m":"操作成功","d":{"retcode":-1,"msg":"查询超时"}}"#;
        let err = classify_body(StatusCode::OK, "application/json", text.len(), text)
            .expect_err("missing readings must fail");

        let rendered = err.to_string();
        assert!(
            rendered.contains("sydl"),
            "should name the field: {}",
            rendered
        );
        assert!(rendered.contains("查询超时"), "got: {}", rendered);
        assert!(rendered.contains("d.retcode=-1"), "got: {}", rendered);
        assert!(matches!(err, FetchError::Shape { .. }), "got: {:?}", err);
        // 请求到达了业务接口，重新登录改变不了返回内容。
        assert!(!err.session_may_be_stale());
    }

    /// 线上真实样本：电费网关业务失败时返回 `"d":"失败{...}"`，内层 JSON
    /// 携带诊断信息与内网直连重试地址（密文与签名已打码）。
    const BUSINESS_FAILURE_BODY: &str = r#"{"d":"失败{\"fjh\":\"407\",\"dffjbh\":\"220407\",\"str2\":\"220407_20260807102647\",\"str3\":\"<ciphertext>\",\"str4\":\"<signature>\",\"sign\":\"<signature>\",\"data\":\"<ciphertext>\",\"url\":\"http:\/\/222.197.164.98:7000\/zxapi\/services\/query\/findeletric?sign=<signature>&data=<ciphertext>\",\"res_hash\":null}","e":0,"m":"操作成功"}"#;

    #[test]
    fn classify_body_reports_business_failures_with_readable_details() {
        // 线上症状：d 是 "失败{...}" 字符串，信封本身 e=0（不是会话问题）。
        let err = classify_body(
            StatusCode::OK,
            "application/json",
            BUSINESS_FAILURE_BODY.len(),
            BUSINESS_FAILURE_BODY,
        )
        .expect_err("business failure must fail");

        assert!(matches!(err, FetchError::Business { .. }), "got: {:?}", err);
        let rendered = err.to_string();
        assert!(rendered.contains("business failure"), "got: {}", rendered);
        assert!(
            rendered.contains("room \"407\"/\"220407\""),
            "got: {}",
            rendered
        );
        assert!(
            rendered.contains("222.197.164.98:7000"),
            "got: {}",
            rendered
        );
        assert!(rendered.contains("res_hash=null"), "got: {}", rendered);
        // 信封 e=0：不是会话问题，重新登录无济于事。
        assert!(!err.session_may_be_stale());
        // 密文与签名材料不进日志。
        assert!(!rendered.contains("<ciphertext>"), "leaked: {}", rendered);
        assert!(!rendered.contains("<signature>"), "leaked: {}", rendered);
    }

    #[test]
    fn classify_body_reports_plain_string_payloads_without_dumping_them() {
        // 不是 "失败{json}" 形态的字符串：只报形态与长度，不 dump 内容。
        let err = classify_body(
            StatusCode::OK,
            "application/json",
            BUSINESS_FAILURE_BODY.len(),
            BUSINESS_FAILURE_BODY,
        )
        .expect_err("business failure must fail");
        assert!(matches!(err, FetchError::Business { .. }), "got: {:?}", err);

        let text = r#"{"e":0,"m":"ok","d":"会话已过期，请重新登录电费系统"}"#;
        let err = classify_body(StatusCode::OK, "application/json", text.len(), text)
            .expect_err("a string payload is a business failure");
        assert!(matches!(err, FetchError::Business { .. }), "got: {:?}", err);
        let rendered = err.to_string();
        assert!(rendered.contains("non-JSON text"), "got: {}", rendered);
        assert!(
            !rendered.contains("会话已过期"),
            "should not dump the payload: {}",
            rendered
        );
    }

    #[test]
    fn classify_body_rejects_non_object_payloads_as_shape_errors() {
        // d 是数组/数字等其它类型：结构不符，归 Shape（200 下不触发重登）。
        let text = r#"{"e":0,"m":"ok","d":[1,2,3]}"#;
        let err = classify_body(StatusCode::OK, "application/json", text.len(), text)
            .expect_err("array payload must fail");
        assert!(matches!(err, FetchError::Shape { .. }), "got: {:?}", err);
        assert!(err.to_string().contains("d is an array"), "got: {}", err);
        assert!(!err.session_may_be_stale());
    }

    #[test]
    fn classify_body_separates_failing_statuses_from_login_redirects() {
        let err = classify_body(
            StatusCode::BAD_GATEWAY,
            "text/html",
            LOGIN_PAGE.len(),
            LOGIN_PAGE,
        )
        .expect_err("502 html is not an envelope");
        assert!(matches!(err, FetchError::Status { .. }), "got: {:?}", err);
        assert!(!err.to_string().contains("SECRET-TICKET"));
        // 5xx 是上游故障，不该触发重新登录。
        assert!(!err.session_may_be_stale());

        let err = classify_body(StatusCode::UNAUTHORIZED, "text/html", 0, "")
            .expect_err("401 is not an envelope");
        assert!(err.session_may_be_stale());
    }

    /// 起一个只回一次响应的本地 HTTP 服务，用来驱动真实的分块读取路径。
    async fn serve_once(body: Vec<u8>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let (reader, mut writer) = socket.split();

            // 读完请求头再回响应，否则客户端可能先撞上 RST。
            let mut reader = tokio::io::BufReader::new(reader);
            let mut line = String::new();
            loop {
                line.clear();
                let n = tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line)
                    .await
                    .expect("read request");
                if n == 0 || line == "\r\n" {
                    break;
                }
            }

            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            tokio::io::AsyncWriteExt::write_all(&mut writer, head.as_bytes())
                .await
                .expect("write head");
            tokio::io::AsyncWriteExt::write_all(&mut writer, &body)
                .await
                .expect("write body");
            tokio::io::AsyncWriteExt::flush(&mut writer)
                .await
                .expect("flush");
        });

        format!("http://{}/", addr)
    }

    async fn read_with_cap(body: Vec<u8>, cap: usize) -> CappedBody {
        let url = serve_once(body).await;
        let mut resp = reqwest::Client::new().get(&url).send().await.expect("send");
        read_body_capped(&mut resp, cap).await.expect("read body")
    }

    #[tokio::test]
    async fn capped_read_returns_a_body_that_fits() {
        let body = b"{\"e\":0}".to_vec();
        match read_with_cap(body.clone(), 1024).await {
            CappedBody::Complete(read) => assert_eq!(read, body),
            CappedBody::TooLarge { read } => panic!("unexpected TooLarge after {read} bytes"),
        }
    }

    #[tokio::test]
    async fn capped_read_accepts_a_body_exactly_at_the_cap() {
        // 边界：恰好等于上限的响应是完整的，不该被当成超限丢弃。
        let body = vec![b'x'; 64];
        match read_with_cap(body.clone(), 64).await {
            CappedBody::Complete(read) => assert_eq!(read.len(), 64),
            CappedBody::TooLarge { read } => panic!("body exactly at the cap was rejected: {read}"),
        }
    }

    #[tokio::test]
    async fn capped_read_gives_up_once_the_body_exceeds_the_cap() {
        match read_with_cap(vec![b'x'; 4096], 64).await {
            CappedBody::Complete(read) => panic!("cap not enforced, read {} bytes", read.len()),
            CappedBody::TooLarge { read } => assert!(read > 64, "read={read}"),
        }
    }

    #[test]
    fn logged_upstream_strings_are_stripped_of_control_characters_at_parse_time() {
        // 成功路径上的 m / roomName 是直接插进 warn!/info! 的，裸换行足以伪造
        // 出一条完整的日志记录。清理放在反序列化处，日志、通知、入库一并覆盖。
        let forged = "220407\\n2026-07-26T00:00:00 ERROR uestc_power_monitor: fake";
        let json = format!(
            r#"{{"e":0,"m":"ok\nERROR forged envelope","d":{{"sydl":1.0,"syje":2.0,"roomName":"{}"}}}}"#,
            forged
        );

        let resp: ApiResponse<PowerInfo> = serde_json::from_str(&json).expect("parse");
        assert!(!resp.message.contains('\n'), "got: {:?}", resp.message);
        let data = resp.data.expect("data");
        assert!(
            !data.room_display_name.contains('\n'),
            "got: {:?}",
            data.room_display_name
        );
        // 控制字符换成空格，可读内容仍然保留。
        assert!(data.room_display_name.starts_with("220407"));
        assert!(resp.message.starts_with("ok "));
    }

    #[test]
    fn sanitizing_replaces_every_control_character() {
        let sanitized = sanitize_control_chars("a\r\nb\tc\u{1b}[2Jd");
        assert_eq!(sanitized, "a  b c [2Jd");
        assert!(!sanitized.chars().any(char::is_control));
    }

    #[test]
    fn redaction_catches_credential_shaped_values_under_benign_keys() {
        // 键名匹配挡不住这种：键名无害，值里带着 CAS ticket。
        let snippet = json_snippet(&parse(
            r#"{"d":{"redirect":"https://idas.uestc.edu.cn/authserver/login?ticket=ST-99-SECRET"},"e":401}"#,
        ));
        assert!(!snippet.contains("ST-99-SECRET"), "leaked: {}", snippet);
        assert!(snippet.contains("<redacted>"), "got: {}", snippet);

        assert!(value_looks_secret("Bearer eyJhbGciOi.X"));
        assert!(value_looks_secret("Set-Cookie: JSESSIONID=abc"));
        assert!(!value_looks_secret("查询失败"));
        assert!(!value_looks_secret("220407"));
    }

    #[test]
    fn a_json_401_is_treated_as_a_stale_session_like_an_html_401() {
        // 同样是 401，不该因为错误页恰好是 JSON（而不是 HTML）就走上相反的恢复路径。
        let text = r#"{"code":"UNAUTHORIZED","message":"token expired"}"#;
        let err = classify_body(
            StatusCode::UNAUTHORIZED,
            "application/json",
            text.len(),
            text,
        )
        .expect_err("not our envelope");
        assert!(matches!(err, FetchError::Shape { .. }), "got: {:?}", err);
        assert!(err.session_may_be_stale(), "a 401 must drive a re-login");
    }

    #[test]
    fn an_http_401_carrying_a_valid_envelope_is_still_a_stale_session() {
        // 网关可能用 HTTP 401 配一个能正常解析的信封；只看信封里的 e 会漏掉它。
        let text = r#"{"e":0,"m":"unauthorized","d":null}"#;
        let resp = classify_body(
            StatusCode::UNAUTHORIZED,
            "application/json",
            text.len(),
            text,
        )
        .expect("envelope parses");
        assert_ne!(resp.body.error, 401, "the envelope itself does not say 401");
        assert!(
            session_expired_status(resp.status),
            "the HTTP status is the only stale-session signal here"
        );
    }

    fn power_response(status: StatusCode, text: &str) -> PowerResponse {
        classify_body(status, "application/json", text.len(), text).expect("valid envelope")
    }

    #[test]
    fn a_401_status_carrying_real_readings_does_not_discard_them() {
        // 状态码说未授权，但信封里带着读数：重登会把这条读数丢掉，得不偿失。
        let text = sample_json_with_values("26.91", "14.44");
        let resp = power_response(StatusCode::UNAUTHORIZED, &text);
        assert!(resp.body.data.is_some());
        assert!(
            !should_relogin(&resp),
            "a usable reading must not be thrown away for a re-login"
        );
    }

    #[test]
    fn a_401_status_without_readings_drives_a_re_login() {
        let resp = power_response(
            StatusCode::UNAUTHORIZED,
            r#"{"e":0,"m":"unauthorized","d":null}"#,
        );
        assert!(should_relogin(&resp));

        // 信封自己说 401 时，无论状态码如何都要重登。
        let resp = power_response(StatusCode::OK, r#"{"e":401,"m":"未登录","d":null}"#);
        assert!(should_relogin(&resp));

        // 一切正常时不该无端重登。
        let text = sample_json_with_values("26.91", "14.44");
        assert!(!should_relogin(&power_response(StatusCode::OK, &text)));
    }

    #[test]
    fn login_throttle_allows_one_attempt_per_cooldown() {
        let throttle = LoginThrottle::new();
        let cooldown = Duration::from_secs(60);
        let start = Instant::now();

        assert!(throttle.claim(start, cooldown), "first attempt allowed");
        assert!(
            !throttle.claim(start + Duration::from_secs(59), cooldown),
            "must not hammer SSO inside the cooldown"
        );
        assert!(
            throttle.claim(start + Duration::from_secs(60), cooldown),
            "allowed again once the cooldown elapsed"
        );
        // 每次放行都会重置窗口，否则一次故障期内仍会连续登录。
        assert!(!throttle.claim(start + Duration::from_secs(61), cooldown));
    }

    #[test]
    fn only_session_related_failures_trigger_a_re_login() {
        assert!(
            FetchError::NotJson {
                status: StatusCode::OK,
                body: "html".to_string(),
            }
            .session_may_be_stale()
        );
        assert!(
            FetchError::Status {
                status: StatusCode::FORBIDDEN,
                body: String::new(),
            }
            .session_may_be_stale()
        );
        assert!(
            !FetchError::Status {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                body: String::new(),
            }
            .session_may_be_stale()
        );
        // 上游返回了 JSON 但结构不对，重新登录也没用。
        assert!(
            !FetchError::Shape {
                status: StatusCode::OK,
                envelope: String::new(),
                detail: String::new(),
                snippet: String::new(),
            }
            .session_may_be_stale()
        );
        // 超大响应体已经放弃读取，同样不是会话问题。
        let too_large = FetchError::TooLarge {
            status: StatusCode::OK,
            content_type: "text/html".to_string(),
            read: MAX_BODY_BYTES,
        };
        // 超大响应体几乎只可能是登录页/错误页，按会话失效处理。
        assert!(too_large.session_may_be_stale());
        assert!(too_large.to_string().contains("content not logged"));
    }
}
