// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Provide a global config.

use anyhow::Context as _;

lazy_static::lazy_static! {
    static ref CONFIG: std::sync::Arc<std::sync::RwLock<Config>> = std::sync::Arc::new(std::sync::RwLock::new(Config::default()));
}

pub fn current_config() -> impl core::ops::Deref<Target = Config> {
    CONFIG.read().unwrap()
}

pub fn set_config(config: Config) {
    *CONFIG.write().unwrap() = config;
}

#[derive(Debug, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(with = "humantime_serde")]
    pub error_timeout_interval: std::time::Duration,
    #[serde(with = "humantime_serde")]
    pub gotosleep_duration: std::time::Duration,
    #[serde(with = "humantime_serde")]
    pub gotosleep_duration_fota: std::time::Duration,
    #[serde(with = "humantime_serde")]
    pub fota_min_wait: std::time::Duration,
    #[serde(with = "humantime_serde")]
    pub send_message_timeout: std::time::Duration,
    #[serde(with = "humantime_serde")]
    pub request_default_timeout: std::time::Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            error_timeout_interval: std::time::Duration::from_secs(900),
            gotosleep_duration: std::time::Duration::from_secs(20),
            gotosleep_duration_fota: std::time::Duration::from_secs(100),
            fota_min_wait: std::time::Duration::from_secs(1),
            send_message_timeout: std::time::Duration::from_secs(2),
            request_default_timeout: std::time::Duration::from_millis(2500),
        }
    }
}

impl Config {
    pub async fn load<P: AsRef<std::path::Path>>(path: P) -> anyhow::Result<Option<Self>> {
        let path = path.as_ref();
        let data = match tokio::fs::read(path).await {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("can't read config from `{path:?}`")),
            Ok(v) => v,
        };

        let config: Config = serde_yaml::from_slice(&data)
            .with_context(|| format!("can't parse config at `{path:?}`"))?;

        tracing::info!(?path, "Loaded config");

        Ok(Some(config))
    }
}
