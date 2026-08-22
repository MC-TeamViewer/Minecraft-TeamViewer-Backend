use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;
use prost::Message;
use serde_json::{Value, json};
use sha1::{Digest as Sha1Digest, Sha1};
use tokio::sync::{mpsc, oneshot, watch};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use crate::config::RuntimeConfig;
use crate::proto::teamviewer::v1::{
    BattleChunkEntry, BattleChunkMetaEntry, BattleChunkMetaSnapshot, BattleChunkPatchScope,
    BattleChunkRef, BattleChunkUpsert, BattleChunkValue, BattleMapObservation, Digest, EntityData,
    EntityDelta, EntityPatchScope, EntityUpsert, LastSeenPlayerData, LastSeenPlayerPatchScope,
    LastSeenPlayerUpsert, Patch, PlayerData, PlayerDelta, PlayerMark, PlayerMarkPatchScope,
    PlayerMarkUpsert, PlayerPatchScope, PlayerPositionSourceKind, PlayerReportBundle, PlayerUpsert,
    RefreshRequest, ReportRateHint, SameServerGroup, SameServerGroupList, SnapshotFull, StringList,
    TabHistoryHead, TabPlayerEntry, TabPlayerReport, TabStatePatch, WaypointData, WaypointDelta,
    WaypointPatchScope, WaypointUpsert, WebMapAck, WebMapClearAllPlayerMarksAckDetail,
    WebMapCommand, WebMapPlayerMarkAckDetail, WebMapSameServerFilterAckDetail, WebMapTabState,
    WebMapTacticalWaypointAckDetail, WebMapWaypointsDeleteAckDetail, WireChannel, WireEnvelope,
    web_map_ack, web_map_command, wire_envelope,
};

pub const EVENT_CAPACITY: usize = 2_048;
pub const CONTROL_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionKind {
    Player,
    ExternalSource,
    WebMap,
}

#[derive(Clone)]
pub struct StateFrame {
    pub revision: u64,
    pub bytes: Arc<[u8]>,
}

pub struct RegisterConnection {
    pub id: String,
    pub room: String,
    pub protocol: String,
    pub kind: ConnectionKind,
    pub display_name: Option<String>,
    pub position_resolution: Option<f64>,
    pub program_version: String,
    pub remote_addr: String,
    pub control: mpsc::Sender<Arc<[u8]>>,
    pub state: watch::Sender<Option<StateFrame>>,
}

pub enum RelayEvent {
    Register(RegisterConnection),
    Disconnect {
        id: String,
    },
    Delivered {
        id: String,
        revision: u64,
    },
    PlayerReport {
        id: String,
        report: Box<PlayerReportBundle>,
    },
    Resync {
        id: String,
    },
    BattleChunkMeta {
        id: String,
        battle_chunks: Vec<BattleChunkRef>,
    },
    WebMapCommand {
        id: String,
        command: WebMapCommand,
    },
    TabHistorySubscription {
        id: String,
        enabled: bool,
    },
    TabHistoryChanged {
        room: String,
        head: TabHistoryHead,
    },
    Snapshot {
        room: Option<String>,
        reply: oneshot::Sender<Value>,
    },
    DeleteLastSeen {
        records: Vec<(String, String, String)>,
        reply: oneshot::Sender<usize>,
    },
}

#[derive(Clone)]
pub struct RelayHandle {
    tx: mpsc::Sender<RelayEvent>,
    player_connections: Arc<std::sync::atomic::AtomicUsize>,
}

impl RelayHandle {
    pub fn spawn(config: Arc<RuntimeConfig>) -> Self {
        let (tx, rx) = mpsc::channel(EVENT_CAPACITY);
        let player_connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        tokio::spawn(Relay::new(config, player_connections.clone()).run(rx));
        Self {
            tx,
            player_connections,
        }
    }

    pub async fn send(&self, event: RelayEvent) -> anyhow::Result<()> {
        self.tx.send(event).await.context("relay task stopped")
    }

    pub fn sender(&self) -> mpsc::Sender<RelayEvent> {
        self.tx.clone()
    }

    pub fn player_connection_count(&self) -> usize {
        self.player_connections
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub async fn snapshot(&self, room: Option<String>) -> anyhow::Result<Value> {
        let (reply, response) = oneshot::channel();
        self.send(RelayEvent::Snapshot { room, reply }).await?;
        response.await.context("relay snapshot response dropped")
    }

    pub async fn delete_last_seen(
        &self,
        records: Vec<(String, String, String)>,
    ) -> anyhow::Result<usize> {
        let (reply, response) = oneshot::channel();
        self.send(RelayEvent::DeleteLastSeen { records, reply })
            .await?;
        response.await.context("relay delete response dropped")
    }
}

struct Connection {
    room: String,
    protocol: String,
    kind: ConnectionKind,
    display_name: Option<String>,
    position_resolution: Option<f64>,
    program_version: String,
    remote_addr: String,
    connected_at: i64,
    control: mpsc::Sender<Arc<[u8]>>,
    state: watch::Sender<Option<StateFrame>>,
    delivered_revision: u64,
    queued_revision: u64,
    delivered_snapshot: Option<Arc<SnapshotFull>>,
    queued_snapshot: Option<Arc<SnapshotFull>>,
    force_full: bool,
    tab_history_subscribed: bool,
    last_digest_sent: Option<Instant>,
}

#[derive(Clone)]
struct Timed<T> {
    data: T,
    received_at: Instant,
}

impl<T> Timed<T> {
    fn new(data: T, received_at: Instant) -> Self {
        Self { data, received_at }
    }
}

#[derive(Default)]
struct SourceState {
    room: String,
    players: HashMap<String, Timed<PlayerData>>,
    entities: HashMap<String, Timed<EntityData>>,
    waypoints: HashMap<String, Timed<WaypointData>>,
    last_seen_players: HashMap<String, Timed<LastSeenPlayerData>>,
    tab_players: Vec<TabPlayerEntry>,
    tab_received_at: Option<Instant>,
    tab_timestamp: Option<f64>,
    battle_chunks: HashMap<String, Timed<BattleChunkEntry>>,
    battle_projection: Option<BattleProjection>,
    external_health: Option<i32>,
    external_failure_code: Option<String>,
    external_status_received_at: Option<f64>,
    external_last_healthy_at: Option<f64>,
}

#[derive(Default, Clone)]
struct BattleProjection {
    dimension: String,
    base_chunk_x: i32,
    base_chunk_z: i32,
    snapshot_observed_at: i64,
    chunk_ids: Vec<String>,
}

#[derive(Default)]
struct RoomFrame {
    revision: u64,
    snapshot: SnapshotFull,
    web_snapshot: SnapshotFull,
    selected_players: HashMap<String, String>,
    selected_entities: HashMap<String, String>,
    selected_waypoints: HashMap<String, String>,
    selected_battle_chunks: HashMap<String, String>,
    selected_last_seen_players: HashMap<String, String>,
}

#[derive(Default, Clone)]
struct ScopeSelections {
    players: HashMap<String, String>,
    entities: HashMap<String, String>,
    waypoints: HashMap<String, String>,
    battle_chunks: HashMap<String, String>,
    last_seen_players: HashMap<String, String>,
}

#[derive(Default)]
struct SameServerGrouping {
    active_sources: HashSet<String>,
    source_to_group: HashMap<String, String>,
    groups: Vec<SameServerGroup>,
}

struct Relay {
    connections: HashMap<String, Connection>,
    sources: HashMap<String, SourceState>,
    rooms: HashMap<String, RoomFrame>,
    dirty: bool,
    config: Arc<RuntimeConfig>,
    player_connection_count: Arc<std::sync::atomic::AtomicUsize>,
    broadcast_hz: f64,
    last_refresh_sent: HashMap<String, Instant>,
    player_marks: HashMap<String, PlayerMark>,
    same_server_filter_enabled: bool,
    scoped_selections: HashMap<(String, Vec<String>), ScopeSelections>,
    disconnected_external_sources: VecDeque<Value>,
}

impl Relay {
    fn new(
        config: Arc<RuntimeConfig>,
        player_connection_count: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        let same_server_filter_enabled = config.same_server_filter_enabled;
        Self {
            connections: HashMap::new(),
            sources: HashMap::new(),
            rooms: HashMap::new(),
            dirty: false,
            broadcast_hz: config.default_broadcast_hz,
            config,
            player_connection_count,
            last_refresh_sent: HashMap::new(),
            player_marks: HashMap::new(),
            same_server_filter_enabled,
            scoped_selections: HashMap::new(),
            disconnected_external_sources: VecDeque::new(),
        }
    }

    async fn run(mut self, mut events: mpsc::Receiver<RelayEvent>) {
        let mut next_tick = tokio::time::Instant::now();
        loop {
            tokio::select! {
                event = events.recv() => {
                    let Some(event) = event else { break };
                    self.handle_event(event);
                }
                _ = tokio::time::sleep_until(next_tick) => {
                    let next_hz = self.config.broadcast_hz(
                        self.player_connection_count.load(std::sync::atomic::Ordering::Relaxed)
                    );
                    if (next_hz - self.broadcast_hz).abs() > f64::EPSILON {
                        self.broadcast_hz = next_hz;
                        self.broadcast_report_rate_hints("congestion");
                    }
                    let now = Instant::now();
                    if self.cleanup_timeouts(now) {
                        self.dirty = true;
                    }
                    self.send_preexpiry_refresh_requests(now);
                    if self.dirty {
                        self.broadcast();
                        self.dirty = false;
                    }
                    self.send_due_digests(now);
                    next_tick = tokio::time::Instant::now()
                        + Duration::from_secs_f64(
                            1.0 / self.broadcast_hz.max(self.config.min_broadcast_hz)
                        );
                }
            }
        }
    }

    fn handle_event(&mut self, event: RelayEvent) {
        match event {
            RelayEvent::Register(register) => {
                self.connections.insert(
                    register.id,
                    Connection {
                        room: register.room,
                        protocol: register.protocol,
                        kind: register.kind,
                        display_name: register.display_name,
                        position_resolution: register.position_resolution,
                        program_version: register.program_version,
                        remote_addr: register.remote_addr,
                        connected_at: unix_millis(),
                        control: register.control,
                        state: register.state,
                        delivered_revision: 0,
                        queued_revision: 0,
                        delivered_snapshot: None,
                        queued_snapshot: None,
                        force_full: true,
                        tab_history_subscribed: false,
                        last_digest_sent: None,
                    },
                );
                self.update_player_connection_count();
                self.dirty = true;
            }
            RelayEvent::Disconnect { id } => {
                let disconnected = self.connections.remove(&id);
                if let Some(connection) = disconnected
                    && connection.kind == ConnectionKind::ExternalSource
                {
                    let source = self.sources.get(&id);
                    let health = source
                        .and_then(|source| source.external_health)
                        .and_then(|health| {
                            crate::proto::teamviewer::v1::ExternalSourceHealth::try_from(health)
                                .ok()
                        })
                        .map(|health| health.as_str_name());
                    self.disconnected_external_sources.push_back(json!({
                        "channel":"external_source",
                        "actorId":id,
                        "displayName":connection.display_name.as_deref().unwrap_or(&id),
                        "roomCode":connection.room,
                        "protocolVersion":connection.protocol,
                        "programVersion":connection.program_version,
                        "remoteAddr":connection.remote_addr,
                        "connectedAt":connection.connected_at,
                        "connected":false,
                        "disconnectedAt":unix_millis(),
                        "health":health,
                        "failureCode":source.and_then(|source| source.external_failure_code.clone()),
                        "statusReceivedAt":source.and_then(|source| source.external_status_received_at),
                        "lastHealthyAt":source.and_then(|source| source.external_last_healthy_at),
                        "lastSeenPlayerCount":source.map_or(0, |source| source.last_seen_players.len()),
                    }));
                    while self.disconnected_external_sources.len() > 128 {
                        self.disconnected_external_sources.pop_front();
                    }
                }
                self.update_player_connection_count();
                if let Some(source) = self.sources.get_mut(&id) {
                    let had_live_state = !source.players.is_empty()
                        || !source.entities.is_empty()
                        || !source.waypoints.is_empty()
                        || !source.tab_players.is_empty()
                        || !source.battle_chunks.is_empty();
                    source.players.clear();
                    source.entities.clear();
                    source.waypoints.clear();
                    source.tab_players.clear();
                    source.tab_received_at = None;
                    source.tab_timestamp = None;
                    source.battle_chunks.clear();
                    source.battle_projection = None;
                    if source.last_seen_players.is_empty() {
                        self.sources.remove(&id);
                    }
                    self.dirty |= had_live_state;
                }
                self.last_refresh_sent.remove(&id);
            }
            RelayEvent::Delivered { id, revision } => {
                if let Some(connection) = self.connections.get_mut(&id) {
                    connection.delivered_revision = connection.delivered_revision.max(revision);
                    if connection.queued_revision == revision {
                        connection.delivered_snapshot = connection.queued_snapshot.clone();
                    }
                }
            }
            RelayEvent::PlayerReport { id, report } => {
                self.apply_report(&id, *report);
                self.dirty = true;
            }
            RelayEvent::Resync { id } => {
                if let Some(connection) = self.connections.get_mut(&id) {
                    connection.force_full = true;
                }
                self.dirty = true;
            }
            RelayEvent::BattleChunkMeta { id, battle_chunks } => {
                self.send_battle_chunk_meta(&id, &battle_chunks);
            }
            RelayEvent::WebMapCommand { id, command } => {
                self.apply_web_map_command(&id, command);
            }
            RelayEvent::TabHistorySubscription { id, enabled } => {
                if let Some(connection) = self.connections.get_mut(&id) {
                    connection.tab_history_subscribed = enabled;
                }
            }
            RelayEvent::TabHistoryChanged { room, head } => {
                let player = encode_payload(
                    WireChannel::Player,
                    wire_envelope::Payload::TabHistoryDigest(
                        crate::proto::teamviewer::v1::TabHistoryDigest {
                            head: Some(head.clone()),
                        },
                    ),
                );
                let web_map = encode_payload(
                    WireChannel::WebMap,
                    wire_envelope::Payload::TabHistoryDigest(
                        crate::proto::teamviewer::v1::TabHistoryDigest { head: Some(head) },
                    ),
                );
                for connection in self.connections.values().filter(|connection| {
                    connection.room == room && connection.tab_history_subscribed
                }) {
                    let bytes = if connection.kind == ConnectionKind::WebMap {
                        web_map.clone()
                    } else {
                        player.clone()
                    };
                    let _ = connection.control.try_send(bytes);
                }
            }
            RelayEvent::Snapshot { room, reply } => {
                let _ = reply.send(self.snapshot_json(room.as_deref()));
            }
            RelayEvent::DeleteLastSeen { records, reply } => {
                let mut deleted = 0;
                for (room, source_id, player_uuid) in records {
                    let Some(source) = self.sources.get_mut(&source_id) else {
                        continue;
                    };
                    if source.room != room {
                        continue;
                    }
                    let before = source.last_seen_players.len();
                    source.last_seen_players.retain(|object_id, value| {
                        !object_id.eq_ignore_ascii_case(&player_uuid)
                            && !value.data.player_uuid.eq_ignore_ascii_case(&player_uuid)
                    });
                    deleted += before - source.last_seen_players.len();
                }
                self.dirty |= deleted > 0;
                let _ = reply.send(deleted);
            }
        }
    }

    fn update_player_connection_count(&self) {
        self.player_connection_count.store(
            self.connections
                .values()
                .filter(|connection| connection.kind == ConnectionKind::Player)
                .count(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    fn broadcast_report_rate_hints(&self, reason: &str) {
        let bytes = encode_payload(
            WireChannel::Player,
            wire_envelope::Payload::ReportRateHint(ReportRateHint {
                report_interval_ticks: self.config.report_interval_ticks(self.broadcast_hz),
                broadcast_hz: self.broadcast_hz,
                reason: Some(reason.to_owned()),
            }),
        );
        for connection in self
            .connections
            .values()
            .filter(|connection| connection.kind == ConnectionKind::Player)
        {
            let _ = connection.control.try_send(bytes.clone());
        }
    }

    fn apply_report(&mut self, id: &str, report: PlayerReportBundle) {
        let now = Instant::now();
        let battle_touched = report.battle_map_observation.is_some();
        let room = self
            .connections
            .get(id)
            .map(|connection| connection.room.clone())
            .unwrap_or_else(|| "default".to_owned());
        let source = self.sources.entry(id.to_owned()).or_default();
        source.room = room;
        if let Some(status) = &report.external_source_status {
            source.external_health = Some(status.health);
            source.external_failure_code = status.failure_code.clone();
            source.external_status_received_at = Some(unix_seconds());
            if status.health == crate::proto::teamviewer::v1::ExternalSourceHealth::Healthy as i32 {
                source.external_last_healthy_at = Some(unix_seconds());
            }
        }

        let touches_tab = report.players_replace.is_some()
            || report.players_patch.is_some()
            || report.entities_replace.is_some()
            || report.entities_patch.is_some()
            || report.waypoints_replace.is_some()
            || report.waypoints_patch.is_some()
            || report.battle_map_observation.is_some()
            || report.state_keepalive.is_some()
            || report.waypoints_delete.is_some()
            || report.waypoints_entity_death_cancel.is_some()
            || report.external_source_status.is_some();

        if let Some(replace) = report.players_replace {
            source.players = replace
                .players
                .into_iter()
                .map(|(id, player)| (id, Timed::new(normalize_player(player), now)))
                .collect();
        }
        if let Some(patch) = report.players_patch {
            for upsert in patch.upsert {
                if let Some(delta) = upsert.data {
                    let existing = source
                        .players
                        .entry(upsert.id)
                        .or_insert_with(|| Timed::new(PlayerData::default(), now));
                    clear_player_fields(&mut existing.data, &upsert.clear_fields);
                    apply_player_delta(&mut existing.data, delta);
                    existing.data = normalize_player(existing.data.clone());
                    existing.received_at = now;
                }
            }
            for id in patch.delete {
                source.players.remove(&id);
            }
        }
        if let Some(replace) = report.entities_replace {
            source.entities = replace
                .entities
                .into_iter()
                .map(|(id, entity)| (id, Timed::new(normalize_entity(entity), now)))
                .collect();
        }
        if let Some(patch) = report.entities_patch {
            for upsert in patch.upsert {
                if let Some(delta) = upsert.data {
                    let existing = source
                        .entities
                        .entry(upsert.id)
                        .or_insert_with(|| Timed::new(EntityData::default(), now));
                    clear_entity_fields(&mut existing.data, &upsert.clear_fields);
                    apply_entity_delta(&mut existing.data, delta);
                    normalize_entity_in_place(&mut existing.data);
                    existing.received_at = now;
                }
            }
            for id in patch.delete {
                source.entities.remove(&id);
            }
        }
        if let Some(replace) = report.waypoints_replace {
            source.waypoints = replace
                .waypoints
                .into_iter()
                .map(|(id, waypoint)| (id, Timed::new(normalize_waypoint(waypoint), now)))
                .collect();
        }
        if let Some(patch) = report.waypoints_patch {
            for upsert in patch.upsert {
                if let Some(delta) = upsert.data {
                    let existing = source
                        .waypoints
                        .entry(upsert.id)
                        .or_insert_with(|| Timed::new(WaypointData::default(), now));
                    clear_waypoint_fields(&mut existing.data, &upsert.clear_fields);
                    apply_waypoint_delta(&mut existing.data, delta);
                    normalize_waypoint_in_place(&mut existing.data);
                    existing.received_at = now;
                }
            }
            for id in patch.delete {
                source.waypoints.remove(&id);
            }
        }
        if let Some(replace) = report.last_seen_players_replace {
            source.last_seen_players = replace
                .players
                .into_iter()
                .map(|(id, data)| (id, Timed::new(data, now)))
                .collect();
        }
        if let Some(patch) = report.last_seen_players_patch {
            for upsert in patch.upsert {
                if let Some(data) = upsert.data {
                    source
                        .last_seen_players
                        .insert(upsert.id, Timed::new(data, now));
                }
            }
            for id in patch.delete {
                source.last_seen_players.remove(&id);
            }
        }
        if let Some(replace) = report.tab_players_replace {
            source.tab_players = replace.tab_players;
            source.tab_received_at = Some(now);
            source.tab_timestamp = Some(unix_seconds());
        }
        if let Some(patch) = report.tab_players_patch {
            let mut keyed: HashMap<String, _> = source
                .tab_players
                .drain(..)
                .filter_map(|entry| tab_key(&entry).map(|key| (key, entry)))
                .collect();
            for upsert in patch.upsert {
                if let Some(data) = upsert.data {
                    keyed.insert(upsert.key, data);
                }
            }
            for key in patch.delete {
                keyed.remove(&key);
            }
            source.tab_players = keyed.into_values().collect();
            source.tab_received_at = Some(now);
            source.tab_timestamp = Some(unix_seconds());
        }
        if let Some(observation) = report.battle_map_observation {
            apply_battle_observation(source, id, observation, now);
        }
        if let Some(keepalive) = report.state_keepalive {
            for object_id in keepalive.players {
                if let Some(value) = source.players.get_mut(&object_id) {
                    value.received_at = now;
                }
            }
            for object_id in keepalive.entities {
                if let Some(value) = source.entities.get_mut(&object_id) {
                    value.received_at = now;
                }
            }
            for chunk in keepalive.battle_chunks {
                if let Some(key) = battle_ref_key(&chunk)
                    && let Some(value) = source.battle_chunks.get_mut(&key)
                {
                    value.received_at = now;
                }
            }
        }
        if let Some(delete) = report.waypoints_delete {
            for waypoint_id in delete.waypoint_ids {
                source.waypoints.remove(&waypoint_id);
            }
        }
        if let Some(clear) = report.source_state_clear {
            let scopes: HashSet<_> = clear
                .scopes
                .iter()
                .map(|scope| scope.trim().to_ascii_lowercase())
                .collect();
            let defaults = scopes.is_empty();
            if defaults || scopes.contains("players") {
                source.players.clear();
            }
            if defaults || scopes.contains("entities") {
                source.entities.clear();
            }
            if defaults || scopes.contains("waypoints") {
                source.waypoints.clear();
            }
            if scopes.contains("last_seen_players") {
                source.last_seen_players.clear();
            }
            if defaults || scopes.contains("tab_players") || scopes.contains("tab") {
                source.tab_players.clear();
                source.tab_received_at = None;
                source.tab_timestamp = None;
            }
            if defaults || scopes.contains("battle_chunks") {
                source.battle_chunks.clear();
                source.battle_projection = None;
            }
        }
        if touches_tab && !source.tab_players.is_empty() {
            source.tab_received_at = Some(now);
            source.tab_timestamp = Some(unix_seconds());
        }
        let battle_cache_update = battle_touched.then(|| {
            (
                source.room.clone(),
                source.battle_chunks.clone(),
                source.battle_projection.clone(),
            )
        });

        if let Some((room, chunks, projection)) = battle_cache_update {
            let cache = self
                .sources
                .entry(format!("__battle_chunk_cache__:{room}"))
                .or_default();
            cache.room = room;
            cache.battle_chunks.extend(chunks);
            cache.battle_projection = projection;
        }

        let death_targets: HashSet<_> = report
            .waypoints_entity_death_cancel
            .into_iter()
            .flat_map(|cancel| cancel.target_entity_ids)
            .filter(|target| !target.trim().is_empty())
            .collect();
        if !death_targets.is_empty() {
            for source in self.sources.values_mut() {
                source.waypoints.retain(|_, waypoint| {
                    waypoint.data.target_type.as_deref() != Some("entity")
                        || waypoint
                            .data
                            .target_entity_id
                            .as_ref()
                            .is_none_or(|target| !death_targets.contains(target))
                });
            }
        }
    }

    fn apply_web_map_command(&mut self, id: &str, command: WebMapCommand) {
        let Some(connection) = self.connections.get(id) else {
            return;
        };
        if connection.kind != ConnectionKind::WebMap {
            return;
        }
        let room = connection.room.clone();
        let control = connection.control.clone();
        let Some(command) = command.command else {
            send_web_ack(
                &control,
                WebMapAck {
                    ok: false,
                    error: Some("unsupported_command".to_owned()),
                    ..Default::default()
                },
            );
            return;
        };

        let ack = match command {
            web_map_command::Command::ResyncRequest(_) => {
                if let Some(connection) = self.connections.get_mut(id) {
                    connection.force_full = true;
                }
                self.dirty = true;
                return;
            }
            web_map_command::Command::SetPlayerMark(command) => {
                let player_id = command.player_id.trim().to_owned();
                if player_id.is_empty() {
                    WebMapAck {
                        ok: false,
                        error: Some("invalid_player_id".to_owned()),
                        ..Default::default()
                    }
                } else {
                    let mark = PlayerMark {
                        team: command.team.trim().to_owned(),
                        color: command.color.filter(|value| !value.trim().is_empty()),
                        label: command.label.filter(|value| !value.trim().is_empty()),
                        source: command.source.filter(|value| !value.trim().is_empty()),
                    };
                    self.player_marks.insert(player_id.clone(), mark.clone());
                    self.dirty = true;
                    WebMapAck {
                        ok: true,
                        action: Some("command_player_mark_set".to_owned()),
                        detail: Some(web_map_ack::Detail::PlayerMark(WebMapPlayerMarkAckDetail {
                            player_id,
                            mark: Some(mark),
                        })),
                        ..Default::default()
                    }
                }
            }
            web_map_command::Command::ClearPlayerMark(command) => {
                let player_id = command.player_id.trim().to_owned();
                let removed = self.player_marks.remove(&player_id).is_some();
                self.dirty |= removed;
                WebMapAck {
                    ok: removed,
                    action: Some("command_player_mark_clear".to_owned()),
                    error: (!removed).then(|| "mark_not_found".to_owned()),
                    detail: Some(web_map_ack::Detail::PlayerMark(WebMapPlayerMarkAckDetail {
                        player_id,
                        mark: None,
                    })),
                    ..Default::default()
                }
            }
            web_map_command::Command::ClearAllPlayerMarks(_) => {
                let removed_count = i32::try_from(self.player_marks.len()).unwrap_or(i32::MAX);
                self.player_marks.clear();
                self.dirty |= removed_count > 0;
                WebMapAck {
                    ok: true,
                    action: Some("command_player_mark_clear_all".to_owned()),
                    detail: Some(web_map_ack::Detail::ClearAllPlayerMarks(
                        WebMapClearAllPlayerMarksAckDetail { removed_count },
                    )),
                    ..Default::default()
                }
            }
            web_map_command::Command::SetSameServerFilter(command) => {
                self.same_server_filter_enabled = command.enabled;
                self.dirty = true;
                WebMapAck {
                    ok: true,
                    action: Some("command_same_server_filter_set".to_owned()),
                    detail: Some(web_map_ack::Detail::SameServerFilter(
                        WebMapSameServerFilterAckDetail {
                            enabled: command.enabled,
                        },
                    )),
                    ..Default::default()
                }
            }
            web_map_command::Command::SetTacticalWaypoint(command) => {
                let Some(x) = command.x.filter(|value| value.is_finite()) else {
                    send_web_ack(&control, invalid_tactical_ack());
                    return;
                };
                let Some(z) = command.z.filter(|value| value.is_finite()) else {
                    send_web_ack(&control, invalid_tactical_ack());
                    return;
                };
                let waypoint_id = command
                    .waypoint_id
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| {
                        format!(
                            "web_map_tactical:{}:{}",
                            unix_millis(),
                            &Uuid::new_v4().simple().to_string()[..8]
                        )
                    });
                let permanent = command.permanent.unwrap_or(false);
                let waypoint = WaypointData {
                    x,
                    y: 64.0,
                    z,
                    dimension: command
                        .dimension
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or_else(|| "minecraft:overworld".to_owned()),
                    name: truncate_chars(command.label.as_deref().unwrap_or("战术标记").trim(), 64),
                    symbol: Some("T".to_owned()),
                    color: Some(parse_waypoint_color(command.color.as_deref())),
                    owner_name: Some("Web Map Tactical".to_owned()),
                    created_at: Some(unix_millis()),
                    ttl_seconds: (!permanent)
                        .then(|| command.ttl_seconds.unwrap_or(60).clamp(10, 86_400)),
                    waypoint_kind: Some("web_map_tactical".to_owned()),
                    replace_old_quick: Some(false),
                    target_type: Some("block".to_owned()),
                    room_code: Some(room.clone()),
                    permanent: Some(permanent),
                    tactical_type: Some(
                        command
                            .tactical_type
                            .filter(|value| !value.trim().is_empty())
                            .unwrap_or_else(|| "attack".to_owned()),
                    ),
                    source_type: Some("web_map_tactical".to_owned()),
                    deletable_by: Some("owner".to_owned()),
                    ..Default::default()
                };
                let source_id = format!("__web_map_tactical__:{room}");
                let source = self.sources.entry(source_id).or_default();
                source.room = room;
                source.waypoints.insert(
                    waypoint_id.clone(),
                    Timed::new(waypoint.clone(), Instant::now()),
                );
                self.dirty = true;
                WebMapAck {
                    ok: true,
                    action: Some("command_tactical_waypoint_set".to_owned()),
                    detail: Some(web_map_ack::Detail::TacticalWaypoint(
                        WebMapTacticalWaypointAckDetail {
                            waypoint_id,
                            waypoint: Some(waypoint),
                        },
                    )),
                    ..Default::default()
                }
            }
            web_map_command::Command::DeleteWaypoints(command) => {
                let tactical_source = format!("__web_map_tactical__:{room}");
                let mut removed = Vec::new();
                for waypoint_id in command.waypoint_ids {
                    let mut removed_one = false;
                    for (source_id, source) in self
                        .sources
                        .iter_mut()
                        .filter(|(_, source)| source.room == room)
                    {
                        let deletable = source
                            .waypoints
                            .get(&waypoint_id)
                            .and_then(|value| value.data.deletable_by.as_deref())
                            .unwrap_or("everyone");
                        if deletable == "everyone"
                            || (deletable == "owner" && source_id == &tactical_source)
                        {
                            removed_one |= source.waypoints.remove(&waypoint_id).is_some();
                        }
                    }
                    if removed_one {
                        removed.push(waypoint_id);
                    }
                }
                self.dirty |= !removed.is_empty();
                WebMapAck {
                    ok: !removed.is_empty(),
                    action: Some("waypoints_delete".to_owned()),
                    error: removed.is_empty().then(|| "waypoint_not_found".to_owned()),
                    detail: Some(web_map_ack::Detail::WaypointsDelete(
                        WebMapWaypointsDeleteAckDetail {
                            waypoint_ids: removed,
                        },
                    )),
                    ..Default::default()
                }
            }
        };
        send_web_ack(&control, ack);
    }

    fn build_room_snapshot(
        &mut self,
        room: &str,
        web_map: bool,
        allowed_sources: Option<&HashSet<String>>,
    ) -> SnapshotFull {
        let scoped_key = allowed_sources.map(|allowed| {
            let mut sources = allowed.iter().cloned().collect::<Vec<_>>();
            sources.sort();
            (room.to_owned(), sources)
        });
        let selections = if let Some(key) = &scoped_key {
            self.scoped_selections.get(key).cloned().unwrap_or_default()
        } else {
            let previous = self.rooms.get(room);
            ScopeSelections {
                players: previous
                    .map(|frame| frame.selected_players.clone())
                    .unwrap_or_default(),
                entities: previous
                    .map(|frame| frame.selected_entities.clone())
                    .unwrap_or_default(),
                waypoints: previous
                    .map(|frame| frame.selected_waypoints.clone())
                    .unwrap_or_default(),
                battle_chunks: previous
                    .map(|frame| frame.selected_battle_chunks.clone())
                    .unwrap_or_default(),
                last_seen_players: previous
                    .map(|frame| frame.selected_last_seen_players.clone())
                    .unwrap_or_default(),
            }
        };
        let mut selected_players = selections.players;
        let mut selected_entities = selections.entities;
        let mut selected_waypoints = selections.waypoints;
        let mut selected_battle_chunks = selections.battle_chunks;
        let mut selected_last_seen_players = selections.last_seen_players;

        let mut snapshot = SnapshotFull::default();
        let player_candidates =
            collect_candidates(&self.sources, room, allowed_sources, false, |source| {
                &source.players
            });
        let players = resolve_candidates(
            player_candidates,
            &mut selected_players,
            |object_id, source_id| {
                if object_id.eq_ignore_ascii_case(source_id) {
                    2
                } else if self
                    .connections
                    .get(source_id)
                    .is_some_and(|connection| connection.kind == ConnectionKind::ExternalSource)
                {
                    0
                } else {
                    1
                }
            },
        );
        for (player_id, (source_id, reported)) in players {
            let source_connection = self.connections.get(&source_id);
            let mut player = reported;
            player.position_source_id = Some(source_id.clone());
            player.position_source_kind = Some(match source_connection.map(|value| value.kind) {
                Some(ConnectionKind::ExternalSource) => {
                    PlayerPositionSourceKind::ExternalSource as i32
                }
                _ if player_id.eq_ignore_ascii_case(&source_id) => {
                    PlayerPositionSourceKind::SelfReport as i32
                }
                _ => PlayerPositionSourceKind::PlayerReport as i32,
            });
            player.position_source_display_name =
                source_connection.and_then(|value| value.display_name.clone());
            player.position_resolution =
                source_connection.and_then(|value| value.position_resolution);
            snapshot.players.insert(player_id, player);
        }
        snapshot.entities = resolve_candidates(
            collect_candidates(&self.sources, room, allowed_sources, false, |source| {
                &source.entities
            }),
            &mut selected_entities,
            |_, _| 0,
        )
        .into_iter()
        .map(|(id, (_, data))| (id, data))
        .collect();
        snapshot.waypoints = resolve_candidates(
            collect_candidates(&self.sources, room, allowed_sources, true, |source| {
                &source.waypoints
            }),
            &mut selected_waypoints,
            |_, _| 0,
        )
        .into_iter()
        .map(|(id, (_, data))| (id, data))
        .collect();
        snapshot.battle_chunks = resolve_candidates(
            collect_candidates(&self.sources, room, allowed_sources, true, |source| {
                &source.battle_chunks
            }),
            &mut selected_battle_chunks,
            |_, source_id| i32::from(!source_id.starts_with("__battle_chunk_cache__:")),
        )
        .into_values()
        .map(|(_, entry)| battle_chunk_without_meta(entry))
        .collect();
        snapshot.last_seen_players = resolve_candidates(
            collect_candidates(&self.sources, room, allowed_sources, false, |source| {
                &source.last_seen_players
            }),
            &mut selected_last_seen_players,
            |_, _| 0,
        )
        .into_iter()
        .map(|(id, (_, data))| (id, data))
        .collect();

        let mut online_ids = HashSet::new();
        for (id, player) in &snapshot.players {
            online_ids.insert(id.to_ascii_lowercase());
            if let Some(uuid) = &player.player_uuid {
                online_ids.insert(uuid.to_ascii_lowercase());
            }
        }
        snapshot.last_seen_players.retain(|id, data| {
            !online_ids.contains(&id.to_ascii_lowercase())
                && !online_ids.contains(&data.player_uuid.to_ascii_lowercase())
        });

        let grouping = self.same_server_grouping(room);
        let mut tab_reports = HashMap::new();
        for (source_id, source) in self.sources.iter().filter(|(source_id, source)| {
            source.room == room && grouping.active_sources.contains(*source_id)
        }) {
            if web_map && !source.tab_players.is_empty() {
                tab_reports.insert(
                    source_id.clone(),
                    TabPlayerReport {
                        timestamp: source.tab_timestamp,
                        players: source.tab_players.clone(),
                    },
                );
            }
        }
        snapshot.battle_chunks.sort_by_key(battle_entry_key);
        snapshot.player_marks = self.player_marks.clone();

        if let Some(key) = scoped_key {
            if self.scoped_selections.len() >= 128 && !self.scoped_selections.contains_key(&key) {
                self.scoped_selections.clear();
            }
            self.scoped_selections.insert(
                key,
                ScopeSelections {
                    players: selected_players,
                    entities: selected_entities,
                    waypoints: selected_waypoints,
                    battle_chunks: selected_battle_chunks,
                    last_seen_players: selected_last_seen_players,
                },
            );
        } else {
            let frame = self.rooms.entry(room.to_owned()).or_default();
            frame.selected_players = selected_players;
            frame.selected_entities = selected_entities;
            frame.selected_waypoints = selected_waypoints;
            frame.selected_battle_chunks = selected_battle_chunks;
            frame.selected_last_seen_players = selected_last_seen_players;
        }
        if web_map {
            let mut connections: Vec<_> = self
                .connections
                .iter()
                .filter(|(_, connection)| {
                    connection.room == room && connection.kind == ConnectionKind::Player
                })
                .map(|(id, _)| id.clone())
                .collect();
            connections.sort();
            snapshot.room_code = Some(room.to_owned());
            snapshot.connections_count = Some(i32::try_from(connections.len()).unwrap_or(i32::MAX));
            snapshot.connections = connections;
            snapshot.server_time = Some(unix_seconds());
            snapshot.tab_state = Some(WebMapTabState {
                enabled: self.same_server_filter_enabled,
                room_code: room.to_owned(),
                reports: tab_reports,
                groups: grouping.groups,
            });
        }
        snapshot
    }

    fn same_server_grouping(&self, room: &str) -> SameServerGrouping {
        let mut active_sources = self
            .connections
            .iter()
            .filter(|(_, connection)| {
                connection.room == room
                    && matches!(
                        connection.kind,
                        ConnectionKind::Player | ConnectionKind::ExternalSource
                    )
            })
            .map(|(source_id, _)| source_id.clone())
            .collect::<Vec<_>>();
        active_sources.sort();
        if active_sources.is_empty() {
            return SameServerGrouping::default();
        }

        let identity_sets = active_sources
            .iter()
            .map(|source_id| {
                let mut identities =
                    HashSet::from([format!("uuid:{}", normalize_identity(source_id))]);
                if let Some(source) = self.sources.get(source_id) {
                    for player in &source.tab_players {
                        if let Some(uuid) = player.uuid.as_deref().map(str::trim)
                            && !uuid.is_empty()
                        {
                            identities.insert(format!("uuid:{}", normalize_identity(uuid)));
                        }
                        if let Some(name) = player.name.as_deref().map(str::trim)
                            && !name.is_empty()
                        {
                            identities.insert(format!("name:{}", normalize_identity(name)));
                        }
                    }
                }
                identities
            })
            .collect::<Vec<_>>();

        let mut parents = (0..active_sources.len()).collect::<Vec<_>>();
        for left in 0..active_sources.len() {
            for right in (left + 1)..active_sources.len() {
                if !identity_sets[left].is_disjoint(&identity_sets[right]) {
                    union_groups(&mut parents, left, right);
                }
            }
        }

        let mut members_by_root: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        for (index, source_id) in active_sources.iter().enumerate() {
            let root = find_group(&mut parents, index);
            members_by_root
                .entry(root)
                .or_default()
                .push(source_id.clone());
        }
        let mut member_groups = members_by_root.into_values().collect::<Vec<_>>();
        for members in &mut member_groups {
            members.sort();
        }
        member_groups.sort_by(|left, right| left[0].cmp(&right[0]));

        let mut source_to_group = HashMap::new();
        let groups = member_groups
            .into_iter()
            .enumerate()
            .map(|(index, members)| {
                let group_id = format!("g{}", index + 1);
                for source_id in &members {
                    source_to_group.insert(source_id.clone(), group_id.clone());
                }
                SameServerGroup { group_id, members }
            })
            .collect();
        SameServerGrouping {
            active_sources: active_sources.into_iter().collect(),
            source_to_group,
            groups,
        }
    }

    fn allowed_sources_for_player(
        &self,
        player_id: &str,
        grouping: &SameServerGrouping,
    ) -> HashSet<String> {
        if !self.same_server_filter_enabled || !grouping.active_sources.contains(player_id) {
            return grouping.active_sources.clone();
        }
        let Some(group_id) = grouping.source_to_group.get(player_id) else {
            return grouping.active_sources.clone();
        };
        let allowed = grouping
            .source_to_group
            .iter()
            .filter(|(_, candidate_group)| *candidate_group == group_id)
            .map(|(source_id, _)| source_id.clone())
            .collect::<HashSet<_>>();
        if allowed.is_empty() {
            grouping.active_sources.clone()
        } else {
            allowed
        }
    }

    fn send_battle_chunk_meta(&self, id: &str, requested: &[BattleChunkRef]) {
        let Some(connection) = self.connections.get(id) else {
            return;
        };
        let requested: std::collections::HashSet<_> =
            requested.iter().filter_map(battle_ref_key).collect();
        let mut battle_chunks = Vec::new();
        for source in self
            .sources
            .values()
            .filter(|source| source.room == connection.room)
        {
            battle_chunks.extend(
                source
                    .battle_chunks
                    .iter()
                    .filter(|(key, _)| requested.contains(*key))
                    .map(|(_, entry)| BattleChunkMetaEntry {
                        r#ref: entry.data.r#ref.clone(),
                        data: entry.data.data.clone(),
                    }),
            );
        }
        battle_chunks.sort_by_key(|entry| entry.r#ref.as_ref().and_then(battle_ref_key));
        let channel = if connection.kind == ConnectionKind::WebMap {
            WireChannel::WebMap
        } else {
            WireChannel::Player
        };
        let bytes = encode_payload(
            channel,
            wire_envelope::Payload::BattleChunkMetaSnapshot(BattleChunkMetaSnapshot {
                battle_chunks,
            }),
        );
        let _ = connection.control.try_send(bytes);
    }

    fn cleanup_timeouts(&mut self, now: Instant) -> bool {
        let mut changed = false;
        for (source_id, source) in &mut self.sources {
            let before = source.players.len();
            source.players.retain(|_, value| {
                now.duration_since(value.received_at)
                    <= Duration::from_secs(self.config.player_timeout_sec)
            });
            changed |= before != source.players.len();

            let before = source.entities.len();
            source.entities.retain(|_, value| {
                now.duration_since(value.received_at)
                    <= Duration::from_secs(self.config.entity_timeout_sec)
            });
            changed |= before != source.entities.len();

            let before = source.waypoints.len();
            source.waypoints.retain(|_, value| {
                let timeout = if value.data.permanent.unwrap_or(false) {
                    315_360_000
                } else {
                    value
                        .data
                        .ttl_seconds
                        .map_or(self.config.waypoint_timeout_sec, |ttl| {
                            u64::try_from(ttl.clamp(5, 86_400))
                                .unwrap_or(self.config.waypoint_timeout_sec)
                        })
                };
                now.duration_since(value.received_at) <= Duration::from_secs(timeout)
            });
            changed |= before != source.waypoints.len();

            let before = source.battle_chunks.len();
            let battle_timeout = if source_id.starts_with("__battle_chunk_cache__:") {
                self.config.battle_chunk_cache_retention_sec
            } else {
                self.config.battle_chunk_timeout_sec
            };
            source.battle_chunks.retain(|_, value| {
                now.duration_since(value.received_at) <= Duration::from_secs(battle_timeout)
            });
            changed |= before != source.battle_chunks.len();

            if source.tab_received_at.is_some_and(|received_at| {
                now.duration_since(received_at)
                    > Duration::from_secs(self.config.tab_report_timeout_sec)
            }) {
                changed |= !source.tab_players.is_empty();
                source.tab_players.clear();
                source.tab_received_at = None;
                source.tab_timestamp = None;
            }
        }
        changed
    }

    fn send_preexpiry_refresh_requests(&mut self, now: Instant) {
        let cooldown = Duration::from_secs_f64(self.config.refresh_cooldown_sec);
        let lead = Duration::from_secs_f64(self.config.refresh_lead_sec);
        let max_items = self.config.refresh_max_items;
        let mut requests: HashMap<String, RefreshRequest> = HashMap::new();

        for (source_id, source) in &self.sources {
            if !self.connections.contains_key(source_id)
                || self
                    .last_refresh_sent
                    .get(source_id)
                    .is_some_and(|last| now.duration_since(*last) < cooldown)
            {
                continue;
            }
            let mut request = RefreshRequest {
                reason: "pre_expiry".to_owned(),
                server_time: unix_seconds(),
                ..Default::default()
            };
            for (id, value) in &source.players {
                let age = now.duration_since(value.received_at);
                let timeout = Duration::from_secs(self.config.player_timeout_sec);
                if age < timeout && timeout.saturating_sub(age) <= lead {
                    request.players.push(id.clone());
                    if request.players.len() >= max_items {
                        break;
                    }
                }
            }
            for (id, value) in &source.entities {
                let age = now.duration_since(value.received_at);
                let timeout = Duration::from_secs(self.config.entity_timeout_sec);
                if age < timeout && timeout.saturating_sub(age) <= lead {
                    request.entities.push(id.clone());
                    if request.entities.len() >= max_items {
                        break;
                    }
                }
            }
            for value in source.battle_chunks.values() {
                let age = now.duration_since(value.received_at);
                let timeout = Duration::from_secs(self.config.battle_chunk_timeout_sec);
                if age < timeout
                    && timeout.saturating_sub(age) <= lead
                    && let Some(reference) = &value.data.r#ref
                {
                    request.battle_chunks.push(reference.clone());
                    if request.battle_chunks.len() >= max_items {
                        break;
                    }
                }
            }
            if !request.players.is_empty()
                || !request.entities.is_empty()
                || !request.battle_chunks.is_empty()
            {
                requests.insert(source_id.clone(), request);
            }
        }

        for (source_id, request) in requests {
            let Some(connection) = self.connections.get(&source_id) else {
                continue;
            };
            let bytes = encode_payload(
                WireChannel::Player,
                wire_envelope::Payload::RefreshRequest(request),
            );
            if connection.control.try_send(bytes).is_ok() {
                self.last_refresh_sent.insert(source_id, now);
            }
        }
    }

    fn send_due_digests(&mut self, now: Instant) {
        let interval = Duration::from_secs(self.config.digest_interval_sec);
        let due = self
            .connections
            .iter()
            .filter(|(_, connection)| {
                connection.kind == ConnectionKind::Player
                    && connection
                        .last_digest_sent
                        .is_none_or(|last| now.duration_since(last) >= interval)
            })
            .map(|(id, connection)| {
                (
                    id.clone(),
                    connection.protocol.clone(),
                    connection.control.clone(),
                    connection.delivered_snapshot.clone(),
                )
            })
            .collect::<Vec<_>>();
        for (id, protocol, control, delivered_snapshot) in due {
            let Some(snapshot) = delivered_snapshot else {
                continue;
            };
            let include_last_seen = protocol_at_least(&protocol, "0.6.4");
            let include_source_metadata = protocol_at_least(&protocol, "0.6.5");
            let bytes = encode_payload(
                WireChannel::Player,
                wire_envelope::Payload::Digest(snapshot_digest(
                    &snapshot,
                    include_last_seen,
                    include_source_metadata,
                )),
            );
            if control.try_send(bytes).is_ok()
                && let Some(connection) = self.connections.get_mut(&id)
            {
                connection.last_digest_sent = Some(now);
            }
        }
    }

    fn broadcast(&mut self) {
        let mut rooms: Vec<String> = self
            .connections
            .values()
            .map(|connection| connection.room.clone())
            .collect();
        rooms.sort();
        rooms.dedup();

        for room in rooms {
            let player_snapshot = Arc::new(self.build_room_snapshot(&room, false, None));
            let web_snapshot = Arc::new(self.build_room_snapshot(&room, true, None));
            let previous_revision = self.rooms.get(&room).map_or(0, |frame| frame.revision);
            let revision = previous_revision + 1;
            let grouping = self.same_server_grouping(&room);
            let recipients = self
                .connections
                .iter()
                .filter(|(_, connection)| {
                    connection.room == room && connection.kind != ConnectionKind::ExternalSource
                })
                .map(|(id, connection)| {
                    (
                        id.clone(),
                        connection.kind,
                        connection.protocol.clone(),
                        connection.force_full,
                        connection.delivered_snapshot.clone(),
                    )
                })
                .collect::<Vec<_>>();
            let mut scoped_snapshots: HashMap<Vec<String>, Arc<SnapshotFull>> = HashMap::new();

            for (id, kind, protocol, force_full, delivered_snapshot) in recipients {
                let mut target = if kind == ConnectionKind::WebMap {
                    web_snapshot.clone()
                } else {
                    let allowed = self.allowed_sources_for_player(&id, &grouping);
                    if allowed == grouping.active_sources {
                        player_snapshot.clone()
                    } else {
                        let mut scope_key = allowed.iter().cloned().collect::<Vec<_>>();
                        scope_key.sort();
                        if let Some(snapshot) = scoped_snapshots.get(&scope_key) {
                            snapshot.clone()
                        } else {
                            let snapshot =
                                Arc::new(self.build_room_snapshot(&room, false, Some(&allowed)));
                            scoped_snapshots.insert(scope_key, snapshot.clone());
                            snapshot
                        }
                    }
                };
                if !protocol_at_least(&protocol, "0.6.4") {
                    let mut legacy = (*target).clone();
                    legacy.last_seen_players.clear();
                    target = Arc::new(legacy);
                }

                let channel = if kind == ConnectionKind::WebMap {
                    WireChannel::WebMap
                } else {
                    WireChannel::Player
                };
                let bytes = if force_full || delivered_snapshot.is_none() {
                    encode_payload(
                        channel,
                        wire_envelope::Payload::SnapshotFull((*target).clone()),
                    )
                } else {
                    let patch =
                        build_patch(delivered_snapshot.as_deref().expect("checked"), &target);
                    let Some(patch) = patch else {
                        continue;
                    };
                    if !protocol_at_least(&protocol, "0.6.5") && patch_requires_clear_fields(&patch)
                    {
                        encode_payload(
                            channel,
                            wire_envelope::Payload::SnapshotFull((*target).clone()),
                        )
                    } else {
                        encode_payload(channel, wire_envelope::Payload::Patch(patch))
                    }
                };

                if let Some(connection) = self.connections.get_mut(&id) {
                    connection
                        .state
                        .send_replace(Some(StateFrame { revision, bytes }));
                    connection.queued_revision = revision;
                    connection.queued_snapshot = Some(target);
                    connection.force_full = false;
                }
            }
            let frame = self.rooms.entry(room).or_default();
            frame.revision = revision;
            frame.snapshot = (*player_snapshot).clone();
            frame.web_snapshot = (*web_snapshot).clone();
        }
    }

    fn snapshot_json(&mut self, requested_room: Option<&str>) -> Value {
        let room = requested_room.unwrap_or("default");
        let snapshot = self.build_room_snapshot(room, true, None);
        let connections: Vec<_> = self.connections.keys().cloned().collect();
        let mut room_index: BTreeMap<String, Value> = BTreeMap::new();
        for connection in self.connections.values() {
            let room_entry = room_index
                .entry(connection.room.clone())
                .or_insert_with(|| {
                    json!({
                        "roomCode": connection.room,
                        "playerConnections": 0,
                        "webMapConnections": 0,
                        "externalSourceConnections": 0,
                        "playerIds": [],
                        "webMapIds": [],
                        "externalSourceIds": [],
                    })
                });
            let count_key = match connection.kind {
                ConnectionKind::Player => "playerConnections",
                ConnectionKind::ExternalSource => "externalSourceConnections",
                ConnectionKind::WebMap => "webMapConnections",
            };
            room_entry[count_key] = json!(room_entry[count_key].as_u64().unwrap_or(0) + 1);
        }
        for (connection_id, connection) in &self.connections {
            let ids_key = match connection.kind {
                ConnectionKind::Player => "playerIds",
                ConnectionKind::ExternalSource => "externalSourceIds",
                ConnectionKind::WebMap => "webMapIds",
            };
            if let Some(ids) = room_index
                .get_mut(&connection.room)
                .and_then(|room| room[ids_key].as_array_mut())
            {
                ids.push(json!(connection_id));
            }
        }
        let rooms = room_index.into_values().collect::<Vec<_>>();
        let mut connection_details = self
            .connections
            .iter()
            .map(|(connection_id, connection)| {
                let source = self.sources.get(connection_id);
                let health = source
                    .and_then(|source| source.external_health)
                    .and_then(|health| {
                        crate::proto::teamviewer::v1::ExternalSourceHealth::try_from(health).ok()
                    })
                    .map(|health| health.as_str_name());
                let last_seen_count = source.map_or(0, |source| source.last_seen_players.len());
                json!({
                    "channel": match connection.kind {
                        ConnectionKind::Player => "player",
                        ConnectionKind::ExternalSource => "external_source",
                        ConnectionKind::WebMap => "web_map",
                    },
                    "actorId": connection_id,
                    "displayName": connection.display_name.as_deref().unwrap_or(connection_id),
                    "roomCode": connection.room,
                    "protocolVersion": connection.protocol,
                    "programVersion": connection.program_version,
                    "remoteAddr": connection.remote_addr,
                    "connectedAt": connection.connected_at,
                    "connected": true,
                    "health": health,
                    "failureCode": source.and_then(|source| source.external_failure_code.clone()),
                    "statusReceivedAt": source.and_then(|source| source.external_status_received_at),
                    "lastHealthyAt": source.and_then(|source| source.external_last_healthy_at),
                    "lastSeenPlayerCount": last_seen_count,
                })
            })
            .collect::<Vec<_>>();
        connection_details.extend(self.disconnected_external_sources.iter().cloned());
        let selections = self.rooms.get(room).map(|frame| ScopeSelections {
            players: frame.selected_players.clone(),
            entities: frame.selected_entities.clone(),
            waypoints: frame.selected_waypoints.clone(),
            battle_chunks: frame.selected_battle_chunks.clone(),
            last_seen_players: frame.selected_last_seen_players.clone(),
        });
        let selected_source = |scope: &HashMap<String, String>, id: &str| {
            scope
                .get(id)
                .cloned()
                .map(Value::String)
                .unwrap_or(Value::Null)
        };
        let empty_selections = ScopeSelections::default();
        let selections = selections.as_ref().unwrap_or(&empty_selections);
        let runtime_players = snapshot
            .players
            .iter()
            .map(|(id, data)| {
                json!({
                    "id":id,
                    "sourceId":selected_source(&selections.players, id),
                    "reportedAtUtcMs":Value::Null,
                    "data":player_json(data),
                })
            })
            .collect::<Vec<_>>();
        let runtime_entities = snapshot
            .entities
            .iter()
            .map(|(id, data)| {
                json!({
                    "id":id,
                    "sourceId":selected_source(&selections.entities, id),
                    "reportedAtUtcMs":Value::Null,
                    "data":entity_json(data),
                })
            })
            .collect::<Vec<_>>();
        let runtime_waypoints = snapshot
            .waypoints
            .iter()
            .map(|(id, data)| {
                json!({
                    "id":id,
                    "sourceId":selected_source(&selections.waypoints, id),
                    "reportedAtUtcMs":Value::Null,
                    "data":waypoint_json(data),
                })
            })
            .collect::<Vec<_>>();
        let runtime_battle_chunks = snapshot
            .battle_chunks
            .iter()
            .filter_map(|entry| {
                let id = battle_entry_key(entry)?;
                let data = entry.data.as_ref()?;
                Some(json!({
                    "id":id,
                    "sourceId":selected_source(&selections.battle_chunks, &id),
                    "reportedAtUtcMs":Value::Null,
                    "data":battle_value_json(data),
                }))
            })
            .collect::<Vec<_>>();
        let runtime_tab_reports = self
            .sources
            .iter()
            .filter(|(source_id, source)| {
                source.room == room
                    && self.connections.get(*source_id).is_some_and(|connection| {
                        matches!(
                            connection.kind,
                            ConnectionKind::Player | ConnectionKind::ExternalSource
                        )
                    })
            })
            .map(|(source_id, source)| json!({
                "id":source_id,
                "sourceId":source_id,
                "reportedAtUtcMs":source.tab_timestamp.map(|value| (value * 1000.0) as i64),
                "data":{
                    "playerCount":source.tab_players.len(),
                    "players":source.tab_players.iter().map(tab_player_json).collect::<Vec<_>>(),
                },
            }))
            .collect::<Vec<_>>();
        let runtime_player_marks = self
            .player_marks
            .iter()
            .map(|(id, mark)| {
                json!({
                    "id":id,
                    "sourceId":mark.source,
                    "reportedAtUtcMs":Value::Null,
                    "data":player_mark_json(mark),
                })
            })
            .collect::<Vec<_>>();
        let last_seen_history = self
            .sources
            .iter()
            .flat_map(|(source_id, source)| {
                source.last_seen_players.iter().map(move |(id, value)| {
                    let data = &value.data;
                    json!({
                        "roomCode":source.room,
                        "sourceId":source_id,
                        "playerUuid":if data.player_uuid.is_empty() { id } else { &data.player_uuid },
                        "playerName":data.player_name,
                        "x":data.x,"y":data.y,"z":data.z,"dimension":data.dimension,
                        "lastSeenAtUtcMs":data.last_seen_at_utc_ms,
                        "positionObservedAtUtcMs":data.position_observed_at_utc_ms,
                        "offlineDetectedAtUtcMs":data.offline_detected_at_utc_ms,
                    })
                })
            })
            .collect::<Vec<_>>();
        let mut runtime_rooms = HashSet::from(["default".to_owned()]);
        runtime_rooms.extend(self.connections.values().map(|value| value.room.clone()));
        runtime_rooms.extend(self.sources.values().map(|value| value.room.clone()));
        let mut runtime_rooms = runtime_rooms.into_iter().collect::<Vec<_>>();
        runtime_rooms.sort();
        let player_connections = self
            .connections
            .values()
            .filter(|connection| connection.kind == ConnectionKind::Player)
            .count();
        let web_map_connections = self
            .connections
            .values()
            .filter(|connection| connection.kind == ConnectionKind::WebMap)
            .count();
        let external_source_connections = self
            .connections
            .values()
            .filter(|connection| connection.kind == ConnectionKind::ExternalSource)
            .count();
        let active_rooms = self
            .connections
            .values()
            .map(|connection| &connection.room)
            .collect::<std::collections::HashSet<_>>()
            .len();
        json!({
            "status": "ok",
            "playerConnections": player_connections,
            "webMapConnections": web_map_connections,
            "externalSourceConnections": external_source_connections,
            "activeRooms": active_rooms,
            "rooms": rooms,
            "connectionDetails": connection_details,
            "runtimeRooms": runtime_rooms,
            "runtimeState": {
                "tab-reports": runtime_tab_reports,
                "players": runtime_players,
                "entities": runtime_entities,
                "waypoints": runtime_waypoints,
                "battle-chunks": runtime_battle_chunks,
                "player-marks": runtime_player_marks,
            },
            "lastSeenHistory": last_seen_history,
            "selectedRoomCode": room,
            "connections": connections,
            "connections_count": self.connections.len(),
            "roomView": {
                "roomCode": room,
                "connections": snapshot.connections,
                "connections_count": snapshot.connections_count,
                "players": player_json_map(&snapshot.players),
                "entities_count": snapshot.entities.len(),
                "waypoints_count": snapshot.waypoints.len(),
                "battleChunks_count": snapshot.battle_chunks.len(),
                "lastSeenPlayers_count": snapshot.last_seen_players.len(),
            },
            "broadcastHz": self.broadcast_hz,
        })
    }
}

fn collect_candidates<'a, T>(
    sources: &'a HashMap<String, SourceState>,
    room: &str,
    allowed_sources: Option<&HashSet<String>>,
    allow_tactical: bool,
    scope: impl Fn(&'a SourceState) -> &'a HashMap<String, Timed<T>>,
) -> HashMap<String, Vec<(&'a str, &'a Timed<T>)>> {
    let mut candidates: HashMap<String, Vec<(&str, &Timed<T>)>> = HashMap::new();
    for (source_id, source) in sources.iter().filter(|(source_id, source)| {
        source.room == room
            && allowed_sources.is_none_or(|allowed| {
                allowed.contains(*source_id)
                    || (allow_tactical
                        && (source_id.starts_with("__web_map_tactical__:")
                            || source_id.starts_with("__battle_chunk_cache__:")))
            })
    }) {
        for (object_id, value) in scope(source) {
            candidates
                .entry(object_id.clone())
                .or_default()
                .push((source_id.as_str(), value));
        }
    }
    candidates
}

fn normalize_identity(value: &str) -> String {
    value.trim().nfkc().flat_map(char::to_lowercase).collect()
}

fn find_group(parents: &mut [usize], index: usize) -> usize {
    if parents[index] != index {
        parents[index] = find_group(parents, parents[index]);
    }
    parents[index]
}

fn union_groups(parents: &mut [usize], left: usize, right: usize) {
    let left_root = find_group(parents, left);
    let right_root = find_group(parents, right);
    if left_root == right_root {
        return;
    }
    if left_root < right_root {
        parents[right_root] = left_root;
    } else {
        parents[left_root] = right_root;
    }
}

fn resolve_candidates<T: Clone>(
    candidates: HashMap<String, Vec<(&str, &Timed<T>)>>,
    selected_sources: &mut HashMap<String, String>,
    priority: impl Fn(&str, &str) -> i32,
) -> HashMap<String, (String, T)> {
    const SWITCH_THRESHOLD: Duration = Duration::from_millis(350);
    let mut resolved = HashMap::new();
    let mut next_selected = HashMap::new();

    for (object_id, bucket) in candidates {
        let Some((mut chosen_source, mut chosen_value)) = bucket.iter().copied().max_by(
            |(left_source, left_value), (right_source, right_value)| {
                priority(&object_id, left_source)
                    .cmp(&priority(&object_id, right_source))
                    .then_with(|| left_value.received_at.cmp(&right_value.received_at))
                    .then_with(|| right_source.cmp(left_source))
            },
        ) else {
            continue;
        };

        if let Some(previous_source) = selected_sources.get(&object_id)
            && let Some((source_id, previous_value)) = bucket
                .iter()
                .copied()
                .find(|(source_id, _)| *source_id == previous_source)
            && priority(&object_id, source_id) == priority(&object_id, chosen_source)
            && chosen_value
                .received_at
                .saturating_duration_since(previous_value.received_at)
                <= SWITCH_THRESHOLD
        {
            chosen_source = source_id;
            chosen_value = previous_value;
        }

        next_selected.insert(object_id.clone(), chosen_source.to_owned());
        resolved.insert(
            object_id,
            (chosen_source.to_owned(), chosen_value.data.clone()),
        );
    }
    *selected_sources = next_selected;
    resolved
}

fn protocol_at_least(current: &str, minimum: &str) -> bool {
    fn parse(value: &str) -> [u32; 3] {
        let mut parts = value.split('.').filter_map(|part| part.parse().ok());
        [
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
        ]
    }
    parse(current) >= parse(minimum)
}

fn send_web_ack(control: &mpsc::Sender<Arc<[u8]>>, ack: WebMapAck) {
    let _ = control.try_send(encode_payload(
        WireChannel::WebMap,
        wire_envelope::Payload::WebMapAck(ack),
    ));
}

fn invalid_tactical_ack() -> WebMapAck {
    WebMapAck {
        ok: false,
        error: Some("invalid_tactical_waypoint_payload".to_owned()),
        ..Default::default()
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn parse_waypoint_color(value: Option<&str>) -> i32 {
    let normalized = value
        .unwrap_or_default()
        .trim()
        .trim_start_matches('#')
        .trim_start_matches("0x");
    i32::from_str_radix(normalized, 16).unwrap_or(0xEF4444)
}

pub fn encode_payload(channel: WireChannel, payload: wire_envelope::Payload) -> Arc<[u8]> {
    WireEnvelope {
        channel: channel as i32,
        payload: Some(payload),
    }
    .encode_to_vec()
    .into()
}

fn build_patch(old: &SnapshotFull, new: &SnapshotFull) -> Option<Patch> {
    let players = map_patch(&old.players, &new.players, player_upsert)
        .map(|(upsert, delete)| PlayerPatchScope { upsert, delete });
    let entities = map_patch(&old.entities, &new.entities, entity_upsert)
        .map(|(upsert, delete)| EntityPatchScope { upsert, delete });
    let waypoints = map_patch(&old.waypoints, &new.waypoints, waypoint_upsert)
        .map(|(upsert, delete)| WaypointPatchScope { upsert, delete });
    let last_seen_players = map_patch(
        &old.last_seen_players,
        &new.last_seen_players,
        last_seen_upsert,
    )
    .map(|(upsert, delete)| LastSeenPlayerPatchScope { upsert, delete });
    let player_marks = map_patch(&old.player_marks, &new.player_marks, player_mark_upsert)
        .map(|(upsert, delete)| PlayerMarkPatchScope { upsert, delete });
    let tab_state_patch = build_tab_state_patch(old.tab_state.as_ref(), new.tab_state.as_ref());
    let connections_changed = old.connections != new.connections;
    let connections = connections_changed.then(|| StringList {
        values: new.connections.clone(),
    });
    let connections_count = connections_changed.then_some(new.connections_count.unwrap_or(0));
    let old_battle: HashMap<_, _> = old
        .battle_chunks
        .iter()
        .filter_map(|entry| battle_entry_key(entry).map(|key| (key, entry.clone())))
        .collect();
    let new_battle: HashMap<_, _> = new
        .battle_chunks
        .iter()
        .filter_map(|entry| battle_entry_key(entry).map(|key| (key, entry.clone())))
        .collect();
    let battle_chunks =
        map_patch(&old_battle, &new_battle, battle_upsert).map(|(upsert, delete)| {
            BattleChunkPatchScope {
                upsert,
                delete: delete
                    .into_iter()
                    .filter_map(|key| parse_battle_key(&key))
                    .collect(),
            }
        });
    if players.is_none()
        && entities.is_none()
        && waypoints.is_none()
        && battle_chunks.is_none()
        && last_seen_players.is_none()
        && player_marks.is_none()
        && tab_state_patch.is_none()
        && !connections_changed
    {
        return None;
    }
    Some(Patch {
        players,
        entities,
        waypoints,
        battle_chunks,
        last_seen_players,
        player_marks,
        tab_state_patch,
        connections,
        connections_count,
        server_time: new.server_time,
    })
}

fn patch_requires_clear_fields(patch: &Patch) -> bool {
    patch.players.as_ref().is_some_and(|scope| {
        scope
            .upsert
            .iter()
            .any(|value| !value.clear_fields.is_empty())
    }) || patch.entities.as_ref().is_some_and(|scope| {
        scope
            .upsert
            .iter()
            .any(|value| !value.clear_fields.is_empty())
    }) || patch.waypoints.as_ref().is_some_and(|scope| {
        scope
            .upsert
            .iter()
            .any(|value| !value.clear_fields.is_empty())
    }) || patch.battle_chunks.as_ref().is_some_and(|scope| {
        scope
            .upsert
            .iter()
            .any(|value| !value.clear_fields.is_empty())
    }) || patch.last_seen_players.as_ref().is_some_and(|scope| {
        scope
            .upsert
            .iter()
            .any(|value| !value.clear_fields.is_empty())
    }) || patch.player_marks.as_ref().is_some_and(|scope| {
        scope
            .upsert
            .iter()
            .any(|value| !value.clear_fields.is_empty())
    })
}

fn build_tab_state_patch(
    old: Option<&WebMapTabState>,
    new: Option<&WebMapTabState>,
) -> Option<TabStatePatch> {
    let old = old.cloned().unwrap_or_default();
    let new = new.cloned().unwrap_or_default();
    let upsert_reports = new
        .reports
        .iter()
        .filter(|(source_id, report)| old.reports.get(*source_id) != Some(*report))
        .map(|(source_id, report)| (source_id.clone(), report.clone()))
        .collect::<HashMap<_, _>>();
    let delete_reports = old
        .reports
        .keys()
        .filter(|source_id| !new.reports.contains_key(*source_id))
        .cloned()
        .collect::<Vec<_>>();
    let enabled = (old.enabled != new.enabled).then_some(new.enabled);
    let room_code = (old.room_code != new.room_code).then_some(new.room_code);
    let groups = (old.groups != new.groups).then_some(SameServerGroupList { values: new.groups });
    (enabled.is_some()
        || room_code.is_some()
        || groups.is_some()
        || !upsert_reports.is_empty()
        || !delete_reports.is_empty())
    .then_some(TabStatePatch {
        enabled,
        room_code,
        groups,
        upsert_reports,
        delete_reports,
    })
}

fn map_patch<T, U, F>(
    old: &HashMap<String, T>,
    new: &HashMap<String, T>,
    make_upsert: F,
) -> Option<(Vec<U>, Vec<String>)>
where
    T: PartialEq,
    F: Fn(&str, &T, Option<&T>) -> U,
{
    let upsert: Vec<_> = new
        .iter()
        .filter(|(id, value)| old.get(*id) != Some(*value))
        .map(|(id, value)| make_upsert(id, value, old.get(id)))
        .collect();
    let delete: Vec<_> = old
        .keys()
        .filter(|id| !new.contains_key(*id))
        .cloned()
        .collect();
    (!upsert.is_empty() || !delete.is_empty()).then_some((upsert, delete))
}

fn player_upsert(id: &str, value: &PlayerData, old: Option<&PlayerData>) -> PlayerUpsert {
    PlayerUpsert {
        id: id.to_owned(),
        data: Some(player_delta(value)),
        clear_fields: old.map_or_else(Vec::new, |old| player_cleared_fields(old, value)),
    }
}

fn player_mark_upsert(id: &str, value: &PlayerMark, old: Option<&PlayerMark>) -> PlayerMarkUpsert {
    PlayerMarkUpsert {
        id: id.to_owned(),
        data: Some(value.clone()),
        clear_fields: old.map_or_else(Vec::new, |old| {
            [
                ("color", old.color.is_some() && value.color.is_none()),
                ("label", old.label.is_some() && value.label.is_none()),
                ("source", old.source.is_some() && value.source.is_none()),
            ]
            .into_iter()
            .filter(|(_, cleared)| *cleared)
            .map(|(field, _)| field.to_owned())
            .collect()
        }),
    }
}

fn battle_upsert(
    _id: &str,
    value: &BattleChunkEntry,
    old: Option<&BattleChunkEntry>,
) -> BattleChunkUpsert {
    BattleChunkUpsert {
        r#ref: value.r#ref.clone(),
        data: value.data.clone(),
        clear_fields: old
            .and_then(|old| old.data.as_ref())
            .zip(value.data.as_ref())
            .map_or_else(Vec::new, |(old, new)| battle_cleared_fields(old, new)),
    }
}

fn entity_upsert(id: &str, value: &EntityData, old: Option<&EntityData>) -> EntityUpsert {
    EntityUpsert {
        id: id.to_owned(),
        data: Some(entity_delta(value)),
        clear_fields: old.map_or_else(Vec::new, |old| entity_cleared_fields(old, value)),
    }
}

fn waypoint_upsert(id: &str, value: &WaypointData, old: Option<&WaypointData>) -> WaypointUpsert {
    WaypointUpsert {
        id: id.to_owned(),
        data: Some(waypoint_delta(value)),
        clear_fields: old.map_or_else(Vec::new, |old| waypoint_cleared_fields(old, value)),
    }
}

fn last_seen_upsert(
    id: &str,
    value: &LastSeenPlayerData,
    _old: Option<&LastSeenPlayerData>,
) -> LastSeenPlayerUpsert {
    LastSeenPlayerUpsert {
        id: id.to_owned(),
        data: Some(value.clone()),
        clear_fields: Vec::new(),
    }
}

fn player_delta(value: &PlayerData) -> PlayerDelta {
    PlayerDelta {
        x: Some(value.x),
        y: Some(value.y),
        z: Some(value.z),
        vx: value.vx,
        vy: value.vy,
        vz: value.vz,
        dimension: Some(value.dimension.clone()),
        player_name: value.player_name.clone(),
        player_uuid: value.player_uuid.clone(),
        health: value.health,
        max_health: value.max_health,
        armor: value.armor,
        is_riding: value.is_riding,
        width: value.width,
        height: value.height,
        position_source_id: value.position_source_id.clone(),
        position_source_kind: value.position_source_kind,
        position_source_display_name: value.position_source_display_name.clone(),
        position_resolution: value.position_resolution,
    }
}

fn normalize_player(mut value: PlayerData) -> PlayerData {
    value.vx.get_or_insert(0.0);
    value.vy.get_or_insert(0.0);
    value.vz.get_or_insert(0.0);
    value.health.get_or_insert(0.0);
    value.max_health.get_or_insert(20.0);
    value.armor.get_or_insert(0.0);
    value.is_riding.get_or_insert(false);
    value.width.get_or_insert(0.6);
    value.height.get_or_insert(1.8);
    value
}

fn normalize_entity(mut value: EntityData) -> EntityData {
    normalize_entity_in_place(&mut value);
    value
}

fn normalize_entity_in_place(value: &mut EntityData) {
    value.vx.get_or_insert(0.0);
    value.vy.get_or_insert(0.0);
    value.vz.get_or_insert(0.0);
    value.width.get_or_insert(0.6);
    value.height.get_or_insert(1.8);
}

fn normalize_waypoint(mut value: WaypointData) -> WaypointData {
    normalize_waypoint_in_place(&mut value);
    value
}

fn normalize_waypoint_in_place(value: &mut WaypointData) {
    value.symbol.get_or_insert_with(|| "W".to_owned());
    value
        .deletable_by
        .get_or_insert_with(|| "everyone".to_owned());
}

fn entity_delta(value: &EntityData) -> EntityDelta {
    EntityDelta {
        x: Some(value.x),
        y: Some(value.y),
        z: Some(value.z),
        vx: value.vx,
        vy: value.vy,
        vz: value.vz,
        dimension: Some(value.dimension.clone()),
        entity_type: value.entity_type.clone(),
        entity_name: value.entity_name.clone(),
        width: value.width,
        height: value.height,
    }
}

fn waypoint_delta(value: &WaypointData) -> WaypointDelta {
    WaypointDelta {
        x: Some(value.x),
        y: Some(value.y),
        z: Some(value.z),
        dimension: Some(value.dimension.clone()),
        name: Some(value.name.clone()),
        symbol: value.symbol.clone(),
        color: value.color,
        owner_id: value.owner_id.clone(),
        owner_name: value.owner_name.clone(),
        created_at: value.created_at,
        ttl_seconds: value.ttl_seconds,
        waypoint_kind: value.waypoint_kind.clone(),
        replace_old_quick: value.replace_old_quick,
        max_quick_marks: value.max_quick_marks,
        target_type: value.target_type.clone(),
        target_entity_id: value.target_entity_id.clone(),
        target_entity_type: value.target_entity_type.clone(),
        target_entity_name: value.target_entity_name.clone(),
        room_code: value.room_code.clone(),
        permanent: value.permanent,
        tactical_type: value.tactical_type.clone(),
        source_type: value.source_type.clone(),
        deletable_by: value.deletable_by.clone(),
    }
}

fn apply_player_delta(value: &mut PlayerData, delta: PlayerDelta) {
    if let Some(v) = delta.x {
        value.x = v;
    }
    if let Some(v) = delta.y {
        value.y = v;
    }
    if let Some(v) = delta.z {
        value.z = v;
    }
    if let Some(v) = delta.vx {
        value.vx = Some(v);
    }
    if let Some(v) = delta.vy {
        value.vy = Some(v);
    }
    if let Some(v) = delta.vz {
        value.vz = Some(v);
    }
    if let Some(v) = delta.dimension {
        value.dimension = v;
    }
    if let Some(v) = delta.player_name {
        value.player_name = Some(v);
    }
    if let Some(v) = delta.player_uuid {
        value.player_uuid = Some(v);
    }
    if let Some(v) = delta.health {
        value.health = Some(v);
    }
    if let Some(v) = delta.max_health {
        value.max_health = Some(v);
    }
    if let Some(v) = delta.armor {
        value.armor = Some(v);
    }
    if let Some(v) = delta.is_riding {
        value.is_riding = Some(v);
    }
    if let Some(v) = delta.width {
        value.width = Some(v);
    }
    if let Some(v) = delta.height {
        value.height = Some(v);
    }
    if let Some(v) = delta.position_source_id {
        value.position_source_id = Some(v);
    }
    if let Some(v) = delta.position_source_kind {
        value.position_source_kind = Some(v);
    }
    if let Some(v) = delta.position_source_display_name {
        value.position_source_display_name = Some(v);
    }
    if let Some(v) = delta.position_resolution {
        value.position_resolution = Some(v);
    }
}

fn apply_entity_delta(value: &mut EntityData, delta: EntityDelta) {
    if let Some(v) = delta.x {
        value.x = v;
    }
    if let Some(v) = delta.y {
        value.y = v;
    }
    if let Some(v) = delta.z {
        value.z = v;
    }
    if let Some(v) = delta.vx {
        value.vx = Some(v);
    }
    if let Some(v) = delta.vy {
        value.vy = Some(v);
    }
    if let Some(v) = delta.vz {
        value.vz = Some(v);
    }
    if let Some(v) = delta.dimension {
        value.dimension = v;
    }
    if let Some(v) = delta.entity_type {
        value.entity_type = Some(v);
    }
    if let Some(v) = delta.entity_name {
        value.entity_name = Some(v);
    }
    if let Some(v) = delta.width {
        value.width = Some(v);
    }
    if let Some(v) = delta.height {
        value.height = Some(v);
    }
}

fn apply_waypoint_delta(value: &mut WaypointData, delta: WaypointDelta) {
    if let Some(v) = delta.x {
        value.x = v;
    }
    if let Some(v) = delta.y {
        value.y = v;
    }
    if let Some(v) = delta.z {
        value.z = v;
    }
    if let Some(v) = delta.dimension {
        value.dimension = v;
    }
    if let Some(v) = delta.name {
        value.name = v;
    }
    if let Some(v) = delta.symbol {
        value.symbol = Some(v);
    }
    if let Some(v) = delta.color {
        value.color = Some(v);
    }
    if let Some(v) = delta.owner_id {
        value.owner_id = Some(v);
    }
    if let Some(v) = delta.owner_name {
        value.owner_name = Some(v);
    }
    if let Some(v) = delta.created_at {
        value.created_at = Some(v);
    }
    if let Some(v) = delta.ttl_seconds {
        value.ttl_seconds = Some(v);
    }
    if let Some(v) = delta.waypoint_kind {
        value.waypoint_kind = Some(v);
    }
    if let Some(v) = delta.replace_old_quick {
        value.replace_old_quick = Some(v);
    }
    if let Some(v) = delta.max_quick_marks {
        value.max_quick_marks = Some(v);
    }
    if let Some(v) = delta.target_type {
        value.target_type = Some(v);
    }
    if let Some(v) = delta.target_entity_id {
        value.target_entity_id = Some(v);
    }
    if let Some(v) = delta.target_entity_type {
        value.target_entity_type = Some(v);
    }
    if let Some(v) = delta.target_entity_name {
        value.target_entity_name = Some(v);
    }
    if let Some(v) = delta.room_code {
        value.room_code = Some(v);
    }
    if let Some(v) = delta.permanent {
        value.permanent = Some(v);
    }
    if let Some(v) = delta.tactical_type {
        value.tactical_type = Some(v);
    }
    if let Some(v) = delta.source_type {
        value.source_type = Some(v);
    }
    if let Some(v) = delta.deletable_by {
        value.deletable_by = Some(v);
    }
}

fn apply_battle_observation(
    source: &mut SourceState,
    reporter_id: &str,
    report: BattleMapObservation,
    received_at: Instant,
) {
    let dimension = if report.dimension.trim().is_empty() {
        "minecraft:overworld".to_owned()
    } else {
        report.dimension.trim().to_owned()
    };
    let mode = match report.mode.as_deref().map(str::trim) {
        Some("simmc") => "simmc",
        _ => "nodemc",
    };
    let candidates: Vec<_> = report
        .candidates
        .into_iter()
        .filter(|candidate| {
            matches!(
                candidate.source.as_str(),
                "history_primary" | "history_boundary_alternative"
            )
        })
        .collect();
    let cells: Vec<_> = report
        .cells
        .into_iter()
        .filter(|cell| !cell.color_raw.trim().is_empty())
        .collect();
    if candidates.is_empty() || cells.is_empty() {
        return;
    }

    let chosen = if candidates.len() == 1 {
        candidates.first()
    } else if let Some(previous) = source.battle_projection.as_ref().filter(|previous| {
        previous.dimension == dimension
            && (0..=10_000).contains(&(report.snapshot_observed_at - previous.snapshot_observed_at))
    }) {
        let matched: Vec<_> = candidates
            .iter()
            .filter(|candidate| {
                (candidate.base_chunk_x - previous.base_chunk_x).abs()
                    + (candidate.base_chunk_z - previous.base_chunk_z).abs()
                    <= 1
            })
            .collect();
        (matched.len() == 1).then(|| matched[0]).or_else(|| {
            choose_battle_candidate_by_overlap(
                &candidates,
                &cells,
                &dimension,
                &source.battle_chunks,
            )
        })
    } else {
        choose_battle_candidate_by_overlap(&candidates, &cells, &dimension, &source.battle_chunks)
    };
    let Some(chosen) = chosen else {
        return;
    };

    if let Some(previous) = source.battle_projection.take() {
        for key in previous.chunk_ids {
            source.battle_chunks.remove(&key);
        }
    }
    let mut chunk_ids = Vec::with_capacity(cells.len());
    for cell in cells {
        let chunk_x = chosen.base_chunk_x.saturating_add(cell.rel_chunk_x);
        let chunk_z = chosen.base_chunk_z.saturating_add(cell.rel_chunk_z);
        let reference = BattleChunkRef {
            dimension: dimension.clone(),
            coord: Some(crate::proto::teamviewer::v1::BattleChunkCoord { chunk_x, chunk_z }),
        };
        let Some(key) = battle_ref_key(&reference) else {
            continue;
        };
        let symbol = cell.symbol.filter(|symbol| !symbol.is_empty());
        let marker_type = symbol
            .as_deref()
            .filter(|symbol| matches!(*symbol, "╫" | "╬"))
            .map(|_| "war_core".to_owned());
        source.battle_chunks.insert(
            key.clone(),
            Timed::new(
                BattleChunkEntry {
                    r#ref: Some(reference),
                    data: Some(BattleChunkValue {
                        symbol,
                        marker_type,
                        color_raw: cell.color_raw.trim().to_owned(),
                        color_note: None,
                        observed_at: Some(report.snapshot_observed_at),
                        position_sampled_at: Some(chosen.position_sampled_at),
                        alignment_source: Some(chosen.source.clone()),
                        reporter_id: Some(reporter_id.to_owned()),
                        room_code: Some(source.room.clone()),
                        color_mode: Some("raw_observed".to_owned()),
                        color_semantic_key: None,
                        mode: Some(mode.to_owned()),
                    }),
                },
                received_at,
            ),
        );
        chunk_ids.push(key);
    }
    source.battle_projection = Some(BattleProjection {
        dimension,
        base_chunk_x: chosen.base_chunk_x,
        base_chunk_z: chosen.base_chunk_z,
        snapshot_observed_at: report.snapshot_observed_at,
        chunk_ids,
    });
}

fn choose_battle_candidate_by_overlap<'a>(
    candidates: &'a [crate::proto::teamviewer::v1::BattleMapObservationCandidate],
    cells: &[crate::proto::teamviewer::v1::BattleMapObservationCell],
    dimension: &str,
    existing: &HashMap<String, Timed<BattleChunkEntry>>,
) -> Option<&'a crate::proto::teamviewer::v1::BattleMapObservationCandidate> {
    let mut scored: Vec<_> = candidates
        .iter()
        .map(|candidate| {
            let score = cells
                .iter()
                .filter(|cell| {
                    let key = battle_key(
                        dimension,
                        candidate.base_chunk_x.saturating_add(cell.rel_chunk_x),
                        candidate.base_chunk_z.saturating_add(cell.rel_chunk_z),
                    );
                    existing
                        .get(&key)
                        .and_then(|entry| entry.data.data.as_ref())
                        .is_some_and(|value| {
                            value.symbol == cell.symbol && value.color_raw == cell.color_raw
                        })
                })
                .count();
            (score, candidate)
        })
        .collect();
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    let (best_score, best) = scored.first().copied()?;
    let ties = scored
        .iter()
        .filter(|(score, _)| *score == best_score)
        .count();
    if ties == 1 || (best_score == 0 && existing.is_empty()) {
        Some(best)
    } else {
        None
    }
}

fn battle_chunk_without_meta(mut entry: BattleChunkEntry) -> BattleChunkEntry {
    if let Some(data) = entry.data.as_mut() {
        data.observed_at = None;
        data.position_sampled_at = None;
        data.alignment_source = None;
        data.reporter_id = None;
    }
    entry
}

fn battle_key(dimension: &str, chunk_x: i32, chunk_z: i32) -> String {
    format!("{dimension}|{chunk_x}|{chunk_z}")
}

fn battle_ref_key(reference: &BattleChunkRef) -> Option<String> {
    let coord = reference.coord.as_ref()?;
    (!reference.dimension.trim().is_empty())
        .then(|| battle_key(reference.dimension.trim(), coord.chunk_x, coord.chunk_z))
}

fn battle_entry_key(entry: &BattleChunkEntry) -> Option<String> {
    entry.r#ref.as_ref().and_then(battle_ref_key)
}

fn parse_battle_key(key: &str) -> Option<BattleChunkRef> {
    let mut parts = key.rsplitn(3, '|');
    let chunk_z = parts.next()?.parse().ok()?;
    let chunk_x = parts.next()?.parse().ok()?;
    let dimension = parts.next()?.to_owned();
    Some(BattleChunkRef {
        dimension,
        coord: Some(crate::proto::teamviewer::v1::BattleChunkCoord { chunk_x, chunk_z }),
    })
}

fn clear_player_fields(value: &mut PlayerData, fields: &[String]) {
    for field in fields {
        match field.as_str() {
            "vx" => value.vx = None,
            "vy" => value.vy = None,
            "vz" => value.vz = None,
            "playerName" => value.player_name = None,
            "playerUuid" | "playerUUID" => value.player_uuid = None,
            "health" => value.health = None,
            "maxHealth" => value.max_health = None,
            "armor" => value.armor = None,
            "isRiding" => value.is_riding = None,
            "width" => value.width = None,
            "height" => value.height = None,
            "positionSourceId" => value.position_source_id = None,
            "positionSourceKind" => value.position_source_kind = None,
            "positionSourceDisplayName" => value.position_source_display_name = None,
            "positionResolution" => value.position_resolution = None,
            _ => {}
        }
    }
}

fn clear_entity_fields(value: &mut EntityData, fields: &[String]) {
    for field in fields {
        match field.as_str() {
            "vx" => value.vx = None,
            "vy" => value.vy = None,
            "vz" => value.vz = None,
            "entityType" => value.entity_type = None,
            "entityName" => value.entity_name = None,
            "width" => value.width = None,
            "height" => value.height = None,
            _ => {}
        }
    }
}

fn clear_waypoint_fields(value: &mut WaypointData, fields: &[String]) {
    for field in fields {
        match field.as_str() {
            "symbol" => value.symbol = None,
            "color" => value.color = None,
            "ownerId" => value.owner_id = None,
            "ownerName" => value.owner_name = None,
            "createdAt" => value.created_at = None,
            "ttlSeconds" => value.ttl_seconds = None,
            "waypointKind" => value.waypoint_kind = None,
            "replaceOldQuick" => value.replace_old_quick = None,
            "maxQuickMarks" => value.max_quick_marks = None,
            "targetType" => value.target_type = None,
            "targetEntityId" => value.target_entity_id = None,
            "targetEntityType" => value.target_entity_type = None,
            "targetEntityName" => value.target_entity_name = None,
            "roomCode" => value.room_code = None,
            "permanent" => value.permanent = None,
            "tacticalType" => value.tactical_type = None,
            "sourceType" => value.source_type = None,
            "deletableBy" => value.deletable_by = None,
            _ => {}
        }
    }
}

fn player_cleared_fields(old: &PlayerData, new: &PlayerData) -> Vec<String> {
    let mut fields = Vec::new();
    macro_rules! cleared {
        ($field:ident, $name:literal) => {
            if old.$field.is_some() && new.$field.is_none() {
                fields.push($name.to_owned());
            }
        };
    }
    cleared!(vx, "vx");
    cleared!(vy, "vy");
    cleared!(vz, "vz");
    cleared!(player_name, "playerName");
    cleared!(player_uuid, "playerUUID");
    cleared!(health, "health");
    cleared!(max_health, "maxHealth");
    cleared!(armor, "armor");
    cleared!(is_riding, "isRiding");
    cleared!(width, "width");
    cleared!(height, "height");
    cleared!(position_source_id, "positionSourceId");
    cleared!(position_source_kind, "positionSourceKind");
    cleared!(position_source_display_name, "positionSourceDisplayName");
    cleared!(position_resolution, "positionResolution");
    fields
}

fn entity_cleared_fields(old: &EntityData, new: &EntityData) -> Vec<String> {
    let mut fields = Vec::new();
    macro_rules! cleared {
        ($field:ident, $name:literal) => {
            if old.$field.is_some() && new.$field.is_none() {
                fields.push($name.to_owned());
            }
        };
    }
    cleared!(vx, "vx");
    cleared!(vy, "vy");
    cleared!(vz, "vz");
    cleared!(entity_type, "entityType");
    cleared!(entity_name, "entityName");
    cleared!(width, "width");
    cleared!(height, "height");
    fields
}

fn waypoint_cleared_fields(old: &WaypointData, new: &WaypointData) -> Vec<String> {
    let mut fields = Vec::new();
    macro_rules! cleared {
        ($field:ident, $name:literal) => {
            if old.$field.is_some() && new.$field.is_none() {
                fields.push($name.to_owned());
            }
        };
    }
    cleared!(symbol, "symbol");
    cleared!(color, "color");
    cleared!(owner_id, "ownerId");
    cleared!(owner_name, "ownerName");
    cleared!(created_at, "createdAt");
    cleared!(ttl_seconds, "ttlSeconds");
    cleared!(waypoint_kind, "waypointKind");
    cleared!(replace_old_quick, "replaceOldQuick");
    cleared!(max_quick_marks, "maxQuickMarks");
    cleared!(target_type, "targetType");
    cleared!(target_entity_id, "targetEntityId");
    cleared!(target_entity_type, "targetEntityType");
    cleared!(target_entity_name, "targetEntityName");
    cleared!(room_code, "roomCode");
    cleared!(permanent, "permanent");
    cleared!(tactical_type, "tacticalType");
    cleared!(source_type, "sourceType");
    cleared!(deletable_by, "deletableBy");
    fields
}

fn battle_cleared_fields(old: &BattleChunkValue, new: &BattleChunkValue) -> Vec<String> {
    let mut fields = Vec::new();
    macro_rules! cleared {
        ($field:ident, $name:literal) => {
            if old.$field.is_some() && new.$field.is_none() {
                fields.push($name.to_owned());
            }
        };
    }
    cleared!(symbol, "symbol");
    cleared!(marker_type, "markerType");
    cleared!(color_note, "colorNote");
    cleared!(room_code, "roomCode");
    cleared!(color_mode, "colorMode");
    cleared!(color_semantic_key, "colorSemanticKey");
    cleared!(mode, "mode");
    fields
}

fn tab_key(entry: &crate::proto::teamviewer::v1::TabPlayerEntry) -> Option<String> {
    entry
        .uuid
        .clone()
        .or_else(|| entry.name.as_ref().map(|name| name.to_lowercase()))
}

fn tab_player_json(entry: &TabPlayerEntry) -> Value {
    prune_nulls(json!({
        "uuid":entry.uuid,
        "name":entry.name,
        "displayName":entry.display_name,
        "prefixedName":entry.prefixed_name,
        "scoreboardTeamId":entry.scoreboard_team_id,
        "scoreboardPrefix":entry.scoreboard_prefix,
        "scoreboardSuffix":entry.scoreboard_suffix,
        "scoreboardColorRgb":entry.scoreboard_color_rgb,
    }))
}

fn player_mark_json(mark: &PlayerMark) -> Value {
    prune_nulls(json!({
        "team":mark.team,
        "color":mark.color,
        "label":mark.label,
        "source":mark.source,
    }))
}

fn player_json_map(players: &HashMap<String, PlayerData>) -> Value {
    Value::Object(players.iter().map(|(id, player)| {
        (id.clone(), json!({"x": player.x, "y": player.y, "z": player.z, "dimension": player.dimension,
            "playerName": player.player_name, "playerUUID": player.player_uuid}))
    }).collect())
}

fn snapshot_digest(
    snapshot: &SnapshotFull,
    include_last_seen: bool,
    include_source_metadata: bool,
) -> Digest {
    let players = snapshot
        .players
        .iter()
        .map(|(id, player)| {
            let mut value = player_json(player);
            if !include_source_metadata && let Some(object) = value.as_object_mut() {
                for field in [
                    "positionSourceId",
                    "positionSourceKind",
                    "positionSourceDisplayName",
                    "positionResolution",
                ] {
                    object.remove(field);
                }
            }
            (id.clone(), value)
        })
        .collect();
    let entities = snapshot
        .entities
        .iter()
        .map(|(id, entity)| (id.clone(), entity_json(entity)))
        .collect();
    let waypoints = snapshot
        .waypoints
        .iter()
        .map(|(id, waypoint)| (id.clone(), waypoint_json(waypoint)))
        .collect();
    let battle_chunks = snapshot
        .battle_chunks
        .iter()
        .filter_map(|entry| {
            Some((
                battle_entry_key(entry)?,
                battle_value_json(entry.data.as_ref()?),
            ))
        })
        .collect();
    let last_seen_players = snapshot
        .last_seen_players
        .iter()
        .map(|(id, player)| (id.clone(), last_seen_json(player)))
        .collect();
    Digest {
        players: state_digest_plain(players),
        entities: state_digest_plain(entities),
        waypoints: state_digest_plain(waypoints),
        battle_chunks: Some(state_digest_plain(battle_chunks)),
        last_seen_players: include_last_seen.then(|| state_digest_plain(last_seen_players)),
    }
}

fn player_json(player: &PlayerData) -> Value {
    prune_nulls(json!({
        "x":player.x,"y":player.y,"z":player.z,"vx":player.vx,"vy":player.vy,"vz":player.vz,
        "dimension":player.dimension,"playerName":player.player_name,"playerUUID":player.player_uuid,
        "health":player.health,"maxHealth":player.max_health,"armor":player.armor,
        "isRiding":player.is_riding,"width":player.width,"height":player.height,
        "positionSourceId":player.position_source_id,
        "positionSourceKind":player.position_source_kind.and_then(|value| PlayerPositionSourceKind::try_from(value).ok()).map(|value| value.as_str_name()),
        "positionSourceDisplayName":player.position_source_display_name,
        "positionResolution":player.position_resolution,
    }))
}

fn entity_json(entity: &EntityData) -> Value {
    prune_nulls(json!({
        "x":entity.x,"y":entity.y,"z":entity.z,"vx":entity.vx,"vy":entity.vy,"vz":entity.vz,
        "dimension":entity.dimension,"entityType":entity.entity_type,"entityName":entity.entity_name,
        "width":entity.width,"height":entity.height,
    }))
}

fn waypoint_json(waypoint: &WaypointData) -> Value {
    prune_nulls(json!({
        "x":waypoint.x,"y":waypoint.y,"z":waypoint.z,"dimension":waypoint.dimension,
        "name":waypoint.name,"symbol":waypoint.symbol,"color":waypoint.color,
        "ownerId":waypoint.owner_id,"ownerName":waypoint.owner_name,"createdAt":waypoint.created_at,
        "ttlSeconds":waypoint.ttl_seconds,"waypointKind":waypoint.waypoint_kind,
        "replaceOldQuick":waypoint.replace_old_quick,"maxQuickMarks":waypoint.max_quick_marks,
        "targetType":waypoint.target_type,"targetEntityId":waypoint.target_entity_id,
        "targetEntityType":waypoint.target_entity_type,"targetEntityName":waypoint.target_entity_name,
        "roomCode":waypoint.room_code,"permanent":waypoint.permanent,
        "tacticalType":waypoint.tactical_type,"sourceType":waypoint.source_type,
        "deletableBy":waypoint.deletable_by,
    }))
}

fn battle_value_json(value: &BattleChunkValue) -> Value {
    prune_nulls(json!({
        "symbol":value.symbol,"markerType":value.marker_type,"colorRaw":value.color_raw,
        "colorNote":value.color_note,"roomCode":value.room_code,"colorMode":value.color_mode,
        "colorSemanticKey":value.color_semantic_key,"mode":value.mode,
    }))
}

fn last_seen_json(player: &LastSeenPlayerData) -> Value {
    prune_nulls(json!({
        "x":player.x,"y":player.y,"z":player.z,"dimension":player.dimension,
        "playerName":player.player_name,"playerUUID":player.player_uuid,
        "lastSeenAtUtcMs":player.last_seen_at_utc_ms,
        "positionObservedAtUtcMs":player.position_observed_at_utc_ms,
        "offlineDetectedAtUtcMs":player.offline_detected_at_utc_ms,
    }))
}

fn prune_nulls(value: Value) -> Value {
    match value {
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key, prune_nulls(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(prune_nulls).collect()),
        value => value,
    }
}

fn state_digest_plain(values: BTreeMap<String, Value>) -> String {
    let mut raw = String::new();
    for (id, value) in values {
        raw.push_str(&serde_json::to_string(&id).expect("string is JSON serializable"));
        raw.push(':');
        raw.push_str(&canonical_value(&value));
        raw.push('\n');
    }
    let digest = Sha1::digest(raw.as_bytes());
    format!("{digest:x}")[..16].to_owned()
}

fn canonical_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                value.to_string()
            } else if let Some(value) = value.as_u64() {
                value.to_string()
            } else {
                canonical_number(value.as_f64().unwrap_or(f64::NAN))
            }
        }
        Value::String(value) => serde_json::to_string(value).expect("string is JSON serializable"),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(canonical_value)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::Object(values) => {
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort();
            let body = keys
                .into_iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).expect("key is JSON serializable"),
                        canonical_value(&values[key])
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{body}}}")
        }
    }
}

fn canonical_number(value: f64) -> String {
    if !value.is_finite() {
        return "null".to_owned();
    }
    let rounded = if value.is_sign_negative() {
        (value * 1_000_000.0 - 0.5).ceil() / 1_000_000.0
    } else {
        (value * 1_000_000.0 + 0.5).floor() / 1_000_000.0
    };
    let mut text = format!("{rounded:.6}");
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    if text == "-0" { "0".to_owned() } else { text }
}

fn unix_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn unix_millis() -> i64 {
    (unix_seconds() * 1_000.0) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add_test_connection(relay: &mut Relay, id: &str, kind: ConnectionKind) {
        let (control, _) = mpsc::channel(1);
        let (state, _) = watch::channel(None);
        relay.connections.insert(
            id.to_owned(),
            Connection {
                room: "room".to_owned(),
                protocol: "0.7.0".to_owned(),
                kind,
                display_name: None,
                position_resolution: None,
                program_version: "test".to_owned(),
                remote_addr: "127.0.0.1".to_owned(),
                connected_at: 0,
                control,
                state,
                delivered_revision: 0,
                queued_revision: 0,
                delivered_snapshot: None,
                queued_snapshot: None,
                force_full: true,
                tab_history_subscribed: false,
                last_digest_sent: None,
            },
        );
    }

    #[test]
    fn patch_contains_changed_and_deleted_players() {
        let mut old = SnapshotFull::default();
        old.players.insert(
            "changed".into(),
            PlayerData {
                x: 1.0,
                ..Default::default()
            },
        );
        old.players.insert("deleted".into(), PlayerData::default());
        let mut new = SnapshotFull::default();
        new.players.insert(
            "changed".into(),
            PlayerData {
                x: 2.0,
                ..Default::default()
            },
        );
        let patch = build_patch(&old, &new).expect("patch");
        let players = patch.players.expect("players");
        assert_eq!(players.upsert.len(), 1);
        assert_eq!(players.delete, vec!["deleted"]);
    }

    #[test]
    fn canonical_digest_matches_python_and_mod() {
        let values = BTreeMap::from([("<id>&".to_owned(), json!({"name":"<A&B>", "x":1.2345645}))]);
        assert_eq!(canonical_number(1.2345645), "1.234565");
        assert_eq!(state_digest_plain(values), "0cabc8c9afc26756");
    }

    #[test]
    fn source_resolution_honors_priority_stickiness_and_lexical_ties() {
        let base = Instant::now();
        let mut sources = HashMap::new();
        for (source_id, x, offset_ms) in [
            ("source-b", 2.0, 100),
            ("source-a", 1.0, 100),
            ("target", 9.0, 0),
        ] {
            let mut source = SourceState {
                room: "room".to_owned(),
                ..Default::default()
            };
            source.players.insert(
                "target".to_owned(),
                Timed::new(
                    PlayerData {
                        x,
                        ..Default::default()
                    },
                    base + Duration::from_millis(offset_ms),
                ),
            );
            sources.insert(source_id.to_owned(), source);
        }
        let mut selected = HashMap::new();
        let resolved = resolve_candidates(
            collect_candidates(&sources, "room", None, false, |source| &source.players),
            &mut selected,
            |object_id, source_id| i32::from(object_id == source_id),
        );
        assert_eq!(resolved["target"].0, "target");
        assert_eq!(resolved["target"].1.x, 9.0);

        sources.remove("target");
        selected.insert("target".to_owned(), "source-b".to_owned());
        let sticky = resolve_candidates(
            collect_candidates(&sources, "room", None, false, |source| &source.players),
            &mut selected,
            |_, _| 0,
        );
        assert_eq!(sticky["target"].0, "source-b");

        sources
            .get_mut("source-a")
            .expect("source")
            .players
            .get_mut("target")
            .expect("target")
            .received_at = base + Duration::from_millis(500);
        let switched = resolve_candidates(
            collect_candidates(&sources, "room", None, false, |source| &source.players),
            &mut selected,
            |_, _| 0,
        );
        assert_eq!(switched["target"].0, "source-a");
    }

    #[test]
    fn same_server_filter_groups_overlapping_tab_identities() {
        let config = Arc::new(RuntimeConfig::load());
        let mut relay = Relay::new(config, Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        relay.same_server_filter_enabled = true;
        add_test_connection(&mut relay, "source-a", ConnectionKind::Player);
        add_test_connection(&mut relay, "source-b", ConnectionKind::Player);
        add_test_connection(&mut relay, "source-c", ConnectionKind::ExternalSource);

        for (source_id, name) in [
            ("source-a", "Alice"),
            ("source-b", "ＡＬＩＣＥ"),
            ("source-c", "Bob"),
        ] {
            relay.sources.insert(
                source_id.to_owned(),
                SourceState {
                    room: "room".to_owned(),
                    tab_players: vec![TabPlayerEntry {
                        name: Some(name.to_owned()),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            );
        }

        let grouping = relay.same_server_grouping("room");
        assert_eq!(grouping.groups.len(), 2);
        assert_eq!(
            relay.allowed_sources_for_player("source-a", &grouping),
            HashSet::from(["source-a".to_owned(), "source-b".to_owned()])
        );
        assert_eq!(
            relay.allowed_sources_for_player("source-c", &grouping),
            HashSet::from(["source-c".to_owned()])
        );
    }
}
