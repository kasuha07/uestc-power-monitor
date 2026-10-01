//! Browser authentication and monitoring share one client and one operation lock.
//! Passwords and reauth contexts stay in memory; only encrypted cookies are persisted.
use crate::{api::ApiService, config::AppConfig, db::DbService, notify::NotificationManager};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Query, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tracing::{info, warn};
use uestc_client::{ReauthContext, ReauthMethodKind, UestcClientError};

#[cfg(test)]
#[path = "web/tests.rs"]
mod tests;

#[path = "web/trend.rs"]
mod trend_data;
#[cfg(test)]
use trend_data::build_trend;

#[derive(Clone, Serialize)]
struct Method {
    id: i32,
    name: String,
    kind: &'static str,
}

#[derive(Clone, Serialize)]
struct Reading {
    money: f64,
    energy: f64,
    room: String,
    sampled_at: String,
}

#[derive(Clone, Serialize)]
struct Status {
    phase: &'static str,
    message: String,
    methods: Vec<Method>,
    qr_svg: Option<String>,
    reading: Option<Reading>,
    trust_device: bool,
    refreshing: bool,
}

struct Session {
    api: ApiService,
    reauth: Option<ReauthContext>,
    ready: bool,
    code_sent_at: Option<tokio::time::Instant>,
}

#[derive(Default)]
struct Control {
    task: Option<tokio::task::AbortHandle>,
    cancelling: bool,
}

struct WebState {
    db: DbService,
    session: Arc<AsyncMutex<Session>>,
    status: Mutex<Status>,
    token: String,
    wake: Notify,
    control: Mutex<Control>,
    auth_error: Mutex<Option<&'static str>>,
    /// 当前生效的运行时配置；配置管理保存/重载后原子替换，监控循环按轮次读取。
    config: RwLock<Arc<AppConfig>>,
    /// 页面可编辑的配置文件（config.toml）；配置来自其他格式时为 None。
    config_file: Option<PathBuf>,
    /// 串行化“读文件→写盘→替换运行时配置”，避免并发保存互相覆盖。
    config_ops: Mutex<()>,
}

impl WebState {
    fn new(
        config: &AppConfig,
        token: String,
        db: DbService,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        Ok(Arc::new(Self {
            db,
            session: Arc::new(AsyncMutex::new(Session {
                api: ApiService::unlogged(config, false, true)?,
                reauth: None,
                ready: false,
                code_sent_at: None,
            })),
            status: Mutex::new(Status {
                phase: "awaiting_login",
                message: "请完成登录，监控将在登录成功后自动开始。".into(),
                methods: vec![],
                qr_svg: None,
                reading: None,
                trust_device: config.reauth_trust_device,
                refreshing: false,
            }),
            token,
            wake: Notify::new(),
            control: Mutex::new(Control::default()),
            auth_error: Mutex::new(None),
            config: RwLock::new(Arc::new(config.clone())),
            config_file: resolve_config_file(),
            config_ops: Mutex::new(()),
        }))
    }

    fn config_snapshot(&self) -> Arc<AppConfig> {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn swap_config(&self, config: AppConfig) {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .trust_device = config.reauth_trust_device;
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(config);
    }

    fn update(&self, phase: &'static str, message: impl Into<String>) {
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        status.phase = phase;
        status.message = message.into();
        status.qr_svg = None;
    }

    fn show_methods(&self, ctx: &ReauthContext) {
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        status.methods = ctx
            .available_methods
            .iter()
            .filter(|m| m.is_supported())
            .map(|m| Method {
                id: m.id,
                name: m.name.clone(),
                kind: match m.kind() {
                    ReauthMethodKind::Wechat => "wechat",
                    ReauthMethodKind::DynamicCode => "code",
                    _ => "password",
                },
            })
            .collect();
    }

    fn show_qr(&self, uuid: &str) -> Result<(), UestcClientError> {
        // Generate locally: no QR service receives the authentication URL.
        let url = format!("https://open.weixin.qq.com/connect/confirm?uuid={uuid}");
        let qr =
            qrcode::QrCode::new(url.as_bytes()).map_err(|_| UestcClientError::WeChatError {
                message: "无法生成二维码".into(),
            })?;
        let svg = qr
            .render::<qrcode::render::svg::Color>()
            .min_dimensions(256, 256)
            .build();
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        status.phase = "qr_pending";
        status.message = "请用微信扫描二维码，并在手机上确认。二维码五分钟内有效。".into();
        status.qr_svg = Some(svg);
        Ok(())
    }

    fn start(self: &Arc<Self>, action: Action) -> Result<(), StatusCode> {
        let mut control = self.control.lock().unwrap_or_else(|e| e.into_inner());
        if control.cancelling {
            return Err(StatusCode::CONFLICT);
        }
        // A rejected second request cannot replace an in-flight challenge.
        let session = self
            .session
            .clone()
            .try_lock_owned()
            .map_err(|_| StatusCode::CONFLICT)?;
        if self
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .refreshing
        {
            return Err(StatusCode::CONFLICT);
        }
        self.update("authenticating", "正在处理认证，请稍候…");
        let state = self.clone();
        let task = tokio::spawn(async move {
            let mut session = session;
            let result = tokio::time::timeout(
                Duration::from_secs(420),
                state.authenticate(&mut session, action),
            )
            .await;
            let result = result.unwrap_or(Err(UestcClientError::WeChatError {
                message: "认证超时，请重新登录".into(),
            }));
            if let Err(error) = result {
                if let UestcClientError::NetworkError { source, .. } = &error {
                    // Log transport facts without upstream URLs, bodies or credentials.
                    warn!(
                        upstream_host = source.url().and_then(|url| url.host_str()),
                        status = source.status().map(|status| status.as_u16()),
                        timeout = source.is_timeout(),
                        connect = source.is_connect(),
                        decode = source.is_decode(),
                        redirect = source.is_redirect(),
                        "Web 认证上游请求失败"
                    );
                }
                session.ready = false;
                let phase = if session.reauth.is_some() {
                    "reauth_required"
                } else {
                    "awaiting_login"
                };
                // Never forward upstream URLs, response bodies, passwords or tickets.
                state.update(phase, public_error(&error));
                *state.auth_error.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(public_error(&error));
            }
            state.wake.notify_one();
        });
        control.task = Some(task.abort_handle());
        Ok(())
    }

    async fn authenticate(
        &self,
        session: &mut Session,
        action: Action,
    ) -> Result<(), UestcClientError> {
        session.ready = false;
        let result = match action {
            Action::Resume => {
                if session.api.client.is_session_active().await {
                    Ok(())
                } else {
                    Err(UestcClientError::SessionExpired)
                }
            }
            Action::Login(input) => {
                session.reauth = None;
                session.code_sent_at = None;
                self.status
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .methods
                    .clear();
                match input.login_type.as_str() {
                    "password" => {
                        session
                            .api
                            .client
                            .login(input.username.trim(), &input.password)
                            .await
                    }
                    "wechat" => {
                        session
                            .api
                            .client
                            .wechat_login_with_qr(|uuid| self.show_qr(uuid))
                            .await
                    }
                    _ => unreachable!("validated before dispatch"),
                }
            }
            Action::Reauth(input) => {
                let ctx = session
                    .reauth
                    .as_mut()
                    .ok_or(UestcClientError::SessionExpired)?;
                let method = ctx
                    .available_methods
                    .iter()
                    .find(|m| m.id == input.method && m.is_supported())
                    .cloned()
                    .ok_or(UestcClientError::SessionExpired)?;
                if method.kind() != ReauthMethodKind::Wechat && ctx.current_type_id() != method.id {
                    session.api.client.change_reauth_type(ctx, &method).await?;
                }
                if input.send_code {
                    if method.kind() != ReauthMethodKind::DynamicCode {
                        return Err(UestcClientError::ReauthFailed {
                            message: "该方式不支持发送验证码".into(),
                        });
                    }
                    if session
                        .code_sent_at
                        .is_some_and(|t| t.elapsed() < Duration::from_secs(60))
                    {
                        self.update("reauth_required", "验证码已发送，请等待一分钟后再重发。");
                        return Ok(());
                    }
                    // Also throttle rejected/uncertain sends: retrying may send duplicates.
                    session.code_sent_at = Some(tokio::time::Instant::now());
                    session.api.client.send_reauth_code(ctx, &method).await?;
                    self.update("reauth_required", "验证码已发送，请输入收到的验证码。");
                    return Ok(());
                }
                session
                    .api
                    .client
                    .submit_reauth_with_qr(
                        ctx,
                        &method,
                        Some(&input.code),
                        Some(&input.password),
                        input.trust_device,
                        |uuid| self.show_qr(uuid),
                    )
                    .await
            }
        };
        if let Err(UestcClientError::ReauthRequired { context }) = result {
            self.show_methods(&context);
            session.reauth = Some(*context);
            self.update("reauth_required", "账号需要二次认证，请选择验证方式。");
            return Ok(());
        }
        result?;
        // A CAS success alone is insufficient: verify the electricity portal and persist
        // its cookies before waking the monitor or telling the browser it succeeded.
        session.api.initialize_business_session().await?;
        session.ready = true;
        session.reauth = None;
        session.api.take_reauth_pending();
        session.api.take_login_retry_failure();
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .methods
            .clear();
        self.update("monitoring", "登录成功，正在监控宿舍电费。");
        Ok(())
    }
}

#[derive(Deserialize)]
struct LoginInput {
    login_type: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

#[derive(Deserialize)]
struct ReauthInput {
    method: i32,
    #[serde(default)]
    send_code: bool,
    #[serde(default)]
    code: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    trust_device: bool,
}

enum Action {
    Resume,
    Login(LoginInput),
    Reauth(ReauthInput),
}

fn public_error(error: &UestcClientError) -> &'static str {
    match error {
        UestcClientError::LoginFailed { .. } => {
            "登录失败，请检查账号密码；如学校要求验证码，请改用微信扫码。"
        }
        UestcClientError::ReauthFailed { .. } => "二次认证失败，请检查验证码或重新登录。",
        UestcClientError::WeChatError { .. } => "二维码已失效或扫码失败，请重新发起认证。",
        UestcClientError::NetworkError { source, .. } if source.is_status() => {
            "学校接口返回异常，请查看容器日志中的 HTTP 状态码。"
        }
        UestcClientError::NetworkError { .. } => "连接学校认证平台失败，请稍后重试。",
        UestcClientError::CookieError { .. } => "保存会话失败，请检查数据目录的写入权限。",
        UestcClientError::SessionExpired => "会话未就绪或已过期，请重新登录。",
        _ => "认证未完成，请重试或更换登录方式。",
    }
}

async fn access(State(state): State<Arc<WebState>>, request: Request, next: Next) -> Response {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("");
    if token.len() != state.token.len()
        || !openssl::memcmp::eq(token.as_bytes(), state.token.as_bytes())
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Some(origin) = request.headers().get(header::ORIGIN) {
        let host = request
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        let same_origin = origin
            .to_str()
            .ok()
            .and_then(|s| reqwest::Url::parse(s).ok())
            .is_some_and(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url[url::Position::BeforeHost..url::Position::AfterPort] == *host
            });
        if !same_origin {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    next.run(request).await
}

async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    headers.insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    headers.insert(header::CONTENT_SECURITY_POLICY,
        "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' blob:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'".parse().unwrap());
    response
}

fn router(state: Arc<WebState>) -> Router {
    let protected = Router::new()
        .route("/api/status", get(status))
        .route("/api/history", get(history))
        .route("/api/trend", get(trend))
        .route("/api/refresh", post(refresh))
        .route("/api/login", post(login))
        .route("/api/reauth", post(reauth))
        .route("/api/cancel", post(cancel))
        .route("/api/notify/test", post(test_notify))
        .route("/api/config", get(view_config).post(save_config))
        .route("/api/config/reload", post(reload_config))
        .route_layer(middleware::from_fn_with_state(state.clone(), access));
    Router::new()
        .route("/", get(|| async { Html(include_str!("web/index.html")) }))
        .route(
            "/app.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("web/app.js"),
                )
            }),
        )
        .route(
            "/style.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("web/style.css"),
                )
            }),
        )
        // Liveness measures the process, not whether a user has logged in.
        .route("/healthz", get(|| async { "ok" }))
        .merge(protected)
        .layer(DefaultBodyLimit::max(8 * 1024))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

async fn status(State(state): State<Arc<WebState>>) -> Json<Status> {
    Json(
        state
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
    )
}

async fn history(
    State(state): State<Arc<WebState>>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<Vec<crate::db::PowerRecord>>, StatusCode> {
    let limit = query.limit.unwrap_or(10).clamp(1, 1000);
    state
        .db
        .recent_records(limit)
        .await
        .map(Json)
        .map_err(|error| {
            warn!("读取采集历史失败：{error}");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

#[derive(Deserialize)]
struct HistoryQuery {
    limit: Option<u32>,
}

async fn refresh(State(state): State<Arc<WebState>>) -> Result<StatusCode, StatusCode> {
    let session = state.session.try_lock().map_err(|_| StatusCode::CONFLICT)?;
    if !session.ready {
        return Err(StatusCode::CONFLICT);
    }
    let mut status = state.status.lock().unwrap_or_else(|e| e.into_inner());
    if status.refreshing {
        return Err(StatusCode::CONFLICT);
    }
    status.refreshing = true;
    state.wake.notify_one();
    Ok(StatusCode::ACCEPTED)
}

async fn login(
    State(state): State<Arc<WebState>>,
    Json(input): Json<LoginInput>,
) -> Result<StatusCode, StatusCode> {
    if !matches!(input.login_type.as_str(), "password" | "wechat")
        || (input.login_type == "password"
            && (input.username.trim().is_empty() || input.password.is_empty()))
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    state.start(Action::Login(input))?;
    Ok(StatusCode::ACCEPTED)
}

async fn reauth(
    State(state): State<Arc<WebState>>,
    Json(input): Json<ReauthInput>,
) -> Result<StatusCode, StatusCode> {
    {
        let session = state.session.try_lock().map_err(|_| StatusCode::CONFLICT)?;
        let method = session
            .reauth
            .as_ref()
            .and_then(|ctx| {
                ctx.available_methods
                    .iter()
                    .find(|m| m.id == input.method && m.is_supported())
            })
            .ok_or(StatusCode::BAD_REQUEST)?;
        if (input.send_code && method.kind() != ReauthMethodKind::DynamicCode)
            || (!input.send_code
                && method.kind() == ReauthMethodKind::DynamicCode
                && input.code.trim().is_empty())
            || (!input.send_code
                && method.kind() == ReauthMethodKind::Password
                && input.password.is_empty())
        {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    state.start(Action::Reauth(input))?;
    Ok(StatusCode::ACCEPTED)
}

async fn cancel(State(state): State<Arc<WebState>>) -> StatusCode {
    {
        let mut control = state.control.lock().unwrap_or_else(|e| e.into_inner());
        if control.cancelling || control.task.as_ref().is_none_or(|task| task.is_finished()) {
            return StatusCode::CONFLICT;
        }
        control.cancelling = true;
        if let Some(task) = control.task.take() {
            task.abort();
        }
    }
    // Wait for the aborted task to release the client; prevent another request
    // from starting in the gap between aborting it and clearing its challenge.
    let mut session = state.session.lock().await;
    session.ready = false;
    session.reauth = None;
    state
        .status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .methods
        .clear();
    state.update("awaiting_login", "认证已取消，可重新登录。");
    state
        .control
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .cancelling = false;
    state.wake.notify_one();
    StatusCode::ACCEPTED
}

// ============================== 配置管理 ==============================

const SECRET_MASK: &str = "********";

/// 查看时打码、保存时只允许“保持不变 / 覆盖 / 清除”的敏感配置项。
const SECRET_PATHS: &[&str] = &[
    "password",
    "cookie_encryption_key",
    "web.access_token",
    "notify.telegram_bot_token",
    "notify.pushover_api_token",
    "notify.pushover_user_key",
    "notify.ntfy_token",
    "notify.smtp_password",
];

/// 启动时即绑定到监听器、数据库连接、客户端或密钥的配置项：
/// 页面保存后写入文件，但要重启进程才生效。
const RESTART_REQUIRED_PATHS: &[&str] = &[
    "username",
    "password",
    "login_type",
    "service_url",
    "database_url",
    "cookie_file",
    "cookie_encryption_key",
    "web.enabled",
    "web.bind",
    "web.access_token",
];

#[derive(Serialize)]
struct ConfigWarning {
    path: String,
    message: String,
}

#[derive(Serialize)]
struct ConfigView {
    file: Option<String>,
    editable: bool,
    config: serde_json::Value,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<ConfigWarning>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    restart: Vec<String>,
}

fn config_error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(serde_json::json!({ "message": message.into() })),
    )
        .into_response()
}

fn json_child<'a>(value: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    match path.split_once('.') {
        Some((head, tail)) => json_child(value.get(head)?, tail),
        None => value.get(path),
    }
}

fn json_child_mut<'a>(
    value: &'a mut serde_json::Value,
    path: &str,
) -> Option<&'a mut serde_json::Value> {
    match path.split_once('.') {
        Some((head, tail)) => json_child_mut(value.get_mut(head)?, tail),
        None => value.get_mut(path),
    }
}

/// 查看用的配置快照：敏感字段替换为占位符（未设置显示为 null）。
fn masked_config(config: &AppConfig) -> serde_json::Value {
    let mut value = serde_json::to_value(config).expect("AppConfig always serializes");
    for path in SECRET_PATHS {
        if let Some(slot) = json_child_mut(&mut value, path) {
            match slot {
                serde_json::Value::Null => {}
                serde_json::Value::String(text) if text.is_empty() => {
                    *slot = serde_json::Value::Null;
                }
                _ => *slot = serde_json::Value::String(SECRET_MASK.into()),
            }
        }
    }
    value
}

/// 页面编辑只支持 config.toml：TOML 已存在（或尚无任何配置文件）时返回其路径；
/// 存在其他格式的 config.* 时返回 None，避免页面修改与真实配置来源分叉。
fn resolve_config_file() -> Option<PathBuf> {
    const TOML: &str = "config.toml";
    if Path::new(TOML).is_file() {
        return Some(PathBuf::from(TOML));
    }
    for extension in ["json", "yaml", "yml", "ini", "ron", "json5"] {
        if Path::new(&format!("config.{extension}")).is_file() {
            return None;
        }
    }
    Some(PathBuf::from(TOML))
}

async fn view_config(State(state): State<Arc<WebState>>) -> Json<ConfigView> {
    Json(ConfigView {
        file: state
            .config_file
            .as_ref()
            .map(|path| path.display().to_string()),
        editable: state.config_file.is_some(),
        config: masked_config(&state.config_snapshot()),
        warnings: Vec::new(),
        restart: Vec::new(),
    })
}

async fn save_config(
    State(state): State<Arc<WebState>>,
    Json(patch): Json<serde_json::Value>,
) -> Result<Json<ConfigView>, Response> {
    let file = state.config_file.clone().ok_or_else(|| {
        config_error(
            StatusCode::CONFLICT,
            "未找到可编辑的 config.toml（配置可能来自其他格式或环境变量），请在程序工作目录准备 config.toml 后重试。",
        )
    })?;
    // 以下全程同步（文件读写与配置替换之间没有 await），锁不会跨挂起点持有。
    let _guard = state.config_ops.lock().unwrap_or_else(|e| e.into_inner());

    let existing = match std::fs::read_to_string(&file) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(config_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("读取配置文件失败：{error}"),
            ));
        }
    };
    let mut document: toml_edit::DocumentMut = existing.parse().map_err(|_| {
        config_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "配置文件不是有效的 TOML，请先手工修复。为保护敏感项，不回传配置源码。",
        )
    })?;
    let changes = apply_config_patch(document.as_table_mut(), &patch)
        .map_err(|message| config_error(StatusCode::BAD_REQUEST, message))?;
    if changes.is_empty() {
        return Err(config_error(
            StatusCode::BAD_REQUEST,
            "没有检测到任何修改。",
        ));
    }
    let toml_text = document.to_string();
    let mut config = AppConfig::from_config_toml_str(&toml_text).map_err(|_| {
        config_error(
            StatusCode::BAD_REQUEST,
            "配置校验失败：配置格式或字段类型不正确，请检查修改项。",
        )
    })?;
    if let Err(error) = config.validate_runtime_update() {
        return Err(config_error(
            StatusCode::BAD_REQUEST,
            format!("配置校验失败：{error}"),
        ));
    }
    prepare_cookie_key(&mut config).map_err(|error| {
        config_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("解析 Cookie 加密密钥失败：{error}"),
        )
    })?;
    write_config_file(&file, &toml_text).map_err(|error| {
        config_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("写入配置文件失败：{error}"),
        )
    })?;
    apply_runtime_config(&state, config);

    let snapshot = state.config_snapshot();
    let warnings = config_warnings(&changes, &snapshot);
    let restart = restart_required_paths(&changes);
    info!("配置已通过 Web 页面修改并写入 {}", file.display());
    Ok(Json(ConfigView {
        file: Some(file.display().to_string()),
        editable: true,
        config: masked_config(&snapshot),
        warnings,
        restart,
    }))
}

async fn reload_config(State(state): State<Arc<WebState>>) -> Result<Json<ConfigView>, Response> {
    let _guard = state.config_ops.lock().unwrap_or_else(|e| e.into_inner());
    let mut config = AppConfig::from_config_file(state.config_file.as_deref()).map_err(|_| {
        config_error(
            StatusCode::BAD_REQUEST,
            "从磁盘加载配置失败：请检查文件是否可读、配置格式及字段类型。",
        )
    })?;
    if let Err(error) = config.validate_runtime_update() {
        return Err(config_error(
            StatusCode::BAD_REQUEST,
            format!("配置校验失败（请先修正配置文件）：{error}"),
        ));
    }
    prepare_cookie_key(&mut config).map_err(|error| {
        config_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("解析 Cookie 加密密钥失败：{error}"),
        )
    })?;
    apply_runtime_config(&state, config);
    info!("已从配置文件重载运行时配置");
    Ok(Json(ConfigView {
        file: state
            .config_file
            .as_ref()
            .map(|path| path.display().to_string()),
        editable: state.config_file.is_some(),
        config: masked_config(&state.config_snapshot()),
        warnings: Vec::new(),
        restart: Vec::new(),
    }))
}

/// 替换运行时配置并应用可即时生效的部分：时区与二次认证默认勾选。
/// 唤醒监控循环：下一轮读取新快照，按需重建通知管理器并使用新的采集间隔，
/// 避免保存后要等满当前 interval 才生效。
fn apply_runtime_config(state: &WebState, config: AppConfig) {
    if let Err(error) = crate::time::set_timezone(&config.timezone) {
        warn!(
            "应用新时区失败：{error}（保持 {}）",
            crate::time::current_timezone_name()
        );
    }
    state.swap_config(config);
    state.wake.notify_one();
}

type ConfigChanges = Vec<(String, serde_json::Value)>;

/// 将 JSON 补丁写入 TOML 表：嵌套对象进入子表；null 删除键；
/// 其余标量/数组按值写入。返回“路径 + 写入值”用于保存后的生效性核对。
fn apply_config_patch(
    table: &mut toml_edit::Table,
    patch: &serde_json::Value,
) -> Result<ConfigChanges, String> {
    fn walk(
        table: &mut toml_edit::Table,
        prefix: &str,
        patch: &serde_json::Value,
        changes: &mut ConfigChanges,
    ) -> Result<(), String> {
        let Some(fields) = patch.as_object() else {
            return Err("配置修改必须是 JSON 对象。".into());
        };
        for (key, value) in fields {
            if key.is_empty()
                || !key
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return Err(format!("非法的配置项名称：{key:?}"));
            }
            let path = if prefix.is_empty() {
                key.to_string()
            } else {
                format!("{prefix}.{key}")
            };
            if value.as_object().is_some_and(|object| !object.is_empty()) {
                let child = table
                    .entry(key)
                    .or_insert(toml_edit::table())
                    .as_table_mut()
                    .ok_or_else(|| format!("配置项 {path} 已存在且不是配置表，无法写入其子项。"))?;
                walk(child, &path, value, changes)?;
            } else if value.is_null() {
                table.remove(key);
                changes.push((path, serde_json::Value::Null));
            } else {
                let mut new_value = json_to_toml_value(value)?;
                // 原位替换并继承旧值的 decor（键上的前导注释与等号空白由键本身保留），
                // 否则 Table::insert 重建键会丢掉被改写行的注释。
                if let Some(toml_edit::Item::Value(existing)) = table.get(key) {
                    *new_value.decor_mut() = existing.decor().clone();
                }
                match table.get_mut(key) {
                    Some(existing) => *existing = toml_edit::Item::Value(new_value),
                    None => {
                        table.insert(key, toml_edit::Item::Value(new_value));
                    }
                }
                // heartbeat_hours 是正式键名；与别名 heartbeat_hour 并存会让 serde 报重复字段。
                if path == "notify.heartbeat_hours" {
                    table.remove("heartbeat_hour");
                }
                changes.push((path, value.clone()));
            }
        }
        Ok(())
    }

    let mut changes = ConfigChanges::new();
    walk(table, "", patch, &mut changes)?;
    Ok(changes)
}

fn json_to_toml_value(value: &serde_json::Value) -> Result<toml_edit::Value, String> {
    Ok(match value {
        serde_json::Value::Bool(flag) => (*flag).into(),
        serde_json::Value::Number(number) => match (number.as_i64(), number.as_f64()) {
            (Some(int), _) => int.into(),
            (None, Some(float)) if float.is_finite() => float.into(),
            _ => return Err("数字超出可表示范围。".into()),
        },
        serde_json::Value::String(text) => text.as_str().into(),
        serde_json::Value::Array(items) => {
            let mut array = toml_edit::Array::new();
            for item in items {
                array.push(json_to_toml_value(item)?);
            }
            array.into()
        }
        serde_json::Value::Object(fields) => {
            let mut inline = toml_edit::InlineTable::new();
            for (key, item) in fields {
                inline.insert(key.as_str(), json_to_toml_value(item)?);
            }
            inline.into()
        }
        serde_json::Value::Null => return Err("null 仅用于清除配置项。".into()),
    })
}

/// 保存后核对：逐项比较“写入文件的值”与“重载后的生效值”。需重启生效的
/// 配置项不参与核对（其生效值由启动时决定，且其中 cookie 密钥会被解析改写）。
/// 不一致通常意味着该配置项被环境变量（UPM_*）或 Docker Secrets 覆盖。
fn config_warnings(changes: &ConfigChanges, config: &AppConfig) -> Vec<ConfigWarning> {
    let effective = serde_json::to_value(config).expect("AppConfig always serializes");
    changes
        .iter()
        .filter(|(path, _)| !RESTART_REQUIRED_PATHS.contains(&path.as_str()))
        .filter_map(|(path, written)| {
            if json_effective_eq(written, json_child(&effective, path)) {
                None
            } else {
                Some(ConfigWarning {
                    path: path.clone(),
                    message: "重载后的生效值与写入值不一致，通常是被环境变量（UPM_*）或 Docker Secrets 覆盖，或该配置项不存在。".into(),
                })
            }
        })
        .collect()
}

/// null 写入允许键消失或变为空字符串；数字在整型/浮点之间视为相等。
fn json_effective_eq(written: &serde_json::Value, applied: Option<&serde_json::Value>) -> bool {
    match (written, applied) {
        (serde_json::Value::Null, None) => true,
        (serde_json::Value::Null, Some(serde_json::Value::Null)) => true,
        (serde_json::Value::Null, Some(serde_json::Value::String(text))) => text.is_empty(),
        (written, Some(applied)) => json_loose_eq(written, applied),
        _ => false,
    }
}

fn json_loose_eq(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    match (left, right) {
        (serde_json::Value::Number(left), serde_json::Value::Number(right)) => {
            left.as_f64() == right.as_f64()
        }
        (serde_json::Value::Array(left), serde_json::Value::Array(right)) => {
            left.len() == right.len() && left.iter().zip(right).all(|(l, r)| json_loose_eq(l, r))
        }
        (left, right) => left == right,
    }
}

fn restart_required_paths(changes: &ConfigChanges) -> Vec<String> {
    changes
        .iter()
        .map(|(path, _)| path.clone())
        .filter(|path| RESTART_REQUIRED_PATHS.contains(&path.as_str()))
        .collect()
}

/// 原子写盘：临时文件 + rename，保留原文件权限（新文件 0600，配置可能含密钥）。
fn write_config_file(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    let temporary = path.with_extension(format!("toml.{}.tmp", random_secret()?));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let mode = std::fs::metadata(path)
                .map(|metadata| metadata.permissions().mode() & 0o777)
                .unwrap_or(0o600);
            options.mode(mode);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&temporary); // 成功 rename 后为空操作
    result
}

// ============================== 用电趋势 ==============================

async fn trend(State(state): State<Arc<WebState>>) -> Result<Json<trend_data::Trend>, StatusCode> {
    let tz = crate::time::current_timezone();
    let today = chrono::Utc::now().with_timezone(&tz).date_naive();
    let since = trend_data::window_start(tz, today)
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?
        .to_rfc3339();
    let mut builder =
        trend_data::TrendBuilder::new(tz, today).ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let records = state.db.trend_records(&since);
    tokio::pin!(records);
    while let Some(record) = records.try_next().await.map_err(|error| {
        warn!("读取用电趋势失败：{error}");
        StatusCode::INTERNAL_SERVER_ERROR
    })? {
        builder.push(&record);
    }
    Ok(Json(builder.finish()))
}

// ============================== 测试通知 ==============================

#[derive(Serialize)]
struct NotifyTestView {
    results: Vec<crate::notify::ChannelTestResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<&'static str>,
}

async fn test_notify(State(state): State<Arc<WebState>>) -> Json<NotifyTestView> {
    let config = state.config_snapshot();
    if !config.notify.enabled {
        return Json(NotifyTestView {
            results: vec![],
            message: Some("通知未启用。请先在配置中启用通知并保存，再进行测试。"),
        });
    }
    let results = NotificationManager::send_test_message(
        &config.notify,
        "这是一条来自 Web 页面的测试通知。收到它说明该通道配置可用。",
    )
    .await;
    Json(NotifyTestView {
        results,
        message: None,
    })
}

/// Resolve the existing independent key before deriving from credentials. This lets
/// CLI recovery read the same cookies as a daemon that never stores a password.
pub(crate) fn prepare_cookie_key(config: &mut AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    if config.cookie_encryption_key.is_some() {
        return Ok(());
    }
    let key_path = std::path::PathBuf::from(format!("{}.key", config.cookie_file));
    let secret = if key_path.exists() {
        read_secret(&key_path)?
    } else if let Ok(secret) = config.cookie_encryption_secret() {
        secret // Keep existing installations' password-derived encryption compatible.
    } else {
        persistent_secret(&key_path)?
    };
    config.cookie_encryption_key = Some(secret);
    Ok(())
}

fn random_secret() -> std::io::Result<String> {
    let mut bytes = [0; 32];
    openssl::rand::rand_bytes(&mut bytes).map_err(std::io::Error::other)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn read_secret(path: &Path) -> std::io::Result<String> {
    if !std::fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(std::io::Error::other("secret path must be a regular file"));
    }
    let value = std::fs::read_to_string(path)?;
    let value = value.trim();
    if value.len() < 32 {
        return Err(std::io::Error::other("stored secret is invalid"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(value.to_owned())
}

fn persistent_secret(path: &Path) -> std::io::Result<String> {
    use std::io::Write;
    if path.exists() {
        return read_secret(path);
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let value = random_secret()?;
    let temporary = path.with_extension(format!("{}.tmp", random_secret()?));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(value.as_bytes())?;
        file.sync_all()?;
        // Publish only a complete file, without replacing another process's key.
        match std::fs::hard_link(&temporary, path) {
            Ok(()) => Ok(value),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read_secret(path),
            Err(e) => Err(e),
        }
    })();
    let _ = std::fs::remove_file(temporary);
    result
}

pub(crate) async fn run(config: AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    let token_path = Path::new(&config.cookie_file).with_file_name("web-access-token");
    let token = match &config.web.access_token {
        Some(token) => token.trim().to_owned(),
        None => persistent_secret(&token_path)?,
    };
    let db = DbService::new(config.database_url.clone()).await?;
    db.init().await?;
    let state = WebState::new(&config, token, db.clone())?;
    let listener = tokio::net::TcpListener::bind(&config.web.bind).await?;
    info!(
        "Web 登录页：http://{} （访问密钥文件：{}）",
        listener.local_addr()?,
        token_path.display()
    );
    if config.web.access_token.is_none() {
        // This capability is intentionally given to the operator, never in a URL or an API response.
        info!("Web 访问密钥：{}", state.token);
    }
    let initial = if Path::new(&config.cookie_file).is_file() {
        Some(Action::Resume)
    } else if config.login_type == crate::config::LoginType::Password {
        config
            .username
            .as_ref()
            .zip(config.password.as_ref())
            .filter(|(u, p)| !u.trim().is_empty() && !p.is_empty())
            .map(|(u, p)| {
                Action::Login(LoginInput {
                    login_type: "password".into(),
                    username: u.clone(),
                    password: p.clone(),
                })
            })
    } else {
        None
    };
    if let Some(action) = initial {
        let _ = state.start(action);
    }

    let server = axum::serve(listener, router(state.clone()));
    let monitor = monitor(state.clone(), db, config);
    let result = tokio::select! {
        result = server => result,
        _ = monitor => Ok(()),
        _ = shutdown() => { info!("收到退出信号，停止 Web 服务与监控"); Ok(()) },
    };
    if let Some(task) = state
        .control
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .task
        .take()
    {
        task.abort();
    }
    result?;
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn monitor(state: Arc<WebState>, db: DbService, config: AppConfig) {
    let mut notifications = NotificationManager::new(config.notify.clone());
    let mut notify_config = config.notify.clone();
    let mut waiting = true;
    let mut previously_monitoring = false;
    loop {
        // 配置管理保存/重载后在此跟随：通知配置变更时重建管理器，间隔下轮生效。
        let config = state.config_snapshot();
        if config.notify != notify_config {
            notify_config = config.notify.clone();
            notifications = NotificationManager::new(config.notify.clone());
            info!("通知配置已变更，通知管理器已按新配置重建");
        }
        let interval = Duration::from_secs(config.interval_seconds);
        let error = state
            .auth_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(error) = error
            && let Some(manager) = &notifications
        {
            manager.notify_login_failure(error).await;
        }
        if let Ok(mut session) = state.session.try_lock() {
            if session.ready {
                if waiting {
                    if let Some(manager) = &mut notifications {
                        manager.reset_fetch_failures();
                        manager.reset_login_retry_failures();
                        if previously_monitoring {
                            manager.notify_reauth_resolved().await;
                        }
                    }
                    waiting = false;
                    previously_monitoring = true;
                }
                // A wake received before acquiring the session is fulfilled by
                // this fetch. Consume it so one manual request cannot fetch twice.
                let _ = tokio::time::timeout(Duration::ZERO, state.wake.notified()).await;
                state
                    .status
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .refreshing = true;
                match session.api.fetch_data().await {
                    Ok(Some(data)) => {
                        if let Err(e) = db.save_data(&data).await {
                            warn!("保存电费数据失败：{e}");
                        }
                        {
                            let mut status = state.status.lock().unwrap_or_else(|e| e.into_inner());
                            status.message = "监控正常，电费数据已更新。".into();
                            status.reading = Some(Reading {
                                money: data.remaining_money,
                                energy: data.remaining_energy,
                                room: data.room_display_name.clone(),
                                sampled_at: crate::time::now_rfc3339(),
                            });
                        }
                        if let Some(manager) = &mut notifications {
                            manager.reset_fetch_failures();
                            manager.check_and_notify(&data).await;
                        }
                    }
                    result => {
                        if session.api.take_reauth_pending() {
                            session.ready = false;
                            waiting = true;
                            state.update(
                                "awaiting_login",
                                "会话已过期，请在此页面重新登录。监控已暂停。",
                            );
                            if let Some(manager) = &mut notifications {
                                manager
                                    .record_web_auth_pending("会话失效，请打开 Web 登录页重新登录")
                                    .await;
                            }
                        } else {
                            // The underlying fetch code handles transport diagnostics; do not expose it in the UI.
                            let _ = result;
                            state.update(
                                "monitoring",
                                "本次采集失败，稍后自动重试；上次读数保留供参考。",
                            );
                            if let Some(manager) = &mut notifications {
                                manager.record_fetch_failure().await;
                            }
                        }
                    }
                }
                state
                    .status
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .refreshing = false;
            } else if let Some(manager) = &mut notifications {
                manager
                    .record_web_auth_pending("监控等待登录，请打开 Web 登录页完成认证")
                    .await;
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(interval) => {},
            _ = state.wake.notified() => {},
        }
    }
}
