use crate::{Result, UestcClientError};
use quick_xml::Reader;
use quick_xml::events::Event;
use regex::Regex;
use std::time::{Duration, Instant};
use url::Url;

pub const WECHAT_OPEN_URL: &str = "https://open.weixin.qq.com";
pub const WECHAT_LP_URL: &str = "https://lp.open.weixin.qq.com";

/// 等待扫码的总时长上限。微信通常会先返回 402（过期），但接口异常时
/// 这是唯一能让轮询循环退出的兜底。
pub const QR_SCAN_TIMEOUT: Duration = Duration::from_secs(300);

/// 单次轮询请求的超时。微信长轮询正常在 ~25s 内返回。
pub const POLL_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// 两次轮询之间的间隔。
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 连续轮询失败的容忍次数。一次网络抖动不应作废用户正在扫的二维码。
pub const MAX_CONSECUTIVE_POLL_FAILURES: u32 = 5;

/// 连续未知状态码的容忍次数。防止接口变更或风控导致无限轮询。
pub const MAX_CONSECUTIVE_UNKNOWN_STATUS: u32 = 10;

#[derive(Debug)]
pub struct WechatAuthParams {
    pub appid: String,
    pub redirect_uri: String,
    pub state: String,
}

impl WechatAuthParams {
    pub fn from_url(url: &str) -> Result<Self> {
        log::debug!("Parsing WeChat OAuth parameters from URL");

        let parsed = Url::parse(url).map_err(|e| UestcClientError::WeChatError {
            message: format!("Invalid URL: {}", e),
        })?;

        let query_pairs: std::collections::HashMap<_, _> = parsed.query_pairs().collect();

        let appid = query_pairs
            .get("appid")
            .ok_or_else(|| UestcClientError::WeChatError {
                message: "Missing appid parameter".to_string(),
            })?
            .to_string();

        let redirect_uri = query_pairs
            .get("redirect_uri")
            .ok_or_else(|| UestcClientError::WeChatError {
                message: "Missing redirect_uri parameter".to_string(),
            })?
            .to_string();

        let state = query_pairs
            .get("state")
            .ok_or_else(|| UestcClientError::WeChatError {
                message: "Missing state parameter".to_string(),
            })?
            .to_string();

        log::debug!(
            "Successfully parsed WeChat OAuth params (appid: {}, state: {})",
            appid,
            state
        );

        Ok(Self {
            appid,
            redirect_uri,
            state,
        })
    }

    pub fn build_qr_xml_url(&self) -> String {
        format!(
            "{}/connect/qrconnect?appid={}&redirect_uri={}&state={}&response_type=code&scope=snsapi_login&f=xml&stylelite=1&fast_login=1",
            WECHAT_OPEN_URL,
            urlencoding::encode(&self.appid),
            urlencoding::encode(&self.redirect_uri),
            urlencoding::encode(&self.state)
        )
    }

    /// 与 `build_qr_xml_url` 等价但返回 HTML 页面（reauth 链路 XML 取 uuid 失败时的回退）。
    pub fn build_qr_html_url(&self) -> String {
        format!(
            "{}/connect/qrconnect?appid={}&redirect_uri={}&state={}&response_type=code&scope=snsapi_login&stylelite=1&fast_login=1",
            WECHAT_OPEN_URL,
            urlencoding::encode(&self.appid),
            urlencoding::encode(&self.redirect_uri),
            urlencoding::encode(&self.state)
        )
    }

    pub fn build_callback_url(&self, wx_code: &str) -> String {
        let separator = if self.redirect_uri.contains('?') {
            "&"
        } else {
            "?"
        };

        format!(
            "{}{}code={}&state={}",
            self.redirect_uri, separator, wx_code, self.state
        )
    }
}

/// Parse UUID from WeChat QR code XML response
pub fn parse_qr_uuid_from_xml(xml_text: &str) -> Result<String> {
    log::debug!(
        "Parsing QR UUID from XML response ({} bytes)",
        xml_text.len()
    );

    let mut reader = Reader::from_str(xml_text);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut uuid = None;
    let mut in_uuid_tag = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                if e.name().as_ref() == b"uuid" {
                    in_uuid_tag = true;
                }
            }
            Ok(Event::Text(text)) if in_uuid_tag => {
                let text_str = std::str::from_utf8(text.as_ref()).map_err(|e| {
                    UestcClientError::XmlParseError {
                        message: format!("Failed to parse UUID from text: {}", e),
                        source: None,
                    }
                })?;
                uuid = Some(text_str.trim().to_string());
                in_uuid_tag = false;
            }
            Ok(Event::CData(cdata)) if in_uuid_tag => {
                let text_str = std::str::from_utf8(cdata.as_ref()).map_err(|e| {
                    UestcClientError::XmlParseError {
                        message: format!("Failed to parse UUID from CDATA: {}", e),
                        source: None,
                    }
                })?;
                uuid = Some(text_str.trim().to_string());
                in_uuid_tag = false;
            }
            Ok(Event::End(ref e)) => {
                if e.name().as_ref() == b"uuid" {
                    in_uuid_tag = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                log::error!("XML parse error while extracting UUID: {}", e);
                return Err(UestcClientError::XmlParseError {
                    message: format!("XML parse error: {}", e),
                    source: None,
                });
            }
            _ => {}
        }
        buf.clear();
    }

    uuid.ok_or_else(|| {
        log::error!("UUID not found in XML response");
        UestcClientError::XmlParseError {
            message: "UUID not found in XML response".to_string(),
            source: None,
        }
    })
}

/// 从 qrconnect HTML 页面提取二维码 uuid（镜像分析仓库正则
/// `/connect\/qrcode\/([0-9A-Za-z_-]{8,})/`；XML 路径失败时的回退）。
pub fn parse_qr_uuid_from_html(html: &str) -> Result<String> {
    let re = Regex::new(r"/connect/qrcode/([0-9A-Za-z_-]{8,})").map_err(|e| {
        UestcClientError::WeChatError {
            message: format!("Regex compilation error: {}", e),
        }
    })?;
    re.captures(html)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string())
        .ok_or_else(|| UestcClientError::WeChatError {
            message: "qrconnect 页未提取到 uuid（页面结构可能已变）".to_string(),
        })
}

/// Display QR code in terminal for WeChat login
pub fn display_qr_in_terminal(uuid: &str) -> Result<()> {
    let qr_url = format!("https://open.weixin.qq.com/connect/confirm?uuid={}", uuid);

    log::info!("请使用微信扫描二维码登录");

    qr2term::print_qr(&qr_url).map_err(|e| {
        log::error!("Failed to display QR code: {}", e);
        UestcClientError::WeChatError {
            message: format!("Failed to display QR code: {}", e),
        }
    })?;

    log::debug!("二维码 URL: {}", qr_url);

    Ok(())
}

#[derive(Debug, PartialEq)]
pub enum ScanStatus {
    Waiting,      // 408: Waiting for scan
    Scanned,      // 404: Scanned, waiting for confirmation
    Confirmed,    // 405: Login confirmed
    Expired,      // 402: QR code expired
    Unknown(i32), // Other status codes
}

pub struct ScanResult {
    pub status: ScanStatus,
    pub wx_code: Option<String>,
}

/// Build polling URL for checking scan status
pub fn build_poll_url(uuid: &str, last_code: Option<&str>) -> String {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(std::time::Duration::from_secs(0))
        .as_millis();

    let mut lp_url = format!(
        "{}/connect/l/qrconnect?uuid={}&_={}",
        WECHAT_LP_URL, uuid, timestamp
    );

    if let Some(code) = last_code {
        lp_url.push_str(&format!("&last={}", code));
    }

    lp_url
}

/// Parse scan status from WeChat polling response
pub fn parse_scan_status(text: &str) -> Result<ScanResult> {
    log::debug!("Parsing WeChat scan status from response");

    let errcode_re =
        Regex::new(r"window\.wx_errcode=(\d+)").map_err(|e| UestcClientError::WeChatError {
            message: format!("Regex compilation error: {}", e),
        })?;

    let status = if let Some(caps) = errcode_re.captures(text) {
        let code: i32 = caps[1].parse().unwrap_or(0);
        match code {
            408 => {
                log::debug!("WeChat scan status: Waiting for scan");
                ScanStatus::Waiting
            }
            404 => {
                log::debug!("WeChat scan status: Scanned, awaiting confirmation");
                ScanStatus::Scanned
            }
            405 => {
                log::debug!("WeChat scan status: Confirmed");
                ScanStatus::Confirmed
            }
            402 => {
                log::warn!("WeChat QR code expired");
                ScanStatus::Expired
            }
            _ => {
                log::warn!("Unknown WeChat status code: {}", code);
                ScanStatus::Unknown(code)
            }
        }
    } else {
        log::warn!("Could not extract error code from WeChat response");
        ScanStatus::Unknown(0)
    };

    // If confirmed, extract wx_code
    let wx_code = if status == ScanStatus::Confirmed {
        let code_re = Regex::new(r#"window\.wx_code=['"](.+?)['"]"#).map_err(|e| {
            UestcClientError::WeChatError {
                message: format!("Regex compilation error: {}", e),
            }
        })?;

        let code = code_re
            .captures(text)
            .and_then(|caps| caps.get(1))
            .map(|m| m.as_str().to_string());

        if let Some(ref c) = code {
            log::debug!("Extracted wx_code (length: {})", c.len());
        }

        code
    } else {
        None
    };

    Ok(ScanResult { status, wx_code })
}

/// 扫码轮询的健壮性守卫：总时长上限 + 连续失败计数。
///
/// 轮询循环本身没有出口条件（只靠微信返回 402 过期），网络抖动又会让单次
/// 请求直接冒泡失败而作废二维码。这个守卫把两类问题都收敛成"连续超过阈值
/// 才放弃"，偶发失败只是重试。
pub struct ScanPollGuard {
    deadline: Instant,
    consecutive_failures: u32,
    consecutive_unknown: u32,
}

impl ScanPollGuard {
    pub fn new() -> Self {
        Self::with_timeout(QR_SCAN_TIMEOUT)
    }

    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            deadline: Instant::now() + timeout,
            consecutive_failures: 0,
            consecutive_unknown: 0,
        }
    }

    /// 每轮轮询前调用。超过总时长上限时返回错误，让调用方退出循环。
    pub fn check_deadline(&self) -> Result<()> {
        if Instant::now() >= self.deadline {
            log::warn!("等待扫码超时，放弃当前二维码");
            return Err(UestcClientError::WeChatError {
                message: "Timed out waiting for WeChat QR code scan".to_string(),
            });
        }
        Ok(())
    }

    /// 轮询请求或响应解析失败时调用。连续失败未达上限时返回 `Ok`，调用方
    /// 应当继续下一轮；达到上限则把原始错误交还给调用方。
    pub fn record_failure(&mut self, error: UestcClientError) -> Result<()> {
        self.consecutive_failures += 1;
        if self.consecutive_failures >= MAX_CONSECUTIVE_POLL_FAILURES {
            log::error!(
                "轮询连续失败 {} 次，放弃当前二维码",
                self.consecutive_failures
            );
            return Err(error);
        }

        log::warn!(
            "轮询失败（{}/{}），将继续重试: {}",
            self.consecutive_failures,
            MAX_CONSECUTIVE_POLL_FAILURES,
            error
        );
        Ok(())
    }

    /// 成功拿到一个状态后调用，负责重置/累加计数。
    /// 连续收到未知状态码超过上限时返回错误。
    pub fn record_status(&mut self, status: &ScanStatus) -> Result<()> {
        self.consecutive_failures = 0;

        match status {
            ScanStatus::Unknown(code) => {
                self.consecutive_unknown += 1;
                if self.consecutive_unknown >= MAX_CONSECUTIVE_UNKNOWN_STATUS {
                    log::error!(
                        "连续 {} 次收到未知状态码 {}，放弃当前二维码",
                        self.consecutive_unknown,
                        code
                    );
                    return Err(UestcClientError::WeChatError {
                        message: format!(
                            "Received unknown WeChat status code {} {} times in a row",
                            code, self.consecutive_unknown
                        ),
                    });
                }
                log::warn!(
                    "未知状态码: {}（{}/{}）",
                    code,
                    self.consecutive_unknown,
                    MAX_CONSECUTIVE_UNKNOWN_STATUS
                );
            }
            _ => self.consecutive_unknown = 0,
        }

        Ok(())
    }
}

impl Default for ScanPollGuard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn network_error() -> UestcClientError {
        UestcClientError::WeChatError {
            message: "boom".to_string(),
        }
    }

    #[test]
    fn deadline_passes_before_timeout_and_fails_after() {
        let guard = ScanPollGuard::with_timeout(Duration::from_secs(60));
        guard.check_deadline().expect("should not be expired yet");

        let expired = ScanPollGuard::with_timeout(Duration::ZERO);
        let err = expired
            .check_deadline()
            .expect_err("zero timeout should expire immediately");
        assert!(err.to_string().contains("Timed out waiting"));
    }

    #[test]
    fn transient_failures_are_tolerated_until_the_limit() {
        let mut guard = ScanPollGuard::new();
        for _ in 0..MAX_CONSECUTIVE_POLL_FAILURES - 1 {
            guard
                .record_failure(network_error())
                .expect("below the limit should keep polling");
        }

        guard
            .record_failure(network_error())
            .expect_err("hitting the limit should surface the error");
    }

    #[test]
    fn a_successful_poll_resets_the_failure_counter() {
        let mut guard = ScanPollGuard::new();
        for _ in 0..MAX_CONSECUTIVE_POLL_FAILURES - 1 {
            guard.record_failure(network_error()).expect("below limit");
        }

        guard
            .record_status(&ScanStatus::Waiting)
            .expect("known status is fine");

        // Counter was reset, so we can absorb a full batch of failures again.
        for _ in 0..MAX_CONSECUTIVE_POLL_FAILURES - 1 {
            guard
                .record_failure(network_error())
                .expect("counter should have been reset");
        }
    }

    #[test]
    fn consecutive_unknown_status_codes_abort_the_loop() {
        let mut guard = ScanPollGuard::new();
        for _ in 0..MAX_CONSECUTIVE_UNKNOWN_STATUS - 1 {
            guard
                .record_status(&ScanStatus::Unknown(999))
                .expect("below the limit should keep polling");
        }

        let err = guard
            .record_status(&ScanStatus::Unknown(999))
            .expect_err("hitting the limit should abort");
        assert!(err.to_string().contains("unknown WeChat status code 999"));
    }

    #[test]
    fn a_known_status_resets_the_unknown_counter() {
        let mut guard = ScanPollGuard::new();
        for _ in 0..MAX_CONSECUTIVE_UNKNOWN_STATUS - 1 {
            guard
                .record_status(&ScanStatus::Unknown(999))
                .expect("below limit");
        }

        guard
            .record_status(&ScanStatus::Scanned)
            .expect("known status is fine");

        for _ in 0..MAX_CONSECUTIVE_UNKNOWN_STATUS - 1 {
            guard
                .record_status(&ScanStatus::Unknown(999))
                .expect("counter should have been reset");
        }
    }

    #[test]
    fn scan_status_parsing_covers_documented_codes() {
        let cases = [
            ("window.wx_errcode=408;", ScanStatus::Waiting),
            ("window.wx_errcode=404;", ScanStatus::Scanned),
            ("window.wx_errcode=402;", ScanStatus::Expired),
            ("window.wx_errcode=500;", ScanStatus::Unknown(500)),
        ];

        for (body, expected) in cases {
            let result = parse_scan_status(body).expect("parse should succeed");
            assert_eq!(result.status, expected, "body: {}", body);
        }
    }

    #[test]
    fn confirmed_status_extracts_wx_code() {
        let body = r#"window.wx_errcode=405;window.wx_code='abc123';"#;
        let result = parse_scan_status(body).expect("parse should succeed");
        assert_eq!(result.status, ScanStatus::Confirmed);
        assert_eq!(result.wx_code.as_deref(), Some("abc123"));
    }
}
