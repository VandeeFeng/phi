use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;

use serde::Deserialize;

const DEFAULT_PORT: u16 = 3141;
pub(super) const DEFAULT_EXPIRY_DAYS: u32 = 30;

#[derive(Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct DeploymentConfig {
    port: u16,
    public_origin: Option<String>,
    trusted_proxies: Vec<IpAddr>,
    expiry: u32,
    plugins: crate::plugins::Selection,
    telegram: Option<crate::plugins::TelegramConfig>,
    feishu: Option<crate::plugins::FeishuConfig>,
}

impl Default for DeploymentConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            public_origin: None,
            trusted_proxies: Vec::new(),
            expiry: DEFAULT_EXPIRY_DAYS,
            plugins: crate::plugins::Selection::default(),
            telegram: None,
            feishu: None,
        }
    }
}

pub(super) struct GatewayConfig {
    pub(super) transport: crate::GatewayConfig,
    pub(super) trusted_proxies: Vec<IpAddr>,
    pub(super) expiry: u32,
    pub(super) plugins: crate::plugins::Configuration,
}

impl GatewayConfig {
    pub(super) fn load(directory: &Path, port: Option<u16>) -> anyhow::Result<Self> {
        let config = match super::storage::read_optional(&directory.join("config.json"))? {
            Some(bytes) => serde_json::from_slice::<DeploymentConfig>(&bytes)
                .map_err(|_| anyhow::anyhow!("invalid Gateway configuration"))?,
            None => DeploymentConfig::default(),
        };
        let plugins =
            crate::plugins::Configuration::new(config.plugins, config.telegram, config.feishu)?;
        let transport = crate::GatewayConfig::new(
            SocketAddr::from((Ipv4Addr::LOCALHOST, port.unwrap_or(config.port))),
            config.public_origin.as_deref(),
        )?;
        Ok(Self {
            transport,
            trusted_proxies: config.trusted_proxies,
            expiry: config.expiry,
            plugins,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn selected_clients_require_options() {
        let fixture = super::super::storage::tests::Fixture::new();
        fixture.write(
            "config.json",
            br#"{
            "plugins":["+builtin:telegram","+builtin:feishu"],
            "telegram":{"botToken":"test","allowedUsers":[1]},
            "feishu":{"appId":"cli_test","appSecret":"test-secret","allowedUsers":["ou_user"]}
        }"#,
        );
        assert!(GatewayConfig::load(&fixture.0, Some(0)).is_ok());
        assert!(!fixture.0.join("plugins").exists());
        assert!(!fixture.0.join("pairing.json").exists());
    }

    #[test]
    fn configuration_is_strict_even_with_a_port_override() {
        let fixture = super::super::storage::tests::Fixture::new();
        let missing = fixture.0.join("absent");
        assert!(GatewayConfig::load(&missing, None).is_ok());
        assert!(!missing.exists());
        assert_eq!(
            GatewayConfig::load(&fixture.0, None)
                .unwrap()
                .transport
                .listen_address
                .port(),
            DEFAULT_PORT
        );
        assert!(!fixture.0.join("config.json").exists());
        for json in [
            r#"{"bind":"0.0.0.0"}"#,
            r#"{"port":65536}"#,
            r#"{"trustedProxies":["127.0.0.0/8"]}"#,
            r#"{"publicOrigin":"http://phi.example.com"}"#,
            r#"{"expiry":-1}"#,
            r#"{"expiry":1.5}"#,
            r#"{"expiry":"30"}"#,
            r#"{"expiry":null}"#,
            r#"{"expiry":4294967296}"#,
            r#"{"plugins":null}"#,
            r#"{"plugins":{"enabled":true}}"#,
            r#"{"plugins":{"unknown":{}}}"#,
            r#"{"plugins":[null]}"#,
            r#"{"plugins":[true]}"#,
            r#"{"plugins":["web"]}"#,
            r#"{"plugins":["+builtin:unknown"]}"#,
            r#"{"plugins":["--builtin:web"]}"#,
            r#"{"plugins":["+builtin:telegram"]}"#,
            r#"{"plugins":["+builtin:feishu"]}"#,
            r#"{"telegram":{"botToken":"invalid token","allowedUsers":[1]}}"#,
            r#"{"telegram":{"botToken":"test","allowedUsers":[]}}"#,
            r#"{"telegram":{"gateway":"http://127.0.0.1:3141","botToken":"test","allowedUsers":[1]}}"#,
            r#"{"feishu":{"appId":"cli_test","appSecret":"test-secret","allowedUsers":[]}}"#,
            r#"{"feishu":{"appId":"cli_test","appSecret":"test-secret","allowedUsers":["ou_user"],"allowedGroups":["invalid"]}}"#,
        ] {
            fixture.write("config.json", json.as_bytes());
            assert!(GatewayConfig::load(&fixture.0, Some(0)).is_err(), "{json}");
        }
        fixture.write("config.json", br#"{"port":0,"publicOrigin":"https://phi.example.com","trustedProxies":["127.0.0.1"]}"#);
        let config = GatewayConfig::load(&fixture.0, Some(4321)).unwrap();
        assert_eq!(config.transport.listen_address.port(), 4321);
        assert_eq!(config.trusted_proxies, [IpAddr::V4(Ipv4Addr::LOCALHOST)]);
        for (json, expected) in [
            (r#"{}"#, DEFAULT_EXPIRY_DAYS),
            (r#"{"expiry":0}"#, 0),
            (r#"{"expiry":7}"#, 7),
        ] {
            fixture.write("config.json", json.as_bytes());
            assert_eq!(
                GatewayConfig::load(&fixture.0, Some(0)).unwrap().expiry,
                expected
            );
        }
    }
}
