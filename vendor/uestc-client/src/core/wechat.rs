use crate::{Result, UestcClientError};
use quick_xml::Reader;
use quick_xml::events::Event;
use regex::Regex;
use url::Url;

pub const WECHAT_OPEN_URL: &str = "https://open.weixin.qq.com";
pub const WECHAT_LP_URL: &str = "https://lp.open.weixin.qq.com";

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
