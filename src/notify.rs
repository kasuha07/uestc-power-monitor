use crate::api::PowerInfo;
use crate::config::{NotifyConfig, NotifyType};
use crate::time;
use crate::utils::retry;
use chrono::Timelike;
use chrono_tz::Tz;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Tokio1Executor,
    message::{Message, header::ContentType},
    transport::smtp::authentication::Credentials,
};
use serde_json;
use std::collections::HashMap;
use std::error::Error;
use std::future::Future;
use std::net::IpAddr;
use std::net::ToSocketAddrs;
use std::pin::Pin;
use std::time::Duration;
use tracing::{debug, error, info, warn};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NotificationEvent {
    LowBalance,
    Heartbeat,
    LoginFailure,
    ConsecutiveFetchFailures,
}

pub struct NotificationManager {
    config: NotifyConfig,
    notifiers: Vec<Box<dyn Notifier>>,
    last_low_balance_notify_time: Option<chrono::DateTime<Tz>>,
    last_heartbeat_date: Option<chrono::NaiveDate>,
    last_balance: Option<f64>,
    consecutive_fetch_failures: u32,
    last_fetch_failure_notify_time: Option<chrono::DateTime<Tz>>,
}

impl NotificationManager {
    pub fn new(config: NotifyConfig) -> Option<Self> {
        if !config.enabled {
            debug!("Notifications disabled");
            return None;
        }

        let notify_types = config.get_active_notify_types();
        let mut notifiers = Vec::new();

        for notify_type in notify_types {
            if let Some(notifier) = create_single_notifier(&config, notify_type) {
                notifiers.push(notifier);
            }
        }

        if notifiers.is_empty() {
            warn!("No valid notifiers configured");
            return None;
        }

        Some(Self {
            config,
            notifiers,
            last_low_balance_notify_time: None,
            last_heartbeat_date: None,
            last_balance: None,
            consecutive_fetch_failures: 0,
            last_fetch_failure_notify_time: None,
        })
    }

    async fn notify_all(&self, data: &PowerInfo, event: NotificationEvent) {
        for (idx, notifier) in self.notifiers.iter().enumerate() {
            if retry(|| notifier.notify(data, event), 3, Duration::from_secs(2))
                .await
                .is_err()
            {
                error!("Notifier {} failed: request error (details redacted)", idx);
            }
        }
    }

    async fn notify_error_all(&self, error_msg: &str, event: NotificationEvent) {
        for (idx, notifier) in self.notifiers.iter().enumerate() {
            if retry(
                || notifier.notify_error(error_msg, event),
                3,
                Duration::from_secs(2),
            )
            .await
            .is_err()
            {
                error!("Notifier {} failed: request error (details redacted)", idx);
            }
        }
    }

    pub async fn check_and_notify(&mut self, data: &PowerInfo) {
        let now = time::now();
        debug!("Checking notification conditions at {}", now);

        // Heartbeat Check
        if self.config.enabled && self.config.heartbeat_enabled {
            debug!("Heartbeat enabled, checking conditions...");
            if now.hour() == self.config.heartbeat_hour {
                let today = now.date_naive();
                if self.last_heartbeat_date != Some(today) {
                    info!("Sending daily heartbeat...");
                    self.notify_all(data, NotificationEvent::Heartbeat).await;
                    self.last_heartbeat_date = Some(today);
                    debug!("Heartbeat sent successfully");
                } else {
                    debug!("Heartbeat already sent today");
                }
            } else {
                debug!(
                    "Not heartbeat hour yet (current: {}, target: {})",
                    now.hour(),
                    self.config.heartbeat_hour
                );
            }
        }

        // Low Balance Check
        if self.config.enabled {
            let current_balance = data.remaining_money;
            let threshold = self.config.threshold;
            let is_low = current_balance <= threshold;
            debug!(
                "Balance check: current={:.2}, threshold={:.2}, is_low={}",
                current_balance, threshold, is_low
            );

            let should_notify = if is_low {
                if let Some(last_b) = self.last_balance {
                    if last_b > threshold {
                        // Edge trigger: changed from high to low
                        debug!("Balance dropped below threshold (edge trigger)");
                        true
                    } else {
                        // Already low, check cooldown
                        if let Some(last_time) = self.last_low_balance_notify_time {
                            let elapsed = now.signed_duration_since(last_time);
                            let should =
                                elapsed.num_minutes() >= self.config.cooldown_minutes as i64;
                            debug!(
                                "Balance still low, cooldown check: elapsed={}min, cooldown={}min, should_notify={}",
                                elapsed.num_minutes(),
                                self.config.cooldown_minutes,
                                should
                            );
                            should
                        } else {
                            // Should not happen if logic is correct, but safe fallback
                            debug!("Balance low but no last notify time (fallback)");
                            true
                        }
                    }
                } else {
                    // First run and low
                    debug!("First run with low balance");
                    true
                }
            } else {
                debug!("Balance is above threshold, no notification needed");
                false
            };

            if should_notify {
                debug!("Sending low balance notification...");
                self.notify_all(data, NotificationEvent::LowBalance).await;
                self.last_low_balance_notify_time = Some(now);
                debug!("Low balance notification sent successfully");
            }

            self.last_balance = Some(current_balance);
        }
    }

    pub async fn notify_login_failure(&self, error_msg: &str) {
        if !self.config.enabled || !self.config.login_failure_enabled {
            return;
        }

        info!("Sending login failure notification...");
        self.notify_error_all(error_msg, NotificationEvent::LoginFailure)
            .await;
        debug!("Login failure notification sent successfully");
    }

    pub async fn record_fetch_failure(&mut self) {
        self.consecutive_fetch_failures += 1;
        debug!(
            "Consecutive fetch failures: {}",
            self.consecutive_fetch_failures
        );

        if !self.config.enabled || !self.config.fetch_failure_enabled {
            return;
        }

        if self.consecutive_fetch_failures >= self.config.fetch_failure_threshold {
            let now = time::now();
            let should_notify = if let Some(last_time) = self.last_fetch_failure_notify_time {
                let elapsed = now.signed_duration_since(last_time);
                elapsed.num_minutes() >= self.config.fetch_failure_cooldown_minutes as i64
            } else {
                true
            };

            if should_notify {
                info!(
                    "Sending consecutive fetch failures notification (count: {})...",
                    self.consecutive_fetch_failures
                );
                let error_msg = format!(
                    "Failed to fetch data {} times consecutively",
                    self.consecutive_fetch_failures
                );
                self.notify_error_all(&error_msg, NotificationEvent::ConsecutiveFetchFailures)
                    .await;
                self.last_fetch_failure_notify_time = Some(now);
                debug!("Consecutive fetch failures notification sent successfully");
            }
        }
    }

    pub fn reset_fetch_failures(&mut self) {
        if self.consecutive_fetch_failures > 0 {
            debug!(
                "Resetting consecutive fetch failures counter (was: {})",
                self.consecutive_fetch_failures
            );
            self.consecutive_fetch_failures = 0;
        }
    }
}

pub trait Notifier: Send + Sync {
    fn notify<'a>(
        &'a self,
        info: &'a PowerInfo,
        event: NotificationEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error>>> + Send + 'a>>;

    fn notify_error<'a>(
        &'a self,
        error_msg: &'a str,
        event: NotificationEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error>>> + Send + 'a>>;
}

pub fn create_single_notifier(
    config: &NotifyConfig,
    notify_type: NotifyType,
) -> Option<Box<dyn Notifier>> {
    debug!("Creating notifier of type: {:?}", notify_type);

    match notify_type {
        NotifyType::Console => Some(Box::new(ConsoleNotifier)),
        NotifyType::Webhook => {
            if config.webhook_url.is_empty() {
                warn!("Webhook notifier skipped: webhook_url is not configured");
                return None;
            }

            let webhook_url = match validate_https_public_url(&config.webhook_url, "webhook_url") {
                Some(url) => url,
                None => return None,
            };

            Some(Box::new(WebhookNotifier::new(webhook_url.to_string())))
        }
        NotifyType::Telegram => {
            if config.telegram_bot_token.is_empty() || config.telegram_chat_id.is_empty() {
                warn!(
                    "Telegram notifier skipped: telegram_bot_token or telegram_chat_id is not configured"
                );
                return None;
            }
            Some(Box::new(TelegramNotifier::new(
                config.telegram_bot_token.clone(),
                config.telegram_chat_id.clone(),
            )))
        }
        NotifyType::Pushover => {
            if config.pushover_api_token.is_empty() || config.pushover_user_key.is_empty() {
                warn!(
                    "Pushover notifier skipped: pushover_api_token or pushover_user_key is not configured"
                );
                return None;
            }
            Some(Box::new(PushoverNotifier::new(
                config.pushover_api_token.clone(),
                config.pushover_user_key.clone(),
                config.pushover_priority,
                config.pushover_retry,
                config.pushover_expire,
                optional_string(&config.pushover_url),
            )))
        }
        NotifyType::Ntfy => {
            if config.ntfy_topic_url.is_empty() {
                warn!("ntfy notifier skipped: ntfy_topic_url is not configured");
                return None;
            }

            let topic_url =
                match validate_https_public_url(&config.ntfy_topic_url, "ntfy_topic_url") {
                    Some(url) => url,
                    None => return None,
                };

            Some(Box::new(NtfyNotifier::new(
                topic_url.to_string(),
                optional_string(&config.ntfy_token),
                config.ntfy_priority,
                config.ntfy_tags.clone(),
                optional_string(&config.ntfy_click_action),
                optional_string(&config.ntfy_icon),
                config.ntfy_actions.clone(),
                config.ntfy_use_markdown,
            )))
        }
        NotifyType::Email => match EmailNotifier::new(config) {
            Ok(notifier) => Some(Box::new(notifier)),
            Err(e) => {
                warn!("Email notifier skipped: {}", e);
                None
            }
        },
    }
}

// Backward compatibility wrapper
pub fn create_notifier(config: &NotifyConfig) -> Option<Box<dyn Notifier>> {
    if !config.enabled {
        debug!("Notifications disabled");
        return None;
    }
    create_single_notifier(config, config.notify_type.clone())
}

fn optional_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn validate_https_public_url(raw_url: &str, field_name: &str) -> Option<reqwest::Url> {
    let url = match reqwest::Url::parse(raw_url) {
        Ok(url) => url,
        Err(_) => {
            warn!(
                "{} notifier skipped: {} is not a valid URL",
                field_name, field_name
            );
            return None;
        }
    };

    if url.scheme() != "https" {
        warn!(
            "{} notifier skipped: {} must use https",
            field_name, field_name
        );
        return None;
    }

    let host = match url.host_str() {
        Some(host) => host,
        None => {
            warn!(
                "{} notifier skipped: {} must include a host",
                field_name, field_name
            );
            return None;
        }
    };

    if is_disallowed_host(host) || host_resolves_to_disallowed_ip(host) {
        warn!(
            "{} notifier skipped: {} host is not allowed",
            field_name, field_name
        );
        return None;
    }

    Some(url)
}

fn ensure_https_public_url(raw_url: &str, field_name: &str) -> Result<(), std::io::Error> {
    match validate_https_public_url(raw_url, field_name) {
        Some(_) => Ok(()),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} is blocked by SSRF protection", field_name),
        )),
    }
}

async fn ensure_https_public_url_async(
    raw_url: &str,
    field_name: &str,
) -> Result<(), std::io::Error> {
    let raw_url = raw_url.to_string();
    let field_name_owned = field_name.to_string();
    let field_name_for_error = field_name_owned.clone();
    tokio::task::spawn_blocking(move || ensure_https_public_url(&raw_url, &field_name_owned))
        .await
        .map_err(|e| {
            std::io::Error::other(format!(
                "failed to validate {} in blocking task: {}",
                field_name_for_error, e
            ))
        })??;
    Ok(())
}

fn is_disallowed_host(host: &str) -> bool {
    let host_lower = host.to_ascii_lowercase();
    if host_lower == "localhost" || host_lower.ends_with(".local") {
        return true;
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        match ip {
            IpAddr::V4(ipv4) => {
                ipv4.is_private()
                    || ipv4.is_loopback()
                    || ipv4.is_link_local()
                    || ipv4.is_multicast()
                    || ipv4.is_unspecified()
                    || ipv4.is_broadcast()
            }
            IpAddr::V6(ipv6) => {
                if let Some(mapped_v4) = ipv6.to_ipv4_mapped() {
                    return mapped_v4.is_private()
                        || mapped_v4.is_loopback()
                        || mapped_v4.is_link_local()
                        || mapped_v4.is_multicast()
                        || mapped_v4.is_unspecified()
                        || mapped_v4.is_broadcast();
                }
                ipv6.is_loopback()
                    || ipv6.is_unique_local()
                    || ipv6.is_unicast_link_local()
                    || ipv6.is_multicast()
                    || ipv6.is_unspecified()
            }
        }
    } else {
        false
    }
}

fn host_resolves_to_disallowed_ip(host: &str) -> bool {
    let addr = format!("{}:443", host);
    match addr.to_socket_addrs() {
        Ok(mut addrs) => {
            let mut saw_any = false;
            for socket_addr in addrs.by_ref() {
                saw_any = true;
                if is_disallowed_host(&socket_addr.ip().to_string()) {
                    return true;
                }
            }
            !saw_any
        }
        Err(_) => true,
    }
}

fn create_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("failed to build reqwest client with timeout")
}

fn create_untrusted_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build reqwest client with strict redirect policy")
}

pub struct ConsoleNotifier;

impl Notifier for ConsoleNotifier {
    fn notify<'a>(
        &'a self,
        info: &'a PowerInfo,
        event: NotificationEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error>>> + Send + 'a>> {
        Box::pin(async move {
            match event {
                NotificationEvent::LowBalance => {
                    warn!(
                        "UESTC Power Monitor ⚠️ [Low Power Warning] Room: {}, Money: {:.2} CNY, Energy: {:.2} kWh",
                        info.room_display_name, info.remaining_money, info.remaining_energy
                    );
                }
                NotificationEvent::Heartbeat => {
                    info!(
                        "UESTC Power Monitor ℹ️ [Daily Report] Room: {}, Money: {:.2} CNY, Energy: {:.2} kWh",
                        info.room_display_name, info.remaining_money, info.remaining_energy
                    );
                }
                NotificationEvent::LoginFailure | NotificationEvent::ConsecutiveFetchFailures => {
                    // These events use notify_error instead
                }
            }
            Ok(())
        })
    }

    fn notify_error<'a>(
        &'a self,
        error_msg: &'a str,
        event: NotificationEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error>>> + Send + 'a>> {
        Box::pin(async move {
            match event {
                NotificationEvent::LoginFailure => {
                    error!("UESTC Power Monitor 🔐 [Login Failure] {}", error_msg);
                }
                NotificationEvent::ConsecutiveFetchFailures => {
                    error!("UESTC Power Monitor ❌ [Fetch Failures] {}", error_msg);
                }
                NotificationEvent::LowBalance | NotificationEvent::Heartbeat => {
                    // These events use notify instead
                }
            }
            Ok(())
        })
    }
}

pub struct WebhookNotifier {
    client: reqwest::Client,
    url: String,
}

impl WebhookNotifier {
    pub fn new(url: String) -> Self {
        Self {
            client: create_untrusted_http_client(),
            url,
        }
    }
}

impl Notifier for WebhookNotifier {
    fn notify<'a>(
        &'a self,
        info: &'a PowerInfo,
        event: NotificationEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error>>> + Send + 'a>> {
        Box::pin(async move {
            let event_str = match event {
                NotificationEvent::LowBalance => "low_balance",
                NotificationEvent::Heartbeat => "heartbeat",
                NotificationEvent::LoginFailure | NotificationEvent::ConsecutiveFetchFailures => {
                    return Ok(()); // These events use notify_error instead
                }
            };
            ensure_https_public_url_async(&self.url, "webhook_url").await?;
            debug!("Sending webhook notification: event={}", event_str);
            self.client
                .post(&self.url)
                .header("X-Event-Type", event_str)
                .json(info)
                .send()
                .await?
                .error_for_status()?;
            debug!("Webhook notification sent successfully");
            Ok(())
        })
    }

    fn notify_error<'a>(
        &'a self,
        error_msg: &'a str,
        event: NotificationEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error>>> + Send + 'a>> {
        Box::pin(async move {
            let event_str = match event {
                NotificationEvent::LoginFailure => "login_failure",
                NotificationEvent::ConsecutiveFetchFailures => "consecutive_fetch_failures",
                NotificationEvent::LowBalance | NotificationEvent::Heartbeat => {
                    return Ok(()); // These events use notify instead
                }
            };

            let payload = serde_json::json!({
                "title": "UESTC Power Monitor",
                "event": event_str,
                "error": error_msg,
                "timestamp": time::now_rfc3339(),
            });

            ensure_https_public_url_async(&self.url, "webhook_url").await?;
            debug!("Sending webhook error notification: event={}", event_str);
            self.client
                .post(&self.url)
                .header("X-Event-Type", event_str)
                .json(&payload)
                .send()
                .await?
                .error_for_status()?;
            debug!("Webhook error notification sent successfully");
            Ok(())
        })
    }
}

pub struct TelegramNotifier {
    client: reqwest::Client,
    bot_token: String,
    chat_id: String,
}

impl TelegramNotifier {
    pub fn new(bot_token: String, chat_id: String) -> Self {
        Self {
            client: create_http_client(),
            bot_token,
            chat_id,
        }
    }
}

impl Notifier for TelegramNotifier {
    fn notify<'a>(
        &'a self,
        info: &'a PowerInfo,
        event: NotificationEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error>>> + Send + 'a>> {
        Box::pin(async move {
            let title = match event {
                NotificationEvent::LowBalance => "⚠️ [Low Power Warning]",
                NotificationEvent::Heartbeat => "ℹ️ [Daily Report]",
                NotificationEvent::LoginFailure | NotificationEvent::ConsecutiveFetchFailures => {
                    return Ok(()); // These events use notify_error instead
                }
            };

            let message = format!(
                "UESTC Power Monitor\n{}\nRoom: {}\nMoney: {:.2} CNY\nEnergy: {:.2} kWh",
                title, info.room_display_name, info.remaining_money, info.remaining_energy
            );

            let url = format!("https://api.telegram.org/bot{}/sendMessage", self.bot_token);
            debug!("Sending Telegram notification");
            let params = [("chat_id", &self.chat_id), ("text", &message)];

            self.client
                .post(&url)
                .form(&params)
                .send()
                .await?
                .error_for_status()?;
            debug!("Telegram notification sent successfully");
            Ok(())
        })
    }

    fn notify_error<'a>(
        &'a self,
        error_msg: &'a str,
        event: NotificationEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error>>> + Send + 'a>> {
        Box::pin(async move {
            let title = match event {
                NotificationEvent::LoginFailure => "🔐 [Login Failure]",
                NotificationEvent::ConsecutiveFetchFailures => "❌ [Fetch Failures]",
                NotificationEvent::LowBalance | NotificationEvent::Heartbeat => {
                    return Ok(()); // These events use notify instead
                }
            };

            let message = format!("UESTC Power Monitor\n{}\n{}", title, error_msg);

            let url = format!("https://api.telegram.org/bot{}/sendMessage", self.bot_token);
            debug!("Sending Telegram error notification");
            let params = [("chat_id", &self.chat_id), ("text", &message)];

            self.client
                .post(&url)
                .form(&params)
                .send()
                .await?
                .error_for_status()?;
            debug!("Telegram error notification sent successfully");
            Ok(())
        })
    }
}

fn build_power_notification(
    info: &PowerInfo,
    event: NotificationEvent,
) -> Option<(String, String)> {
    match event {
        NotificationEvent::LowBalance => Some((
            "⚠️ UESTC Power Monitor - Low Balance Warning".to_string(),
            format!(
                "Room: {}\nMoney: {:.2} CNY\nEnergy: {:.2} kWh\nTime: {}",
                info.room_display_name,
                info.remaining_money,
                info.remaining_energy,
                time::now_display()
            ),
        )),
        NotificationEvent::Heartbeat => Some((
            "ℹ️ UESTC Power Monitor - Daily Report".to_string(),
            format!(
                "Room: {}\nMoney: {:.2} CNY\nEnergy: {:.2} kWh\nTime: {}",
                info.room_display_name,
                info.remaining_money,
                info.remaining_energy,
                time::now_display()
            ),
        )),
        NotificationEvent::LoginFailure | NotificationEvent::ConsecutiveFetchFailures => None,
    }
}

fn build_error_notification(error_msg: &str, event: NotificationEvent) -> Option<(String, String)> {
    match event {
        NotificationEvent::LoginFailure => Some((
            "🔐 UESTC Power Monitor - Login Failure".to_string(),
            format!("{}\nTime: {}", error_msg, time::now_display()),
        )),
        NotificationEvent::ConsecutiveFetchFailures => Some((
            "❌ UESTC Power Monitor - Fetch Failures".to_string(),
            format!("{}\nTime: {}", error_msg, time::now_display()),
        )),
        NotificationEvent::LowBalance | NotificationEvent::Heartbeat => None,
    }
}

pub struct PushoverNotifier {
    client: reqwest::Client,
    api_token: String,
    user_key: String,
    default_priority: i8,
    default_retry: u32,
    default_expire: u32,
    default_url: Option<String>,
}

impl PushoverNotifier {
    pub fn new(
        api_token: String,
        user_key: String,
        default_priority: i8,
        default_retry: u32,
        default_expire: u32,
        default_url: Option<String>,
    ) -> Self {
        Self {
            client: create_http_client(),
            api_token,
            user_key,
            default_priority,
            default_retry,
            default_expire,
            default_url,
        }
    }

    fn clamp_priority(priority: i8) -> i8 {
        priority.clamp(-2, 2)
    }

    fn sanitize_emergency_params(retry: u32, expire: u32) -> (u32, u32) {
        let retry = retry.max(30);
        let expire = expire.clamp(30, 10_800);
        (retry, expire)
    }

    async fn send_message(
        &self,
        message: &str,
        title: Option<&str>,
        priority: i8,
        url: Option<&str>,
    ) -> Result<(), Box<dyn Error>> {
        let mut payload = HashMap::<String, String>::new();
        let clamped_priority = Self::clamp_priority(priority);
        payload.insert("token".to_string(), self.api_token.clone());
        payload.insert("user".to_string(), self.user_key.clone());
        payload.insert("message".to_string(), message.to_string());
        payload.insert("priority".to_string(), clamped_priority.to_string());

        if clamped_priority == 2 {
            let (retry, expire) =
                Self::sanitize_emergency_params(self.default_retry, self.default_expire);
            payload.insert("retry".to_string(), retry.to_string());
            payload.insert("expire".to_string(), expire.to_string());
        }

        if let Some(title) = title {
            if !title.trim().is_empty() {
                payload.insert("title".to_string(), title.to_string());
            }
        }

        if let Some(url) = url {
            if !url.trim().is_empty() {
                payload.insert("url".to_string(), url.to_string());
            }
        }

        debug!("Sending Pushover notification");
        self.client
            .post("https://api.pushover.net/1/messages.json")
            .form(&payload)
            .send()
            .await?
            .error_for_status()?;
        debug!("Pushover notification sent successfully");
        Ok(())
    }
}

impl Notifier for PushoverNotifier {
    fn notify<'a>(
        &'a self,
        info: &'a PowerInfo,
        event: NotificationEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error>>> + Send + 'a>> {
        Box::pin(async move {
            let Some((title, message)) = build_power_notification(info, event) else {
                return Ok(());
            };
            let priority = match event {
                NotificationEvent::LowBalance => 2, // highest/emergency priority
                _ => self.default_priority,
            };

            self.send_message(
                &message,
                Some(&title),
                priority,
                self.default_url.as_deref(),
            )
            .await?;
            Ok(())
        })
    }

    fn notify_error<'a>(
        &'a self,
        error_msg: &'a str,
        event: NotificationEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error>>> + Send + 'a>> {
        Box::pin(async move {
            let Some((title, message)) = build_error_notification(error_msg, event) else {
                return Ok(());
            };

            self.send_message(
                &message,
                Some(&title),
                self.default_priority,
                self.default_url.as_deref(),
            )
            .await?;
            Ok(())
        })
    }
}

pub struct NtfyNotifier {
    client: reqwest::Client,
    topic_url: String,
    access_token: Option<String>,
    default_priority: u8,
    default_tags: Vec<String>,
    click_action: Option<String>,
    icon: Option<String>,
    actions: Vec<serde_json::Value>,
    use_markdown: bool,
}

impl NtfyNotifier {
    pub fn new(
        topic_url: String,
        access_token: Option<String>,
        default_priority: u8,
        default_tags: Vec<String>,
        click_action: Option<String>,
        icon: Option<String>,
        actions: Vec<serde_json::Value>,
        use_markdown: bool,
    ) -> Self {
        Self {
            client: create_untrusted_http_client(),
            topic_url,
            access_token,
            default_priority,
            default_tags,
            click_action,
            icon,
            actions,
            use_markdown,
        }
    }

    fn clamp_priority(priority: u8) -> u8 {
        priority.clamp(1, 5)
    }

    async fn send_message(
        &self,
        message: &str,
        title: Option<&str>,
        priority: u8,
        tags: Option<&[String]>,
        click_action: Option<&str>,
        icon: Option<&str>,
        actions: Option<&[serde_json::Value]>,
        use_markdown: bool,
    ) -> Result<(), Box<dyn Error>> {
        ensure_https_public_url_async(&self.topic_url, "ntfy_topic_url").await?;

        let mut payload = serde_json::Map::new();
        payload.insert(
            "message".to_string(),
            serde_json::Value::String(message.to_string()),
        );
        payload.insert(
            "priority".to_string(),
            serde_json::Value::Number(Self::clamp_priority(priority).into()),
        );
        payload.insert(
            "markdown".to_string(),
            serde_json::Value::Bool(use_markdown),
        );

        if let Some(title) = title {
            if !title.trim().is_empty() {
                payload.insert(
                    "title".to_string(),
                    serde_json::Value::String(title.to_string()),
                );
            }
        }

        if let Some(tags) = tags {
            if !tags.is_empty() {
                payload.insert("tags".to_string(), serde_json::json!(tags));
            }
        }

        if let Some(click_action) = click_action {
            if !click_action.trim().is_empty() {
                payload.insert(
                    "click".to_string(),
                    serde_json::Value::String(click_action.to_string()),
                );
            }
        }

        if let Some(icon) = icon {
            if !icon.trim().is_empty() {
                payload.insert(
                    "icon".to_string(),
                    serde_json::Value::String(icon.to_string()),
                );
            }
        }

        if let Some(actions) = actions {
            if !actions.is_empty() {
                payload.insert("actions".to_string(), serde_json::json!(actions));
            }
        }

        debug!("Sending ntfy notification");
        let mut request = self
            .client
            .post(&self.topic_url)
            .json(&serde_json::Value::Object(payload));

        if let Some(access_token) = self.access_token.as_deref() {
            if !access_token.trim().is_empty() {
                request = request.bearer_auth(access_token.trim());
            }
        }

        request.send().await?.error_for_status()?;
        debug!("ntfy notification sent successfully");
        Ok(())
    }
}

impl Notifier for NtfyNotifier {
    fn notify<'a>(
        &'a self,
        info: &'a PowerInfo,
        event: NotificationEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error>>> + Send + 'a>> {
        Box::pin(async move {
            let Some((title, message)) = build_power_notification(info, event) else {
                return Ok(());
            };
            let priority = match event {
                NotificationEvent::LowBalance => 5, // highest priority
                _ => self.default_priority,
            };

            self.send_message(
                &message,
                Some(&title),
                priority,
                Some(&self.default_tags),
                self.click_action.as_deref(),
                self.icon.as_deref(),
                Some(&self.actions),
                self.use_markdown,
            )
            .await?;
            Ok(())
        })
    }

    fn notify_error<'a>(
        &'a self,
        error_msg: &'a str,
        event: NotificationEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error>>> + Send + 'a>> {
        Box::pin(async move {
            let Some((title, message)) = build_error_notification(error_msg, event) else {
                return Ok(());
            };

            self.send_message(
                &message,
                Some(&title),
                self.default_priority,
                Some(&self.default_tags),
                self.click_action.as_deref(),
                self.icon.as_deref(),
                Some(&self.actions),
                self.use_markdown,
            )
            .await?;
            Ok(())
        })
    }
}

pub struct EmailNotifier {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: String,
    to: Vec<String>,
}

impl EmailNotifier {
    pub fn new(config: &NotifyConfig) -> Result<Self, Box<dyn Error>> {
        // Parse recipients (comma-separated)
        let to: Vec<String> = config
            .smtp_to
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        if to.is_empty() {
            return Err("No email recipients configured".into());
        }

        // Build SMTP transport based on encryption type
        let transport = match config.smtp_encryption {
            crate::config::SmtpEncryption::Starttls => {
                AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.smtp_server)?
                    .port(config.smtp_port)
                    .credentials(Credentials::new(
                        config.smtp_username.clone(),
                        config.smtp_password.clone(),
                    ))
                    .build()
            }
            crate::config::SmtpEncryption::Tls => {
                let tls = TlsParameters::new(config.smtp_server.clone())?;
                AsyncSmtpTransport::<Tokio1Executor>::relay(&config.smtp_server)?
                    .port(config.smtp_port)
                    .tls(Tls::Wrapper(tls))
                    .credentials(Credentials::new(
                        config.smtp_username.clone(),
                        config.smtp_password.clone(),
                    ))
                    .build()
            }
            crate::config::SmtpEncryption::None => {
                AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&config.smtp_server)
                    .port(config.smtp_port)
                    .credentials(Credentials::new(
                        config.smtp_username.clone(),
                        config.smtp_password.clone(),
                    ))
                    .build()
            }
        };

        Ok(Self {
            transport,
            from: config.smtp_from.clone(),
            to,
        })
    }

    async fn send_email(&self, subject: &str, body: &str) -> Result<(), Box<dyn Error>> {
        for recipient in &self.to {
            let email = Message::builder()
                .from(self.from.parse()?)
                .to(recipient.parse()?)
                .subject(subject)
                .header(ContentType::TEXT_PLAIN)
                .body(body.to_string())?;

            debug!("Sending email to: {}", recipient);
            self.transport.send(email).await?;
        }
        Ok(())
    }
}

impl Notifier for EmailNotifier {
    fn notify<'a>(
        &'a self,
        info: &'a PowerInfo,
        event: NotificationEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error>>> + Send + 'a>> {
        Box::pin(async move {
            let (subject, body) = match event {
                NotificationEvent::LowBalance => {
                    let subject = "⚠️ UESTC Power Monitor - Low Balance Warning";
                    let body = format!(
                        "UESTC Power Monitor - Low Balance Warning\n\
                        \n\
                        Room: {}\n\
                        Remaining Money: {:.2} CNY\n\
                        Remaining Energy: {:.2} kWh\n\
                        \n\
                        Please recharge your power account soon.\n\
                        \n\
                        Time: {}",
                        info.room_display_name,
                        info.remaining_money,
                        info.remaining_energy,
                        time::now_display()
                    );
                    (subject, body)
                }
                NotificationEvent::Heartbeat => {
                    let subject = "ℹ️ UESTC Power Monitor - Daily Report";
                    let body = format!(
                        "UESTC Power Monitor - Daily Report\n\
                        \n\
                        Room: {}\n\
                        Remaining Money: {:.2} CNY\n\
                        Remaining Energy: {:.2} kWh\n\
                        \n\
                        System is running normally.\n\
                        \n\
                        Time: {}",
                        info.room_display_name,
                        info.remaining_money,
                        info.remaining_energy,
                        time::now_display()
                    );
                    (subject, body)
                }
                NotificationEvent::LoginFailure | NotificationEvent::ConsecutiveFetchFailures => {
                    return Ok(()); // These events use notify_error instead
                }
            };

            debug!("Sending email notification: subject={}", subject);
            self.send_email(subject, &body).await?;
            debug!("Email notification sent successfully");
            Ok(())
        })
    }

    fn notify_error<'a>(
        &'a self,
        error_msg: &'a str,
        event: NotificationEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error>>> + Send + 'a>> {
        Box::pin(async move {
            let (subject, body) = match event {
                NotificationEvent::LoginFailure => {
                    let subject = "🔐 UESTC Power Monitor - Login Failure";
                    let body = format!(
                        "UESTC Power Monitor - Login Failure\n\
                        \n\
                        Failed to login to the power monitoring service.\n\
                        \n\
                        Error: {}\n\
                        \n\
                        Please check your credentials and try again.\n\
                        \n\
                        Time: {}",
                        error_msg,
                        time::now_display()
                    );
                    (subject, body)
                }
                NotificationEvent::ConsecutiveFetchFailures => {
                    let subject = "❌ UESTC Power Monitor - Fetch Failures";
                    let body = format!(
                        "UESTC Power Monitor - Consecutive Fetch Failures\n\
                        \n\
                        {}\n\
                        \n\
                        The system is unable to fetch power data. This may indicate:\n\
                        - Network connectivity issues\n\
                        - Service unavailability\n\
                        - Authentication problems\n\
                        \n\
                        Time: {}",
                        error_msg,
                        time::now_display()
                    );
                    (subject, body)
                }
                NotificationEvent::LowBalance | NotificationEvent::Heartbeat => {
                    return Ok(()); // These events use notify instead
                }
            };

            debug!("Sending email error notification: subject={}", subject);
            self.send_email(subject, &body).await?;
            debug!("Email error notification sent successfully");
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NotifyConfig;

    fn base_notify_config() -> NotifyConfig {
        NotifyConfig {
            enabled: true,
            ..Default::default()
        }
    }

    #[test]
    fn webhook_notifier_allows_public_https_ip() {
        let mut config = base_notify_config();
        config.webhook_url = "https://8.8.8.8/hook".to_string();
        let notifier = create_single_notifier(&config, NotifyType::Webhook);
        assert!(notifier.is_some());
    }

    #[test]
    fn webhook_notifier_rejects_non_https_url() {
        let mut config = base_notify_config();
        config.webhook_url = "http://8.8.8.8/hook".to_string();
        let notifier = create_single_notifier(&config, NotifyType::Webhook);
        assert!(notifier.is_none());
    }

    #[test]
    fn webhook_notifier_rejects_local_and_private_hosts() {
        for url in [
            "https://localhost/hook",
            "https://127.0.0.1/hook",
            "https://[::1]/hook",
            "https://10.0.0.1/hook",
            "https://172.16.0.1/hook",
            "https://192.168.1.10/hook",
            "https://169.254.1.1/hook",
            "https://example.local/hook",
        ] {
            let mut config = base_notify_config();
            config.webhook_url = url.to_string();
            let notifier = create_single_notifier(&config, NotifyType::Webhook);
            assert!(
                notifier.is_none(),
                "expected webhook url to be rejected: {}",
                url
            );
        }
    }

    #[test]
    fn webhook_notifier_rejects_invalid_url() {
        let mut config = base_notify_config();
        config.webhook_url = "not-a-url".to_string();
        let notifier = create_single_notifier(&config, NotifyType::Webhook);
        assert!(notifier.is_none());
    }

    #[test]
    fn ntfy_notifier_applies_same_url_restrictions() {
        let mut allow = base_notify_config();
        allow.ntfy_topic_url = "https://8.8.8.8/topic".to_string();
        assert!(create_single_notifier(&allow, NotifyType::Ntfy).is_some());

        let mut reject = base_notify_config();
        reject.ntfy_topic_url = "https://127.0.0.1/topic".to_string();
        assert!(create_single_notifier(&reject, NotifyType::Ntfy).is_none());
    }

    #[test]
    fn disallowed_host_rejects_ipv4_mapped_ipv6() {
        assert!(is_disallowed_host("::ffff:127.0.0.1"));
        assert!(is_disallowed_host("::ffff:10.1.2.3"));
        assert!(!is_disallowed_host("::ffff:8.8.8.8"));
    }

    #[tokio::test]
    async fn async_url_validation_allows_public_https_ip() {
        let result = ensure_https_public_url_async("https://8.8.8.8/hook", "webhook_url").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn async_url_validation_rejects_private_ip() {
        let result = ensure_https_public_url_async("https://127.0.0.1/hook", "webhook_url").await;
        assert!(result.is_err());
    }
}
