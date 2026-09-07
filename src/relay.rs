use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    sync::{Arc, Weak},
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
use crate::protocol_compat::{
    ProtocolEpoch, ProtocolProfile, adapt_outbound_patch, project_snapshot,
};

pub const EVENT_CAPACITY: usize = 2_048;
pub const CONTROL_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConnectionKind {
    Player,
    ExternalSource,
    WebMap,
}

#[derive(Clone)]
pub struct StateFrame {
    pub revision: u64,
    pub bytes: Option<Arc<[u8]>>,
    pub digest: Option<Arc<[u8]>>,
    pub snapshot: Arc<SnapshotFull>,
    pub battle_revision: u64,
}

/// movement datagram 单帧 patch 编码上限。1024B + 4B 长度前缀落在 QUIC 初版
/// PMTU(~1200B)预算内,低 MTU 链路也留有余量。
pub const MOVEMENT_CHUNK_MAX_BYTES: usize = 1024;

/// 一个连接单个 tick 最多发送的 movement datagram 数;超出部分顺延下一 tick
/// (movement 批每 tick 全量重发,顺延只是该批玩家晚一 tick,无损)。
pub const MOVEMENT_MAX_CHUNKS_PER_TICK: usize = 16;

/// 一次广播周期内的玩家位置批:预编码为若干条目对齐的完整 patch(每块 ≤
/// [`MOVEMENT_CHUNK_MAX_BYTES`] 字节),由 WT datagram 任务按预算发送。
/// 绝对值 upsert、无 delete;丢失任意一块由下一 tick 全量重发自然恢复。
#[derive(Clone, Default)]
pub struct MovementBatch {
    pub chunks: Arc<[Arc<[u8]>]>,
}

pub struct RegisterConnection {
    pub id: String,
    pub room: String,
    pub protocol: ProtocolProfile,
    pub kind: ConnectionKind,
    pub display_name: Option<String>,
    pub position_resolution: Option<f64>,
    pub program_version: String,
    pub remote_addr: String,
    pub control: mpsc::Sender<Arc<[u8]>>,
    pub state: watch::Sender<Option<StateFrame>>,
    /// 服务端→客户端 movement datagram 批;无 datagram 的连接无人消费,空转无害。
    pub movement: watch::Sender<Option<Arc<MovementBatch>>>,
    /// 连接声明消费 datagram 位置且传输层支持:可靠 patch 剥离逐 tick 位置字段。
    pub unreliable_positions: bool,
}

pub enum RelayEvent {
    Register(RegisterConnection),
    Disconnect {
        id: String,
    },
    Delivered {
        id: String,
        revision: u64,
        snapshot: Arc<SnapshotFull>,
        battle_revision: u64,
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
    ReportPolicyUpdate {
        room: String,
        policy: crate::proto::teamviewer::v1::PlayerReportPolicy,
    },
    Snapshot {
        room: Option<String>,
        reply: oneshot::Sender<Value>,
    },
    DeleteLastSeen {
        records: Vec<(String, String, String)>,
        reply: oneshot::Sender<usize>,
    },
    PurgeRoom {
        room: String,
        reply: oneshot::Sender<RoomPurgeResult>,
    },
    RoomActiveConnections {
        room: String,
        reply: oneshot::Sender<usize>,
    },
    #[cfg(feature = "memory-debug")]
    DebugStats {
        reply: oneshot::Sender<RelayDebugStats>,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RoomPurgeResult {
    pub active_connections: usize,
    pub last_seen_records: usize,
    pub sources: usize,
    pub room_frames: usize,
    pub disconnected_external_sources: usize,
}

#[cfg(feature = "memory-debug")]
#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayDebugStats {
    pub connections: usize,
    pub player_connections: usize,
    pub web_map_connections: usize,
    pub external_source_connections: usize,
    pub active_rooms: usize,
    pub sources: usize,
    pub cache_sources: usize,
    pub tactical_sources: usize,
    pub disconnected_sources: usize,
    pub source_players: usize,
    pub source_entities: usize,
    pub source_waypoints: usize,
    pub source_battle_chunks: usize,
    pub battle_cache_chunks: usize,
    pub battle_cache_value_entries: usize,
    pub battle_cache_dimension_entries: usize,
    pub battle_cache_change_batches: usize,
    pub battle_cache_revisions: u64,
    pub battle_cache_evictions: u64,
    pub battle_cache_full_expansions: u64,
    pub battle_cache_patch_builds: u64,
    pub source_last_seen_players: usize,
    pub source_tab_players: usize,
    pub room_frames: usize,
    pub room_snapshot_objects: usize,
    pub room_web_snapshot_objects: usize,
    pub scoped_selections: usize,
    pub scoped_selection_entries: usize,
    pub player_marks: usize,
    pub refresh_entries: usize,
    pub disconnected_external_records: usize,
    pub relay_queue_remaining: usize,
    pub relay_queue_capacity: usize,
    pub broadcast_hz: f64,
    pub events_total: u64,
    pub player_reports: u64,
    pub delivered_events: u64,
    pub ticks_total: u64,
    pub cleanup_nanoseconds: u64,
    pub broadcasts_total: u64,
    pub broadcast_nanoseconds: u64,
    pub broadcast_recipients: u64,
    pub encoded_payload_bytes: u64,
    pub payload_builds_total: u64,
    pub payload_cache_hits: u64,
    pub digests_total: u64,
    pub digest_cache_hits: u64,
    pub digest_nanoseconds: u64,
}

#[cfg(feature = "memory-debug")]
#[derive(Default)]
struct RelayDebugCounters {
    events_total: u64,
    player_reports: u64,
    delivered_events: u64,
    ticks_total: u64,
    cleanup_nanoseconds: u64,
    broadcasts_total: u64,
    broadcast_nanoseconds: u64,
    broadcast_recipients: u64,
    encoded_payload_bytes: u64,
    payload_builds_total: u64,
    payload_cache_hits: u64,
    digests_total: u64,
    digest_cache_hits: u64,
    digest_nanoseconds: u64,
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

    pub async fn purge_room(&self, room: String) -> anyhow::Result<RoomPurgeResult> {
        let (reply, response) = oneshot::channel();
        self.send(RelayEvent::PurgeRoom { room, reply }).await?;
        response.await.context("relay purge response dropped")
    }

    pub async fn room_active_connections(&self, room: String) -> anyhow::Result<usize> {
        let (reply, response) = oneshot::channel();
        self.send(RelayEvent::RoomActiveConnections { room, reply })
            .await?;
        response.await.context("relay room count response dropped")
    }

    #[cfg(feature = "memory-debug")]
    pub async fn debug_stats(&self) -> anyhow::Result<RelayDebugStats> {
        let (reply, response) = oneshot::channel();
        self.send(RelayEvent::DebugStats { reply }).await?;
        let mut stats = response.await.context("relay debug response dropped")?;
        stats.relay_queue_remaining = self.tx.capacity();
        stats.relay_queue_capacity = EVENT_CAPACITY;
        Ok(stats)
    }
}

struct Connection {
    room: String,
    protocol: ProtocolProfile,
    kind: ConnectionKind,
    display_name: Option<String>,
    position_resolution: Option<f64>,
    program_version: String,
    remote_addr: String,
    connected_at: i64,
    control: mpsc::Sender<Arc<[u8]>>,
    state: watch::Sender<Option<StateFrame>>,
    movement: watch::Sender<Option<Arc<MovementBatch>>>,
    unreliable_positions: bool,
    last_position_refresh: Instant,
    delivered_revision: u64,
    delivered_snapshot: Option<Arc<SnapshotFull>>,
    delivered_battle_revision: u64,
    next_digest_at: Instant,
    force_full: bool,
    tab_history_subscribed: bool,
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

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct BattleCacheKey {
    dimension: Arc<str>,
    chunk_x: i32,
    chunk_z: i32,
}

impl BattleCacheKey {
    fn from_ref(reference: &BattleChunkRef, dimension: Arc<str>) -> Option<Self> {
        let coord = reference.coord.as_ref()?;
        Some(Self {
            dimension,
            chunk_x: coord.chunk_x,
            chunk_z: coord.chunk_z,
        })
    }

    fn to_ref(&self) -> BattleChunkRef {
        BattleChunkRef {
            dimension: self.dimension.to_string(),
            coord: Some(crate::proto::teamviewer::v1::BattleChunkCoord {
                chunk_x: self.chunk_x,
                chunk_z: self.chunk_z,
            }),
        }
    }
}

struct CachedBattleChunk {
    value: Arc<BattleChunkValue>,
    generation: u64,
}

struct BattleExpiryBatch {
    received_at: Instant,
    generation: u64,
    keys: VecDeque<BattleCacheKey>,
}

struct BattleChangeBatch {
    revision: u64,
    keys: Vec<BattleCacheKey>,
}

#[derive(Default)]
struct BattleRoomCache {
    entries: BTreeMap<BattleCacheKey, CachedBattleChunk>,
    dimensions: HashMap<String, Weak<str>>,
    values: HashMap<Vec<u8>, Weak<BattleChunkValue>>,
    expiry: VecDeque<BattleExpiryBatch>,
    changes: VecDeque<BattleChangeBatch>,
    change_operations: usize,
    revision: u64,
    next_generation: u64,
    evictions_total: u64,
    full_expansions_total: Cell<u64>,
    patch_builds_total: Cell<u64>,
}

impl BattleRoomCache {
    const MAX_CHANGE_BATCHES: usize = 256;
    const MAX_CHANGE_OPERATIONS: usize = 65_536;

    fn upsert(
        &mut self,
        entries: Vec<BattleChunkEntry>,
        received_at: Instant,
        max_entries: usize,
    ) -> bool {
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let generation = self.next_generation;
        let mut expiry_keys = Vec::with_capacity(entries.len());
        let mut changed = BTreeSet::new();

        for entry in entries {
            let Some(reference) = entry.r#ref.as_ref() else {
                continue;
            };
            let dimension = reference.dimension.trim();
            if dimension.is_empty() {
                continue;
            }
            let dimension = self.intern_dimension(dimension);
            let Some(key) = BattleCacheKey::from_ref(reference, dimension) else {
                continue;
            };
            let Some(value) = entry.data else {
                continue;
            };
            let value = self.intern_value(value);
            let previous = self.entries.get(&key);
            let visible_changed =
                previous.is_none_or(|current| current.value.as_ref() != value.as_ref());
            self.entries
                .insert(key.clone(), CachedBattleChunk { value, generation });
            expiry_keys.push(key.clone());
            if visible_changed {
                changed.insert(key);
            }
        }

        if !expiry_keys.is_empty() {
            self.expiry.push_back(BattleExpiryBatch {
                received_at,
                generation,
                keys: expiry_keys.into(),
            });
        }
        self.evict_to_limit(max_entries, &mut changed);
        let changed = self.record_changes(changed);
        if generation.is_multiple_of(64) {
            self.prune_stale_indexes();
        }
        changed
    }

    fn cleanup(&mut self, now: Instant, retention: Duration, max_entries: usize) -> bool {
        let mut changed = BTreeSet::new();
        while self
            .expiry
            .front()
            .is_some_and(|batch| now.saturating_duration_since(batch.received_at) > retention)
        {
            self.pop_expiry_batch(&mut changed);
        }
        self.evict_to_limit(max_entries, &mut changed);
        let changed = self.record_changes(changed);
        if changed {
            self.prune_interners();
        }
        changed
    }

    fn full_entries(&self, include_mode: bool) -> Vec<BattleChunkEntry> {
        self.full_expansions_total
            .set(self.full_expansions_total.get().saturating_add(1));
        self.entries
            .iter()
            .map(|(key, entry)| self.proto_entry(key, entry, include_mode))
            .collect()
    }

    fn digest(&self, contract: BattleChunkDigestContract, include_mode: bool) -> String {
        battle_cache_digest(self, contract, include_mode)
    }

    fn patch_since(
        &self,
        delivered_revision: u64,
        include_mode: bool,
        include_clear_fields: bool,
    ) -> Option<Option<BattleChunkPatchScope>> {
        if delivered_revision == self.revision {
            return Some(None);
        }
        if delivered_revision > self.revision
            || self
                .changes
                .front()
                .is_none_or(|batch| batch.revision > delivered_revision.saturating_add(1))
        {
            return None;
        }

        let keys = self
            .changes
            .iter()
            .filter(|batch| batch.revision > delivered_revision)
            .flat_map(|batch| batch.keys.iter().cloned())
            .collect::<BTreeSet<_>>();
        let mut upsert = Vec::new();
        let mut delete = Vec::new();
        for key in keys {
            if let Some(entry) = self.entries.get(&key) {
                let proto = self.proto_entry(&key, entry, include_mode);
                upsert.push(BattleChunkUpsert {
                    r#ref: proto.r#ref,
                    clear_fields: if include_clear_fields {
                        battle_missing_fields(
                            proto.data.as_ref().expect("cache entries have data"),
                            include_mode,
                        )
                    } else {
                        Vec::new()
                    },
                    data: proto.data,
                });
            } else {
                delete.push(key.to_ref());
            }
        }
        self.patch_builds_total
            .set(self.patch_builds_total.get().saturating_add(1));
        Some(Some(BattleChunkPatchScope { upsert, delete }))
    }

    fn meta_entries(&self, requested: &HashSet<String>) -> Vec<BattleChunkMetaEntry> {
        self.entries
            .iter()
            .filter(|(key, _)| {
                requested.contains(&battle_key(&key.dimension, key.chunk_x, key.chunk_z))
            })
            .map(|(key, entry)| BattleChunkMetaEntry {
                r#ref: Some(key.to_ref()),
                data: Some(entry.value.as_ref().clone()),
            })
            .collect()
    }

    fn proto_entry(
        &self,
        key: &BattleCacheKey,
        entry: &CachedBattleChunk,
        include_mode: bool,
    ) -> BattleChunkEntry {
        let mut value = entry.value.as_ref().clone();
        value.observed_at = None;
        value.position_sampled_at = None;
        value.alignment_source = None;
        value.reporter_id = None;
        if !include_mode {
            value.mode = None;
        }
        BattleChunkEntry {
            r#ref: Some(key.to_ref()),
            data: Some(value),
        }
    }

    fn intern_dimension(&mut self, value: &str) -> Arc<str> {
        if let Some(value) = self.dimensions.get(value).and_then(Weak::upgrade) {
            return value;
        }
        let interned: Arc<str> = Arc::from(value);
        self.dimensions
            .insert(value.to_owned(), Arc::downgrade(&interned));
        interned
    }

    fn intern_value(&mut self, value: BattleChunkValue) -> Arc<BattleChunkValue> {
        let encoded = value.encode_to_vec();
        if let Some(value) = self.values.get(&encoded).and_then(Weak::upgrade) {
            return value;
        }
        let interned = Arc::new(value);
        self.values.insert(encoded, Arc::downgrade(&interned));
        interned
    }

    fn evict_to_limit(&mut self, max_entries: usize, changed: &mut BTreeSet<BattleCacheKey>) {
        while self.entries.len() > max_entries && !self.expiry.is_empty() {
            self.pop_oldest_key(changed);
        }
    }

    fn pop_oldest_key(&mut self, changed: &mut BTreeSet<BattleCacheKey>) {
        let Some(batch) = self.expiry.front_mut() else {
            return;
        };
        let generation = batch.generation;
        let key = batch.keys.pop_front();
        if batch.keys.is_empty() {
            self.expiry.pop_front();
        }
        let Some(key) = key else {
            return;
        };
        if self
            .entries
            .get(&key)
            .is_some_and(|entry| entry.generation == generation)
        {
            self.entries.remove(&key);
            self.evictions_total = self.evictions_total.saturating_add(1);
            changed.insert(key);
        }
    }

    fn pop_expiry_batch(&mut self, changed: &mut BTreeSet<BattleCacheKey>) {
        let Some(batch) = self.expiry.pop_front() else {
            return;
        };
        for key in batch.keys {
            if self
                .entries
                .get(&key)
                .is_some_and(|entry| entry.generation == batch.generation)
            {
                self.entries.remove(&key);
                self.evictions_total = self.evictions_total.saturating_add(1);
                changed.insert(key);
            }
        }
    }

    fn record_changes(&mut self, changed: BTreeSet<BattleCacheKey>) -> bool {
        if changed.is_empty() {
            return false;
        }
        self.revision = self.revision.wrapping_add(1).max(1);
        let keys = changed.into_iter().collect::<Vec<_>>();
        self.change_operations = self.change_operations.saturating_add(keys.len());
        self.changes.push_back(BattleChangeBatch {
            revision: self.revision,
            keys,
        });
        while self.changes.len() > Self::MAX_CHANGE_BATCHES
            || self.change_operations > Self::MAX_CHANGE_OPERATIONS
        {
            let Some(batch) = self.changes.pop_front() else {
                break;
            };
            self.change_operations = self.change_operations.saturating_sub(batch.keys.len());
        }
        true
    }

    fn prune_interners(&mut self) {
        self.dimensions.retain(|_, value| value.strong_count() > 0);
        self.values.retain(|_, value| value.strong_count() > 0);
    }

    fn prune_stale_indexes(&mut self) {
        let entries = &self.entries;
        for batch in &mut self.expiry {
            batch.keys.retain(|key| {
                entries
                    .get(key)
                    .is_some_and(|entry| entry.generation == batch.generation)
            });
        }
        self.expiry.retain(|batch| !batch.keys.is_empty());
        self.prune_interners();
    }
}

#[derive(Default)]
struct RoomFrame {
    revision: u64,
    snapshot: SnapshotFull,
    web_snapshot: SnapshotFull,
    selected_players: PlayerSourceSelections,
    selected_entities: HashMap<String, String>,
    selected_waypoints: HashMap<String, String>,
    selected_battle_chunks: HashMap<String, String>,
    selected_last_seen_players: HashMap<String, String>,
}

#[derive(Default, Clone)]
struct ScopeSelections {
    players: PlayerSourceSelections,
    entities: HashMap<String, String>,
    waypoints: HashMap<String, String>,
    battle_chunks: HashMap<String, String>,
    last_seen_players: HashMap<String, String>,
}

#[derive(Default, Clone)]
struct PlayerSourceSelections {
    selected_sources: HashMap<String, String>,
    game_seen_since: HashMap<String, HashMap<String, Instant>>,
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
    battle_caches: HashMap<String, BattleRoomCache>,
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
    #[cfg(feature = "memory-debug")]
    debug_counters: RelayDebugCounters,
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
            battle_caches: HashMap::new(),
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
            #[cfg(feature = "memory-debug")]
            debug_counters: RelayDebugCounters::default(),
        }
    }

    async fn run(mut self, mut events: mpsc::Receiver<RelayEvent>) {
        let mut next_tick = tokio::time::Instant::now();
        loop {
            tokio::select! {
                event = events.recv() => {
                    let Some(event) = event else { break };
                    #[cfg(feature = "memory-debug")]
                    {
                        match &event {
                            RelayEvent::DebugStats { .. } => {}
                            RelayEvent::PlayerReport { .. } => {
                                self.debug_counters.events_total =
                                    self.debug_counters.events_total.saturating_add(1);
                                self.debug_counters.player_reports =
                                    self.debug_counters.player_reports.saturating_add(1);
                            }
                            RelayEvent::Delivered { .. } => {
                                self.debug_counters.events_total =
                                    self.debug_counters.events_total.saturating_add(1);
                                self.debug_counters.delivered_events =
                                    self.debug_counters.delivered_events.saturating_add(1);
                            }
                            _ => {
                                self.debug_counters.events_total =
                                    self.debug_counters.events_total.saturating_add(1);
                            }
                        }
                    }
                    self.handle_event(event);
                }
                _ = tokio::time::sleep_until(next_tick) => {
                    #[cfg(feature = "memory-debug")]
                    {
                        self.debug_counters.ticks_total =
                            self.debug_counters.ticks_total.saturating_add(1);
                    }
                    let next_hz = self.config.broadcast_hz(
                        self.player_connection_count.load(std::sync::atomic::Ordering::Relaxed)
                    );
                    if (next_hz - self.broadcast_hz).abs() > f64::EPSILON {
                        self.broadcast_hz = next_hz;
                        self.broadcast_report_rate_hints("congestion");
                    }
                    let now = Instant::now();
                    #[cfg(feature = "memory-debug")]
                    let cleanup_started = Instant::now();
                    if self.cleanup_timeouts(now) {
                        self.dirty = true;
                    }
                    #[cfg(feature = "memory-debug")]
                    {
                        self.debug_counters.cleanup_nanoseconds = self
                            .debug_counters
                            .cleanup_nanoseconds
                            .saturating_add(duration_nanoseconds(cleanup_started.elapsed()));
                    }
                    self.send_preexpiry_refresh_requests(now);
                    let state_dirty = self.dirty;
                    if state_dirty || self.digest_due(now) {
                        self.broadcast(now, state_dirty);
                        self.dirty = false;
                    }
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
                        movement: register.movement,
                        unreliable_positions: register.unreliable_positions,
                        last_position_refresh: Instant::now(),
                        delivered_revision: 0,
                        delivered_snapshot: None,
                        delivered_battle_revision: 0,
                        next_digest_at: Instant::now(),
                        force_full: true,
                        tab_history_subscribed: false,
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
                        "protocolVersion":connection.protocol.peer_current().to_string(),
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
            RelayEvent::Delivered {
                id,
                revision,
                snapshot,
                battle_revision,
            } => {
                if let Some(connection) = self.connections.get_mut(&id)
                    && revision >= connection.delivered_revision
                {
                    connection.delivered_revision = revision;
                    connection.delivered_snapshot = Some(snapshot);
                    connection.delivered_battle_revision = battle_revision;
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
            RelayEvent::ReportPolicyUpdate { room, policy } => {
                let bytes = encode_payload(
                    WireChannel::Player,
                    wire_envelope::Payload::PlayerReportPolicyUpdate(
                        crate::proto::teamviewer::v1::PlayerReportPolicyUpdate {
                            policy: Some(policy),
                        },
                    ),
                );
                for connection in self.connections.values().filter(|connection| {
                    connection.room == room
                        && connection.kind == ConnectionKind::Player
                        && connection.protocol.supports_relationships()
                }) {
                    let _ = connection.control.try_send(bytes.clone());
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
            RelayEvent::PurgeRoom { room, reply } => {
                let active_connections = self
                    .connections
                    .values()
                    .filter(|connection| connection.room == room)
                    .count();
                if active_connections > 0 {
                    let _ = reply.send(RoomPurgeResult {
                        active_connections,
                        ..Default::default()
                    });
                    return;
                }
                let source_ids = self
                    .sources
                    .iter()
                    .filter(|(_, source)| source.room == room)
                    .map(|(source_id, _)| source_id.clone())
                    .collect::<Vec<_>>();
                let last_seen_records = source_ids
                    .iter()
                    .filter_map(|source_id| self.sources.get(source_id))
                    .map(|source| source.last_seen_players.len())
                    .sum();
                for source_id in &source_ids {
                    self.sources.remove(source_id);
                    self.last_refresh_sent.remove(source_id);
                }
                let room_frames = usize::from(self.rooms.remove(&room).is_some());
                self.battle_caches.remove(&room);
                let before_disconnected = self.disconnected_external_sources.len();
                self.disconnected_external_sources
                    .retain(|record| record["roomCode"].as_str() != Some(room.as_str()));
                let disconnected_external_sources =
                    before_disconnected - self.disconnected_external_sources.len();
                self.scoped_selections.clear();
                self.dirty = true;
                let _ = reply.send(RoomPurgeResult {
                    active_connections: 0,
                    last_seen_records,
                    sources: source_ids.len(),
                    room_frames,
                    disconnected_external_sources,
                });
            }
            RelayEvent::RoomActiveConnections { room, reply } => {
                let active_connections = self
                    .connections
                    .values()
                    .filter(|connection| connection.room == room)
                    .count();
                let _ = reply.send(active_connections);
            }
            #[cfg(feature = "memory-debug")]
            RelayEvent::DebugStats { reply } => {
                let _ = reply.send(self.debug_stats());
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

    fn digest_due(&self, now: Instant) -> bool {
        self.connections.values().any(|connection| {
            connection.kind == ConnectionKind::Player && connection.next_digest_at <= now
        })
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
        let battle_cache_update = report.battle_map_observation.and_then(|observation| {
            let entries = apply_battle_observation(source, id, observation, now);
            (!entries.is_empty()).then(|| (source.room.clone(), entries))
        });
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
        if let Some((room, entries)) = battle_cache_update {
            self.battle_caches.entry(room).or_default().upsert(
                entries,
                now,
                self.config.battle_chunk_cache_max_entries,
            );
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
        let players = resolve_player_candidates(
            player_candidates,
            &mut selected_players,
            Instant::now(),
            |object_id, source_id| {
                if object_id.eq_ignore_ascii_case(source_id) {
                    2
                } else {
                    1
                }
            },
            |source_id| {
                self.connections
                    .get(source_id)
                    .is_some_and(|connection| connection.kind == ConnectionKind::ExternalSource)
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
        selected_battle_chunks.clear();
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
        let mut battle_chunks = self
            .battle_caches
            .get(&connection.room)
            .map_or_else(Vec::new, |cache| cache.meta_entries(&requested));
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
            source.battle_chunks.retain(|_, value| {
                now.duration_since(value.received_at)
                    <= Duration::from_secs(self.config.battle_chunk_timeout_sec)
            });
            changed |= before != source.battle_chunks.len();

            if !self.connections.contains_key(source_id)
                && source.tab_received_at.is_some_and(|received_at| {
                    now.duration_since(received_at)
                        > Duration::from_secs(self.config.tab_report_timeout_sec)
                })
            {
                changed |= !source.tab_players.is_empty();
                source.tab_players.clear();
                source.tab_received_at = None;
                source.tab_timestamp = None;
            }
        }
        let retention = Duration::from_secs(self.config.battle_chunk_cache_retention_sec);
        for cache in self.battle_caches.values_mut() {
            changed |= cache.cleanup(now, retention, self.config.battle_chunk_cache_max_entries);
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

    fn broadcast(&mut self, now: Instant, state_dirty: bool) {
        #[cfg(feature = "memory-debug")]
        let debug_started = Instant::now();
        #[cfg(feature = "memory-debug")]
        let mut debug_recipients = 0_u64;
        #[cfg(feature = "memory-debug")]
        let mut debug_payload_bytes = 0_u64;
        #[cfg(feature = "memory-debug")]
        let mut debug_payload_builds = 0_u64;
        #[cfg(feature = "memory-debug")]
        let mut debug_payload_cache_hits = 0_u64;
        #[cfg(feature = "memory-debug")]
        let mut debug_digests = 0_u64;
        #[cfg(feature = "memory-debug")]
        let mut debug_digest_cache_hits = 0_u64;
        #[cfg(feature = "memory-debug")]
        let mut debug_digest_nanoseconds = 0_u64;

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
            let battle_revision = self
                .battle_caches
                .get(&room)
                .map_or(0, |cache| cache.revision);
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
                    let position_refresh_due = now.duration_since(connection.last_position_refresh)
                        >= Duration::from_secs(self.config.movement_refresh_sec);
                    (
                        id.clone(),
                        connection.kind,
                        connection.protocol,
                        connection.force_full,
                        connection.delivered_snapshot.clone(),
                        connection.delivered_battle_revision,
                        connection.kind == ConnectionKind::Player
                            && connection.next_digest_at <= now,
                        // 位置低频保底到期的 tick 不剥离:可靠 patch 携带全量位置,
                        // datagram 路径整体失效时客户端最多落后一个刷新周期
                        connection.unreliable_positions && !position_refresh_due,
                    )
                })
                .collect::<Vec<_>>();
            let mut scoped_snapshots: HashMap<Vec<String>, Arc<SnapshotFull>> = HashMap::new();
            type ProjectionKey = (ConnectionKind, Vec<String>, ProtocolEpoch);
            type PayloadKey = (ProjectionKey, bool, Option<usize>, u64, bool);
            type DigestKey = (Vec<String>, bool, bool, bool, bool);
            let mut projected_snapshots: HashMap<ProjectionKey, Arc<SnapshotFull>> = HashMap::new();
            let mut encoded_payloads: HashMap<PayloadKey, Option<Arc<[u8]>>> = HashMap::new();
            let mut encoded_digests: HashMap<DigestKey, Arc<[u8]>> = HashMap::new();
            let mut movement_batches: HashMap<ProjectionKey, Arc<MovementBatch>> = HashMap::new();

            for (
                id,
                kind,
                protocol,
                force_full,
                delivered_snapshot,
                delivered_battle_revision,
                digest_due,
                strip_positions,
            ) in recipients
            {
                if !state_dirty && kind != ConnectionKind::Player {
                    continue;
                }
                let (base_target, scope_key) = if kind == ConnectionKind::WebMap {
                    (web_snapshot.clone(), Vec::new())
                } else {
                    let allowed = self.allowed_sources_for_player(&id, &grouping);
                    let mut scope_key = allowed.iter().cloned().collect::<Vec<_>>();
                    scope_key.sort();
                    if allowed == grouping.active_sources {
                        (player_snapshot.clone(), scope_key)
                    } else {
                        if let Some(snapshot) = scoped_snapshots.get(&scope_key) {
                            (snapshot.clone(), scope_key)
                        } else {
                            let snapshot =
                                Arc::new(self.build_room_snapshot(&room, false, Some(&allowed)));
                            scoped_snapshots.insert(scope_key.clone(), snapshot.clone());
                            (snapshot, scope_key)
                        }
                    }
                };
                let projection_key = (kind, scope_key, protocol.epoch());
                let target = projected_snapshots
                    .entry(projection_key.clone())
                    .or_insert_with(|| project_snapshot(protocol, base_target))
                    .clone();

                let channel = if kind == ConnectionKind::WebMap {
                    WireChannel::WebMap
                } else {
                    WireChannel::Player
                };
                // movement 批必须在 payload 判空之前发布:能力连接的可靠 patch
                // 常因位置被剥离而为空,datagram 恰是这些 tick 的唯一位置来源。
                // 仅对启用分流的连接编码发布,legacy 连接(WS/未声明能力)不浪费。
                if kind == ConnectionKind::WebMap
                    && self
                        .connections
                        .get(&id)
                        .is_some_and(|connection| connection.unreliable_positions)
                {
                    let batch = movement_batches
                        .entry(projection_key.clone())
                        .or_insert_with(|| Arc::new(movement_batch(&target)))
                        .clone();
                    if let Some(connection) = self.connections.get_mut(&id) {
                        connection.movement.send_replace(Some(batch));
                    }
                }

                let delivered_key = delivered_snapshot
                    .as_ref()
                    .map(|snapshot| Arc::as_ptr(snapshot) as usize);
                let payload_key = (
                    projection_key.clone(),
                    force_full,
                    delivered_key,
                    delivered_battle_revision,
                    strip_positions,
                );
                let bytes = match encoded_payloads.entry(payload_key) {
                    std::collections::hash_map::Entry::Occupied(entry) => {
                        #[cfg(feature = "memory-debug")]
                        {
                            debug_payload_cache_hits = debug_payload_cache_hits.saturating_add(1);
                        }
                        entry.get().clone()
                    }
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        #[cfg(feature = "memory-debug")]
                        {
                            debug_payload_builds = debug_payload_builds.saturating_add(1);
                        }
                        let bytes = state_payload_for_profile_with_battle(
                            protocol,
                            force_full,
                            delivered_snapshot.as_deref(),
                            delivered_battle_revision,
                            strip_positions,
                            &target,
                            self.battle_caches.get(&room),
                        )
                        .map(|payload| encode_payload(channel, payload));
                        entry.insert(bytes.clone());
                        bytes
                    }
                };
                let digest = if digest_due {
                    let digest_key = (
                        projection_key.1.clone(),
                        protocol.supports_battle_chunk_mode(),
                        protocol.uses_structured_battle_chunk_digest(),
                        protocol.supports_player_source_metadata(),
                        protocol.supports_last_seen_players(),
                    );
                    Some(match encoded_digests.entry(digest_key) {
                        std::collections::hash_map::Entry::Occupied(entry) => {
                            #[cfg(feature = "memory-debug")]
                            {
                                debug_digest_cache_hits = debug_digest_cache_hits.saturating_add(1);
                            }
                            entry.get().clone()
                        }
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            #[cfg(feature = "memory-debug")]
                            let digest_started = Instant::now();
                            let battle_digest = self.battle_caches.get(&room).map_or_else(
                                || {
                                    battle_chunk_digest(
                                        &[],
                                        battle_chunk_digest_contract(protocol),
                                        true,
                                    )
                                },
                                |cache| {
                                    cache.digest(
                                        battle_chunk_digest_contract(protocol),
                                        protocol.supports_battle_chunk_mode(),
                                    )
                                },
                            );
                            let bytes = encode_payload(
                                WireChannel::Player,
                                wire_envelope::Payload::Digest(snapshot_digest_with_battle_digest(
                                    &target,
                                    battle_digest,
                                    protocol,
                                )),
                            );
                            #[cfg(feature = "memory-debug")]
                            {
                                debug_digests = debug_digests.saturating_add(1);
                                debug_digest_nanoseconds = debug_digest_nanoseconds
                                    .saturating_add(duration_nanoseconds(digest_started.elapsed()));
                            }
                            entry.insert(bytes.clone());
                            bytes
                        }
                    })
                } else {
                    None
                };
                if bytes.is_none() && digest.is_none() {
                    continue;
                }
                #[cfg(feature = "memory-debug")]
                {
                    debug_recipients = debug_recipients.saturating_add(1);
                    if let Some(bytes) = &bytes {
                        debug_payload_bytes =
                            debug_payload_bytes.saturating_add(bytes.len() as u64);
                    }
                    if let Some(digest) = &digest {
                        debug_payload_bytes =
                            debug_payload_bytes.saturating_add(digest.len() as u64);
                    }
                }

                if let Some(connection) = self.connections.get_mut(&id) {
                    let sent_state = bytes.is_some();
                    connection.state.send_replace(Some(StateFrame {
                        revision,
                        bytes,
                        digest,
                        snapshot: target,
                        battle_revision,
                    }));
                    if sent_state {
                        connection.force_full = false;
                    }
                    // 本 tick 的可靠 patch/full 携带了完整位置(未剥离),视为一次保底刷新
                    if sent_state && !strip_positions {
                        connection.last_position_refresh = now;
                    }
                    if digest_due {
                        connection.next_digest_at =
                            now + Duration::from_secs(self.config.digest_interval_sec);
                    }
                }
            }
            let frame = self.rooms.entry(room).or_default();
            frame.revision = revision;
            frame.snapshot = (*player_snapshot).clone();
            frame.web_snapshot = (*web_snapshot).clone();
        }

        #[cfg(feature = "memory-debug")]
        {
            self.debug_counters.broadcasts_total =
                self.debug_counters.broadcasts_total.saturating_add(1);
            self.debug_counters.broadcast_nanoseconds = self
                .debug_counters
                .broadcast_nanoseconds
                .saturating_add(duration_nanoseconds(debug_started.elapsed()));
            self.debug_counters.broadcast_recipients = self
                .debug_counters
                .broadcast_recipients
                .saturating_add(debug_recipients);
            self.debug_counters.encoded_payload_bytes = self
                .debug_counters
                .encoded_payload_bytes
                .saturating_add(debug_payload_bytes);
            self.debug_counters.payload_builds_total = self
                .debug_counters
                .payload_builds_total
                .saturating_add(debug_payload_builds);
            self.debug_counters.payload_cache_hits = self
                .debug_counters
                .payload_cache_hits
                .saturating_add(debug_payload_cache_hits);
            self.debug_counters.digests_total = self
                .debug_counters
                .digests_total
                .saturating_add(debug_digests);
            self.debug_counters.digest_cache_hits = self
                .debug_counters
                .digest_cache_hits
                .saturating_add(debug_digest_cache_hits);
            self.debug_counters.digest_nanoseconds = self
                .debug_counters
                .digest_nanoseconds
                .saturating_add(debug_digest_nanoseconds);
        }
    }

    #[cfg(feature = "memory-debug")]
    fn debug_stats(&self) -> RelayDebugStats {
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
            .collect::<HashSet<_>>()
            .len();
        let source_players = self
            .sources
            .values()
            .map(|source| source.players.len())
            .sum();
        let source_entities = self
            .sources
            .values()
            .map(|source| source.entities.len())
            .sum();
        let source_waypoints = self
            .sources
            .values()
            .map(|source| source.waypoints.len())
            .sum();
        let source_battle_chunks = self
            .sources
            .values()
            .map(|source| source.battle_chunks.len())
            .sum();
        let battle_cache_chunks = self
            .battle_caches
            .values()
            .map(|cache| cache.entries.len())
            .sum();
        let battle_cache_value_entries = self
            .battle_caches
            .values()
            .map(|cache| cache.values.len())
            .sum();
        let battle_cache_dimension_entries = self
            .battle_caches
            .values()
            .map(|cache| cache.dimensions.len())
            .sum();
        let battle_cache_change_batches = self
            .battle_caches
            .values()
            .map(|cache| cache.changes.len())
            .sum();
        let battle_cache_revisions = self
            .battle_caches
            .values()
            .map(|cache| cache.revision)
            .sum();
        let battle_cache_evictions = self
            .battle_caches
            .values()
            .map(|cache| cache.evictions_total)
            .sum();
        let battle_cache_full_expansions = self
            .battle_caches
            .values()
            .map(|cache| cache.full_expansions_total.get())
            .sum();
        let battle_cache_patch_builds = self
            .battle_caches
            .values()
            .map(|cache| cache.patch_builds_total.get())
            .sum();
        let source_last_seen_players = self
            .sources
            .values()
            .map(|source| source.last_seen_players.len())
            .sum();
        let source_tab_players = self
            .sources
            .values()
            .map(|source| source.tab_players.len())
            .sum();
        let scoped_selection_entries = self
            .scoped_selections
            .values()
            .map(scope_selection_count)
            .sum();
        let room_snapshot_objects = self
            .rooms
            .iter()
            .map(|(room_id, room)| {
                snapshot_object_count(&room.snapshot)
                    + self
                        .battle_caches
                        .get(room_id)
                        .map_or(0, |cache| cache.entries.len())
            })
            .sum();
        let room_web_snapshot_objects = self
            .rooms
            .iter()
            .map(|(room_id, room)| {
                snapshot_object_count(&room.web_snapshot)
                    + self
                        .battle_caches
                        .get(room_id)
                        .map_or(0, |cache| cache.entries.len())
            })
            .sum();
        let counters = &self.debug_counters;
        RelayDebugStats {
            connections: self.connections.len(),
            player_connections,
            web_map_connections,
            external_source_connections,
            active_rooms,
            sources: self.sources.len(),
            cache_sources: self.battle_caches.len(),
            tactical_sources: self
                .sources
                .keys()
                .filter(|id| id.starts_with("__web_map_tactical__:"))
                .count(),
            disconnected_sources: self
                .sources
                .keys()
                .filter(|id| !self.connections.contains_key(*id))
                .count(),
            source_players,
            source_entities,
            source_waypoints,
            source_battle_chunks,
            battle_cache_chunks,
            battle_cache_value_entries,
            battle_cache_dimension_entries,
            battle_cache_change_batches,
            battle_cache_revisions,
            battle_cache_evictions,
            battle_cache_full_expansions,
            battle_cache_patch_builds,
            source_last_seen_players,
            source_tab_players,
            room_frames: self.rooms.len(),
            room_snapshot_objects,
            room_web_snapshot_objects,
            scoped_selections: self.scoped_selections.len(),
            scoped_selection_entries,
            player_marks: self.player_marks.len(),
            refresh_entries: self.last_refresh_sent.len(),
            disconnected_external_records: self.disconnected_external_sources.len(),
            relay_queue_remaining: 0,
            relay_queue_capacity: EVENT_CAPACITY,
            broadcast_hz: self.broadcast_hz,
            events_total: counters.events_total,
            player_reports: counters.player_reports,
            delivered_events: counters.delivered_events,
            ticks_total: counters.ticks_total,
            cleanup_nanoseconds: counters.cleanup_nanoseconds,
            broadcasts_total: counters.broadcasts_total,
            broadcast_nanoseconds: counters.broadcast_nanoseconds,
            broadcast_recipients: counters.broadcast_recipients,
            encoded_payload_bytes: counters.encoded_payload_bytes,
            payload_builds_total: counters.payload_builds_total,
            payload_cache_hits: counters.payload_cache_hits,
            digests_total: counters.digests_total,
            digest_cache_hits: counters.digest_cache_hits,
            digest_nanoseconds: counters.digest_nanoseconds,
        }
    }

    fn snapshot_json(&mut self, requested_room: Option<&str>) -> Value {
        let room = requested_room.unwrap_or("default");
        let mut snapshot = self.build_room_snapshot(room, true, None);
        snapshot.battle_chunks = self
            .battle_caches
            .get(room)
            .map_or_else(Vec::new, |cache| cache.full_entries(true));
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
                    "protocolVersion": connection.protocol.peer_current().to_string(),
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
                    "sourceId":selected_source(&selections.players.selected_sources, id),
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
                let reference = entry.r#ref.as_ref()?;
                let coord = reference.coord.as_ref()?;
                let data = entry.data.as_ref()?;
                Some(json!({
                    "ref":{
                        "dimension":reference.dimension,
                        "chunkX":coord.chunk_x,
                        "chunkZ":coord.chunk_z,
                    },
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

#[cfg(feature = "memory-debug")]
fn snapshot_object_count(snapshot: &SnapshotFull) -> usize {
    snapshot.players.len()
        + snapshot.entities.len()
        + snapshot.waypoints.len()
        + snapshot.battle_chunks.len()
        + snapshot.player_marks.len()
        + snapshot.last_seen_players.len()
        + snapshot.connections.len()
        + snapshot.tab_state.as_ref().map_or(0, |tab_state| {
            tab_state.reports.len() + tab_state.groups.len()
        })
}

#[cfg(feature = "memory-debug")]
fn scope_selection_count(selections: &ScopeSelections) -> usize {
    selections.players.selected_sources.len()
        + selections.entities.len()
        + selections.waypoints.len()
        + selections.battle_chunks.len()
        + selections.last_seen_players.len()
}

#[cfg(feature = "memory-debug")]
fn duration_nanoseconds(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
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

fn resolve_player_candidates<T: Clone>(
    candidates: HashMap<String, Vec<(&str, &Timed<T>)>>,
    selections: &mut PlayerSourceSelections,
    now: Instant,
    priority: impl Fn(&str, &str) -> i32,
    is_external: impl Fn(&str) -> bool,
) -> HashMap<String, (String, T)> {
    const GAME_TAKEOVER_DELAY: Duration = Duration::from_millis(500);
    let mut resolved = HashMap::new();
    let mut next_selected = HashMap::new();

    for (object_id, bucket) in candidates {
        let external_candidates = bucket
            .iter()
            .copied()
            .filter(|(source_id, _)| is_external(source_id))
            .collect::<Vec<_>>();
        let game_candidates = bucket
            .iter()
            .copied()
            .filter(|(source_id, _)| !is_external(source_id))
            .collect::<Vec<_>>();

        let active_game_sources = game_candidates
            .iter()
            .map(|(source_id, _)| (*source_id).to_owned())
            .collect::<HashSet<_>>();
        let seen_since = selections
            .game_seen_since
            .entry(object_id.clone())
            .or_default();
        seen_since.retain(|source_id, _| active_game_sources.contains(source_id));
        for source_id in &active_game_sources {
            seen_since.entry(source_id.clone()).or_insert(now);
        }

        let selected_source = selections.selected_sources.get(&object_id);
        let best_external = selected_source
            .filter(|source_id| is_external(source_id))
            .and_then(|selected_source| {
                external_candidates
                    .iter()
                    .copied()
                    .find(|(source_id, _)| source_id == selected_source)
            })
            .or_else(|| {
                external_candidates.iter().copied().max_by(
                    |(left_source, left_value), (right_source, right_value)| {
                        left_value
                            .received_at
                            .cmp(&right_value.received_at)
                            .then_with(|| right_source.cmp(left_source))
                    },
                )
            });
        let best_game = game_candidates.iter().copied().max_by(
            |(left_source, left_value), (right_source, right_value)| {
                priority(&object_id, left_source)
                    .cmp(&priority(&object_id, right_source))
                    .then_with(|| left_value.received_at.cmp(&right_value.received_at))
                    .then_with(|| right_source.cmp(left_source))
            },
        );
        let current_game = selected_source
            .filter(|source_id| !is_external(source_id))
            .and_then(|selected_source| {
                game_candidates
                    .iter()
                    .copied()
                    .find(|(source_id, _)| source_id == selected_source)
            });

        let chosen = match (best_external, best_game, current_game) {
            (Some(_), Some(best_game), Some(current_game)) => {
                if priority(&object_id, best_game.0) > priority(&object_id, current_game.0) {
                    best_game
                } else {
                    current_game
                }
            }
            (Some(external), Some(best_game), None) => {
                if seen_since.get(best_game.0).is_some_and(|first_seen| {
                    now.saturating_duration_since(*first_seen) >= GAME_TAKEOVER_DELAY
                }) {
                    best_game
                } else {
                    external
                }
            }
            (Some(external), None, _) => external,
            (None, Some(best_game), Some(current_game)) => {
                if priority(&object_id, best_game.0) > priority(&object_id, current_game.0) {
                    best_game
                } else {
                    current_game
                }
            }
            (None, Some(best_game), _) => best_game,
            (None, None, _) => continue,
        };

        next_selected.insert(object_id.clone(), chosen.0.to_owned());
        resolved.insert(object_id, (chosen.0.to_owned(), chosen.1.data.clone()));
    }

    selections.selected_sources = next_selected;
    selections.game_seen_since.retain(|object_id, sources| {
        selections.selected_sources.contains_key(object_id) && !sources.is_empty()
    });
    resolved
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

#[cfg(test)]
fn state_payload_for_profile(
    profile: ProtocolProfile,
    force_full: bool,
    delivered_snapshot: Option<&SnapshotFull>,
    target: &SnapshotFull,
) -> Option<wire_envelope::Payload> {
    state_payload_for_profile_with_battle(
        profile,
        force_full,
        delivered_snapshot,
        0,
        false,
        target,
        None,
    )
}

fn state_payload_for_profile_with_battle(
    profile: ProtocolProfile,
    force_full: bool,
    delivered_snapshot: Option<&SnapshotFull>,
    delivered_battle_revision: u64,
    strip_positions: bool,
    target: &SnapshotFull,
    battle_cache: Option<&BattleRoomCache>,
) -> Option<wire_envelope::Payload> {
    let battle_revision = battle_cache.map_or(0, |cache| cache.revision);
    let battle_changed = delivered_battle_revision != battle_revision;
    let battle_patch = if battle_changed {
        battle_cache.and_then(|cache| {
            cache.patch_since(
                delivered_battle_revision,
                profile.supports_battle_chunk_mode(),
                true,
            )
        })
    } else {
        Some(None)
    };
    let requires_full_battle = battle_changed && battle_patch.is_none();

    if force_full || delivered_snapshot.is_none() || requires_full_battle {
        let mut full = target.clone();
        full.battle_chunks = battle_cache.map_or_else(Vec::new, |cache| {
            cache.full_entries(profile.supports_battle_chunk_mode())
        });
        return Some(wire_envelope::Payload::SnapshotFull(full));
    }

    let mut patch = build_patch(delivered_snapshot.expect("checked"), target);
    if let Some(battle_chunks) = battle_patch.flatten() {
        patch.get_or_insert_with(Patch::default).battle_chunks = Some(battle_chunks);
    }
    if strip_positions && let Some(patch) = patch.as_mut() {
        strip_player_positions(patch, delivered_snapshot.expect("checked"));
        if patch_is_empty(patch) {
            return None;
        }
    }
    let patch = patch?;
    Some(wire_envelope::Payload::Patch(adapt_outbound_patch(
        profile, patch,
    )))
}

/// 从 patch 剥离既有玩家的逐 tick 位置字段(位置由 movement datagram 批承载),
/// 并同时剔除与基线相同的非位置字段——纯移动玩家的 upsert 会被整体丢弃,
/// 只真正变化了非位置字段(如血量)的玩家保留最小增量。
/// 新玩家(基线中不存在)保留全字段——客户端 missing-baseline 检查要求新 upsert
/// 携带 x/y/z/dimension。字段清除(clear_fields)始终保留,与位置无关。
fn strip_player_positions(patch: &mut Patch, delivered: &SnapshotFull) {
    let Some(scope) = &mut patch.players else {
        return;
    };
    scope.upsert.retain_mut(|upsert| {
        let Some(old) = delivered.players.get(&upsert.id) else {
            return true;
        };
        let new = upsert.data.take().expect("upsert carries delta");
        upsert.data = Some(PlayerDelta {
            x: None,
            y: None,
            z: None,
            vx: None,
            vy: None,
            vz: None,
            dimension: None,
            player_name: changed_field(&new.player_name, &old.player_name),
            player_uuid: changed_field(&new.player_uuid, &old.player_uuid),
            health: changed_field(&new.health, &old.health),
            max_health: changed_field(&new.max_health, &old.max_health),
            armor: changed_field(&new.armor, &old.armor),
            is_riding: changed_field(&new.is_riding, &old.is_riding),
            width: changed_field(&new.width, &old.width),
            height: changed_field(&new.height, &old.height),
            position_source_id: changed_field(&new.position_source_id, &old.position_source_id),
            position_source_kind: changed_field(
                &new.position_source_kind,
                &old.position_source_kind,
            ),
            position_source_display_name: changed_field(
                &new.position_source_display_name,
                &old.position_source_display_name,
            ),
            position_resolution: changed_field(
                &new.position_resolution,
                &old.position_resolution,
            ),
        });
        // 剥离后不再携带任何字段的 upsert 无信息量,丢弃
        upsert.data.as_ref().is_some_and(|delta| *delta != PlayerDelta::default())
            || !upsert.clear_fields.is_empty()
    });
    if scope.upsert.is_empty() && scope.delete.is_empty() {
        patch.players = None;
    }
}

/// 基线 diff:字段值未变化返回 None(不下发),变化则携带新值。
fn changed_field<T: PartialEq + Clone>(new: &Option<T>, old: &Option<T>) -> Option<T> {
    if new == old { None } else { new.clone() }
}

fn patch_is_empty(patch: &Patch) -> bool {
    patch.players.is_none()
        && patch.entities.is_none()
        && patch.waypoints.is_none()
        && patch.battle_chunks.is_none()
        && patch.last_seen_players.is_none()
        && patch.player_marks.is_none()
        && patch.tab_state_patch.is_none()
        && patch.connections.is_none()
}

/// 从快照编码 movement 批:全部活跃玩家的绝对值 upsert,按编码体积切成
/// 条目对齐的块。upsert 不携带 clear_fields(快照即全部字段),同 ID 的
/// 条目永不跨块,客户端按块原子应用。
///
/// 块数受 [`MOVEMENT_MAX_CHUNKS_PER_TICK`] 约束:超大花名册溢出的玩家本 tick
/// 不占 datagram,由可靠流的低频位置刷新兜底,绝对值语义保证不产生污染。
fn movement_batch(target: &SnapshotFull) -> MovementBatch {
    let mut ids: Vec<&String> = target.players.keys().collect();
    ids.sort();
    let mut chunks: Vec<Arc<[u8]>> = Vec::new();
    let mut current: Vec<PlayerUpsert> = Vec::new();
    let mut current_bytes = 0_usize;
    for id in ids {
        if chunks.len() >= MOVEMENT_MAX_CHUNKS_PER_TICK {
            break;
        }
        let value = &target.players[id.as_str()];
        let upsert = player_upsert(id, value, None);
        // +4:envelope 内 repeated 字段的 tag 与 varint 长度前缀的保守余量
        let entry_bytes = upsert.encoded_len() + 4;
        if !current.is_empty() && current_bytes + entry_bytes > MOVEMENT_CHUNK_MAX_BYTES {
            flush_movement_chunk(&mut current, &mut chunks);
            current_bytes = 0;
        }
        current_bytes += entry_bytes;
        current.push(upsert);
    }
    flush_movement_chunk(&mut current, &mut chunks);
    MovementBatch {
        chunks: chunks.into(),
    }
}

fn flush_movement_chunk(current: &mut Vec<PlayerUpsert>, chunks: &mut Vec<Arc<[u8]>>) {
    if current.is_empty() {
        return;
    }
    let patch = Patch {
        players: Some(PlayerPatchScope {
            upsert: std::mem::take(current),
            delete: Vec::new(),
        }),
        ..Default::default()
    };
    chunks.push(encode_payload(
        WireChannel::WebMap,
        wire_envelope::Payload::Patch(patch),
    ));
}

fn battle_missing_fields(value: &BattleChunkValue, include_mode: bool) -> Vec<String> {
    [
        ("symbol", value.symbol.is_none()),
        ("markerType", value.marker_type.is_none()),
        ("colorNote", value.color_note.is_none()),
        ("roomCode", value.room_code.is_none()),
        ("colorMode", value.color_mode.is_none()),
        ("colorSemanticKey", value.color_semantic_key.is_none()),
        ("mode", include_mode && value.mode.is_none()),
    ]
    .into_iter()
    .filter(|(_, missing)| *missing)
    .map(|(field, _)| field.to_owned())
    .collect()
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
) -> Vec<BattleChunkEntry> {
    let dimension = if report.dimension.trim().is_empty() {
        "minecraft:overworld".to_owned()
    } else {
        report.dimension.trim().to_owned()
    };
    let mode = match report.mode.as_deref().map(str::trim) {
        Some("simmc") => "simmc",
        _ => "generic",
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
        return Vec::new();
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
        return Vec::new();
    };

    if let Some(previous) = source.battle_projection.take() {
        for key in previous.chunk_ids {
            source.battle_chunks.remove(&key);
        }
    }
    let mut chunk_ids = Vec::with_capacity(cells.len());
    let mut cache_entries = Vec::with_capacity(cells.len());
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
        let entry = BattleChunkEntry {
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
        };
        source
            .battle_chunks
            .insert(key.clone(), Timed::new(entry.clone(), received_at));
        cache_entries.push(entry);
        chunk_ids.push(key);
    }
    source.battle_projection = Some(BattleProjection {
        dimension,
        base_chunk_x: chosen.base_chunk_x,
        base_chunk_z: chosen.base_chunk_z,
        snapshot_observed_at: report.snapshot_observed_at,
        chunk_ids,
    });
    cache_entries
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

fn snapshot_digest_with_battle_digest(
    snapshot: &SnapshotFull,
    battle_digest: String,
    profile: ProtocolProfile,
) -> Digest {
    let players = snapshot
        .players
        .iter()
        .map(|(id, player)| {
            let mut value = player_json(player);
            if !profile.supports_player_source_metadata()
                && let Some(object) = value.as_object_mut()
            {
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
    let last_seen_players = snapshot
        .last_seen_players
        .iter()
        .map(|(id, player)| (id.clone(), last_seen_json(player)))
        .collect();
    Digest {
        players: state_digest_plain(players),
        entities: state_digest_plain(entities),
        waypoints: state_digest_plain(waypoints),
        battle_chunks: Some(battle_digest),
        last_seen_players: profile
            .supports_last_seen_players()
            .then(|| state_digest_plain(last_seen_players)),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BattleChunkDigestContract {
    LegacyKeyed,
    StructuredV2,
}

fn battle_chunk_digest_contract(profile: ProtocolProfile) -> BattleChunkDigestContract {
    if profile.uses_structured_battle_chunk_digest() {
        BattleChunkDigestContract::StructuredV2
    } else {
        BattleChunkDigestContract::LegacyKeyed
    }
}

fn battle_chunk_digest(
    entries: &[BattleChunkEntry],
    contract: BattleChunkDigestContract,
    include_mode: bool,
) -> String {
    match contract {
        BattleChunkDigestContract::LegacyKeyed => {
            let mut values = entries
                .iter()
                .filter_map(|entry| {
                    let reference = entry.r#ref.as_ref()?;
                    Some((battle_ref_key(reference)?, reference, entry.data.as_ref()?))
                })
                .collect::<Vec<_>>();
            values.sort_by(|left, right| left.0.cmp(&right.0));
            let mut writer = Sha1Writer::default();
            for (key, reference, value) in values {
                serde_json::to_writer(&mut writer, &key).expect("hash writer cannot fail");
                writer.update(b":");
                write_battle_digest_data(&mut writer, value, include_mode, Some(reference));
                writer.update(b"\n");
            }
            writer.finish_short_hex()
        }
        BattleChunkDigestContract::StructuredV2 => {
            let mut values = entries
                .iter()
                .filter_map(|entry| {
                    let reference = entry.r#ref.as_ref()?;
                    let coord = reference.coord.as_ref()?;
                    let dimension = reference.dimension.trim();
                    if dimension.is_empty() {
                        return None;
                    }
                    Some((
                        (dimension.to_owned(), coord.chunk_x, coord.chunk_z),
                        reference,
                        entry.data.as_ref()?,
                    ))
                })
                .collect::<Vec<_>>();
            values.sort_by(|left, right| left.0.cmp(&right.0));
            let mut writer = Sha1Writer::default();
            for (_, reference, value) in values {
                let coord = reference.coord.as_ref().expect("validated coordinate");
                writer.update(b"{\"data\":");
                write_battle_digest_data(&mut writer, value, include_mode, None);
                writer.update(b",\"ref\":{\"chunkX\":");
                serde_json::to_writer(&mut writer, &coord.chunk_x)
                    .expect("hash writer cannot fail");
                writer.update(b",\"chunkZ\":");
                serde_json::to_writer(&mut writer, &coord.chunk_z)
                    .expect("hash writer cannot fail");
                writer.update(b",\"dimension\":");
                serde_json::to_writer(&mut writer, reference.dimension.trim())
                    .expect("hash writer cannot fail");
                writer.update(b"}}\n");
            }
            writer.finish_short_hex()
        }
    }
}

fn battle_cache_digest(
    cache: &BattleRoomCache,
    contract: BattleChunkDigestContract,
    include_mode: bool,
) -> String {
    let mut writer = Sha1Writer::default();
    match contract {
        BattleChunkDigestContract::LegacyKeyed => {
            let mut entries = cache
                .entries
                .iter()
                .map(|(key, value)| {
                    (
                        battle_key(&key.dimension, key.chunk_x, key.chunk_z),
                        key,
                        value,
                    )
                })
                .collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            for (wire_key, key, entry) in entries {
                serde_json::to_writer(&mut writer, &wire_key).expect("hash writer cannot fail");
                writer.update(b":");
                let reference = key.to_ref();
                write_battle_digest_data(
                    &mut writer,
                    entry.value.as_ref(),
                    include_mode,
                    Some(&reference),
                );
                writer.update(b"\n");
            }
        }
        BattleChunkDigestContract::StructuredV2 => {
            for (key, entry) in &cache.entries {
                writer.update(b"{\"data\":");
                write_battle_digest_data(&mut writer, entry.value.as_ref(), include_mode, None);
                writer.update(b",\"ref\":{\"chunkX\":");
                serde_json::to_writer(&mut writer, &key.chunk_x).expect("hash writer cannot fail");
                writer.update(b",\"chunkZ\":");
                serde_json::to_writer(&mut writer, &key.chunk_z).expect("hash writer cannot fail");
                writer.update(b",\"dimension\":");
                serde_json::to_writer(&mut writer, key.dimension.as_ref())
                    .expect("hash writer cannot fail");
                writer.update(b"}}\n");
            }
        }
    }
    writer.finish_short_hex()
}

fn write_battle_digest_data(
    writer: &mut Sha1Writer,
    value: &BattleChunkValue,
    include_mode: bool,
    legacy_ref: Option<&BattleChunkRef>,
) {
    let mut first = true;
    writer.update(b"{");
    if let Some(reference) = legacy_ref {
        let coord = reference.coord.as_ref().expect("validated coordinate");
        write_i64_field(writer, &mut first, "chunkX", i64::from(coord.chunk_x));
        write_i64_field(writer, &mut first, "chunkZ", i64::from(coord.chunk_z));
    }
    write_string_field(
        writer,
        &mut first,
        "colorMode",
        value.color_mode.as_deref().unwrap_or("raw_observed"),
    );
    write_optional_string_field(writer, &mut first, "colorNote", &value.color_note);
    write_string_field(writer, &mut first, "colorRaw", &value.color_raw);
    write_optional_string_field(
        writer,
        &mut first,
        "colorSemanticKey",
        &value.color_semantic_key,
    );
    if let Some(reference) = legacy_ref {
        write_string_field(writer, &mut first, "dimension", reference.dimension.trim());
    }
    write_optional_string_field(writer, &mut first, "markerType", &value.marker_type);
    if include_mode {
        write_optional_string_field(writer, &mut first, "mode", &value.mode);
    }
    write_optional_string_field(writer, &mut first, "roomCode", &value.room_code);
    write_optional_string_field(writer, &mut first, "symbol", &value.symbol);
    writer.update(b"}");
}

fn write_field_name(writer: &mut Sha1Writer, first: &mut bool, name: &str) {
    if !*first {
        writer.update(b",");
    }
    *first = false;
    serde_json::to_writer(&mut *writer, name).expect("hash writer cannot fail");
    writer.update(b":");
}

fn write_string_field(writer: &mut Sha1Writer, first: &mut bool, name: &str, value: &str) {
    write_field_name(writer, first, name);
    serde_json::to_writer(writer, value).expect("hash writer cannot fail");
}

fn write_optional_string_field(
    writer: &mut Sha1Writer,
    first: &mut bool,
    name: &str,
    value: &Option<String>,
) {
    if let Some(value) = value {
        write_string_field(writer, first, name, value);
    }
}

fn write_i64_field(writer: &mut Sha1Writer, first: &mut bool, name: &str, value: i64) {
    write_field_name(writer, first, name);
    serde_json::to_writer(writer, &value).expect("hash writer cannot fail");
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

#[cfg(test)]
fn battle_digest_data_json(value: &BattleChunkValue, include_mode: bool) -> Value {
    let mut value = battle_value_json(value);
    if let Some(object) = value.as_object_mut() {
        if !include_mode {
            object.remove("mode");
        }
        object
            .entry("colorMode".to_owned())
            .or_insert_with(|| Value::String("raw_observed".to_owned()));
    }
    value
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
    let mut writer = Sha1Writer::default();
    for (id, value) in values {
        serde_json::to_writer(&mut writer, &id).expect("hash writer cannot fail");
        writer.update(b":");
        write_canonical_value(&mut writer, &value);
        writer.update(b"\n");
    }
    writer.finish_short_hex()
}

#[derive(Default)]
struct Sha1Writer(Sha1);

impl Sha1Writer {
    fn update(&mut self, bytes: &[u8]) {
        Sha1Digest::update(&mut self.0, bytes);
    }

    fn finish_short_hex(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let digest = self.0.finalize();
        let mut encoded = [0_u8; 16];
        for (index, byte) in digest[..8].iter().copied().enumerate() {
            encoded[index * 2] = HEX[(byte >> 4) as usize];
            encoded[index * 2 + 1] = HEX[(byte & 0x0f) as usize];
        }
        String::from_utf8(encoded.to_vec()).expect("hex is valid UTF-8")
    }
}

impl std::io::Write for Sha1Writer {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.update(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn write_canonical_value(writer: &mut Sha1Writer, value: &Value) {
    match value {
        Value::Null => writer.update(b"null"),
        Value::Bool(true) => writer.update(b"true"),
        Value::Bool(false) => writer.update(b"false"),
        Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                serde_json::to_writer(writer, &value).expect("hash writer cannot fail");
            } else if let Some(value) = value.as_u64() {
                serde_json::to_writer(writer, &value).expect("hash writer cannot fail");
            } else {
                writer.update(canonical_number(value.as_f64().unwrap_or(f64::NAN)).as_bytes());
            }
        }
        Value::String(value) => {
            serde_json::to_writer(writer, value).expect("hash writer cannot fail");
        }
        Value::Array(values) => {
            writer.update(b"[");
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    writer.update(b",");
                }
                write_canonical_value(writer, value);
            }
            writer.update(b"]");
        }
        Value::Object(values) => {
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort();
            writer.update(b"{");
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    writer.update(b",");
                }
                serde_json::to_writer(&mut *writer, key).expect("hash writer cannot fail");
                writer.update(b":");
                write_canonical_value(writer, &values[key]);
            }
            writer.update(b"}");
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

    fn protocol_profile(version: &str) -> ProtocolProfile {
        ProtocolProfile::negotiate(version, "0.6.1").expect("supported protocol")
    }

    fn battle_chunk_fixture(dimension: &str, chunk_x: i32, chunk_z: i32) -> BattleChunkEntry {
        BattleChunkEntry {
            r#ref: Some(BattleChunkRef {
                dimension: dimension.to_owned(),
                coord: Some(crate::proto::teamviewer::v1::BattleChunkCoord { chunk_x, chunk_z }),
            }),
            data: Some(BattleChunkValue {
                color_raw: "#112233".to_owned(),
                color_mode: Some("raw_observed".to_owned()),
                mode: Some("simmc".to_owned()),
                room_code: Some("default".to_owned()),
                ..Default::default()
            }),
        }
    }

    fn reference_battle_digest(
        entries: &[BattleChunkEntry],
        contract: BattleChunkDigestContract,
        include_mode: bool,
    ) -> String {
        match contract {
            BattleChunkDigestContract::LegacyKeyed => {
                let values = entries
                    .iter()
                    .filter_map(|entry| {
                        let reference = entry.r#ref.as_ref()?;
                        let coord = reference.coord.as_ref()?;
                        let mut value = battle_digest_data_json(entry.data.as_ref()?, include_mode);
                        let object = value.as_object_mut()?;
                        object.insert(
                            "dimension".to_owned(),
                            Value::String(reference.dimension.trim().to_owned()),
                        );
                        object.insert("chunkX".to_owned(), Value::from(coord.chunk_x));
                        object.insert("chunkZ".to_owned(), Value::from(coord.chunk_z));
                        Some((battle_ref_key(reference)?, value))
                    })
                    .collect();
                state_digest_plain(values)
            }
            BattleChunkDigestContract::StructuredV2 => {
                let mut values = entries
                    .iter()
                    .filter_map(|entry| {
                        let reference = entry.r#ref.as_ref()?;
                        let coord = reference.coord.as_ref()?;
                        let dimension = reference.dimension.trim();
                        Some((
                            (dimension.to_owned(), coord.chunk_x, coord.chunk_z),
                            json!({
                                "ref": {
                                    "dimension": dimension,
                                    "chunkX": coord.chunk_x,
                                    "chunkZ": coord.chunk_z,
                                },
                                "data": battle_digest_data_json(entry.data.as_ref()?, include_mode),
                            }),
                        ))
                    })
                    .collect::<Vec<_>>();
                values.sort_by(|left, right| left.0.cmp(&right.0));
                let mut writer = Sha1Writer::default();
                for (_, value) in values {
                    write_canonical_value(&mut writer, &value);
                    writer.update(b"\n");
                }
                writer.finish_short_hex()
            }
        }
    }

    fn add_test_connection(relay: &mut Relay, id: &str, kind: ConnectionKind) {
        let (control, _) = mpsc::channel(1);
        let (state, _) = watch::channel(None);
        relay.connections.insert(
            id.to_owned(),
            Connection {
                room: "room".to_owned(),
                protocol: protocol_profile("0.7.0"),
                kind,
                display_name: None,
                position_resolution: None,
                program_version: "test".to_owned(),
                remote_addr: "127.0.0.1".to_owned(),
                connected_at: 0,
                control,
                state,
                delivered_revision: 0,
                delivered_snapshot: None,
                delivered_battle_revision: 0,
                next_digest_at: Instant::now(),
                force_full: true,
                movement: watch::channel(None).0,
                unreliable_positions: false,
                last_position_refresh: Instant::now(),
                tab_history_subscribed: false,
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
    fn legacy_clear_fields_rebuild_only_the_changed_object() {
        let mut old = SnapshotFull::default();
        old.players.insert(
            "player".into(),
            PlayerData {
                player_name: Some("Alice".into()),
                ..Default::default()
            },
        );
        let mut new = old.clone();
        new.players.get_mut("player").expect("player").player_name = None;

        let Some(wire_envelope::Payload::Patch(legacy)) =
            state_payload_for_profile(protocol_profile("0.6.4"), false, Some(&old), &new)
        else {
            panic!("expected legacy object rebuild patch");
        };
        let legacy_players = legacy.players.expect("players");
        assert_eq!(legacy_players.delete, ["player"]);
        assert!(legacy_players.upsert[0].clear_fields.is_empty());
        assert!(matches!(
            state_payload_for_profile(protocol_profile("0.6.5"), false, Some(&old), &new,),
            Some(wire_envelope::Payload::Patch(_))
        ));
    }

    #[test]
    fn delivered_event_carries_the_exact_written_snapshot() {
        let config = Arc::new(RuntimeConfig::load());
        let mut relay = Relay::new(config, Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        add_test_connection(&mut relay, "player", ConnectionKind::Player);
        let written = Arc::new(SnapshotFull {
            server_time: Some(2.0),
            ..Default::default()
        });

        relay.handle_event(RelayEvent::Delivered {
            id: "player".to_owned(),
            revision: 2,
            snapshot: written.clone(),
            battle_revision: 7,
        });

        let connection = relay.connections.get("player").expect("connection");
        assert_eq!(connection.delivered_revision, 2);
        assert_eq!(connection.delivered_battle_revision, 7);
        assert!(Arc::ptr_eq(
            connection.delivered_snapshot.as_ref().expect("snapshot"),
            &written
        ));

        relay.handle_event(RelayEvent::Delivered {
            id: "player".to_owned(),
            revision: 1,
            snapshot: Arc::new(SnapshotFull {
                server_time: Some(1.0),
                ..Default::default()
            }),
            battle_revision: 3,
        });
        let connection = relay.connections.get("player").expect("connection");
        assert_eq!(connection.delivered_revision, 2);
        assert!(Arc::ptr_eq(
            connection.delivered_snapshot.as_ref().expect("snapshot"),
            &written
        ));
    }

    #[test]
    fn broadcast_reuses_payload_digest_and_snapshot_for_equivalent_recipients() {
        let config = Arc::new(RuntimeConfig::load());
        let mut relay = Relay::new(config, Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        add_test_connection(&mut relay, "first", ConnectionKind::Player);
        add_test_connection(&mut relay, "second", ConnectionKind::Player);

        relay.broadcast(Instant::now(), true);

        let first = relay
            .connections
            .get("first")
            .expect("first connection")
            .state
            .subscribe()
            .borrow_and_update()
            .clone()
            .expect("first frame");
        let second = relay
            .connections
            .get("second")
            .expect("second connection")
            .state
            .subscribe()
            .borrow_and_update()
            .clone()
            .expect("second frame");

        assert!(Arc::ptr_eq(
            first.bytes.as_ref().expect("first payload"),
            second.bytes.as_ref().expect("second payload")
        ));
        assert!(Arc::ptr_eq(
            first.digest.as_ref().expect("first digest"),
            second.digest.as_ref().expect("second digest")
        ));
        assert!(Arc::ptr_eq(&first.snapshot, &second.snapshot));
        #[cfg(feature = "memory-debug")]
        {
            assert_eq!(relay.debug_counters.payload_builds_total, 1);
            assert_eq!(relay.debug_counters.payload_cache_hits, 1);
            assert_eq!(relay.debug_counters.digests_total, 1);
            assert_eq!(relay.debug_counters.digest_cache_hits, 1);
        }
    }

    #[test]
    fn canonical_digest_matches_protocol_contract() {
        let values = BTreeMap::from([("<id>&".to_owned(), json!({"name":"<A&B>", "x":1.2345645}))]);
        assert_eq!(canonical_number(1.2345645), "1.234565");
        assert_eq!(state_digest_plain(values), "0cabc8c9afc26756");
    }

    #[test]
    fn battle_chunk_digests_match_protocol_vectors() {
        let entry = battle_chunk_fixture("minecraft:overworld", 1, 2);
        assert_eq!(
            battle_chunk_digest(
                std::slice::from_ref(&entry),
                BattleChunkDigestContract::LegacyKeyed,
                true,
            ),
            "d6fccb4a1bd18438"
        );
        assert_eq!(
            battle_chunk_digest(
                std::slice::from_ref(&entry),
                BattleChunkDigestContract::StructuredV2,
                true,
            ),
            "31c63cd6e92bbc39"
        );

        let buggy_projection = BTreeMap::from([(
            battle_entry_key(&entry).expect("valid ref"),
            battle_digest_data_json(entry.data.as_ref().expect("data"), true),
        )]);
        assert_eq!(state_digest_plain(buggy_projection), "9cecf13aa4592a1c");

        let mut rich = entry;
        let value = rich.data.as_mut().expect("data");
        value.symbol = Some("<A&B>".to_owned());
        value.marker_type = Some("war_core".to_owned());
        value.color_note = Some("note".to_owned());
        value.color_semantic_key = Some("enemy".to_owned());
        value.observed_at = Some(123);
        value.position_sampled_at = Some(122);
        value.alignment_source = Some("history_primary".to_owned());
        value.reporter_id = Some("reporter".to_owned());
        for contract in [
            BattleChunkDigestContract::LegacyKeyed,
            BattleChunkDigestContract::StructuredV2,
        ] {
            assert_eq!(
                battle_chunk_digest(std::slice::from_ref(&rich), contract, true),
                reference_battle_digest(std::slice::from_ref(&rich), contract, true),
            );
        }
    }

    #[test]
    fn battle_chunk_digest_contract_is_versioned_and_order_stable() {
        assert_eq!(
            battle_chunk_digest_contract(protocol_profile("0.7.0")),
            BattleChunkDigestContract::LegacyKeyed
        );
        assert_eq!(
            battle_chunk_digest_contract(protocol_profile("0.7.1")),
            BattleChunkDigestContract::StructuredV2
        );
        assert_eq!(
            battle_chunk_digest(&[], BattleChunkDigestContract::LegacyKeyed, true),
            "da39a3ee5e6b4b0d"
        );
        assert_eq!(
            battle_chunk_digest(&[], BattleChunkDigestContract::StructuredV2, true),
            "da39a3ee5e6b4b0d"
        );

        let first = battle_chunk_fixture("minecraft:the_nether", -12, 4);
        let second = battle_chunk_fixture("minecraft:overworld", 12, -4);
        let forward = battle_chunk_digest(
            &[first.clone(), second.clone()],
            BattleChunkDigestContract::StructuredV2,
            true,
        );
        let reverse = battle_chunk_digest(
            &[second, first],
            BattleChunkDigestContract::StructuredV2,
            true,
        );
        assert_eq!(forward, reverse);
    }

    #[test]
    fn protocol_0_6_1_battle_digest_omits_mode() {
        let with_mode = battle_chunk_fixture("minecraft:overworld", 1, 2);
        let mut without_mode = with_mode.clone();
        without_mode.data.as_mut().expect("data").mode = None;
        assert_eq!(
            battle_chunk_digest(&[with_mode], BattleChunkDigestContract::LegacyKeyed, false,),
            battle_chunk_digest(
                &[without_mode],
                BattleChunkDigestContract::LegacyKeyed,
                false,
            )
        );
    }

    #[test]
    fn battle_cache_is_bounded_and_losslessly_interns_values() {
        let now = Instant::now();
        let mut cache = BattleRoomCache::default();
        let entries = (0..3)
            .map(|chunk_x| battle_chunk_fixture("minecraft:overworld", chunk_x, 0))
            .collect();

        assert!(cache.upsert(entries, now, 2));
        assert_eq!(cache.entries.len(), 2);
        assert_eq!(cache.values.len(), 1);
        let expanded = cache.full_entries(true);
        assert_eq!(
            expanded
                .iter()
                .filter_map(battle_entry_key)
                .collect::<Vec<_>>(),
            vec![
                "minecraft:overworld|1|0".to_owned(),
                "minecraft:overworld|2|0".to_owned(),
            ]
        );
        assert!(
            expanded
                .iter()
                .all(|entry| entry.data.as_ref().expect("data").color_raw == "#112233")
        );
        for contract in [
            BattleChunkDigestContract::LegacyKeyed,
            BattleChunkDigestContract::StructuredV2,
        ] {
            assert_eq!(
                cache.digest(contract, true),
                battle_chunk_digest(&expanded, contract, true),
            );
        }
    }

    #[test]
    fn battle_cache_expiry_does_not_remove_a_newer_overwrite() {
        let now = Instant::now();
        let mut cache = BattleRoomCache::default();
        let first = battle_chunk_fixture("minecraft:overworld", 1, 2);
        cache.upsert(vec![first], now, 10);
        let mut second = battle_chunk_fixture("minecraft:overworld", 1, 2);
        second.data.as_mut().expect("data").color_raw = "#abcdef".to_owned();
        cache.upsert(vec![second], now + Duration::from_secs(10), 10);

        cache.cleanup(now + Duration::from_secs(121), Duration::from_secs(120), 10);
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(
            cache
                .full_entries(true)
                .pop()
                .expect("entry")
                .data
                .expect("data")
                .color_raw,
            "#abcdef"
        );

        cache.cleanup(now + Duration::from_secs(131), Duration::from_secs(120), 10);
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn battle_payload_uses_delta_after_the_initial_snapshot() {
        let mut cache = BattleRoomCache::default();
        cache.upsert(
            vec![battle_chunk_fixture("minecraft:overworld", 1, 2)],
            Instant::now(),
            10,
        );
        let delivered = SnapshotFull::default();
        let target = SnapshotFull::default();
        let profile = protocol_profile("0.7.1");

        let payload = state_payload_for_profile_with_battle(
            profile,
            false,
            Some(&delivered),
            0,
            false,
            &target,
            Some(&cache),
        )
        .expect("battle patch");
        let wire_envelope::Payload::Patch(patch) = payload else {
            panic!("expected patch");
        };
        assert_eq!(patch.battle_chunks.expect("battle scope").upsert.len(), 1);

        assert!(
            state_payload_for_profile_with_battle(
                profile,
                false,
                Some(&delivered),
                cache.revision,
                false,
                &target,
                Some(&cache),
            )
            .is_none()
        );

        let legacy_profile = protocol_profile("0.6.4");
        assert!(matches!(
            state_payload_for_profile_with_battle(
                legacy_profile,
                false,
                Some(&delivered),
                0,
                false,
                &target,
                Some(&cache),
            ),
            Some(wire_envelope::Payload::Patch(_))
        ));

        let delivered_revision = cache.revision;
        let mut cleared = battle_chunk_fixture("minecraft:overworld", 1, 2);
        cleared.data.as_mut().expect("data").mode = None;
        cache.upsert(vec![cleared], Instant::now(), 10);
        let Some(wire_envelope::Payload::Patch(legacy_patch)) =
            state_payload_for_profile_with_battle(
                legacy_profile,
                false,
                Some(&delivered),
                delivered_revision,
                false,
                &target,
                Some(&cache),
            )
        else {
            panic!("expected legacy battle object rebuild patch");
        };
        let battle_scope = legacy_patch.battle_chunks.expect("battle scope");
        assert_eq!(battle_scope.delete.len(), 1);
        assert!(battle_scope.upsert[0].clear_fields.is_empty());
    }

    #[test]
    fn player_patch_does_not_expand_a_full_battle_cache() {
        let mut cache = BattleRoomCache::default();
        let now = Instant::now();
        for start in (0..65_536).step_by(256) {
            cache.upsert(
                (start..start + 256)
                    .map(|chunk_x| battle_chunk_fixture("minecraft:overworld", chunk_x, 0))
                    .collect(),
                now,
                65_536,
            );
        }
        assert_eq!(cache.entries.len(), 65_536);
        assert_eq!(cache.values.len(), 1);

        let mut delivered = SnapshotFull::default();
        delivered.players.insert(
            "player".to_owned(),
            PlayerData {
                player_name: Some("Alice".to_owned()),
                ..Default::default()
            },
        );
        let mut target = delivered.clone();
        let target_player = target.players.get_mut("player").expect("player");
        target_player.x = 1.0;
        target_player.player_name = None;
        let expansions = cache.full_expansions_total.get();
        let payload = state_payload_for_profile_with_battle(
            protocol_profile("0.7.1"),
            false,
            Some(&delivered),
            cache.revision,
            false,
            &target,
            Some(&cache),
        );

        let Some(wire_envelope::Payload::Patch(patch)) = payload else {
            panic!("expected player patch");
        };
        assert!(patch.battle_chunks.is_none());
        assert_eq!(cache.full_expansions_total.get(), expansions);

        let Some(wire_envelope::Payload::Patch(legacy_patch)) =
            state_payload_for_profile_with_battle(
                protocol_profile("0.6.4"),
                false,
                Some(&delivered),
                cache.revision,
                false,
                &target,
                Some(&cache),
            )
        else {
            panic!("expected legacy object rebuild patch");
        };
        assert!(legacy_patch.battle_chunks.is_none());
        assert_eq!(legacy_patch.players.expect("players").delete, ["player"]);
        assert_eq!(cache.full_expansions_total.get(), expansions);
        assert_ne!(
            cache.digest(BattleChunkDigestContract::StructuredV2, true),
            "da39a3ee5e6b4b0d"
        );
        assert_eq!(cache.full_expansions_total.get(), expansions);
    }

    #[test]
    fn connected_tab_report_survives_report_timeout() {
        let config = Arc::new(RuntimeConfig::load());
        let timeout = config.tab_report_timeout_sec;
        let mut relay = Relay::new(config, Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        add_test_connection(&mut relay, "source", ConnectionKind::Player);
        let received_at = Instant::now() - Duration::from_secs(timeout + 1);
        relay.sources.insert(
            "source".to_owned(),
            SourceState {
                room: "room".to_owned(),
                tab_players: vec![TabPlayerEntry {
                    name: Some("Alice".to_owned()),
                    ..Default::default()
                }],
                tab_received_at: Some(received_at),
                tab_timestamp: Some(100.0),
                ..Default::default()
            },
        );

        relay.cleanup_timeouts(Instant::now());

        let report = relay.sources.get("source").expect("source");
        assert_eq!(report.tab_players.len(), 1);
        assert_eq!(report.tab_received_at, Some(received_at));
        assert_eq!(report.tab_timestamp, Some(100.0));
    }

    #[test]
    fn unrelated_player_report_does_not_change_tab_report_time() {
        let config = Arc::new(RuntimeConfig::load());
        let mut relay = Relay::new(config, Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        add_test_connection(&mut relay, "source", ConnectionKind::Player);
        let received_at = Instant::now() - Duration::from_secs(5);
        relay.sources.insert(
            "source".to_owned(),
            SourceState {
                room: "room".to_owned(),
                tab_players: vec![TabPlayerEntry {
                    name: Some("Alice".to_owned()),
                    ..Default::default()
                }],
                tab_received_at: Some(received_at),
                tab_timestamp: Some(100.0),
                ..Default::default()
            },
        );

        relay.apply_report(
            "source",
            PlayerReportBundle {
                players_replace: Some(Default::default()),
                ..Default::default()
            },
        );

        let report = relay.sources.get("source").expect("source");
        assert_eq!(report.tab_received_at, Some(received_at));
        assert_eq!(report.tab_timestamp, Some(100.0));
    }

    #[test]
    fn explicit_clear_and_empty_report_remain_authoritative() {
        let config = Arc::new(RuntimeConfig::load());
        let mut relay = Relay::new(config, Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        add_test_connection(&mut relay, "source", ConnectionKind::Player);
        relay.sources.insert(
            "source".to_owned(),
            SourceState {
                room: "room".to_owned(),
                tab_players: vec![TabPlayerEntry::default()],
                tab_received_at: Some(Instant::now()),
                tab_timestamp: Some(100.0),
                ..Default::default()
            },
        );

        relay.apply_report(
            "source",
            PlayerReportBundle {
                source_state_clear: Some(Default::default()),
                ..Default::default()
            },
        );
        let cleared = relay.sources.get("source").expect("source");
        assert!(cleared.tab_players.is_empty());
        assert!(cleared.tab_received_at.is_none());
        assert!(cleared.tab_timestamp.is_none());

        relay.apply_report(
            "source",
            PlayerReportBundle {
                tab_players_replace: Some(Default::default()),
                ..Default::default()
            },
        );
        let empty_report = relay.sources.get("source").expect("source");
        assert!(empty_report.tab_players.is_empty());
        assert!(empty_report.tab_received_at.is_some());
        assert!(empty_report.tab_timestamp.is_some());
    }

    #[test]
    fn orphaned_tab_report_expires_and_disconnect_clears_immediately() {
        let config = Arc::new(RuntimeConfig::load());
        let timeout = config.tab_report_timeout_sec;
        let mut relay = Relay::new(config, Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        let old = Instant::now() - Duration::from_secs(timeout + 1);
        relay.sources.insert(
            "orphan".to_owned(),
            SourceState {
                room: "room".to_owned(),
                tab_players: vec![TabPlayerEntry::default()],
                tab_received_at: Some(old),
                tab_timestamp: Some(100.0),
                ..Default::default()
            },
        );
        relay.cleanup_timeouts(Instant::now());
        assert!(relay.sources["orphan"].tab_players.is_empty());
        assert!(relay.sources["orphan"].tab_received_at.is_none());

        add_test_connection(&mut relay, "connected", ConnectionKind::Player);
        relay.sources.insert(
            "connected".to_owned(),
            SourceState {
                room: "room".to_owned(),
                tab_players: vec![TabPlayerEntry::default()],
                tab_received_at: Some(Instant::now()),
                ..Default::default()
            },
        );
        relay.handle_event(RelayEvent::Disconnect {
            id: "connected".to_owned(),
        });
        assert!(!relay.sources.contains_key("connected"));
    }

    #[test]
    fn room_purge_refuses_active_room_and_preserves_unrelated_state() {
        let config = Arc::new(RuntimeConfig::load());
        let mut relay = Relay::new(config, Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        add_test_connection(&mut relay, "connected", ConnectionKind::Player);
        let mut target = SourceState {
            room: "room".to_owned(),
            ..Default::default()
        };
        target.last_seen_players.insert(
            "target-player".to_owned(),
            Timed::new(LastSeenPlayerData::default(), Instant::now()),
        );
        relay.sources.insert("target-source".to_owned(), target);
        relay.sources.insert(
            "other-source".to_owned(),
            SourceState {
                room: "other".to_owned(),
                ..Default::default()
            },
        );
        relay.rooms.insert("room".to_owned(), RoomFrame::default());
        relay.rooms.insert("other".to_owned(), RoomFrame::default());
        relay
            .disconnected_external_sources
            .push_back(json!({"roomCode":"room"}));

        let (reply, mut response) = oneshot::channel();
        relay.handle_event(RelayEvent::PurgeRoom {
            room: "room".to_owned(),
            reply,
        });
        assert_eq!(
            response
                .try_recv()
                .expect("purge response")
                .active_connections,
            1
        );
        assert!(relay.sources.contains_key("target-source"));

        relay.connections.remove("connected");
        let (reply, mut response) = oneshot::channel();
        relay.handle_event(RelayEvent::PurgeRoom {
            room: "room".to_owned(),
            reply,
        });
        let result = response.try_recv().expect("purge response");
        assert_eq!(result.last_seen_records, 1);
        assert_eq!(result.sources, 1);
        assert_eq!(result.room_frames, 1);
        assert_eq!(result.disconnected_external_sources, 1);
        assert!(!relay.sources.contains_key("target-source"));
        assert!(!relay.rooms.contains_key("room"));
        assert!(relay.sources.contains_key("other-source"));
        assert!(relay.rooms.contains_key("other"));
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
    fn player_source_uses_external_baseline_then_stable_game_and_immediate_fallback() {
        let base = Instant::now();
        let mut sources = HashMap::new();
        for (source_id, x) in [("external", 1.0), ("game", 2.0)] {
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
                    base,
                ),
            );
            sources.insert(source_id.to_owned(), source);
        }
        let mut selections = PlayerSourceSelections::default();
        let resolve = |sources: &HashMap<String, SourceState>,
                       selections: &mut PlayerSourceSelections,
                       now| {
            resolve_player_candidates(
                collect_candidates(sources, "room", None, false, |source| &source.players),
                selections,
                now,
                |_, _| 1,
                |source_id| source_id == "external",
            )
        };

        let initial = resolve(&sources, &mut selections, base);
        assert_eq!(initial["target"].0, "external");
        let pending = resolve(&sources, &mut selections, base + Duration::from_millis(499));
        assert_eq!(pending["target"].0, "external");
        let taken_over = resolve(&sources, &mut selections, base + Duration::from_millis(500));
        assert_eq!(taken_over["target"].0, "game");

        sources.remove("game");
        let fallback = resolve(&sources, &mut selections, base + Duration::from_millis(501));
        assert_eq!(fallback["target"].0, "external");
        assert!(fallback.contains_key("target"));
    }

    #[test]
    fn player_source_without_external_is_immediate_and_same_priority_is_sticky() {
        let base = Instant::now();
        let mut sources = HashMap::new();
        for (source_id, x, offset_ms) in [("source-a", 1.0, 0), ("source-b", 2.0, 100)] {
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
        let mut selections = PlayerSourceSelections::default();
        let initial = resolve_player_candidates(
            collect_candidates(&sources, "room", None, false, |source| &source.players),
            &mut selections,
            base,
            |_, _| 1,
            |_| false,
        );
        assert_eq!(initial["target"].0, "source-b");

        sources
            .get_mut("source-a")
            .unwrap()
            .players
            .get_mut("target")
            .unwrap()
            .received_at = base + Duration::from_secs(1);
        let sticky = resolve_player_candidates(
            collect_candidates(&sources, "room", None, false, |source| &source.players),
            &mut selections,
            base + Duration::from_secs(1),
            |_, _| 1,
            |_| false,
        );
        assert_eq!(sticky["target"].0, "source-b");
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

    fn add_player(
        players: &mut HashMap<String, PlayerData>,
        id: &str,
        name: &str,
        x: f64,
        health: f64,
    ) {
        players.insert(
            id.to_owned(),
            PlayerData {
                x,
                y: 64.0,
                z: -x,
                vx: Some(0.5),
                vy: Some(0.0),
                vz: Some(-0.5),
                dimension: "minecraft:overworld".to_owned(),
                player_name: Some(name.to_owned()),
                player_uuid: Some(format!("uuid-{id}")),
                health: Some(health),
                max_health: Some(20.0),
                ..Default::default()
            },
        );
    }

    /// 剥离规则:纯移动的既有玩家 upsert 整体丢弃;仅非位置字段变化的保留
    /// 最小增量(无位置字段);新玩家保留全字段(missing-baseline 检查依赖)。
    #[test]
    fn strip_player_positions_keeps_minimal_delta_and_new_players() {
        let mut delivered = SnapshotFull::default();
        add_player(&mut delivered.players, "moved", "Moved", 1.0, 20.0);
        add_player(&mut delivered.players, "hurt", "Hurt", 2.0, 20.0);
        let mut target = delivered.clone();
        target.players.get_mut("moved").expect("moved").x = 5.0;
        target.players.get_mut("hurt").expect("hurt").health = Some(9.0);
        add_player(&mut target.players, "newcomer", "New", 3.0, 20.0);

        let mut patch = build_patch(&delivered, &target).expect("changed players");
        strip_player_positions(&mut patch, &delivered);

        let scope = patch.players.expect("players scope");
        let upserts: HashMap<&str, &PlayerUpsert> = scope
            .upsert
            .iter()
            .map(|upsert| (upsert.id.as_str(), upsert))
            .collect();
        // 纯位置变化:整体丢弃
        assert!(!upserts.contains_key("moved"));
        // 血量变化:保留最小增量,无任何位置字段
        let hurt = upserts["hurt"].data.as_ref().expect("delta");
        assert_eq!(hurt.health, Some(9.0));
        assert_eq!(hurt.x, None);
        assert_eq!(hurt.dimension, None);
        assert_eq!(upserts["hurt"].clear_fields, Vec::<String>::new());
        // 新玩家:保留全字段
        let newcomer = upserts["newcomer"].data.as_ref().expect("delta");
        assert_eq!(newcomer.x, Some(3.0));
        assert_eq!(
            newcomer.dimension.as_deref(),
            Some("minecraft:overworld")
        );
        assert_eq!(scope.delete, Vec::<String>::new());
    }

    /// 位置字段被清除(字段消失而非值变化)同样走保留路径:clear_fields 表达删除。
    #[test]
    fn strip_player_positions_preserves_clear_fields() {
        let mut delivered = SnapshotFull::default();
        add_player(&mut delivered.players, "p", "P", 1.0, 20.0);
        delivered
            .players
            .get_mut("p")
            .expect("player")
            .position_source_id = Some("source-1".to_owned());
        let mut target = delivered.clone();
        let player = target.players.get_mut("p").expect("player");
        player.position_source_id = None;
        player.x = 9.0;

        let mut patch = build_patch(&delivered, &target).expect("changed player");
        strip_player_positions(&mut patch, &delivered);

        let scope = patch.players.expect("players scope");
        assert_eq!(scope.upsert.len(), 1);
        let delta = scope.upsert[0].data.as_ref().expect("delta");
        assert_eq!(delta.x, None);
        assert_eq!(scope.upsert[0].clear_fields, ["positionSourceId"]);
    }

    /// movement 批:绝对值 upsert、条目对齐切块、不超过单 tick 块数上限。
    #[test]
    fn movement_batch_packs_entry_aligned_chunks_within_budget() {
        let mut target = SnapshotFull::default();
        for index in 0..36 {
            let name = format!("{:0>180}", index); // 拉大单条编码体积,强制多块
            add_player(
                &mut target.players,
                &format!("p{index}"),
                &name,
                index as f64,
                20.0,
            );
        }

        let batch = movement_batch(&target);
        assert!(!batch.chunks.is_empty());
        assert!(batch.chunks.len() <= MOVEMENT_MAX_CHUNKS_PER_TICK);

        let mut seen_ids = HashSet::new();
        for chunk in batch.chunks.iter() {
            assert!(chunk.len() <= MOVEMENT_CHUNK_MAX_BYTES + 64);
            let envelope = WireEnvelope::decode(chunk.as_ref()).expect("envelope");
            assert_eq!(envelope.channel, WireChannel::WebMap as i32);
            let wire_envelope::Payload::Patch(patch) = envelope.payload.expect("payload") else {
                panic!("movement chunk carries a patch");
            };
            let scope = patch.players.expect("players scope");
            assert!(scope.delete.is_empty());
            for upsert in scope.upsert {
                assert!(seen_ids.insert(upsert.id), "player split across chunks");
                let delta = upsert.data.as_ref().expect("absolute delta");
                // 绝对值:坐标与维度始终在场,客户端丢帧也不会污染状态
                assert!(delta.x.is_some() && delta.dimension.is_some());
            }
        }
        // 预算上限只在本 tick 截断,不丢人:花名册内的玩家要么本 tick 出现,
        // 要么下 tick 重发(绝对值语义),这里 36 人应全部命中
        assert_eq!(seen_ids.len(), target.players.len());
    }
}
