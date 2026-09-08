use std::{
    collections::HashSet,
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
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
use futures_util::{Sink, SinkExt, Stream, StreamExt};
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
        ClientRole, ExternalDatasetPublishAck, HandshakeAck, PlayerDirectoryLookupChunk,
        PlayerDirectoryLookupResult, PlayerRelationQueryChunk, Pong, RelationshipQueryCapabilities,
        TabHistoryCapabilities, TabHistoryDigest, TabHistoryErrorCode, TabHistoryLookupChunk,
        TabHistorySyncChunk, TabHistorySyncMode, UnreliableChannel, WireChannel, WireEnvelope,
        wire_envelope,
    },
    protocol_compat::{
        CURRENT_PROTOCOL_VERSION, MINIMUM_PROTOCOL_VERSION, ProtocolProfile,
        accepts_unreliable_channels, sanitize_player_report,
    },
    proxy_ip::effective_remote_addr,
    relationship_store::RelationshipStore,
    relay::{
        CONTROL_CAPACITY, ConnectionKind, MovementBatch, RegisterConnection, RelayEvent,
        RelayHandle, StateFrame, encode_payload,
    },
    tab_history::{
        DEFAULT_CHUNK_ENTRIES, MAX_CHUNK_BYTES, MAX_CHUNK_ENTRIES, MAX_LOOKUP_SELECTORS,
        TabHistoryStore,
    },
    transport::TransportConnectInfo,
};

type WebMapIncoming = Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>;

pub struct WebMapFrameStream {
    stream: WebMapIncoming,
}

impl WebMapFrameStream {
    pub fn new<S>(stream: S) -> Self
    where
        S: Stream<Item = Result<Bytes, io::Error>> + Send + 'static,
    {
        Self {
            stream: Box::pin(stream),
        }
    }
}

impl Stream for WebMapFrameStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().stream.as_mut().poll_next(cx)
    }
}

const PROGRAM_VERSION: &str = concat!(
    "team-view-relay-rust-v",
    env!("CARGO_PKG_VERSION"),
    "-proto0.9.0"
);

#[derive(Clone)]
pub struct AppState {
    pub relay: RelayHandle,
    pub db: SqlitePool,
    pub tab_history: Arc<TabHistoryStore>,
    pub relationships: Arc<crate::relationship_store::RelationshipStore>,
    pub config: Arc<RuntimeConfig>,
    pub metrics: Arc<Metrics>,
    pub maintenance_rooms: Arc<RwLock<HashSet<String>>>,
    /// bulk 传输通道触发表(QUIC/WT 门会话注册;debug 端点投递用)
    pub bulk_hub: Arc<crate::bulk::Hub>,
    #[cfg(feature = "memory-debug")]
    pub resource_debug: Option<crate::resource_debug::ResourceDebugHandle>,
}

#[derive(RustEmbed)]
#[folder = "admin-ui/dist"]
struct AdminAssets;

pub fn router(state: AppState) -> Router {
    let router = Router::new()
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
        .route("/admin/api/runtime/{kind}", get(admin::runtime_state));
    #[cfg(feature = "memory-debug")]
    let router = router
        .route(
            "/admin/api/debug/resources/current",
            get(admin::debug_resources_current),
        )
        .route(
            "/admin/api/debug/resources/profiles",
            get(admin::debug_resource_profiles),
        )
        .route(
            "/admin/api/debug/resources/profiles/{name}",
            post(admin::trigger_debug_resource_profile).get(admin::download_debug_resource_profile),
        )
        .route("/admin/api/debug/bulk-push", post(admin::bulk_push));
    router.layer(TraceLayer::new_for_http()).with_state(state)
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
    upgrade_websocket(
        ws,
        state,
        ConnectionKind::Player,
        transport,
        remote_addr,
        &headers,
    )
    .await
}

async fn web_map_ws(
    ws: IncomingUpgrade,
    State(state): State<AppState>,
    ConnectInfo(transport): ConnectInfo<TransportConnectInfo>,
    headers: HeaderMap,
) -> Response {
    let remote_addr = effective_remote_addr(&headers, &transport);
    upgrade_websocket(
        ws,
        state,
        ConnectionKind::WebMap,
        transport,
        remote_addr,
        &headers,
    )
    .await
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

async fn upgrade_websocket(
    ws: IncomingUpgrade,
    state: AppState,
    route_kind: ConnectionKind,
    transport: TransportConnectInfo,
    remote_addr: String,
    headers: &HeaderMap,
) -> Response {
    // 压缩套协商:客户端提供 teamviewrelay 子协议 → 择优回显(zstd 选中时
    // 关闭 permessage-deflate,zstd 输出近高熵,叠加零收益纯烧 CPU);
    // 旧客户端无该子协议 → 维持原 permessage-deflate 协商路径,行为不变。
    let suite = ws_suite_from_headers(headers);
    let options = match suite {
        Some(_) => websocket_options().without_compression(),
        None => websocket_options(),
    };
    let Ok((mut response, upgraded)) = ws.upgrade(options) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if let Some(suite) = suite
        && let Ok(value) = header::HeaderValue::from_str(suite.ws_subprotocol())
    {
        response
            .headers_mut()
            .insert(header::SEC_WEBSOCKET_PROTOCOL, value);
    }
    tokio::spawn(async move {
        match upgraded.await {
            Ok(socket) => {
                transport.wire.activate();
                serve_socket(socket, state, route_kind, &transport, remote_addr, suite).await;
                transport.wire.deactivate();
            }
            Err(error) => warn!(%error, "websocket upgrade failed"),
        }
    });
    response.map(Body::new)
}

/// `Sec-WebSocket-Protocol` → 压缩套。子协议可能跨多个头或单头逗号分隔;
/// None 表示客户端未提供 teamviewrelay 子协议(旧客户端)。
fn ws_suite_from_headers(headers: &HeaderMap) -> Option<crate::compress::Suite> {
    let offered = headers
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|token| !token.is_empty());
    crate::compress::Suite::select_ws_subprotocol(offered)
}

fn websocket_options() -> Options {
    Options::default()
        .with_balanced_compression()
        .with_limits(16 * 1024 * 1024, 32 * 1024 * 1024)
        .with_backpressure_boundary(32 * 1024 * 1024)
}

pub async fn serve_web_map_session<Si>(
    mut socket: WebMapFrameStream,
    mut writer_sink: Si,
    state: AppState,
    remote_addr: String,
    movement_tx: watch::Sender<Option<Arc<MovementBatch>>>,
    datagram_capable: bool,
) where
    Si: Sink<Bytes, Error = io::Error> + Unpin + Send + 'static,
{
    let first = match tokio::time::timeout(Duration::from_secs(10), socket.next()).await {
        Ok(Some(Ok(frame))) if !frame.is_empty() => frame,
        _ => {
            return;
        }
    };
    let envelope = match WireEnvelope::decode(first.clone()) {
        Ok(envelope) => envelope,
        Err(error) => {
            warn!(%error, "invalid protobuf handshake");
            return;
        }
    };

    let (
        id,
        room,
        profile,
        kind,
        channel,
        display_name,
        position_resolution,
        program_version,
        complete_online_roster,
        unreliable_positions,
    ) = match envelope.payload {
        // 无 path 的门(WT/QUIC)靠首个握手的载荷类型自识别 Player/WebMap 通道
        Some(wire_envelope::Payload::PlayerHandshakeRequest(handshake)) => {
            if handshake.submit_player_id.is_empty() {
                door_reject(
                    &mut writer_sink,
                    WireChannel::Player,
                    "invalid_submit_player_id",
                )
                .await;
                return;
            }
            let profile = match ProtocolProfile::negotiate(
                &handshake.network_protocol_version,
                &handshake.minimum_compatible_network_protocol_version,
            ) {
                Ok(profile) => profile,
                Err(error) => {
                    door_reject(&mut writer_sink, WireChannel::Player, error.reason()).await;
                    return;
                }
            };
            let role = if profile.supports_external_source_role()
                && matches!(
                    handshake
                        .client_role
                        .and_then(|value| ClientRole::try_from(value).ok()),
                    Some(ClientRole::ExternalSource)
                ) {
                ConnectionKind::ExternalSource
            } else {
                ConnectionKind::Player
            };
            let complete_online_roster = role == ConnectionKind::ExternalSource
                && handshake
                    .external_source_capabilities
                    .as_ref()
                    .is_some_and(|caps| {
                        caps.datasets.iter().any(|dataset| {
                            dataset.coverage
                                == crate::proto::teamviewer::v1::DatasetCoverage::Complete as i32
                                && dataset.scopes.contains(
                                    &(crate::proto::teamviewer::v1::ExternalDataScope::OnlineRoster
                                        as i32),
                                )
                        })
                    });
            (
                handshake.submit_player_id,
                normalize_room(handshake.room_code.as_deref()),
                profile,
                role,
                WireChannel::Player,
                handshake.client_display_name,
                handshake.position_resolution,
                handshake.local_program_version,
                complete_online_roster,
                // 玩家连接是位置的产出方,不做 movement datagram 分流
                false,
            )
        }
        Some(wire_envelope::Payload::WebMapHandshakeRequest(handshake)) => {
            let profile = match ProtocolProfile::negotiate(
                &handshake.network_protocol_version,
                &handshake.minimum_compatible_network_protocol_version,
            ) {
                Ok(profile) => profile,
                Err(_error) => return,
            };
            // 客户端声明可消费 movement datagram(0.9.0 列表或 0.8.1 布尔映射)
            // 且 本连接 datagram 预算装得下 movement 块
            let unreliable_positions = accepts_unreliable_channels(&handshake)
                .contains(&UnreliableChannel::Movement)
                && datagram_capable;
            (
                format!("web-map-{}", Uuid::new_v4()),
                normalize_room(handshake.room_code.as_deref()),
                profile,
                ConnectionKind::WebMap,
                WireChannel::WebMap,
                Some("Web Map".to_owned()),
                None,
                handshake.local_program_version,
                false,
                unreliable_positions,
            )
        }
        _ => {
            return;
        }
    };

    let maintenance_guard = state.maintenance_rooms.clone().read_owned().await;
    if maintenance_guard.contains(&room) {
        return;
    }
    let traffic_channel = match channel {
        WireChannel::WebMap => TrafficChannel::WebMap,
        _ => TrafficChannel::Player,
    };
    let accepted_role = match kind {
        ConnectionKind::ExternalSource => ClientRole::ExternalSource,
        _ => ClientRole::Player,
    };
    state.metrics.register(&id, traffic_channel);
    let ack = HandshakeAck {
        ready: true,
        network_protocol_version: CURRENT_PROTOCOL_VERSION.to_string(),
        minimum_compatible_network_protocol_version: MINIMUM_PROTOCOL_VERSION.to_string(),
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
        accepted_client_role: profile
            .supports_external_source_role()
            .then_some(accepted_role as i32),
        tab_history: profile
            .supports_tab_history()
            .then(|| tab_capabilities(&state.config)),
        relationship_query: profile.supports_relationships().then_some(
            RelationshipQueryCapabilities {
                supported: true,
                max_selectors: 256,
                default_chunk_entries: 256,
                max_chunk_entries: 256,
                max_chunk_bytes: 256 * 1024,
                relation_kinds: vec![1, 2, 3, 4, 5],
            },
        ),
        report_policy: profile
            .supports_relationships()
            .then(|| report_policy(false)),
        ..Default::default()
    };
    let ack = encode_payload(channel, wire_envelope::Payload::HandshakeAck(ack));
    let ack_len = ack.len();
    if tokio::time::timeout(
        Duration::from_secs(2),
        writer_sink.send(Bytes::from_owner(ack)),
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
            protocol: profile,
            kind,
            display_name,
            position_resolution,
            program_version,
            remote_addr: remote_addr.clone(),
            control: control_tx.clone(),
            state: state_tx,
            movement: movement_tx,
            unreliable_positions,
        }))
        .await
        .is_err()
    {
        state.metrics.unregister(&id);
        return;
    }
    drop(maintenance_guard);
    record_connection_started(&state.db, &id, &room, kind, &remote_addr).await;
    info!(connection_id = %id, %room, ?kind, protocol = %profile.peer_current(), %remote_addr, "door session connected");

    let event_tx = state.relay.sender();
    let writer_events = event_tx.clone();
    let mut writer = tokio::spawn(wt_writer_loop(
        writer_sink,
        control_rx,
        state_rx,
        WriterContext {
            id: id.clone(),
            events: writer_events,
            metrics: state.metrics.clone(),
            traffic_channel,
        },
    ));
    let reader_id = id.clone();
    let mut reader = tokio::spawn(wt_reader_loop(
        first,
        socket,
        ReaderContext {
            id: reader_id,
            room: room.clone(),
            control: control_tx,
            events: event_tx.clone(),
            channel,
            kind,
            tab_history: state.tab_history.clone(),
            relationships: state.relationships.clone(),
            metrics: state.metrics.clone(),
            traffic_channel,
            profile,
            complete_online_roster,
        },
    ));

    tokio::select! {
        _ = &mut writer => reader.abort(),
        _ = &mut reader => writer.abort(),
    }
    let _ = event_tx
        .send(RelayEvent::Disconnect { id: id.clone() })
        .await;
    record_connection_ended(&state.db, &id, &room, kind, &remote_addr).await;
    state.metrics.unregister(&id);
    info!(connection_id = %id, "door session disconnected");
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

async fn serve_socket(
    mut socket: HttpWebSocket,
    state: AppState,
    route_kind: ConnectionKind,
    transport: &TransportConnectInfo,
    remote_addr: String,
    suite: Option<crate::compress::Suite>,
) {
    // 每连接一个持久 zstd 压缩器:消息级 flush 块,跨消息共享压缩上下文。
    // zstd 套语义为单向:仅下行(服务端→客户端)压缩,上行恒为 plain 消息。
    let mut encoder = match suite.filter(|suite| suite.stream_zstd()) {
        Some(_) => match crate::compress::StreamEncoder::new() {
            Ok(encoder) => Some(encoder),
            Err(error) => {
                warn!(%error, "zstd encoder unavailable, closing connection");
                return;
            }
        },
        None => None,
    };
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

    let (
        id,
        room,
        profile,
        kind,
        channel,
        display_name,
        position_resolution,
        program_version,
        complete_online_roster,
    ) = match (route_kind, envelope.payload) {
        (
            ConnectionKind::Player,
            Some(wire_envelope::Payload::PlayerHandshakeRequest(handshake)),
        ) => {
            if handshake.submit_player_id.is_empty() {
                reject(&mut socket, WireChannel::Player, "invalid_submit_player_id").await;
                return;
            }
            let profile = match ProtocolProfile::negotiate(
                &handshake.network_protocol_version,
                &handshake.minimum_compatible_network_protocol_version,
            ) {
                Ok(profile) => profile,
                Err(error) => {
                    reject(&mut socket, WireChannel::Player, error.reason()).await;
                    return;
                }
            };
            let role = if profile.supports_external_source_role()
                && matches!(
                    handshake
                        .client_role
                        .and_then(|value| ClientRole::try_from(value).ok()),
                    Some(ClientRole::ExternalSource)
                ) {
                ConnectionKind::ExternalSource
            } else {
                ConnectionKind::Player
            };
            let complete_online_roster = role == ConnectionKind::ExternalSource
                && handshake
                    .external_source_capabilities
                    .as_ref()
                    .is_some_and(|caps| {
                        caps.datasets.iter().any(|dataset| {
                            dataset.coverage
                                == crate::proto::teamviewer::v1::DatasetCoverage::Complete as i32
                                && dataset.scopes.contains(
                                    &(crate::proto::teamviewer::v1::ExternalDataScope::OnlineRoster
                                        as i32),
                                )
                        })
                    });
            (
                handshake.submit_player_id,
                normalize_room(handshake.room_code.as_deref()),
                profile,
                role,
                WireChannel::Player,
                handshake.client_display_name,
                handshake.position_resolution,
                handshake.local_program_version,
                complete_online_roster,
            )
        }
        (
            ConnectionKind::WebMap,
            Some(wire_envelope::Payload::WebMapHandshakeRequest(handshake)),
        ) => {
            let profile = match ProtocolProfile::negotiate(
                &handshake.network_protocol_version,
                &handshake.minimum_compatible_network_protocol_version,
            ) {
                Ok(profile) => profile,
                Err(error) => {
                    reject(&mut socket, WireChannel::WebMap, error.reason()).await;
                    return;
                }
            };
            (
                format!("web-map-{}", Uuid::new_v4()),
                normalize_room(handshake.room_code.as_deref()),
                profile,
                ConnectionKind::WebMap,
                WireChannel::WebMap,
                Some("Web Map".to_owned()),
                None,
                handshake.local_program_version,
                false,
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
        network_protocol_version: CURRENT_PROTOCOL_VERSION.to_string(),
        minimum_compatible_network_protocol_version: MINIMUM_PROTOCOL_VERSION.to_string(),
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
        accepted_client_role: profile
            .supports_external_source_role()
            .then_some(accepted_role as i32),
        tab_history: profile
            .supports_tab_history()
            .then(|| tab_capabilities(&state.config)),
        relationship_query: profile.supports_relationships().then_some(
            RelationshipQueryCapabilities {
                supported: true,
                max_selectors: 256,
                default_chunk_entries: 256,
                max_chunk_entries: 256,
                max_chunk_bytes: 256 * 1024,
                relation_kinds: vec![1, 2, 3, 4, 5],
            },
        ),
        report_policy: profile
            .supports_relationships()
            .then(|| report_policy(false)),
        ..Default::default()
    };
    let ack = encode_payload(channel, wire_envelope::Payload::HandshakeAck(ack));
    let ack_len = ack.len();
    let ack_wire = match encoder.as_mut() {
        Some(encoder) => match encoder.compress_chunk(&ack) {
            Ok(chunk) => chunk,
            Err(error) => {
                warn!(%error, "ack compress failed");
                state.metrics.unregister(&id);
                return;
            }
        },
        None => Bytes::from_owner(ack),
    };
    if tokio::time::timeout(Duration::from_secs(2), socket.send(Frame::binary(ack_wire)))
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
    // WS 没有 datagram 通道:即便客户端声明可消费 movement datagram 也不启用分流
    let (movement_tx, _movement_rx) = watch::channel(None);
    if state
        .relay
        .send(RelayEvent::Register(RegisterConnection {
            id: id.clone(),
            room: room.clone(),
            protocol: profile,
            kind,
            display_name,
            position_resolution,
            program_version,
            remote_addr: remote_addr.clone(),
            control: control_tx.clone(),
            state: state_tx,
            movement: movement_tx,
            unreliable_positions: false,
        }))
        .await
        .is_err()
    {
        state.metrics.unregister(&id);
        return;
    }
    drop(maintenance_guard);
    record_connection_started(&state.db, &id, &room, kind, &remote_addr).await;
    info!(connection_id = %id, %room, ?kind, protocol = %profile.peer_current(), %remote_addr, "websocket connected");

    let (sink, stream) = socket.split();
    let event_tx = state.relay.sender();
    let writer_events = event_tx.clone();
    let mut writer = tokio::spawn(writer_loop(
        sink,
        control_rx,
        state_rx,
        WriterContext {
            id: id.clone(),
            events: writer_events,
            metrics: state.metrics.clone(),
            traffic_channel,
        },
        encoder,
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
            kind,
            tab_history: state.tab_history.clone(),
            relationships: state.relationships.clone(),
            metrics: state.metrics.clone(),
            traffic_channel,
            profile,
            complete_online_roster,
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

enum WriterAction {
    State(StateFrame),
    Control(Arc<[u8]>),
}

struct WriterContext {
    id: String,
    events: mpsc::Sender<RelayEvent>,
    metrics: Arc<Metrics>,
    traffic_channel: TrafficChannel,
}

async fn writer_loop<S, E>(
    mut sink: S,
    mut control: mpsc::Receiver<Arc<[u8]>>,
    mut state: watch::Receiver<Option<StateFrame>>,
    context: WriterContext,
    mut encoder: Option<crate::compress::StreamEncoder>,
) where
    S: Sink<Frame, Error = E> + Unpin,
{
    loop {
        let action = tokio::select! {
            biased;
            changed = state.changed() => {
                if changed.is_err() { None } else {
                    state.borrow_and_update().clone().map(WriterAction::State)
                }
            }
            value = control.recv() => value.map(WriterAction::Control),
        };
        let Some(action) = action else {
            break;
        };

        match action {
            WriterAction::State(frame) => {
                if let Some(bytes) = frame.bytes
                    && !send_writer_payload(
                        &context.id,
                        &mut sink,
                        bytes,
                        &context.metrics,
                        context.traffic_channel,
                        &mut encoder,
                    )
                    .await
                {
                    break;
                }

                if let Some(digest) = frame.digest
                    && !send_writer_payload(
                        &context.id,
                        &mut sink,
                        digest,
                        &context.metrics,
                        context.traffic_channel,
                        &mut encoder,
                    )
                    .await
                {
                    break;
                }

                let _ = context
                    .events
                    .send(RelayEvent::Delivered {
                        id: context.id.clone(),
                        revision: frame.revision,
                        snapshot: frame.snapshot,
                        battle_revision: frame.battle_revision,
                    })
                    .await;
            }
            WriterAction::Control(bytes) => {
                if !send_writer_payload(
                    &context.id,
                    &mut sink,
                    bytes,
                    &context.metrics,
                    context.traffic_channel,
                    &mut encoder,
                )
                .await
                {
                    break;
                }
            }
        }
    }
}

async fn send_writer_payload<S, E>(
    id: &str,
    sink: &mut S,
    bytes: Arc<[u8]>,
    metrics: &Metrics,
    traffic_channel: TrafficChannel,
    encoder: &mut Option<crate::compress::StreamEncoder>,
) -> bool
where
    S: Sink<Frame, Error = E> + Unpin,
{
    let byte_count = bytes.len();
    let message_type = protobuf_message_type(&bytes);
    // 计量按应用层 envelope 字节;压缩后实际线上字节由 Wire 层计数反映
    let payload = match encoder.as_mut() {
        Some(encoder) => match encoder.compress_chunk(&bytes) {
            Ok(chunk) => chunk,
            Err(error) => {
                warn!(connection_id = %id, %error, "egress compress failed");
                return false;
            }
        },
        None => Bytes::from_owner(bytes),
    };
    #[cfg(feature = "memory-debug")]
    let send_started = std::time::Instant::now();
    let sent =
        tokio::time::timeout(Duration::from_secs(2), sink.send(Frame::binary(payload))).await;
    let succeeded = matches!(sent, Ok(Ok(())));
    #[cfg(feature = "memory-debug")]
    metrics.record_writer_send(send_started.elapsed(), succeeded);
    if !succeeded {
        warn!(connection_id = %id, "slow or failed websocket writer disconnected");
        return false;
    }
    metrics.record(
        Layer::Application,
        traffic_channel,
        Direction::Egress,
        byte_count,
    );
    metrics.record_protobuf(id, message_type, byte_count);
    true
}

async fn wt_writer_loop<Si>(
    mut sink: Si,
    mut control: mpsc::Receiver<Arc<[u8]>>,
    mut state: watch::Receiver<Option<StateFrame>>,
    context: WriterContext,
) where
    Si: Sink<Bytes, Error = io::Error> + Unpin + Send,
{
    loop {
        let action = tokio::select! {
            biased;
            changed = state.changed() => { if changed.is_err() { None } else { state.borrow_and_update().clone().map(WriterAction::State) } }
            value = control.recv() => value.map(WriterAction::Control),
        };
        let Some(action) = action else { break };
        let (payloads, state_frame): (Vec<Arc<[u8]>>, Option<StateFrame>) = match action {
            WriterAction::State(frame) => {
                let mut payloads = Vec::new();
                if let Some(bytes) = frame.bytes.clone() {
                    payloads.push(bytes);
                }
                if let Some(digest) = frame.digest.clone() {
                    payloads.push(digest);
                }
                (payloads, Some(frame))
            }
            WriterAction::Control(bytes) => (vec![bytes], None),
        };
        let mut failed = false;
        for payload in payloads {
            let byte_count = payload.len();
            let message_type = protobuf_message_type(&payload);
            let sent = tokio::time::timeout(
                Duration::from_secs(2),
                sink.send(Bytes::from_owner(payload)),
            )
            .await;
            if !matches!(sent, Ok(Ok(()))) {
                failed = true;
                break;
            }
            context.metrics.record(
                Layer::Application,
                context.traffic_channel,
                Direction::Egress,
                byte_count,
            );
            context
                .metrics
                .record_protobuf(&context.id, message_type, byte_count);
        }
        if failed {
            break;
        }
        if let Some(frame) = state_frame {
            let _ = context
                .events
                .send(RelayEvent::Delivered {
                    id: context.id.clone(),
                    revision: frame.revision,
                    snapshot: frame.snapshot,
                    battle_revision: frame.battle_revision,
                })
                .await;
        }
    }
}

async fn wt_reader_loop(first_frame: Bytes, mut stream: WebMapFrameStream, context: ReaderContext) {
    let ReaderContext {
        id,
        room,
        control,
        events,
        channel,
        kind,
        tab_history,
        relationships,
        metrics,
        traffic_channel,
        profile,
        complete_online_roster,
    } = context;
    let first = futures_util::stream::iter(vec![Ok(first_frame)]);
    let mut frames = Box::pin(first.chain(&mut stream));
    while let Some(item) = frames.next().await {
        let Ok(frame) = item else { break };
        metrics.record(
            Layer::Application,
            traffic_channel,
            Direction::Ingress,
            frame.len(),
        );
        let Ok(envelope) = WireEnvelope::decode(frame.clone()) else {
            continue;
        };
        let mut ctx = ReaderContext {
            id: id.clone(),
            room: room.clone(),
            control: control.clone(),
            events: events.clone(),
            channel,
            kind,
            tab_history: tab_history.clone(),
            relationships: relationships.clone(),
            metrics: metrics.clone(),
            traffic_channel,
            profile,
            complete_online_roster,
        };
        if !wt_handle_envelope(envelope, &mut ctx).await {
            break;
        }
    }
}

async fn wt_handle_envelope(envelope: WireEnvelope, context: &mut ReaderContext) -> bool {
    match envelope.payload {
        Some(wire_envelope::Payload::ResyncRequest(_)) => context
            .events
            .send(RelayEvent::Resync {
                id: context.id.clone(),
            })
            .await
            .is_ok(),
        Some(wire_envelope::Payload::WebMapCommand(command))
            if context.channel == WireChannel::WebMap =>
        {
            context
                .events
                .send(RelayEvent::WebMapCommand {
                    id: context.id.clone(),
                    command,
                })
                .await
                .is_ok()
        }
        Some(wire_envelope::Payload::BattleChunkMetaRequest(request)) => context
            .events
            .send(RelayEvent::BattleChunkMeta {
                id: context.id.clone(),
                battle_chunks: request.battle_chunks.into_iter().take(256).collect(),
            })
            .await
            .is_ok(),
        Some(wire_envelope::Payload::TabHistorySubscribeRequest(request)) => {
            if !context.profile.supports_tab_history() {
                return true;
            }
            context
                .events
                .send(RelayEvent::TabHistorySubscription {
                    id: context.id.clone(),
                    enabled: request.enabled,
                })
                .await
                .is_ok()
        }
        Some(wire_envelope::Payload::TabHistorySyncRequest(request)) => {
            if !context.profile.supports_tab_history() {
                return true;
            }
            send_tab_sync(
                &context.tab_history,
                &context.control,
                context.channel,
                &context.room,
                request,
            )
            .await
            .is_ok()
        }
        Some(wire_envelope::Payload::TabHistoryLookupRequest(request)) => {
            if !context.profile.supports_tab_history() {
                return true;
            }
            send_tab_lookup(
                &context.tab_history,
                &context.control,
                context.channel,
                &context.room,
                request,
            )
            .await
            .is_ok()
        }
        Some(wire_envelope::Payload::Ping(_)) => {
            let pong = encode_payload(
                context.channel,
                wire_envelope::Payload::Pong(Pong {
                    server_time: unix_seconds(),
                }),
            );
            context.control.send(pong).await.is_ok()
        }
        _ => true,
    }
}

struct ReaderContext {
    id: String,
    room: String,
    control: mpsc::Sender<Arc<[u8]>>,
    events: mpsc::Sender<RelayEvent>,
    channel: WireChannel,
    kind: ConnectionKind,
    tab_history: Arc<TabHistoryStore>,
    relationships: Arc<RelationshipStore>,
    metrics: Arc<Metrics>,
    traffic_channel: TrafficChannel,
    profile: ProtocolProfile,
    complete_online_roster: bool,
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
        kind,
        tab_history,
        relationships,
        metrics,
        traffic_channel,
        profile,
        complete_online_roster,
    } = context;
    while let Some(frame) = stream.next().await {
        if frame.opcode() != OpCode::Binary {
            if frame.opcode() == OpCode::Close {
                break;
            }
            continue;
        }
        // 上行恒为 plain 消息(zstd 套仅压缩下行)
        let payload = frame.payload().clone();
        metrics.record(
            Layer::Application,
            traffic_channel,
            Direction::Ingress,
            payload.len(),
        );
        let Ok(envelope) = WireEnvelope::decode(payload) else {
            continue;
        };
        match envelope.payload {
            Some(wire_envelope::Payload::PlayerReportBundle(mut report))
                if report.submit_player_id == id =>
            {
                sanitize_player_report(profile, &mut report);
                if kind == ConnectionKind::ExternalSource
                    && complete_online_roster
                    && let Some(status) = &report.external_source_status
                {
                    let healthy = status.health
                        == crate::proto::teamviewer::v1::ExternalSourceHealth::Healthy as i32;
                    let _ = events
                        .send(RelayEvent::ReportPolicyUpdate {
                            room: room.clone(),
                            policy: report_policy(healthy),
                        })
                        .await;
                }
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
                let changed_history_head =
                    if !profile.supports_tab_history() || tab_players.is_empty() {
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
                if !profile.supports_tab_history() {
                    continue;
                }
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
                let result = if profile.supports_tab_history() {
                    send_tab_sync(&tab_history, &control, channel, &room, request).await
                } else {
                    send_tab_sync_error(
                        &control,
                        channel,
                        request.request_id,
                        TabHistoryErrorCode::Unsupported,
                    )
                    .await
                };
                if result.is_err() {
                    break;
                }
            }
            Some(wire_envelope::Payload::TabHistoryLookupRequest(request)) => {
                let result = if profile.supports_tab_history() {
                    send_tab_lookup(&tab_history, &control, channel, &room, request).await
                } else {
                    send_tab_lookup_error(
                        &control,
                        channel,
                        request.request_id,
                        TabHistoryErrorCode::Unsupported,
                    )
                    .await
                };
                if result.is_err() {
                    break;
                }
            }
            Some(wire_envelope::Payload::ExternalDatasetPublish(publish))
                if kind == ConnectionKind::ExternalSource =>
            {
                if !profile.supports_relationships() {
                    continue;
                }
                let Some(descriptor) = publish.descriptor.clone() else {
                    continue;
                };
                if descriptor.realm_id != room {
                    continue;
                }
                let mut accepted = false;
                let mut error_detail = None;
                match publish.update {
                    Some(crate::proto::teamviewer::v1::external_dataset_publish::Update::Status(status)) => {
                        let health = status.health;
                        if let Err(error) = relationships.set_health(&descriptor, health, unix_millis()).await {
                            error_detail = Some(error.to_string());
                        } else {
                            accepted = true;
                        }
                    }
                    Some(crate::proto::teamviewer::v1::external_dataset_publish::Update::RelationshipSnapshotChunk(chunk)) => {
                        if chunk.head.is_some() {
                            let compatibility_tabs = chunk.players.iter().filter_map(|player| player.compatibility_tab_entry.clone()).collect::<Vec<_>>();
                            if let Err(error) = relationships.upsert_chunk(&descriptor, &chunk, unix_millis()).await {
                                error_detail = Some(error.to_string());
                            } else {
                                accepted = true;
                                if !compatibility_tabs.is_empty()
                                    && matches!(tab_history.upsert_authoritative_players(&room, &compatibility_tabs, unix_millis()).await, Ok(true))
                                    && let Ok(head) = tab_history.head(&room).await
                                {
                                    let _ = events.send(RelayEvent::TabHistoryChanged { room: room.clone(), head }).await;
                                }
                            }
                        }
                    }
                    _ => error_detail = Some("missing_dataset_update".to_owned()),
                }
                let ack = ExternalDatasetPublishAck {
                    request_id: publish.request_id,
                    dataset_id: descriptor.dataset_id,
                    accepted,
                    accepted_revision: None,
                    error_code: (!accepted).then_some(
                        crate::proto::teamviewer::v1::ExternalDatasetPublishErrorCode::Internal
                            as i32,
                    ),
                    error_detail,
                };
                if control
                    .send(encode_payload(
                        channel,
                        wire_envelope::Payload::ExternalDatasetPublishAck(ack),
                    ))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Some(wire_envelope::Payload::PlayerDirectoryLookupRequest(request))
                if profile.supports_relationships() =>
            {
                if request.selectors.len() > 256 {
                    continue;
                }
                let entries = relationships
                    .lookup(
                        &room,
                        request.dataset_id.as_deref(),
                        &request.selectors,
                        unix_millis(),
                    )
                    .await
                    .unwrap_or_default();
                let results = entries
                    .into_iter()
                    .enumerate()
                    .map(|(index, entries)| PlayerDirectoryLookupResult {
                        selector_index: index as u32,
                        entries,
                        error_code: None,
                    })
                    .collect::<Vec<_>>();
                let limit = request.max_chunk_entries.unwrap_or(256).clamp(1, 256) as usize;
                let chunk_count = results.len().max(1).div_ceil(limit) as u32;
                for index in 0..chunk_count {
                    let start = index as usize * limit;
                    let end = (start + limit).min(results.len());
                    let chunk = PlayerDirectoryLookupChunk {
                        request_id: request.request_id.clone(),
                        results: results.get(start..end).unwrap_or_default().to_vec(),
                        chunk_index: index,
                        chunk_count,
                        r#final: index + 1 == chunk_count,
                        error_code: None,
                        error_detail: None,
                    };
                    if control
                        .send(encode_payload(
                            channel,
                            wire_envelope::Payload::PlayerDirectoryLookupChunk(chunk),
                        ))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
            Some(wire_envelope::Payload::PlayerRelationQueryRequest(request))
                if profile.supports_relationships() =>
            {
                let (subjects, results) = relationships
                    .relations(
                        &room,
                        request.dataset_id.as_deref(),
                        request.subject.as_ref().unwrap_or(
                            &crate::proto::teamviewer::v1::PlayerSelector { selector: None },
                        ),
                        &request.targets,
                        unix_millis(),
                    )
                    .await
                    .unwrap_or_default();
                let limit = request.max_chunk_entries.unwrap_or(256).clamp(1, 256) as usize;
                let filtered = results
                    .into_iter()
                    .filter(|result| {
                        request.include_relations.is_empty()
                            || request.include_relations.contains(&result.relation)
                    })
                    .collect::<Vec<_>>();
                let chunk_count = filtered.len().max(1).div_ceil(limit) as u32;
                for index in 0..chunk_count {
                    let start = index as usize * limit;
                    let end = (start + limit).min(filtered.len());
                    let chunk = PlayerRelationQueryChunk {
                        request_id: request.request_id.clone(),
                        subjects: if index == 0 {
                            subjects.clone()
                        } else {
                            Vec::new()
                        },
                        results: filtered.get(start..end).unwrap_or_default().to_vec(),
                        chunk_index: index,
                        chunk_count,
                        r#final: index + 1 == chunk_count,
                        error_code: if subjects.len() > 1 {
                            Some(
                                crate::proto::teamviewer::v1::RelationshipQueryErrorCode::Ambiguous
                                    as i32,
                            )
                        } else if subjects.is_empty() {
                            Some(
                                crate::proto::teamviewer::v1::RelationshipQueryErrorCode::NotFound
                                    as i32,
                            )
                        } else {
                            None
                        },
                        error_detail: None,
                    };
                    if control
                        .send(encode_payload(
                            channel,
                            wire_envelope::Payload::PlayerRelationQueryChunk(chunk),
                        ))
                        .await
                        .is_err()
                    {
                        break;
                    }
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
        network_protocol_version: CURRENT_PROTOCOL_VERSION.to_string(),
        minimum_compatible_network_protocol_version: MINIMUM_PROTOCOL_VERSION.to_string(),
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

/// 无 path 门(WT/QUIC)会话的握手拒绝:回一帧 ready=false 的握手应答后直接结束会话,
/// 连接随会话帧流 drop 而关闭。
async fn door_reject<Si>(writer_sink: &mut Si, channel: WireChannel, reason: &str)
where
    Si: Sink<Bytes, Error = io::Error> + Unpin + Send,
{
    let ack = HandshakeAck {
        ready: false,
        network_protocol_version: CURRENT_PROTOCOL_VERSION.to_string(),
        minimum_compatible_network_protocol_version: MINIMUM_PROTOCOL_VERSION.to_string(),
        local_program_version: PROGRAM_VERSION.to_owned(),
        error: Some("version_incompatible".to_owned()),
        reject_reason: Some(reason.to_owned()),
        ..Default::default()
    };
    let bytes = encode_payload(channel, wire_envelope::Payload::HandshakeAck(ack));
    let _ = writer_sink.send(Bytes::from_owner(bytes)).await;
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

fn report_policy(suppress_tab: bool) -> crate::proto::teamviewer::v1::PlayerReportPolicy {
    use crate::proto::teamviewer::v1::{
        PlayerReportPolicy, ReportRecommendation, ReportRecommendationMode, ReportScope,
    };
    PlayerReportPolicy {
        recommendations: vec![
            ReportRecommendation {
                scope: ReportScope::Tab as i32,
                mode: if suppress_tab {
                    ReportRecommendationMode::Suppress as i32
                } else {
                    ReportRecommendationMode::Report as i32
                },
                reason: if suppress_tab {
                    "healthy complete external online roster is active"
                } else {
                    "no healthy complete external online roster is active"
                }
                .to_owned(),
                replacement_dataset_id: suppress_tab.then_some("external-online-roster".to_owned()),
            },
            ReportRecommendation {
                scope: ReportScope::Positions as i32,
                mode: ReportRecommendationMode::Report as i32,
                reason: "continue player position reporting".to_owned(),
                replacement_dataset_id: None,
            },
        ],
        generated_at_utc_ms: unix_millis(),
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

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        pin::Pin,
        task::{Context, Poll},
    };

    use super::*;
    use crate::proto::teamviewer::v1::SnapshotFull;

    struct RecordingSink {
        frames: mpsc::UnboundedSender<Vec<u8>>,
    }

    impl Sink<Frame> for RecordingSink {
        type Error = Infallible;

        fn poll_ready(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, frame: Frame) -> Result<(), Self::Error> {
            let _ = self.frames.send(frame.into_payload().to_vec());
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn writer_orders_each_state_before_its_matching_digest() {
        let (frames_tx, mut frames_rx) = mpsc::unbounded_channel();
        let sink = RecordingSink { frames: frames_tx };
        let (control_tx, control_rx) = mpsc::channel(4);
        let (state_tx, state_rx) = watch::channel(None::<StateFrame>);
        let (events_tx, mut events_rx) = mpsc::channel(4);

        control_tx
            .send(Arc::from(&b"control"[..]))
            .await
            .expect("control channel");
        let first_snapshot = Arc::new(SnapshotFull {
            server_time: Some(1.0),
            ..Default::default()
        });
        state_tx.send_replace(Some(StateFrame {
            revision: 1,
            bytes: Some(Arc::from(&b"state-1"[..])),
            digest: Some(Arc::from(&b"digest-1"[..])),
            snapshot: first_snapshot.clone(),
            battle_revision: 1,
        }));

        let writer = tokio::spawn(writer_loop(
            sink,
            control_rx,
            state_rx,
            WriterContext {
                id: "test".to_owned(),
                events: events_tx,
                metrics: Arc::new(Metrics::default()),
                traffic_channel: TrafficChannel::Player,
            },
            None,
        ));

        assert_eq!(frames_rx.recv().await.as_deref(), Some(&b"state-1"[..]));
        assert_eq!(frames_rx.recv().await.as_deref(), Some(&b"digest-1"[..]));
        assert_eq!(frames_rx.recv().await.as_deref(), Some(&b"control"[..]));

        let RelayEvent::Delivered {
            revision, snapshot, ..
        } = events_rx.recv().await.expect("delivered event")
        else {
            panic!("expected delivered event");
        };
        assert_eq!(revision, 1);
        assert!(Arc::ptr_eq(&snapshot, &first_snapshot));

        state_tx.send_replace(Some(StateFrame {
            revision: 2,
            bytes: Some(Arc::from(&b"state-2"[..])),
            digest: Some(Arc::from(&b"digest-2"[..])),
            snapshot: Arc::new(SnapshotFull {
                server_time: Some(2.0),
                ..Default::default()
            }),
            battle_revision: 2,
        }));
        assert_eq!(frames_rx.recv().await.as_deref(), Some(&b"state-2"[..]));
        assert_eq!(frames_rx.recv().await.as_deref(), Some(&b"digest-2"[..]));

        writer.abort();
    }

    /// WS 门 +zstd 套:writer_loop 出站的每条消息必须是可独立解码的 zstd
    /// 块(逐 envelope flush),按序还原出原 envelope。
    #[tokio::test(start_paused = true)]
    async fn writer_compresses_each_message_with_persistent_zstd_context() {
        let (frames_tx, mut frames_rx) = mpsc::unbounded_channel();
        let sink = RecordingSink { frames: frames_tx };
        let (control_tx, control_rx) = mpsc::channel(4);
        let (state_tx, state_rx) = watch::channel(None::<StateFrame>);
        let (events_tx, mut events_rx) = mpsc::channel(4);
        let metrics = Arc::new(Metrics::default());

        let first_payload = vec![9u8; 4096];
        let second_payload = b"control-frame".to_vec();
        control_tx
            .send(Arc::from(&second_payload[..]))
            .await
            .expect("control channel");
        state_tx.send_replace(Some(StateFrame {
            revision: 1,
            bytes: Some(Arc::from(&first_payload[..])),
            digest: None,
            snapshot: Arc::new(SnapshotFull {
                server_time: Some(1.0),
                ..Default::default()
            }),
            battle_revision: 1,
        }));

        let encoder = crate::compress::StreamEncoder::new().expect("zstd encoder");
        let writer = tokio::spawn(writer_loop(
            sink,
            control_rx,
            state_rx,
            WriterContext {
                id: "test-zstd".to_owned(),
                events: events_tx,
                metrics: metrics.clone(),
                traffic_channel: TrafficChannel::Player,
            },
            Some(encoder),
        ));

        let mut decoder = crate::compress::StreamDecoder::new().expect("zstd decoder");
        for expected in [first_payload, second_payload] {
            let chunk = frames_rx.recv().await.expect("compressed frame");
            assert_ne!(chunk.as_slice(), &expected[..], "压缩块不应与原文字节相同");
            // 逐 envelope flush:每块恰好解出本条 envelope
            let decoded = decoder.decompress_chunk(&chunk).expect("decompress");
            assert_eq!(decoded, expected);
        }

        let RelayEvent::Delivered { revision, .. } =
            events_rx.recv().await.expect("delivered event")
        else {
            panic!("expected delivered event");
        };
        assert_eq!(revision, 1);
        writer.abort();
    }

    /// WS 子协议 → 压缩套:多值/逗号合并头都按客户端偏好序解析,
    /// 无 teamviewrelay 子协议返回 None(旧客户端维持 deflate 路径)。
    #[test]
    fn ws_suite_negotiation_parses_all_header_shapes() {
        use axum::http::header::SEC_WEBSOCKET_PROTOCOL;

        let mut headers = HeaderMap::new();
        assert_eq!(ws_suite_from_headers(&headers), None);

        headers.insert(
            SEC_WEBSOCKET_PROTOCOL,
            header::HeaderValue::from_static("chat.supergame, teamviewrelay.zstd.v1"),
        );
        assert_eq!(
            ws_suite_from_headers(&headers),
            Some(crate::compress::Suite::Zstd)
        );

        let mut headers = HeaderMap::new();
        headers.append(
            SEC_WEBSOCKET_PROTOCOL,
            header::HeaderValue::from_static("teamviewrelay.zstd.v1"),
        );
        headers.append(
            SEC_WEBSOCKET_PROTOCOL,
            header::HeaderValue::from_static("teamviewrelay.plain.v1"),
        );
        assert_eq!(
            ws_suite_from_headers(&headers),
            Some(crate::compress::Suite::Zstd)
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            SEC_WEBSOCKET_PROTOCOL,
            header::HeaderValue::from_static("teamviewrelay.zstd-dict.v1"),
        );
        assert_eq!(
            ws_suite_from_headers(&headers),
            Some(crate::compress::Suite::ZstdDict)
        );
    }

    /// 无 path 门(WT/QUIC)会话接受 Player 握手:mod 经裸 QUIC 门以玩家身份连接的
    /// 应用层合同——首帧 PlayerHandshakeRequest → 下行流 ready=true 的 HandshakeAck。
    #[tokio::test]
    async fn door_session_accepts_player_handshake_and_answers_ready_ack() {
        use crate::proto::teamviewer::v1::{PlayerHandshakeRequest, wire_envelope};
        use crate::web_transport::{MpscReceiver, MpscSink};

        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory database");
        sqlx::migrate!().run(&db).await.expect("migrations");
        let tab_history = Arc::new(TabHistoryStore::new(db.clone()));
        tab_history.initialize().await.expect("tab history schema");
        let relationships = Arc::new(RelationshipStore::new(db.clone()));
        relationships
            .initialize()
            .await
            .expect("relationship schema");
        let config = Arc::new(RuntimeConfig::load());
        let state = AppState {
            relay: RelayHandle::spawn(config.clone()),
            db,
            tab_history,
            relationships,
            config,
            metrics: Arc::new(Metrics::default()),
            maintenance_rooms: Arc::new(RwLock::new(HashSet::new())),
            bulk_hub: crate::bulk::Hub::new(),
            #[cfg(feature = "memory-debug")]
            resource_debug: None,
        };

        // 入站帧流与出站帧收集:门桥接层已消化 varint 分帧,一帧 = 裸 envelope
        let (inbound_tx, inbound_rx) = mpsc::channel::<Result<Bytes, io::Error>>(4);
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<Bytes>(4);
        let stream = WebMapFrameStream::new(MpscReceiver::new(inbound_rx));
        let sink = MpscSink::new(outbound_tx);
        let (movement_tx, _movement_rx) = watch::channel(None);

        let session = tokio::spawn(serve_web_map_session(
            stream,
            sink,
            state,
            "test-remote".to_owned(),
            movement_tx,
            false,
        ));

        let handshake = PlayerHandshakeRequest {
            submit_player_id: "door-player".to_owned(),
            network_protocol_version: CURRENT_PROTOCOL_VERSION.to_string(),
            minimum_compatible_network_protocol_version: MINIMUM_PROTOCOL_VERSION.to_string(),
            room_code: Some("door-room".to_owned()),
            ..Default::default()
        };
        let envelope = WireEnvelope {
            channel: WireChannel::Player as i32,
            payload: Some(wire_envelope::Payload::PlayerHandshakeRequest(handshake)),
        };
        inbound_tx
            .send(Ok(Bytes::from(envelope.encode_to_vec())))
            .await
            .expect("inbound frame");

        let ack_bytes = tokio::time::timeout(Duration::from_secs(5), outbound_rx.recv())
            .await
            .expect("ack timeout")
            .expect("ack frame");
        let ack_envelope = WireEnvelope::decode(ack_bytes).expect("ack envelope");
        assert_eq!(ack_envelope.channel, WireChannel::Player as i32);
        let Some(wire_envelope::Payload::HandshakeAck(ack)) = ack_envelope.payload else {
            panic!("expected handshake ack");
        };
        assert!(ack.ready);
        assert_eq!(ack.network_protocol_version, "0.9.0");
        assert_eq!(ack.room_code, "door-room");
        assert_eq!(ack.accepted_client_role, Some(ClientRole::Player as i32));

        session.abort();
    }
}
