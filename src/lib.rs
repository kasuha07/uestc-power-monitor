pub mod api;
pub mod config;
pub mod db;
pub mod notify;

use crate::api::ApiService;
use crate::config::AppConfig;
use crate::db::DbService;
use crate::notify::{NotificationEvent, create_notifier};
use chrono::{Local, Timelike};
use std::time::Duration;
use tokio::time::sleep;

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting Uestc Power Monitor...");
    let config = match AppConfig::new() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Failed to load configuration: {}", e);
            return Err(e.into());
        }
    };
    // initialize services
    let api_service = ApiService::new(&config).await?;
    let db_service = DbService::new(config.database_url.clone()).await?;
    db_service.init().await?;
    let notifier = create_notifier(&config.notify);

    let mut last_low_balance_notify_time: Option<chrono::DateTime<Local>> = None;
    let mut last_heartbeat_date: Option<chrono::NaiveDate> = None;
    let mut last_balance: Option<f64> = None;
    let interval = Duration::from_secs(config.interval_seconds);

    // main loop
    loop {
        let now = Local::now();

        match api_service.fetch_data().await {
            Ok(Some(data)) => {
                // save data to database
                if let Err(e) = db_service.save_data(&data).await {
                    eprintln!("Failed to save data: {}", e);
                }

                // notify logic
                if let Some(notifier) = &notifier {
                    // Heartbeat Check
                    if config.notify.enabled && config.notify.heartbeat_enabled {
                        if now.hour() == config.notify.heartbeat_hour {
                            let today = now.date_naive();
                            if last_heartbeat_date != Some(today) {
                                println!("Sending daily heartbeat...");
                                if let Err(e) =
                                    notifier.notify(&data, NotificationEvent::Heartbeat).await
                                {
                                    eprintln!("Failed to send heartbeat: {}", e);
                                } else {
                                    last_heartbeat_date = Some(today);
                                }
                            }
                        }
                    }

                    // Low Balance Check
                    let current_balance = data.remaining_money;
                    let threshold = config.notify.threshold;
                    let is_low = current_balance <= threshold;

                    let should_notify = if is_low {
                        if let Some(last_b) = last_balance {
                            if last_b > threshold {
                                // Edge trigger: changed from high to low
                                true
                            } else {
                                // Already low, check cooldown
                                if let Some(last_time) = last_low_balance_notify_time {
                                    let elapsed = now.signed_duration_since(last_time);
                                    elapsed.num_minutes() >= config.notify.cooldown_minutes as i64
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
                        if let Err(e) = notifier.notify(&data, NotificationEvent::LowBalance).await
                        {
                            eprintln!("Failed to notify low balance: {}", e);
                        } else {
                            last_low_balance_notify_time = Some(now);
                        }
                    }

                    last_balance = Some(current_balance);
                }
            }
            Ok(None) => {
                println!("No data available");
            }
            Err(e) => {
                eprintln!("Failed to fetch data: {}", e);
            }
        }

        sleep(interval).await;
    }
}
