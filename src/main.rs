use tracing::error;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;

// Custom time formatter that uses local timezone (respects TZ environment variable)
struct LocalTimeFormatter;

impl FormatTime for LocalTimeFormatter {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(
            w,
            "{}",
            chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.6f%:z")
        )
    }
}

#[tokio::main]
async fn main() {
    // Initialize logging with default filter (info) if RUST_LOG is not set
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_timer(LocalTimeFormatter)
        .init();

    if let Err(e) = uestc_power_monitor::run().await {
        error!("Error: {}", e);
        std::process::exit(1);
    }
}
