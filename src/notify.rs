use crate::api::PowerInfo;
use crate::config::{NotifyConfig, NotifyType};
use crate::utils::retry;
use chrono::{Local, Timelike};
use lettre::{
    message::{header::ContentType, Message},
    transport::smtp::authentication::Credentials,
    AsyncSmtpTransport, AsyncTransport, Tokio1Executor,
};
use lettre::transport::smtp::client::{Tls, TlsParameters};
use serde_json;
use std::error::Error;
use std::future::Future;
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
    notifier: Box<dyn Notifier>,
    last_low_balance_notify_time: Option<chrono::DateTime<Local>>,
    last_heartbeat_date: Option<chrono::NaiveDate>,
    last_balance: Option<f64>,
    consecutive_fetch_failures: u32,
    last_fetch_failure_notify_time: Option<chrono::DateTime<Local>>,
}

impl NotificationManager {
    pub fn new(config: NotifyConfig) -> Option<Self> {
        let notifier = create_notifier(&config)?;
        Some(Self {
            config,
            notifier,
            last_low_balance_notify_time: None,
            last_heartbeat_date: None,
            last_balance: None,
            consecutive_fetch_failures: 0,
            last_fetch_failure_notify_time: None,
        })
    }

    pub async fn check_and_notify(&mut self, data: &PowerInfo) {
        let notifier = &self.notifier;
        let now = Local::now();
        debug!("Checking notification conditions at {}", now);

        // Heartbeat Check
        if self.config.enabled && self.config.heartbeat_enabled {
            debug!("Heartbeat enabled, checking conditions...");
            if now.hour() == self.config.heartbeat_hour {
                let today = now.date_naive();
                if self.last_heartbeat_date != Some(today) {
                    info!("Sending daily heartbeat...");
                    if let Err(e) = retry(
                        || notifier.notify(data, NotificationEvent::Heartbeat),
                        3,
                        Duration::from_secs(2),
                    )
                    .await
                    {
                        error!("Failed to send heartbeat: {}", e);
                    } else {
                        self.last_heartbeat_date = Some(today);
                        debug!("Heartbeat sent successfully");
                    }
                } else {
                    debug!("Heartbeat already sent today");
                }
            } else {
                debug!("Not heartbeat hour yet (current: {}, target: {})", now.hour(), self.config.heartbeat_hour);
            }
        }

        // Low Balance Check
        if self.config.enabled {
            let current_balance = data.remaining_money;
            let threshold = self.config.threshold;
            let is_low = current_balance <= threshold;
            debug!("Balance check: current={:.2}, threshold={:.2}, is_low={}",
                current_balance, threshold, is_low);

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
                            let should = elapsed.num_minutes() >= self.config.cooldown_minutes as i64;
                            debug!("Balance still low, cooldown check: elapsed={}min, cooldown={}min, should_notify={}",
                                elapsed.num_minutes(), self.config.cooldown_minutes, should);
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
                if let Err(e) = retry(
                    || notifier.notify(data, NotificationEvent::LowBalance),
                    3,
                    Duration::from_secs(2),
                )
                .await
                {
                    error!("Failed to notify low balance: {}", e);
                } else {
                    self.last_low_balance_notify_time = Some(now);
                    debug!("Low balance notification sent successfully");
                }
            }

            self.last_balance = Some(current_balance);
        }
    }

    pub async fn notify_login_failure(&self, error_msg: &str) {
        if !self.config.enabled || !self.config.login_failure_enabled {
            return;
        }

        info!("Sending login failure notification...");
        if let Err(e) = retry(
            || self.notifier.notify_error(error_msg, NotificationEvent::LoginFailure),
            3,
            Duration::from_secs(2),
        )
        .await
        {
            error!("Failed to send login failure notification: {}", e);
        } else {
            debug!("Login failure notification sent successfully");
        }
    }

    pub async fn record_fetch_failure(&mut self) {
        self.consecutive_fetch_failures += 1;
        debug!("Consecutive fetch failures: {}", self.consecutive_fetch_failures);

        if !self.config.enabled || !self.config.fetch_failure_enabled {
            return;
        }

        if self.consecutive_fetch_failures >= self.config.fetch_failure_threshold {
            let now = Local::now();
            let should_notify = if let Some(last_time) = self.last_fetch_failure_notify_time {
                let elapsed = now.signed_duration_since(last_time);
                elapsed.num_minutes() >= self.config.fetch_failure_cooldown_minutes as i64
            } else {
                true
            };

            if should_notify {
                info!("Sending consecutive fetch failures notification (count: {})...", self.consecutive_fetch_failures);
                let error_msg = format!("Failed to fetch data {} times consecutively", self.consecutive_fetch_failures);
                if let Err(e) = retry(
                    || self.notifier.notify_error(&error_msg, NotificationEvent::ConsecutiveFetchFailures),
                    3,
                    Duration::from_secs(2),
                )
                .await
                {
                    error!("Failed to send consecutive fetch failures notification: {}", e);
                } else {
                    self.last_fetch_failure_notify_time = Some(now);
                    debug!("Consecutive fetch failures notification sent successfully");
                }
            }
        }
    }

    pub fn reset_fetch_failures(&mut self) {
        if self.consecutive_fetch_failures > 0 {
            debug!("Resetting consecutive fetch failures counter (was: {})", self.consecutive_fetch_failures);
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

pub fn create_notifier(config: &NotifyConfig) -> Option<Box<dyn Notifier>> {
    if !config.enabled {
        debug!("Notifications disabled");
        return None;
    }

    debug!("Creating notifier of type: {:?}", config.notify_type);
    match config.notify_type {
        NotifyType::Console => Some(Box::new(ConsoleNotifier)),
        NotifyType::Webhook => Some(Box::new(WebhookNotifier::new(config.webhook_url.clone()))),
        NotifyType::Telegram => Some(Box::new(TelegramNotifier::new(
            config.telegram_bot_token.clone(),
            config.telegram_chat_id.clone(),
        ))),
        NotifyType::Email => {
            match EmailNotifier::new(config) {
                Ok(notifier) => Some(Box::new(notifier)),
                Err(e) => {
                    error!("Failed to create email notifier: {}", e);
                    None
                }
            }
        }
    }
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
            client: reqwest::Client::new(),
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
            debug!("Sending webhook notification: event={}, url={}", event_str, self.url);
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
                "timestamp": chrono::Local::now().to_rfc3339(),
            });

            debug!("Sending webhook error notification: event={}, url={}", event_str, self.url);
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
            client: reqwest::Client::new(),
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
            debug!("Sending Telegram notification to chat_id: {}", self.chat_id);
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
            debug!("Sending Telegram error notification to chat_id: {}", self.chat_id);
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

pub struct EmailNotifier {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: String,
    to: Vec<String>,
}

impl EmailNotifier {
    pub fn new(config: &NotifyConfig) -> Result<Self, Box<dyn Error>> {
        // Parse recipients (comma-separated)
        let to: Vec<String> = config.smtp_to
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
                        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
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
                        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
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
                        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
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
                        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
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
