use crate::config::Config;
use reqwest::Client;
use tracing::warn;

pub struct ChainRecorder {
    client:           Client,
    config:           Config,
    sensor_id_encoded: String,
    registered:       bool,
}

impl ChainRecorder {
    pub fn new(config: Config) -> Self {
        let sensor_id = format!("{}/gnss-base", config.miner);
        let sensor_id_encoded = urlencoding::encode(&sensor_id).into_owned();
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(8))
                .build()
                .expect("reqwest client"),
            config,
            sensor_id_encoded,
            registered: false,
        }
    }

    async fn ensure_registered(&mut self) {
        if self.registered { return; }
        let url = format!("http://127.0.0.1:{}/api/sensors", self.config.api_port);
        let body = serde_json::json!({
            "account": self.config.miner,
            "name": "gnss-base",
            "type": "gps",
            "unit": "rtcm3_frame",
            "decimals": 0,
            "region": format!("gnss-base/{}", self.config.device_ip),
            "hardware_model": "Hyfix MobileCM",
            "device_name": "hyfix-mobilecm",
        });
        let mut req = self.client.post(&url).json(&body);
        if !self.config.auth_token.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", self.config.auth_token));
        }
        match req.send().await {
            Ok(r) if matches!(r.status().as_u16(), 201 | 409) => self.registered = true,
            Ok(r)  => warn!("sensor register HTTP {}", r.status()),
            Err(e) => warn!("sensor register: {}", e),
        }
    }

    pub async fn record(&mut self, rtcm_type: u16, payload_bytes: usize) {
        self.ensure_registered().await;
        let url = format!(
            "http://127.0.0.1:{}/api/sensors/{}/readings",
            self.config.api_port, self.sensor_id_encoded
        );
        let body = serde_json::json!({
            "account": self.config.miner,
            "value": rtcm_type,
            "metadata": {
                "type": "gps",
                "unit": "rtcm3_frame",
                "source": "hyfix-mobilecm",
                "rtcm_type": rtcm_type,
                "payload_bytes": payload_bytes,
                "device_ip": self.config.device_ip,
            }
        });
        let mut req = self.client.post(&url).json(&body);
        if !self.config.auth_token.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", self.config.auth_token));
        }
        match req.send().await {
            Ok(r) => {
                let s = r.status().as_u16();
                if !matches!(s, 200 | 201 | 422) { warn!("chain HTTP {}", s); }
            }
            Err(e) => warn!("chain: {}", e),
        }
    }
}
