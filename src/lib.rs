pub mod api;
pub mod config;
pub mod db;
pub mod notify;
pub mod time;
pub mod utils;

use crate::api::ApiService;
use crate::config::{AppConfig, LoginType};
use crate::db::DbService;
use crate::notify::NotificationManager;
use crate::time::DEFAULT_TIMEZONE;
use crate::utils::retry;
use std::time::Duration;
use tokio::time::sleep;
use uestc_client::UestcClient;

use tracing::{debug, error, info, warn};

/// `logout` 子命令：登出当前会话并清除本地 cookie 文件。
///
/// 无 cookie 文件时直接通过（无需登出）；cookie 加密密钥无法解析时
/// 报错并提示手动删除 cookie 文件（此时无法解密 cookie、发不出登出请求）。
pub async fn logout() -> Result<(), Box<dyn std::error::Error>> {
    let config = match AppConfig::new() {
        Ok(cfg) => cfg,
        Err(e) => {
            error!("Failed to load configuration: {}", e);
            return Err(e.into());
        }
    };

    if !std::path::Path::new(&config.cookie_file).exists() {
        info!("未找到 cookie 文件（{}），无需登出", config.cookie_file);
        return Ok(());
    }

    let cookie_encryption_secret = config
        .cookie_encryption_secret()
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{e}（无法解密 cookie，可手动删除 cookie 文件）"),
            )
        })?;
    let client = UestcClient::with_encrypted_cookie_file(
        &config.cookie_file,
        cookie_encryption_secret.as_bytes(),
    );
    client.logout().await?;
    info!("已登出，本地 cookie 已清除");
    Ok(())
}

pub async fn run(
    login_only: bool,
    force: bool,
    login_type_override: Option<LoginType>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut config = match AppConfig::new() {
        Ok(cfg) => cfg,
        Err(e) => {
            error!("Failed to load configuration: {}", e);
            return Err(e.into());
        }
    };

    // `login --type`：本次登录方式覆盖配置（仅对 login 模式生效，main.rs 已校验），
    // 只影响本次会话，不写回配置文件/环境变量。
    if let Some(login_type) = login_type_override {
        config.login_type = login_type;
    }

    // 凭据缺失时交互式输入（仅当 stdin 为终端时生效，否则报错提示改用环境变量等）
    // `force`（`login --force`）时忽略 cookie 捷径，强制要求凭据。
    if let Err(e) = config.prompt_for_credentials(force) {
        error!("{}", e);
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, e).into());
    }

    if let Err(e) = config.validate() {
        error!("{}", e);
        return Err(Box::new(e));
    }

    let configured_timezone = config.timezone.trim();
    if let Err(e) = crate::time::set_timezone(configured_timezone) {
        warn!(
            "Invalid timezone '{}': {}. Falling back to {}",
            crate::time::sanitize_for_log(configured_timezone),
            e,
            DEFAULT_TIMEZONE
        );
        crate::time::set_timezone(DEFAULT_TIMEZONE).map_err(|fallback_err| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, fallback_err)
        })?;
    }

    info!("Starting Uestc Power Monitor...");
    debug!("Configuration loaded successfully");
    info!(
        "Application timezone set to {}",
        crate::time::current_timezone_name()
    );

    // initialize services
    debug!("Initializing API service...");
    let api_service = match retry(|| ApiService::new(&config, force), 3, Duration::from_secs(5)).await {
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

    // `login`：只完成登录 + 交互式 reauth + 保存 cookie，然后退出。
    // 供无人值守 daemon 会话失效后人工恢复使用（幂等：会话有效时直接通过）。
    if login_only {
        info!("login 模式：登录 + 完成二次认证后退出（cookie 已保存）");
        return Ok(());
    }

    let interval = Duration::from_secs(config.interval_seconds);
    debug!(
        "Monitoring interval set to {} seconds",
        config.interval_seconds
    );

    #[cfg(unix)]
    let mut sigterm = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        Ok(stream) => Some(stream),
        Err(e) => {
            warn!(
                "Failed to setup SIGTERM handler: {}. SIGINT (Ctrl+C) shutdown remains available.",
                e
            );
            None
        }
    };

    // main loop
    // `reauth_waiting`：运行期命中 reauth（无人值守无法交互）后进入等待模式——
    // 不再取数，按轮询间隔重载 cookie 文件（人工跑 `login --force` 写入的新会话）
    // 并探测业务会话，恢复后继续监控并发送确认通知。
    let mut reauth_waiting = false;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Received SIGINT, shutting down gracefully...");
                break;
            }
            _ = async {
                #[cfg(unix)]
                {
                    match &mut sigterm {
                        Some(stream) => {
                            let _ = stream.recv().await;
                        }
                        None => {
                            std::future::pending::<()>().await;
                        }
                    }
                }
                #[cfg(not(unix))]
                {
                    std::future::pending::<()>().await;
                }
            } => {
                info!("Received SIGTERM, shutting down gracefully...");
                break;
            }
            _ = async {
                if reauth_waiting {
                    // 等待人工 reauth：只探测恢复，不取数不重登（避免拿账号撞锁）
                    debug!("等待人工 reauth，探测会话恢复...");
                    if api_service.probe_recovered_session().await {
                        info!("会话已恢复（reauth 完成），继续监控");
                        if let Some(manager) = &mut notification_manager {
                            manager.notify_reauth_resolved().await;
                        }
                        reauth_waiting = false;
                    } else {
                        if let Some(manager) = &mut notification_manager {
                            manager
                                .record_reauth_pending("业务会话仍未恢复，请尽快运行 `uestc-power-monitor login --force`")
                                .await;
                        }
                        debug!("Sleeping for {:?}...", interval);
                        sleep(interval).await;
                    }
                    return;
                }

                debug!("Fetching power data...");
                match retry(|| api_service.fetch_data(), 3, Duration::from_secs(2)).await {
                    Ok(Some(data)) => {
                        debug!("Data fetched successfully: room={}, money={:.2}, energy={:.2}",
                            crate::api::truncate_for_log(&data.room_display_name, crate::api::MAX_LOGGED_FIELD),
                            data.remaining_money, data.remaining_energy);

                        // Reset consecutive failure counters on success
                        if let Some(manager) = &mut notification_manager {
                            manager.reset_fetch_failures();
                            manager.reset_login_retry_failures();
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
                        // 区分失败原因：reauth 需要人工 → 进入等待模式；
                        // 会话失效后重登失败 → 登录重试通知（一天一次）；
                        // 其余网络/上游失败仍计入连续拉取失败。
                        if let Some(manager) = &mut notification_manager {
                            if api_service.take_reauth_pending() {
                                warn!("需要人工完成二次认证（reauth），进入等待模式（可在终端运行 `uestc-power-monitor login --force` 完成）");
                                manager
                                    .record_reauth_pending(&e.to_string())
                                    .await;
                                reauth_waiting = true;
                            } else if api_service.take_login_retry_failure() {
                                manager.record_login_retry_failure(&e.to_string()).await;
                            } else {
                                manager.record_fetch_failure().await;
                            }
                        } else if api_service.take_reauth_pending() {
                            warn!("需要人工完成二次认证（reauth），进入等待模式（可在终端运行 `uestc-power-monitor login --force` 完成）");
                            reauth_waiting = true;
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
