use std::{
    env,
    net::{IpAddr, SocketAddr},
};

use axum::http::HeaderMap;
use ipnet::IpNet;

use crate::transport::TransportConnectInfo;

const DEFAULT_TRUSTED_PROXY_CIDRS: &str = "127.0.0.1/32,::1/128,172.16.0.0/12";

pub fn effective_remote_addr(headers: &HeaderMap, transport: &TransportConnectInfo) -> String {
    let direct_ip = transport.remote_addr.ip();
    effective_remote_ip(headers, direct_ip, trusted_proxy(direct_ip)).to_string()
}

pub fn request_is_secure(headers: &HeaderMap, transport: &TransportConnectInfo) -> bool {
    trusted_proxy(transport.remote_addr.ip())
        && headers
            .get("x-forwarded-proto")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("https"))
}

fn trusted_proxy(remote: IpAddr) -> bool {
    if !env_bool("TEAMVIEWER_TRUST_PROXY_HEADERS") {
        return false;
    }
    env::var("TEAMVIEWER_TRUSTED_PROXY_CIDRS")
        .unwrap_or_else(|_| DEFAULT_TRUSTED_PROXY_CIDRS.to_owned())
        .split(',')
        .filter_map(|value| value.trim().parse::<IpNet>().ok())
        .any(|network| network.contains(&remote))
}

fn env_bool(name: &str) -> bool {
    env::var(name).ok().is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn effective_remote_ip(headers: &HeaderMap, direct_ip: IpAddr, trust_headers: bool) -> IpAddr {
    if !trust_headers {
        return direct_ip;
    }

    ["cf-connecting-ip", "x-real-ip", "x-forwarded-for"]
        .into_iter()
        .find_map(|name| first_header_ip(headers, name))
        .unwrap_or(direct_ip)
}

fn first_header_ip(headers: &HeaderMap, name: &str) -> Option<IpAddr> {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .find_map(parse_ip)
}

fn parse_ip(value: &str) -> Option<IpAddr> {
    let value = value.trim().trim_matches('"');
    if value.is_empty() {
        return None;
    }
    value
        .parse::<IpAddr>()
        .ok()
        .or_else(|| value.parse::<SocketAddr>().ok().map(|address| address.ip()))
        .or_else(|| {
            value
                .strip_prefix('[')
                .and_then(|value| value.strip_suffix(']'))
                .and_then(|value| value.parse::<IpAddr>().ok())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn untrusted_peer_ignores_forwarded_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("cf-connecting-ip", HeaderValue::from_static("203.0.113.10"));

        assert_eq!(
            effective_remote_ip(&headers, "192.0.2.20".parse().unwrap(), false),
            "192.0.2.20".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn trusted_peer_uses_supported_header_priority() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.30, 172.20.0.1"),
        );
        headers.insert("x-real-ip", HeaderValue::from_static("203.0.113.20"));
        headers.insert("cf-connecting-ip", HeaderValue::from_static("203.0.113.10"));

        assert_eq!(
            effective_remote_ip(&headers, "172.20.0.1".parse().unwrap(), true),
            "203.0.113.10".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn invalid_headers_are_skipped_and_addresses_are_normalized() {
        let mut headers = HeaderMap::new();
        headers.insert("cf-connecting-ip", HeaderValue::from_static("unknown"));
        headers.insert("x-real-ip", HeaderValue::from_static("[2001:db8::5]"));
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("invalid, 198.51.100.12"),
        );

        assert_eq!(
            effective_remote_ip(&headers, "172.20.0.1".parse().unwrap(), true),
            "2001:db8::5".parse::<IpAddr>().unwrap()
        );

        headers.remove("x-real-ip");
        assert_eq!(
            effective_remote_ip(&headers, "172.20.0.1".parse().unwrap(), true),
            "198.51.100.12".parse::<IpAddr>().unwrap()
        );
    }
}
