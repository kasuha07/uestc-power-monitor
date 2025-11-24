use config::{Config, ConfigError, Environment, File, FileFormat};
use serde::Deserialize;
use std::{fs, path::Path};

#[derive(Debug, Deserialize, Clone)]
pub struct AppConfig {
    pub username: String,
    pub password: String,
    pub service_url: Option<String>,
    pub database_url: String,
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
    pub notify_type: NotifyType,
    #[serde(default)]
    pub webhook_url: String,
    #[serde(default)]
    pub telegram_bot_token: String,
    #[serde(default)]
    pub telegram_chat_id: String,
}

#[derive(Debug, Deserialize, Clone, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum NotifyType {
    #[default]
    Console,
    Webhook,
    Telegram,
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
