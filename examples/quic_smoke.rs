//! Phase 4 端到端冒烟:对本地起着的 QUIC 门(默认 127.0.0.1:8767)做黑盒验证。
//!
//! 覆盖两条 ALPN 路径,均走 WebMap 通道(握手 → resync → snapshot_full):
//!   1. `teamviewrelay/v1+zstd`:上行恒 plain varint 分帧;下行 uni 流
//!      `[varint][zstd 压缩块]`,持久解压器跨块连续解码;首块必须以 zstd
//!      帧魔数开头,证明下行真被压缩。
//!   2. `teamviewrelay/v1` plain:下行帧即裸 envelope,不出现 zstd 魔数。
//!
//! 运行(后端已监听时):
//!   cargo run --example quic-smoke -j 2
//!
//! TLS 侧与 mod `allowInsecureTls` 同语义:信任任意服务端证书(自签冒烟环境)。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use prost::Message as _;
use quinn::rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
    ClientConfig, DigitallySignedStruct, SignatureScheme,
};
use quinn::rustls as rustls;
use teamviewrelay_rust::proto::teamviewer::v1::{
    wire_envelope, ResyncRequest, WebMapHandshakeRequest, WireChannel, WireEnvelope,
};

const ALPN_PLAIN: &str = "teamviewrelay/v1";
const ALPN_ZSTD: &str = "teamviewrelay/v1+zstd";
const ZSTD_FRAME_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
const SERVER_ADDR: &str = "127.0.0.1:8767";
/// 与 mod/脚本一致的解压窗口上限(2^23)。
const MAX_DECOMPRESSION_WINDOW_LOG: u32 = 23;

/// 目标门地址,默认本机;弱网代理/netem 场景用环境变量改指代理端口。
fn server_addr() -> std::net::SocketAddr {
    std::env::var("TEAMVIEWER_QUIC_SMOKE_SERVER")
        .unwrap_or_else(|_| SERVER_ADDR.to_string())
        .parse()
        .expect("TEAMVIEWER_QUIC_SMOKE_SERVER 必须是合法的 host:port")
}

#[tokio::main]
async fn main() -> Result<()> {
    let zstd = run_case(ALPN_ZSTD, true, "quic-smoke-zstd").await;
    let plain = run_case(ALPN_PLAIN, false, "quic-smoke-plain").await;
    zstd?;
    plain?;
    println!("QUIC 门冒烟矩阵全部通过(+zstd 与 plain 两条 ALPN 路径)");
    Ok(())
}

async fn run_case(alpn: &str, expect_zstd: bool, room: &str) -> Result<()> {
    let outcome = tokio::time::timeout(Duration::from_secs(20), async {
        // (内部 Result:块内任何一步失败都必须如实向上传播)
        let connection = connect(alpn).await?;
        let handshake = connection
            .handshake_data()
            .ok_or_else(|| anyhow!("缺握手数据"))?
            .downcast::<quinn::crypto::rustls::HandshakeData>()
            .map_err(|_| anyhow!("握手数据类型不符"))?;
        let negotiated = handshake
            .protocol
            .as_ref()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .unwrap_or_default();
        if negotiated != alpn {
            bail!("服务端必须回显客户端选的 ALPN:{negotiated} != {alpn}");
        }

        // 上行:客户端第 1 条双向流,plain varint 分帧(zstd 套只压下行)。
        let (mut uplink, _uplink_recv) = connection.open_bi().await?;
        write_frame(&mut uplink, &web_handshake(room)).await?;
        println!("  [{alpn}] 上行握手已发(plain varint 分帧)");

        // 下行:服务端第 1 条单向流。
        let mut downlink = connection.accept_uni().await?;
        let first_chunk = read_frame(&mut downlink).await?;

        // 持续 zstd 解压器(+zstd 套):跨块连续解码,与 mod ZstdStreamDecoder 同型。
        let mut zstd_decoder = if expect_zstd {
            if first_chunk.len() < 4 || first_chunk[..4] != ZSTD_FRAME_MAGIC {
                bail!("+zstd 套下行首块必须以 zstd 帧魔数开头(证明真被压缩)");
            }
            Some(ZstdContinuousDecoder::new()?)
        } else {
            if first_chunk.len() >= 4 && first_chunk[..4] == ZSTD_FRAME_MAGIC {
                bail!("plain 套下行不得出现 zstd 帧");
            }
            None
        };

        let ack = decode_envelope(zstd_decoder.as_mut(), &first_chunk)?;
        assert_payload(&ack, "handshake_ack")?;
        println!(
            "  [{alpn}] 握手 ack 通(下行{})",
            if expect_zstd { "zstd 连续流解码" } else { "裸 envelope" }
        );

        write_frame(&mut uplink, &resync_request()).await?;
        loop {
            let chunk = read_frame(&mut downlink).await?;
            let envelope = decode_envelope(zstd_decoder.as_mut(), &chunk)?;
            let is_snapshot = envelope
                .payload
                .as_ref()
                .is_some_and(|payload| matches!(payload, wire_envelope::Payload::SnapshotFull(_)));
            if is_snapshot {
                println!("  [{alpn}] plain 上行 resync → snapshot_full 下行通");
                break;
            }
        }
        Ok::<(), anyhow::Error>(())
    })
    .await;
    // timeout 产出嵌套 Result:外层 = 超时,内层 = 用例自身成败。
    match outcome {
        Ok(Ok(())) => {
            println!("{alpn}: PASS");
            Ok(())
        }
        Ok(Err(error)) => Err(error),
        Err(_) => Err(anyhow!("用例 {alpn} 超时(20s)")),
    }
}

/// 与后端测试同款的"信任一切"客户端:自签冒烟环境,语义同 mod allowInsecureTls。
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

async fn connect(alpn: &str) -> Result<quinn::Connection> {
    let mut tls = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(InsecureVerifier))
        .with_no_client_auth();
    tls.alpn_protocols = vec![alpn.as_bytes().to_vec()];
    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    let mut config = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(tls)
            .context("rustls 配置缺初始 cipher suite")?,
    ));
    config.transport_config(Arc::new(quinn::TransportConfig::default()));
    endpoint.set_default_client_config(config);
    let connection = endpoint
        .connect(server_addr(), "127.0.0.1")?
        .await?;
    Ok(connection)
}

#[allow(deprecated)]
fn web_handshake(room: &str) -> Vec<u8> {
    let mut envelope = WireEnvelope::default();
    envelope.channel = WireChannel::WebMap as i32;
    envelope.payload = Some(wire_envelope::Payload::WebMapHandshakeRequest(
        WebMapHandshakeRequest {
            network_protocol_version: "0.8.0".into(),
            minimum_compatible_network_protocol_version: "0.6.1".into(),
            local_program_version: "quic-smoke-rust".into(),
            room_code: Some(room.into()),
            accepts_unreliable_positions: None,
            accepts_channels: Vec::new(),
        },
    ));
    envelope.encode_to_vec()
}

fn resync_request() -> Vec<u8> {
    let mut envelope = WireEnvelope::default();
    envelope.channel = WireChannel::WebMap as i32;
    envelope.payload = Some(wire_envelope::Payload::ResyncRequest(ResyncRequest::default()));
    envelope.encode_to_vec()
}

async fn write_frame(stream: &mut quinn::SendStream, payload: &[u8]) -> Result<()> {
    let mut header = [0u8; 4];
    let header_len = encode_varint(payload.len(), &mut header);
    stream.write_all(&header[..header_len]).await?;
    stream.write_all(payload).await?;
    Ok(())
}

async fn read_frame(stream: &mut quinn::RecvStream) -> Result<Vec<u8>> {
    let mut length: usize = 0;
    let mut shift = 0u32;
    loop {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).await?;
        length |= usize::from(byte[0] & 0x7F) << shift;
        if byte[0] & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift >= 32 || length > 8 * 1024 * 1024 {
            bail!("下行 varint 长度违规:{length}");
        }
    }
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

fn encode_varint(mut value: usize, out: &mut [u8; 4]) -> usize {
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

/// 持续 zstd 解压器:跨块连续解码,与后端 compress.rs StreamDecoder 同型
/// (zstd-safe 流式状态机;不用 zstd-rs 的 Read 包装——其内部读到空输入会
/// 永久进入 PastEof 状态,不再消费后续数据)。
struct ZstdContinuousDecoder {
    ctx: zstd_safe::DCtx<'static>,
    pending: Vec<u8>,
}

impl ZstdContinuousDecoder {
    fn new() -> Result<Self> {
        let mut ctx = zstd_safe::DCtx::try_create()
            .ok_or_else(|| anyhow!("zstd DCtx 创建失败"))?;
        ctx.set_parameter(zstd_safe::DParameter::WindowLogMax(
            MAX_DECOMPRESSION_WINDOW_LOG,
        ))
        .map_err(|code| anyhow!("zstd 窗口上限设置失败:{code:?}"))?;
        Ok(Self { ctx, pending: Vec::new() })
    }

    fn drain(&mut self, chunk: &[u8]) -> Result<Vec<u8>> {
        // 与后端 compress.rs StreamDecoder 同型:窗口用定长数组(避免
        // zstd-safe 对 Vec 载体的 filled_until 改长语义),终止条件为
        // "输入耗尽且本轮零产出"。跨块连续解码,不依赖块边界。
        self.pending.extend_from_slice(chunk);
        let mut collected = Vec::new();
        let mut input_pos = 0usize;
        let mut window = [0u8; 64 * 1024];
        loop {
            let mut in_buffer = zstd_safe::InBuffer::around(&self.pending[input_pos..]);
            let mut out_buffer = zstd_safe::OutBuffer::around(&mut window);
            self.ctx
                .decompress_stream(&mut out_buffer, &mut in_buffer)
                .map_err(|code| anyhow!("zstd 流解码失败:{code:?}"))?;
            input_pos += in_buffer.pos();
            let produced = out_buffer.as_slice().len();
            collected.extend_from_slice(out_buffer.as_slice());
            if collected.len() > 8 * 1024 * 1024 {
                bail!("zstd 流解压超过 8 MiB 单块上限");
            }
            if input_pos >= self.pending.len() && produced == 0 {
                break;
            }
        }
        self.pending.drain(..input_pos);
        Ok(collected)
    }
}

/// 一个下行压缩块 → 连续解码产出(通常恰一个 envelope);
/// plain 套不设解码器,块即 envelope。
fn decode_envelope(
    decoder: Option<&mut ZstdContinuousDecoder>,
    chunk: &[u8],
) -> Result<WireEnvelope> {
    let bytes = match decoder {
        Some(decoder) => decoder.drain(chunk)?,
        None => chunk.to_vec(),
    };
    if bytes.is_empty() {
        bail!("压缩块未产出任何字节");
    }
    WireEnvelope::decode(&bytes[..]).context("下行 envelope 解码失败")
}

fn assert_payload(envelope: &WireEnvelope, expected: &str) -> Result<()> {
    let actual = match envelope.payload.as_ref() {
        Some(wire_envelope::Payload::HandshakeAck(_)) => "handshake_ack",
        Some(wire_envelope::Payload::SnapshotFull(_)) => "snapshot_full",
        Some(wire_envelope::Payload::Patch(_)) => "patch",
        Some(wire_envelope::Payload::Digest(_)) => "digest",
        None => "(none)",
        _ => "(other)",
    };
    if actual != expected {
        bail!("期望 {expected},收到 {actual}(channel={})", envelope.channel);
    }
    Ok(())
}
