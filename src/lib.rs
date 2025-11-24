pub mod api;
pub mod config;
pub mod db;
pub mod notify;

use crate::api::ApiService;
use crate::config::AppConfig;
use crate::db::DbService;

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = match AppConfig::new() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Failed to load configuration: {}", e);
            return Err(e.into());
        }
    };

    let api_service = ApiService::new(&config).await?;
    let db_service = DbService::new(config.database_url.clone()).await?;
    db_service.init().await?;
    let notifier = crate::notify::create_notifier(&config.notify);

    loop {
        match api_service.fetch_data().await {
            Ok(Some(data)) => {
                // save data to database
                if let Err(e) = db_service.save_data(&data).await {
                    eprintln!("Failed to save data: {}", e);
                }

                // notify if remaining money is less than threshold
                if let Some(notifier) = &notifier {
                    if data.remaining_money <= config.notify.threshold {
                        if let Err(e) = notifier.notify(&data).await {
                            eprintln!("Failed to notify: {}", e);
                        }
                    }
                }
            }
            Ok(None) => {
                println!("No data available");
            }
            Err(e) => {
                eprintln!("Failed to fetch data: {}", e);
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(config.interval_seconds)).await;
    }
}
