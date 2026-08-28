use std::{fmt, str::FromStr, sync::Arc};

use crate::proto::teamviewer::v1::{Patch, PlayerReportBundle, SnapshotFull};
use serde_json::{Map, Value, json};

pub const CURRENT_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::new(0, 8, 0);
pub const MINIMUM_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::new(0, 6, 1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolVersion {
    major: u32,
    minor: u32,
    patch: u32,
}

impl ProtocolVersion {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl FromStr for ProtocolVersion {
    type Err = ProtocolVersionParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut parts = value.trim().split('.');
        let parse_part = |part: Option<&str>| {
            let part = part.ok_or(ProtocolVersionParseError)?;
            if part.is_empty() || !part.bytes().all(|value| value.is_ascii_digit()) {
                return Err(ProtocolVersionParseError);
            }
            part.parse::<u32>().map_err(|_| ProtocolVersionParseError)
        };
        let version = Self::new(
            parse_part(parts.next())?,
            parse_part(parts.next())?,
            parse_part(parts.next())?,
        );
        if parts.next().is_some() {
            return Err(ProtocolVersionParseError);
        }
        Ok(version)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProtocolVersionParseError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProtocolEpoch {
    V0_6_1,
    V0_6_2,
    V0_6_3,
    V0_6_4,
    V0_6_5,
    V0_7_0,
    V0_7_1,
    V0_8_0,
}

impl ProtocolEpoch {
    pub const ALL: [Self; 8] = [
        Self::V0_6_1,
        Self::V0_6_2,
        Self::V0_6_3,
        Self::V0_6_4,
        Self::V0_6_5,
        Self::V0_7_0,
        Self::V0_7_1,
        Self::V0_8_0,
    ];

    pub const fn version(self) -> ProtocolVersion {
        match self {
            Self::V0_6_1 => ProtocolVersion::new(0, 6, 1),
            Self::V0_6_2 => ProtocolVersion::new(0, 6, 2),
            Self::V0_6_3 => ProtocolVersion::new(0, 6, 3),
            Self::V0_6_4 => ProtocolVersion::new(0, 6, 4),
            Self::V0_6_5 => ProtocolVersion::new(0, 6, 5),
            Self::V0_7_0 => ProtocolVersion::new(0, 7, 0),
            Self::V0_7_1 => ProtocolVersion::new(0, 7, 1),
            Self::V0_8_0 => ProtocolVersion::new(0, 8, 0),
        }
    }

    pub fn from_version(version: ProtocolVersion) -> Self {
        Self::ALL
            .into_iter()
            .rev()
            .find(|epoch| version >= epoch.version())
            .unwrap_or(Self::V0_6_1)
    }
}

impl fmt::Display for ProtocolEpoch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.version().fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompatibilityRuleId {
    OmitBattleChunkMode,
    DefaultPlayerRole,
    OmitLastSeenPlayers,
    OmitPlayerSourceMetadata,
    FullSnapshotForClearFields,
    DisableTabHistory,
    LegacyBattleChunkDigest,
}

impl CompatibilityRuleId {
    pub const ALL: [Self; 7] = [
        Self::OmitBattleChunkMode,
        Self::DefaultPlayerRole,
        Self::OmitLastSeenPlayers,
        Self::OmitPlayerSourceMetadata,
        Self::FullSnapshotForClearFields,
        Self::DisableTabHistory,
        Self::LegacyBattleChunkDigest,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Self::OmitBattleChunkMode => "omit_battle_chunk_mode",
            Self::DefaultPlayerRole => "default_player_role",
            Self::OmitLastSeenPlayers => "omit_last_seen_players",
            Self::OmitPlayerSourceMetadata => "omit_player_source_metadata",
            Self::FullSnapshotForClearFields => "full_snapshot_for_clear_fields",
            Self::DisableTabHistory => "disable_tab_history",
            Self::LegacyBattleChunkDigest => "legacy_battle_chunk_digest",
        }
    }

    pub const fn introduced_in(self) -> ProtocolVersion {
        match self {
            Self::OmitBattleChunkMode => ProtocolVersion::new(0, 6, 2),
            Self::DefaultPlayerRole => ProtocolVersion::new(0, 6, 3),
            Self::OmitLastSeenPlayers => ProtocolVersion::new(0, 6, 4),
            Self::OmitPlayerSourceMetadata | Self::FullSnapshotForClearFields => {
                ProtocolVersion::new(0, 6, 5)
            }
            Self::DisableTabHistory => ProtocolVersion::new(0, 7, 0),
            Self::LegacyBattleChunkDigest => ProtocolVersion::new(0, 7, 1),
        }
    }

    pub const fn summary(self) -> &'static str {
        match self {
            Self::OmitBattleChunkMode => "移除旧客户端无法解码的战区模式字段",
            Self::DefaultPlayerRole => "按普通玩家处理不支持角色协商的客户端",
            Self::OmitLastSeenPlayers => "不收发或摘要离线玩家最后位置",
            Self::OmitPlayerSourceMetadata => "移除玩家位置来源元数据",
            Self::FullSnapshotForClearFields => "字段删除时重建受影响对象",
            Self::DisableTabHistory => "不声明、接收或持久化 Tab 历史能力",
            Self::LegacyBattleChunkDigest => "使用旧版 keyed 战区摘要合同",
        }
    }

    pub fn active_for(self, version: ProtocolVersion) -> bool {
        version < self.introduced_in()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProtocolProfile {
    peer_current: ProtocolVersion,
    peer_minimum: ProtocolVersion,
    negotiated: ProtocolVersion,
    epoch: ProtocolEpoch,
}

impl ProtocolProfile {
    pub fn negotiate(peer_current: &str, peer_minimum: &str) -> Result<Self, NegotiationError> {
        let peer_current = peer_current
            .parse()
            .map_err(|_| NegotiationError::InvalidProtocolVersion)?;
        let peer_minimum = peer_minimum
            .parse()
            .map_err(|_| NegotiationError::InvalidProtocolVersion)?;
        if peer_minimum > peer_current {
            return Err(NegotiationError::InvalidProtocolVersion);
        }
        if peer_current < MINIMUM_PROTOCOL_VERSION {
            return Err(NegotiationError::ClientProtocolTooOld);
        }
        if CURRENT_PROTOCOL_VERSION < peer_minimum {
            return Err(NegotiationError::ServerProtocolTooOld);
        }
        let negotiated = peer_current.min(CURRENT_PROTOCOL_VERSION);
        Ok(Self {
            peer_current,
            peer_minimum,
            negotiated,
            epoch: ProtocolEpoch::from_version(negotiated),
        })
    }

    pub fn for_accepted_peer(peer_current: &str) -> Option<Self> {
        let peer_current: ProtocolVersion = peer_current.parse().ok()?;
        let negotiated = peer_current.min(CURRENT_PROTOCOL_VERSION);
        (negotiated >= MINIMUM_PROTOCOL_VERSION).then_some(Self {
            peer_current,
            peer_minimum: MINIMUM_PROTOCOL_VERSION,
            negotiated,
            epoch: ProtocolEpoch::from_version(negotiated),
        })
    }

    pub const fn peer_current(self) -> ProtocolVersion {
        self.peer_current
    }

    pub const fn peer_minimum(self) -> ProtocolVersion {
        self.peer_minimum
    }

    pub const fn negotiated(self) -> ProtocolVersion {
        self.negotiated
    }

    pub const fn epoch(self) -> ProtocolEpoch {
        self.epoch
    }

    pub fn active_rules(self) -> impl Iterator<Item = CompatibilityRuleId> {
        CompatibilityRuleId::ALL
            .into_iter()
            .filter(move |rule| rule.active_for(self.negotiated))
    }

    pub fn compatibility_rule_count(self) -> usize {
        self.active_rules().count()
    }

    pub fn supports_battle_chunk_mode(self) -> bool {
        self.negotiated >= ProtocolVersion::new(0, 6, 2)
    }

    pub fn supports_external_source_role(self) -> bool {
        self.negotiated >= ProtocolVersion::new(0, 6, 3)
    }

    pub fn supports_last_seen_players(self) -> bool {
        self.negotiated >= ProtocolVersion::new(0, 6, 4)
    }

    pub fn supports_player_source_metadata(self) -> bool {
        self.negotiated >= ProtocolVersion::new(0, 6, 5)
    }

    pub fn supports_clear_fields(self) -> bool {
        self.negotiated >= ProtocolVersion::new(0, 6, 5)
    }

    pub fn supports_tab_history(self) -> bool {
        self.negotiated >= ProtocolVersion::new(0, 7, 0)
    }

    pub fn uses_structured_battle_chunk_digest(self) -> bool {
        self.negotiated >= ProtocolVersion::new(0, 7, 1)
    }

    pub fn supports_relationships(self) -> bool {
        self.negotiated >= ProtocolVersion::new(0, 8, 0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NegotiationError {
    InvalidProtocolVersion,
    ClientProtocolTooOld,
    ServerProtocolTooOld,
}

impl NegotiationError {
    pub const fn reason(self) -> &'static str {
        match self {
            Self::InvalidProtocolVersion => "invalid_protocol_version",
            Self::ClientProtocolTooOld => "client_protocol_too_old",
            Self::ServerProtocolTooOld => "server_protocol_too_old",
        }
    }
}

pub fn project_snapshot(
    profile: ProtocolProfile,
    snapshot: Arc<SnapshotFull>,
) -> Arc<SnapshotFull> {
    let needs_projection = !profile.supports_battle_chunk_mode()
        || !profile.supports_last_seen_players()
        || !profile.supports_player_source_metadata()
        || !profile.supports_tab_history();
    if !needs_projection {
        return snapshot;
    }

    let mut projected = (*snapshot).clone();
    if !profile.supports_battle_chunk_mode() {
        for entry in &mut projected.battle_chunks {
            if let Some(data) = &mut entry.data {
                data.mode = None;
            }
        }
    }
    if !profile.supports_last_seen_players() {
        projected.last_seen_players.clear();
    }
    if !profile.supports_player_source_metadata() {
        for player in projected.players.values_mut() {
            player.position_source_id = None;
            player.position_source_kind = None;
            player.position_source_display_name = None;
            player.position_resolution = None;
        }
    }
    if !profile.supports_tab_history()
        && let Some(tab_state) = &mut projected.tab_state
    {
        for report in tab_state.reports.values_mut() {
            for player in &mut report.players {
                player.scoreboard_team_id = None;
                player.scoreboard_prefix = None;
                player.scoreboard_suffix = None;
                player.scoreboard_color_rgb = None;
                player.formatted_display_name = None;
                player.formatted_scoreboard_prefix = None;
                player.formatted_scoreboard_suffix = None;
            }
        }
    }
    Arc::new(projected)
}

pub fn sanitize_player_report(profile: ProtocolProfile, report: &mut PlayerReportBundle) {
    if !profile.supports_battle_chunk_mode()
        && let Some(observation) = &mut report.battle_map_observation
    {
        observation.mode = None;
    }
    if !profile.supports_external_source_role() {
        report.external_source_status = None;
    }
    if !profile.supports_last_seen_players() {
        report.last_seen_players_replace = None;
        report.last_seen_players_patch = None;
    }
}

pub fn adapt_outbound_patch(profile: ProtocolProfile, mut patch: Patch) -> Patch {
    if profile.supports_clear_fields() {
        return patch;
    }

    if let Some(scope) = &mut patch.players {
        for upsert in &mut scope.upsert {
            if !upsert.clear_fields.is_empty() {
                if !scope.delete.contains(&upsert.id) {
                    scope.delete.push(upsert.id.clone());
                }
                upsert.clear_fields.clear();
            }
        }
    }
    if let Some(scope) = &mut patch.entities {
        for upsert in &mut scope.upsert {
            if !upsert.clear_fields.is_empty() {
                if !scope.delete.contains(&upsert.id) {
                    scope.delete.push(upsert.id.clone());
                }
                upsert.clear_fields.clear();
            }
        }
    }
    if let Some(scope) = &mut patch.waypoints {
        for upsert in &mut scope.upsert {
            if !upsert.clear_fields.is_empty() {
                if !scope.delete.contains(&upsert.id) {
                    scope.delete.push(upsert.id.clone());
                }
                upsert.clear_fields.clear();
            }
        }
    }
    if let Some(scope) = &mut patch.battle_chunks {
        for upsert in &mut scope.upsert {
            if !upsert.clear_fields.is_empty() {
                if let Some(reference) = &upsert.r#ref
                    && !scope.delete.contains(reference)
                {
                    scope.delete.push(reference.clone());
                }
                upsert.clear_fields.clear();
            }
        }
    }
    if let Some(scope) = &mut patch.last_seen_players {
        for upsert in &mut scope.upsert {
            if !upsert.clear_fields.is_empty() {
                if !scope.delete.contains(&upsert.id) {
                    scope.delete.push(upsert.id.clone());
                }
                upsert.clear_fields.clear();
            }
        }
    }
    if let Some(scope) = &mut patch.player_marks {
        for upsert in &mut scope.upsert {
            if !upsert.clear_fields.is_empty() {
                if !scope.delete.contains(&upsert.id) {
                    scope.delete.push(upsert.id.clone());
                }
                upsert.clear_fields.clear();
            }
        }
    }
    patch
}

pub struct CompatibilityOverview {
    pub connection_details: Value,
    pub summary: Value,
}

pub fn compatibility_overview(connection_details: &Value) -> CompatibilityOverview {
    let mut epoch_counts = ProtocolEpoch::ALL
        .into_iter()
        .map(|epoch| (epoch, 0_u64))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut rule_counts = CompatibilityRuleId::ALL
        .into_iter()
        .map(|rule| (rule.id(), 0_u64))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut compatibility_connection_count = 0_u64;
    let mut active_rule_binding_count = 0_u64;

    let details = connection_details
        .as_array()
        .into_iter()
        .flatten()
        .map(|connection| {
            let mut connection = connection.as_object().cloned().unwrap_or_default();
            let profile = connection
                .get("protocolVersion")
                .and_then(Value::as_str)
                .and_then(ProtocolProfile::for_accepted_peer);
            let rules = profile
                .map(|profile| profile.active_rules().collect::<Vec<_>>())
                .unwrap_or_default();
            let connected = connection
                .get("connected")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            if connected && let Some(profile) = profile {
                *epoch_counts.entry(profile.epoch()).or_default() += 1;
                if !rules.is_empty() {
                    compatibility_connection_count += 1;
                }
                active_rule_binding_count += rules.len() as u64;
                for rule in &rules {
                    *rule_counts.entry(rule.id()).or_default() += 1;
                }
            }
            insert_profile_fields(&mut connection, profile, &rules);
            Value::Object(connection)
        })
        .collect::<Vec<_>>();

    let connections_by_epoch = ProtocolEpoch::ALL
        .into_iter()
        .map(|epoch| {
            json!({
                "epoch": epoch.to_string(),
                "connections": epoch_counts.get(&epoch).copied().unwrap_or_default(),
            })
        })
        .collect::<Vec<_>>();
    let rules = CompatibilityRuleId::ALL
        .into_iter()
        .map(|rule| {
            json!({
                "id": rule.id(),
                "introducedIn": rule.introduced_in().to_string(),
                "summary": rule.summary(),
                "activeConnections": rule_counts.get(rule.id()).copied().unwrap_or_default(),
            })
        })
        .collect::<Vec<_>>();

    CompatibilityOverview {
        connection_details: Value::Array(details),
        summary: json!({
            "currentVersion": CURRENT_PROTOCOL_VERSION.to_string(),
            "minimumSupportedVersion": MINIMUM_PROTOCOL_VERSION.to_string(),
            "knownEpochCount": ProtocolEpoch::ALL.len(),
            "registeredRuleCount": CompatibilityRuleId::ALL.len(),
            "compatibilityConnectionCount": compatibility_connection_count,
            "activeRuleBindingCount": active_rule_binding_count,
            "connectionsByEpoch": connections_by_epoch,
            "rules": rules,
        }),
    }
}

fn insert_profile_fields(
    connection: &mut Map<String, Value>,
    profile: Option<ProtocolProfile>,
    rules: &[CompatibilityRuleId],
) {
    connection.insert(
        "negotiatedProtocolVersion".to_owned(),
        profile
            .map(|profile| Value::String(profile.negotiated().to_string()))
            .unwrap_or(Value::Null),
    );
    connection.insert(
        "compatibilityEpoch".to_owned(),
        profile
            .map(|profile| Value::String(profile.epoch().to_string()))
            .unwrap_or(Value::Null),
    );
    connection.insert(
        "compatibilityRuleCount".to_owned(),
        Value::from(rules.len()),
    );
    connection.insert(
        "activeCompatibilityRules".to_owned(),
        Value::Array(
            rules
                .iter()
                .map(|rule| Value::String(rule.id().to_owned()))
                .collect(),
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::teamviewer::v1::{
        BattleChunkEntry, BattleChunkPatchScope, BattleChunkRef, BattleChunkUpsert,
        BattleChunkValue, BattleMapObservation, EntityPatchScope, EntityUpsert,
        ExternalSourceStatus, LastSeenPlayerPatchScope, LastSeenPlayerUpsert,
        LastSeenPlayersReplace, PlayerData, PlayerMarkPatchScope, PlayerMarkUpsert,
        PlayerPatchScope, PlayerUpsert, TabPlayerEntry, TabPlayerReport, WaypointPatchScope,
        WaypointUpsert, WebMapTabState,
    };
    use std::collections::HashMap;

    #[test]
    fn parses_only_three_numeric_components() {
        assert_eq!("0.7.1".parse(), Ok(ProtocolVersion::new(0, 7, 1)));
        assert_eq!(" 0.7.1 ".parse(), Ok(ProtocolVersion::new(0, 7, 1)));
        for invalid in ["0.7", "0.7.1.0", "v0.7.1", "0.x.1", ""] {
            assert!(invalid.parse::<ProtocolVersion>().is_err(), "{invalid}");
        }
    }

    #[test]
    fn negotiates_both_ends_of_the_compatibility_interval() {
        let future = ProtocolProfile::negotiate("0.9.0", "0.7.1").expect("overlap");
        assert_eq!(future.negotiated(), CURRENT_PROTOCOL_VERSION);
        assert_eq!(
            ProtocolProfile::negotiate("0.6.0", "0.6.0"),
            Err(NegotiationError::ClientProtocolTooOld)
        );
        assert_eq!(
            ProtocolProfile::negotiate("0.9.0", "0.9.0"),
            Err(NegotiationError::ServerProtocolTooOld)
        );
        assert_eq!(
            ProtocolProfile::negotiate("0.7.0", "0.7.1"),
            Err(NegotiationError::InvalidProtocolVersion)
        );
    }

    #[test]
    fn epoch_rule_matrix_is_explicit() {
        for (version, epoch, count) in [
            ("0.6.1", ProtocolEpoch::V0_6_1, 7),
            ("0.6.2", ProtocolEpoch::V0_6_2, 6),
            ("0.6.3", ProtocolEpoch::V0_6_3, 5),
            ("0.6.4", ProtocolEpoch::V0_6_4, 4),
            ("0.6.5", ProtocolEpoch::V0_6_5, 2),
            ("0.7.0", ProtocolEpoch::V0_7_0, 1),
            ("0.7.1", ProtocolEpoch::V0_7_1, 0),
            ("0.8.0", ProtocolEpoch::V0_8_0, 0),
        ] {
            let profile = ProtocolProfile::negotiate(version, "0.6.1").expect("supported");
            assert_eq!(profile.epoch(), epoch);
            assert_eq!(profile.compatibility_rule_count(), count);
        }
    }

    #[test]
    fn legacy_projection_does_not_mutate_canonical_snapshot() {
        let snapshot = Arc::new(SnapshotFull {
            players: HashMap::from([(
                "player".to_owned(),
                PlayerData {
                    position_source_id: Some("source".to_owned()),
                    ..Default::default()
                },
            )]),
            battle_chunks: vec![BattleChunkEntry {
                data: Some(BattleChunkValue {
                    mode: Some("simmc".to_owned()),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            tab_state: Some(WebMapTabState {
                reports: HashMap::from([(
                    "source".to_owned(),
                    TabPlayerReport {
                        players: vec![TabPlayerEntry {
                            scoreboard_team_id: Some("team".to_owned()),
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            }),
            ..Default::default()
        });
        let profile = ProtocolProfile::negotiate("0.6.1", "0.6.1").expect("supported");
        let projected = project_snapshot(profile, snapshot.clone());
        assert!(projected.players["player"].position_source_id.is_none());
        assert!(
            projected.battle_chunks[0]
                .data
                .as_ref()
                .unwrap()
                .mode
                .is_none()
        );
        assert!(
            projected.tab_state.as_ref().unwrap().reports["source"].players[0]
                .scoreboard_team_id
                .is_none()
        );
        assert_eq!(
            snapshot.players["player"].position_source_id.as_deref(),
            Some("source")
        );
        assert_eq!(
            snapshot.battle_chunks[0]
                .data
                .as_ref()
                .unwrap()
                .mode
                .as_deref(),
            Some("simmc")
        );
    }

    #[test]
    fn overview_counts_only_current_connections() {
        let overview = compatibility_overview(&json!([
            {"protocolVersion":"0.6.4", "connected":true},
            {"protocolVersion":"0.7.1", "connected":true},
            {"protocolVersion":"0.6.1", "connected":false}
        ]));
        assert_eq!(overview.summary["registeredRuleCount"], 7);
        assert_eq!(overview.summary["compatibilityConnectionCount"], 1);
        assert_eq!(overview.summary["activeRuleBindingCount"], 4);
        assert_eq!(overview.connection_details[0]["compatibilityRuleCount"], 4);
        assert_eq!(overview.connection_details[1]["compatibilityRuleCount"], 0);
        assert_eq!(overview.connection_details[2]["compatibilityRuleCount"], 7);
    }

    #[test]
    fn inbound_report_is_sanitized_to_the_negotiated_epoch() {
        let mut report = PlayerReportBundle {
            battle_map_observation: Some(BattleMapObservation {
                mode: Some("simmc".to_owned()),
                ..Default::default()
            }),
            external_source_status: Some(ExternalSourceStatus::default()),
            last_seen_players_replace: Some(LastSeenPlayersReplace::default()),
            ..Default::default()
        };
        sanitize_player_report(
            ProtocolProfile::negotiate("0.6.1", "0.6.1").expect("supported"),
            &mut report,
        );
        assert!(report.battle_map_observation.unwrap().mode.is_none());
        assert!(report.external_source_status.is_none());
        assert!(report.last_seen_players_replace.is_none());
    }

    #[test]
    fn legacy_patch_rebuilds_only_objects_that_clear_fields() {
        let mut patch = Patch {
            players: Some(PlayerPatchScope {
                upsert: vec![PlayerUpsert {
                    id: "player".to_owned(),
                    clear_fields: vec!["health".to_owned()],
                    ..Default::default()
                }],
                ..Default::default()
            }),
            entities: Some(EntityPatchScope {
                upsert: vec![EntityUpsert {
                    id: "entity".to_owned(),
                    clear_fields: vec!["entityName".to_owned()],
                    ..Default::default()
                }],
                ..Default::default()
            }),
            waypoints: Some(WaypointPatchScope {
                upsert: vec![WaypointUpsert {
                    id: "waypoint".to_owned(),
                    clear_fields: vec!["symbol".to_owned()],
                    ..Default::default()
                }],
                ..Default::default()
            }),
            battle_chunks: Some(BattleChunkPatchScope {
                upsert: vec![BattleChunkUpsert {
                    r#ref: Some(BattleChunkRef {
                        dimension: "minecraft:overworld".to_owned(),
                        ..Default::default()
                    }),
                    clear_fields: vec!["mode".to_owned()],
                    ..Default::default()
                }],
                ..Default::default()
            }),
            last_seen_players: Some(LastSeenPlayerPatchScope {
                upsert: vec![LastSeenPlayerUpsert {
                    id: "last-seen".to_owned(),
                    clear_fields: vec!["playerName".to_owned()],
                    ..Default::default()
                }],
                ..Default::default()
            }),
            player_marks: Some(PlayerMarkPatchScope {
                upsert: vec![PlayerMarkUpsert {
                    id: "mark".to_owned(),
                    clear_fields: vec!["label".to_owned()],
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        patch.players.as_mut().unwrap().upsert.push(PlayerUpsert {
            id: "ordinary-update".to_owned(),
            ..Default::default()
        });

        let adapted = adapt_outbound_patch(
            ProtocolProfile::negotiate("0.6.4", "0.6.1").expect("supported"),
            patch,
        );

        let players = adapted.players.unwrap();
        assert_eq!(players.delete, ["player"]);
        assert!(
            players
                .upsert
                .iter()
                .all(|value| value.clear_fields.is_empty())
        );
        assert!(!players.delete.contains(&"ordinary-update".to_owned()));
        assert_eq!(adapted.entities.unwrap().delete, ["entity"]);
        assert_eq!(adapted.waypoints.unwrap().delete, ["waypoint"]);
        assert_eq!(adapted.battle_chunks.unwrap().delete.len(), 1);
        assert_eq!(adapted.last_seen_players.unwrap().delete, ["last-seen"]);
        assert_eq!(adapted.player_marks.unwrap().delete, ["mark"]);
    }
}
