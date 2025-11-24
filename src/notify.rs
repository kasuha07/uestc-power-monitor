use crate::api::PowerInfo;
use crate::config::{NotifyConfig, NotifyType};
use chrono::{Local, Timelike};
use std::error::Error;
use std::future::Future;
use std::pin::Pin;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NotificationEvent {
    LowBalance,
    Heartbeat,
}

pub struct NotificationManager {
    config: NotifyConfig,
    notifier: Box<dyn Notifier>,
    last_low_balance_notify_time: Option<chrono::DateTime<Local>>,
    last_heartbeat_date: Option<chrono::NaiveDate>,
    last_balance: Option<f64>,
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
        })
    }

    pub async fn check_and_notify(&mut self, data: &PowerInfo) {
        let notifier = &self.notifier;
        let now = Local::now();

        // Heartbeat Check
        if self.config.enabled && self.config.heartbeat_enabled {
            if now.hour() == self.config.heartbeat_hour {
                let today = now.date_naive();
                if self.last_heartbeat_date != Some(today) {
                    println!("Sending daily heartbeat...");
                    if let Err(e) = notifier.notify(data, NotificationEvent::Heartbeat).await {
                        eprintln!("Failed to send heartbeat: {}", e);
                    } else {
                        self.last_heartbeat_date = Some(today);
                    }
                }
            }
        }

        // Low Balance Check
        if self.config.enabled {
            let current_balance = data.remaining_money;
            let threshold = self.config.threshold;
            let is_low = current_balance <= threshold;

            let should_notify = if is_low {
                if let Some(last_b) = self.last_balance {
                    if last_b > threshold {
                        // Edge trigger: changed from high to low
                        true
                    } else {
                        // Already low, check cooldown
                        if let Some(last_time) = self.last_low_balance_notify_time {
                            let elapsed = now.signed_duration_since(last_time);
                            elapsed.num_minutes() >= self.config.cooldown_minutes as i64
                        } else {
                            // Should not happen if logic is correct, but safe fallback
                            true
                        }
                    }
                } else {
                    // First run and low
                    true
                }
            } else {
                false
            };

            if should_notify {
                if let Err(e) = notifier.notify(data, NotificationEvent::LowBalance).await {
                    eprintln!("Failed to notify low balance: {}", e);
                } else {
                    self.last_low_balance_notify_time = Some(now);
                }
            }

            self.last_balance = Some(current_balance);
        }
    }
}

pub trait Notifier: Send + Sync {
    fn notify<'a>(
        &'a self,
        info: &'a PowerInfo,
        event: NotificationEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error>>> + Send + 'a>>;
}

pub fn create_notifier(config: &NotifyConfig) -> Option<Box<dyn Notifier>> {
    if !config.enabled {
        return None;
    }

    match config.notify_type {
        NotifyType::Console => Some(Box::new(ConsoleNotifier)),
        NotifyType::Webhook => Some(Box::new(WebhookNotifier::new(config.webhook_url.clone()))),
        NotifyType::Telegram => Some(Box::new(TelegramNotifier::new(
            config.telegram_bot_token.clone(),
            config.telegram_chat_id.clone(),
        ))),
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
                    println!(
                        "⚠️ [Low Power Warning] Room: {}, Money: {:.2} CNY, Energy: {:.2} kWh",
                        info.room_display_name, info.remaining_money, info.remaining_energy
                    );
                }
                NotificationEvent::Heartbeat => {
                    println!(
                        "ℹ️ [Daily Report] Room: {}, Money: {:.2} CNY, Energy: {:.2} kWh",
                        info.room_display_name, info.remaining_money, info.remaining_energy
                    );
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
            };
            self.client
                .post(&self.url)
                .header("X-Event-Type", event_str)
                .json(info)
                .send()
                .await?
                .error_for_status()?;
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
            };

            let message = format!(
                "{}\nRoom: {}\nMoney: {:.2} CNY\nEnergy: {:.2} kWh",
                title, info.room_display_name, info.remaining_money, info.remaining_energy
            );

            let url = format!("https://api.telegram.org/bot{}/sendMessage", self.bot_token);
            let params = [("chat_id", &self.chat_id), ("text", &message)];

            self.client
                .post(&url)
                .form(&params)
                .send()
                .await?
                .error_for_status()?;
            Ok(())
        })
    }
}
