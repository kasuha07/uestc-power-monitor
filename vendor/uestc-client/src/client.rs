use reqwest::header;
use std::time::Duration;

mod cookie_persistence;

#[cfg(feature = "async")]
pub mod async_impl;

#[cfg(feature = "blocking")]
pub mod blocking_impl;

#[cfg(feature = "async")]
pub use async_impl::UestcClient;

#[cfg(feature = "blocking")]
pub use blocking_impl::UestcBlockingClient;

pub(crate) const AUTH_SERVER_URL: &str = "https://idas.uestc.edu.cn/authserver";

/// 建连超时，避免 DNS 解析或 TCP 握手挂死。
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 单次请求的总超时（含读取响应体）。没有它，半开连接会让调用方永久阻塞；
/// 扫码长轮询会用 `core::wechat::POLL_REQUEST_TIMEOUT` 单独覆盖。
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// 空闲连接的存活时间，需要短于服务端的 keep-alive 超时。
///
/// reqwest 默认保留 90s，而轮询型调用方（例如每分钟取一次数据的监控进程）
/// 每次都会复用一条已空闲约 60s 的连接。若服务端先关掉它，请求就会在写入后
/// 撞上 FIN，表现为随机的 "connection closed before message completed"。
/// 25s 让连接在服务端回收之前先由客户端丢弃，代价只是偶尔多一次 TLS 握手。
pub(crate) const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(25);

/// TCP keepalive。必须短于 `POOL_IDLE_TIMEOUT`，否则空闲连接总是先被回收、
/// 探测包永远发不出去；取 15s 也能让扫码长轮询（30s 级）中途探到对端消失。
pub(crate) const TCP_KEEPALIVE: Duration = Duration::from_secs(15);

pub(crate) fn default_headers() -> header::HeaderMap {
    let mut headers = header::HeaderMap::new();
    // common headers
    headers.insert(header::ACCEPT, header::HeaderValue::from_static("text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7"));
    headers.insert(
        header::ACCEPT_LANGUAGE,
        header::HeaderValue::from_static("zh-CN,zh;q=0.9"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-cache"),
    );
    headers.insert(
        header::UPGRADE_INSECURE_REQUESTS,
        header::HeaderValue::from_static("1"),
    );
    headers.insert(header::PRAGMA, header::HeaderValue::from_static("no-cache"));
    headers.insert(header::DNT, header::HeaderValue::from_static("1"));
    headers.insert(header::USER_AGENT, header::HeaderValue::from_static("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36"));

    // Sec-Fetch headers
    headers.insert(
        "Sec-Fetch-Dest",
        header::HeaderValue::from_static("document"),
    );
    headers.insert(
        "Sec-Fetch-Mode",
        header::HeaderValue::from_static("navigate"),
    );
    headers.insert("Sec-Fetch-Site", header::HeaderValue::from_static("none"));
    headers.insert("Sec-Fetch-User", header::HeaderValue::from_static("?1"));

    // Sec-Ch-Ua headers
    headers.insert(
        "Sec-Ch-Ua",
        header::HeaderValue::from_static(r#""Not_A Brand";v="99", "Chromium";v="142""#),
    );
    headers.insert("Sec-Ch-Ua-Mobile", header::HeaderValue::from_static("?0"));
    headers.insert(
        "Sec-Ch-Ua-Platform",
        header::HeaderValue::from_static(r#""Windows""#),
    );

    headers
}
