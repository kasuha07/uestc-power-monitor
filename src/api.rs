use crate::config::AppConfig;
use serde::Deserialize;
use std::sync::Arc;
use uestc_client::UestcClient;

const BASE_URL: &str = "https://online.uestc.edu.cn/site";

pub struct ApiService {
    client: Arc<UestcClient>,
    config: Arc<AppConfig>,
}

impl ApiService {
    pub fn new(client: Arc<UestcClient>, config: Arc<AppConfig>) -> Self {
        Self { client, config }
    }

    pub async fn fetch_data(&self) -> Result<Option<PowerInfo>, Box<dyn std::error::Error>> {
        let url = format!("{}/bedroom", BASE_URL);
        let resp = self
            .client
            .get(&url)
            .send()
            .await?
            .json::<ApiResponse<PowerInfo>>()
            .await?;

        Ok(resp.data)
    }
}

#[derive(Debug, Deserialize)]
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
