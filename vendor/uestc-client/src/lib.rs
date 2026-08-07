mod client;
mod core;

#[cfg(feature = "async")]
pub use client::UestcClient;

#[cfg(feature = "blocking")]
pub use client::UestcBlockingClient;

pub use core::reauth::{ReauthContext, ReauthMethod, ReauthMethodKind};

use thiserror::Error;

#[derive(Error, Debug)]
pub enum UestcClientError {
    #[error("Network error: {message}")]
    NetworkError {
        message: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("HTML parsing failed: {message}")]
    HtmlParseError {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    #[error("XML parsing failed: {message}")]
    XmlParseError {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    #[error("Password encryption failed: {message}")]
    CryptoError {
        message: String,
        key_length: Option<usize>,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    #[error("Login failed: {message}")]
    LoginFailed {
        message: String,
        username: Option<String>,
    },

    /// 密码校验通过（TGT 已发）但被多因子策略锁定，需要完成 reauth 第二因素。
    ///
    /// 调用方拿到 `context` 后按需编排：
    /// `change_reauth_type` → `send_reauth_code` → `submit_reauth`。
    #[error(
        "Multi-factor authentication (reauth) required; complete it via change_reauth_type/send_reauth_code/submit_reauth"
    )]
    ReauthRequired {
        /// reauth 会话上下文（可用方式列表 + 服务端渲染参数）。
        /// 装箱以控制错误变体体积（`result_large_err`）。
        context: Box<ReauthContext>,
    },

    /// reauth 提交/切换/发码被服务端拒绝。
    #[error("Reauth failed: {message}")]
    ReauthFailed { message: String },

    #[error("Logout failed: {message}")]
    LogoutFailed { message: String },

    #[error("Cookie operation failed: {operation} - {message}")]
    CookieError {
        operation: String,
        file_path: Option<String>,
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    #[error("Session expired or invalid")]
    SessionExpired,

    #[error("WeChat QR code operation failed: {message}")]
    WeChatError { message: String },

    #[error("Client initialization failed: {message}")]
    ClientInitError { message: String },
}

// Helper implementations for backward compatibility
impl From<reqwest::Error> for UestcClientError {
    fn from(err: reqwest::Error) -> Self {
        UestcClientError::NetworkError {
            message: err.to_string(),
            source: err,
        }
    }
}

pub type Result<T> = std::result::Result<T, UestcClientError>;
