use reqwest::header;

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
