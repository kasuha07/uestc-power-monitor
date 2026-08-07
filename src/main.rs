use tracing::error;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use uestc_power_monitor::config::LoginType;
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
            "  uestc-power-monitor login --force  强制重新登录（忽略 cookie 文件捷径，\n",
            "                                      凭据缺失时交互输入；服务端会话实际\n",
            "                                      有效时客户端会复用，不重复登录）\n",
            "  uestc-power-monitor login --type <password|wechat>\n",
            "                                      指定本次登录方式（默认取配置 login_type）\n",
            "  uestc-power-monitor logout          登出当前会话（保留本地 cookie 与\n",
            "                                      设备指纹，下次登录仍识别为可信设备）\n",
            "  uestc-power-monitor logout --clear  登出并彻底清除本地 cookie（含设备指纹，\n",
            "                                      下次登录视为新设备）\n",
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
    // `--force`：强制重新登录——忽略"cookie 文件存在"捷径（凭据缺失时交互输入），
    // 并跳过本层会话探测；服务端会话实际有效时客户端会复用，不重复登录。
    // `--type <password|wechat>`：指定本次登录方式（仅覆盖本次，不改持久配置）。
    // `logout`：登出当前会话；默认保留本地 cookie（含设备指纹），`--clear` 彻底清除。
    let mut login_only = false;
    let mut logout_only = false;
    let mut force = false;
    let mut clear = false;
    let mut login_type_override: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "login" => login_only = true,
            "logout" => logout_only = true,
            "--clear" => clear = true,
            "--force" => force = true,
            "--help" | "-h" => {
                print_usage();
                return;
            }
            "--type" => {
                let value = match args.next() {
                    Some(value) => value,
                    None => {
                        error!("`--type` 缺少取值（可选: password / wechat）");
                        std::process::exit(2);
                    }
                };
                login_type_override = Some(value);
            }
            other => {
                if let Some(value) = other.strip_prefix("--type=") {
                    login_type_override = Some(value.to_string());
                } else {
                    error!("未知参数: {other}");
                    print_usage();
                    std::process::exit(2);
                }
            }
        }
    }
    let login_type_override = match login_type_override {
        Some(value) => match LoginType::parse(&value) {
            Ok(login_type) => Some(login_type),
            Err(msg) => {
                error!("{msg}");
                std::process::exit(2);
            }
        },
        None => None,
    };
    if login_only && logout_only {
        error!("`login` 与 `logout` 子命令不能同时使用");
        std::process::exit(2);
    }
    if (force || login_type_override.is_some()) && !login_only {
        error!("`--force`/`--type` 仅在与 `login` 子命令搭配时有效");
        print_usage();
        std::process::exit(2);
    }
    if clear && !logout_only {
        error!("`--clear` 仅在与 `logout` 子命令搭配时有效");
        print_usage();
        std::process::exit(2);
    }
    if login_only {
        let type_hint = login_type_override
            .map(|t| format!("，登录方式: {:?}", t))
            .unwrap_or_default();
        println!("login 模式：完成登录（含二次认证）后退出{type_hint}，不进入监控循环");
    }
    if logout_only {
        println!("logout 模式：登出当前会话（{}）", if clear {
            "并清除本地 cookie".to_string()
        } else {
            "保留本地 cookie".to_string()
        });
    }

    let result = if logout_only {
        uestc_power_monitor::logout(clear).await
    } else {
        uestc_power_monitor::run(login_only, force, login_type_override).await
    };
    if let Err(e) = result {
        error!("Error: {}", e);
        std::process::exit(1);
    }
}
