pub mod api;
pub mod config;
pub mod db;
pub mod notify;
pub mod utils;

use crate::api::ApiService;
use crate::config::AppConfig;
use crate::db::DbService;
use crate::notify::NotificationManager;
use crate::utils::retry;
use std::time::Duration;
use tokio::time::sleep;

use tracing::{debug, error, info};

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting Uestc Power Monitor...");
    let config = match AppConfig::new() {
        Ok(cfg) => {
            debug!("Configuration loaded successfully");
            cfg
        }
        Err(e) => {
            error!("Failed to load configuration: {}", e);
            return Err(e.into());
        }
    };
    // initialize services
    debug!("Initializing API service...");
    let api_service = match retry(|| ApiService::new(&config), 3, Duration::from_secs(5)).await {
        Ok(service) => {
            debug!("API service initialized");
            service
        }
        Err(e) => {
            error!("Failed to initialize API service (login failed): {}", e);
            // Try to send login failure notification
            if let Some(manager) = NotificationManager::new(config.notify.clone()) {
                manager
                    .notify_login_failure(&format!("Failed to login: {}", e))
                    .await;
            }
            return Err(e);
        }
    };

    debug!("Initializing database service...");
    let db_service = DbService::new(config.database_url.clone()).await?;
    db_service.init().await?;
    debug!("Database service initialized");

    debug!("Initializing notification manager...");
    let mut notification_manager = NotificationManager::new(config.notify.clone());
    debug!(
        "Notification manager initialized: {:?}",
        notification_manager.is_some()
    );

    let interval = Duration::from_secs(config.interval_seconds);
    debug!(
        "Monitoring interval set to {} seconds",
        config.interval_seconds
    );

    // main loop
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Received SIGINT, shutting down gracefully...");
                break;
            }
            _ = async {
                #[cfg(unix)]
                {
                    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("Failed to setup SIGTERM handler");
                    sigterm.recv().await
                }
                #[cfg(not(unix))]
                {
                    std::future::pending::<()>().await;
                    Some(())
                }
            } => {
                info!("Received SIGTERM, shutting down gracefully...");
                break;
            }
            _ = async {
                debug!("Fetching power data...");
                match retry(|| api_service.fetch_data(), 3, Duration::from_secs(2)).await {
                    Ok(Some(data)) => {
                        debug!("Data fetched successfully: room={}, money={:.2}, energy={:.2}",
                            data.room_display_name, data.remaining_money, data.remaining_energy);

                        // Reset consecutive failure counter on success
                        if let Some(manager) = &mut notification_manager {
                            manager.reset_fetch_failures();
                        }

                        // save data to database
                        if let Err(e) = db_service.save_data(&data).await {
                            error!("Failed to save data: {}", e);
                        }

                        // notify logic
                        if let Some(manager) = &mut notification_manager {
                            debug!("Checking notification conditions...");
                            manager.check_and_notify(&data).await;
                        }
                    }
                    Ok(None) => {
                        debug!("No data returned from API (details logged above)");
                        // Record as a fetch failure
                        if let Some(manager) = &mut notification_manager {
                            manager.record_fetch_failure().await;
                        }
                    }
                    Err(e) => {
                        error!("Failed to fetch data: {}", e);
                        // Record consecutive fetch failure
                        if let Some(manager) = &mut notification_manager {
                            manager.record_fetch_failure().await;
                        }
                    }
                }

                debug!("Sleeping for {:?}...", interval);
                sleep(interval).await;
            } => {}
        }
    }

    info!("Shutdown complete");
    Ok(())
}
