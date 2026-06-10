use serde_json::Value;
use uestc_client::UestcClient;

/// Integration test for querying dormitory electricity fees
///
/// This test simulates the complete business flow:
/// 1. Login with automatic cookie management
/// 2. Initialize session with online service hall (forced CAS authentication)
/// 3. Query dormitory electricity information
///
/// To run this test:
/// ```bash
/// UESTC_USERNAME=your_student_id UESTC_PASSWORD=your_password cargo test --test bedroom_query_integration -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore] // Requires real credentials, run explicitly with --ignored
async fn test_bedroom_electricity_query() {
    // Get credentials from environment variables
    let username =
        std::env::var("UESTC_USERNAME").expect("UESTC_USERNAME environment variable not set");
    let password =
        std::env::var("UESTC_PASSWORD").expect("UESTC_PASSWORD environment variable not set");

    let cookie_file = "uestc_cookies.json";

    // Step 1: Login with automatic cookie management
    let client = UestcClient::new();
    client
        .login(&username, &password)
        .await
        .expect("Login failed");
    println!("[✓] Login successful");

    // Step 2: Initialize session with forced CAS authentication
    // This URL forces the system to go through IDAS authentication and set p_auth_token
    println!("[*] Initializing online service hall session (forced CAS authentication)...");
    let init_url = "https://online.uestc.edu.cn/common/actionCasLogin?redirect_url=https://online.uestc.edu.cn/page/";
    let init_resp = client
        .get(init_url)
        .send()
        .await
        .expect("Failed to initialize session");

    assert!(
        init_resp.status().is_success() || init_resp.status().is_redirection(),
        "Session initialization failed with status: {}",
        init_resp.status()
    );
    println!("[✓] Session initialized");

    // Step 3: Query dormitory electricity information
    println!("[*] Querying dormitory electricity information...");
    let api_url = "https://online.uestc.edu.cn/site/bedroom";
    let resp = client
        .get(api_url)
        .header("Referer", "https://online.uestc.edu.cn/page/")
        .header("Accept", "application/json, text/plain, */*")
        .send()
        .await
        .expect("Failed to query bedroom API");

    let json: Value = resp.json().await.expect("Failed to parse JSON response");

    println!(
        "[*] API Response: {}",
        serde_json::to_string_pretty(&json).unwrap()
    );

    // Verify response structure
    assert_eq!(
        json.get("e").and_then(|v| v.as_i64()),
        Some(0),
        "API returned error: {}",
        json.get("m")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown error")
    );

    // Extract and display information
    let data = json.get("d").expect("Missing 'd' field in response");
    let room_name = data
        .get("roomName")
        .and_then(|v| v.as_str())
        .unwrap_or("N/A");
    let room_id = data.get("roomId").and_then(|v| v.as_str()).unwrap_or("N/A");
    let electricity = data.get("sydl").and_then(|v| v.as_str()).unwrap_or("0");
    let balance = data.get("syje").and_then(|v| v.as_str()).unwrap_or("0");

    println!("\n{}", "-".repeat(30));
    println!("🏠 Room: {} (ID: {})", room_name, room_id);
    println!("⚡ Electricity: {} kWh", electricity);
    println!("💰 Balance: {} CNY", balance);
    println!("{}", "-".repeat(30));

    // Warning for low electricity
    if let Ok(elec_value) = electricity.parse::<f64>() {
        if elec_value < 10.0 {
            println!(
                "⚠️  Warning: Low electricity ({} kWh), please recharge soon!",
                elec_value
            );
        }
    }

    println!("\n[✓] Integration test completed successfully");
    println!("[*] Cookies saved to: {}", cookie_file);
}
