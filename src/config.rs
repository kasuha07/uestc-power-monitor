use config::{Config, ConfigError, Environment, File, FileFormat};
use serde::Deserialize;
use std::error::Error;
use std::fmt::{Display, Formatter};
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
    #[serde(default = "default_timezone")]
    pub timezone: String,
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

fn default_timezone() -> String {
    "Asia/Shanghai".to_string()
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
    None, // Deprecated/insecure; parsed for compatibility but rejected at runtime
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
    pub startup_enabled: bool,
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
    #[serde(default)]
    pub ntfy_token: String,
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
        let source = if !self.notify_types.is_empty() {
            self.notify_types.clone()
        } else {
            vec![self.notify_type.clone()]
        };

        let mut unique = Vec::new();
        for notify_type in source {
            if !unique.contains(&notify_type) {
                unique.push(notify_type);
            }
        }
        unique
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigValidationError {
    errors: Vec<String>,
}

impl ConfigValidationError {
    fn new(errors: Vec<String>) -> Self {
        Self { errors }
    }
}

impl Display for ConfigValidationError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if self.errors.len() == 1 {
            write!(f, "Configuration validation failed: {}", self.errors[0])
        } else {
            writeln!(
                f,
                "Configuration validation failed with {} issue(s):",
                self.errors.len()
            )?;
            for (idx, error) in self.errors.iter().enumerate() {
                writeln!(f, "  {}. {}", idx + 1, error)?;
            }
            Ok(())
        }
    }
}

impl Error for ConfigValidationError {}

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
                .prefix_separator("_")
                .try_parsing(true)
                .separator("__")
                .list_separator(",")
                .with_list_parse_key("notify.notify_types")
                .with_list_parse_key("notify.ntfy_tags"),
        );

        let mut cfg: AppConfig = builder.build()?.try_deserialize()?;
        if let Ok(tz) = std::env::var("UPM_TIMEZONE") {
            let tz = tz.trim();
            if !tz.is_empty() {
                cfg.timezone = tz.to_string();
            }
        }

        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), ConfigValidationError> {
        let mut errors = Vec::new();

        if self.interval_seconds == 0 {
            errors.push("interval_seconds must be greater than 0".to_string());
        }

        if self.database_url.trim().is_empty() {
            errors.push("database_url cannot be empty".to_string());
        }

        if self.cookie_file.trim().is_empty() {
            errors.push("cookie_file cannot be empty".to_string());
        }

        // Static validation only: avoid runtime/business dependency checks.
        if self.notify.heartbeat_hour > 23 {
            errors.push("notify.heartbeat_hour must be in range 0..=23".to_string());
        }

        if self.notify.fetch_failure_threshold == 0 {
            errors.push("notify.fetch_failure_threshold must be greater than 0".to_string());
        }

        if self.notify.smtp_encryption == SmtpEncryption::None {
            errors.push(
                "notify.smtp_encryption=none is not allowed; use starttls or tls".to_string(),
            );
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ConfigValidationError::new(errors))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::{LazyLock, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    static CONFIG_TEST_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    struct TestConfigGuard {
        original_cwd: PathBuf,
        original_upm_timezone: Option<String>,
        temp_dir: PathBuf,
    }

    impl Drop for TestConfigGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original_cwd);
            match &self.original_upm_timezone {
                Some(value) => {
                    // SAFETY: test code runs under a global mutex to avoid concurrent env writes.
                    unsafe { std::env::set_var("UPM_TIMEZONE", value) }
                }
                None => {
                    // SAFETY: test code runs under a global mutex to avoid concurrent env writes.
                    unsafe { std::env::remove_var("UPM_TIMEZONE") }
                }
            }
            let _ = std::fs::remove_dir_all(&self.temp_dir);
        }
    }

    fn setup_test_config(config_toml: &str, upm_timezone: Option<&str>) -> TestConfigGuard {
        let original_cwd = std::env::current_dir().expect("read current dir");
        let original_upm_timezone = std::env::var("UPM_TIMEZONE").ok();

        let uniq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before epoch")
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!("upm-config-test-{uniq}"));
        std::fs::create_dir_all(&temp_dir).expect("create temp dir");
        std::fs::write(temp_dir.join("config.toml"), config_toml).expect("write config.toml");
        std::env::set_current_dir(&temp_dir).expect("enter temp dir");

        match upm_timezone {
            Some(value) => {
                // SAFETY: test code runs under a global mutex to avoid concurrent env writes.
                unsafe { std::env::set_var("UPM_TIMEZONE", value) }
            }
            None => {
                // SAFETY: test code runs under a global mutex to avoid concurrent env writes.
                unsafe { std::env::remove_var("UPM_TIMEZONE") }
            }
        }

        TestConfigGuard {
            original_cwd,
            original_upm_timezone,
            temp_dir,
        }
    }

    #[test]
    fn timezone_defaults_to_asia_shanghai_when_missing() {
        let _lock = CONFIG_TEST_MUTEX.lock().expect("lock config test mutex");
        let _guard = setup_test_config("database_url = \"sqlite://test.db\"\n", None);
        let cfg = AppConfig::new().expect("load config");
        assert_eq!(cfg.timezone, "Asia/Shanghai");
    }

    #[test]
    fn env_timezone_overrides_config_timezone() {
        let _lock = CONFIG_TEST_MUTEX.lock().expect("lock config test mutex");
        let _guard = setup_test_config(
            "database_url = \"sqlite://test.db\"\ntimezone = \"UTC\"\n",
            Some("Asia/Tokyo"),
        );
        let cfg = AppConfig::new().expect("load config");
        assert_eq!(cfg.timezone, "Asia/Tokyo");
    }

    struct EnvVarGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            // SAFETY: test code runs under a global mutex to avoid concurrent env writes.
            unsafe { std::env::set_var(key, value) };
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(value) => {
                    // SAFETY: test code runs under a global mutex to avoid concurrent env writes.
                    unsafe { std::env::set_var(self.key, value) };
                }
                None => {
                    // SAFETY: test code runs under a global mutex to avoid concurrent env writes.
                    unsafe { std::env::remove_var(self.key) };
                }
            }
        }
    }

    #[test]
    fn env_database_url_and_nested_notify_fields_load_with_upm_prefix() {
        let _lock = CONFIG_TEST_MUTEX.lock().expect("lock config test mutex");
        let _guard = setup_test_config("", None);
        let _db_guard = EnvVarGuard::set("UPM_DATABASE_URL", "sqlite://env.db");
        let _notify_guard = EnvVarGuard::set("UPM_NOTIFY__ENABLED", "true");
        let _notify_types_guard = EnvVarGuard::set("UPM_NOTIFY__NOTIFY_TYPES", "telegram,ntfy");
        let _ntfy_tags_guard = EnvVarGuard::set("UPM_NOTIFY__NTFY_TAGS", "warning,zap");

        let cfg = AppConfig::new().expect("load config from env");

        assert_eq!(cfg.database_url, "sqlite://env.db");
        assert!(cfg.notify.enabled);
        assert_eq!(
            cfg.notify.notify_types,
            vec![NotifyType::Telegram, NotifyType::Ntfy]
        );
        assert_eq!(
            cfg.notify.ntfy_tags,
            vec!["warning".to_string(), "zap".to_string()]
        );
    }

    fn valid_app_config() -> AppConfig {
        let notify = NotifyConfig {
            fetch_failure_threshold: default_fetch_failure_threshold(),
            ..Default::default()
        };

        AppConfig {
            username: Some("alice".to_string()),
            password: Some("secret".to_string()),
            service_url: None,
            database_url: "sqlite://test.db".to_string(),
            timezone: "Asia/Shanghai".to_string(),
            login_type: LoginType::Password,
            cookie_file: "cookies.json".to_string(),
            interval_seconds: 600,
            notify,
        }
    }

    #[test]
    fn validate_rejects_zero_interval() {
        let mut cfg = valid_app_config();
        cfg.interval_seconds = 0;

        let err = cfg.validate().expect_err("validation should fail");
        assert!(err.to_string().contains("interval_seconds"));
    }

    #[test]
    fn validate_allows_missing_password_login_credentials_for_static_validation() {
        let mut cfg = valid_app_config();
        cfg.username = None;
        cfg.password = Some("   ".to_string());

        cfg.validate()
            .expect("static validation should not enforce runtime login credentials");
    }

    #[test]
    fn validate_allows_missing_webhook_url_for_static_validation() {
        let mut cfg = valid_app_config();
        cfg.notify.enabled = true;
        cfg.notify.notify_types = vec![NotifyType::Webhook];

        cfg.validate()
            .expect("static validation should not enforce runtime notifier credentials");
    }

    #[test]
    fn validate_rejects_invalid_heartbeat_hour() {
        let mut cfg = valid_app_config();
        cfg.notify.heartbeat_hour = 24;

        let err = cfg.validate().expect_err("validation should fail");
        assert!(err.to_string().contains("notify.heartbeat_hour"));
    }

    #[test]
    fn validate_rejects_zero_fetch_failure_threshold() {
        let mut cfg = valid_app_config();
        cfg.notify.fetch_failure_threshold = 0;

        let err = cfg.validate().expect_err("validation should fail");
        assert!(err.to_string().contains("notify.fetch_failure_threshold"));
    }

    #[test]
    fn validate_rejects_insecure_smtp_encryption_mode() {
        let mut cfg = valid_app_config();
        cfg.notify.smtp_encryption = SmtpEncryption::None;

        let err = cfg.validate().expect_err("validation should fail");
        assert!(err.to_string().contains("notify.smtp_encryption=none"));
    }

    #[test]
    fn active_notify_types_are_deduplicated_preserving_order() {
        let mut notify = NotifyConfig::default();
        notify.notify_types = vec![
            NotifyType::Telegram,
            NotifyType::Webhook,
            NotifyType::Telegram,
            NotifyType::Webhook,
            NotifyType::Email,
        ];

        assert_eq!(
            notify.get_active_notify_types(),
            vec![NotifyType::Telegram, NotifyType::Webhook, NotifyType::Email]
        );
    }
}
