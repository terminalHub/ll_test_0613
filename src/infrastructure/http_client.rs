//! HMAC 认证的 HTTP 客户端
//!
//! 用于 SDK 不支持的 API 端点（如 redeem）
//! header 名称与 SDK 保持一致：lmts-api-key, lmts-timestamp, lmts-signature

use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{de::DeserializeOwned, Serialize};
use sha2::Sha256;
use tracing::debug;

type HmacSha256 = Hmac<Sha256>;

/// HMAC 认证的 HTTP 客户端
#[derive(Clone)]
pub struct HmacHttpClient {
    client: reqwest::Client,
    base_url: String,
    token_id: String,
    secret: String,
}

impl HmacHttpClient {
    pub fn new(token_id: &str, secret: &str) -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
            base_url: "https://api.limitless.exchange".to_string(),
            token_id: token_id.to_string(),
            secret: secret.to_string(),
        })
    }

    /// 发送带 HMAC 签名的 POST 请求
    pub async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> anyhow::Result<T> {
        let url = format!("{}{}", self.base_url, path);
        let body_json = serde_json::to_string(body)?;

        // ISO 8601 时间戳（与 SDK 格式一致）
        let timestamp = iso8601_timestamp();

        // HMAC 签名：timestamp\nMETHOD\nPATH\nBODY
        let message = format!("{}\nPOST\n{}\n{}", timestamp, path, body_json);
        let signature = self.sign(&message)?;

        debug!(url = %url, timestamp = %timestamp, "发送 HMAC 请求");

        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("lmts-api-key", &self.token_id)
            .header("lmts-timestamp", &timestamp)
            .header("lmts-signature", &signature)
            .body(body_json)
            .send()
            .await?;

        let status = resp.status();
        let text = resp.text().await?;

        debug!(status = %status, body = %text, "API 响应");

        if !status.is_success() {
            anyhow::bail!("API error {} POST {}: {}", status, path, text);
        }

        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("解析响应失败: {}", e))
    }

    fn sign(&self, message: &str) -> anyhow::Result<String> {
        let key = base64::engine::general_purpose::STANDARD.decode(&self.secret)?;
        let mut mac = HmacSha256::new_from_slice(&key)?;
        mac.update(message.as_bytes());
        Ok(base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes()))
    }
}

/// ISO 8601 UTC 时间戳，格式：2026-06-13T13:52:22.028Z
fn iso8601_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let millis = now.subsec_millis();

    let secs_of_day = secs.rem_euclid(86_400);
    let days = secs.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year,
        month,
        day,
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
        millis
    )
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_iso8601_format() {
        let ts = iso8601_timestamp();
        assert!(ts.ends_with('Z'));
        assert_eq!(ts.len(), 24);
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
    }
}
