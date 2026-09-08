//! 裸 QUIC 门:面向 Java mod 客户端(Netty codec-quic),与 WebTransport 门平行。
//!
//! 线路级与 WT 互不兼容(无 H3/extended CONNECT 层),独立 UDP 端口监听;
//! 应用层完全共享:同一 WireEnvelope、同一 `serve_web_map_session`。
//! 门约定与 WT 一致(见 TeamViewRelay-Protocol):客户端开 1 条 bi 流上行、
//! 服务端开 1 条 uni 流下行,流分帧 `[varint 长度][payload]`,datagram 裸块
//! 无前缀;首个控制流 10s 内必须建立,否则视为无效连接。
//! 裸 QUIC 没有 WT 的 path 路由——首个 WireEnvelope 的 channel 自识别
//! Player/WebMap(web.rs 握手逻辑天然支持)。

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Context as AnyhowContext;
use bytes::Bytes;
use quinn::{Connection, Endpoint, ServerConfig, crypto::rustls::QuicServerConfig};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info};

use crate::{
    bulk,
    cert::CertRuntime,
    config::WebTransportConfig,
    door_control,
    frame,
    proto::teamviewer::door::v1::door_control_frame::Payload as DoorControlPayload,
    relay::{MOVEMENT_CHUNK_MAX_BYTES, MovementBatch},
    web,
};

/// 门合同违规的应用层错误码(混用/合同外流等,当场断连的唯一出口)。
const DOOR_VIOLATION_CODE: u32 = 0x01;

/// 协议版本线:ALPN 三套并列(alpha.6 压缩阶段启用),rustls 按客户端
/// 偏好序选择;语义见 compress::Suite。
pub(crate) const ALPN_PLAIN: &str = "teamviewrelay/v1";
pub(crate) const ALPN_ZSTD: &str = "teamviewrelay/v1+zstd";
pub(crate) const ALPN_ZSTD_DICT: &str = "teamviewrelay/v1+zstd-dict";

const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// QUIC 门与 WT 门共用同一份证书/绑定配置(TLS 1.3 证书与门无关);
/// 端口与开关由 config 层的独立 `[quicTransport]` 段区分。
pub async fn serve(config: WebTransportConfig, state: crate::web::AppState) -> anyhow::Result<()> {
    let (runtime, endpoint) = build_server_endpoint(&config).await?;
    info!(
        address = %config.bind_address,
        identities = %runtime.describe(),
        alpn = %ALPN_PLAIN,
        "QUIC endpoint listening"
    );
    let mut reload_interval = tokio::time::interval(Duration::from_secs(config.poll_interval_sec));
    reload_interval.tick().await;
    loop {
        tokio::select! {
            _ = reload_interval.tick() => {
                runtime.reload().await;
            }
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else {
                    anyhow::bail!("QUIC endpoint closed");
                };
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(error) = accept_connection(incoming, state).await {
                        debug!(%error, "QUIC connection rejected");
                    }
                });
            }
        }
    }
}

/// 证书表 → rustls → quinn 的完整装配;测试用同一入口(端口 0)。
async fn build_server_endpoint(
    config: &WebTransportConfig,
) -> anyhow::Result<(CertRuntime, Endpoint)> {
    let (runtime, tls_config) = CertRuntime::load(config).await?;
    let mut tls_config = tls_config;
    // 三套并列;rustls 按客户端偏好序选择,服务端列表即"全部接受"
    tls_config.alpn_protocols = vec![
        ALPN_ZSTD_DICT.as_bytes().to_vec(),
        ALPN_ZSTD.as_bytes().to_vec(),
        ALPN_PLAIN.as_bytes().to_vec(),
    ];
    let quic_tls = QuicServerConfig::try_from(tls_config).context("rustls → QUIC TLS 适配失败")?;
    let mut server_config = ServerConfig::with_crypto(Arc::new(quic_tls));
    server_config.transport_config(Arc::new(bbr_transport_config()));
    let endpoint = Endpoint::server(server_config, config.bind_address)
        .context("failed to bind QUIC UDP endpoint")?;
    Ok((runtime, endpoint))
}

async fn accept_connection(
    incoming: quinn::Incoming,
    state: crate::web::AppState,
) -> anyhow::Result<()> {
    let connection = incoming.await.context("QUIC handshake failed")?;
    let remote_addr = connection.remote_address().to_string();
    let alpn = negotiated_alpn(&connection);
    let suite = crate::compress::Suite::from_alpn(alpn.as_deref().unwrap_or_default())
        .ok_or_else(|| anyhow::anyhow!("QUIC connection without a teamviewrelay ALPN"))?;
    info!(
        %remote_addr,
        alpn = alpn.as_deref().unwrap_or(""),
        ?suite,
        "QUIC connection accepted"
    );
    let mut session = stream_bridge(connection, suite);
    // bulk 触发通道入表(debug 端点按表投递;会话死透时发送失败自然逐出)
    if let Some(bulk_tx) = session.bulk.take() {
        let hub_id = state.bulk_hub.register(bulk_tx).await;
        debug!(connection_id = %hub_id, "QUIC session registered for bulk push");
    }
    web::serve_web_map_session(session, state, remote_addr).await;
    Ok(())
}

/// 服务端传输配置:BBR 拥塞控制。BBR 按带宽×RTT 模型发速,不把随机
/// 丢包当拥塞信号——跨洲高丢包线路上 Cubic 会把窗口压死(5% 丢 +
/// 400ms RTT 实测坍缩到 ~9KiB/s),BBR 则贴近瓶颈速率。其余参数全默认。
pub(crate) fn bbr_transport_config() -> quinn::TransportConfig {
    let mut transport = quinn::TransportConfig::default();
    transport.congestion_controller_factory(Arc::new(
        quinn::congestion::BbrConfig::default(),
    ));
    transport
}

/// 读取握手协商出的 ALPN(现阶段仅日志;压缩阶段起据此选择压缩套)。
fn negotiated_alpn(connection: &Connection) -> Option<String> {
    let data = connection
        .handshake_data()?
        .downcast::<quinn::crypto::rustls::HandshakeData>()
        .ok()?;
    Some(String::from_utf8_lossy(&data.protocol?).into_owned())
}

fn stream_bridge(
    connection: Connection,
    suite: crate::compress::Suite,
) -> crate::door::DoorSession {
    let (incoming_tx, incoming_rx) = mpsc::channel::<Result<Bytes, io::Error>>(256);
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<Bytes>(256);
    let (movement_tx, mut movement_rx) = watch::channel(None::<Arc<MovementBatch>>);
    // 上行位置 datagram 通道(alpha.5):桥内解析,只转发合法形状
    let (uplink_tx, uplink_rx) = mpsc::channel::<crate::proto::teamviewer::v1::PlayerReportBundle>(32);
    // bulk 触发通道(容量 1 = 每会话同时只允许一个在途 bulk,满即拒)
    let (bulk_tx, mut bulk_rx) = mpsc::channel::<bulk::BulkRequest>(1);
    // bulk 流必须在 door-control 下行流之后开启(流识别按开启序),
    // door_ready 由 bi 接收任务在开出 door-control 流后放行
    // Option 包装:仅首个 bulk 需要等门控流实体化,之后置空
    let (door_ready_tx, door_ready_rx) = tokio::sync::oneshot::channel::<()>();
    let mut door_ready_rx = Some(door_ready_rx);
    let read_connection = connection.clone();
    let write_connection = connection.clone();

    // 位置 datagram 发送任务:与 WT 门语义一致——relay 每 dirty tick 发布
    // 预切好的 movement 批,逐块压缩后装入单个 datagram(plain 套原样,
    // +zstd* 套单帧自包含压缩);TooLarge 时重查预算,装不下的块跳过——
    // 下个 dirty tick 全量重发。同一 select! 消化 door-control 上行的
    // dict_ready(+zstd-dict 套):激活就发生在本任务独占的编码器上,与
    // 编码天然串行,无锁。
    let send_connection = connection.clone();
    // door-control 下行流帧通道:dict_offer(+zstd-dict)与 bulk 通告共用,
    // 单一写出任务独占该流,天然保序
    let (door_frame_tx, door_frame_rx) = mpsc::channel::<Bytes>(8);
    let (dict_ready_tx, mut dict_ready_rx) = mpsc::channel::<String>(8);
    // bulk 任务的门控帧通道克隆(必须在 movement 任务 spawn 前建,它会把
    // door_frame_tx move 走)
    let bulk_door_tx = door_frame_tx.clone();
    let mut dict_encoder = door_control::DatagramDictEncoder::for_suite(suite);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                changed = movement_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let Some(batch) = movement_rx.borrow_and_update().clone() else {
                        continue;
                    };
                    // 字典训练(+zstd-dict 套独有):样本攒够且过了最小重训
                    // 间隔即训练,offer 经 door-control 下行流下发,ready 回执
                    // 后才激活切换
                    if let Some(offer) = dict_encoder.observe(&batch, Instant::now()) {
                        match door_control::dict_offer_frame(&offer.id, &offer.content) {
                            Some(offer_frame) => {
                                if door_frame_tx.try_send(offer_frame).is_err() {
                                    // 下行控制流死透或堆积:字典保持不激活,
                                    // datagram 维持独立压缩,语义自愈,不阻塞
                                    // movement 主路径
                                    debug!("door-control offer queue full; offer skipped");
                                }
                            }
                            None => debug!("dictionary content rejected by size limit"),
                        }
                    }
                    let mut max_size = send_connection.max_datagram_size();
                    for chunk in batch.chunks.iter() {
                        let Some(datagram) = dict_encoder.encode(chunk) else {
                            continue;
                        };
                        let frame_len = datagram.len();
                        if !max_size.is_some_and(|size| frame_len <= size) {
                            max_size = send_connection.max_datagram_size();
                            if !max_size.is_some_and(|size| frame_len <= size) {
                                continue;
                            }
                        }
                        match send_connection.send_datagram(datagram) {
                            Ok(()) => {}
                            // 握手时已确认对端支持 datagram;其余错误意味着连接已死,
                            // 停发即可——可靠流的低频位置刷新兜底
                            Err(quinn::SendDatagramError::TooLarge) => continue,
                            Err(error) => {
                                debug!(%error, "QUIC datagram send stopped");
                                return;
                            }
                        }
                    }
                }
                // door-control 上行:dict_ready(ID) → 激活压缩字典。守卫兼防
                // 对端早退后通道关闭的热轮询(非 +zstd-dict 套恒关)
                Some(id) = dict_ready_rx.recv(), if dict_encoder.dict_enabled() => {
                    dict_encoder.activate(&id);
                    debug!(dictionary_id = %id, "QUIC datagram dictionary activated");
                }
                else => return,
            }
        }
    });

    // 上行 datagram 接收(alpha.5):裸 WireEnvelope{PLAYER, PlayerReportBundle}
    // 且仅 players_patch 有值才转发;声明校验在应用层(serve 侧)。
    // 持续读取本身也必须保留:否则对端发送缓冲堆积会触发其丢弃或流控。
    {
        let drain_connection = connection.clone();
        tokio::spawn(async move {
            while let Ok(datagram) = drain_connection.read_datagram().await {
                use prost::Message as _;
use crate::proto::teamviewer::v1::{WireChannel, WireEnvelope, wire_envelope};
                let Ok(envelope) = WireEnvelope::decode(datagram.as_ref()) else {
                    continue;
                };
                if envelope.channel != WireChannel::Player as i32 {
                    continue;
                }
                let Some(wire_envelope::Payload::PlayerReportBundle(report)) = envelope.payload
                else {
                    continue;
                };
                if report.players_patch.is_none() {
                    continue;
                }
                if uplink_tx.send(report).await.is_err() {
                    return;
                }
            }
        });
    }

    // door-control 上行(全部压缩套常开,约定见 TeamViewRelay-Protocol
    // "传输层/应用层分层与 door-control 流"):客户端第 1 条单向流承载门控
    // 帧。分层铁律:解析失败即混用/损坏 → 协议违规断连(未知 oneof 成员
    // 仍前向兼容容忍);字典回执仍只作用于 +zstd-dict 套。
    {
        let control_connection = connection.clone();
        tokio::spawn(async move {
            let Ok(mut stream) = control_connection.accept_uni().await else {
                return;
            };
            let violation_connection = control_connection.clone();
            let framing = frame::read_frames(&mut stream, |door_frame| {
                let dict_ready_tx = dict_ready_tx.clone();
                let violation_connection = violation_connection.clone();
                async move {
                    match door_control::decode_frame(&door_frame) {
                        Some(frame) => match frame.payload {
                            Some(DoorControlPayload::DictReady(ready)) => {
                                dict_ready_tx.send(ready.dictionary_id).await.is_ok()
                            }
                            // 未知 oneof 成员(前向兼容)与其他门控载荷:无害,继续读
                            Some(_) => true,
                            // protobuf 合法但没有任何已知门控成员 = 应用层帧混入
                            // (envelope 解出门控流时 payload 恒为 None),违规断连
                            None => {
                                door_violation(&violation_connection, "door frame without payload");
                                false
                            }
                        },
                        // 门控流上的非法 protobuf(混用/损坏),违规断连
                        None => {
                            door_violation(&violation_connection, "door frame undecodable");
                            false
                        }
                    }
                }
            })
            .await;
            if framing.is_err() {
                door_violation(&control_connection, "door framing violation");
                return;
            }
            // 合同外流:客户端第 2 条起单向流一律违规断连
            while let Ok(mut extra) = control_connection.accept_uni().await {
                door_violation(&control_connection, "unexpected client uni stream");
                extra.stop(0u32.into()).ok();
            }
        });
    }

    // bulk 传输通道(proto 0.9.0-alpha.4):door-control 流就绪后,按请求
    // 通告 + 开专用单向流,小块写入让应用流自然交错(低优先级是写方约定,
    // 不引入任何带宽假设)。bulk 期间并行发门控心跳 datagram,供突发压测
    // 统计 datagram 送达率。
    {
        let bulk_connection = connection.clone();
        tokio::spawn(async move {
            while let Some(request) = bulk_rx.recv().await {
                let transfer_id = bulk::transfer_id(&request.content);
                let announce = door_control::bulk_start_frame(
                    &transfer_id,
                    request.content.len() as u64,
                    &request.content_type,
                );
                // 通告先进门控帧通道:门控泵收到首帧才实体化门控流
                // (服务端第 2 条单向流),写完首帧后放行 door_ready,
                // bulk 流自此可开(恒为第 3 条)——流序由此保证
                if bulk_door_tx.send(announce).await.is_err() {
                    break;
                }
                if let Some(door_ready) = door_ready_rx.as_mut() {
                    if door_ready.await.is_err() {
                        break;
                    }
                }
                let Ok(mut stream) = bulk_connection.open_uni().await else {
                    break;
                };
                // 低优先级主实现:门控/应用流(默认 0)永远先于 bulk 出队
                let _ = stream.set_priority(bulk::STREAM_PRIORITY);
                // 心跳随 bulk 启停:独立任务,不被 bulk 写阻塞
                let stop = Arc::new(AtomicBool::new(false));
                let heartbeat_task = spawn_bulk_heartbeat(&bulk_connection, Arc::clone(&stop));
                let prefix = bulk::stream_prefix(&transfer_id);
                let mut failed = stream.write_all(&prefix).await.is_err();
                let mut pacer = bulk::Pacer::default();
                for chunk in request.content.chunks(bulk::CHUNK) {
                    if failed {
                        break;
                    }
                    // 在途字节钉底节流:RTT 来自运行时观测,无带宽假设
                    pacer.wait(chunk.len(), bulk_connection.stats().path.rtt).await;
                    if stream.write_all(chunk).await.is_err() {
                        failed = true;
                        break;
                    }
                }
                let _ = stream.finish();
                stop.store(true, Ordering::Relaxed);
                let _ = heartbeat_task.await;
                debug!(%transfer_id, len = request.content.len(), "QUIC bulk transfer done");
            }
        });
    }

    tokio::spawn(async move {
        let control = tokio::time::timeout(FIRST_FRAME_TIMEOUT, read_connection.accept_bi()).await;
        let Ok(Ok((send, mut recv))) = control else {
            debug!("QUIC control stream missing (accept_bi timeout or failure)");
            let _ = incoming_tx
                .send(Err(io::Error::other("QUIC control stream missing")))
                .await;
            return;
        };
        debug!("QUIC control stream accepted");
        // 合同外流:客户端第 2 条起双向流一律违规断连。必须在首条 bi 接收
        // 成功之后再启动消化任务,否则本任务与消化任务会竞抢首条流
        {
            let bi_connection = read_connection.clone();
            tokio::spawn(async move {
                while let Ok((_send, recv)) = bi_connection.accept_bi().await {
                    door_violation(&bi_connection, "unexpected client bi stream");
                    drop(recv);
                }
            });
        }
        // 控制流只承载 client→server;服务端→客户端的数据必须写服务端单向流,
        // mod 端只从 incoming unidirectional streams 读取
        drop(send);
        let mut state_stream =
            match tokio::time::timeout(WRITE_TIMEOUT, write_connection.open_uni()).await {
                Ok(Ok(state_stream)) => state_stream,
                _ => {
                    debug!("QUIC state stream unavailable (open_uni timeout or failure)");
                    let _ = incoming_tx
                        .send(Err(io::Error::other("QUIC state stream unavailable")))
                        .await;
                    return;
                }
            };
        debug!("QUIC state stream opened");

        // door-control 下行流:服务端第 2 条单向流,必须在状态流之后开启
        // (流识别按开启序);全部压缩套常开(空闲零开销),字典载荷仍只在
        // +zstd-dict 套产生。开不出来时字典永不激活、bulk 不启动,datagram
        // 维持独立压缩,链路照常。
        // door-control 下行流:服务端第 2 条单向流,按需实体化——quinn 的
        // 流要等首帧写出才会过线,故泵任务在收到首帧时才 open_uni(流序仍
        // 为第 2 条:应用下行流的首帧 ack 必然先于任何门控帧,而 bulk 泵要
        // 等 door_ready 放行后才开第 3 条)。门控帧仍只在 +zstd-dict(字典)
        // 与 debug bulk(通告)时产生,空闲零开销。开不出来时字典永不激活、
        // bulk 不启动,datagram 维持独立压缩,链路照常。
        let door_pump_connection = write_connection.clone();
        tokio::spawn(async move {
            let mut door_frame_rx = door_frame_rx;
            let Some(first_frame) = door_frame_rx.recv().await else {
                return;
            };
            let Ok(mut door_stream) = door_pump_connection.open_uni().await else {
                debug!("QUIC door-control stream unavailable");
                return;
            };
            if frame::write_frame(&mut door_stream, &first_frame).await.is_err() {
                debug!("QUIC door-control first frame write failed");
                return;
            }
            // 首帧已过线,流序 #2 已占,bulk 流(uni #3+)自此放行
            let _ = door_ready_tx.send(());
            while let Some(door_frame) = door_frame_rx.recv().await {
                if let Err(error) = frame::write_frame(&mut door_stream, &door_frame).await {
                    debug!(%error, "QUIC door-control write failed");
                    break;
                }
            }
        });

        let reader = tokio::spawn(async move {
            // 上行恒为 plain varint 分帧:zstd 套只压缩下行(上行载荷小,
            // 且浏览器端 fzstd 仅解压,压缩套语义为单向)。
            if let Err(error) = frame::read_frames(&mut recv, |frame| {
                let incoming_tx = incoming_tx.clone();
                async move {
                    debug!(frame_len = frame.len(), "QUIC frame read");
                    incoming_tx.send(Ok(frame)).await.is_ok()
                }
            })
            .await
            {
                debug!(%error, "QUIC read failed");
                let _ = incoming_tx
                    .send(Err(io::Error::other(format!("QUIC read failed: {error}"))))
                    .await;
            }
        });

        // 下行 zstd:envelope → 持久 CCtx flush 出压缩块 → `[varint][块]`
        let mut encoder = if suite.stream_zstd() {
            match crate::compress::StreamEncoder::new() {
                Ok(encoder) => Some(encoder),
                Err(error) => {
                    debug!(%error, "QUIC downlink encoder unavailable");
                    // 无下行通道即会话失效:结束写出循环,状态流关闭会
                    // 触发会话拆除;读向由 reader 任务独立处理
                    reader.abort();
                    return;
                }
            }
        } else {
            None
        };
        while let Some(payload) = outgoing_rx.recv().await {
            let wire = match encoder.as_mut() {
                Some(encoder) => match encoder.compress_chunk(&payload) {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        debug!(%error, "QUIC downlink compress failed");
                        break;
                    }
                },
                None => payload,
            };
            let result =
                tokio::time::timeout(WRITE_TIMEOUT, frame::write_frame(&mut state_stream, &wire))
                    .await
                    .unwrap_or(Err(io::Error::other("state stream write timeout")));
            if let Err(error) = result {
                debug!(%error, "QUIC state write failed");
                break;
            }
        }
        reader.abort();
    });

    crate::door::DoorSession {
        incoming: crate::web::WebMapFrameStream::new(crate::web_transport::MpscReceiver::new(
            incoming_rx,
        )),
        outgoing: crate::web_transport::MpscSink::new(outgoing_tx),
        movement_tx,
        bulk: Some(bulk_tx),
        incoming_datagrams: Some(uplink_rx),
        capabilities: crate::door::DoorCapabilities {
            datagram: connection
                .max_datagram_size()
                .is_some_and(|size| size >= MOVEMENT_CHUNK_MAX_BYTES),
            door_control: true,
            bulk: true,
            wire_metrics: false,
        },
    }
}

/// 门合同违规的唯一出口:当场断连,绝不静默容忍(合同真相源 README
/// "传输层/应用层分层与 door-control 流")。
fn door_violation(connection: &Connection, reason: &str) {
    debug!(%reason, "QUIC door contract violation; closing connection");
    connection.close(DOOR_VIOLATION_CODE.into(), reason.as_bytes());
}

/// bulk 期间的门控心跳:每 `bulk::HEARTBEAT_INTERVAL` 发一枚固定魔数
/// datagram,供接收端统计饱和期送达率。`stop` 置位或连接死亡即退出。
fn spawn_bulk_heartbeat(connection: &Connection, stop: Arc<AtomicBool>) -> tokio::task::JoinHandle<()> {
    let connection = connection.clone();
    tokio::spawn(async move {
        let mut seq: u32 = 0;
        let mut interval = tokio::time::interval(bulk::HEARTBEAT_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        while !stop.load(Ordering::Relaxed) {
            interval.tick().await;
            // 对端未协商 datagram 扩展或连接已死:静默停发(心跳是纯观测)
            if connection.send_datagram(bulk::heartbeat(seq)).is_err() {
                break;
            }
            seq = seq.wrapping_add(1);
        }
    })
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, sync::Arc as StdArc, time::Duration};

    use futures_util::{SinkExt as _, StreamExt as _};

    use quinn::crypto::rustls::QuicClientConfig;
    use wtransport::Identity;
    use wtransport::tls::rustls::{
        ClientConfig as RustlsClientConfig, RootCertStore,
        crypto::{CryptoProvider, ring::default_provider as ring_provider},
        pki_types::{CertificateDer, pem::PemObject},
    };

    use crate::{
        config::{CertIdentity, WebTransportConfig},
        relay::{MOVEMENT_CHUNK_MAX_BYTES, MovementBatch},
    };

    use super::*;

    const TEST_SNI: &str = "a.example.com";

    fn test_provider() -> CryptoProvider {
        ring_provider()
    }

    fn write_test_cert(dir: &std::path::Path, name: &str, sans: &[&str]) {
        let identity = Identity::self_signed(sans).expect("self signed identity");
        let cert_pem = identity
            .certificate_chain()
            .as_slice()
            .iter()
            .map(|certificate| certificate.to_pem())
            .collect::<Vec<_>>()
            .join("");
        let key_pem = identity.private_key().to_secret_pem();
        std::fs::write(dir.join(format!("{name}.cert.pem")), cert_pem).expect("write cert pem");
        std::fs::write(dir.join(format!("{name}.key.pem")), key_pem).expect("write key pem");
    }

    fn cert_identity(dir: &std::path::Path, name: &str, default: bool) -> CertIdentity {
        CertIdentity {
            cert_path: dir.join(format!("{name}.cert.pem")),
            key_path: dir.join(format!("{name}.key.pem")),
            default,
        }
    }

    /// 起一个端口 0 的 QUIC 测试端点。
    /// 返回 (地址, 证书 DER, 服务端接受连接的接收端)——bridge 必须建在服务端连接上。
    async fn start_endpoint(
        sans: &[&str],
    ) -> anyhow::Result<(
        SocketAddr,
        CertificateDer<'static>,
        tokio::sync::mpsc::Receiver<quinn::Connection>,
    )> {
        let dir = std::env::temp_dir().join(format!(
            "quic-door-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        write_test_cert(&dir, "svc", sans);
        let config = WebTransportConfig {
            enabled: true,
            bind_address: SocketAddr::from(([127, 0, 0, 1], 0)),
            identities: vec![cert_identity(&dir, "svc", true)],
            poll_interval_sec: 30,
            renew_window_sec: 7 * 24 * 60 * 60,
        };
        let (_runtime, endpoint) = build_server_endpoint(&config).await?;
        let addr = endpoint.local_addr().expect("local addr");
        let (conn_tx, conn_rx) = tokio::sync::mpsc::channel::<quinn::Connection>(1);
        tokio::spawn(async move {
            loop {
                let Some(incoming) = endpoint.accept().await else {
                    return;
                };
                let conn_tx = conn_tx.clone();
                tokio::spawn(async move {
                    // 接受连接让客户端 connect() 完成,回传服务端连接供测试装配
                    if let Ok(connection) = incoming.await {
                        conn_tx.send(connection.clone()).await.ok();
                        let _ = tokio::time::timeout(Duration::from_secs(10), connection.closed())
                            .await;
                    }
                });
            }
        });
        let leaf_der = CertificateDer::pem_file_iter(dir.join("svc.cert.pem"))
            .expect("pem iter")
            .next()
            .expect("leaf")
            .expect("der");
        Ok((addr, leaf_der, conn_rx))
    }

    /// 以自签名证书为信任锚、固定 ALPN 的 quinn 测试客户端。
    fn client_endpoint(leaf_der: CertificateDer<'static>, alpn: &[&[u8]]) -> Endpoint {
        let mut roots = RootCertStore::empty();
        roots.add(leaf_der).expect("trust anchor");
        let tls = RustlsClientConfig::builder_with_provider(StdArc::new(test_provider()))
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        let mut tls = tls;
        tls.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        let quic_tls = QuicClientConfig::try_from(tls).expect("quic client tls");
        let client_config = quinn::ClientConfig::new(StdArc::new(quic_tls));
        let mut endpoint =
            Endpoint::client("127.0.0.1:0".parse().expect("bind addr")).expect("client endpoint");
        endpoint.set_default_client_config(client_config);
        endpoint
    }

    async fn connect(
        addr: SocketAddr,
        leaf_der: CertificateDer<'static>,
        alpn: &[&[u8]],
    ) -> quinn::Connection {
        let client = client_endpoint(leaf_der, alpn);
        tokio::time::timeout(
            Duration::from_secs(5),
            client.connect(addr, TEST_SNI).expect("connect"),
        )
        .await
        .expect("connect timeout")
        .expect("handshake")
    }

    #[tokio::test]
    async fn negotiates_plain_alpn() {
        let (addr, leaf, _server_rx) = start_endpoint(&[TEST_SNI]).await.expect("endpoint");
        let connection = connect(addr, leaf, &[ALPN_PLAIN.as_bytes()]).await;
        assert_eq!(
            negotiated_alpn(&connection).as_deref(),
            Some(ALPN_PLAIN),
            "服务端应回显协商出的 ALPN"
        );
    }

    #[tokio::test]
    async fn rejects_unknown_alpn() {
        let (addr, leaf, _server_rx) = start_endpoint(&[TEST_SNI]).await.expect("endpoint");
        let client = client_endpoint(leaf, &[b"unrelated/v9"]);
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            client.connect(addr, TEST_SNI).expect("connect"),
        )
        .await
        .expect("connect timeout");
        assert!(result.is_err(), "ALPN 不匹配必须握手失败");
    }

    /// 集成测试:客户端 bi 控制流上行帧 → bridge incoming;
    /// 服务端出站帧必须出现在服务端 uni 流上(varint 分帧)。
    #[tokio::test]
    async fn state_frames_flow_on_unidirectional_stream() {
        let (addr, leaf, mut server_rx) = start_endpoint(&[TEST_SNI]).await.expect("endpoint");
        let connection = connect(addr, leaf, &[ALPN_PLAIN.as_bytes()]).await;
        let server_conn = server_rx.recv().await.expect("server accepted");

        // bridge 建立(镜像 accept_connection 装配,movement 关闭)
        let session = stream_bridge(server_conn, crate::compress::Suite::Plain);
        let mut incoming_rx = session.incoming;
        let mut outgoing_tx = session.outgoing;

        // 客户端打开控制流并发送一帧
        let (mut control_send, mut control_recv) = connection.open_bi().await.expect("open bi");
        frame::write_frame(&mut control_send, b"h")
            .await
            .expect("control write");

        let frame = tokio::time::timeout(Duration::from_secs(5), incoming_rx.next())
            .await
            .expect("frame timeout")
            .expect("frame")
            .expect("frame ok");
        assert_eq!(&frame[..], b"h");

        // 服务端出站帧必须出现在客户端收到的单向流上。
        // QUIC 流惰性物化:bridge 打开 uni 后要有数据写出,客户端才看得到流,
        // 所以先投递出站帧再等流出现
        outgoing_tx
            .send(Bytes::from_static(b"state"))
            .await
            .expect("outgoing send");
        let mut uni_recv = tokio::time::timeout(Duration::from_secs(5), connection.accept_uni())
            .await
            .expect("uni timeout")
            .expect("uni stream");
        // varint 长度头:5 字节 payload 恰为单字节 0x05
        let mut header = [0u8; 1];
        uni_recv.read_exact(&mut header).await.expect("header");
        assert_eq!(header[0], 5);
        let mut payload = vec![0u8; 5];
        uni_recv.read_exact(&mut payload).await.expect("payload");
        assert_eq!(&payload, b"state");

        // 控制流的服务端方向不允许出现数据(与 WT 门同款约定)
        drop(outgoing_tx);
        let mut probe = [0u8; 1];
        let probed =
            tokio::time::timeout(Duration::from_millis(300), control_recv.read(&mut probe)).await;
        assert!(
            !matches!(&probed, Ok(Ok(Some(_)))),
            "control stream server direction unexpectedly carried data"
        );
    }

    /// 集成测试:+zstd 套下 bridge 双向都必须走连续 zstd 分块流——
    /// 上行压缩块解出 envelope 进 incoming,下行 envelope 压成块上 uni 流。
    #[tokio::test]
    async fn zstd_suite_compresses_only_downlink() {
        let (addr, leaf, mut server_rx) = start_endpoint(&[TEST_SNI]).await.expect("endpoint");
        let connection = connect(addr, leaf, &[ALPN_ZSTD.as_bytes()]).await;
        assert_eq!(
            negotiated_alpn(&connection).as_deref(),
            Some(ALPN_ZSTD),
            "服务端应接受 +zstd ALPN"
        );
        let server_conn = server_rx.recv().await.expect("server accepted");

        let session = stream_bridge(server_conn, crate::compress::Suite::Zstd);
        let mut incoming_rx = session.incoming;
        let mut outgoing_tx = session.outgoing;

        // 上行恒为 plain:envelope 原样 varint 分帧写控制流(zstd 套仅压缩下行)
        let (mut control_send, _control_recv) = connection.open_bi().await.expect("open bi");
        for envelope in [b"handshake".to_vec(), vec![7u8; 2048]] {
            frame::write_frame(&mut control_send, &envelope)
                .await
                .expect("control write");
        }
        for envelope in [b"handshake".to_vec(), vec![7u8; 2048]] {
            let frame = tokio::time::timeout(Duration::from_secs(5), incoming_rx.next())
                .await
                .expect("frame timeout")
                .expect("frame")
                .expect("frame ok");
            assert_eq!(&frame[..], &envelope[..], "上行须按序原样还原 envelope");
        }

        // 下行:bridge 压出的块上 uni 流,测试侧持久解压器逐块还原
        outgoing_tx
            .send(Bytes::from_static(b"state-payload"))
            .await
            .expect("outgoing send");
        let mut uni_recv = tokio::time::timeout(Duration::from_secs(5), connection.accept_uni())
            .await
            .expect("uni timeout")
            .expect("uni stream");
        let wire = read_first_frame(&mut uni_recv).await;
        let mut downlink = crate::compress::StreamDecoder::new().expect("downlink decoder");
        let restored = downlink.decompress_chunk(&wire).expect("decompress");
        assert_eq!(&restored, b"state-payload");
    }

    /// 测试辅助:从流上读第一条 varint 分帧(read_frames 捕获首帧即停)。
    async fn read_first_frame<S>(stream: &mut S) -> Bytes
    where
        S: tokio::io::AsyncRead + Unpin,
    {
        let slot = std::rc::Rc::new(std::cell::RefCell::new(None::<Bytes>));
        let captured = slot.clone();
        frame::read_frames(stream, move |frame| {
            let stop = {
                let mut guard = captured.borrow_mut();
                let fresh = guard.is_none();
                if fresh {
                    *guard = Some(frame);
                }
                !fresh
            };
            std::future::ready(stop)
        })
        .await
        .expect("frame read");
        slot.borrow().clone().expect("frame captured")
    }

    /// 集成测试:relay 发布的 movement 批按裸块作为 datagram 到达客户端。
    #[tokio::test]
    async fn movement_batches_flow_as_datagrams() {
        let (addr, leaf, mut server_rx) = start_endpoint(&[TEST_SNI]).await.expect("endpoint");
        let connection = connect(addr, leaf, &[ALPN_PLAIN.as_bytes()]).await;
        let server_conn = server_rx.recv().await.expect("server accepted");

        let session = stream_bridge(server_conn, crate::compress::Suite::Plain);
        let movement_tx = session.movement_tx;

        let chunk_a: Arc<[u8]> = Bytes::from_static(b"abc").to_vec().into();
        let chunk_b: Arc<[u8]> = Bytes::from_static(b"xyz").to_vec().into();
        movement_tx.send_replace(Some(Arc::new(MovementBatch {
            chunks: Vec::from([chunk_a, chunk_b]).into(),
        })));

        let mut received = Vec::new();
        for _ in 0..2 {
            let datagram = tokio::time::timeout(Duration::from_secs(5), connection.read_datagram())
                .await
                .expect("datagram timeout")
                .expect("datagram ok");
            received.push(datagram.to_vec());
        }
        assert_eq!(received[0], b"abc");
        assert_eq!(received[1], b"xyz");
    }

    #[tokio::test]
    async fn datagram_budget_admits_movement_chunk() {
        // 环回路径下 max_datagram_size 必须容得下满额 movement 块,
        // 否则 datagram_capable 探测会让位置分流整个哑掉
        let (addr, leaf, _server_rx) = start_endpoint(&[TEST_SNI]).await.expect("endpoint");
        let connection = connect(addr, leaf, &[ALPN_PLAIN.as_bytes()]).await;
        let budget = connection
            .max_datagram_size()
            .expect("quinn datagram enabled by default");
        assert!(budget >= MOVEMENT_CHUNK_MAX_BYTES, "budget {budget}");
    }
}
