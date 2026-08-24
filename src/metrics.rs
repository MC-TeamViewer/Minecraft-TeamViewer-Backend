use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(feature = "memory-debug")]
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

const WINDOW_SECONDS: i64 = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrafficChannel {
    Player,
    WebMap,
}

#[derive(Clone, Copy)]
pub enum Direction {
    Ingress,
    Egress,
}

#[derive(Clone, Copy)]
pub enum Layer {
    Application,
    Wire,
}

#[derive(Default)]
struct TrafficBucket {
    second: i64,
    application: [u64; 4],
    wire: [u64; 4],
    protobuf: HashMap<(String, String), (u64, u64)>,
}

#[derive(Clone, Default)]
struct PacketTotals {
    messages: u64,
    bytes: u64,
    max_bytes: u64,
    last_sent_at: Option<f64>,
}

#[derive(Clone)]
struct ConnectionInfo {
    channel: TrafficChannel,
    by_type: HashMap<String, PacketTotals>,
}

#[derive(Default)]
struct MetricsState {
    buckets: VecDeque<TrafficBucket>,
    by_type: HashMap<String, PacketTotals>,
    connections: HashMap<String, ConnectionInfo>,
    pending: HashMap<(i64, u8, u8, u8), u64>,
}

pub struct TrafficIncrement {
    pub second: i64,
    pub layer: Layer,
    pub channel: TrafficChannel,
    pub direction: Direction,
    pub bytes: u64,
}

#[derive(Default)]
pub struct Metrics {
    state: Mutex<MetricsState>,
    #[cfg(feature = "memory-debug")]
    writer_send_count: AtomicU64,
    #[cfg(feature = "memory-debug")]
    writer_send_nanoseconds: AtomicU64,
    #[cfg(feature = "memory-debug")]
    writer_send_failures: AtomicU64,
}

#[cfg(feature = "memory-debug")]
#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsDebugStats {
    pub connections: usize,
    pub pending_buckets: usize,
    pub live_application_bytes: [u64; 4],
    pub live_wire_bytes: [u64; 4],
    pub protobuf_messages_total: u64,
    pub protobuf_bytes_total: u64,
    pub protobuf_by_type: BTreeMap<String, [u64; 2]>,
    pub writer_send_count: u64,
    pub writer_send_nanoseconds: u64,
    pub writer_send_failures: u64,
}

impl Metrics {
    #[cfg(feature = "memory-debug")]
    pub fn record_writer_send(&self, elapsed: std::time::Duration, success: bool) {
        self.writer_send_count.fetch_add(1, Ordering::Relaxed);
        self.writer_send_nanoseconds.fetch_add(
            u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        if !success {
            self.writer_send_failures.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[cfg(feature = "memory-debug")]
    pub fn debug_stats(&self) -> MetricsDebugStats {
        let now = unix_seconds() as i64;
        let mut state = self.state.lock().expect("metrics lock poisoned");
        prune(&mut state, now);
        let mut application = [0_u64; 4];
        let mut wire = [0_u64; 4];
        for bucket in state
            .buckets
            .iter()
            .filter(|bucket| bucket.second > now - WINDOW_SECONDS)
        {
            for index in 0..4 {
                application[index] = application[index].saturating_add(bucket.application[index]);
                wire[index] = wire[index].saturating_add(bucket.wire[index]);
            }
        }
        let protobuf_messages_total = state.by_type.values().map(|totals| totals.messages).sum();
        let protobuf_bytes_total = state.by_type.values().map(|totals| totals.bytes).sum();
        let protobuf_by_type = state
            .by_type
            .iter()
            .map(|(name, totals)| (name.clone(), [totals.messages, totals.bytes]))
            .collect();
        MetricsDebugStats {
            connections: state.connections.len(),
            pending_buckets: state.pending.len(),
            live_application_bytes: application,
            live_wire_bytes: wire,
            protobuf_messages_total,
            protobuf_bytes_total,
            protobuf_by_type,
            writer_send_count: self.writer_send_count.load(Ordering::Relaxed),
            writer_send_nanoseconds: self.writer_send_nanoseconds.load(Ordering::Relaxed),
            writer_send_failures: self.writer_send_failures.load(Ordering::Relaxed),
        }
    }

    pub fn register(&self, id: &str, channel: TrafficChannel) {
        self.state
            .lock()
            .expect("metrics lock poisoned")
            .connections
            .insert(
                id.to_owned(),
                ConnectionInfo {
                    channel,
                    by_type: HashMap::new(),
                },
            );
    }

    pub fn unregister(&self, id: &str) {
        self.state
            .lock()
            .expect("metrics lock poisoned")
            .connections
            .remove(id);
    }

    pub fn record(
        &self,
        layer: Layer,
        channel: TrafficChannel,
        direction: Direction,
        bytes: usize,
    ) {
        if bytes == 0 {
            return;
        }
        let now = unix_seconds();
        let mut state = self.state.lock().expect("metrics lock poisoned");
        let bucket = current_bucket(&mut state, now as i64);
        let values = match layer {
            Layer::Application => &mut bucket.application,
            Layer::Wire => &mut bucket.wire,
        };
        values[series_index(channel, direction)] =
            values[series_index(channel, direction)].saturating_add(bytes as u64);
        let pending = state
            .pending
            .entry((
                now as i64,
                layer_code(layer),
                channel_code(channel),
                direction_code(direction),
            ))
            .or_default();
        *pending = pending.saturating_add(bytes as u64);
    }

    pub fn drain_pending(&self) -> Vec<TrafficIncrement> {
        let pending =
            std::mem::take(&mut self.state.lock().expect("metrics lock poisoned").pending);
        pending
            .into_iter()
            .map(
                |((second, layer, channel, direction), bytes)| TrafficIncrement {
                    second,
                    layer: if layer == 0 {
                        Layer::Application
                    } else {
                        Layer::Wire
                    },
                    channel: if channel == 0 {
                        TrafficChannel::Player
                    } else {
                        TrafficChannel::WebMap
                    },
                    direction: if direction == 0 {
                        Direction::Ingress
                    } else {
                        Direction::Egress
                    },
                    bytes,
                },
            )
            .collect()
    }

    pub fn requeue(&self, increments: Vec<TrafficIncrement>) {
        let mut state = self.state.lock().expect("metrics lock poisoned");
        for increment in increments {
            let pending = state
                .pending
                .entry((
                    increment.second,
                    layer_code(increment.layer),
                    channel_code(increment.channel),
                    direction_code(increment.direction),
                ))
                .or_default();
            *pending = pending.saturating_add(increment.bytes);
        }
    }

    pub fn record_protobuf(&self, connection_id: &str, message_type: &str, bytes: usize) {
        if bytes == 0 || message_type.is_empty() {
            return;
        }
        let now = unix_seconds();
        let mut state = self.state.lock().expect("metrics lock poisoned");
        let session = state.by_type.entry(message_type.to_owned()).or_default();
        add_packet(session, bytes, now);
        if let Some(connection) = state.connections.get_mut(connection_id) {
            add_packet(
                connection
                    .by_type
                    .entry(message_type.to_owned())
                    .or_default(),
                bytes,
                now,
            );
        }
        let bucket = current_bucket(&mut state, now as i64);
        let live = bucket
            .protobuf
            .entry((connection_id.to_owned(), message_type.to_owned()))
            .or_default();
        live.0 = live.0.saturating_add(1);
        live.1 = live.1.saturating_add(bytes as u64);
    }

    pub fn live_traffic_json(&self) -> Value {
        let now = unix_seconds() as i64;
        let mut state = self.state.lock().expect("metrics lock poisoned");
        prune(&mut state, now);
        let mut application = [0_u64; 4];
        let mut wire = [0_u64; 4];
        for bucket in state
            .buckets
            .iter()
            .filter(|bucket| bucket.second > now - WINDOW_SECONDS)
        {
            for index in 0..4 {
                application[index] = application[index].saturating_add(bucket.application[index]);
                wire[index] = wire[index].saturating_add(bucket.wire[index]);
            }
        }
        json!({
            "sampleWindowSec": WINDOW_SECONDS,
            "selectedLayer": "application",
            "application": layer_json(application),
            "wire": layer_json(wire),
            "serverTime": unix_seconds(),
        })
    }

    pub fn protobuf_json(&self) -> Value {
        let now = unix_seconds() as i64;
        let mut state = self.state.lock().expect("metrics lock poisoned");
        prune(&mut state, now);
        let mut live_by_connection: HashMap<String, HashMap<String, (u64, u64)>> = HashMap::new();
        for bucket in state
            .buckets
            .iter()
            .filter(|bucket| bucket.second > now - WINDOW_SECONDS)
        {
            for ((connection_id, message_type), values) in &bucket.protobuf {
                let target = live_by_connection
                    .entry(connection_id.clone())
                    .or_default()
                    .entry(message_type.clone())
                    .or_default();
                target.0 = target.0.saturating_add(values.0);
                target.1 = target.1.saturating_add(values.1);
            }
        }
        let mut live_by_type: HashMap<String, (u64, u64)> = HashMap::new();
        for by_type in live_by_connection.values() {
            for (message_type, values) in by_type {
                let target = live_by_type.entry(message_type.clone()).or_default();
                target.0 = target.0.saturating_add(values.0);
                target.1 = target.1.saturating_add(values.1);
            }
        }
        let message_types = message_types_json(&state.by_type, &live_by_type);
        let total_session = combine_session(state.by_type.values());
        let total_live = combine_live(live_by_type.values());
        let mut connections = Vec::new();
        for (connection_id, connection) in &state.connections {
            let live = live_by_connection
                .get(connection_id)
                .cloned()
                .unwrap_or_default();
            let session_total = combine_session(connection.by_type.values());
            let live_total = combine_live(live.values());
            connections.push(json!({
                "actorId": connection_id,
                "channel": channel_name(connection.channel),
                "total": metric_json(&session_total, live_total),
                "snapshotFull": metric_json(
                    connection.by_type.get("snapshot_full").unwrap_or(&PacketTotals::default()),
                    live.get("snapshot_full").copied().unwrap_or_default(),
                ),
                "messageTypes": message_types_json(&connection.by_type, &live),
            }));
        }
        connections.sort_by(|left, right| {
            left["channel"]
                .as_str()
                .cmp(&right["channel"].as_str())
                .then_with(|| left["actorId"].as_str().cmp(&right["actorId"].as_str()))
        });
        json!({
            "sampleWindowSec": WINDOW_SECONDS,
            "total": metric_json(&total_session, total_live),
            "snapshotFull": metric_json(
                state.by_type.get("snapshot_full").unwrap_or(&PacketTotals::default()),
                live_by_type.get("snapshot_full").copied().unwrap_or_default(),
            ),
            "messageTypes": message_types,
            "connections": connections,
            "serverTime": unix_seconds(),
        })
    }
}

fn current_bucket(state: &mut MetricsState, second: i64) -> &mut TrafficBucket {
    prune(state, second);
    if state
        .buckets
        .back()
        .is_none_or(|bucket| bucket.second != second)
    {
        state.buckets.push_back(TrafficBucket {
            second,
            ..Default::default()
        });
    }
    state.buckets.back_mut().expect("bucket was inserted")
}

fn prune(state: &mut MetricsState, second: i64) {
    while state
        .buckets
        .front()
        .is_some_and(|bucket| bucket.second < second - WINDOW_SECONDS - 2)
    {
        state.buckets.pop_front();
    }
}

fn series_index(channel: TrafficChannel, direction: Direction) -> usize {
    match (channel, direction) {
        (TrafficChannel::Player, Direction::Ingress) => 0,
        (TrafficChannel::Player, Direction::Egress) => 1,
        (TrafficChannel::WebMap, Direction::Ingress) => 2,
        (TrafficChannel::WebMap, Direction::Egress) => 3,
    }
}

fn layer_code(layer: Layer) -> u8 {
    match layer {
        Layer::Application => 0,
        Layer::Wire => 1,
    }
}

fn channel_code(channel: TrafficChannel) -> u8 {
    match channel {
        TrafficChannel::Player => 0,
        TrafficChannel::WebMap => 1,
    }
}

fn direction_code(direction: Direction) -> u8 {
    match direction {
        Direction::Ingress => 0,
        Direction::Egress => 1,
    }
}

fn layer_json(values: [u64; 4]) -> Value {
    let divisor = WINDOW_SECONDS as f64;
    let player_ingress = values[0] as f64 / divisor;
    let player_egress = values[1] as f64 / divisor;
    let web_ingress = values[2] as f64 / divisor;
    let web_egress = values[3] as f64 / divisor;
    json!({
        "playerIngressBps": player_ingress,
        "playerEgressBps": player_egress,
        "webMapIngressBps": web_ingress,
        "webMapEgressBps": web_egress,
        "totalIngressBps": player_ingress + web_ingress,
        "totalEgressBps": player_egress + web_egress,
    })
}

fn add_packet(value: &mut PacketTotals, bytes: usize, now: f64) {
    value.messages = value.messages.saturating_add(1);
    value.bytes = value.bytes.saturating_add(bytes as u64);
    value.max_bytes = value.max_bytes.max(bytes as u64);
    value.last_sent_at = Some(now);
}

fn metric_json(session: &PacketTotals, live: (u64, u64)) -> Value {
    json!({
        "messageCount": session.messages,
        "messagesPerSecond": live.0 as f64 / WINDOW_SECONDS as f64,
        "byteCount": session.bytes,
        "bytesPerSecond": live.1 as f64 / WINDOW_SECONDS as f64,
        "maxPacketBytes": session.max_bytes,
        "lastSentAt": session.last_sent_at,
    })
}

fn message_types_json(
    session: &HashMap<String, PacketTotals>,
    live: &HashMap<String, (u64, u64)>,
) -> Vec<Value> {
    let mut keys = BTreeMap::new();
    for key in session.keys().chain(live.keys()) {
        keys.insert(key, ());
    }
    keys.into_keys()
        .map(|message_type| {
            let metric = metric_json(
                session
                    .get(message_type)
                    .unwrap_or(&PacketTotals::default()),
                live.get(message_type).copied().unwrap_or_default(),
            );
            json!({"messageType": message_type, "metric": metric})
        })
        .collect()
}

fn combine_session<'a>(values: impl Iterator<Item = &'a PacketTotals>) -> PacketTotals {
    let mut total = PacketTotals::default();
    for value in values {
        total.messages = total.messages.saturating_add(value.messages);
        total.bytes = total.bytes.saturating_add(value.bytes);
        total.max_bytes = total.max_bytes.max(value.max_bytes);
        total.last_sent_at = match (total.last_sent_at, value.last_sent_at) {
            (Some(left), Some(right)) => Some(left.max(right)),
            (None, right) => right,
            (left, None) => left,
        };
    }
    total
}

fn combine_live<'a>(values: impl Iterator<Item = &'a (u64, u64)>) -> (u64, u64) {
    values.fold((0_u64, 0_u64), |total, value| {
        (
            total.0.saturating_add(value.0),
            total.1.saturating_add(value.1),
        )
    })
}

fn channel_name(channel: TrafficChannel) -> &'static str {
    match channel {
        TrafficChannel::Player => "player",
        TrafficChannel::WebMap => "web_map",
    }
}

fn unix_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

pub fn protobuf_message_type(bytes: &[u8]) -> &'static str {
    let Some((_, mut offset)) = read_varint(bytes, 0) else {
        return "unknown";
    };
    let Some((_, next)) = read_varint(bytes, offset) else {
        return "unknown";
    };
    offset = next;
    let Some((key, _)) = read_varint(bytes, offset) else {
        return "unknown";
    };
    match key >> 3 {
        17 => "handshake_ack",
        18 => "web_map_ack",
        19 => "pong",
        20 => "snapshot_full",
        21 => "patch",
        22 => "digest",
        23 => "refresh_request",
        24 => "report_rate_hint",
        26 => "battle_chunk_meta_snapshot",
        30 => "tab_history_digest",
        31 => "tab_history_sync_chunk",
        32 => "tab_history_lookup_chunk",
        _ => "unknown",
    }
}

fn read_varint(bytes: &[u8], mut offset: usize) -> Option<(u64, usize)> {
    let mut value = 0_u64;
    for shift in (0..70).step_by(7) {
        let byte = *bytes.get(offset)?;
        offset += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some((value, offset));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traffic_layers_are_kept_separate() {
        let metrics = Metrics::default();
        metrics.record(
            Layer::Application,
            TrafficChannel::Player,
            Direction::Ingress,
            100,
        );
        metrics.record(Layer::Wire, TrafficChannel::Player, Direction::Ingress, 25);
        let value = metrics.live_traffic_json();
        assert_eq!(value["application"]["playerIngressBps"], 10.0);
        assert_eq!(value["wire"]["playerIngressBps"], 2.5);
    }
}
