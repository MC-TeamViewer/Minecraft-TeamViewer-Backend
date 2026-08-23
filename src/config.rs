use serde::Deserialize;

const STATE_CONFIG: &str = include_str!("../config/server_state_config.toml");

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub player_timeout_sec: u64,
    pub entity_timeout_sec: u64,
    pub waypoint_timeout_sec: u64,
    pub battle_chunk_timeout_sec: u64,
    pub battle_chunk_cache_retention_sec: u64,
    pub refresh_cooldown_sec: f64,
    pub refresh_lead_sec: f64,
    pub refresh_max_items: usize,
    pub digest_interval_sec: u64,
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
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            player_timeout_sec: 120,
            entity_timeout_sec: 300,
            waypoint_timeout_sec: 60,
            battle_chunk_timeout_sec: 120,
            battle_chunk_cache_retention_sec: 7_200,
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
        congestion_levels.sort_by(|left, right| right.0.cmp(&left.0));
        if congestion_levels.is_empty() {
            congestion_levels = ProtocolConfig::default().congestion_levels;
        }
        let max_chunks = parsed.tab_history.max_chunk_entries.clamp(1, 512);
        Self {
            player_timeout_sec: parsed.timeouts.player_timeout_sec.clamp(1, 3_600),
            entity_timeout_sec: parsed.timeouts.entity_timeout_sec.clamp(1, 3_600),
            waypoint_timeout_sec: parsed.timeouts.waypoint_timeout_sec.clamp(5, 86_400),
            battle_chunk_timeout_sec: parsed.timeouts.battle_chunk_timeout_sec.clamp(5, 86_400),
            battle_chunk_cache_retention_sec: parsed
                .timeouts
                .battle_chunk_cache_retention_sec
                .clamp(60, 604_800),
            refresh_cooldown_sec: parsed.refresh_request.cooldown_sec.clamp(0.1, 120.0),
            refresh_lead_sec: parsed.refresh_request.lead_sec.clamp(0.1, 30.0),
            refresh_max_items: parsed.refresh_request.max_items_per_scope.clamp(1, 500),
            digest_interval_sec: parsed.protocol.digest_interval_sec.clamp(1, 120),
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
    use super::RuntimeConfig;

    #[test]
    fn bundled_state_config_is_loaded() {
        let config = RuntimeConfig::load();
        assert_eq!(config.broadcast_hz(0), 20.0);
        assert_eq!(config.broadcast_hz(8), 10.0);
        assert_eq!(config.broadcast_hz(20), 5.0);
        assert_eq!(config.broadcast_hz(40), 2.0);
        assert_eq!(config.report_interval_ticks(2.0), 10);
    }
}
