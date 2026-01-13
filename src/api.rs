use crate::config::{AppConfig, LoginType};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};
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
        let client = UestcClient::with_cookie_file(&config.cookie_file);

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

        debug!("API response: error={}, message={}", resp.error, resp.message);
        if let Some(ref data) = resp.data {
            info!("Power info received: room={}, money={:.2}, energy={:.2}",
                data.room_display_name, data.remaining_money, data.remaining_energy);
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
    /// 注意：原JSON中是字符串类型 ("26.91")，自动转换为 f64
    #[serde(rename = "sydl", deserialize_with = "deserialize_f64_from_str")]
    pub remaining_energy: f64,

    /// syje: 剩余金额 (Remaining Money - CNY)
    /// 注意：原JSON中是字符串类型 ("14.44")，自动转换为 f64
    #[serde(rename = "syje", deserialize_with = "deserialize_f64_from_str")]
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

fn deserialize_f64_from_str<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: String = serde::Deserialize::deserialize(deserializer)?;
    s.parse::<f64>().map_err(serde::de::Error::custom)
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
