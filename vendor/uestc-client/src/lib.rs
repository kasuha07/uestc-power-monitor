mod client;
mod core;

#[cfg(feature = "async")]
pub use client::UestcClient;

#[cfg(feature = "blocking")]
pub use client::UestcBlockingClient;

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
