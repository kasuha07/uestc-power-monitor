# uestc-client

A minimal reqwest client for UESTC (University of Electronic Science and Technology of China).

这是一个电子科技大学（UESTC）的最小化 reqwest 客户端封装，用于模拟登录 UESTC 统一身份认证系统，并维持会话以便进行后续的 HTTP 请求。

## Features

- **Async & Blocking**: Supports both asynchronous (tokio) and blocking APIs.
- **Multiple Login Methods**:
  - Username/password login with automatic password encryption
  - WeChat QR code login via terminal
- **Reauth (Multi-Factor) Support**: automatic detection of the multifactor `reauth` requirement and a three-step API to complete it (SMS / WeCom / email-style dynamic codes, WeChat scan, password)
- **Encrypted Cookie Persistence**: Optionally saves and loads cookies encrypted at rest (AES-256-GCM).
- **Session Management**: Reuses an active session only when it belongs to the account being logged in.
- **Reqwest Wrapper**: Exposes `reqwest`'s request builder for full flexibility.

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
uestc-client = "0.5.0"
tokio = { version = "1", features = ["full"] }
```

To use the blocking client, enable the `blocking` feature:

```toml
[dependencies]
uestc-client = { version = "0.5.0", features = ["blocking"] }
```

## Usage

### Async Client (Default)

#### Username/Password Login

```rust
use uestc_client::UestcClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create a client whose cookies are persisted encrypted at rest
    let client = UestcClient::with_encrypted_cookie_file("uestc_cookies.json", "a-strong-secret");

    // Login - cookies are encrypted, saved and reused.
    // If the saved session belongs to this account, login is skipped automatically.
    client.login("your_student_id", "your_password").await?;

    // Now you can make authenticated requests
    let resp = client.get("https://eportal.uestc.edu.cn/new/index.html")
        .send()
        .await?;

    println!("Response status: {}", resp.status());

    // Logout when done: ends the server session and clears local session cookies
    // (only the trusted-device fingerprint cookie is kept).
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

#### Cookie persistence

- `UestcClient::new()` / `with_cookie_file(path)` keep cookies **in memory only**. An existing
  file at `path` is never read, written or deleted.
- `with_encrypted_cookie_file(path, secret)` persists cookies encrypted at rest. If the file
  cannot be decrypted, the client starts with an empty session and **leaves the file untouched**.
- `try_with_encrypted_cookie_file(path, secret)` does the same but returns an error instead,
  so you can decide how to recover.
- In 0.5.0, plaintext cookie files written by earlier versions are no longer loaded
  implicitly. Migrate them once, explicitly:

```rust
uestc_client::migrate_plaintext_cookie_file("uestc_cookies.json", "a-strong-secret")?;
let client = UestcClient::try_with_encrypted_cookie_file("uestc_cookies.json", "a-strong-secret")?;
```

#### Session ownership and logout

- `login(username, ..)` reuses a saved session only if it was created by a password login
  (or reauth) for the same `username`. Sessions of another account, or of unknown ownership
  (WeChat login, files saved by older versions), are discarded and a fresh login is performed.
- `logout()` ends the server session and clears all local session cookies, in memory and on
  disk, keeping only the `MULTIFACTOR_BROWSER_FINGERPRINT` trusted-device cookie so the next
  login is not asked for reauth again.
- `logout_and_forget_device()` also removes that fingerprint cookie.
- If the WeChat QR login lands on a multi-factor reauth page, `wechat_login()` returns
  `UestcClientError::ReauthRequired` instead of reporting success.

### Multi-Factor Reauth (Password Login)

When the account requires multi-factor authentication, `login()` returns
`UestcClientError::ReauthRequired` (carrying the server-rendered available methods)
instead of silently "succeeding" with a locked TGT. Complete it with the three-step API:

```rust
use uestc_client::{UestcClientError, ReauthMethodKind};

let mut ctx = match client.login("your_student_id", "your_password").await {
    Ok(()) => {
        // No reauth needed
        // ... use the session ...
        return;
    }
    Err(UestcClientError::ReauthRequired { context }) => context,
    Err(e) => return Err(e.into()),
};

// Pick a method from ctx.available_methods (server-rendered, already filtered by account)
let method = ctx
    .available_methods
    .iter()
    .find(|m| m.kind() == ReauthMethodKind::Wechat)
    .cloned()
    .unwrap(); // e.g. WeChat scan

match method.kind() {
    // WeChat scan: QR shown in terminal, scan with phone WeChat. No code needed.
    ReauthMethodKind::Wechat => {
        client.submit_reauth(&ctx, &method, None, None, false).await?;
    }
    // Dynamic codes (SMS / WeCom / email / ...): switch method, request code, then submit
    ReauthMethodKind::DynamicCode => {
        client.change_reauth_type(&mut ctx, &method).await?;
        client.send_reauth_code(&ctx, &method).await?;
        let code = prompt("Enter the code: ");
        client.submit_reauth(&ctx, &method, Some(&code), None, false).await?;
    }
    // Password reauth (if enabled for the account)
    ReauthMethodKind::Password => {
        client.submit_reauth(&ctx, &method, None, Some("your_password"), false).await?;
    }
    _ => unreachable!(),
}
```

The last `bool` parameter of `submit_reauth` is `skip_tmp_reauth` (trust-device dialog):
`false` = this time only (default), `true` = trust this device.

### Blocking Client

Enable the `blocking` feature in your `Cargo.toml`. The blocking client mirrors the
async API, including reauth support.

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

    // Logout (clears local session cookies, keeps the trusted-device fingerprint)
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
