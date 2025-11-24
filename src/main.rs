#[tokio::main]
async fn main() {
    if let Err(e) = uestc_power_monitor::run().await {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
