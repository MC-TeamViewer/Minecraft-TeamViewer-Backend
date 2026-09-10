//! Phase 4 端到端冒烟:对本地起着的 QUIC 门(默认 127.0.0.1:8767)做黑盒验证。
//!
//! 覆盖三条 ALPN 路径:
//!   1. `teamviewrelay/v1+zstd`:上行恒 plain varint 分帧;下行 uni 流
//!      `[varint][zstd 压缩块]`,持久解压器跨块连续解码;首块必须以 zstd
//!      帧魔数开头,证明下行真被压缩。
//!   2. `teamviewrelay/v1` plain:下行帧即裸 envelope,不出现 zstd 魔数。
//!   3. `teamviewrelay/v1+zstd-dict`:Player 连接(plain)持续上报位置喂
//!      movement 批,服务端攒样本训练字典,经门控下行流下发 DictOffer;
//!      本地装字典后经客户端第 1 条单向流回 DictReady,后续 movement
//!      datagram 字典可解,且至少一条「无字典必不解」——字典压缩实证。
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
use teamviewrelay_rust::proto::teamviewer::door::v1::{
    door_control_frame, DatagramDictReady, DoorControlFrame,
};
use teamviewrelay_rust::proto::teamviewer::v1::{
    wire_envelope, PlayerDelta, PlayerHandshakeRequest, PlayerPatchScope, PlayerReportBundle,
    PlayerUpsert, ResyncRequest, UnreliableChannel, WebMapHandshakeRequest, WireChannel,
    WireEnvelope,
};

const ALPN_PLAIN: &str = "teamviewrelay/v1";
const ALPN_ZSTD: &str = "teamviewrelay/v1+zstd";
const ALPN_ZSTD_DICT: &str = "teamviewrelay/v1+zstd-dict";
const ZSTD_FRAME_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
const SERVER_ADDR: &str = "127.0.0.1:8767";
/// 与 mod/脚本一致的解压窗口上限(2^23)。
const MAX_DECOMPRESSION_WINDOW_LOG: u32 = 23;
/// 字典用例的位置上报者(Player 角色),也是 movement 批的唯一数据源。
const SMOKE_PLAYER_ID: &str = "00000000-0000-0000-0000-00000000d1c7";

/// 目标门地址,默认本机;弱网代理/netem 场景用环境变量改指代理端口。
fn server_addr() -> std::net::SocketAddr {
    std::env::var("TEAMVIEWER_QUIC_SMOKE_SERVER")
        .unwrap_or_else(|_| SERVER_ADDR.to_string())
        .parse()
        .expect("TEAMVIEWER_QUIC_SMOKE_SERVER 必须是合法的 host:port")
}

#[tokio::main]
async fn main() -> Result<()> {
    let zstd_dict = run_dict_case("quic-smoke-zstd-dict").await;
    let zstd = run_case(ALPN_ZSTD, "quic-smoke-zstd").await;
    let plain = run_case(ALPN_PLAIN, "quic-smoke-plain").await;
    zstd_dict?;
    zstd?;
    plain?;
    println!("QUIC 门冒烟矩阵全部通过(+zstd-dict、+zstd 与 plain 三条 ALPN 路径)");
    Ok(())
}

async fn run_case(alpn: &str, room: &str) -> Result<()> {
    let expect_zstd = alpn == ALPN_ZSTD;
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
        write_frame(&mut uplink, &web_handshake(room, false)).await?;
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
fn web_handshake(room: &str, accepts_movement: bool) -> Vec<u8> {
    let envelope = WireEnvelope {
        channel: WireChannel::WebMap as i32,
        payload: Some(wire_envelope::Payload::WebMapHandshakeRequest(
            WebMapHandshakeRequest {
                network_protocol_version: "0.9.0".into(),
                minimum_compatible_network_protocol_version: "0.6.1".into(),
                local_program_version: "quic-smoke-rust".into(),
                room_code: Some(room.into()),
                accepts_unreliable_positions: None,
                accepts_channels: if accepts_movement {
                    vec![UnreliableChannel::Movement as i32]
                } else {
                    Vec::new()
                },
            },
        )),
    };
    envelope.encode_to_vec()
}

fn resync_request() -> Vec<u8> {
    let envelope = WireEnvelope {
        channel: WireChannel::WebMap as i32,
        payload: Some(wire_envelope::Payload::ResyncRequest(ResyncRequest::default())),
    };
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

/// +zstd-dict 全链路用例(90s 预算:训练阈值 64 批 + 16 KiB 样本):
/// Player 连接(plain)持续上报位置喂 movement 批 → 服务端攒样本训练字典 →
/// 门控下行流(服务端第 2 条单向流)DictOffer → 本地装字典、客户端第 1 条
/// 单向流回 DictReady → 后续 movement datagram 字典可解,且至少一条
/// 「无字典必不解」,证明服务端真在用字典压缩。
async fn run_dict_case(room: &str) -> Result<()> {
    let outcome = tokio::time::timeout(Duration::from_secs(90), async {
        let connection = connect(ALPN_ZSTD_DICT).await?;
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
        if negotiated != ALPN_ZSTD_DICT {
            bail!("服务端必须回显客户端选的 ALPN:{negotiated} != {ALPN_ZSTD_DICT}");
        }

        // 上行握手:WebMap 声明消费 movement datagram(alpha.6 字段 6)
        let (mut uplink, _uplink_recv) = connection.open_bi().await?;
        write_frame(&mut uplink, &web_handshake(room, true)).await?;
        println!("  [{ALPN_ZSTD_DICT}] 上行握手已发(声明消费 movement datagram)");

        // 可靠下行 = 服务端第 1 条单向流,仍是连续 zstd 流(字典只作用于 datagram)
        let mut downlink = connection.accept_uni().await?;
        let first_chunk = read_frame(&mut downlink).await?;
        if first_chunk.len() < 4 || first_chunk[..4] != ZSTD_FRAME_MAGIC {
            bail!("+zstd-dict 套可靠下行必须以 zstd 帧魔数开头(字典不该影响可靠流)");
        }
        let mut zstd_decoder = ZstdContinuousDecoder::new()?;
        let ack = decode_envelope(Some(&mut zstd_decoder), &first_chunk)?;
        assert_payload(&ack, "handshake_ack")?;

        // Player 连接(plain ALPN)持续上报位置:movement 批 → 训练样本
        let reporter = tokio::spawn(report_positions(room.to_string()));

        // 门控下行 = 服务端第 2 条单向流;样本攒够才实体化,accept 一直等
        let mut door = connection.accept_uni().await?;
        println!("  [{ALPN_ZSTD_DICT}] 门控下行流已实体化(字典训练完成)");
        let offer_frame = read_frame(&mut door).await?;
        let offer = match DoorControlFrame::decode(offer_frame.as_slice())?.payload {
            Some(door_control_frame::Payload::DictOffer(offer)) => offer,
            other => bail!("门控下行首帧应为 DictOffer,收到 {other:?}"),
        };
        if offer.content.is_empty() {
            bail!("DictOffer 缺字典内容");
        }

        // 装字典 + 客户端第 1 条单向流回 DictReady(单流,不关闭)
        let mut dctx = zstd_safe::DCtx::try_create().ok_or_else(|| anyhow!("zstd DCtx 创建失败"))?;
        dctx.set_parameter(zstd_safe::DParameter::WindowLogMax(
            MAX_DECOMPRESSION_WINDOW_LOG,
        ))
        .map_err(|code| anyhow!("zstd 窗口上限设置失败:{code:?}"))?;
        dctx.load_dictionary(&offer.content)
            .map_err(|code| anyhow!("字典装载失败:{code:?}"))?;
        let mut door_uplink = connection.open_uni().await?;
        write_frame(&mut door_uplink, &dict_ready_frame(&offer.dictionary_id)).await?;
        println!(
            "  [{ALPN_ZSTD_DICT}] DictOffer 已装字典,DatagramDictReady 已回(字典 {})",
            offer.dictionary_id
        );

        // datagram:字典可解 → WireEnvelope{WebMap, Patch};至少一条无字典
        // 必不解,证明服务端真在用字典压缩
        let mut dict_proved = false;
        for _ in 0..1000 {
            let datagram = match tokio::time::timeout(Duration::from_secs(5), connection.read_datagram()).await {
                Ok(Ok(datagram)) => datagram,
                Ok(Err(error)) => bail!("datagram 读取失败:{error}"),
                Err(_) => break, // 5 秒无 datagram:收尾并按已有证据判定
            };
            let mut window = vec![0u8; 64 * 1024];
            let written = match dctx.decompress(&mut window, &datagram) {
                Ok(written) if written > 0 => written,
                // in-flight 无字典帧(ready 生效前发出):跳过
                _ => continue,
            };
            let envelope =
                WireEnvelope::decode(&window[..written]).context("字典 datagram 解出非法 envelope")?;
            match envelope.payload {
                Some(wire_envelope::Payload::Patch(patch)) => {
                    if patch
                        .players
                        .as_ref()
                        .is_none_or(|scope| scope.upsert.is_empty())
                    {
                        bail!("movement datagram patch 无 players upsert");
                    }
                }
                other => bail!("datagram 载荷应为 Patch,收到 {other:?}"),
            }
            if decompress_without_dict(&datagram).is_none() {
                dict_proved = true;
                break;
            }
        }
        reporter.abort();
        if !dict_proved {
            bail!("datagram 从未出现「无字典不可解」的字典帧(服务端疑似未激活字典)");
        }
        println!("  [{ALPN_ZSTD_DICT}] movement datagram 字典解码通(无字典不可解,字典压缩实证)");
        Ok::<(), anyhow::Error>(())
    })
    .await;
    match outcome {
        Ok(Ok(())) => {
            println!("{ALPN_ZSTD_DICT}: PASS");
            Ok(())
        }
        Ok(Err(error)) => Err(error),
        Err(_) => Err(anyhow!("用例 {ALPN_ZSTD_DICT} 超时(90s)")),
    }
}

/// Player 角色(plain ALPN)持续上报移动位置:喂出 movement 批流,驱动服务端
/// 攒样本训练字典。用例收尾时 abort;自身失败只能表现为用例超时,诚实可见。
async fn report_positions(room: String) {
    let _ = async {
        let connection = connect(ALPN_PLAIN).await?;
        let (mut uplink, _recv) = connection.open_bi().await?;
        write_frame(&mut uplink, &player_handshake(&room)).await?;
        tokio::time::sleep(Duration::from_millis(500)).await; // 等握手注册
        for tick in 0..4000 {
            let envelope = player_report(tick);
            if write_frame(&mut uplink, &envelope).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
}

fn player_handshake(room: &str) -> Vec<u8> {
    let envelope = WireEnvelope {
        channel: WireChannel::Player as i32,
        payload: Some(wire_envelope::Payload::PlayerHandshakeRequest(
            PlayerHandshakeRequest {
                network_protocol_version: "0.9.0".into(),
                minimum_compatible_network_protocol_version: "0.6.1".into(),
                local_program_version: "quic-smoke-rust".into(),
                submit_player_id: SMOKE_PLAYER_ID.into(),
                room_code: Some(room.into()),
                preferred_report_interval_ticks: Some(1),
                min_report_interval_ticks: Some(1),
                max_report_interval_ticks: Some(1000),
                ..Default::default()
            },
        )),
    };
    envelope.encode_to_vec()
}

fn player_report(tick: usize) -> Vec<u8> {
    let envelope = WireEnvelope {
        channel: WireChannel::Player as i32,
        payload: Some(wire_envelope::Payload::PlayerReportBundle(
            PlayerReportBundle {
                submit_player_id: SMOKE_PLAYER_ID.into(),
                players_patch: Some(PlayerPatchScope {
                    upsert: vec![PlayerUpsert {
                        id: SMOKE_PLAYER_ID.into(),
                        data: Some(PlayerDelta {
                            x: Some(100.0 + (tick % 1000) as f64 * 0.1),
                            y: Some(64.0),
                            z: Some(200.0),
                            dimension: Some("minecraft:overworld".into()),
                            player_name: Some("quic-smoke-player".into()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }],
                    delete: Vec::new(),
                }),
                ..Default::default()
            },
        )),
    };
    envelope.encode_to_vec()
}

fn dict_ready_frame(dictionary_id: &str) -> Vec<u8> {
    DoorControlFrame {
        payload: Some(door_control_frame::Payload::DictReady(DatagramDictReady {
            dictionary_id: dictionary_id.into(),
        })),
    }
    .encode_to_vec()
}

/// 无字典单帧解压:字典帧必须失败(返回 None),这是「字典压缩实证」的依据。
fn decompress_without_dict(data: &[u8]) -> Option<Vec<u8>> {
    let mut dctx = zstd_safe::DCtx::try_create()?;
    dctx.set_parameter(zstd_safe::DParameter::WindowLogMax(
        MAX_DECOMPRESSION_WINDOW_LOG,
    ))
    .ok()?;
    let mut window = vec![0u8; 64 * 1024];
    let written = dctx.decompress(&mut window, data).ok()?;
    (written > 0).then(|| window[..written].to_vec())
}
