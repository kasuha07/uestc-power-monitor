use crate::config::{AppConfig, LoginType};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use uestc_client::UestcClient;

const BASE_URL: &str = "https://online.uestc.edu.cn/site";

pub struct ApiService {
    client: UestcClient,
    config: AppConfig,
}

impl ApiService {
    pub async fn new(config: &AppConfig) -> Result<Self, Box<dyn std::error::Error>> {
        let user_display = config.username.as_deref().unwrap_or("unknown");
        debug!("Creating new API service for user: {}", user_display);
        let cookie_encryption_secret = config
            .cookie_encryption_secret()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let client = UestcClient::with_encrypted_cookie_file(
            &config.cookie_file,
            cookie_encryption_secret.as_bytes(),
        );

        let service = Self {
            client,
            config: config.clone(),
        };

        service.login().await?;
        Ok(service)
    }

    async fn login(&self) -> Result<(), Box<dyn std::error::Error>> {
        debug!("Attempting login via {:?}", self.config.login_type);
        match self.config.login_type {
            LoginType::Password => {
                let username = self
                    .config
                    .username
                    .as_ref()
                    .ok_or_else(|| "Username required for password login".to_string())?;
                let password = self
                    .config
                    .password
                    .as_ref()
                    .ok_or_else(|| "Password required for password login".to_string())?;
                self.client.login(username, password).await?;
            }
            LoginType::Wechat => {
                self.client.wechat_login().await?;
            }
        }
        debug!("Login successful");

        // Initialize session with forced CAS authentication
        let init_url = "https://online.uestc.edu.cn/common/actionCasLogin?redirect_url=https://online.uestc.edu.cn/page/";
        debug!("Initializing session with CAS authentication...");
        self.client.get(init_url).send().await?;
        debug!("Session initialized");

        Ok(())
    }

    async fn check_session(&self) -> bool {
        debug!("Checking session validity...");
        let url = "https://online.uestc.edu.cn/common/getLanguageTypes.htl";
        match self.client.post(url).send().await {
            Ok(resp) => match resp.json::<SessionCheckResponse>().await {
                Ok(data) => {
                    let is_valid = data.success;
                    debug!("Session check result: valid={}", is_valid);
                    is_valid
                }
                Err(e) => {
                    debug!("Failed to parse session check response: {}", e);
                    false
                }
            },
            Err(e) => {
                debug!("Session check request failed: {}", e);
                false
            }
        }
    }

    pub async fn fetch_data(&self) -> Result<Option<PowerInfo>, Box<dyn std::error::Error>> {
        let url = format!("{}/bedroom", BASE_URL);
        debug!("Fetching power data from: {}", url);

        let result = self
            .client
            .get(&url)
            .header("Referer", "https://online.uestc.edu.cn/page/")
            .header("Accept", "application/json, text/plain, */*")
            .send()
            .await;

        // If request fails, check session and retry once
        let resp = match result {
            Ok(r) => r,
            Err(e) => {
                debug!("Request failed: {}, checking session...", e);
                if !self.check_session().await {
                    debug!("Session invalid, re-login and retry...");
                    self.login().await?;
                    self.client
                        .get(&url)
                        .header("Referer", "https://online.uestc.edu.cn/page/")
                        .header("Accept", "application/json, text/plain, */*")
                        .send()
                        .await?
                } else {
                    return Err(e.into());
                }
            }
        };

        let resp = resp.json::<ApiResponse<PowerInfo>>().await?;

        debug!(
            "API response: error={}, message={}",
            resp.error, resp.message
        );

        // Session expired (401) — re-login and retry once
        if resp.error == 401 {
            warn!(
                "Session expired (error=401, message='{}'). Re-logging in...",
                resp.message
            );
            self.login().await?;
            let retry_resp = self
                .client
                .get(&url)
                .header("Referer", "https://online.uestc.edu.cn/page/")
                .header("Accept", "application/json, text/plain, */*")
                .send()
                .await?;
            let resp = retry_resp.json::<ApiResponse<PowerInfo>>().await?;
            debug!(
                "Retry API response: error={}, message={}",
                resp.error, resp.message
            );
            if let Some(ref data) = resp.data {
                info!(
                    "Power info received: room={}, money={:.2}, energy={:.2}",
                    data.room_display_name, data.remaining_money, data.remaining_energy
                );
            } else {
                warn!(
                    "API returned no data after re-login - error_code={}, message='{}', url='{}'",
                    resp.error, resp.message, url
                );
            }
            return Ok(resp.data);
        }

        if let Some(ref data) = resp.data {
            info!(
                "Power info received: room={}, money={:.2}, energy={:.2}",
                data.room_display_name, data.remaining_money, data.remaining_energy
            );
        } else {
            warn!(
                "API returned no data - error_code={}, message='{}', url='{}'. This usually means: 1) No room is bound to your account, 2) Session expired, or 3) API service issue",
                resp.error, resp.message, url
            );
        }

        Ok(resp.data)
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PowerInfo {
    /// retcode: 返回代码
    #[serde(rename = "retcode")]
    pub code: i32,

    /// msg: 消息提示
    #[serde(rename = "msg")]
    pub message: String,

    /// sydl: 剩余电量 (Remaining Energy - kWh)
    /// 兼容字符串和数字类型（例如 "26.91" 或 26.91）
    #[serde(rename = "sydl", deserialize_with = "deserialize_f64_lossy")]
    pub remaining_energy: f64,

    /// syje: 剩余金额 (Remaining Money - CNY)
    /// 兼容字符串和数字类型（例如 "14.44" 或 14.44）
    #[serde(rename = "syje", deserialize_with = "deserialize_f64_lossy")]
    pub remaining_money: f64,

    /// dffjbh: 控电房间编号 (Meter Room ID for Utility System)
    #[serde(rename = "dffjbh")]
    pub meter_room_id: String,

    /// roomName: 房间显示名称 (e.g., "220407")
    #[serde(rename = "roomName")]
    pub room_display_name: String,

    /// roomId: 房间逻辑ID (Database ID)
    #[serde(rename = "roomId")]
    pub room_id: String,

    /// buiId: 楼栋ID (Building ID)
    #[serde(rename = "buiId")]
    pub building_id: String,

    /// areaid: 校区ID (Campus/Area ID)
    #[serde(rename = "areaid")]
    pub campus_id: String,

    /// fjh: 门牌号 (e.g., "407")
    #[serde(rename = "fjh")]
    pub room_number: String,
}

fn deserialize_f64_lossy<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum NumberOrString {
        Number(f64),
        String(String),
    }

    match NumberOrString::deserialize(deserializer)? {
        NumberOrString::Number(n) => Ok(n),
        NumberOrString::String(s) => s.trim().parse::<f64>().map_err(serde::de::Error::custom),
    }
}

#[derive(Debug, Deserialize)]
pub struct ApiResponse<T> {
    #[serde(rename = "e")]
    pub error: i32,

    #[serde(rename = "m")]
    pub message: String,

    #[serde(rename = "d")]
    pub data: Option<T>,
}

#[derive(Debug, Deserialize)]
struct SessionCheckResponse {
    success: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_json_with_values(sydl: &str, syje: &str) -> String {
        format!(
            r#"{{
                "e": 0,
                "m": "ok",
                "d": {{
                    "retcode": 0,
                    "msg": "ok",
                    "sydl": {},
                    "syje": {},
                    "dffjbh": "meter-room-id",
                    "roomName": "220407",
                    "roomId": "room-id",
                    "buiId": "building-id",
                    "areaid": "campus-id",
                    "fjh": "407"
                }}
            }}"#,
            sydl, syje
        )
    }

    #[test]
    fn power_info_deserializes_numeric_strings() {
        let json = sample_json_with_values(r#""26.91""#, r#""14.44""#);
        let resp: ApiResponse<PowerInfo> = serde_json::from_str(&json).expect("parse response");
        let data = resp.data.expect("response data should exist");
        assert!((data.remaining_energy - 26.91).abs() < f64::EPSILON);
        assert!((data.remaining_money - 14.44).abs() < f64::EPSILON);
    }

    #[test]
    fn power_info_deserializes_numeric_values() {
        let json = sample_json_with_values("26.91", "14.44");
        let resp: ApiResponse<PowerInfo> = serde_json::from_str(&json).expect("parse response");
        let data = resp.data.expect("response data should exist");
        assert!((data.remaining_energy - 26.91).abs() < f64::EPSILON);
        assert!((data.remaining_money - 14.44).abs() < f64::EPSILON);
    }

    #[test]
    fn power_info_rejects_non_numeric_values() {
        let json = sample_json_with_values(r#""abc""#, r#""14.44""#);
        let err = serde_json::from_str::<ApiResponse<PowerInfo>>(&json).expect_err("should fail");
        assert!(
            err.to_string().contains("invalid float literal"),
            "unexpected error: {}",
            err
        );
    }
}
