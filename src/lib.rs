pub mod api;
pub mod config;
pub mod db;

use crate::api::ApiService;
use crate::config::AppConfig;
use crate::db::DbService;
use std::sync::Arc;
use uestc_client::UestcClient;

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = match AppConfig::new() {
        Ok(cfg) => Arc::new(cfg),
        Err(e) => {
            eprintln!("Failed to load configuration: {}", e);
            return Err(e.into());
        }
    };

    let client = Arc::new(UestcClient::new());
    let api_service = ApiService::new(client.clone(), config.clone());
    let db_service = DbService::new(config.clone());
    db_service.init().await?;

    loop {
        match api_service.fetch_data().await {
            Ok(data) => {
                if let Err(e) = db_service.save_data(&data).await {
                    eprintln!("Failed to save data: {}", e);
                }
            }
            Err(e) => {
                eprintln!("Failed to fetch data: {}", e);
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(config.interval_seconds)).await;
    }
}
