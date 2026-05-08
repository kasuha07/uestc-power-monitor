use config::{Config, ConfigError, Environment, File, FileFormat};
use serde::Deserialize;
use serde::de::{self, SeqAccess, Unexpected, Visitor};
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

#[derive(Debug, Clone, PartialEq)]
pub struct HeartbeatHours(Vec<u32>);

impl Default for HeartbeatHours {
    fn default() -> Self {
        Self(vec![default_heartbeat_hour()])
    }
}

impl HeartbeatHours {
    pub fn as_slice(&self) -> &[u32] {
        &self.0
    }

    pub fn contains(&self, hour: u32) -> bool {
        self.0.contains(&hour)
    }
}

fn dedup_hours(hours: Vec<u32>) -> Vec<u32> {
    let mut unique = Vec::new();
    for hour in hours {
        if !unique.contains(&hour) {
            unique.push(hour);
        }
    }
    unique
}

fn parse_heartbeat_hours<E>(value: &str) -> Result<Vec<u32>, E>
where
    E: de::Error,
{
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(E::custom("heartbeat hour list cannot be empty"));
    }

    let content = if trimmed.starts_with('[') && trimmed.ends_with(']') {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };

    let mut hours = Vec::new();
    for part in content.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(E::custom("heartbeat hour entries cannot be empty"));
        }

        let hour = part.parse::<u32>().map_err(|_| {
            E::custom(format!(
                "invalid heartbeat hour '{}', expected integer(s) in range 0..=23",
                part
            ))
        })?;
        hours.push(hour);
    }

    if hours.is_empty() {
        return Err(E::custom("heartbeat hour list cannot be empty"));
    }

    Ok(dedup_hours(hours))
}

impl<'de> Deserialize<'de> for HeartbeatHours {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum HourValue {
            Number(u32),
            Text(String),
        }

        struct HeartbeatHoursVisitor;

        impl<'de> Visitor<'de> for HeartbeatHoursVisitor {
            type Value = HeartbeatHours;

            fn expecting(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
                formatter
                    .write_str("a single hour, a comma-separated hour list, or an array of hours")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let hour = u32::try_from(value).map_err(|_| {
                    E::invalid_value(
                        Unexpected::Unsigned(value),
                        &"a 32-bit unsigned integer in range 0..=23",
                    )
                })?;
                Ok(HeartbeatHours(vec![hour]))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value < 0 {
                    return Err(E::invalid_value(
                        Unexpected::Signed(value),
                        &"a non-negative integer in range 0..=23",
                    ));
                }
                self.visit_u64(value as u64)
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(HeartbeatHours(parse_heartbeat_hours::<E>(value)?))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.visit_str(&value)
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut hours = Vec::new();
                while let Some(value) = seq.next_element::<HourValue>()? {
                    match value {
                        HourValue::Number(hour) => hours.push(hour),
                        HourValue::Text(text) => {
                            hours.extend(parse_heartbeat_hours::<A::Error>(&text)?);
                        }
                    }
                }

                if hours.is_empty() {
                    return Err(de::Error::custom("heartbeat hour list cannot be empty"));
                }

                Ok(HeartbeatHours(dedup_hours(hours)))
            }
        }

        deserializer.deserialize_any(HeartbeatHoursVisitor)
    }
}

fn default_fetch_failure_threshold() -> u32 {
    3 // 3 consecutive failures
}

fn default_fetch_failure_cooldown_minutes() -> u64 {
    60 // 1 hour
}

fn default_notify_retry_attempts() -> u32 {
    3
}

fn default_notify_retry_initial_delay_seconds() -> u64 {
    2
}

fn default_notify_retry_max_delay_seconds() -> u64 {
    60
}

fn default_notify_request_timeout_seconds() -> u64 {
    15
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

#[derive(Debug, Deserialize, Clone)]
pub struct NotifyConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_threshold")]
    pub threshold: f64,
    #[serde(default = "default_cooldown_minutes")]
    pub cooldown_minutes: u64,
    #[serde(default)]
    pub heartbeat_enabled: bool,
    #[serde(default, alias = "heartbeat_hour")]
    pub heartbeat_hours: HeartbeatHours,
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
    #[serde(default = "default_notify_retry_attempts")]
    pub retry_attempts: u32,
    #[serde(default = "default_notify_retry_initial_delay_seconds")]
    pub retry_initial_delay_seconds: u64,
    #[serde(default = "default_notify_retry_max_delay_seconds")]
    pub retry_max_delay_seconds: u64,
    #[serde(default = "default_notify_request_timeout_seconds")]
    pub request_timeout_seconds: u64,
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

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold: default_threshold(),
            cooldown_minutes: default_cooldown_minutes(),
            heartbeat_enabled: false,
            heartbeat_hours: HeartbeatHours::default(),
            startup_enabled: false,
            login_failure_enabled: false,
            fetch_failure_enabled: false,
            fetch_failure_threshold: default_fetch_failure_threshold(),
            fetch_failure_cooldown_minutes: default_fetch_failure_cooldown_minutes(),
            retry_attempts: default_notify_retry_attempts(),
            retry_initial_delay_seconds: default_notify_retry_initial_delay_seconds(),
            retry_max_delay_seconds: default_notify_retry_max_delay_seconds(),
            request_timeout_seconds: default_notify_request_timeout_seconds(),
            notify_type: NotifyType::default(),
            notify_types: Vec::new(),
            webhook_url: String::new(),
            telegram_bot_token: String::new(),
            telegram_chat_id: String::new(),
            pushover_api_token: String::new(),
            pushover_user_key: String::new(),
            pushover_priority: default_pushover_priority(),
            pushover_retry: default_pushover_retry(),
            pushover_expire: default_pushover_expire(),
            pushover_url: String::new(),
            ntfy_topic_url: String::new(),
            ntfy_token: String::new(),
            ntfy_priority: default_ntfy_priority(),
            ntfy_tags: Vec::new(),
            ntfy_click_action: String::new(),
            ntfy_icon: String::new(),
            ntfy_actions: Vec::new(),
            ntfy_use_markdown: default_ntfy_use_markdown(),
            smtp_server: String::new(),
            smtp_port: default_smtp_port(),
            smtp_username: String::new(),
            smtp_password: String::new(),
            smtp_from: String::new(),
            smtp_to: String::new(),
            smtp_encryption: default_smtp_encryption(),
        }
    }
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
        if self
            .notify
            .heartbeat_hours
            .as_slice()
            .iter()
            .any(|&hour| hour > 23)
        {
            errors.push(
                "notify.heartbeat_hours / notify.heartbeat_hour must contain only values in range 0..=23"
                    .to_string(),
            );
        }

        if self.notify.fetch_failure_threshold == 0 {
            errors.push("notify.fetch_failure_threshold must be greater than 0".to_string());
        }

        if self.notify.retry_attempts == 0 {
            errors.push("notify.retry_attempts must be greater than 0".to_string());
        }

        if self.notify.retry_initial_delay_seconds == 0 {
            errors.push("notify.retry_initial_delay_seconds must be greater than 0".to_string());
        }

        if self.notify.retry_max_delay_seconds == 0 {
            errors.push("notify.retry_max_delay_seconds must be greater than 0".to_string());
        }

        if self.notify.request_timeout_seconds == 0 {
            errors.push("notify.request_timeout_seconds must be greater than 0".to_string());
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
        let _heartbeat_hours_guard = EnvVarGuard::set("UPM_NOTIFY__HEARTBEAT_HOURS", "9,21");

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
        assert_eq!(cfg.notify.heartbeat_hours.as_slice(), &[9, 21]);
    }

    #[test]
    fn heartbeat_hours_support_single_value_and_array_aliases() {
        let _lock = CONFIG_TEST_MUTEX.lock().expect("lock config test mutex");
        let _guard = setup_test_config(
            r#"
database_url = "sqlite://test.db"

[notify]
heartbeat_hour = [9, 21, 9]
"#,
            None,
        );

        let cfg = AppConfig::new().expect("load config");
        assert_eq!(cfg.notify.heartbeat_hours.as_slice(), &[9, 21]);
    }

    #[test]
    fn heartbeat_hours_support_plural_key() {
        let _lock = CONFIG_TEST_MUTEX.lock().expect("lock config test mutex");
        let _guard = setup_test_config(
            r#"
database_url = "sqlite://test.db"

[notify]
heartbeat_hours = 8
"#,
            None,
        );

        let cfg = AppConfig::new().expect("load config");
        assert_eq!(cfg.notify.heartbeat_hours.as_slice(), &[8]);
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
        cfg.notify.heartbeat_hours = HeartbeatHours(vec![9, 24]);

        let err = cfg.validate().expect_err("validation should fail");
        assert!(err.to_string().contains("notify.heartbeat_hours"));
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
