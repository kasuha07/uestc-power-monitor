use tracing::error;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use uestc_power_monitor::time;

// Custom time formatter that uses application timezone (defaults to Asia/Shanghai)
/// 打印命令行用法说明。
fn print_usage() {
    // 不用行尾 `\` 续行：它会吃掉下一行的前导空白，导致排版缩进丢失。
    println!(
        concat!(
            "UESTC Power Monitor - 宿舍电费监控\n",
            "\n",
            "用法:\n",
            "  uestc-power-monitor                启动监控（默认）\n",
            "  uestc-power-monitor login          登录并交互完成二次认证（reauth）后退出；\n",
            "                                      会话有效时直接通过（幂等）\n",
            "  uestc-power-monitor login --force  忽略现有会话强制重新登录（凭据缺失时交互输入）\n",
            "\n",
            "凭据与配置: 环境变量 UPM_* / 配置文件 / Docker Secrets（见 README）\n",
        )
    );
}

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

    // `login`：只完成登录（必要时交互完成 reauth）+ 保存 cookie 后退出，
    // 供无人值守 daemon 会话失效后人工恢复使用（需在终端运行）。
    // `--force`：忽略现有 cookie 会话强制重新登录。
    let mut login_only = false;
    let mut force = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "login" => login_only = true,
            "--force" => force = true,
            "--help" | "-h" => {
                print_usage();
                return;
            }
            other => {
                error!("未知参数: {other}");
                print_usage();
                std::process::exit(2);
            }
        }
    }
    if force && !login_only {
        error!("`--force` 仅在与 `login` 子命令搭配时有效");
        print_usage();
        std::process::exit(2);
    }
    if login_only {
        println!("login 模式：完成登录（含二次认证）后退出，不进入监控循环");
    }

    if let Err(e) = uestc_power_monitor::run(login_only, force).await {
        error!("Error: {}", e);
        std::process::exit(1);
    }
}
