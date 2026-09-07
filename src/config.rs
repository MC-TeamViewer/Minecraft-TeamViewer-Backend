use serde::Deserialize;
use std::net::SocketAddr;
use std::path::PathBuf;

const STATE_CONFIG: &str = include_str!("../config/server_state_config.toml");

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub player_timeout_sec: u64,
    pub entity_timeout_sec: u64,
    pub waypoint_timeout_sec: u64,
    pub battle_chunk_timeout_sec: u64,
    pub battle_chunk_cache_retention_sec: u64,
    pub battle_chunk_cache_max_entries: usize,
    pub refresh_cooldown_sec: f64,
    pub refresh_lead_sec: f64,
    pub refresh_max_items: usize,
    pub digest_interval_sec: u64,
    pub movement_refresh_sec: u64,
    pub default_broadcast_hz: f64,
    pub min_broadcast_hz: f64,
    pub congestion_levels: Vec<(usize, f64)>,
    pub tab_report_timeout_sec: u64,
    pub same_server_filter_enabled: bool,
    pub tab_history_enabled: bool,
    pub tab_history_retention_days: u32,
    pub tab_history_delta_retention_days: u32,
    pub tab_history_default_chunk_entries: usize,
    pub tab_history_max_chunk_entries: usize,
    pub tab_history_max_chunk_bytes: usize,
    pub tab_history_max_lookup_selectors: usize,
    pub tab_history_observation_update_interval_sec: u64,
    pub web_transport: WebTransportConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FileConfig {
    timeouts: TimeoutConfig,
    #[serde(rename = "refreshRequest")]
    refresh_request: RefreshConfig,
    protocol: ProtocolConfig,
    features: FeatureConfig,
    #[serde(rename = "tabHistory")]
    tab_history: TabHistoryConfig,
    #[serde(rename = "webTransport")]
    web_transport: WebTransportFileConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
struct WebTransportFileConfig {
    enabled: bool,
    #[serde(rename = "bindAddress")]
    bind_address: String,
    #[serde(rename = "certPath")]
    cert_path: String,
    #[serde(rename = "keyPath")]
    key_path: String,
    #[serde(rename = "identities")]
    identities: Vec<CertIdentityFileConfig>,
    #[serde(rename = "pollIntervalSec")]
    poll_interval_sec: u64,
    #[serde(rename = "renewWindowSec")]
    renew_window_sec: u64,
}

/// 单张证书的文件配置；TOML `identities` 数组与 `TEAMVIEWER_WT_IDENTITIES` JSON 共用。
#[derive(Clone, Debug, Deserialize)]
struct CertIdentityFileConfig {
    #[serde(rename = "certPath")]
    cert_path: String,
    #[serde(rename = "keyPath")]
    key_path: String,
    #[serde(default)]
    default: bool,
}

impl Default for WebTransportFileConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind_address: "0.0.0.0:8766".to_owned(),
            cert_path: String::new(),
            key_path: String::new(),
            identities: Vec::new(),
            poll_interval_sec: 300,
            renew_window_sec: 7 * 24 * 60 * 60,
        }
    }
}

/// 运行时单张证书:文件路径与是否作为无 SNI/IP 直连的兜底证书。
#[derive(Clone, Debug)]
pub struct CertIdentity {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    pub default: bool,
}

#[derive(Clone, Debug)]
pub struct WebTransportConfig {
    pub enabled: bool,
    pub bind_address: SocketAddr,
    pub identities: Vec<CertIdentity>,
    pub poll_interval_sec: u64,
    pub renew_window_sec: u64,
}

fn env_string(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn env_bool(name: &str) -> Option<bool> {
    env_string(name).map(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

const MAX_WT_IDENTITIES: usize = 16;

/// 解析 `TEAMVIEWER_WT_IDENTITIES`(JSON 数组);非法 JSON 记 error 日志后视为未配置。
fn env_identities(name: &str) -> Option<Vec<CertIdentityFileConfig>> {
    let raw = env_string(name)?;
    parse_identities_json(&raw, name)
}

fn parse_identities_json(raw: &str, name: &str) -> Option<Vec<CertIdentityFileConfig>> {
    match serde_json::from_str(raw) {
        Ok(identities) => Some(identities),
        Err(error) => {
            tracing::error!(
                %error,
                variable = name,
                "WebTransport identities 环境变量解析失败，已忽略"
            );
            None
        }
    }
}

/// 合并 TOML 与环境变量的 WebTransport 证书配置。
///
/// 证书来源优先级:`TEAMVIEWER_WT_IDENTITIES` > `TEAMVIEWER_WT_CERT_PATH`/`KEY_PATH` >
/// TOML `identities` > TOML `certPath`/`keyPath`;环境变量任一形式存在时整体替换 TOML 证书配置。
/// 单证书环境变量允许只设置其一,另一端回落 TOML 单证书配置。
fn resolve_web_transport(
    file: WebTransportFileConfig,
    env_identities: Option<Vec<CertIdentityFileConfig>>,
    env_cert: Option<String>,
    env_key: Option<String>,
) -> WebTransportConfig {
    let configured = if let Some(list) = env_identities {
        list
    } else if env_cert.is_some() || env_key.is_some() {
        vec![CertIdentityFileConfig {
            cert_path: env_cert.unwrap_or(file.cert_path),
            key_path: env_key.unwrap_or(file.key_path),
            default: false,
        }]
    } else if !file.identities.is_empty() {
        file.identities
    } else {
        vec![CertIdentityFileConfig {
            cert_path: file.cert_path,
            key_path: file.key_path,
            default: false,
        }]
    };
    if configured.len() > MAX_WT_IDENTITIES {
        tracing::warn!(
            total = configured.len(),
            max = MAX_WT_IDENTITIES,
            "WebTransport identities 超过上限，已截断"
        );
    }
    let mut identities = Vec::with_capacity(configured.len().min(MAX_WT_IDENTITIES));
    for entry in configured {
        let cert_empty = entry.cert_path.trim().is_empty();
        let key_empty = entry.key_path.trim().is_empty();
        if cert_empty || key_empty {
            if cert_empty != key_empty {
                tracing::warn!(
                    "WebTransport identities 存在缺少 certPath 或 keyPath 的条目，已忽略"
                );
            }
            continue;
        }
        if identities.len() >= MAX_WT_IDENTITIES {
            break;
        }
        identities.push(CertIdentity {
            cert_path: PathBuf::from(entry.cert_path),
            key_path: PathBuf::from(entry.key_path),
            default: entry.default,
        });
    }
    let mut default_seen = false;
    for identity in &mut identities {
        if identity.default {
            if default_seen {
                identity.default = false;
                tracing::warn!("WebTransport identities 配置了多个 default，仅第一个生效");
            } else {
                default_seen = true;
            }
        }
    }
    let bind_address = file
        .bind_address
        .parse()
        .unwrap_or(SocketAddr::from(([0, 0, 0, 0], 8766)));
    WebTransportConfig {
        enabled: file.enabled,
        bind_address,
        identities,
        poll_interval_sec: file.poll_interval_sec.clamp(30, 86_400),
        renew_window_sec: file.renew_window_sec.clamp(1, 30 * 24 * 60 * 60),
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct TimeoutConfig {
    #[serde(rename = "playerTimeoutSec")]
    player_timeout_sec: u64,
    #[serde(rename = "entityTimeoutSec")]
    entity_timeout_sec: u64,
    #[serde(rename = "waypointTimeoutSec")]
    waypoint_timeout_sec: u64,
    #[serde(rename = "battleChunkTimeoutSec")]
    battle_chunk_timeout_sec: u64,
    #[serde(rename = "battleChunkCacheRetentionSec")]
    battle_chunk_cache_retention_sec: u64,
    #[serde(rename = "battleChunkCacheMaxEntries")]
    battle_chunk_cache_max_entries: usize,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            player_timeout_sec: 120,
            entity_timeout_sec: 300,
            waypoint_timeout_sec: 60,
            battle_chunk_timeout_sec: 120,
            battle_chunk_cache_retention_sec: 7_200,
            battle_chunk_cache_max_entries: 65_536,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct RefreshConfig {
    #[serde(rename = "cooldownSec")]
    cooldown_sec: f64,
    #[serde(rename = "leadSec")]
    lead_sec: f64,
    #[serde(rename = "maxItemsPerScope")]
    max_items_per_scope: usize,
}

impl Default for RefreshConfig {
    fn default() -> Self {
        Self {
            cooldown_sec: 1.5,
            lead_sec: 1.2,
            max_items_per_scope: 64,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct ProtocolConfig {
    #[serde(rename = "digestIntervalSec")]
    digest_interval_sec: u64,
    #[serde(rename = "movementRefreshSec")]
    movement_refresh_sec: u64,
    #[serde(rename = "defaultBroadcastHz")]
    default_broadcast_hz: f64,
    #[serde(rename = "minBroadcastHz")]
    min_broadcast_hz: f64,
    #[serde(rename = "tabReportTimeoutSec")]
    tab_report_timeout_sec: u64,
    #[serde(rename = "congestionLevels")]
    congestion_levels: Vec<(usize, f64)>,
}

impl Default for ProtocolConfig {
    fn default() -> Self {
        Self {
            digest_interval_sec: 10,
            movement_refresh_sec: 10,
            default_broadcast_hz: 20.0,
            min_broadcast_hz: 2.0,
            tab_report_timeout_sec: 45,
            congestion_levels: vec![(40, 2.0), (20, 5.0), (8, 10.0)],
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FeatureConfig {
    #[serde(rename = "sameServerFilterEnabled")]
    same_server_filter_enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct TabHistoryConfig {
    enabled: bool,
    #[serde(rename = "retentionDays")]
    retention_days: u32,
    #[serde(rename = "deltaRetentionDays")]
    delta_retention_days: u32,
    #[serde(rename = "defaultChunkEntries")]
    default_chunk_entries: usize,
    #[serde(rename = "maxChunkEntries")]
    max_chunk_entries: usize,
    #[serde(rename = "maxChunkBytes")]
    max_chunk_bytes: usize,
    #[serde(rename = "maxLookupSelectors")]
    max_lookup_selectors: usize,
    #[serde(rename = "observationUpdateIntervalSec")]
    observation_update_interval_sec: u64,
}

impl Default for TabHistoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            retention_days: 400,
            delta_retention_days: 30,
            default_chunk_entries: 256,
            max_chunk_entries: 512,
            max_chunk_bytes: 256 * 1024,
            max_lookup_selectors: 128,
            observation_update_interval_sec: 300,
        }
    }
}

impl RuntimeConfig {
    pub fn load() -> Self {
        let parsed = toml::from_str::<FileConfig>(STATE_CONFIG).unwrap_or_default();
        let mut congestion_levels = parsed.protocol.congestion_levels;
        congestion_levels.retain(|(threshold, hz)| *threshold > 0 && hz.is_finite());
        // congestion_levels.sort_by(|left, right| right.0.cmp(&left.0)); // clippy::unnecessary_sort_by)
        congestion_levels.sort_by_key(|left| std::cmp::Reverse(left.0));
        if congestion_levels.is_empty() {
            congestion_levels = ProtocolConfig::default().congestion_levels;
        }
        let max_chunks = parsed.tab_history.max_chunk_entries.clamp(1, 512);
        let mut web_transport_file = parsed.web_transport;
        web_transport_file.enabled =
            env_bool("TEAMVIEWER_WT_ENABLED").unwrap_or(web_transport_file.enabled);
        web_transport_file.bind_address =
            env_string("TEAMVIEWER_WT_BIND").unwrap_or(web_transport_file.bind_address);
        if let Some(value) =
            env_string("TEAMVIEWER_WT_POLL_INTERVAL_SEC").and_then(|value| value.parse().ok())
        {
            web_transport_file.poll_interval_sec = value;
        }
        if let Some(value) =
            env_string("TEAMVIEWER_WT_RENEW_WINDOW_SEC").and_then(|value| value.parse().ok())
        {
            web_transport_file.renew_window_sec = value;
        }
        let web_transport = resolve_web_transport(
            web_transport_file,
            env_identities("TEAMVIEWER_WT_IDENTITIES"),
            env_string("TEAMVIEWER_WT_CERT_PATH"),
            env_string("TEAMVIEWER_WT_KEY_PATH"),
        );
        Self {
            player_timeout_sec: parsed.timeouts.player_timeout_sec.clamp(1, 3_600),
            entity_timeout_sec: parsed.timeouts.entity_timeout_sec.clamp(1, 3_600),
            waypoint_timeout_sec: parsed.timeouts.waypoint_timeout_sec.clamp(5, 86_400),
            battle_chunk_timeout_sec: parsed.timeouts.battle_chunk_timeout_sec.clamp(5, 86_400),
            battle_chunk_cache_retention_sec: parsed
                .timeouts
                .battle_chunk_cache_retention_sec
                .clamp(60, 604_800),
            battle_chunk_cache_max_entries: parsed
                .timeouts
                .battle_chunk_cache_max_entries
                .clamp(1_024, 1_048_576),
            refresh_cooldown_sec: parsed.refresh_request.cooldown_sec.clamp(0.1, 120.0),
            refresh_lead_sec: parsed.refresh_request.lead_sec.clamp(0.1, 30.0),
            refresh_max_items: parsed.refresh_request.max_items_per_scope.clamp(1, 500),
            digest_interval_sec: parsed.protocol.digest_interval_sec.clamp(1, 120),
            movement_refresh_sec: env_string("TEAMVIEWER_MOVEMENT_REFRESH_SEC")
                .and_then(|value| value.parse().ok())
                .unwrap_or(parsed.protocol.movement_refresh_sec)
                .clamp(1, 3_600),
            default_broadcast_hz: parsed.protocol.default_broadcast_hz.clamp(1.0, 120.0),
            min_broadcast_hz: parsed.protocol.min_broadcast_hz.clamp(0.5, 60.0),
            congestion_levels,
            tab_report_timeout_sec: parsed.protocol.tab_report_timeout_sec.clamp(5, 600),
            same_server_filter_enabled: parsed.features.same_server_filter_enabled,
            tab_history_enabled: parsed.tab_history.enabled,
            tab_history_retention_days: parsed.tab_history.retention_days.min(36_500),
            tab_history_delta_retention_days: parsed.tab_history.delta_retention_days.min(36_500),
            tab_history_default_chunk_entries: parsed
                .tab_history
                .default_chunk_entries
                .clamp(1, max_chunks),
            tab_history_max_chunk_entries: max_chunks,
            tab_history_max_chunk_bytes: parsed
                .tab_history
                .max_chunk_bytes
                .clamp(16 * 1024, 1024 * 1024),
            tab_history_max_lookup_selectors: parsed.tab_history.max_lookup_selectors.clamp(1, 512),
            tab_history_observation_update_interval_sec: parsed
                .tab_history
                .observation_update_interval_sec
                .clamp(1, 86_400),
            web_transport,
        }
    }

    pub fn broadcast_hz(&self, player_connections: usize) -> f64 {
        self.congestion_levels
            .iter()
            .find_map(|(threshold, hz)| (player_connections >= *threshold).then_some(*hz))
            .unwrap_or(self.default_broadcast_hz)
            .max(self.min_broadcast_hz)
    }

    pub fn report_interval_ticks(&self, broadcast_hz: f64) -> i32 {
        if broadcast_hz >= 20.0 {
            1
        } else if broadcast_hz >= 10.0 {
            2
        } else if broadcast_hz >= 5.0 {
            4
        } else {
            10
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CertIdentityFileConfig, MAX_WT_IDENTITIES, RuntimeConfig, WebTransportFileConfig,
        parse_identities_json, resolve_web_transport,
    };

    #[test]
    fn bundled_state_config_is_loaded() {
        let config = RuntimeConfig::load();
        assert_eq!(config.broadcast_hz(0), 20.0);
        assert_eq!(config.broadcast_hz(8), 10.0);
        assert_eq!(config.broadcast_hz(20), 5.0);
        assert_eq!(config.broadcast_hz(40), 2.0);
        assert_eq!(config.report_interval_ticks(2.0), 10);
        assert_eq!(config.battle_chunk_cache_retention_sec, 7_200);
        assert_eq!(config.battle_chunk_cache_max_entries, 65_536);
    }

    fn file_config(
        cert_path: &str,
        key_path: &str,
        identities: Vec<CertIdentityFileConfig>,
    ) -> WebTransportFileConfig {
        WebTransportFileConfig {
            enabled: true,
            bind_address: "0.0.0.0:8766".to_owned(),
            cert_path: cert_path.to_owned(),
            key_path: key_path.to_owned(),
            identities,
            poll_interval_sec: 300,
            renew_window_sec: 7 * 24 * 60 * 60,
        }
    }

    fn identity(cert: &str, key: &str, default: bool) -> CertIdentityFileConfig {
        CertIdentityFileConfig {
            cert_path: cert.to_owned(),
            key_path: key.to_owned(),
            default,
        }
    }

    #[test]
    fn toml_single_pair_becomes_single_identity() {
        let config = resolve_web_transport(
            file_config("/a/c.pem", "/a/k.pem", Vec::new()),
            None,
            None,
            None,
        );
        assert_eq!(config.identities.len(), 1);
        assert_eq!(config.identities[0].cert_path.to_str(), Some("/a/c.pem"));
        assert!(!config.identities[0].default);
    }

    #[test]
    fn toml_identities_preferred_over_single_pair() {
        let config = resolve_web_transport(
            file_config(
                "/a/c.pem",
                "/a/k.pem",
                vec![
                    identity("/b/c.pem", "/b/k.pem", false),
                    identity("/c/c.pem", "/c/k.pem", true),
                ],
            ),
            None,
            None,
            None,
        );
        assert_eq!(config.identities.len(), 2);
        assert_eq!(config.identities[1].cert_path.to_str(), Some("/c/c.pem"));
        assert!(config.identities[1].default);
    }

    #[test]
    fn env_single_pair_replaces_toml_and_falls_back_independently() {
        // 两个变量都设置时整体替换 TOML identities
        let config = resolve_web_transport(
            file_config(
                "/a/c.pem",
                "/a/k.pem",
                vec![identity("/b/c.pem", "/b/k.pem", false)],
            ),
            None,
            Some("/e/c.pem".to_owned()),
            Some("/e/k.pem".to_owned()),
        );
        assert_eq!(config.identities.len(), 1);
        assert_eq!(config.identities[0].cert_path.to_str(), Some("/e/c.pem"));

        // 只设置 CERT_PATH 时,KEY_PATH 独立回落 TOML 单证书配置
        let config = resolve_web_transport(
            file_config("/a/c.pem", "/a/k.pem", Vec::new()),
            None,
            Some("/e/c.pem".to_owned()),
            None,
        );
        assert_eq!(config.identities[0].cert_path.to_str(), Some("/e/c.pem"));
        assert_eq!(config.identities[0].key_path.to_str(), Some("/a/k.pem"));
    }

    #[test]
    fn env_identities_win_over_single_pair_env() {
        let config = resolve_web_transport(
            file_config("/a/c.pem", "/a/k.pem", Vec::new()),
            Some(vec![identity("/j/c.pem", "/j/k.pem", true)]),
            Some("/e/c.pem".to_owned()),
            Some("/e/k.pem".to_owned()),
        );
        assert_eq!(config.identities.len(), 1);
        assert_eq!(config.identities[0].cert_path.to_str(), Some("/j/c.pem"));
        assert!(config.identities[0].default);
    }

    #[test]
    fn parse_identities_json_camel_case_and_invalid() {
        let parsed = parse_identities_json(
            r#"[{"certPath":"/j/c.pem","keyPath":"/j/k.pem","default":true}]"#,
            "TEST",
        )
        .expect("valid json");
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].default);

        assert!(parse_identities_json("not json", "TEST").is_none());
        // 空数组是合法 JSON:返回 Some(空列表)
        let empty = parse_identities_json("[]", "TEST").expect("empty array is valid");
        assert!(empty.is_empty());
    }

    #[test]
    fn empty_paths_dropped_and_multiple_defaults_first_wins() {
        let config = resolve_web_transport(
            file_config(
                "",
                "",
                vec![
                    identity("/1/c.pem", "/1/k.pem", true),
                    identity("/2/c.pem", "", false),
                    identity("/3/c.pem", "/3/k.pem", true),
                    identity("/4/c.pem", "/4/k.pem", true),
                ],
            ),
            None,
            None,
            None,
        );
        // /2 缺 keyPath 被丢弃;多个 default 只保留第一个(/3 的 default 降级)
        assert_eq!(config.identities.len(), 3);
        assert!(config.identities[0].default);
        assert!(!config.identities[1].default);
        assert!(!config.identities[2].default);
    }

    #[test]
    fn identities_over_limit_are_truncated() {
        let many: Vec<_> = (0..MAX_WT_IDENTITIES + 5)
            .map(|index| {
                identity(
                    &format!("/{index}/c.pem"),
                    &format!("/{index}/k.pem"),
                    false,
                )
            })
            .collect();
        let config = resolve_web_transport(file_config("", "", many), None, None, None);
        assert_eq!(config.identities.len(), MAX_WT_IDENTITIES);
    }

    #[test]
    fn legacy_empty_config_yields_no_identities() {
        let config = resolve_web_transport(file_config("", "", Vec::new()), None, None, None);
        assert!(config.identities.is_empty());
    }

    #[test]
    fn poll_and_renew_are_clamped_and_bind_falls_back() {
        let mut file = file_config("/a/c.pem", "/a/k.pem", Vec::new());
        file.poll_interval_sec = 1;
        file.renew_window_sec = 40 * 24 * 60 * 60;
        file.bind_address = "not-an-address".to_owned();
        let config = resolve_web_transport(file, None, None, None);
        assert_eq!(config.poll_interval_sec, 30);
        assert_eq!(config.renew_window_sec, 30 * 24 * 60 * 60);
        assert_eq!(config.bind_address.to_string(), "0.0.0.0:8766");
    }
}
