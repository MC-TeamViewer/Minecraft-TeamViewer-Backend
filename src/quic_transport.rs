//! 裸 QUIC 门:面向 Java mod 客户端(Netty codec-quic),与 WebTransport 门平行。
//!
//! 线路级与 WT 互不兼容(无 H3/extended CONNECT 层),独立 UDP 端口监听;
//! 应用层完全共享:同一 WireEnvelope、同一 `serve_web_map_session`。
//! 门约定与 WT 一致(见 TeamViewRelay-Protocol):客户端开 1 条 bi 流上行、
//! 服务端开 1 条 uni 流下行,流分帧 `[varint 长度][payload]`,datagram 裸块
//! 无前缀;首个控制流 10s 内必须建立,否则视为无效连接。
//! 裸 QUIC 没有 WT 的 path 路由——首个 WireEnvelope 的 channel 自识别
//! Player/WebMap(web.rs 握手逻辑天然支持)。

use std::{io, sync::Arc, time::Duration};

use anyhow::Context as AnyhowContext;
use bytes::Bytes;
use quinn::{Connection, Endpoint, ServerConfig, crypto::rustls::QuicServerConfig};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info};

use crate::{
    cert::CertRuntime,
    config::WebTransportConfig,
    frame,
    relay::{MOVEMENT_CHUNK_MAX_BYTES, MovementBatch},
    web::{self, WebMapFrameStream},
    web_transport::{MpscReceiver, MpscSink},
};

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
    let server_config = ServerConfig::with_crypto(Arc::new(quic_tls));
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
    // max_datagram_size 已扣除 QUIC 帧头开销;裸 QUIC 无 WT capsule 头,
    // 预算即应用层整帧大小。None 表示对端未协商 datagram 扩展。
    let datagram_capable = connection
        .max_datagram_size()
        .is_some_and(|size| size >= MOVEMENT_CHUNK_MAX_BYTES);
    let (movement_tx, movement_rx) = watch::channel(None::<Arc<MovementBatch>>);
    let (control_rx, state_sink) = stream_bridge(connection, movement_rx, suite);
    web::serve_web_map_session(
        WebMapFrameStream::new(MpscReceiver::new(control_rx)),
        MpscSink::new(state_sink),
        state,
        remote_addr,
        movement_tx,
        datagram_capable,
    )
    .await;
    Ok(())
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
    mut movement_rx: watch::Receiver<Option<Arc<MovementBatch>>>,
    suite: crate::compress::Suite,
) -> (
    mpsc::Receiver<Result<Bytes, io::Error>>,
    mpsc::Sender<Bytes>,
) {
    let (incoming_tx, incoming_rx) = mpsc::channel::<Result<Bytes, io::Error>>(256);
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<Bytes>(256);
    let read_connection = connection.clone();
    let write_connection = connection.clone();

    // 位置 datagram 发送任务:与 WT 门语义一致——relay 每 dirty tick 发布
    // 预切好的 movement 批,逐块装入单个 datagram(裸 WireEnvelope,无前缀);
    // TooLarge 时重查预算,装不下的块跳过——下个 dirty tick 全量重发。
    let send_connection = connection.clone();
    tokio::spawn(async move {
        loop {
            if movement_rx.changed().await.is_err() {
                return;
            }
            let Some(batch) = movement_rx.borrow_and_update().clone() else {
                continue;
            };
            let mut max_size = send_connection.max_datagram_size();
            for chunk in batch.chunks.iter() {
                let frame_len = chunk.len();
                if !max_size.is_some_and(|size| frame_len <= size) {
                    max_size = send_connection.max_datagram_size();
                    if !max_size.is_some_and(|size| frame_len <= size) {
                        continue;
                    }
                }
                let datagram = Bytes::copy_from_slice(chunk);
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
    });

    // 收方向 datagram 排空:mod 客户端不回发 datagram,但必须持续读取,
    // 否则对端发送缓冲堆积会触发其丢弃或流控
    let drain_connection = connection.clone();
    tokio::spawn(async move {
        while let Ok(datagram) = drain_connection.read_datagram().await {
            drop(datagram);
        }
    });

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

        let reader = tokio::spawn(async move {
            // 上行 zstd:压缩块 → 持久 DCtx → envelope;分块边界由对端
            // flush 保证,但解码按流推进,不依赖该假设
            let mut decoder = if suite.stream_zstd() {
                match crate::compress::StreamDecoder::new() {
                    Ok(decoder) => Some(decoder),
                    Err(error) => {
                        debug!(%error, "QUIC uplink decoder unavailable");
                        let _ = incoming_tx
                            .clone()
                            .send(Err(io::Error::other("uplink decoder unavailable")))
                            .await;
                        return;
                    }
                }
            } else {
                None
            };
            if let Err(error) = frame::read_frames(&mut recv, |frame| {
                let incoming_tx = incoming_tx.clone();
                // 解码在闭包体内同步完成;future 只做跨任务投递
                let decoded = match decoder.as_mut() {
                    Some(decoder) => decoder.decompress_chunk(&frame).map(Bytes::from),
                    None => Ok(frame),
                };
                async move {
                    match decoded {
                        Ok(payload) => {
                            debug!(frame_len = payload.len(), "QUIC frame read");
                            incoming_tx.send(Ok(payload)).await.is_ok()
                        }
                        // 解压失败属协议违规:显式断连
                        Err(error) => {
                            debug!(%error, "QUIC uplink decompress failed");
                            incoming_tx
                                .send(Err(io::Error::other(format!(
                                    "uplink decompress failed: {error}"
                                ))))
                                .await
                                .is_ok()
                        }
                    }
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

    (incoming_rx, outgoing_tx)
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, sync::Arc as StdArc, time::Duration};

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
        let (_movement_tx, movement_rx) = watch::channel(None);
        let (mut incoming_rx, outgoing_tx) =
            stream_bridge(server_conn, movement_rx, crate::compress::Suite::Plain);

        // 客户端打开控制流并发送一帧
        let (mut control_send, mut control_recv) = connection.open_bi().await.expect("open bi");
        frame::write_frame(&mut control_send, b"h")
            .await
            .expect("control write");

        let frame = tokio::time::timeout(Duration::from_secs(5), incoming_rx.recv())
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
    async fn zstd_suite_compresses_both_bridge_directions() {
        let (addr, leaf, mut server_rx) = start_endpoint(&[TEST_SNI]).await.expect("endpoint");
        let connection = connect(addr, leaf, &[ALPN_ZSTD.as_bytes()]).await;
        assert_eq!(
            negotiated_alpn(&connection).as_deref(),
            Some(ALPN_ZSTD),
            "服务端应接受 +zstd ALPN"
        );
        let server_conn = server_rx.recv().await.expect("server accepted");

        let (_movement_tx, movement_rx) = watch::channel(None);
        let (mut incoming_rx, outgoing_tx) =
            stream_bridge(server_conn, movement_rx, crate::compress::Suite::Zstd);

        // 上行:测试侧持久压缩器逐 envelope flush 出压缩块,写控制流
        let (mut control_send, _control_recv) = connection.open_bi().await.expect("open bi");
        let mut uplink = crate::compress::StreamEncoder::new().expect("uplink encoder");
        for envelope in [b"handshake".to_vec(), vec![7u8; 2048]] {
            let chunk = uplink.compress_chunk(&envelope).expect("compress");
            frame::write_frame(&mut control_send, &chunk)
                .await
                .expect("control write");
        }
        for envelope in [b"handshake".to_vec(), vec![7u8; 2048]] {
            let frame = tokio::time::timeout(Duration::from_secs(5), incoming_rx.recv())
                .await
                .expect("frame timeout")
                .expect("frame")
                .expect("frame ok");
            assert_eq!(&frame[..], &envelope[..], "上行解压须按序还原 envelope");
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

        let (movement_tx, movement_rx) = watch::channel(None);
        let (_incoming_rx, _outgoing_tx) =
            stream_bridge(server_conn, movement_rx, crate::compress::Suite::Plain);

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
