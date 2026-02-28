use config::{Config, ConfigError, Environment, File, FileFormat};
use serde::Deserialize;
use std::{fs, path::Path};

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum LoginType {
    #[default]
    Password,
    Wechat,
}

fn default_cookie_file() -> String {
    "uestc_cookies.json".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct AppConfig {
    pub username: Option<String>,
    pub password: Option<String>,
    pub service_url: Option<String>,
    pub database_url: String,
    #[serde(default)]
    pub login_type: LoginType,
    #[serde(default = "default_cookie_file")]
    pub cookie_file: String,
    #[serde(default = "default_interval")]
    pub interval_seconds: u64,
    #[serde(default)]
    pub notify: NotifyConfig,
}

fn default_interval() -> u64 {
    600 // 10 minutes
}

fn default_threshold() -> f64 {
    5.0 // 5 yuan
}

fn default_cooldown_minutes() -> u64 {
    520 // 8 hours 40 minutes
}

fn default_heartbeat_hour() -> u32 {
    9 // 9:00 AM
}

fn default_fetch_failure_threshold() -> u32 {
    3 // 3 consecutive failures
}

fn default_fetch_failure_cooldown_minutes() -> u64 {
    60 // 1 hour
}

fn default_pushover_priority() -> i8 {
    0
}

fn default_pushover_retry() -> u32 {
    60
}

fn default_pushover_expire() -> u32 {
    3600
}

fn default_ntfy_priority() -> u8 {
    3
}

fn default_ntfy_use_markdown() -> bool {
    true
}

fn default_smtp_port() -> u16 {
    587 // Default to STARTTLS port
}

fn default_smtp_encryption() -> SmtpEncryption {
    SmtpEncryption::Starttls
}

#[derive(Debug, Deserialize, Clone, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SmtpEncryption {
    #[default]
    Starttls, // Port 587, STARTTLS
    Tls,  // Port 465, direct TLS
    None, // No encryption (for testing/internal servers)
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct NotifyConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_threshold")]
    pub threshold: f64,
    #[serde(default = "default_cooldown_minutes")]
    pub cooldown_minutes: u64,
    #[serde(default)]
    pub heartbeat_enabled: bool,
    #[serde(default = "default_heartbeat_hour")]
    pub heartbeat_hour: u32,
    #[serde(default)]
    pub login_failure_enabled: bool,
    #[serde(default)]
    pub fetch_failure_enabled: bool,
    #[serde(default = "default_fetch_failure_threshold")]
    pub fetch_failure_threshold: u32,
    #[serde(default = "default_fetch_failure_cooldown_minutes")]
    pub fetch_failure_cooldown_minutes: u64,
    #[serde(default)]
    pub notify_type: NotifyType, // Keep for backward compatibility
    #[serde(default)]
    pub notify_types: Vec<NotifyType>, // New: support multiple channels
    #[serde(default)]
    pub webhook_url: String,
    #[serde(default)]
    pub telegram_bot_token: String,
    #[serde(default)]
    pub telegram_chat_id: String,
    // Pushover configuration
    #[serde(default)]
    pub pushover_api_token: String,
    #[serde(default)]
    pub pushover_user_key: String,
    #[serde(default = "default_pushover_priority")]
    pub pushover_priority: i8,
    #[serde(default = "default_pushover_retry")]
    pub pushover_retry: u32,
    #[serde(default = "default_pushover_expire")]
    pub pushover_expire: u32,
    #[serde(default)]
    pub pushover_url: String,
    // ntfy configuration
    #[serde(default)]
    pub ntfy_topic_url: String,
    #[serde(default = "default_ntfy_priority")]
    pub ntfy_priority: u8,
    #[serde(default)]
    pub ntfy_tags: Vec<String>,
    #[serde(default)]
    pub ntfy_click_action: String,
    #[serde(default)]
    pub ntfy_icon: String,
    #[serde(default)]
    pub ntfy_actions: Vec<serde_json::Value>,
    #[serde(default = "default_ntfy_use_markdown")]
    pub ntfy_use_markdown: bool,
    // Email/SMTP configuration
    #[serde(default)]
    pub smtp_server: String,
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    #[serde(default)]
    pub smtp_username: String,
    #[serde(default)]
    pub smtp_password: String,
    #[serde(default)]
    pub smtp_from: String,
    #[serde(default)]
    pub smtp_to: String, // Comma-separated list of recipients
    #[serde(default = "default_smtp_encryption")]
    pub smtp_encryption: SmtpEncryption,
}

#[derive(Debug, Deserialize, Clone, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum NotifyType {
    #[default]
    Console,
    Webhook,
    Telegram,
    Pushover,
    Ntfy,
    Email,
}

impl NotifyConfig {
    pub fn get_active_notify_types(&self) -> Vec<NotifyType> {
        if !self.notify_types.is_empty() {
            self.notify_types.clone()
        } else {
            vec![self.notify_type.clone()]
        }
    }
}

impl AppConfig {
    pub fn new() -> Result<Self, ConfigError> {
        let mut builder = Config::builder();

        // 1. Load from configuration file (if exists)
        // "config" matches "config.toml", "config.json", etc.
        builder = builder.add_source(File::with_name("config").required(false));

        // 2. Load from Docker Secrets
        // Docker secrets are typically stored in /run/secrets/<secret_name>
        // We read them and add them as a source (overriding config file).
        let secrets = [
            ("username", "/run/secrets/username"),
            ("password", "/run/secrets/password"),
            ("service_url", "/run/secrets/service_url"),
            ("database_url", "/run/secrets/database_url"),
        ];

        let mut secrets_map = std::collections::HashMap::new();
        for (key, path) in secrets {
            if Path::new(path).exists() {
                if let Ok(content) = fs::read_to_string(path) {
                    secrets_map.insert(key, content.trim().to_string());
                }
            }
        }

        if !secrets_map.is_empty() {
            // Construct a TOML string source from the secrets
            let mut toml_str = String::new();
            for (k, v) in secrets_map {
                // Escape string for TOML
                let escaped = v.replace('\\', "\\\\").replace('"', "\\\"");
                toml_str.push_str(&format!("{} = \"{}\"\n", k, escaped));
            }
            builder = builder.add_source(File::from_str(&toml_str, FileFormat::Toml));
        }

        // 3. Load from Environment Variables
        // Prefix "UPM" (Uestc Power Monitor) to avoid collisions.
        // e.g. UPM_USERNAME, UPM_PASSWORD
        // This source is added last, so it overrides Secrets and Config File.
        builder = builder.add_source(
            Environment::with_prefix("UPM")
                .try_parsing(true)
                .separator("__")
                .list_separator(","),
        );

        builder.build()?.try_deserialize()
    }
}
