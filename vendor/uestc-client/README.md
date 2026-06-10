# uestc-client

A minimal reqwest client for UESTC (University of Electronic Science and Technology of China).

这是一个电子科技大学（UESTC）的最小化 reqwest 客户端封装，用于模拟登录 UESTC 统一身份认证系统，并维持会话以便进行后续的 HTTP 请求。

## Features

- **Async & Blocking**: Supports both asynchronous (tokio) and blocking APIs.
- **Multiple Login Methods**:
  - Username/password login with automatic password encryption
  - WeChat QR code login via terminal
- **Automatic Cookie Persistence**: Transparently saves and loads cookies, just like a browser.
- **Session Management**: Automatically checks if session is active before logging in.
- **Reqwest Wrapper**: Exposes `reqwest`'s request builder for full flexibility.

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
uestc-client = "0.3.0"
tokio = { version = "1", features = ["full"] }
```

To use the blocking client, enable the `blocking` feature:

```toml
[dependencies]
uestc-client = { version = "0.2.1", features = ["blocking"] }
```

## Usage

### Async Client (Default)

#### Username/Password Login

```rust
use uestc_client::UestcClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create a new client with automatic cookie persistence
    let client = UestcClient::new();

    // Login - cookies are automatically saved and reused
    // If valid cookies exist, login is skipped automatically
    client.login("your_student_id", "your_password").await?;

    // Now you can make authenticated requests
    let resp = client.get("https://eportal.uestc.edu.cn/new/index.html")
        .send()
        .await?;

    println!("Response status: {}", resp.status());

    // Logout when done (clears cookies)
    client.logout().await?;

    Ok(())
}
```

#### WeChat QR Code Login

```rust
use uestc_client::UestcClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create a new client
    let client = UestcClient::new();

    // Login using WeChat QR code
    // A QR code will be displayed in the terminal for you to scan
    client.wechat_login().await?;

    // Session is now active
    let resp = client.get("https://eportal.uestc.edu.cn/new/index.html")
        .send()
        .await?;

    println!("Response status: {}", resp.status());

    Ok(())
}
```

You can also specify a custom cookie file path:

```rust
let client = UestcClient::with_cookie_file("my_cookies.json");
client.login("your_student_id", "your_password").await?;
```

### Blocking Client

Enable the `blocking` feature in your `Cargo.toml`.

#### Username/Password Login

```rust
use uestc_client::UestcBlockingClient;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = UestcBlockingClient::new();

    // Login - cookies are automatically managed
    client.login("your_student_id", "your_password")?;

    // Authenticated request
    let resp = client.get("https://eportal.uestc.edu.cn/new/index.html")
        .send()?;

    println!("Response status: {}", resp.status());

    // Logout
    client.logout()?;

    Ok(())
}
```

#### WeChat QR Code Login

```rust
use uestc_client::UestcBlockingClient;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = UestcBlockingClient::new();

    // Login using WeChat QR code
    // A QR code will be displayed in the terminal for you to scan
    client.wechat_login()?;

    // Session is now active
    let resp = client.get("https://eportal.uestc.edu.cn/new/index.html")
        .send()?;

    println!("Response status: {}", resp.status());

    Ok(())
}
```

## Examples

The repository includes working examples in the `examples/` directory:

- `async_wechat_login.rs` - Async WeChat QR code login
- `blocking_wechat_login.rs` - Blocking WeChat QR code login

Run them with:

```bash
# Async WeChat login (default features)
cargo run --example async_wechat_login

# Blocking WeChat login
cargo run --no-default-features --features blocking --example blocking_wechat_login
```

## License

This project is licensed under the MIT License.
