//! 突发流量压测(裸 QUIC 门):100KiB/s 级受限带宽下接收 10 MiB bulk
//! 低优先级内容,同步验证高优 ping RTT 与门控心跳 datagram 送达率。
//!
//! 配套:scripts/quic_burst_test.sh(起后端 + 令牌桶 UDP 代理 + 触发
//! /admin/api/debug/bulk-push)。端点零带宽假设:quinn 客户端全部默认
//! 参数,拥塞控制自主探测。
//!
//! 环境:TEAMVIEWER_BURST_SERVER(默认 127.0.0.1:8767)、
//! TEAMVIEWER_BURST_ROOM(默认 burst-room)。

#![allow(clippy::too_many_arguments)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use prost::Message as _;
use quinn::crypto::rustls::QuicClientConfig;
use quinn::rustls as rustls;
use quinn::rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
    DigitallySignedStruct, SignatureScheme,
};
use tokio::sync::Mutex as AsyncMutex;

#[path = "burst_common/mod.rs"]
mod burst_common;
use burst_common::*;

use teamviewrelay_rust::proto::teamviewer::door::v1::{
    DoorControlFrame, door_control_frame::Payload as DoorControlPayload,
};
use teamviewrelay_rust::proto::teamviewer::v1::{WireEnvelope, wire_envelope};

type Anyhow = Result<()>;

#[derive(Default)]
struct HbStats {
    received: u64,
    max_seq: u32,
    other: u64,
}

#[tokio::main]
async fn main() -> Anyhow {
    let server = env_or("TEAMVIEWER_BURST_SERVER", "127.0.0.1:8767");
    let room = env_or("TEAMVIEWER_BURST_ROOM", "burst-room");

    let outcome = tokio::time::timeout(BULK_TIMEOUT + Duration::from_secs(60), async {
        run(&server, &room).await
    })
    .await;
    match outcome {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(_) => bail!("burst 压测整体超时"),
    }
}

async fn run(server: &str, room: &str) -> Anyhow {
    let started = Instant::now();
    let connection = connect(server).await?;
    verify_alpn(&connection, ALPN_ZSTD_DICT)?;

    // 应用上行:握手后交 ping 任务
    let (mut uplink, _uplink_recv) = connection.open_bi().await?;
    write_frame(&mut uplink, &web_handshake(room)).await?;

    // 应用下行(zstd-dict 套 = 连续 zstd 流):ack 之后 pong 读取也在这条流
    let mut state_stream = connection.accept_uni().await?;
    let first_chunk = read_frame(&mut state_stream).await?;
    if first_chunk.len() < 4 || first_chunk[..4] != ZSTD_FRAME_MAGIC {
        bail!("+zstd-dict 套下行首块必须以 zstd 帧魔数开头");
    }
    let mut decoder = ZstdContinuousDecoder::new()?;
    let ack = decoder.drain(&first_chunk)?;
    let ack_envelope = WireEnvelope::decode(ack.as_slice())?;
    assert_payload(&ack_envelope, "handshake_ack")?;

    // RTT 采样:pong 读取(下行任务)+ ping 发送(独占上行任务)
    let outstanding = Arc::new(AsyncMutex::<Option<Instant>>::new(None));
    let rtt_log = Arc::new(Mutex::<Vec<(Instant, f64)>>::new(Vec::new()));
    let pong_state = Arc::clone(&outstanding);
    let pong_log = Arc::clone(&rtt_log);
    let pong_task = tokio::spawn(async move {
        loop {
            let Ok(chunk) = read_frame(&mut state_stream).await else {
                return;
            };
            let Ok(decoded) = decoder.drain(&chunk) else {
                return;
            };
            let Ok(envelope) = WireEnvelope::decode(decoded.as_slice()) else {
                continue;
            };
            if matches!(
                envelope.payload,
                Some(wire_envelope::Payload::Pong(_))
            ) {
                if let Some(sent) = pong_state.lock().await.take() {
                    pong_log
                        .lock()
                        .expect("rtt log")
                        .push((Instant::now(), sent.elapsed().as_secs_f64() * 1000.0));
                }
            }
        }
    });
    let ping_state = Arc::clone(&outstanding);
    let ping_uplink = uplink;
    let ping_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ping_stop_task = Arc::clone(&ping_stop);
    let ping_task = tokio::spawn(async move {
        let mut uplink = ping_uplink;
        let mut interval = tokio::time::interval(Duration::from_millis(300));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if ping_stop_task.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            {
                let mut guard = ping_state.lock().await;
                if guard.is_some() {
                    continue; // 上一发未回,跳过(保持至多一发在途)
                }
                if write_frame(&mut uplink, &ping_envelope()).await.is_err() {
                    return;
                }
                *guard = Some(Instant::now());
            }
        }
    });

    // 心跳 datagram 计数:统计写入共享状态,任务被 abort 也不丢数
    let counter_connection = connection.clone();
    let hb_stats = Arc::new(Mutex::new(HbStats::default()));
    let hb_task = {
        let hb_stats = Arc::clone(&hb_stats);
        tokio::spawn(async move {
            loop {
                match counter_connection.read_datagram().await {
                    Ok(datagram) => {
                        let mut stats = hb_stats.lock().expect("hb stats");
                        if datagram.starts_with(HEARTBEAT_MAGIC) && datagram.len() >= 13 {
                            stats.received += 1;
                            let seq = u32::from_be_bytes([
                                datagram[9], datagram[10], datagram[11], datagram[12],
                            ]);
                            stats.max_seq = stats.max_seq.max(seq);
                        } else {
                            stats.other += 1;
                        }
                    }
                    Err(_) => return,
                }
            }
        })
    };

    // READY + 基线相位先走;通告与 bulk 流几乎同时实体化,accept 先后序
    // 不保证,按内容分派(README 接收端规则),同 wt_burst
    println!("READY 握手完成,基线采样 {QUIET_PHASE:?} ...");
    tokio::time::sleep(QUIET_PHASE).await;

    let mut pending_bulk: Option<(String, quinn::RecvStream)> = None;
    let announce = loop {
        let stream = connection.accept_uni().await?;
        let (payload, stream) = match read_varint_group(stream).await {
            Ok(group) => group,
            Err(error) => bail!("door/bulk 流首组读取失败:{error}"),
        };
        if let Ok(decoded) = DoorControlFrame::decode(payload.as_slice()) {
            match decoded.payload {
                Some(DoorControlPayload::BulkTransferStart(start)) => break start,
                Some(_) => continue,
                None => bail!("door-control 下行帧无载荷(协议违规)"),
            }
        }
        if is_bulk_id(&payload) {
            let id = String::from_utf8_lossy(&payload).into_owned();
            println!("  bulk 流先于通告到达,id={id},缓存待匹配");
            pending_bulk = Some((id, stream));
            continue;
        }
        bail!("未知流首组负载:{:02x?}", &payload[..payload.len().min(32)]);
    };
    let seed = parse_seed(&announce.content_type)
        .ok_or_else(|| anyhow::anyhow!("contentType 缺 seed,完整性核对不可行"))?;

    println!(
        "bulk 通告已收 transfer_id={} total_len={} seed={seed},等待数据...",
        announce.transfer_id, announce.total_len
    );

    // bulk 流:优先用先到缓存的流;否则收下一条并核对自标识
    let mut bulk_stream = match pending_bulk {
        Some((id, stream)) => {
            if id != announce.transfer_id {
                bail!("缓存的 bulk 流自标识不匹配:{id} != {}", announce.transfer_id);
            }
            stream
        }
        None => {
            let mut stream = connection.accept_uni().await?;
            let stream_id = read_stream_id(&mut stream).await?;
            if stream_id != announce.transfer_id {
                bail!("bulk 流自标识不匹配:{stream_id} != {}", announce.transfer_id);
            }
            stream
        }
    };
    let read_start = Instant::now();
    let mut received: u64 = 0;
    let mut last_mib_mark: u64 = 0;
    let mut buffer = vec![0u8; 64 * 1024];
    while received < announce.total_len {
        let want = (announce.total_len - received).min(buffer.len() as u64) as usize;
        bulk_stream.read_exact(&mut buffer[..want]).await?;
        for (index, byte) in buffer[..want].iter().enumerate() {
            if *byte != expected_byte(received + index as u64, seed) {
                bail!("bulk 内容图案失配 @{}(完整性破坏)", received + index as u64);
            }
        }
        received += want as u64;
        if received / (1024 * 1024) > last_mib_mark {
            last_mib_mark = received / (1024 * 1024);
            println!(
                "  bulk {}/{} MiB, {:+.0}s",
                last_mib_mark,
                announce.total_len / (1024 * 1024),
                read_start.elapsed().as_secs_f64()
            );
        }
    }
    let bulk_seconds = read_start.elapsed().as_secs_f64();

    // 收尾相位 + 停采样
    // 收尾相位(RTT post 样本仍会积累,但判定只用 baseline/during)
    tokio::time::sleep(QUIET_PHASE).await;
    ping_stop.store(true, std::sync::atomic::Ordering::Relaxed);
    ping_task.abort();
    pong_task.abort();
    hb_task.abort();
    let (hb_received, hb_max_seq, hb_other) = {
        let stats = hb_stats.lock().expect("hb stats");
        (stats.received, stats.max_seq, stats.other)
    };
    // 服务端心跳按 100ms 周期编号,送达率按已发出的最大序号计
    // (周期抖动会让"时长/周期"高估发送量);max_seq 空洞即真实丢失
    let expected = if hb_max_seq == u32::MAX { 0 } else { hb_max_seq as u64 + 1 };
    let samples = rtt_log.lock().expect("rtt log").clone();

    let window = PhaseWindow {
        started: Some(read_start),
        ended: Some(read_start + Duration::from_secs_f64(bulk_seconds)),
    };
    let mut rtt = RttSamples::default();
    for (at, rtt_ms) in samples {
        window.record(at, rtt_ms, &mut rtt);
    }
    let baseline_p95 = percentile(&rtt.baseline, 0.95);
    let during_p95 = percentile(&rtt.during, 0.95);
    let delivery_ratio = if expected == 0 {
        1.0
    } else {
        (hb_received as f64 / expected as f64).min(1.0)
    };
    let hb_lost_by_gap = (hb_max_seq as u64 + 1).saturating_sub(hb_received);
    let integrity_ok = true; // 图案逐字节核对通过才会走到这里
    let ratio = if baseline_p95 > 0.0 { during_p95 / baseline_p95 } else { 1.0 };
    let tolerance = dgram_loss_tolerance();
    let lost_ratio = if expected > 0 { hb_lost_by_gap as f64 / expected as f64 } else { 0.0 };
    let verdict_ok = integrity_ok && lost_ratio <= tolerance;
    let verdict = BurstVerdict {
        door: "quic".into(),
        suite: ALPN_ZSTD_DICT.into(),
        bulk_bytes: announce.total_len,
        bulk_seconds,
        bulk_kib_per_s: announce.total_len as f64 / 1024.0 / bulk_seconds,
        integrity_ok,
        rtt_baseline_p50_ms: percentile(&rtt.baseline, 0.5),
        rtt_baseline_p95_ms: baseline_p95,
        rtt_during_p50_ms: percentile(&rtt.during, 0.5),
        rtt_during_p95_ms: during_p95,
        rtt_during_baseline_ratio_p95: ratio,
        datagram_expected: expected,
        datagram_received: hb_received,
        datagram_delivery_ratio: delivery_ratio,
        datagram_other: hb_other,
        verdict: if verdict_ok { "PASS" } else { "FAIL" },
    };
    print_verdict(&verdict);
    println!(
        "总耗时 {:.1}s,心跳序号空洞(丢失上界){hb_lost_by_gap},max_seq={hb_max_seq}",
        started.elapsed().as_secs_f64()
    );
    if verdict_ok {
        Ok(())
    } else {
        bail!("burst 判定 FAIL");
    }
}

// ---------- 线格式与连接 ----------

async fn connect(server: &str) -> Result<quinn::Connection> {
    let addr = server
        .parse()
        .map_err(|error| anyhow::anyhow!("TEAMVIEWER_BURST_SERVER 解析失败:{error}"))?;
    let mut tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(InsecureVerifier))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN_ZSTD_DICT.as_bytes().to_vec()];
    let quic_tls = QuicClientConfig::try_from(tls)
        .map_err(|error| anyhow::anyhow!("rustls → QUIC TLS 适配失败:{error}"))?;
    let mut config = quinn::ClientConfig::new(Arc::new(quic_tls));
    config.transport_config(Arc::new(quinn::TransportConfig::default()));
    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(config);
    Ok(endpoint.connect(addr, "localhost")?.await?)
}

fn verify_alpn(connection: &quinn::Connection, expected: &str) -> Anyhow {
    let data = connection
        .handshake_data()
        .ok_or_else(|| anyhow::anyhow!("缺握手数据"))?
        .downcast::<quinn::crypto::rustls::HandshakeData>()
        .map_err(|_| anyhow::anyhow!("握手数据类型不符"))?;
    let negotiated = data
        .protocol
        .as_ref()
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .unwrap_or_default();
    if negotiated != expected {
        bail!("服务端必须回显客户端选的 ALPN:{negotiated} != {expected}");
    }
    Ok(())
}

/// 读一个 varint 前缀组:`[varint len][len 字节]`。返回 (payload, 流)。
async fn read_varint_group(mut stream: quinn::RecvStream) -> Result<(Vec<u8>, quinn::RecvStream)> {
    let mut length: usize = 0;
    let mut header_len = 0usize;
    loop {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).await?;
        if header_len >= 4 {
            bail!("varint 长度头超 4 字节");
        }
        length |= usize::from(byte[0] & 0x7F) << (7 * header_len);
        header_len += 1;
        if byte[0] & 0x80 == 0 {
            break;
        }
    }
    if length > MAX_FRAME_LEN {
        bail!("varint 长度越限:{length}");
    }
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload).await?;
    Ok((payload, stream))
}

/// bulk 流自标识前缀:`[varint len][transfer_id UTF-8]`。
async fn read_stream_id(stream: &mut quinn::RecvStream) -> Result<String> {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).await?;
    let len = usize::from(header[0]);
    if len == 0 || len > 64 {
        bail!("bulk 流自标识长度违规:{len}");
    }
    let mut id = vec![0u8; len];
    stream.read_exact(&mut id).await?;
    String::from_utf8(id).map_err(|error| anyhow::anyhow!("transfer_id 非 UTF-8:{error}"))
}

async fn write_frame(stream: &mut quinn::SendStream, payload: &[u8]) -> Result<()> {
    let mut header = [0u8; 4];
    let header_len = encode_varint(payload.len(), &mut header);
    stream.write_all(&header[..header_len]).await?;
    stream.write_all(payload).await?;
    Ok(())
}

async fn read_frame(stream: &mut quinn::RecvStream) -> Result<Vec<u8>> {
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        if let Some((payload, consumed)) = scan_frame(&buffer) {
            buffer.drain(..consumed);
            return Ok(payload);
        }
        if buffer.len() > MAX_FRAME_LEN + 8 {
            bail!("下行帧缓冲超限");
        }
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).await?;
        buffer.push(byte[0]);
    }
}

fn assert_payload(envelope: &WireEnvelope, expected: &str) -> Result<()> {
    let actual = envelope
        .payload
        .as_ref()
        .map(|payload| match payload {
            wire_envelope::Payload::HandshakeAck(_) => "handshake_ack",
            wire_envelope::Payload::SnapshotFull(_) => "snapshot_full",
            wire_envelope::Payload::Pong(_) => "pong",
            wire_envelope::Payload::Ping(_) => "ping",
            _ => "other",
        })
        .unwrap_or("none");
    if actual != expected {
        bail!("期望 {expected} 实得 {actual}");
    }
    Ok(())
}

/// 与后端测试同款的"信任一切"客户端(自签冒烟环境)。
#[derive(Debug)]
struct InsecureVerifier;

impl ServerCertVerifier for InsecureVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ED25519,
        ]
    }
}
