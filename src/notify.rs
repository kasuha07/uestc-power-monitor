use crate::api::PowerInfo;
use std::error::Error;
use std::future::Future;
use std::pin::Pin;

pub trait Notifier: Send + Sync {
    fn notify<'a>(
        &'a self,
        info: &'a PowerInfo,
    ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error>>> + Send + 'a>>;
}

pub struct ConsoleNotifier;

impl Notifier for ConsoleNotifier {
    fn notify<'a>(
        &'a self,
        info: &'a PowerInfo,
    ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error>>> + Send + 'a>> {
        Box::pin(async move {
            println!(
                "⚠️ [Low Power Warning] Room: {}, Money: {:.2} CNY, Energy: {:.2} kWh",
                info.room_display_name, info.remaining_money, info.remaining_energy
            );
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
    ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error>>> + Send + 'a>> {
        Box::pin(async move {
            self.client
                .post(&self.url)
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
    ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error>>> + Send + 'a>> {
        Box::pin(async move {
            let message = format!(
                "⚠️ [Low Power Warning]\nRoom: {}\nMoney: {:.2} CNY\nEnergy: {:.2} kWh",
                info.room_display_name, info.remaining_money, info.remaining_energy
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
