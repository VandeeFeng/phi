use std::net::{Ipv4Addr, SocketAddr};

use anyhow::ensure;
use reqwest::Url;

pub struct GatewayConfig {
    pub(crate) listen_address: SocketAddr,
    pub(crate) public_origin: Option<GatewayOrigin>,
    pub(crate) plugins: crate::plugins::PluginRegistry,
}

impl GatewayConfig {
    /// Installs plugin registrations without starting tasks or connecting clients.
    pub fn with_plugins(mut self, plugins: crate::plugins::PluginRegistry) -> Self {
        self.plugins = plugins;
        self
    }

    pub fn new(listen_address: SocketAddr, public_origin: Option<&str>) -> anyhow::Result<Self> {
        ensure!(
            listen_address.ip() == Ipv4Addr::LOCALHOST,
            "gateway requires an IPv4 loopback listener"
        );
        let public_origin = public_origin.map(GatewayOrigin::public).transpose()?;
        Ok(Self {
            listen_address,
            public_origin,
            plugins: crate::plugins::PluginRegistry::new(),
        })
    }
}

#[derive(Clone)]
pub(crate) struct GatewayOrigin {
    pub(crate) url: String,
    authority: String,
}

impl GatewayOrigin {
    fn public(value: &str) -> anyhow::Result<Self> {
        let url = Url::parse(value)?;
        ensure!(
            url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none()
                && !value.contains('\\')
                && value
                    .split_once("://")
                    .is_some_and(|(_, suffix)| { !suffix.trim_end_matches('/').contains('/') }),
            "gateway public origin must be an HTTPS origin without credentials, path, query or fragment"
        );
        ensure!(
            !matches!(url.host_str(), Some("0.0.0.0" | "[::]" | "*")),
            "gateway public origin cannot use a wildcard host"
        );
        let origin = url.origin().ascii_serialization();
        let authority = origin.strip_prefix("https://").unwrap().to_owned();
        Ok(Self {
            url: origin,
            authority,
        })
    }

    pub(crate) fn local(address: SocketAddr) -> Self {
        Self {
            url: format!("http://{address}"),
            authority: address.to_string(),
        }
    }

    pub(crate) fn accepts(&self, host: Option<&str>, origin: Option<&str>) -> bool {
        host == Some(self.authority.as_str()) && origin.is_none_or(|origin| origin == self.url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployment_requires_explicit_secure_ingress() {
        for origin in [
            "http://phi.example.com",
            "https://user@phi.example.com",
            "https://phi.example.com/path",
            "https://phi.example.com/a/..",
            "https://phi.example.com?query",
            "https://phi.example.com#fragment",
            "https://0.0.0.0",
            "https://[::]",
            "https://*",
            "https://phi.example.com\\path",
        ] {
            assert!(
                GatewayConfig::new("127.0.0.1:3141".parse().unwrap(), Some(origin)).is_err(),
                "{origin}"
            );
        }
        assert!(GatewayConfig::new("0.0.0.0:3141".parse().unwrap(), None).is_err());
        assert!(GatewayConfig::new("127.0.0.1:3141".parse().unwrap(), None).is_ok());
        assert!(
            GatewayConfig::new(
                "0.0.0.0:3141".parse().unwrap(),
                Some("https://phi.example.com")
            )
            .is_err()
        );
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum IngressKind {
    Local,
    Public,
}

#[derive(Clone)]
pub(crate) struct GatewayIngress {
    pub(crate) source: std::net::IpAddr,
    pub(crate) audience: String,
    pub(crate) kind: IngressKind,
}

pub(crate) fn admit_ingress(
    origins: &[GatewayOrigin],
    trusted_proxies: &[std::net::IpAddr],
    peer: std::net::IpAddr,
    headers: &axum::http::HeaderMap,
) -> anyhow::Result<GatewayIngress> {
    use axum::http::header::{HOST, ORIGIN};
    let single_header = |name: &str| -> anyhow::Result<Option<&str>> {
        let mut values = headers.get_all(name).iter();
        let value = values.next();
        ensure!(values.next().is_none(), "duplicate ingress header");
        value
            .map(|value| value.to_str().map_err(Into::into))
            .transpose()
    };
    let host = single_header(HOST.as_str())?;
    let origin = single_header(ORIGIN.as_str())?;
    ensure!(
        single_header("forwarded")?.is_none(),
        "unsupported forwarding metadata"
    );
    let forwarded = single_header("x-forwarded-for")?;
    ensure!(
        !headers.keys().any(|name| {
            let name = name.as_str();
            (name.starts_with("x-forwarded-") && name != "x-forwarded-for") || name == "x-real-ip"
        }),
        "unsupported forwarding metadata"
    );
    let (index, entry) = origins
        .iter()
        .enumerate()
        .find(|(_, entry)| entry.accepts(host, origin))
        .ok_or_else(|| anyhow::anyhow!("unrecognized Gateway ingress"))?;
    let kind = if index == 0 {
        IngressKind::Local
    } else {
        IngressKind::Public
    };
    let source = match kind {
        IngressKind::Local => {
            ensure!(
                peer == std::net::IpAddr::V4(Ipv4Addr::LOCALHOST) && forwarded.is_none(),
                "local ingress cannot be forwarded"
            );
            peer
        }
        IngressKind::Public => {
            ensure!(trusted_proxies.contains(&peer), "untrusted Gateway proxy");
            forwarded
                .ok_or_else(|| anyhow::anyhow!("proxy client address required"))?
                .parse::<std::net::IpAddr>()
                .map_err(|_| anyhow::anyhow!("proxy must overwrite client address with one IP"))?
        }
    };
    Ok(GatewayIngress {
        source,
        audience: entry.url.clone(),
        kind,
    })
}

#[cfg(test)]
mod ingress_tests {
    use super::*;
    use axum::http::HeaderMap;
    use proptest::prelude::*;

    proptest::proptest! {
        #[test]
        fn ingress_identity_cannot_be_mixed_or_promoted(
            source in proptest::prop_oneof![
                proptest::prelude::any::<[u8; 4]>().prop_map(|bytes| std::net::IpAddr::from(bytes)),
                proptest::prelude::any::<[u8; 16]>().prop_map(|bytes| std::net::IpAddr::from(bytes)),
                proptest::strategy::Just(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST)),
            ],
            include_origin in proptest::prelude::any::<bool>(),
        ) {
            let peer = std::net::IpAddr::V4(Ipv4Addr::LOCALHOST);
            let origins = [
                GatewayOrigin::local("127.0.0.1:3141".parse().unwrap()),
                GatewayOrigin::public("https://PHI.example.com:443/").unwrap(),
            ];
            for (kind, host, origin) in [
                (IngressKind::Local, "127.0.0.1:3141", "http://127.0.0.1:3141"),
                (IngressKind::Public, "phi.example.com", "https://phi.example.com"),
            ] {
                let mut headers = HeaderMap::new();
                headers.insert("host", host.parse().unwrap());
                if include_origin {
                    headers.insert("origin", origin.parse().unwrap());
                }
                if kind == IngressKind::Public {
                    headers.insert("x-forwarded-for", source.to_string().parse().unwrap());
                }
                let admitted = admit_ingress(&origins, &[peer], peer, &headers).unwrap();
                proptest::prop_assert!(admitted.kind == kind);
                proptest::prop_assert_eq!(admitted.source, if kind == IngressKind::Local { peer } else { source });
                proptest::prop_assert_eq!(admitted.audience, origin);
                proptest::prop_assert!(admit_ingress(&origins, &[peer], "192.0.2.254".parse().unwrap(), &headers).is_err());
                if kind == IngressKind::Public {
                    proptest::prop_assert!(admit_ingress(&origins, &[], peer, &headers).is_err());
                    let mut missing_source = headers.clone();
                    missing_source.remove("x-forwarded-for");
                    proptest::prop_assert!(admit_ingress(&origins, &[peer], peer, &missing_source).is_err());
                }
                for (name, value) in [
                    ("host", "phi.example.com.evil.test"),
                    ("host", if kind == IngressKind::Local { "phi.example.com" } else { "127.0.0.1:3141" }),
                    ("origin", if kind == IngressKind::Local { "https://phi.example.com" } else { "http://127.0.0.1:3141" }),
                    ("origin", "null"),
                    ("origin", "https://other.example.com"),
                    ("forwarded", "for=127.0.0.1"),
                    ("x-forwarded-proto", "https"),
                    ("x-forwarded-host", "phi.example.com"),
                    ("x-real-ip", "127.0.0.1"),
                    ("x-forwarded-for", "192.0.2.5, 127.0.0.1"),
                    ("x-forwarded-for", "192.0.2.5:9000"),
                    ("x-forwarded-for", "unknown"),
                    ("x-forwarded-for", ""),
                ] {
                    let mut invalid = headers.clone();
                    invalid.insert(name, value.parse().unwrap());
                    proptest::prop_assert!(admit_ingress(&origins, &[peer], peer, &invalid).is_err(), "{}", name);
                }
                if kind == IngressKind::Local {
                    headers.insert("x-forwarded-for", source.to_string().parse().unwrap());
                    proptest::prop_assert!(admit_ingress(&origins, &[peer], peer, &headers).is_err());
                    headers.remove("x-forwarded-for");
                }
                headers.insert("origin", origin.parse().unwrap());
                for name in ["host", "origin", "x-forwarded-for"] {
                    let mut duplicate = headers.clone();
                    let value = duplicate.get(name).cloned().unwrap_or_else(|| source.to_string().parse().unwrap());
                    duplicate.insert(name, value.clone());
                    duplicate.append(name, value);
                    proptest::prop_assert!(admit_ingress(&origins, &[peer], peer, &duplicate).is_err(), "{}", name);
                }
            }
        }
    }
}
