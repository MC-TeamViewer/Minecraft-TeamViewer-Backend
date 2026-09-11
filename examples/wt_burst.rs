//! 突发流量压测(WebTransport 门):与 quic_burst 同一测量语义,走
//! wtransport 客户端打 WT 门。证书按 TEAMVIEWER_BURST_CERT_SHA256
//! (DER 的 SHA-256 hex,由压测脚本计算)钉扎,等价浏览器
//! serverCertificateHashes 语义。压缩套经 URL query `?suite=` 协商
//! +zstd-dict(浏览器脚本同款语义)。
//!
//! 环境:TEAMVIEWER_BURST_SERVER(默认 127.0.0.1:8766)、
//! TEAMVIEWER_BURST_CERT_SHA256(必填)、TEAMVIEWER_BURST_ROOM。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use prost::Message as _;
use tokio::sync::Mutex as AsyncMutex;
use wtransport::ClientConfig;
use wtransport::tls::Sha256Digest;

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
    let server = env_or("TEAMVIEWER_BURST_SERVER", "127.0.0.1:8766");
    let room = env_or("TEAMVIEWER_BURST_ROOM", "burst-room");
    let cert_sha = std::env::var("TEAMVIEWER_BURST_CERT_SHA256")
        .map_err(|_| anyhow::anyhow!("TEAMVIEWER_BURST_CERT_SHA256 未设置"))?;

    let outcome = tokio::time::timeout(BULK_TIMEOUT + Duration::from_secs(60), async {
        run(&server, &room, &cert_sha).await
    })
    .await;
    match outcome {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(_) => bail!("burst 压测整体超时"),
    }
}

async fn run(server: &str, room: &str, cert_sha: &str) -> Anyhow {
    let started = Instant::now();
    let session = connect(server, cert_sha).await?;

    // 应用上行:握手后交 ping 任务
    let (mut uplink, _uplink_recv) = session.open_bi().await?.await?;
    write_frame(&mut uplink, &web_handshake(room)).await?;

    // 应用下行(zstd-dict 套 = 连续 zstd 流)
    let mut state_stream = session.accept_uni().await?;
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
            if matches!(envelope.payload, Some(wire_envelope::Payload::Pong(_)))
                && let Some(sent) = pong_state.lock().await.take()
            {
                pong_log
                    .lock()
                    .expect("rtt log")
                    .push((Instant::now(), sent.elapsed().as_secs_f64() * 1000.0));
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
                    continue;
                }
                if write_frame(&mut uplink, &ping_envelope()).await.is_err() {
                    return;
                }
                *guard = Some(Instant::now());
            }
        }
    });

    // 心跳 datagram 计数:统计写入共享状态,任务被 abort 也不丢数
    let counter_session = session.clone();
    let hb_stats = Arc::new(Mutex::new(HbStats::default()));
    let hb_task = {
        let hb_stats = Arc::clone(&hb_stats);
        tokio::spawn(async move {
            loop {
                match counter_session.receive_datagram().await {
                    Ok(datagram) => {
                        let payload = datagram.payload();
                        let mut stats = hb_stats.lock().expect("hb stats");
                        if payload.starts_with(HEARTBEAT_MAGIC) && payload.len() >= 13 {
                            stats.received += 1;
                            let seq = u32::from_be_bytes([
                                payload[9], payload[10], payload[11], payload[12],
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

    // 门控下行流按需实体化(首帧写出才过线):READY + 基线相位必须先走
    println!("READY 握手完成,基线采样 {QUIET_PHASE:?} ...");
    tokio::time::sleep(QUIET_PHASE).await;

    // 门控通告与 bulk 流几乎同时实体化,accept 先后序不保证:按内容分派
    // (README 接收端规则)——能解出门控帧的是门控流;载荷形如 transfer_id
    // 的是先到的 bulk 流,缓存备用,其数据从当前读取位置无缝续读
    let mut pending_bulk: Option<(String, wtransport::RecvStream)> = None;
    let announce = loop {
        let stream = session.accept_uni().await?;
        let (payload, stream) = match read_varint_group(stream).await {
            Ok(group) => group,
            Err(error) => bail!("door/bulk 流首组读取失败:{error}"),
        };
        if let Ok(decoded) = DoorControlFrame::decode(payload.as_slice()) {
            match decoded.payload {
                Some(DoorControlPayload::BulkTransferStart(start)) => break start,
                // dict_offer 等其他门控帧:容忍,继续收
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
    let read_start = Instant::now();
    let mut bulk_stream = match pending_bulk {
        Some((id, stream)) => {
            if id != announce.transfer_id {
                bail!("缓存的 bulk 流自标识不匹配:{id} != {}", announce.transfer_id);
            }
            stream
        }
        None => {
            let mut stream = session.accept_uni().await?;
            let stream_id = read_stream_id(&mut stream).await?;
            if stream_id != announce.transfer_id {
                bail!("bulk 流自标识不匹配:{stream_id} != {}", announce.transfer_id);
            }
            stream
        }
    };
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
    let ratio = if baseline_p95 > 0.0 { during_p95 / baseline_p95 } else { 1.0 };
    let integrity_ok = true; // 图案逐字节核对通过才会走到这里
    let tolerance = dgram_loss_tolerance();
    let lost_ratio = if expected > 0 { hb_lost_by_gap as f64 / expected as f64 } else { 0.0 };
    let verdict_ok = integrity_ok && lost_ratio <= tolerance;
    let verdict = BurstVerdict {
        door: "webtransport".into(),
        suite: "wt-zstd-dict-header".into(),
        bulk_bytes: announce.total_len,
        bulk_seconds,
        bulk_kib_per_s: announce.total_len as f64 / 1024.0 / bulk_seconds,
        integrity_ok: true,
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

// ---------- 连接与线格式 ----------

async fn connect(server: &str, cert_sha: &str) -> Result<wtransport::Connection> {
    let (host, port) = server
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("TEAMVIEWER_BURST_SERVER 解析失败"))?;
    let hash_bytes: [u8; 32] = (0..32)
        .map(|index| {
            u8::from_str_radix(
                &cert_sha[index * 2..index * 2 + 2],
                16,
            )
            .map_err(|error| anyhow::anyhow!("证书哈希 hex 解析失败:{error}"))
        })
        .collect::<Result<Vec<_>>>()?
        .try_into()
        .map_err(|_| anyhow::anyhow!("证书哈希长度必须为 64 hex"))?;
    let config = ClientConfig::builder()
        .with_bind_default()
        .with_server_certificate_hashes([Sha256Digest::from(hash_bytes)])
        .build();
    let endpoint = wtransport::Endpoint::client(config)?;
    // 压缩套协商走 URL query(服务端 query 唯一权威;WT-Protocol 响应头
    // 回显仅前向兼容,请求头不参与决策)
    let url = format!("https://{host}:{port}/web-map/wt?suite=zstd-dict");
    let connection = endpoint
        .connect(wtransport::endpoint::ConnectOptions::builder(&url))
        .await?;
    Ok(connection)
}

/// 读一个 varint 前缀组:`[varint len][len 字节]`。返回 (payload, 流)。
async fn read_varint_group(
    mut stream: wtransport::RecvStream,
) -> Result<(Vec<u8>, wtransport::RecvStream)> {
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

async fn read_stream_id(stream: &mut wtransport::RecvStream) -> Result<String> {
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

async fn write_frame(stream: &mut wtransport::SendStream, payload: &[u8]) -> Result<()> {
    let mut header = [0u8; 4];
    let header_len = encode_varint(payload.len(), &mut header);
    stream.write_all(&header[..header_len]).await?;
    stream.write_all(payload).await?;
    Ok(())
}

async fn read_frame(stream: &mut wtransport::RecvStream) -> Result<Vec<u8>> {
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
            wire_envelope::Payload::Pong(_) => "pong",
            _ => "other",
        })
        .unwrap_or("none");
    if actual != expected {
        bail!("期望 {expected} 实得 {actual}");
    }
    Ok(())
}
