use tracing::error;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use uestc_power_monitor::time;

// Custom time formatter that uses application timezone (defaults to Asia/Shanghai)
struct LocalTimeFormatter;

impl FormatTime for LocalTimeFormatter {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", time::now().format("%Y-%m-%dT%H:%M:%S%.6f%:z"))
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

    // `--reauth`：只完成登录 + 交互式 reauth + 保存 cookie 后退出，
    // 供无人值守 daemon 会话失效后人工恢复使用（需在终端运行）。
    let reauth_only = std::env::args().any(|arg| arg == "--reauth");
    if reauth_only {
        println!("reauth 模式：完成登录 + 二次认证后退出，不进入监控循环");
    }

    if let Err(e) = uestc_power_monitor::run(reauth_only).await {
        error!("Error: {}", e);
        std::process::exit(1);
    }
}
