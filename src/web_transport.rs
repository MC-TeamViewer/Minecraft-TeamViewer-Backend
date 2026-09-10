use std::{
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use anyhow::Context as AnyhowContext;
use bytes::Bytes;
use futures_util::{Sink, Stream};

use tokio::sync::mpsc;
use tokio::sync::watch;
use tracing::{debug, info};
use wtransport::error::SendDatagramError;
use wtransport::{Endpoint, ServerConfig, endpoint::IncomingSession};

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

/// 门合同违规的应用层错误码(与 QUIC 门同值,断连出口唯一)。
const DOOR_VIOLATION_CODE: u32 = 0x01;

pub struct MpscReceiver {
    receiver: mpsc::Receiver<Result<Bytes, io::Error>>,
}

impl MpscReceiver {
    pub(crate) fn new(receiver: mpsc::Receiver<Result<Bytes, io::Error>>) -> Self {
        Self { receiver }
    }
}

impl Stream for MpscReceiver {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(cx)
    }
}

pub struct MpscSink {
    sender: mpsc::Sender<Bytes>,
}

impl MpscSink {
    pub(crate) fn new(sender: mpsc::Sender<Bytes>) -> Self {
        Self { sender }
    }
}

impl Sink<Bytes> for MpscSink {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self.sender.try_reserve() {
            Ok(_) => Poll::Ready(Ok(())),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Poll::Pending,
            Err(error) => Poll::Ready(Err(io::Error::other(error))),
        }
    }

    fn start_send(self: Pin<&mut Self>, item: Bytes) -> Result<(), Self::Error> {
        self.sender.try_send(item).map_err(io::Error::other)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

const WEB_TRANSPORT_PATH: &str = "/web-map/wt";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

pub async fn serve(config: WebTransportConfig, state: crate::web::AppState) -> anyhow::Result<()> {
    let (runtime, tls_config) = CertRuntime::load(&config).await?;
    let server_config = ServerConfig::builder()
        .with_bind_address(config.bind_address)
        .with_custom_tls_and_transport(tls_config, crate::quic_transport::bbr_transport_config())
        .build();
    let endpoint =
        Endpoint::server(server_config).context("failed to bind WebTransport UDP endpoint")?;
    info!(
        address = %config.bind_address,
        identities = %runtime.describe(),
        "WebTransport endpoint listening"
    );
    let mut reload_interval = tokio::time::interval(Duration::from_secs(config.poll_interval_sec));
    reload_interval.tick().await;
    loop {
        tokio::select! {
            _ = reload_interval.tick() => {
                runtime.reload().await;
            }
            incoming = endpoint.accept() => {
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(error) = accept_session(incoming, state).await {
                        debug!(%error, "WebTransport session rejected");
                    }
                });
            }
        }
    }
}

async fn accept_session(
    incoming: IncomingSession,
    state: crate::web::AppState,
) -> anyhow::Result<()> {
    let request = tokio::time::timeout(HANDSHAKE_TIMEOUT, incoming)
        .await
        .context("WebTransport session timeout")??;
    let remote_addr = request.remote_address();
    let path = request.path().to_owned();
    let user_agent = request.user_agent().map(str::to_owned);
    if path != WEB_TRANSPORT_PATH {
        drop(request.not_found());
        anyhow::bail!("unexpected WebTransport path: {path}");
    }

    // 压缩套协商搭 extended CONNECT 便车(0 额外 RTT):客户端在
    // WT-Available-Protocols(RFC 9651 List)按偏好序携带协议值,服务端择一
    // 并以 WT-Protocol 回显(必须取自客户端列表)。当前浏览器尚未实现该头,
    // 不发即 None → plain 行为,与 alpha.5 完全一致;未来浏览器实现后自动激活。
    let suite = wt_suite_from_headers(request.headers());
    let connection = match suite {
        Some(suite) => {
            request
                .accept_with_headers([("WT-Protocol", suite.wt_protocol_value())])
                .await?
        }
        None => request.accept().await?,
    };
    let remote_addr = remote_addr.to_string();
    info!(
        %remote_addr,
        path = %path,
        user_agent = user_agent.as_deref().unwrap_or("unknown"),
        ?suite,
        "WebTransport session accepted"
    );
    let mut session =
        stream_bridge(connection, suite.unwrap_or_default(), state.config.zstd_compression_level);
    // bulk 触发通道入表(debug 端点按表投递;会话死透时发送失败自然逐出)
    if let Some(bulk_tx) = session.bulk.take() {
        let hub_id = state.bulk_hub.register(bulk_tx).await;
        debug!(connection_id = %hub_id, "WebTransport session registered for bulk push");
    }
    web::serve_web_map_session(session, state, remote_addr).await;
    Ok(())
}

/// 从 extended CONNECT 请求头解析压缩套:`WT-Available-Protocols`(RFC 9651
/// 字符串 List,逗号分隔,客户端偏好序)。h3 头名本应小写,这里仍大小写不敏感
/// 查找以防客户端实现差异;无该头或无可识别值 → None(plain)。
fn wt_suite_from_headers(
    headers: &std::collections::HashMap<String, String>,
) -> Option<crate::compress::Suite> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("wt-available-protocols"))
        .map(|(_, value)| value)
        .and_then(|value| crate::compress::select_wt_protocol(value.split(',')))
}

fn stream_bridge(
    connection: wtransport::Connection,
    suite: crate::compress::Suite,
    compression_level: i32,
) -> crate::door::DoorSession {
    let (incoming_tx, incoming_rx) = mpsc::channel::<Result<Bytes, io::Error>>(256);
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<Bytes>(256);
    let (movement_tx, mut movement_rx) = watch::channel(None::<Arc<MovementBatch>>);
    // 上行位置 datagram 通道(alpha.5):桥内解析,只转发合法形状
    let (uplink_tx, uplink_rx) =
        mpsc::channel::<crate::proto::teamviewer::v1::PlayerReportBundle>(32);
    // bulk 触发通道(容量 1 = 每会话同时只允许一个在途 bulk,满即拒)
    let (bulk_tx, mut bulk_rx) = mpsc::channel::<bulk::BulkRequest>(1);
    // bulk 流必须在 door-control 下行流之后开启(流识别按开启序),
    // door_ready 由 bi 接收任务在开出 door-control 流后放行
    // Option 包装:仅首个 bulk 需要等门控流实体化,之后置空
    let (door_ready_tx, door_ready_rx) = tokio::sync::oneshot::channel::<()>();
    let mut door_ready_rx = Some(door_ready_rx);
    let read_connection = connection.clone();
    let write_connection = connection.clone();

    // 位置 datagram 发送任务:relay 每 dirty tick 经 movement watch 发布
    // 预切好的 movement 批,这里逐块压缩后装入单个 datagram——plain 套裸
    // WireEnvelope、+zstd* 套单帧自包含压缩,均无应用层长度前缀(datagram
    // 自带报文边界,alpha.5 起,与流分帧解耦)。同一 select! 消化 door-control
    // 上行的 dict_ready(+zstd-dict 套):激活就发生在本任务独占的编码器上,
    // 与编码天然串行,无锁。
    let send_connection = connection.clone();
    // door-control 下行流帧通道:dict_offer(+zstd-dict)与 bulk 通告共用,
    // 单一写出任务独占该流,天然保序
    let (door_frame_tx, door_frame_rx) = mpsc::channel::<Bytes>(8);
    let (dict_ready_tx, mut dict_ready_rx) = mpsc::channel::<String>(8);
    // bulk 任务的门控帧通道克隆(必须在 movement 任务 spawn 前建,它会把
    // door_frame_tx move 走)
    let bulk_door_tx = door_frame_tx.clone();
    let mut dict_encoder =
        door_control::DatagramDictEncoder::for_suite(suite, compression_level);
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
                    if let Some(offer) =
                        dict_encoder.observe(&batch, std::time::Instant::now())
                    {
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
                            // 握手时已确认对端支持 datagram;UnsupportedByPeer/NotConnected
                            // 意味着会话已死,停发即可——可靠流的低频位置刷新兜底
                            Err(SendDatagramError::TooLarge) => continue,
                            Err(error) => {
                                debug!(%error, "WebTransport datagram send stopped");
                                return;
                            }
                        }
                    }
                }
                // door-control 上行:dict_ready(ID) → 激活压缩字典。守卫兼防
                // 对端早退后通道关闭的热轮询(非 +zstd-dict 套恒关)
                Some(id) = dict_ready_rx.recv(), if dict_encoder.dict_enabled() => {
                    dict_encoder.activate(&id);
                    debug!(dictionary_id = %id, "WebTransport datagram dictionary activated");
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
            while let Ok(datagram) = drain_connection.receive_datagram().await {
                use prost::Message as _;
use crate::proto::teamviewer::v1::{WireChannel, WireEnvelope, wire_envelope};
                let Ok(envelope) = WireEnvelope::decode(datagram.payload().as_ref()) else {
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
            while let Ok(extra) = control_connection.accept_uni().await {
                door_violation(&control_connection, "unexpected client uni stream");
                extra.stop(DOOR_VIOLATION_CODE.into());
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
                if let Some(door_ready) = door_ready_rx.as_mut()
                    && door_ready.await.is_err()
                {
                    break;
                }
                let Ok(opening) = bulk_connection.open_uni().await else {
                    break;
                };
                let Ok(mut stream) = opening.await else {
                    break;
                };
                // 低优先级主实现:门控/应用流(默认 0)永远先于 bulk 出队
                stream.set_priority(bulk::STREAM_PRIORITY);
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
                    pacer.wait(chunk.len(), bulk_connection.rtt()).await;
                    if stream.write_all(chunk).await.is_err() {
                        failed = true;
                        break;
                    }
                }
                // wtransport 的 finish 是 async(FIN 落网后才 resolve),丢弃
                // future 等于从不发 FIN:必须 await;写侧已尽,错误无需处理
                let _ = stream.finish().await;
                stop.store(true, Ordering::Relaxed);
                let _ = heartbeat_task.await;
                debug!(%transfer_id, len = request.content.len(), "WebTransport bulk transfer done");
            }
        });
    }

    tokio::spawn(async move {
        let control = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_connection.accept_bi()).await;
        let Ok(Ok((send, mut recv))) = control else {
            let _ = incoming_tx
                .send(Err(io::Error::other("WebTransport control stream missing")))
                .await;
            return;
        };
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
        // web 脚本只从 incomingUnidirectionalStreams 读取,写回 bi 流 send 半流无人消费
        drop(send);
        let opening = match tokio::time::timeout(WRITE_TIMEOUT, write_connection.open_uni()).await {
            Ok(Ok(opening)) => opening,
            _ => {
                let _ = incoming_tx
                    .send(Err(io::Error::other(
                        "WebTransport state stream unavailable",
                    )))
                    .await;
                return;
            }
        };
        let mut state_stream = match opening.await {
            Ok(state_stream) => state_stream,
            Err(error) => {
                let _ = incoming_tx
                    .send(Err(io::Error::other(format!(
                        "WebTransport state stream failed: {error}"
                    ))))
                    .await;
                return;
            }
        };

        // door-control 下行流:服务端第 2 条单向流,按需实体化——首帧需要
        // 时才开启(流序仍为第 2 条:应用下行流的首帧 ack 必然先于任何
        // 门控帧,而 bulk 泵要等 door_ready 放行后才开第 3 条)。字典载荷
        // 仍只在 +zstd-dict 套产生。开不出来时字典永不激活、bulk 不启动,
        // datagram 维持独立压缩,链路照常。
        let door_pump_connection = write_connection.clone();
        tokio::spawn(async move {
            let mut door_frame_rx = door_frame_rx;
            let Some(first_frame) = door_frame_rx.recv().await else {
                return;
            };
            let Ok(opening) = door_pump_connection.open_uni().await else {
                debug!("WebTransport door-control stream unavailable");
                return;
            };
            let Ok(mut door_stream) = opening.await else {
                debug!("WebTransport door-control stream failed");
                return;
            };
            if frame::write_frame(&mut door_stream, &first_frame).await.is_err() {
                debug!("WebTransport door-control first frame write failed");
                return;
            }
            // 首帧已过线,流序 #2 已占,bulk 流(uni #3+)自此放行
            let _ = door_ready_tx.send(());
            while let Some(door_frame) = door_frame_rx.recv().await {
                if let Err(error) = frame::write_frame(&mut door_stream, &door_frame).await {
                    debug!(%error, "WebTransport door-control write failed");
                    break;
                }
            }
        });

        let reader = tokio::spawn(async move {
            // 上行恒为 plain varint 分帧:zstd 套只压缩下行(上行载荷小,
            // 且浏览器端 fzstd 仅解压,压缩套语义为单向)。
            if let Err(error) = frame::read_frames(&mut recv, |frame| {
                let incoming_tx = incoming_tx.clone();
                async move { incoming_tx.send(Ok(frame)).await.is_ok() }
            })
            .await
                && incoming_tx
                    .send(Err(io::Error::other(format!(
                        "WebTransport read failed: {error}"
                    ))))
                    .await
                    .is_err()
            {}
        });

        // 下行 zstd:envelope → 持久 CCtx flush 出压缩块 → `[varint][块]`
        let mut encoder = if suite.stream_zstd() {
            match crate::compress::StreamEncoder::new(compression_level) {
                Ok(encoder) => Some(encoder),
                Err(error) => {
                    debug!(%error, "WebTransport downlink encoder unavailable");
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
                        debug!(%error, "WebTransport downlink compress failed");
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
                debug!(%error, "WebTransport state write failed");
                break;
            }
        }
        reader.abort();
    });

    crate::door::DoorSession {
        incoming: crate::web::WebMapFrameStream::new(MpscReceiver::new(incoming_rx)),
        outgoing: MpscSink::new(outgoing_tx),
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
fn door_violation(connection: &wtransport::Connection, reason: &str) {
    debug!(%reason, "WebTransport door contract violation; closing session");
    connection.close(DOOR_VIOLATION_CODE.into(), reason.as_bytes());
}

/// bulk 期间的门控心跳:每 `bulk::HEARTBEAT_INTERVAL` 发一枚固定魔数
/// datagram,供接收端统计饱和期送达率。`stop` 置位或会话死亡即退出。
fn spawn_bulk_heartbeat(
    connection: &wtransport::Connection,
    stop: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    let connection = connection.clone();
    tokio::spawn(async move {
        let mut seq: u32 = 0;
        let mut interval = tokio::time::interval(bulk::HEARTBEAT_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        while !stop.load(Ordering::Relaxed) {
            interval.tick().await;
            // 对端未协商 datagram 扩展或会话已死:静默停发(心跳是纯观测)
            if connection.send_datagram(bulk::heartbeat(seq)).is_err() {
                break;
            }
            seq = seq.wrapping_add(1);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        net::SocketAddr,
        path::{Path, PathBuf},
        process,
        time::{SystemTime, UNIX_EPOCH},
    };

    use wtransport::Identity;

    use crate::config::CertIdentity;
    use wtransport::config::{ClientConfig, DnsLookupFuture, DnsResolver};
    use wtransport::stream::RecvStream;
    use wtransport::tls::Sha256Digest;

    #[test]
    fn splits_varint_prefixed_frames() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&[0x07]);
        wire.extend_from_slice(b"web-map");
        match frame::split_frame_header(&wire) {
            frame::FrameScan::Complete {
                header_len,
                payload_len,
            } => {
                assert_eq!(header_len, 1);
                assert_eq!(payload_len, 7);
            }
            other => panic!("expected complete frame, got {other:?}"),
        }
    }

    #[test]
    fn rejects_oversized_frames() {
        // 4 字节 varint,值 = MAX_FRAME_LEN + 1
        let frame = [0x81u8, 0x80, 0x80, 0x04];
        assert!(matches!(
            frame::split_frame_header(&frame),
            frame::FrameScan::Malformed
        ));
    }

    // ---------- 集成测试:真实 QUIC 握手 ----------

    /// 把测试域名静态解析到 127.0.0.1,让客户端可用任意 SNI 直连本机 endpoint。
    #[derive(Debug)]
    struct StaticDnsResolver;

    impl DnsResolver for StaticDnsResolver {
        fn resolve(&self, host: &str) -> Pin<Box<dyn DnsLookupFuture>> {
            let port = host
                .rsplit(':')
                .next()
                .and_then(|port| port.parse().ok())
                .unwrap_or(443);
            Box::pin(async move { Ok(Some(SocketAddr::from(([127, 0, 0, 1], port)))) })
        }
    }

    struct TestCert {
        cert_path: PathBuf,
        key_path: PathBuf,
        digest: Sha256Digest,
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("wt-multi-cert-{}-{nanos}-{name}", process::id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn write_test_cert(dir: &Path, name: &str, sans: &[&str]) -> TestCert {
        let identity = Identity::self_signed(sans).expect("self signed identity");
        let cert_pem = identity
            .certificate_chain()
            .as_slice()
            .iter()
            .map(|certificate| certificate.to_pem())
            .collect::<Vec<_>>()
            .join("");
        let key_pem = identity.private_key().to_secret_pem();
        let cert_path = dir.join(format!("{name}.cert.pem"));
        let key_path = dir.join(format!("{name}.key.pem"));
        fs::write(&cert_path, cert_pem).expect("write cert pem");
        fs::write(&key_path, key_pem).expect("write key pem");
        let digest = identity.certificate_chain().as_slice()[0].hash();
        TestCert {
            cert_path,
            key_path,
            digest,
        }
    }

    fn wt_config(
        bind: SocketAddr,
        identities: Vec<CertIdentity>,
        renew_window_sec: u64,
    ) -> WebTransportConfig {
        WebTransportConfig {
            enabled: true,
            bind_address: bind,
            identities,
            poll_interval_sec: 30,
            renew_window_sec,
        }
    }

    fn cert_identity(cert: &TestCert, default: bool) -> CertIdentity {
        CertIdentity {
            cert_path: cert.cert_path.clone(),
            key_path: cert.key_path.clone(),
            default,
        }
    }

    async fn start_endpoint(
        config: &WebTransportConfig,
    ) -> anyhow::Result<(SocketAddr, CertRuntime)> {
        let (runtime, tls_config) = CertRuntime::load(config).await?;
        let server_config = ServerConfig::builder()
            .with_bind_address(config.bind_address)
            .with_custom_tls(tls_config)
            .build();
        let endpoint = Endpoint::server(server_config)?;
        let local_addr = endpoint.local_addr()?;
        tokio::spawn(async move {
            loop {
                let incoming = endpoint.accept().await;
                tokio::spawn(async move {
                    if let Ok(Ok(request)) = tokio::time::timeout(HANDSHAKE_TIMEOUT, incoming).await
                    {
                        // 接受会话让客户端 connect() 完成;之后等客户端先关闭,
                        // 避免服务端先 drop Connection 发出 close(0) 干扰握手结果
                        if let Ok(connection) = request.accept().await {
                            let _ =
                                tokio::time::timeout(HANDSHAKE_TIMEOUT, connection.closed()).await;
                        }
                    }
                });
            }
        });
        Ok((local_addr, runtime))
    }

    /// 用证书哈希白名单连一次握手,返回 TLS 握手是否成功(即服务端选中的证书是否为 allowed)。
    async fn handshake_allowed(addr: SocketAddr, host: &str, allowed: Sha256Digest) -> bool {
        let client_config = ClientConfig::builder()
            .with_bind_default()
            .with_server_certificate_hashes([allowed])
            .dns_resolver(StaticDnsResolver)
            .build();
        let endpoint = Endpoint::client(client_config).expect("client endpoint");
        let url = format!("https://{host}:{}{WEB_TRANSPORT_PATH}", addr.port());
        match endpoint.connect(url).await {
            Ok(_session) => true,
            Err(error) => {
                eprintln!("handshake to {host} failed: {error:#}");
                false
            }
        }
    }

    #[tokio::test]
    async fn sni_selects_matching_certificate() {
        let dir = temp_dir("sni");
        let a = write_test_cert(&dir, "a", &["a.example.com"]);
        let b = write_test_cert(&dir, "b", &["b.example.com"]);
        let config = wt_config(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            vec![cert_identity(&a, false), cert_identity(&b, true)],
            7 * 24 * 60 * 60,
        );
        let (addr, _runtime) = start_endpoint(&config).await.expect("endpoint");

        assert!(handshake_allowed(addr, "a.example.com", a.digest.clone()).await);
        assert!(!handshake_allowed(addr, "a.example.com", b.digest.clone()).await);
        assert!(handshake_allowed(addr, "b.example.com", b.digest.clone()).await);
    }

    #[tokio::test]
    async fn ip_direct_serves_default_certificate() {
        let dir = temp_dir("ip");
        let a = write_test_cert(&dir, "a", &["a.example.com"]);
        let b = write_test_cert(&dir, "b", &["b.example.com", "127.0.0.1"]);
        let config = wt_config(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            vec![cert_identity(&a, false), cert_identity(&b, true)],
            7 * 24 * 60 * 60,
        );
        let (addr, _runtime) = start_endpoint(&config).await.expect("endpoint");

        // rustls 将 IP SNI 视为无 SNI → 命中 default 证书 b
        assert!(handshake_allowed(addr, "127.0.0.1", b.digest.clone()).await);
        assert!(!handshake_allowed(addr, "127.0.0.1", a.digest.clone()).await);
    }

    #[tokio::test]
    async fn wildcard_sni_selects_certificate() {
        let dir = temp_dir("wild");
        let w = write_test_cert(&dir, "w", &["*.wt.test"]);
        let config = wt_config(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            vec![cert_identity(&w, false)],
            7 * 24 * 60 * 60,
        );
        let (addr, _runtime) = start_endpoint(&config).await.expect("endpoint");

        assert!(handshake_allowed(addr, "x.wt.test", w.digest.clone()).await);
    }

    #[tokio::test]
    async fn reload_swaps_certificate_for_new_connections() {
        let dir = temp_dir("reload");
        let initial = write_test_cert(&dir, "initial", &["a.example.com"]);
        let cert_path = dir.join("svc.cert.pem");
        let key_path = dir.join("svc.key.pem");
        fs::copy(&initial.cert_path, &cert_path).expect("copy cert");
        fs::copy(&initial.key_path, &key_path).expect("copy key");
        let identity = CertIdentity {
            cert_path: cert_path.clone(),
            key_path: key_path.clone(),
            default: false,
        };
        let config = wt_config(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            vec![identity],
            7 * 24 * 60 * 60,
        );
        let (addr, runtime) = start_endpoint(&config).await.expect("endpoint");
        assert!(handshake_allowed(addr, "a.example.com", initial.digest.clone()).await);

        let updated = write_test_cert(&dir, "updated", &["a.example.com"]);
        fs::copy(&updated.cert_path, &cert_path).expect("overwrite cert");
        fs::copy(&updated.key_path, &key_path).expect("overwrite key");
        runtime.reload().await;
        assert!(handshake_allowed(addr, "a.example.com", updated.digest.clone()).await);
        assert!(!handshake_allowed(addr, "a.example.com", initial.digest.clone()).await);
    }

    #[tokio::test]
    async fn reload_keeps_certificate_expiring_inside_renew_window() {
        let dir = temp_dir("renew");
        let initial = write_test_cert(&dir, "initial", &["a.example.com"]);
        let cert_path = dir.join("svc.cert.pem");
        let key_path = dir.join("svc.key.pem");
        fs::copy(&initial.cert_path, &cert_path).expect("copy cert");
        fs::copy(&initial.key_path, &key_path).expect("copy key");
        let identity = CertIdentity {
            cert_path: cert_path.clone(),
            key_path: key_path.clone(),
            default: false,
        };
        // renewWindow 30 天 > 自签证书 14 天有效期 → 换新被拒绝,保留旧证书
        let config = wt_config(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            vec![identity],
            30 * 24 * 60 * 60,
        );
        let (addr, runtime) = start_endpoint(&config).await.expect("endpoint");

        let updated = write_test_cert(&dir, "updated", &["a.example.com"]);
        fs::copy(&updated.cert_path, &cert_path).expect("overwrite cert");
        fs::copy(&updated.key_path, &key_path).expect("overwrite key");
        runtime.reload().await;
        assert!(handshake_allowed(addr, "a.example.com", initial.digest.clone()).await);
        assert!(!handshake_allowed(addr, "a.example.com", updated.digest.clone()).await);
    }

    #[tokio::test]
    async fn reload_with_broken_file_keeps_old_certificate() {
        let dir = temp_dir("broken");
        let a = write_test_cert(&dir, "a", &["a.example.com"]);
        let b = write_test_cert(&dir, "b", &["b.example.com"]);
        let config = wt_config(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            vec![cert_identity(&a, false), cert_identity(&b, false)],
            7 * 24 * 60 * 60,
        );
        let (addr, runtime) = start_endpoint(&config).await.expect("endpoint");

        // 损坏 b 的私钥文件后重载:b 保留旧证书,a 不受影响
        fs::write(&b.key_path, "not a key").expect("corrupt key");
        runtime.reload().await;
        assert!(handshake_allowed(addr, "a.example.com", a.digest.clone()).await);
        assert!(handshake_allowed(addr, "b.example.com", b.digest.clone()).await);
    }

    async fn read_exact(stream: &mut RecvStream, buf: &mut [u8]) {
        let mut filled = 0;
        while filled < buf.len() {
            let read =
                tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf[filled..]))
                    .await
                    .expect("read timeout")
                    .expect("read ok")
                    .expect("stream open");
            assert!(read > 0);
            filled += read;
        }
    }

    /// 回归测试:client→server 帧走客户端 bi 控制流,
    /// server→client 帧(出站 MpscSink)必须走服务端单向流——web 脚本只读单向流。
    /// 分层铁律端到端实证:门控上行流上的非法 protobuf(混用/垃圾)
    /// 必须当场断连,不得静默容忍。
    #[tokio::test]
    async fn door_uplink_undecodable_frame_closes_session() {
        use crate::proto::teamviewer::v1::WireEnvelope;
        use prost::Message as _;

        let dir = temp_dir("bridge-door-violation");
        let cert = write_test_cert(&dir, "a", &["a.example.com"]);
        let config = wt_config(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            vec![cert_identity(&cert, true)],
            7 * 24 * 60 * 60,
        );
        let (runtime, tls_config) = CertRuntime::load(&config).await.expect("load");
        let server_config = ServerConfig::builder()
            .with_bind_address(config.bind_address)
            .with_custom_tls(tls_config)
            .build();
        let endpoint = Endpoint::server(server_config).expect("server endpoint");
        let addr = endpoint.local_addr().expect("local addr");
        tokio::spawn(async move {
            let request = tokio::time::timeout(HANDSHAKE_TIMEOUT, endpoint.accept().await)
                .await
                .expect("incoming timeout")
                .expect("session request");
            let connection = request.accept().await.expect("session accepted");
            let _bridge = stream_bridge(connection, crate::compress::Suite::Plain, crate::compress::DEFAULT_STREAM_COMPRESSION_LEVEL);
        });
        let _ = runtime;

        let client_config = ClientConfig::builder()
            .with_bind_default()
            .with_server_certificate_hashes([cert.digest.clone()])
            .dns_resolver(StaticDnsResolver)
            .build();
        let client = Endpoint::client(client_config).expect("client endpoint");
        let url = format!("https://a.example.com:{}{WEB_TRANSPORT_PATH}", addr.port());
        let session = client.connect(url).await.expect("connect");

        // 客户端把应用层 envelope 塞进 door-control 上行(跨层混用):
        // prost 对 oneof 成员 wire type 不匹配直接解析失败 → 服务端违规断连
        let uni = session.open_uni().await.expect("open uni");
        let mut door_uplink = uni.await.expect("uni stream");
        let envelope = WireEnvelope::default();
        let mut buf = Vec::with_capacity(envelope.encoded_len());
        envelope.encode(&mut buf).expect("envelope encode");
        frame::write_frame(&mut door_uplink, &buf)
            .await
            .expect("uplink write");

        // 会话必须在数秒内被服务端关闭(违规断连),而非静默吞掉
        let _closed = tokio::time::timeout(Duration::from_secs(5), session.closed())
            .await
            .expect("session should be closed by door violation");
    }

    #[tokio::test]
    async fn state_frames_flow_on_unidirectional_stream() {
        use futures_util::{SinkExt as _, StreamExt as _};
        let dir = temp_dir("bridge");
        let cert = write_test_cert(&dir, "a", &["a.example.com"]);
        let config = wt_config(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            vec![cert_identity(&cert, true)],
            7 * 24 * 60 * 60,
        );
        let (runtime, tls_config) = CertRuntime::load(&config).await.expect("load");
        let server_config = ServerConfig::builder()
            .with_bind_address(config.bind_address)
            .with_custom_tls(tls_config)
            .build();
        let endpoint = Endpoint::server(server_config).expect("server endpoint");
        let addr = endpoint.local_addr().expect("local addr");
        let (bridges_tx, mut bridges_rx) =
            tokio::sync::mpsc::channel::<crate::door::DoorSession>(1);
        tokio::spawn(async move {
            let request = tokio::time::timeout(HANDSHAKE_TIMEOUT, endpoint.accept().await)
                .await
                .expect("incoming timeout")
                .expect("session request");
            let connection = request.accept().await.expect("session accepted");
            let bridge = stream_bridge(connection, crate::compress::Suite::Plain, crate::compress::DEFAULT_STREAM_COMPRESSION_LEVEL);
            bridges_tx.send(bridge).await.ok();
        });
        let _ = runtime;

        let client_config = ClientConfig::builder()
            .with_bind_default()
            .with_server_certificate_hashes([cert.digest.clone()])
            .dns_resolver(StaticDnsResolver)
            .build();
        let client = Endpoint::client(client_config).expect("client endpoint");
        let url = format!("https://a.example.com:{}{WEB_TRANSPORT_PATH}", addr.port());
        let session = client.connect(url).await.expect("connect");

        // 客户端打开控制流并发送一帧
        let control = session.open_bi().await.expect("open bi");
        let (mut control_send, mut control_recv) = control.await.expect("bi stream");
        frame::write_frame(&mut control_send, b"h")
            .await
            .expect("control write");

        // 服务端 bridge 读到该帧
        let arrived = tokio::time::timeout(Duration::from_secs(5), bridges_rx.recv())
            .await
            .expect("bridge ready")
            .expect("bridge channels");
        let mut incoming_rx = arrived.incoming;
        let mut outgoing_tx = arrived.outgoing;
        let frame = tokio::time::timeout(Duration::from_secs(5), incoming_rx.next())
            .await
            .expect("frame timeout")
            .expect("frame")
            .expect("frame ok");
        assert_eq!(&frame[..], b"h");

        // 服务端出站帧必须出现在客户端的单向流上
        let mut uni_recv = tokio::time::timeout(Duration::from_secs(5), session.accept_uni())
            .await
            .expect("uni timeout")
            .expect("uni stream");
        outgoing_tx
            .send(Bytes::from_static(b"state"))
            .await
            .expect("outgoing send");
        // varint 长度头:5 字节 payload 恰为单字节 0x05
        let mut header = [0u8; 1];
        read_exact(&mut uni_recv, &mut header).await;
        assert_eq!(header[0], 5);
        let mut payload = vec![0u8; 5];
        read_exact(&mut uni_recv, &mut payload).await;
        assert_eq!(&payload, b"state");

        // 控制流的服务端方向不允许出现数据(修复前状态帧被错误写到这里);
        // 服务端 drop send 半流会产生干净 EOF,同样算通过
        let mut probe = [0u8; 1];
        let probed =
            tokio::time::timeout(Duration::from_millis(500), control_recv.read(&mut probe)).await;
        assert!(
            !matches!(&probed, Ok(Ok(Some(_)))),
            "control stream server direction unexpectedly carried data"
        );
    }

    /// 集成测试:relay 经 movement watch 发布的批,必须原样作为 datagram
    /// 到达客户端(裸 WireEnvelope,无长度前缀;datagram 自带报文边界)。
    #[tokio::test]
    async fn movement_batches_flow_as_datagrams() {
        let dir = temp_dir("datagram");
        let cert = write_test_cert(&dir, "a", &["a.example.com"]);
        let config = wt_config(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            vec![cert_identity(&cert, true)],
            7 * 24 * 60 * 60,
        );
        let (runtime, tls_config) = CertRuntime::load(&config).await.expect("load");
        let server_config = ServerConfig::builder()
            .with_bind_address(config.bind_address)
            .with_custom_tls(tls_config)
            .build();
        let endpoint = Endpoint::server(server_config).expect("server endpoint");
        let addr = endpoint.local_addr().expect("local addr");
        let (sessions_tx, mut sessions_rx) =
            tokio::sync::mpsc::channel::<crate::door::DoorSession>(1);
        tokio::spawn(async move {
            let request = tokio::time::timeout(HANDSHAKE_TIMEOUT, endpoint.accept().await)
                .await
                .expect("incoming timeout")
                .expect("session request");
            let connection = request.accept().await.expect("session accepted");
            let bridge = stream_bridge(connection, crate::compress::Suite::Plain, crate::compress::DEFAULT_STREAM_COMPRESSION_LEVEL);
            sessions_tx.send(bridge).await.ok();
        });
        let _ = runtime;

        let client_config = ClientConfig::builder()
            .with_bind_default()
            .with_server_certificate_hashes([cert.digest.clone()])
            .dns_resolver(StaticDnsResolver)
            .build();
        let client = Endpoint::client(client_config).expect("client endpoint");
        let url = format!("https://a.example.com:{}{WEB_TRANSPORT_PATH}", addr.port());
        let session = client.connect(url).await.expect("connect");

        // 客户端打开控制流触发 bridge 建立
        let control = session.open_bi().await.expect("open bi");
        let (mut control_send, _control_recv) = control.await.expect("bi stream");
        frame::write_frame(&mut control_send, b"h")
            .await
            .expect("control write");

        let arrived = tokio::time::timeout(Duration::from_secs(5), sessions_rx.recv())
            .await
            .expect("bridge ready")
            .expect("bridge channels");
        let movement_tx = arrived.movement_tx;

        // relay 侧发布一个 movement 批(两块);块即 datagram 载荷本身
        let chunk_a: Arc<[u8]> = Bytes::from_static(b"abc").to_vec().into();
        let chunk_b: Arc<[u8]> = Bytes::from_static(b"xyz").to_vec().into();
        movement_tx.send_replace(Some(Arc::new(MovementBatch {
            chunks: Vec::from([chunk_a, chunk_b]).into(),
        })));

        // 客户端按序收到两个 datagram,载荷 = 裸块(无长度前缀)
        let mut received = Vec::new();
        for _ in 0..2 {
            let datagram = tokio::time::timeout(Duration::from_secs(5), session.receive_datagram())
                .await
                .expect("datagram timeout")
                .expect("datagram ok");
            received.push(datagram.payload().to_vec());
        }
        assert_eq!(received[0], b"abc");
        assert_eq!(received[1], b"xyz");
    }

    #[test]
    fn wt_suite_negotiation_parses_available_protocols_header() {
        let mut headers = std::collections::HashMap::new();
        // 无该头:当前浏览器均未实现 → plain 路径(alpha.5 行为)
        assert_eq!(wt_suite_from_headers(&headers), None);
        // 规范形态:RFC 9651 List,带引号字符串项,按客户端偏好序择一
        headers.insert(
            "wt-available-protocols".to_string(),
            "\"teamviewrelay.zstd.v1\", \"teamviewrelay.plain.v1\"".to_string(),
        );
        assert_eq!(
            wt_suite_from_headers(&headers),
            Some(crate::compress::Suite::Zstd)
        );
        // 头名大小写不敏感兜底
        let mut upper = std::collections::HashMap::new();
        upper.insert(
            "WT-Available-Protocols".to_string(),
            "\"teamviewrelay.plain.v1\"".to_string(),
        );
        assert_eq!(
            wt_suite_from_headers(&upper),
            Some(crate::compress::Suite::Plain)
        );
        // 无可识别值 → None
        let mut other = std::collections::HashMap::new();
        other.insert(
            "wt-available-protocols".to_string(),
            "\"chat.v2\", \"other\"".to_string(),
        );
        assert_eq!(wt_suite_from_headers(&other), None);
    }

    /// zstd 套桥接单向压缩:上行 bi 流恒为 plain 分帧(原样解出 envelope),
    /// 下行 uni 流由桥内持久 CCtx 逐 envelope flush 压缩;下行发两个
    /// envelope,第二个验证跨块上下文共享(重新初始化的压缩器不可能让第二
    /// 块解出依赖首块窗口的字节)。
    #[tokio::test]
    async fn zstd_suite_compresses_only_downlink() {
        use futures_util::{SinkExt as _, StreamExt as _};
        let dir = temp_dir("bridge-zstd");
        let cert = write_test_cert(&dir, "a", &["a.example.com"]);
        let config = wt_config(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            vec![cert_identity(&cert, true)],
            7 * 24 * 60 * 60,
        );
        let (runtime, tls_config) = CertRuntime::load(&config).await.expect("load");
        let server_config = ServerConfig::builder()
            .with_bind_address(config.bind_address)
            .with_custom_tls(tls_config)
            .build();
        let endpoint = Endpoint::server(server_config).expect("server endpoint");
        let addr = endpoint.local_addr().expect("local addr");
        let (bridges_tx, mut bridges_rx) =
            tokio::sync::mpsc::channel::<crate::door::DoorSession>(1);
        tokio::spawn(async move {
            let request = tokio::time::timeout(HANDSHAKE_TIMEOUT, endpoint.accept().await)
                .await
                .expect("incoming timeout")
                .expect("session request");
            let connection = request.accept().await.expect("session accepted");
            let bridge = stream_bridge(connection, crate::compress::Suite::Zstd, crate::compress::DEFAULT_STREAM_COMPRESSION_LEVEL);
            bridges_tx.send(bridge).await.ok();
        });
        let _ = runtime;

        let client_config = ClientConfig::builder()
            .with_bind_default()
            .with_server_certificate_hashes([cert.digest.clone()])
            .dns_resolver(StaticDnsResolver)
            .build();
        let client = Endpoint::client(client_config).expect("client endpoint");
        let url = format!("https://a.example.com:{}{WEB_TRANSPORT_PATH}", addr.port());
        let session = client.connect(url).await.expect("connect");

        // 上行恒为 plain:envelope 原样 varint 分帧写控制流(zstd 套仅压缩下行)
        let control = session.open_bi().await.expect("open bi");
        let (mut control_send, _control_recv) = control.await.expect("bi stream");
        let uplink_envelopes: [&[u8]; 2] = [b"first-uplink", b"second-uplink-payload-larger"];
        for envelope in uplink_envelopes {
            frame::write_frame(&mut control_send, envelope)
                .await
                .expect("control write");
        }

        // 服务端桥原样解出两个 envelope,顺序一致
        let arrived = tokio::time::timeout(Duration::from_secs(5), bridges_rx.recv())
            .await
            .expect("bridge ready")
            .expect("bridge channels");
        let mut incoming_rx = arrived.incoming;
        let mut outgoing_tx = arrived.outgoing;
        for expected in uplink_envelopes {
            let frame = tokio::time::timeout(Duration::from_secs(5), incoming_rx.next())
                .await
                .expect("frame timeout")
                .expect("frame")
                .expect("frame ok");
            assert_eq!(&frame[..], expected);
        }

        // 服务端出站:envelope 经桥内持久 CCtx 压缩后写 uni 流;
        // 客户端读 varint 帧,喂进自己的持久 DCtx 逐块解出
        let mut uni_recv = tokio::time::timeout(Duration::from_secs(5), session.accept_uni())
            .await
            .expect("uni timeout")
            .expect("uni stream");
        let mut downlink_decoder = crate::compress::StreamDecoder::new().expect("downlink decoder");
        let downlink_envelopes: [&[u8]; 2] = [b"first-downlink", b"second-downlink-payload-larger"];
        for envelope in downlink_envelopes {
            outgoing_tx
                .send(Bytes::from_static(envelope))
                .await
                .expect("outgoing send");
            // 压缩块远小于 128 字节,varint 长度头为单字节
            let mut header = [0u8; 1];
            read_exact(&mut uni_recv, &mut header).await;
            let mut payload = vec![0u8; header[0] as usize];
            read_exact(&mut uni_recv, &mut payload).await;
            let decoded = downlink_decoder
                .decompress_chunk(&payload)
                .expect("decompress");
            assert_eq!(decoded, envelope);
        }
    }

    /// +zstd-dict 套全因果链:独立压缩 datagram → 样本攒满训练字典 →
    /// dict_offer 经服务端第 2 条单向流下发(开序必须紧跟状态流)→ 客户端
    /// 回 dict_ready(客户端第 1 条单向流)→ 服务端激活 → 后续 datagram
    /// 换字典压缩。door-control 帧与流通道同款 varint 分帧,载荷为
    /// DoorControlFrame protobuf。
    #[tokio::test]
    async fn zstd_dict_suite_trains_activates_datagram_dictionary() {
        use crate::proto::teamviewer::door::v1::{
            DatagramDictReady, DoorControlFrame, door_control_frame::Payload as DoorControlPayload,
        };
        use prost::Message as _;

        let dir = temp_dir("bridge-zstd-dict");
        let cert = write_test_cert(&dir, "a", &["a.example.com"]);
        let config = wt_config(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            vec![cert_identity(&cert, true)],
            7 * 24 * 60 * 60,
        );
        let (runtime, tls_config) = CertRuntime::load(&config).await.expect("load");
        let server_config = ServerConfig::builder()
            .with_bind_address(config.bind_address)
            .with_custom_tls(tls_config)
            .build();
        let endpoint = Endpoint::server(server_config).expect("server endpoint");
        let addr = endpoint.local_addr().expect("local addr");
        let (bridges_tx, mut bridges_rx) =
            tokio::sync::mpsc::channel::<crate::door::DoorSession>(1);
        tokio::spawn(async move {
            let request = tokio::time::timeout(HANDSHAKE_TIMEOUT, endpoint.accept().await)
                .await
                .expect("incoming timeout")
                .expect("session request");
            let connection = request.accept().await.expect("session accepted");
            let bridge = stream_bridge(connection, crate::compress::Suite::ZstdDict, crate::compress::DEFAULT_STREAM_COMPRESSION_LEVEL);
            bridges_tx.send(bridge).await.ok();
        });
        let _ = runtime;

        let client_config = ClientConfig::builder()
            .with_bind_default()
            .with_server_certificate_hashes([cert.digest.clone()])
            .dns_resolver(StaticDnsResolver)
            .build();
        let client = Endpoint::client(client_config).expect("client endpoint");
        let url = format!("https://a.example.com:{}{WEB_TRANSPORT_PATH}", addr.port());
        let session = client.connect(url).await.expect("connect");

        // 客户端开控制流触发 bridge;服务端第 1 条单向流 = 应用层状态流
        // (door-control 按需实体化:首帧 dict_offer 写出时才过线,故先发
        // movement 触发训练,再 accept 第 2 条单向流)
        let control = session.open_bi().await.expect("open bi");
        let (mut control_send, _control_recv) = control.await.expect("bi stream");
        frame::write_frame(&mut control_send, b"h")
            .await
            .expect("control write");
        let state_stream = tokio::time::timeout(Duration::from_secs(5), session.accept_uni())
            .await
            .expect("state uni timeout")
            .expect("state uni");
        let arrived = tokio::time::timeout(Duration::from_secs(5), bridges_rx.recv())
            .await
            .expect("bridge ready")
            .expect("bridge channels");
        let incoming_rx = arrived.incoming;
        let movement_tx = arrived.movement_tx;
        drop(incoming_rx);
        drop(state_stream);

        // 第 1 批 movement:64 块恰好同时跨过生产训练阈值(条数 64、样本
        // 字节 ≥16KiB),首个 dirty tick 即训练并经 door-control 下发 offer
        let mut chunks: Vec<Arc<[u8]>> = Vec::new();
        for index in 0..64 {
            let mut chunk = format!(r#"{{"p":"uuid-{index}","x":{index}.25,"y":0.5,"#).into_bytes();
            chunk.resize(270, b' ');
            chunk.extend_from_slice(b"}");
            chunks.push(chunk.into());
        }
        movement_tx.send_replace(Some(Arc::new(MovementBatch {
            chunks: chunks.clone().into(),
        })));

        // 首个门控帧(dict_offer)写出 → door-control 下行流此刻过线
        let mut offer_stream = tokio::time::timeout(Duration::from_secs(5), session.accept_uni())
            .await
            .expect("door-control uni timeout")
            .expect("door-control uni");

        // 激活前:每个 datagram 都是独立(无字典)zstd 帧,可无字典解出
        for expected in &chunks {
            let datagram = tokio::time::timeout(Duration::from_secs(5), session.receive_datagram())
                .await
                .expect("datagram timeout")
                .expect("datagram ok");
            let decoded = zstd::stream::decode_all(datagram.payload().as_ref())
                .expect("independent decode pre-activation");
            assert_eq!(&decoded[..], &expected[..]);
        }

        // 读 dict_offer(内容 4KB+,varint 头 2 字节;read_frames 不做头长假设)
        let mut offer_payload: Option<Bytes> = None;
        frame::read_frames(&mut offer_stream, |door_frame| {
            offer_payload = Some(door_frame);
            async { false }
        })
        .await
        .expect("door-control offer read");
        let offer_frame =
            DoorControlFrame::decode(offer_payload.expect("dict offer frame").as_ref())
                .expect("dict offer decode");
        let (dict_id, dict_content) = match offer_frame.payload.expect("offer payload") {
            DoorControlPayload::DictOffer(offer) => (offer.dictionary_id, offer.content),
            other => panic!("expected dict_offer, got {other:?}"),
        };
        assert!(!dict_id.is_empty() && !dict_content.is_empty());

        // 客户端第 1 条单向流 = door-control 上行:完整收到即回 dict_ready
        let ready_opening = session.open_uni().await.expect("open uni");
        let mut ready_stream = ready_opening.await.expect("uni stream");
        let ready = DoorControlFrame {
            payload: Some(DoorControlPayload::DictReady(DatagramDictReady {
                dictionary_id: dict_id,
            })),
        };
        let mut ready_buf = Vec::with_capacity(ready.encoded_len());
        ready.encode(&mut ready_buf).expect("ready encode");
        frame::write_frame(&mut ready_stream, &ready_buf)
            .await
            .expect("ready write");
        drop(ready_stream); // fin:door-control 上行帧到此为止

        // 激活后:重发同批,datagram 应换字典压缩(独立解码失败、字典解码
        // 成功才算数)。ready 回执与新批之间是回环 UDP,激活先于批到达是
        // 常态,但 movement 任务的 select! 分支次序不作保证——重发最多
        // 10 批,出现字典帧即证激活
        let mut dctx = zstd_safe::DCtx::create();
        let ddict = zstd_safe::DDict::create(&dict_content);
        let batch = Arc::new(MovementBatch {
            chunks: chunks.clone().into(),
        });
        let mut activated = false;
        for _ in 0..10 {
            movement_tx.send_replace(Some(batch.clone()));
            for expected in &chunks {
                let datagram =
                    tokio::time::timeout(Duration::from_secs(5), session.receive_datagram())
                        .await
                        .expect("datagram timeout")
                        .expect("datagram ok");
                let payload = datagram.payload();
                let decoded = if let Ok(independent) = zstd::stream::decode_all(payload.as_ref()) {
                    independent
                } else {
                    let mut out = vec![0u8; expected.len()];
                    let written = dctx
                        .decompress_using_ddict(&mut out, payload.as_ref(), &ddict)
                        .expect("dict decode");
                    activated = true;
                    out[..written].to_vec()
                };
                assert_eq!(&decoded[..], &expected[..]);
            }
            if activated {
                break;
            }
        }
        assert!(activated, "dictionary never activated");
    }
}
