//! 突发流量压测共享模块(quic_burst / wt_burst 两个 example 共用)。
//!
//! 场景:外部带宽受限(压测脚本用令牌桶 UDP 代理模拟,如 100KiB/s),
//! 服务端经 bulk 传输通道下发 10 MiB 低优先级大内容;客户端验证
//! ① bulk 完整性与吞吐(CC 自主探测,端点零带宽假设);
//! ② bulk 饱和期高优 ping RTT 不饿死;
//! ③ bulk 期间门控心跳 datagram 送达率(不敏感语义)。
//!
//! 端点侧(quinn/wtransport 客户端与后端)一律使用通用默认参数,
//! 不做任何按已知带宽的调参。

use std::time::{Duration, Instant};

use prost::Message as _;
use teamviewrelay_rust::proto::teamviewer::v1::{
    WebMapHandshakeRequest, WireChannel, WireEnvelope, wire_envelope,
};

#[allow(dead_code)] // 仅 quic_burst 使用(WT 套经 WT-Available-Protocols 头协商)
pub const ALPN_ZSTD_DICT: &str = "teamviewrelay/v1+zstd-dict";
pub const ZSTD_FRAME_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
pub const HEARTBEAT_MAGIC: &[u8] = b"tvbulk-hb";
pub const MAX_FRAME_LEN: usize = 8 * 1024 * 1024;
/// bulk 到齐的整体超时:100KiB/s 下 10MiB ≈ 105s;跨国极端档
/// (15% 丢 + 300-600ms RTT)重传开销翻倍以上,放宽到 10 分钟。
pub const BULK_TIMEOUT: Duration = Duration::from_secs(600);
/// 心跳 datagram 丢包容忍(比例):弱网档经环境变量放宽。
/// 默认 1%——好路 datagram 损耗应近零;跨国档按线路丢包率 + 裕度设置。
pub fn dgram_loss_tolerance() -> f64 {
    std::env::var("TEAMVIEWER_BURST_DGRAM_LOSS_TOLERANCE")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(0.01)
        .clamp(0.0, 0.9)
}
/// 基线/收尾 RTT 采样窗口。
pub const QUIET_PHASE: Duration = Duration::from_secs(3);

pub fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

// ---------- 线格式 ----------

pub fn encode_varint(mut value: usize, out: &mut [u8; 4]) -> usize {
    for (index, slot) in out.iter_mut().enumerate() {
        if value < 0x80 {
            *slot = value as u8;
            return index + 1;
        }
        *slot = ((value & 0x7F) as u8) | 0x80;
        value >>= 7;
    }
    unreachable!("8 MiB 帧的 varint 至多 4 字节")
}

/// 从字节缓冲头部扫描一帧;不足时返回 None。返回 (payload, 消耗字节数)。
pub fn scan_frame(buffer: &[u8]) -> Option<(Vec<u8>, usize)> {
    let mut length: usize = 0;
    let mut header_len = 0usize;
    loop {
        if header_len >= 4 || header_len >= buffer.len() {
            return None;
        }
        let byte = buffer[header_len];
        length |= usize::from(byte & 0x7F) << (7 * header_len);
        header_len += 1;
        if byte & 0x80 == 0 {
            break;
        }
    }
    if length > MAX_FRAME_LEN || buffer.len() < header_len + length {
        return None;
    }
    let payload = buffer[header_len..header_len + length].to_vec();
    Some((payload, header_len + length))
}

#[allow(deprecated)]
pub fn web_handshake(room: &str) -> Vec<u8> {
    let mut envelope = WireEnvelope::default();
    envelope.channel = WireChannel::WebMap as i32;
    envelope.payload = Some(wire_envelope::Payload::WebMapHandshakeRequest(
        WebMapHandshakeRequest {
            network_protocol_version: "0.8.0".into(),
            minimum_compatible_network_protocol_version: "0.6.1".into(),
            local_program_version: "burst-rust".into(),
            room_code: Some(room.into()),
            accepts_unreliable_positions: None,
            accepts_channels: Vec::new(),
        },
    ));
    envelope.encode_to_vec()
}

pub fn ping_envelope() -> Vec<u8> {
    let mut envelope = WireEnvelope::default();
    envelope.channel = WireChannel::WebMap as i32;
    envelope.payload = Some(wire_envelope::Payload::Ping(
        teamviewrelay_rust::proto::teamviewer::v1::Ping {},
    ));
    envelope.encode_to_vec()
}

// ---------- 确定性内容图案(与 admin bulk-push 生成端一致) ----------

pub fn expected_byte(index: u64, seed: u32) -> u8 {
    (index.wrapping_mul(31).wrapping_add(u64::from(seed)) & 0xFF) as u8
}

/// contentType 形如 `x/...;seed=42`,解析出 seed。
pub fn parse_seed(content_type: &str) -> Option<u32> {
    content_type
        .split(';')
        .find_map(|part| part.trim().strip_prefix("seed="))
        .and_then(|seed| seed.trim().parse().ok())
}

// ---------- 统计 ----------

pub fn percentile(samples: &[f64], p: f64) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("finite rtt"));
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[index]
}

// ---------- 持续 zstd 解压器(与 quic_smoke / 后端 StreamDecoder 同型) ----------

pub struct ZstdContinuousDecoder {
    ctx: zstd_safe::DCtx<'static>,
    pending: Vec<u8>,
}

impl ZstdContinuousDecoder {
    pub fn new() -> anyhow::Result<Self> {
        let mut ctx = zstd_safe::DCtx::try_create()
            .ok_or_else(|| anyhow::anyhow!("zstd DCtx 创建失败"))?;
        ctx.set_parameter(zstd_safe::DParameter::WindowLogMax(23))
            .map_err(|code| anyhow::anyhow!("zstd 窗口上限设置失败:{code:?}"))?;
        Ok(Self {
            ctx,
            pending: Vec::new(),
        })
    }

    pub fn drain(&mut self, chunk: &[u8]) -> anyhow::Result<Vec<u8>> {
        self.pending.extend_from_slice(chunk);
        let mut collected = Vec::new();
        let mut input_pos = 0usize;
        let mut window = [0u8; 64 * 1024];
        loop {
            let mut in_buffer = zstd_safe::InBuffer::around(&self.pending[input_pos..]);
            let mut out_buffer = zstd_safe::OutBuffer::around(&mut window);
            self.ctx
                .decompress_stream(&mut out_buffer, &mut in_buffer)
                .map_err(|code| anyhow::anyhow!("zstd 流解码失败:{code:?}"))?;
            input_pos += in_buffer.pos();
            let produced = out_buffer.as_slice().len();
            collected.extend_from_slice(out_buffer.as_slice());
            if collected.len() > 8 * 1024 * 1024 {
                anyhow::bail!("zstd 流解压超过 8 MiB 单块上限");
            }
            if input_pos >= self.pending.len() && produced == 0 {
                break;
            }
        }
        self.pending.drain(..input_pos);
        Ok(collected)
    }
}

// ---------- RTT 采样与判定报告 ----------

#[derive(Default)]
#[allow(dead_code)] // post 样本积累备用,判定只用 baseline/during
pub struct RttSamples {
    pub baseline: Vec<f64>,
    pub during: Vec<f64>,
    pub post: Vec<f64>,
}

pub struct PhaseWindow {
    pub started: Option<Instant>,
    pub ended: Option<Instant>,
}

impl PhaseWindow {
    pub fn record(&self, at: Instant, rtt_ms: f64, samples: &mut RttSamples) {
        match (self.started, self.ended) {
            (Some(start), Some(end)) if at >= start && at <= end => {
                samples.during.push(rtt_ms)
            }
            (Some(start), None) if at >= start => samples.during.push(rtt_ms),
            _ => samples.baseline.push(rtt_ms),
        }
    }
}

#[derive(serde::Serialize)]
pub struct BurstVerdict {
    pub door: String,
    pub suite: String,
    pub bulk_bytes: u64,
    pub bulk_seconds: f64,
    pub bulk_kib_per_s: f64,
    pub integrity_ok: bool,
    pub rtt_baseline_p50_ms: f64,
    pub rtt_baseline_p95_ms: f64,
    pub rtt_during_p50_ms: f64,
    pub rtt_during_p95_ms: f64,
    pub rtt_during_baseline_ratio_p95: f64,
    pub datagram_expected: u64,
    pub datagram_received: u64,
    pub datagram_delivery_ratio: f64,
    pub datagram_other: u64,
    pub verdict: &'static str,
}

pub fn print_verdict(verdict: &BurstVerdict) {
    let json = serde_json::to_string(verdict).expect("verdict json");
    println!("VERDICT_JSON {json}");
    println!(
        "bulk: {} bytes in {:.1}s = {:.1} KiB/s, integrity={}",
        verdict.bulk_bytes, verdict.bulk_seconds, verdict.bulk_kib_per_s, verdict.integrity_ok
    );
    println!(
        "rtt p95: baseline {:.0}ms -> during {:.0}ms (x{:.2})",
        verdict.rtt_baseline_p95_ms, verdict.rtt_during_p95_ms,
        verdict.rtt_during_baseline_ratio_p95
    );
    println!(
        "datagram(heartbeat): {}/{} = {:.1}%(other={})",
        verdict.datagram_received,
        verdict.datagram_expected,
        verdict.datagram_delivery_ratio * 100.0,
        verdict.datagram_other
    );
    println!("verdict: {}", verdict.verdict);
}

// ---------- 流角色分派(README"bulk 传输通道"接收端规则) ----------

/// 门控通告与 bulk 流几乎同时实体化,客户端 accept 的先后序不保证。
/// 读一个 varint 前缀组(门控帧的 `[varint][DoorControlFrame]` 与 bulk 流
/// 的 `[varint][transfer_id]` 前缀同构),由调用方按内容分派:能解成
/// `DoorControlFrame` 的是门控流;载荷形如 `b` + hex 的是 bulk 自标识。
pub fn is_bulk_id(payload: &[u8]) -> bool {
    payload.len() >= 3
        && payload.len() <= 64
        && payload[0] == b'b'
        && payload[1..].iter().all(|b| b.is_ascii_hexdigit())
}
