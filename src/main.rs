use tracing::error;

#[tokio::main]
async fn main() {
    // Initialize logging with default filter (info) if RUST_LOG is not set
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    tracing_subscriber::fmt().with_env_filter(filter).init();

    if let Err(e) = uestc_power_monitor::run().await {
        error!("Error: {}", e);
        std::process::exit(1);
    }
}
