//! Async WeChat Login Example
//!
//! This example demonstrates how to use the async WeChat login functionality.
//! It will display a QR code in the terminal that you can scan with WeChat.
//!
//! Usage:
//!   cargo run --features async --example async_wechat_login
//!
//! Note: The 'async' feature is enabled by default, so you can also run:
//!   cargo run --example async_wechat_login

use uestc_client::UestcClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logger to see debug messages
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    println!("=== UESTC Async WeChat Login Example ===\n");

    // Create a new client with default cookie file
    let client = UestcClient::new();

    // Perform WeChat login
    println!("Starting WeChat login...");
    println!("A QR code will be displayed below. Please scan it with WeChat.\n");

    match client.wechat_login().await {
        Ok(_) => {
            println!("\n✓ Login successful!");
            println!("Session cookies have been saved.");

            // Verify session is active
            if client.is_session_active().await {
                println!("✓ Session is active and ready to use.");
            }
        }
        Err(e) => {
            eprintln!("\n✗ Login failed: {}", e);
            return Err(e.into());
        }
    }

    // Example: Make a request to verify the session works
    println!("\nTesting session with a simple request...");
    let login_url = "https://idas.uestc.edu.cn/authserver/login";
    match client.get(login_url).send().await {
        Ok(resp) => {
            println!("✓ Request successful! Final URL: {}", resp.url());
        }
        Err(e) => {
            eprintln!("✗ Request failed: {}", e);
        }
    }

    Ok(())
}
