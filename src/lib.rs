pub mod api;
pub mod config;
pub mod db;
pub mod notify;

use crate::api::ApiService;
use crate::config::AppConfig;
use crate::db::DbService;
use crate::notify::NotificationManager;
use std::time::Duration;
use tokio::time::sleep;

use tracing::{error, info, warn};

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting Uestc Power Monitor...");
    let config = match AppConfig::new() {
        Ok(cfg) => cfg,
        Err(e) => {
            error!("Failed to load configuration: {}", e);
            return Err(e.into());
        }
    };
    // initialize services
    let api_service = ApiService::new(&config).await?;
    let db_service = DbService::new(config.database_url.clone()).await?;
    db_service.init().await?;
    let mut notification_manager = NotificationManager::new(config.notify.clone());
    let interval = Duration::from_secs(config.interval_seconds);

    // main loop
    loop {
        match api_service.fetch_data().await {
            Ok(Some(data)) => {
                // save data to database
                if let Err(e) = db_service.save_data(&data).await {
                    error!("Failed to save data: {}", e);
                }

                // notify logic
                if let Some(manager) = &mut notification_manager {
                    manager.check_and_notify(&data).await;
                }
            }
            Ok(None) => {
                warn!("No data available");
            }
            Err(e) => {
                error!("Failed to fetch data: {}", e);
            }
        }

        sleep(interval).await;
    }
}
