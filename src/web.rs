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
        CONTROL_CAPACITY, ConnectionKind, RegisterConnection, RelayEvent,
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
                // 桥接模式:WS 门桥把 socket 泵成稳定信道,会话层与
                // WT/QUIC 门共享同一实现
                if let Some(session) =
                    ws_socket_bridge(socket, suite, route_kind, &transport, &state).await
                {
                    serve_web_map_session(session, state, remote_addr).await;
                }
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

/// WS 门桥(桥接模式收编):把 yawc WebSocket 泵成稳定信道 `DoorSession`,
/// 对上与 WT/QUIC 门桥交付同一形态,会话层由 `serve_web_map_session` 统一承载。
/// 门差异封在本桥内:URL 路由通道前门(WS 有 path,不像无 path 门自识别)、
/// WS 关闭帧诊断、下行 zstd 压缩(消息级 flush)、Wire 层 TCP 字节计量。
/// 返回 `None` = 握手被前门拒绝(关闭帧已发出,连接即弃)。
async fn ws_socket_bridge(
    mut socket: HttpWebSocket,
    suite: Option<crate::compress::Suite>,
    route_kind: ConnectionKind,
    transport: &TransportConnectInfo,
    state: &AppState,
) -> Option<crate::door::DoorSession> {
    // 握手前门:首帧必须是指定时间内到达的 binary 帧且载荷可解码,
    // 载荷类型必须与路由通道匹配(错配即 channel_mismatch)
    let first = match tokio::time::timeout(Duration::from_secs(10), socket.next()).await {
        Ok(Some(frame)) if frame.opcode() == OpCode::Binary => frame.payload().clone(),
        _ => {
            let _ = socket
                .send(Frame::close(
                    yawc::close::CloseCode::Policy,
                    "handshake_required",
                ))
                .await;
            return None;
        }
    };
    let envelope = match WireEnvelope::decode(first.clone()) {
        Ok(envelope) => envelope,
        Err(error) => {
            warn!(%error, "invalid protobuf handshake");
            let _ = socket
                .send(Frame::close(
                    yawc::close::CloseCode::Policy,
                    "invalid_payload",
                ))
                .await;
            return None;
        }
    };
    let matches_route = matches!(
        (&envelope.payload, route_kind),
        (
            Some(wire_envelope::Payload::PlayerHandshakeRequest(_)),
            ConnectionKind::Player
        ) | (
            Some(wire_envelope::Payload::WebMapHandshakeRequest(_)),
            ConnectionKind::WebMap
        )
    );
    if !matches_route {
        reject(&mut socket, channel_for(route_kind), "channel_mismatch").await;
        return None;
    }

    // 下行 zstd 压缩器:消息级 flush 块,跨消息共享压缩上下文。zstd 套语义
    // 为单向:仅下行(服务端→客户端)压缩,上行恒为 plain 消息。
    let encoder = match suite.filter(|suite| suite.stream_zstd()) {
        Some(_) => match crate::compress::StreamEncoder::new(state.config.zstd_compression_level)
        {
            Ok(encoder) => Some(encoder),
            Err(error) => {
                warn!(%error, "zstd encoder unavailable, closing connection");
                return None;
            }
        },
        None => None,
    };

    let (incoming_tx, incoming_rx) = mpsc::channel::<Result<Bytes, io::Error>>(256);
    let (outgoing_tx, outgoing_rx) = mpsc::channel::<Bytes>(256);
    let (movement_tx, _movement_rx) = watch::channel(None);

    let (sink, mut stream) = socket.split();
    // 入站泵:上行恒 plain,非 binary 帧跳过(Close 帧即会话终结);
    // 首帧已过前门,补进出站流保持 envelope 次序
    let reader_incoming_tx = incoming_tx.clone();
    let reader = tokio::spawn(async move {
        if reader_incoming_tx.send(Ok(first)).await.is_err() {
            return;
        }
        while let Some(frame) = stream.next().await {
            let keep_going = match frame.opcode() {
                OpCode::Binary => reader_incoming_tx
                    .send(Ok(frame.payload().clone()))
                    .await
                    .is_ok(),
                OpCode::Close => false,
                _ => true,
            };
            if !keep_going {
                break;
            }
        }
    });

    // 出站泵:plain envelope → 持久 CCtx flush 压缩块 → WS binary 帧。
    // 出站泵是本门会话的"心跳":它退出(压缩失败/写出失败/socket 死亡)时
    // abort 入站泵,会话随之拆除——与 QUIC/WT 门桥的 reader.abort() 同构。
    // Wire 层(TCP 字节)计量随泵起止采样,退场时落最终增量。
    let traffic_channel = match route_kind {
        ConnectionKind::WebMap => TrafficChannel::WebMap,
        _ => TrafficChannel::Player,
    };
    let wire = transport.wire.clone();
    let metrics = state.metrics.clone();
    tokio::spawn(async move {
        let last_wire_ingress = Arc::new(AtomicU64::new(0));
        let last_wire_egress = Arc::new(AtomicU64::new(0));
        let wire_sampler = tokio::spawn(sample_wire_traffic(
            wire.clone(),
            metrics.clone(),
            traffic_channel,
            last_wire_ingress.clone(),
            last_wire_egress.clone(),
        ));
        ws_egress_loop(sink, outgoing_rx, encoder, metrics.clone()).await;
        reader.abort();
        wire_sampler.abort();
        let (wire_ingress, wire_egress) = wire.totals();
        metrics.record(
            Layer::Wire,
            traffic_channel,
            Direction::Ingress,
            wire_ingress.saturating_sub(last_wire_ingress.load(Ordering::Relaxed)) as usize,
        );
        metrics.record(
            Layer::Wire,
            traffic_channel,
            Direction::Egress,
            wire_egress.saturating_sub(last_wire_egress.load(Ordering::Relaxed)) as usize,
        );
    });

    Some(crate::door::DoorSession {
        incoming: crate::web::WebMapFrameStream::new(crate::web_transport::MpscReceiver::new(
            incoming_rx,
        )),
        outgoing: crate::web_transport::MpscSink::new(outgoing_tx),
        movement_tx,
        // WS 门无多流/datagram 能力,bulk 触发与上行位置通道不可见
        bulk: None,
        incoming_datagrams: None,
        capabilities: crate::door::DoorCapabilities {
            datagram: false,
            door_control: false,
            bulk: false,
            wire_metrics: true,
        },
    })
}

/// WS 门桥出站泵:每条消息独立成帧(zstd 套下为可独立解码的压缩块),
/// 写出超时 2s 判慢速/死亡。应用层 envelope 计量由会话层 door_writer_loop
/// 负责(那里才有连接 id),本泵只做门原生的压缩与分帧。
async fn ws_egress_loop<S, E>(
    mut sink: S,
    mut outgoing: mpsc::Receiver<Bytes>,
    mut encoder: Option<crate::compress::StreamEncoder>,
    metrics: Arc<Metrics>,
) where
    S: Sink<Frame, Error = E> + Unpin,
{
    #[cfg(not(feature = "memory-debug"))]
    let _ = &metrics;
    while let Some(payload) = outgoing.recv().await {
        let wire = match encoder.as_mut() {
            Some(encoder) => match encoder.compress_chunk(&payload) {
                Ok(chunk) => chunk,
                Err(error) => {
                    warn!(%error, "egress compress failed");
                    return;
                }
            },
            None => payload,
        };
        #[cfg(feature = "memory-debug")]
        let send_started = std::time::Instant::now();
        let sent =
            tokio::time::timeout(Duration::from_secs(2), sink.send(Frame::binary(wire))).await;
        let succeeded = matches!(sent, Ok(Ok(())));
        #[cfg(feature = "memory-debug")]
        metrics.record_writer_send(send_started.elapsed(), succeeded);
        if !succeeded {
            warn!("slow or failed websocket writer disconnected");
            return;
        }
    }
}

pub(crate) async fn serve_web_map_session(
    session: crate::door::DoorSession,
    state: AppState,
    remote_addr: String,
) {
    // 稳定信道解构(桥接模式):上层只依赖 DoorSession,门差异封在桥内
    let crate::door::DoorSession {
        incoming: mut socket,
        outgoing: mut writer_sink,
        movement_tx,
        incoming_datagrams,
        capabilities: crate::door::DoorCapabilities { datagram: datagram_capable, .. },
        ..
    } = session;
    let first = match tokio::time::timeout(Duration::from_secs(10), socket.next()).await {
        Ok(Some(Ok(frame))) if !frame.is_empty() => frame,
        _ => {
            warn!(%remote_addr, "door handshake first frame missing (client silent for 10s)");
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
        unreliable_channels_accepted,
        downlink_channels_accepted,
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
            // 上行位置通道(alpha.5):客户端声明将经 datagram 上送的通道;
            // 实际启用 = 声明 ∩ 已知值 ∩ 本门具备 datagram 接收能力
            let unreliable_channels_accepted: Vec<UnreliableChannel> =
                if incoming_datagrams.is_some() {
                    handshake
                        .unreliable_channels
                        .iter()
                        .filter_map(|value| UnreliableChannel::try_from(*value).ok())
                        .filter(|value| *value != UnreliableChannel::Unspecified)
                        .collect()
                } else {
                    Vec::new()
                };
            // 下行消费声明(alpha.6):客户端声明可经 datagram 消费的通道;
            // 实际启用 = 声明 ∩ 已知值 ∩ 本门 datagram 预算装得下 movement 块
            let downlink_channels_accepted: Vec<UnreliableChannel> = if datagram_capable {
                handshake
                    .accepts_channels
                    .iter()
                    .filter_map(|value| UnreliableChannel::try_from(*value).ok())
                    .filter(|value| *value != UnreliableChannel::Unspecified)
                    .collect()
            } else {
                Vec::new()
            };
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
                downlink_channels_accepted.contains(&UnreliableChannel::Movement),
                unreliable_channels_accepted,
                downlink_channels_accepted,
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
            // 且 本连接 datagram 预算装得下 movement 块;回执 = 声明 ∩ 已知值
            let declared_downlink = accepts_unreliable_channels(&handshake);
            let unreliable_positions =
                declared_downlink.contains(&UnreliableChannel::Movement) && datagram_capable;
            let downlink_channels_accepted = if datagram_capable {
                declared_downlink
            } else {
                Vec::new()
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
                unreliable_positions,
                Vec::new(),
                downlink_channels_accepted,
            )
        }
        _ => {
            return;
        }
    };

    let maintenance_guard = state.maintenance_rooms.clone().read_owned().await;
    if maintenance_guard.contains(&room) {
        door_reject(&mut writer_sink, channel, "room_maintenance").await;
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
        unreliable_channels_accepted: if channel == WireChannel::Player {
            unreliable_channels_accepted
                .iter()
                .map(|value| *value as i32)
                .collect()
        } else {
            Vec::new()
        },
        // 下行消费回执(alpha.6):Player 自 accepts_channels(14)、WebMap 自
        // accepts_channels(6)/deprecated 布尔协商,两类连接统一在此回执
        downlink_channels_accepted: downlink_channels_accepted
            .iter()
            .map(|value| *value as i32)
            .collect(),
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
        warn!(%remote_addr, "handshake ack enqueue failed/timed out; dropping door session");
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
    let mut writer = tokio::spawn(door_writer_loop(
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
    let mut reader = tokio::spawn(door_reader_loop(
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

    // 上行位置 datagram 消费(alpha.5):Player 通道且握手声明启用时,
    // datagram 里的 players_patch 与可靠流上报汇入同一 relay 入口;
    // 未启用/非 Player 通道则排空(保持 datagram 管道畅通,内容丢弃)
    let mut datagram_task = match (channel, &unreliable_channels_accepted, incoming_datagrams)
    {
        (WireChannel::Player, accepted, Some(rx))
            if accepted.contains(&UnreliableChannel::Movement) =>
        {
            let events = event_tx.clone();
            let id = id.clone();
            Some(tokio::spawn(async move {
                let mut rx = rx;
                while let Some(mut report) = rx.recv().await {
                    if report.submit_player_id != id {
                        continue;
                    }
                    sanitize_player_report(profile, &mut report);
                    if events
                        .send(RelayEvent::PlayerReport {
                            id: id.clone(),
                            report: Box::new(report),
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }))
        }
        (_, _, Some(mut rx)) => Some(tokio::spawn(async move {
            while rx.recv().await.is_some() {}
        })),
        (_, _, None) => None,
    };

    let end_reason = tokio::select! {
        _ = &mut writer => "writer_ended",
        _ = &mut reader => "reader_ended",
        _ = async {
            match datagram_task.as_mut() {
                Some(task) => {
                    let _ = task.await;
                }
                None => futures_util::future::pending::<()>().await,
            }
        } => {
            if let Some(task) = datagram_task.take() {
                task.abort();
            }
            reader.abort();
            writer.abort();
            "datagram_consumer_ended"
        }
    };
    let _ = event_tx
        .send(RelayEvent::Disconnect { id: id.clone() })
        .await;
    record_connection_ended(&state.db, &id, &room, kind, &remote_addr).await;
    state.metrics.unregister(&id);
    info!(connection_id = %id, reason = end_reason, "door session disconnected");
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

/// 三门共享的会话出站循环(桥接模式):DoorSession.outgoing 是 plain
/// envelope 的 MpscSink,门原生的压缩与分帧封在各门桥的出站泵内。
async fn door_writer_loop<Si>(
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
        let Some(action) = action else {
            info!(id = %context.id, "door writer ended: input channels closed (control/state dropped)");
            break;
        };
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
                match sent {
                    Err(_) => warn!(id = %context.id, bytes = byte_count, "door writer send timeout (sink blocked 2s)"),
                    Ok(Err(error)) => warn!(id = %context.id, %error, bytes = byte_count, "door writer send failed"),
                    Ok(Ok(())) => unreachable!(),
                }
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

/// 三门共享的会话入站循环(桥接模式):入站一帧 = 裸 envelope(恒 plain),
/// 门原生分帧已由门桥消化;载荷分发统一委托 handle_envelope。
async fn door_reader_loop(first_frame: Bytes, mut stream: WebMapFrameStream, context: ReaderContext) {
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
        if !handle_envelope(envelope, &mut ctx).await {
            break;
        }
    }
}

async fn handle_envelope(envelope: WireEnvelope, context: &mut ReaderContext) -> bool {
    // 三门共享的入站载荷分发(桥接模式):WS reader 与 WT/QUIC 门桥的
    // reader 都委托到这里,任何载荷臂的行为差异都是缺陷。
    // 返回 false = 会话应当终止(对端通道已死)。
    let ReaderContext {
        id,
        room,
        control,
        events,
        channel,
        kind,
        tab_history,
        relationships,
        profile,
        complete_online_roster,
        ..
    } = context;
    let id = id.clone();
    let room = room.clone();
    match envelope.payload {
        Some(wire_envelope::Payload::PlayerReportBundle(mut report))
            if report.submit_player_id == id =>
        {
            sanitize_player_report(*profile, &mut report);
            if *kind == ConnectionKind::ExternalSource
                && *complete_online_roster
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
                return false;
            }
            if let Some(head) = changed_history_head {
                let _ = events
                    .send(RelayEvent::TabHistoryChanged { room, head })
                    .await;
            }
            true
        }
        Some(wire_envelope::Payload::ResyncRequest(_)) => events
            .send(RelayEvent::Resync { id })
            .await
            .is_ok(),
        Some(wire_envelope::Payload::WebMapCommand(command))
            if *channel == WireChannel::WebMap =>
        {
            events
                .send(RelayEvent::WebMapCommand { id, command })
                .await
                .is_ok()
        }
        Some(wire_envelope::Payload::BattleChunkMetaRequest(request)) => events
            .send(RelayEvent::BattleChunkMeta {
                id,
                battle_chunks: request.battle_chunks.into_iter().take(256).collect(),
            })
            .await
            .is_ok(),
        Some(wire_envelope::Payload::TabHistorySubscribeRequest(request)) => {
            if !profile.supports_tab_history() {
                return true;
            }
            let _ = events
                .send(RelayEvent::TabHistorySubscription {
                    id,
                    enabled: request.enabled,
                })
                .await;
            if request.enabled
                && let Ok(head) = tab_history.head(&room).await
            {
                let bytes = encode_payload(
                    *channel,
                    wire_envelope::Payload::TabHistoryDigest(TabHistoryDigest {
                        head: Some(head),
                    }),
                );
                control.send(bytes).await.is_ok()
            } else {
                true
            }
        }
        Some(wire_envelope::Payload::TabHistorySyncRequest(request)) => {
            let result = if profile.supports_tab_history() {
                send_tab_sync(tab_history, control, *channel, &room, request).await
            } else {
                send_tab_sync_error(
                    control,
                    *channel,
                    request.request_id,
                    TabHistoryErrorCode::Unsupported,
                )
                .await
            };
            result.is_ok()
        }
        Some(wire_envelope::Payload::TabHistoryLookupRequest(request)) => {
            let result = if profile.supports_tab_history() {
                send_tab_lookup(tab_history, control, *channel, &room, request).await
            } else {
                send_tab_lookup_error(
                    control,
                    *channel,
                    request.request_id,
                    TabHistoryErrorCode::Unsupported,
                )
                .await
            };
            result.is_ok()
        }
        Some(wire_envelope::Payload::ExternalDatasetPublish(publish))
            if kind == &ConnectionKind::ExternalSource =>
        {
            if !profile.supports_relationships() {
                return true;
            }
            let Some(descriptor) = publish.descriptor.clone() else {
                return true;
            };
            if descriptor.realm_id != room {
                return true;
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
            control
                .send(encode_payload(
                    *channel,
                    wire_envelope::Payload::ExternalDatasetPublishAck(ack),
                ))
                .await
                .is_ok()
        }
        Some(wire_envelope::Payload::PlayerDirectoryLookupRequest(request))
            if profile.supports_relationships() =>
        {
            if request.selectors.len() > 256 {
                return true;
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
                        *channel,
                        wire_envelope::Payload::PlayerDirectoryLookupChunk(chunk),
                    ))
                    .await
                    .is_err()
                {
                    return false;
                }
            }
            true
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
                        *channel,
                        wire_envelope::Payload::PlayerRelationQueryChunk(chunk),
                    ))
                    .await
                    .is_err()
                {
                    return false;
                }
            }
            true
        }
        Some(wire_envelope::Payload::Ping(_)) => {
            let pong = encode_payload(
                *channel,
                wire_envelope::Payload::Pong(Pong {
                    server_time: unix_seconds(),
                }),
            );
            control.send(pong).await.is_ok()
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

    /// 会话出站循环的接收端:直接收集 plain envelope 字节(门桥出站泵之前)。
    struct RecordingByteSink {
        payloads: mpsc::UnboundedSender<Vec<u8>>,
    }

    impl Sink<Bytes> for RecordingByteSink {
        type Error = io::Error;

        fn poll_ready(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, item: Bytes) -> Result<(), Self::Error> {
            let _ = self.payloads.send(item.to_vec());
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
    async fn door_writer_orders_each_state_before_its_matching_digest() {
        let (payloads_tx, mut payloads_rx) = mpsc::unbounded_channel();
        let sink = RecordingByteSink {
            payloads: payloads_tx,
        };
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

        let writer = tokio::spawn(door_writer_loop(
            sink,
            control_rx,
            state_rx,
            WriterContext {
                id: "test".to_owned(),
                events: events_tx,
                metrics: Arc::new(Metrics::default()),
                traffic_channel: TrafficChannel::Player,
            },
        ));

        assert_eq!(payloads_rx.recv().await.as_deref(), Some(&b"state-1"[..]));
        assert_eq!(payloads_rx.recv().await.as_deref(), Some(&b"digest-1"[..]));
        assert_eq!(payloads_rx.recv().await.as_deref(), Some(&b"control"[..]));

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
        assert_eq!(payloads_rx.recv().await.as_deref(), Some(&b"state-2"[..]));
        assert_eq!(payloads_rx.recv().await.as_deref(), Some(&b"digest-2"[..]));

        writer.abort();
    }

    /// WS 门桥出站泵 +zstd 套:经出站泵落地的每条 WS 消息必须是可独立
    /// 解码的 zstd 块(逐 envelope flush),按序还原出原 envelope。
    #[tokio::test]
    async fn ws_egress_compresses_each_message_with_persistent_zstd_context() {
        let (frames_tx, mut frames_rx) = mpsc::unbounded_channel();
        let sink = RecordingSink { frames: frames_tx };
        let (payload_tx, payload_rx) = mpsc::channel::<Bytes>(4);
        let metrics = Arc::new(Metrics::default());

        let first_payload = vec![9u8; 4096];
        let second_payload = b"control-frame".to_vec();
        let encoder = crate::compress::StreamEncoder::new(crate::compress::DEFAULT_STREAM_COMPRESSION_LEVEL)
            .expect("zstd encoder");
        let egress = tokio::spawn(ws_egress_loop(
            sink,
            payload_rx,
            Some(encoder),
            metrics,
        ));
        payload_tx
            .send(Bytes::from(first_payload.clone()))
            .await
            .expect("payload channel");
        payload_tx
            .send(Bytes::from(second_payload.clone()))
            .await
            .expect("payload channel");
        drop(payload_tx);

        let mut decoder = crate::compress::StreamDecoder::new().expect("zstd decoder");
        for expected in [first_payload, second_payload] {
            let chunk = frames_rx.recv().await.expect("compressed frame");
            assert_ne!(chunk.as_slice(), &expected[..], "压缩块不应与原文字节相同");
            // 逐 envelope flush:每块恰好解出本条 envelope
            let decoded = decoder.decompress_chunk(&chunk).expect("decompress");
            assert_eq!(decoded, expected);
        }
        // 通道关闭后出站泵退出(会话拆除信号)
        tokio::time::timeout(Duration::from_secs(5), egress)
            .await
            .expect("egress loop exit")
            .expect("egress join");
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
            crate::door::DoorSession {
                incoming: stream,
                outgoing: sink,
                movement_tx,
                bulk: None,
                incoming_datagrams: None,
                capabilities: crate::door::DoorCapabilities::default(),
            },
            state,
            "test-remote".to_owned(),
        ));

        let handshake = PlayerHandshakeRequest {
            submit_player_id: "door-player".to_owned(),
            network_protocol_version: CURRENT_PROTOCOL_VERSION.to_string(),
            minimum_compatible_network_protocol_version: MINIMUM_PROTOCOL_VERSION.to_string(),
            room_code: Some("door-room".to_owned()),
            unreliable_channels: vec![UnreliableChannel::Movement as i32],
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
        // 声明回执:本测试 DoorSession 无 datagram 通道(None)→ 不启用
        assert!(ack.unreliable_channels_accepted.is_empty());

        session.abort();
    }

    /// 上行位置 datagram(alpha.5)端到端:握手声明 MOVEMENT 且门具备
    /// datagram 接收 → ack 回执启用;datagram 里的 players_patch 与可靠
    /// 流上报汇入同一 relay,位置出现在快照中。
    #[tokio::test]
    async fn door_session_uplink_position_datagram_reaches_relay() {
        use crate::proto::teamviewer::v1::{
            PlayerDelta, PlayerHandshakeRequest, PlayerPatchScope, PlayerUpsert,
            UnreliableChannel, wire_envelope,
        };
        use crate::web_transport::{MpscReceiver, MpscSink};
        use prost::Message as _;

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

        let (inbound_tx, inbound_rx) = mpsc::channel::<Result<Bytes, io::Error>>(4);
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<Bytes>(4);
        let (datagram_tx, datagram_rx) = mpsc::channel::<crate::proto::teamviewer::v1::PlayerReportBundle>(8);
        let stream = WebMapFrameStream::new(MpscReceiver::new(inbound_rx));
        let sink = MpscSink::new(outbound_tx);
        let (movement_tx, _movement_rx) = watch::channel(None);

        let session = tokio::spawn(serve_web_map_session(
            crate::door::DoorSession {
                incoming: stream,
                outgoing: sink,
                movement_tx,
                bulk: None,
                incoming_datagrams: Some(datagram_rx),
                capabilities: crate::door::DoorCapabilities {
                    datagram: true,
                    ..Default::default()
                },
            },
            state,
            "test-remote".to_owned(),
        ));

        let handshake = PlayerHandshakeRequest {
            submit_player_id: "door-player".to_owned(),
            network_protocol_version: CURRENT_PROTOCOL_VERSION.to_string(),
            minimum_compatible_network_protocol_version: MINIMUM_PROTOCOL_VERSION.to_string(),
            room_code: Some("door-room".to_owned()),
            unreliable_channels: vec![UnreliableChannel::Movement as i32],
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
        let Some(wire_envelope::Payload::HandshakeAck(ack)) = ack_envelope.payload else {
            panic!("expected handshake ack");
        };
        // 门具备 datagram 能力:声明获得回执
        assert_eq!(
            ack.unreliable_channels_accepted,
            vec![UnreliableChannel::Movement as i32]
        );

        // datagram 上报:位置 upsert(独立 envelope,无分帧)
        let report = crate::proto::teamviewer::v1::PlayerReportBundle {
            submit_player_id: "door-player".to_owned(),
            players_patch: Some(PlayerPatchScope {
                upsert: vec![PlayerUpsert {
                    id: "p1".to_owned(),
                    data: Some(PlayerDelta {
                        x: Some(1.5),
                        y: Some(64.0),
                        z: Some(-2.0),
                        dimension: Some("minecraft:overworld".to_owned()),
                        player_name: Some("p1".to_owned()),
                        ..Default::default()
                    }),
                    clear_fields: Vec::new(),
                }],
                delete: Vec::new(),
            }),
            ..Default::default()
        };
        datagram_tx.send(report).await.expect("datagram send");

        // 位置经 relay 进入快照:轮询出站帧直至 SnapshotFull 含 p1
        let deadline = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let frame = outbound_rx.recv().await.expect("outbound frame");
                let Ok(envelope) = WireEnvelope::decode(frame) else {
                    continue;
                };
                // delta_enabled 会话首帧为 SnapshotFull,此后位置走 Patch
                let arrived = match &envelope.payload {
                    Some(wire_envelope::Payload::SnapshotFull(snapshot)) => {
                        snapshot.players.contains_key("p1")
                    }
                    Some(wire_envelope::Payload::Patch(patch)) => patch
                        .players
                        .as_ref()
                        .is_some_and(|scope| scope.upsert.iter().any(|u| u.id == "p1")),
                    _ => false,
                };
                if arrived {
                    return;
                }
            }
        })
        .await;
        assert!(deadline.is_ok(), "位置未在超时内进入快照");

        session.abort();
    }

    /// 下行消费对称化(alpha.6)端到端:Player 握手 accepts_channels 声明
    /// MOVEMENT 且门具备 datagram 能力 → ack downlink_channels_accepted 回执;
    /// 上报位置后 relay 向该 Player 的 movement watch 发布 movement 批
    /// (0.9.0-alpha.5 及以前此路径为 WebMap 专属)。
    #[tokio::test]
    async fn door_session_downlink_movement_datagram_granted_and_published() {
        use crate::proto::teamviewer::v1::{
            PlayerDelta, PlayerHandshakeRequest, PlayerPatchScope, PlayerUpsert,
            UnreliableChannel, wire_envelope,
        };
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

        let (inbound_tx, inbound_rx) = mpsc::channel::<Result<Bytes, io::Error>>(4);
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<Bytes>(4);
        let stream = WebMapFrameStream::new(MpscReceiver::new(inbound_rx));
        let sink = MpscSink::new(outbound_tx);
        // movement watch 留住接收端:relay 发布的 movement 批从这里观测
        let (movement_tx, mut movement_rx) = watch::channel(None);

        let session = tokio::spawn(serve_web_map_session(
            crate::door::DoorSession {
                incoming: stream,
                outgoing: sink,
                movement_tx,
                bulk: None,
                incoming_datagrams: None,
                capabilities: crate::door::DoorCapabilities {
                    datagram: true,
                    ..Default::default()
                },
            },
            state,
            "test-remote".to_owned(),
        ));

        let handshake = PlayerHandshakeRequest {
            submit_player_id: "downlink-player".to_owned(),
            network_protocol_version: CURRENT_PROTOCOL_VERSION.to_string(),
            minimum_compatible_network_protocol_version: MINIMUM_PROTOCOL_VERSION.to_string(),
            room_code: Some("door-room".to_owned()),
            accepts_channels: vec![UnreliableChannel::Movement as i32],
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
        let Some(wire_envelope::Payload::HandshakeAck(ack)) = ack_envelope.payload else {
            panic!("expected handshake ack");
        };
        // 下行消费回执:声明获得回执;上行回执保持为空(未声明)
        assert_eq!(
            ack.downlink_channels_accepted,
            vec![UnreliableChannel::Movement as i32]
        );
        assert!(ack.unreliable_channels_accepted.is_empty());

        // 可靠流上报一个位置,驱动 relay 建立玩家并进入发布循环
        let report = crate::proto::teamviewer::v1::PlayerReportBundle {
            submit_player_id: "downlink-player".to_owned(),
            players_patch: Some(PlayerPatchScope {
                upsert: vec![PlayerUpsert {
                    id: "downlink-player".to_owned(),
                    data: Some(PlayerDelta {
                        x: Some(3.5),
                        y: Some(64.0),
                        z: Some(-1.0),
                        dimension: Some("minecraft:overworld".to_owned()),
                        player_name: Some("downlink".to_owned()),
                        ..Default::default()
                    }),
                    clear_fields: Vec::new(),
                }],
                delete: Vec::new(),
            }),
            ..Default::default()
        };
        let report_envelope = WireEnvelope {
            channel: WireChannel::Player as i32,
            payload: Some(wire_envelope::Payload::PlayerReportBundle(report)),
        };
        inbound_tx
            .send(Ok(Bytes::from(report_envelope.encode_to_vec())))
            .await
            .expect("inbound report");

        // relay 应向该 Player 的 movement watch 发布批(alpha.6 前仅 WebMap);
        // 玩家建立前的空 tick 也会发布空批,等到首个含位置的批
        let published = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if movement_rx.changed().await.is_err() {
                    return None;
                }
                if let Some(batch) = movement_rx.borrow_and_update().clone()
                    && !batch.chunks.is_empty()
                {
                    return Some(batch);
                }
            }
        })
        .await
        .expect("movement batch timeout");
        let batch = published.expect("movement batch published");
        assert!(!batch.chunks.is_empty());
        // movement 批载荷 = 裸 WebMap-channel Patch(与消费端角色无关的契约)
        let chunk = &batch.chunks[0][..];
        let datagram_envelope = WireEnvelope::decode(Bytes::copy_from_slice(chunk))
            .expect("movement chunk envelope");
        assert_eq!(datagram_envelope.channel, WireChannel::WebMap as i32);
        let Some(wire_envelope::Payload::Patch(patch)) = datagram_envelope.payload else {
            panic!("expected patch payload in movement chunk");
        };
        assert!(patch
            .players
            .expect("players scope")
            .upsert
            .iter()
            .any(|upsert| upsert.id == "downlink-player"));

        session.abort();
    }
}
