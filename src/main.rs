use uestc_client::UestcClient;
use uestc_power_monitor::config::AppConfig;

#[tokio::main]
async fn main() {
    let config = match AppConfig::new() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Failed to load configuration: {}", e);
            std::process::exit(1);
        }
    };
}
