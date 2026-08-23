use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    body::Body,
    extract::{ConnectInfo, Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use prost::Message as ProstMessage;
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;
use tokio::sync::{RwLock, mpsc, watch};
use tower_http::trace::TraceLayer;
use tracing::{info, warn};
use uuid::Uuid;
use yawc::{HttpWebSocket, IncomingUpgrade, Options, frame::Frame, frame::OpCode};

use crate::{
    admin,
    config::RuntimeConfig,
    metrics::{Direction, Layer, Metrics, TrafficChannel, protobuf_message_type},
    proto::teamviewer::v1::{
        ClientRole, HandshakeAck, Pong, TabHistoryCapabilities, TabHistoryDigest,
        TabHistoryErrorCode, TabHistoryLookupChunk, TabHistorySyncChunk, TabHistorySyncMode,
        WireChannel, WireEnvelope, wire_envelope,
    },
    proxy_ip::effective_remote_addr,
    relay::{
        CONTROL_CAPACITY, ConnectionKind, RegisterConnection, RelayEvent, RelayHandle, StateFrame,
        encode_payload,
    },
    tab_history::{
        DEFAULT_CHUNK_ENTRIES, MAX_CHUNK_BYTES, MAX_CHUNK_ENTRIES, MAX_LOOKUP_SELECTORS,
        TabHistoryStore,
    },
    transport::TransportConnectInfo,
};

const PROTOCOL_VERSION: &str = "0.7.1";
const MIN_PROTOCOL_VERSION: &str = "0.6.1";
const PROGRAM_VERSION: &str = concat!(
    "team-view-relay-rust-v",
    env!("CARGO_PKG_VERSION"),
    "-proto0.7.1"
);

#[derive(Clone)]
pub struct AppState {
    pub relay: RelayHandle,
    pub db: SqlitePool,
    pub tab_history: Arc<TabHistoryStore>,
    pub config: Arc<RuntimeConfig>,
    pub metrics: Arc<Metrics>,
    pub maintenance_rooms: Arc<RwLock<HashSet<String>>>,
}

#[derive(RustEmbed)]
#[folder = "admin-ui/dist"]
struct AdminAssets;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/snapshot", get(snapshot))
        .route("/mc-client", get(player_ws))
        .route("/playeresp", get(player_ws))
        .route("/web-map/ws", get(web_map_ws))
        .route("/adminws", get(web_map_ws))
        .route("/admin/ws", get(reserved_admin_ws))
        .route("/admin", get(admin_index))
        .route("/admin/assets/{*path}", get(admin_asset))
        .route("/admin/api/session/login", post(admin::login))
        .route("/admin/api/session", get(admin::current))
        .route("/admin/api/session/logout", post(admin::logout))
        .route("/admin/api/overview", get(admin::overview))
        .route("/admin/api/events", get(admin::events))
        .route("/admin/api/metrics/daily", get(admin::metrics_daily))
        .route("/admin/api/metrics/hourly", get(admin::metrics_hourly))
        .route("/admin/api/traffic/live", get(admin::traffic_live))
        .route("/admin/api/protobuf/live", get(admin::protobuf_live))
        .route("/admin/api/traffic/history", get(admin::traffic_history))
        .route("/admin/api/traffic/hourly", get(admin::traffic_hourly))
        .route("/admin/api/traffic/daily", get(admin::traffic_daily))
        .route("/admin/api/audit", get(admin::audit))
        .route(
            "/admin/api/maintenance/room-data",
            get(admin::room_data_maintenance),
        )
        .route(
            "/admin/api/maintenance/room-data",
            delete(admin::delete_room_data),
        )
        .route(
            "/admin/api/history/last-seen",
            get(admin::last_seen_history),
        )
        .route(
            "/admin/api/history/last-seen",
            delete(admin::delete_last_seen_history),
        )
        .route("/admin/api/history/tab", get(admin::tab_history))
        .route("/admin/api/history/tab", delete(admin::delete_tab_history))
        .route("/admin/api/runtime/{kind}", get(admin::runtime_state))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    axum::Json(json!({"status": "ok", "buildVersion": PROGRAM_VERSION}))
}

#[derive(Deserialize)]
struct SnapshotQuery {
    #[serde(rename = "roomCode")]
    room_code: Option<String>,
}

async fn snapshot(
    State(state): State<AppState>,
    Query(query): Query<SnapshotQuery>,
) -> impl IntoResponse {
    match state.relay.snapshot(query.room_code).await {
        Ok(value) => (StatusCode::OK, axum::Json(value)).into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({"detail": error.to_string()})),
        )
            .into_response(),
    }
}

async fn player_ws(
    ws: IncomingUpgrade,
    State(state): State<AppState>,
    ConnectInfo(transport): ConnectInfo<TransportConnectInfo>,
    headers: HeaderMap,
) -> Response {
    let remote_addr = effective_remote_addr(&headers, &transport);
    upgrade_websocket(ws, state, ConnectionKind::Player, transport, remote_addr)
}

async fn web_map_ws(
    ws: IncomingUpgrade,
    State(state): State<AppState>,
    ConnectInfo(transport): ConnectInfo<TransportConnectInfo>,
    headers: HeaderMap,
) -> Response {
    let remote_addr = effective_remote_addr(&headers, &transport);
    upgrade_websocket(ws, state, ConnectionKind::WebMap, transport, remote_addr)
}

async fn reserved_admin_ws(
    ws: IncomingUpgrade,
    ConnectInfo(transport): ConnectInfo<TransportConnectInfo>,
) -> Response {
    let Ok((response, upgraded)) = ws.upgrade(websocket_options()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    tokio::spawn(async move {
        if let Ok(mut socket) = upgraded.await {
            transport.wire.activate();
            let _ = socket
                .send(Frame::close(
                    yawc::close::CloseCode::Policy,
                    "admin_interface_reserved",
                ))
                .await;
            transport.wire.deactivate();
        }
    });
    response.map(Body::new)
}

fn upgrade_websocket(
    ws: IncomingUpgrade,
    state: AppState,
    route_kind: ConnectionKind,
    transport: TransportConnectInfo,
    remote_addr: String,
) -> Response {
    let Ok((response, upgraded)) = ws.upgrade(websocket_options()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    tokio::spawn(async move {
        match upgraded.await {
            Ok(socket) => {
                transport.wire.activate();
                serve_socket(socket, state, route_kind, &transport, remote_addr).await;
                transport.wire.deactivate();
            }
            Err(error) => warn!(%error, "websocket upgrade failed"),
        }
    });
    response.map(Body::new)
}

fn websocket_options() -> Options {
    Options::default()
        .with_balanced_compression()
        .with_limits(16 * 1024 * 1024, 32 * 1024 * 1024)
        .with_backpressure_boundary(32 * 1024 * 1024)
}

async fn serve_socket(
    mut socket: HttpWebSocket,
    state: AppState,
    route_kind: ConnectionKind,
    transport: &TransportConnectInfo,
    remote_addr: String,
) {
    let first = match tokio::time::timeout(Duration::from_secs(10), socket.next()).await {
        Ok(Some(frame)) if frame.opcode() == OpCode::Binary => frame.payload().clone(),
        _ => {
            let _ = socket
                .send(Frame::close(
                    yawc::close::CloseCode::Policy,
                    "handshake_required",
                ))
                .await;
            return;
        }
    };
    let envelope = match WireEnvelope::decode(first) {
        Ok(envelope) => envelope,
        Err(error) => {
            warn!(%error, "invalid protobuf handshake");
            let _ = socket
                .send(Frame::close(
                    yawc::close::CloseCode::Policy,
                    "invalid_payload",
                ))
                .await;
            return;
        }
    };

    let (id, room, protocol, kind, channel, display_name, position_resolution, program_version) =
        match (route_kind, envelope.payload) {
            (
                ConnectionKind::Player,
                Some(wire_envelope::Payload::PlayerHandshakeRequest(handshake)),
            ) => {
                if handshake.submit_player_id.is_empty()
                    || !protocol_at_least(&handshake.network_protocol_version, MIN_PROTOCOL_VERSION)
                {
                    reject(&mut socket, WireChannel::Player, "client_protocol_too_old").await;
                    return;
                }
                let role = match handshake
                    .client_role
                    .and_then(|value| ClientRole::try_from(value).ok())
                {
                    Some(ClientRole::ExternalSource) => ConnectionKind::ExternalSource,
                    _ => ConnectionKind::Player,
                };
                (
                    handshake.submit_player_id,
                    normalize_room(handshake.room_code.as_deref()),
                    handshake.network_protocol_version,
                    role,
                    WireChannel::Player,
                    handshake.client_display_name,
                    handshake.position_resolution,
                    handshake.local_program_version,
                )
            }
            (
                ConnectionKind::WebMap,
                Some(wire_envelope::Payload::WebMapHandshakeRequest(handshake)),
            ) => {
                if !protocol_at_least(&handshake.network_protocol_version, MIN_PROTOCOL_VERSION) {
                    reject(&mut socket, WireChannel::WebMap, "client_protocol_too_old").await;
                    return;
                }
                (
                    format!("web-map-{}", Uuid::new_v4()),
                    normalize_room(handshake.room_code.as_deref()),
                    handshake.network_protocol_version,
                    ConnectionKind::WebMap,
                    WireChannel::WebMap,
                    Some("Web Map".to_owned()),
                    None,
                    handshake.local_program_version,
                )
            }
            _ => {
                reject(&mut socket, channel_for(route_kind), "channel_mismatch").await;
                return;
            }
        };

    let accepted_role = match kind {
        ConnectionKind::ExternalSource => ClientRole::ExternalSource,
        _ => ClientRole::Player,
    };
    let maintenance_guard = state.maintenance_rooms.clone().read_owned().await;
    if maintenance_guard.contains(&room) {
        reject(&mut socket, channel, "room_maintenance").await;
        return;
    }
    let traffic_channel = match channel {
        WireChannel::WebMap => TrafficChannel::WebMap,
        _ => TrafficChannel::Player,
    };
    state.metrics.register(&id, traffic_channel);
    let ack = HandshakeAck {
        ready: true,
        network_protocol_version: PROTOCOL_VERSION.to_owned(),
        minimum_compatible_network_protocol_version: MIN_PROTOCOL_VERSION.to_owned(),
        local_program_version: PROGRAM_VERSION.to_owned(),
        room_code: room.clone(),
        delta_enabled: true,
        digest_interval_sec: Some(state.config.digest_interval_sec as i32),
        broadcast_hz: Some(
            state
                .config
                .broadcast_hz(state.relay.player_connection_count()),
        ),
        report_interval_ticks: Some(
            state.config.report_interval_ticks(
                state
                    .config
                    .broadcast_hz(state.relay.player_connection_count()),
            ),
        ),
        player_timeout_sec: Some(state.config.player_timeout_sec as i32),
        entity_timeout_sec: Some(state.config.entity_timeout_sec as i32),
        battle_chunk_timeout_sec: Some(state.config.battle_chunk_timeout_sec as i32),
        accepted_client_role: Some(accepted_role as i32),
        tab_history: protocol_at_least(&protocol, "0.7.0").then(|| tab_capabilities(&state.config)),
        ..Default::default()
    };
    let ack = encode_payload(channel, wire_envelope::Payload::HandshakeAck(ack));
    let ack_len = ack.len();
    if tokio::time::timeout(
        Duration::from_secs(2),
        socket.send(Frame::binary(Bytes::from_owner(ack))),
    )
    .await
    .is_err()
    {
        state.metrics.unregister(&id);
        return;
    }
    state.metrics.record(
        Layer::Application,
        traffic_channel,
        Direction::Egress,
        ack_len,
    );
    state.metrics.record_protobuf(&id, "handshake_ack", ack_len);

    let (control_tx, control_rx) = mpsc::channel(CONTROL_CAPACITY);
    let (state_tx, state_rx) = watch::channel(None::<StateFrame>);
    if state
        .relay
        .send(RelayEvent::Register(RegisterConnection {
            id: id.clone(),
            room: room.clone(),
            protocol: protocol.clone(),
            kind,
            display_name,
            position_resolution,
            program_version,
            remote_addr: remote_addr.clone(),
            control: control_tx.clone(),
            state: state_tx,
        }))
        .await
        .is_err()
    {
        state.metrics.unregister(&id);
        return;
    }
    drop(maintenance_guard);
    record_connection_started(&state.db, &id, &room, kind, &remote_addr).await;
    info!(connection_id = %id, %room, ?kind, %protocol, %remote_addr, "websocket connected");

    let (sink, stream) = socket.split();
    let event_tx = state.relay.sender();
    let writer_id = id.clone();
    let writer_events = event_tx.clone();
    let mut writer = tokio::spawn(writer_loop(
        writer_id,
        sink,
        control_rx,
        state_rx,
        writer_events,
        state.metrics.clone(),
        traffic_channel,
    ));
    let reader_id = id.clone();
    let mut reader = tokio::spawn(reader_loop(
        stream,
        ReaderContext {
            id: reader_id,
            room: room.clone(),
            control: control_tx,
            events: event_tx.clone(),
            channel,
            tab_history: state.tab_history.clone(),
            metrics: state.metrics.clone(),
            traffic_channel,
        },
    ));
    let last_wire_ingress = Arc::new(AtomicU64::new(0));
    let last_wire_egress = Arc::new(AtomicU64::new(0));
    let wire_sampler = tokio::spawn(sample_wire_traffic(
        transport.wire.clone(),
        state.metrics.clone(),
        traffic_channel,
        last_wire_ingress.clone(),
        last_wire_egress.clone(),
    ));

    tokio::select! {
        _ = &mut writer => reader.abort(),
        _ = &mut reader => writer.abort(),
    }
    wire_sampler.abort();
    let (wire_ingress, wire_egress) = transport.wire.totals();
    state.metrics.record(
        Layer::Wire,
        traffic_channel,
        Direction::Ingress,
        wire_ingress.saturating_sub(last_wire_ingress.load(Ordering::Relaxed)) as usize,
    );
    state.metrics.record(
        Layer::Wire,
        traffic_channel,
        Direction::Egress,
        wire_egress.saturating_sub(last_wire_egress.load(Ordering::Relaxed)) as usize,
    );
    let _ = event_tx
        .send(RelayEvent::Disconnect { id: id.clone() })
        .await;
    record_connection_ended(&state.db, &id, &room, kind, &remote_addr).await;
    state.metrics.unregister(&id);
    info!(connection_id = %id, "websocket disconnected");
}

async fn sample_wire_traffic(
    wire: Arc<crate::transport::WireCounter>,
    metrics: Arc<Metrics>,
    channel: TrafficChannel,
    last_ingress: Arc<AtomicU64>,
    last_egress: Arc<AtomicU64>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.tick().await;
    loop {
        interval.tick().await;
        let (ingress, egress) = wire.totals();
        let previous_ingress = last_ingress.swap(ingress, Ordering::Relaxed);
        let previous_egress = last_egress.swap(egress, Ordering::Relaxed);
        metrics.record(
            Layer::Wire,
            channel,
            Direction::Ingress,
            ingress.saturating_sub(previous_ingress) as usize,
        );
        metrics.record(
            Layer::Wire,
            channel,
            Direction::Egress,
            egress.saturating_sub(previous_egress) as usize,
        );
    }
}

async fn writer_loop(
    id: String,
    mut sink: futures_util::stream::SplitSink<HttpWebSocket, Frame>,
    mut control: mpsc::Receiver<Arc<[u8]>>,
    mut state: watch::Receiver<Option<StateFrame>>,
    events: mpsc::Sender<RelayEvent>,
    metrics: Arc<Metrics>,
    traffic_channel: TrafficChannel,
) {
    loop {
        let frame = tokio::select! {
            biased;
            value = control.recv() => value.map(|bytes| (bytes, None)),
            changed = state.changed() => {
                if changed.is_err() { None } else {
                    state.borrow_and_update().clone().map(|frame| (frame.bytes, Some(frame.revision)))
                }
            }
        };
        let Some((bytes, revision)) = frame else {
            break;
        };
        let byte_count = bytes.len();
        let message_type = protobuf_message_type(&bytes);
        let sent = tokio::time::timeout(
            Duration::from_secs(2),
            sink.send(Frame::binary(Bytes::from_owner(bytes))),
        )
        .await;
        if !matches!(sent, Ok(Ok(()))) {
            warn!(connection_id = %id, "slow or failed websocket writer disconnected");
            break;
        }
        metrics.record(
            Layer::Application,
            traffic_channel,
            Direction::Egress,
            byte_count,
        );
        metrics.record_protobuf(&id, message_type, byte_count);
        if let Some(revision) = revision {
            let _ = events
                .send(RelayEvent::Delivered {
                    id: id.clone(),
                    revision,
                })
                .await;
        }
    }
}

struct ReaderContext {
    id: String,
    room: String,
    control: mpsc::Sender<Arc<[u8]>>,
    events: mpsc::Sender<RelayEvent>,
    channel: WireChannel,
    tab_history: Arc<TabHistoryStore>,
    metrics: Arc<Metrics>,
    traffic_channel: TrafficChannel,
}

async fn reader_loop(
    mut stream: futures_util::stream::SplitStream<HttpWebSocket>,
    context: ReaderContext,
) {
    let ReaderContext {
        id,
        room,
        control,
        events,
        channel,
        tab_history,
        metrics,
        traffic_channel,
    } = context;
    while let Some(frame) = stream.next().await {
        if frame.opcode() != OpCode::Binary {
            if frame.opcode() == OpCode::Close {
                break;
            }
            continue;
        }
        metrics.record(
            Layer::Application,
            traffic_channel,
            Direction::Ingress,
            frame.payload().len(),
        );
        let Ok(envelope) = WireEnvelope::decode(frame.payload().clone()) else {
            continue;
        };
        match envelope.payload {
            Some(wire_envelope::Payload::PlayerReportBundle(report))
                if report.submit_player_id == id =>
            {
                let tab_players = report
                    .tab_players_replace
                    .as_ref()
                    .map(|replace| replace.tab_players.clone())
                    .or_else(|| {
                        report.tab_players_patch.as_ref().map(|patch| {
                            patch
                                .upsert
                                .iter()
                                .filter_map(|upsert| upsert.data.clone())
                                .collect()
                        })
                    })
                    .unwrap_or_default();
                let changed_history_head = if tab_players.is_empty() {
                    None
                } else if matches!(
                    tab_history
                        .upsert_players(&room, &tab_players, unix_millis())
                        .await,
                    Ok(true)
                ) {
                    tab_history.head(&room).await.ok()
                } else {
                    None
                };
                if events
                    .send(RelayEvent::PlayerReport {
                        id: id.clone(),
                        report: Box::new(report),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
                if let Some(head) = changed_history_head {
                    let _ = events
                        .send(RelayEvent::TabHistoryChanged {
                            room: room.clone(),
                            head,
                        })
                        .await;
                }
            }
            Some(wire_envelope::Payload::ResyncRequest(_)) => {
                let _ = events.send(RelayEvent::Resync { id: id.clone() }).await;
            }
            Some(wire_envelope::Payload::WebMapCommand(command))
                if channel == WireChannel::WebMap =>
            {
                let _ = events
                    .send(RelayEvent::WebMapCommand {
                        id: id.clone(),
                        command,
                    })
                    .await;
            }
            Some(wire_envelope::Payload::BattleChunkMetaRequest(request)) => {
                let _ = events
                    .send(RelayEvent::BattleChunkMeta {
                        id: id.clone(),
                        battle_chunks: request.battle_chunks.into_iter().take(256).collect(),
                    })
                    .await;
            }
            Some(wire_envelope::Payload::TabHistorySubscribeRequest(request)) => {
                let _ = events
                    .send(RelayEvent::TabHistorySubscription {
                        id: id.clone(),
                        enabled: request.enabled,
                    })
                    .await;
                if request.enabled
                    && let Ok(head) = tab_history.head(&room).await
                {
                    let bytes = encode_payload(
                        channel,
                        wire_envelope::Payload::TabHistoryDigest(TabHistoryDigest {
                            head: Some(head),
                        }),
                    );
                    if control.send(bytes).await.is_err() {
                        break;
                    }
                }
            }
            Some(wire_envelope::Payload::TabHistorySyncRequest(request)) => {
                if send_tab_sync(&tab_history, &control, channel, &room, request)
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Some(wire_envelope::Payload::TabHistoryLookupRequest(request)) => {
                if send_tab_lookup(&tab_history, &control, channel, &room, request)
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Some(wire_envelope::Payload::Ping(_)) => {
                let pong = encode_payload(
                    channel,
                    wire_envelope::Payload::Pong(Pong {
                        server_time: unix_seconds(),
                    }),
                );
                if control.send(pong).await.is_err() {
                    break;
                }
            }
            _ => {}
        }
    }
}

async fn send_tab_sync(
    store: &TabHistoryStore,
    control: &mpsc::Sender<Arc<[u8]>>,
    channel: WireChannel,
    room: &str,
    request: crate::proto::teamviewer::v1::TabHistorySyncRequest,
) -> anyhow::Result<()> {
    let request_id = request
        .request_id
        .trim()
        .chars()
        .take(128)
        .collect::<String>();
    if request_id.is_empty() {
        return send_tab_sync_error(
            control,
            channel,
            request_id,
            TabHistoryErrorCode::InvalidRequest,
        )
        .await;
    }
    let result = store
        .sync(
            room,
            request.preferred_mode,
            request.base_revision,
            request.base_digest_sha256.as_deref(),
            request.allow_full_fallback,
        )
        .await?;
    #[derive(Clone)]
    enum Item {
        Upsert(Box<crate::proto::teamviewer::v1::TabHistoryEntry>),
        Delete(String),
    }
    let items: Vec<_> = result
        .upsert
        .into_iter()
        .map(|entry| Item::Upsert(Box::new(entry)))
        .chain(result.delete_uuids.into_iter().map(Item::Delete))
        .collect();
    let limit = chunk_limit(request.max_chunk_entries);
    let groups = split_sized(items, limit, |group| {
        let (upsert, delete_uuids) = split_sync_items(group);
        TabHistorySyncChunk {
            request_id: request_id.clone(),
            mode: result.mode,
            head: Some(result.head.clone()),
            upsert,
            delete_uuids,
            chunk_index: 0,
            chunk_count: 1,
            r#final: true,
            reset_reason: result.reset_reason,
            ..Default::default()
        }
        .encoded_len()
    });
    let count = groups.len() as u32;
    for (index, group) in groups.iter().enumerate() {
        let (upsert, delete_uuids) = split_sync_items(group);
        let chunk = TabHistorySyncChunk {
            request_id: request_id.clone(),
            mode: result.mode,
            head: Some(result.head.clone()),
            upsert,
            delete_uuids,
            chunk_index: index as u32,
            chunk_count: count,
            r#final: index + 1 == groups.len(),
            reset_reason: result.reset_reason,
            ..Default::default()
        };
        control
            .send(encode_payload(
                channel,
                wire_envelope::Payload::TabHistorySyncChunk(chunk),
            ))
            .await?;
    }
    return Ok(());

    fn split_sync_items(
        items: &[Item],
    ) -> (
        Vec<crate::proto::teamviewer::v1::TabHistoryEntry>,
        Vec<String>,
    ) {
        let mut upsert = Vec::new();
        let mut delete = Vec::new();
        for item in items {
            match item {
                Item::Upsert(entry) => upsert.push(entry.as_ref().clone()),
                Item::Delete(uuid) => delete.push(uuid.clone()),
            }
        }
        (upsert, delete)
    }
}

async fn send_tab_lookup(
    store: &TabHistoryStore,
    control: &mpsc::Sender<Arc<[u8]>>,
    channel: WireChannel,
    room: &str,
    request: crate::proto::teamviewer::v1::TabHistoryLookupRequest,
) -> anyhow::Result<()> {
    let request_id = request
        .request_id
        .trim()
        .chars()
        .take(128)
        .collect::<String>();
    if request_id.is_empty() || request.selectors.is_empty() {
        return send_tab_lookup_error(
            control,
            channel,
            request_id,
            TabHistoryErrorCode::InvalidRequest,
        )
        .await;
    }
    if request.selectors.len() > MAX_LOOKUP_SELECTORS {
        return send_tab_lookup_error(
            control,
            channel,
            request_id,
            TabHistoryErrorCode::TooManySelectors,
        )
        .await;
    }
    let (head, results) = store.lookup(room, &request.selectors).await?;
    let groups = split_sized(results, chunk_limit(request.max_chunk_entries), |group| {
        TabHistoryLookupChunk {
            request_id: request_id.clone(),
            head: Some(head.clone()),
            results: group.to_vec(),
            chunk_index: 0,
            chunk_count: 1,
            r#final: true,
            ..Default::default()
        }
        .encoded_len()
    });
    let count = groups.len() as u32;
    for (index, group) in groups.iter().enumerate() {
        let chunk = TabHistoryLookupChunk {
            request_id: request_id.clone(),
            head: Some(head.clone()),
            results: group.clone(),
            chunk_index: index as u32,
            chunk_count: count,
            r#final: index + 1 == groups.len(),
            ..Default::default()
        };
        control
            .send(encode_payload(
                channel,
                wire_envelope::Payload::TabHistoryLookupChunk(chunk),
            ))
            .await?;
    }
    Ok(())
}

fn split_sized<T: Clone>(
    items: Vec<T>,
    max_entries: usize,
    encoded_len: impl Fn(&[T]) -> usize,
) -> Vec<Vec<T>> {
    if items.is_empty() {
        return vec![Vec::new()];
    }
    let mut groups = Vec::new();
    let mut offset = 0;
    while offset < items.len() {
        let end = (offset + max_entries).min(items.len());
        let mut candidate = items[offset..end].to_vec();
        while candidate.len() > 1 && encoded_len(&candidate) > MAX_CHUNK_BYTES {
            candidate.truncate(candidate.len().div_ceil(2));
        }
        offset += candidate.len();
        groups.push(candidate);
    }
    groups
}

fn chunk_limit(requested: Option<u32>) -> usize {
    requested
        .map_or(DEFAULT_CHUNK_ENTRIES, |value| value as usize)
        .clamp(1, MAX_CHUNK_ENTRIES)
}

async fn send_tab_sync_error(
    control: &mpsc::Sender<Arc<[u8]>>,
    channel: WireChannel,
    request_id: String,
    code: TabHistoryErrorCode,
) -> anyhow::Result<()> {
    control
        .send(encode_payload(
            channel,
            wire_envelope::Payload::TabHistorySyncChunk(TabHistorySyncChunk {
                request_id,
                chunk_count: 1,
                r#final: true,
                error_code: Some(code as i32),
                error_detail: Some(code.as_str_name().to_owned()),
                ..Default::default()
            }),
        ))
        .await?;
    Ok(())
}

async fn send_tab_lookup_error(
    control: &mpsc::Sender<Arc<[u8]>>,
    channel: WireChannel,
    request_id: String,
    code: TabHistoryErrorCode,
) -> anyhow::Result<()> {
    control
        .send(encode_payload(
            channel,
            wire_envelope::Payload::TabHistoryLookupChunk(TabHistoryLookupChunk {
                request_id,
                chunk_count: 1,
                r#final: true,
                error_code: Some(code as i32),
                error_detail: Some(code.as_str_name().to_owned()),
                ..Default::default()
            }),
        ))
        .await?;
    Ok(())
}

async fn reject(socket: &mut HttpWebSocket, channel: WireChannel, reason: &str) {
    let ack = HandshakeAck {
        ready: false,
        network_protocol_version: PROTOCOL_VERSION.to_owned(),
        minimum_compatible_network_protocol_version: MIN_PROTOCOL_VERSION.to_owned(),
        local_program_version: PROGRAM_VERSION.to_owned(),
        error: Some("version_incompatible".to_owned()),
        reject_reason: Some(reason.to_owned()),
        ..Default::default()
    };
    let bytes = encode_payload(channel, wire_envelope::Payload::HandshakeAck(ack));
    let _ = socket.send(Frame::binary(Bytes::from_owner(bytes))).await;
    let _ = socket
        .send(Frame::close(
            yawc::close::CloseCode::Policy,
            truncate_close_reason(reason),
        ))
        .await;
}

fn truncate_close_reason(reason: &str) -> String {
    let mut end = reason.len().min(123);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason[..end].to_owned()
}

async fn admin_index() -> Response {
    embedded_response("index.html")
}

async fn admin_asset(Path(path): Path<String>) -> Response {
    embedded_response(&path)
}

fn embedded_response(path: &str) -> Response {
    let Some(asset) = AdminAssets::get(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime.as_ref())
        .body(Body::from(asset.data))
        .expect("embedded response")
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

fn normalize_room(room: Option<&str>) -> String {
    room.map(str::trim)
        .filter(|room| !room.is_empty())
        .unwrap_or("default")
        .to_owned()
}

fn channel_for(kind: ConnectionKind) -> WireChannel {
    match kind {
        ConnectionKind::WebMap => WireChannel::WebMap,
        _ => WireChannel::Player,
    }
}

fn tab_capabilities(config: &RuntimeConfig) -> TabHistoryCapabilities {
    TabHistoryCapabilities {
        supported: true,
        sync_modes: vec![
            TabHistorySyncMode::Full as i32,
            TabHistorySyncMode::Delta as i32,
        ],
        default_chunk_entries: config.tab_history_default_chunk_entries as u32,
        max_chunk_entries: config.tab_history_max_chunk_entries as u32,
        max_chunk_bytes: config.tab_history_max_chunk_bytes as u32,
        max_lookup_selectors: config.tab_history_max_lookup_selectors as u32,
        retention_days: config.tab_history_retention_days,
        delta_retention_days: config.tab_history_delta_retention_days,
        max_formatted_text_spans: 64,
        max_formatted_text_utf8_bytes: 4096,
    }
}

fn unix_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn unix_millis() -> i64 {
    (unix_seconds() * 1000.0) as i64
}

async fn record_connection_started(
    db: &SqlitePool,
    actor_id: &str,
    room: &str,
    kind: ConnectionKind,
    remote_addr: &str,
) {
    let now = unix_millis();
    if kind == ConnectionKind::Player {
        for query in [
            "INSERT INTO daily_player_activity (local_date, player_id, room_code, first_seen_at, last_seen_at) VALUES (strftime('%Y-%m-%d', 'now', 'localtime'), ?, ?, ?, ?) ON CONFLICT(local_date, player_id, room_code) DO UPDATE SET first_seen_at = MIN(first_seen_at, excluded.first_seen_at), last_seen_at = MAX(last_seen_at, excluded.last_seen_at)",
            "INSERT INTO hourly_player_activity (local_hour, player_id, room_code, first_seen_at, last_seen_at) VALUES (strftime('%Y-%m-%dT%H:00:00', 'now', 'localtime'), ?, ?, ?, ?) ON CONFLICT(local_hour, player_id, room_code) DO UPDATE SET first_seen_at = MIN(first_seen_at, excluded.first_seen_at), last_seen_at = MAX(last_seen_at, excluded.last_seen_at)",
        ] {
            let _ = sqlx::query(query)
                .bind(actor_id)
                .bind(room)
                .bind(now)
                .bind(now)
                .execute(db)
                .await;
        }
    }
    let event_type = match kind {
        ConnectionKind::Player => "player_connected",
        ConnectionKind::ExternalSource => "external_source_handshake_success",
        ConnectionKind::WebMap => "web_map_connected",
    };
    record_connection_audit(db, event_type, actor_id, room, remote_addr, json!({})).await;
}

async fn record_connection_ended(
    db: &SqlitePool,
    actor_id: &str,
    room: &str,
    kind: ConnectionKind,
    remote_addr: &str,
) {
    let event_type = match kind {
        ConnectionKind::Player => "player_disconnected",
        ConnectionKind::ExternalSource => "external_source_disconnected",
        ConnectionKind::WebMap => "web_map_disconnected",
    };
    record_connection_audit(
        db,
        event_type,
        actor_id,
        room,
        remote_addr,
        json!({"reason":"connection_closed"}),
    )
    .await;
}

async fn record_connection_audit(
    db: &SqlitePool,
    event_type: &str,
    actor_id: &str,
    room: &str,
    remote_addr: &str,
    detail: serde_json::Value,
) {
    let actor_type = match event_type {
        value if value.starts_with("web_map") => "web_map",
        value if value.starts_with("external_source") => "external_source",
        _ => "player",
    };
    let _ = sqlx::query(
        "INSERT INTO audit_events (occurred_at, local_date, local_hour, event_type, actor_type, actor_id, room_code, success, remote_addr, detail_json) VALUES (?, strftime('%Y-%m-%d', 'now', 'localtime'), strftime('%Y-%m-%dT%H:00:00', 'now', 'localtime'), ?, ?, ?, ?, 1, ?, ?)",
    )
    .bind(unix_millis())
    .bind(event_type)
    .bind(actor_type)
    .bind(actor_id)
    .bind(room)
    .bind(remote_addr)
    .bind(detail.to_string())
    .execute(db)
    .await;
}
