use crate::config::Config;
use crate::keypair::SensorKeypair;
use reqwest::Client;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

pub struct ChainRecorder {
    client:            Client,
    config:            Config,
    sensor_id:         String,
    sensor_id_encoded: String,
    keypair:           Option<SensorKeypair>,
    registered:        bool,
    key_registered:    bool,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl ChainRecorder {
    pub fn new(config: Config) -> Self {
        let sensor_id = format!("{}/gnss-base", config.miner);
        let sensor_id_encoded = urlencoding::encode(&sensor_id).into_owned();

        // Load or generate sensor keypair
        let keypair = match SensorKeypair::load_or_generate(&sensor_id) {
            Ok(kp) => {
                info!("Sensor keypair ready. Public key: {}", &kp.public_key_spki_hex[..16]);
                Some(kp)
            }
            Err(e) => {
                warn!("Could not load sensor keypair: {} — readings will be unsigned", e);
                None
            }
        };

        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(8))
                .build()
                .expect("reqwest client"),
            config,
            sensor_id,
            sensor_id_encoded,
            keypair,
            registered: false,
            key_registered: false,
        }
    }

    async fn ensure_registered(&mut self) {
        if self.registered { return; }

        let url = format!("http://127.0.0.1:{}/api/sensors", self.config.api_port);
        let mut body = serde_json::json!({
            "account": self.config.miner,
            "name": "gnss-base",
            "type": "gps",
            "unit": "rtcm3_frame",
            "decimals": 0,
            "region": format!("gnss-base/{}", self.config.device_ip),
            "hardware_model": "Hyfix MobileCM",
            "device_name": "hyfix-mobilecm",
        });

        // Include public key on registration so it's stored from the start
        if let Some(ref kp) = self.keypair {
            body["public_key"] = serde_json::Value::String(kp.public_key_spki_hex.clone());
        }

        let mut req = self.client.post(&url).json(&body);
        if !self.config.auth_token.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", self.config.auth_token));
        }
        match req.send().await {
            Ok(r) if matches!(r.status().as_u16(), 201 | 409) => {
                self.registered = true;
                // If sensor exists (409), still try to register key below
            }
            Ok(r)  => warn!("sensor register HTTP {}", r.status()),
            Err(e) => warn!("sensor register: {}", e),
        }
    }

    async fn ensure_key_registered(&mut self) {
        if self.key_registered || !self.registered { return; }
        let kp = match &self.keypair {
            Some(k) => k,
            None => { self.key_registered = true; return; }
        };

        let url = format!(
            "http://127.0.0.1:{}/api/sensors/{}/register-key",
            self.config.api_port,
            self.sensor_id_encoded
        );
        let body = serde_json::json!({ "public_key": kp.public_key_spki_hex });
        let mut req = self.client.post(&url).json(&body);
        if !self.config.auth_token.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", self.config.auth_token));
        }
        match req.send().await {
            Ok(r) if matches!(r.status().as_u16(), 200 | 201) => {
                info!("Sensor public key registered on chain for {}", self.sensor_id);
                self.key_registered = true;
            }
            Ok(r) if r.status().as_u16() == 409 => {
                // Already registered
                self.key_registered = true;
            }
            Ok(r) => {
                warn!("sensor key-register HTTP {} — will retry", r.status());
                // Log hint for manual registration
                if let Some(kp) = &self.keypair {
                    kp.print_registration_hint(&format!("http://127.0.0.1:{}", self.config.api_port));
                }
            }
            Err(e) => warn!("sensor key-register: {}", e),
        }
    }

    pub async fn record(&mut self, rtcm_type: u16, payload_bytes: usize) {
        self.ensure_registered().await;
        self.ensure_key_registered().await;

        let url = format!(
            "http://127.0.0.1:{}/api/sensors/{}/readings",
            self.config.api_port, self.sensor_id_encoded
        );

        let device_timestamp = now_ms();

        // Sign the reading if we have a keypair
        let (reading_sig, sig_ts) = if let Some(ref kp) = self.keypair {
            let sig = kp.sign_reading(rtcm_type, device_timestamp);
            (Some(sig), Some(device_timestamp))
        } else {
            (None, None)
        };

        let mut body = serde_json::json!({
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

        if let (Some(sig), Some(ts)) = (reading_sig, sig_ts) {
            body["reading_sig"] = serde_json::Value::String(sig);
            body["device_timestamp"] = serde_json::Value::Number(ts.into());
        }

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
