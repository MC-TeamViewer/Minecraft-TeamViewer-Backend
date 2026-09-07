use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    pin::Pin,
    sync::{Arc, RwLock as StdRwLock},
    task::{Context, Poll},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context as AnyhowContext;
use bytes::{Buf, Bytes, BytesMut};
use futures_util::{Sink, Stream};
use std::future::Future;

use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use wtransport::tls::rustls::{
    ServerConfig as TlsServerConfig,
    crypto::{CryptoProvider, ring::default_provider as ring_provider},
    pki_types::{CertificateDer, pem::PemObject},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};
use wtransport::{
    Endpoint, Identity, ServerConfig,
    endpoint::IncomingSession,
    error::{StreamReadError, StreamWriteError},
    stream::{RecvStream, SendStream},
};

use x509_parser::prelude::{GeneralName, parse_x509_certificate, parse_x509_pem};

use crate::{
    config::{CertIdentity, WebTransportConfig},
    web::{self, WebMapFrameStream},
};

pub struct MpscReceiver {
    receiver: mpsc::Receiver<Result<Bytes, io::Error>>,
}

impl MpscReceiver {
    fn new(receiver: mpsc::Receiver<Result<Bytes, io::Error>>) -> Self {
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
    fn new(sender: mpsc::Sender<Bytes>) -> Self {
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
const MAX_FRAME_LEN: usize = 8 * 1024 * 1024;

pub async fn serve(config: WebTransportConfig, state: crate::web::AppState) -> anyhow::Result<()> {
    let (runtime, server_config) = CertRuntime::load(&config).await?;
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

    let connection = request.accept().await?;
    let remote_addr = remote_addr.to_string();
    info!(
        %remote_addr,
        path = %path,
        user_agent = user_agent.as_deref().unwrap_or("unknown"),
        "WebTransport session accepted"
    );
    let (control_rx, state_sink) = stream_bridge(connection);
    web::serve_web_map_session(
        WebMapFrameStream::new(MpscReceiver::new(control_rx)),
        MpscSink::new(state_sink),
        state,
        remote_addr,
    )
    .await;
    Ok(())
}

fn stream_bridge(
    connection: wtransport::Connection,
) -> (
    mpsc::Receiver<Result<Bytes, io::Error>>,
    mpsc::Sender<Bytes>,
) {
    let (incoming_tx, incoming_rx) = mpsc::channel::<Result<Bytes, io::Error>>(256);
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<Bytes>(256);
    let read_connection = connection.clone();
    let write_connection = connection.clone();

    tokio::spawn(async move {
        let control = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_connection.accept_bi()).await;
        let Ok(Ok((mut send, mut recv))) = control else {
            let _ = incoming_tx
                .send(Err(io::Error::other("WebTransport control stream missing")))
                .await;
            return;
        };
        let state = match tokio::time::timeout(WRITE_TIMEOUT, write_connection.open_uni()).await {
            Ok(Ok(state)) => state,
            _ => {
                let _ = incoming_tx
                    .send(Err(io::Error::other(
                        "WebTransport state stream unavailable",
                    )))
                    .await;
                return;
            }
        };
        let _state = match state.await {
            Ok(state) => state,
            Err(error) => {
                let _ = incoming_tx
                    .send(Err(io::Error::other(format!(
                        "WebTransport state stream failed: {error}"
                    ))))
                    .await;
                return;
            }
        };

        let reader = tokio::spawn(async move {
            if let Err(error) = read_frames(&mut recv, |frame| {
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

        while let Some(payload) = outgoing_rx.recv().await {
            let result = tokio::time::timeout(WRITE_TIMEOUT, write_frame(&mut send, payload))
                .await
                .unwrap_or(Err(StreamWriteError::QuicProto));
            if let Err(error) = result {
                debug!(%error, "WebTransport state write failed");
                break;
            }
        }
        reader.abort();
    });

    (incoming_rx, outgoing_tx)
}

/// 管理多张证书的运行时:加载、按 SNI 选择、逐张热轮换。
///
/// 证书表被 TLS 握手同步读取(`ResolvesServerCert::resolve` 是同步方法),
/// 因此内部使用 `std::sync::RwLock` 并以不可变快照整体替换;热轮换直接更新
/// 共享 resolver,新握手即生效,已有连接保留旧 TLS 配置。
#[derive(Clone)]
struct CertRuntime {
    config: WebTransportConfig,
    provider: Arc<CryptoProvider>,
    resolver: Arc<MultiCertResolver>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct CertFingerprint {
    cert: u64,
    key: u64,
}

impl CertRuntime {
    async fn load(config: &WebTransportConfig) -> anyhow::Result<(Self, ServerConfig)> {
        let provider: Arc<CryptoProvider> = Arc::new(ring_provider());
        let mut usable = Vec::with_capacity(config.identities.len());
        let mut entries = Vec::with_capacity(config.identities.len());
        let mut seed = None;
        for identity in &config.identities {
            match load_cert_entry(identity, &provider).await {
                Ok(entry) => {
                    if seed.is_none() {
                        // 用第一张成功证书初始化 wtransport 默认 TLS 配置,
                        // 复用其 provider/TLS 版本/ALPN 设置,避免与 wtransport 行为漂移
                        seed = Some(
                            Identity::load_pemfiles(&identity.cert_path, &identity.key_path)
                                .await
                                .context("seed WebTransport default TLS config")?,
                        );
                    }
                    usable.push(identity.clone());
                    entries.push(entry);
                }
                Err(error) => warn!(
                    cert = %identity.cert_path.display(),
                    %error,
                    "WebTransport 证书加载失败，已跳过该证书"
                ),
            }
        }
        anyhow::ensure!(!entries.is_empty(), "no usable WebTransport certificate");
        // 运行时只保留成功加载的证书,保证下标与证书表一一对应
        let runtime_config = WebTransportConfig {
            identities: usable,
            ..config.clone()
        };
        let default_index = runtime_config
            .identities
            .iter()
            .position(|identity| identity.default);
        let resolver = Arc::new(MultiCertResolver::new(ResolverTable {
            entries,
            default_index,
        }));
        let mut tls_config: TlsServerConfig =
            wtransport::tls::server::build_default_tls_config(seed.expect("seed set with entries"));
        tls_config.cert_resolver = Arc::clone(&resolver) as Arc<dyn ResolvesServerCert>;
        let server_config = ServerConfig::builder()
            .with_bind_address(config.bind_address)
            .with_custom_tls(tls_config)
            .build();
        Ok((
            Self {
                config: runtime_config,
                provider,
                resolver,
            },
            server_config,
        ))
    }

    /// 逐张轮询证书文件;单张失败只影响该张,其余证书不受拖累。
    async fn reload(&self) {
        for (index, identity) in self.config.identities.iter().enumerate() {
            let loaded = match load_cert_entry(identity, &self.provider).await {
                Ok(entry) => entry,
                Err(error) => {
                    warn!(
                        cert = %identity.cert_path.display(),
                        %error,
                        "WebTransport 证书重载失败，保留该证书当前版本"
                    );
                    continue;
                }
            };
            if self
                .resolver
                .entry_fingerprint(index)
                .is_some_and(|current| current == loaded.fingerprint)
            {
                continue;
            }
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            if loaded.not_after
                < UNIX_EPOCH
                    + Duration::from_secs(
                        now.as_secs().saturating_add(self.config.renew_window_sec),
                    )
            {
                error!(
                    cert = %identity.cert_path.display(),
                    "WebTransport 新证书将在 renewWindowSec 内过期，保持当前证书"
                );
                continue;
            }
            self.resolver.replace_entry(index, loaded);
            info!(
                cert = %identity.cert_path.display(),
                "WebTransport 证书已热轮换，对新连接生效"
            );
        }
    }

    fn describe(&self) -> String {
        self.config
            .identities
            .iter()
            .map(|identity| {
                format!(
                    "{}{}",
                    identity.cert_path.display(),
                    if identity.default { "(default)" } else { "" }
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// rustls 按握手逐次调用的证书选择器;读路径只克隆快照 Arc,临界区极短。
struct MultiCertResolver {
    table: StdRwLock<Arc<ResolverTable>>,
}

#[derive(Clone)]
struct ResolverTable {
    entries: Vec<CertEntry>,
    default_index: Option<usize>,
}

#[derive(Clone)]
struct CertEntry {
    key: Arc<CertifiedKey>,
    sans: CertSans,
    fingerprint: CertFingerprint,
    not_after: SystemTime,
}

#[derive(Clone, Debug, Default)]
struct CertSans {
    /// 精确 DNS SAN(已转小写)
    dns: Vec<String>,
    /// 形如 `*.example.com` 的泛域名 SAN(已转小写)
    wildcard: Vec<String>,
    ip: Vec<IpAddr>,
}

impl std::fmt::Debug for MultiCertResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 避免在 Debug 输出中出现证书表内容
        f.debug_struct("MultiCertResolver").finish_non_exhaustive()
    }
}

impl MultiCertResolver {
    fn new(table: ResolverTable) -> Self {
        Self {
            table: StdRwLock::new(Arc::new(table)),
        }
    }

    fn snapshot(&self) -> Arc<ResolverTable> {
        Arc::clone(&self.table.read().expect("WebTransport cert table poisoned"))
    }

    fn entry_fingerprint(&self, index: usize) -> Option<CertFingerprint> {
        Some(self.snapshot().entries.get(index)?.fingerprint)
    }

    fn replace_entry(&self, index: usize, entry: CertEntry) {
        let mut snapshot = ResolverTable::clone(&self.snapshot());
        let Some(slot) = snapshot.entries.get_mut(index) else {
            return;
        };
        *slot = entry;
        *self
            .table
            .write()
            .expect("WebTransport cert table poisoned") = Arc::new(snapshot);
    }
}

impl ResolvesServerCert for MultiCertResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let table = self.snapshot();
        let index = choose_entry(&table, client_hello.server_name());
        Some(Arc::clone(&table.entries[index].key))
    }
}

/// 证书选择规则:
/// 1) SNI 精确命中 DNS SAN;2) SNI 泛域名命中;3) default 标记;4) 第一张带 IP SAN 的;
/// 5) 兜底第一张(启动保证非空,避免 resolve 返回 None 导致握手 access_denied)。
///
/// 注:rustls 把 SNI 中的 IP 字面量视为未提供 SNI,所以 IP 直连必然走规则 3 起。
fn choose_entry(table: &ResolverTable, server_name: Option<&str>) -> usize {
    if let Some(name) = server_name {
        let lowered = name.to_ascii_lowercase();
        if let Some(index) = table
            .entries
            .iter()
            .position(|entry| entry.sans.dns.iter().any(|dns| dns == &lowered))
        {
            return index;
        }
        if let Some(index) = table.entries.iter().position(|entry| {
            entry
                .sans
                .wildcard
                .iter()
                .any(|pattern| wildcard_matches(pattern, &lowered))
        }) {
            return index;
        }
        warn!(server_name = %name, "SNI 未匹配任何 WebTransport 证书，回退到默认证书");
    }
    if let Some(index) = table.default_index
        && index < table.entries.len()
    {
        return index;
    }
    if let Some(index) = table
        .entries
        .iter()
        .position(|entry| !entry.sans.ip.is_empty())
    {
        return index;
    }
    0
}

/// 仅支持最左单标签泛域名 `*.suffix`;裸 suffix 与多级标签不匹配。
fn wildcard_matches(pattern: &str, name: &str) -> bool {
    let Some(suffix) = pattern.strip_prefix("*.") else {
        return pattern == name;
    };
    if suffix.is_empty() {
        return false;
    }
    let Some(label) = name
        .strip_suffix(suffix)
        .and_then(|rest| rest.strip_suffix('.'))
    else {
        return false;
    };
    !label.is_empty() && !label.contains('.')
}

async fn load_cert_entry(
    identity: &CertIdentity,
    provider: &CryptoProvider,
) -> anyhow::Result<CertEntry> {
    let cert_bytes = tokio::fs::read(&identity.cert_path)
        .await
        .with_context(|| format!("read {}", identity.cert_path.display()))?;
    let key_bytes = tokio::fs::read(&identity.key_path)
        .await
        .with_context(|| format!("read {}", identity.key_path.display()))?;
    load_cert_entry_from_bytes(&cert_bytes, &key_bytes, provider)
}

fn load_cert_entry_from_bytes(
    cert_bytes: &[u8],
    key_bytes: &[u8],
    provider: &CryptoProvider,
) -> anyhow::Result<CertEntry> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_bytes)
        .collect::<Result<_, _>>()
        .map_err(|error| anyhow::anyhow!("parse certificate PEM: {error}"))?;
    anyhow::ensure!(
        !chain.is_empty(),
        "certificate file contains no certificate"
    );
    let key = rustls_pemfile::private_key(&mut &key_bytes[..])
        .context("parse private key PEM")?
        .context("private key file contains no key")?;
    // from_der 会经 provider 加载私钥并比对 SPKI,校验证书与私钥配对
    let certified = CertifiedKey::from_der(chain, key, provider)
        .context("certificate and private key do not match or key is unsupported")?;
    let sans = extract_sans(certified.cert[0].as_ref())?;
    Ok(CertEntry {
        fingerprint: CertFingerprint {
            cert: fingerprint(cert_bytes),
            key: fingerprint(key_bytes),
        },
        not_after: parse_not_after(cert_bytes)?,
        sans,
        key: Arc::new(certified),
    })
}

fn extract_sans(leaf_der: &[u8]) -> anyhow::Result<CertSans> {
    let (_, certificate) = parse_x509_certificate(leaf_der)
        .map_err(|error| anyhow::anyhow!("parse certificate: {error}"))?;
    let mut sans = CertSans::default();
    if let Ok(Some(san)) = certificate.subject_alternative_name() {
        for general_name in &san.value.general_names {
            match general_name {
                GeneralName::DNSName(name) => {
                    let name = name.to_ascii_lowercase();
                    if name.starts_with("*.") {
                        sans.wildcard.push(name);
                    } else {
                        sans.dns.push(name);
                    }
                }
                GeneralName::IPAddress(bytes) => {
                    if let Ok(octets) = <[u8; 4]>::try_from(*bytes) {
                        sans.ip.push(IpAddr::V4(Ipv4Addr::from(octets)));
                    } else if let Ok(octets) = <[u8; 16]>::try_from(*bytes) {
                        sans.ip.push(IpAddr::V6(Ipv6Addr::from(octets)));
                    }
                }
                _ => {}
            }
        }
    }
    Ok(sans)
}

fn fingerprint(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn parse_not_after(cert_bytes: &[u8]) -> anyhow::Result<SystemTime> {
    let (_, pem) =
        parse_x509_pem(cert_bytes).map_err(|error| anyhow::anyhow!("parse PEM: {error}"))?;
    let (_, certificate) = parse_x509_certificate(&pem.contents)
        .map_err(|error| anyhow::anyhow!("parse certificate: {error}"))?;
    let timestamp = certificate.validity.not_after.timestamp().max(0) as u64;
    Ok(SystemTime::UNIX_EPOCH + Duration::from_secs(timestamp))
}

async fn read_frames<F, Fut>(
    stream: &mut RecvStream,
    mut on_frame: F,
) -> Result<(), StreamReadError>
where
    F: FnMut(Bytes) -> Fut,
    Fut: Future<Output = bool>,
{
    let mut buffer = BytesMut::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        while let Some((length, payload)) = split_frame_length(&buffer) {
            let frame = payload.to_vec();
            buffer.advance(4 + length);
            if !on_frame(Bytes::from(frame)).await {
                return Ok(());
            }
        }
        let read = stream.read(&mut chunk).await?;
        let Some(read) = read else {
            return Ok(());
        };
        buffer.extend_from_slice(&chunk[..read]);
    }
}

fn split_frame_length(buffer: &[u8]) -> Option<(usize, &[u8])> {
    if buffer.len() < 4 {
        return None;
    }
    let length = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
    if length > MAX_FRAME_LEN {
        return None;
    }
    let end = 4usize.checked_add(length)?;
    if buffer.len() < end {
        return None;
    }
    Some((length, &buffer[4..end]))
}

async fn write_frame(stream: &mut SendStream, payload: Bytes) -> Result<(), StreamWriteError> {
    let length = u32::try_from(payload.len())
        .map_err(|_| StreamWriteError::QuicProto)?
        .to_be_bytes();
    stream.write_all(&length).await?;
    stream.write_all(&payload).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        net::SocketAddr,
        path::{Path, PathBuf},
        process,
    };

    use wtransport::config::{ClientConfig, DnsLookupFuture, DnsResolver};
    use wtransport::tls::Sha256Digest;

    // ---------- 单元测试:纯函数 ----------

    fn test_provider() -> CryptoProvider {
        ring_provider()
    }

    fn entry_from_sans(sans: &[&str]) -> CertEntry {
        let identity = Identity::self_signed(sans).expect("self signed identity");
        let cert_pem = identity.certificate_chain().as_slice()[0].to_pem();
        let key_pem = identity.private_key().to_secret_pem();
        load_cert_entry_from_bytes(cert_pem.as_bytes(), key_pem.as_bytes(), &test_provider())
            .expect("load test certificate")
    }

    fn table(entries: Vec<CertEntry>, default_index: Option<usize>) -> ResolverTable {
        ResolverTable {
            entries,
            default_index,
        }
    }

    #[test]
    fn splits_length_prefixed_frames_from_partial_buffer() {
        let mut frame = Vec::new();
        frame.extend_from_slice(&7u32.to_be_bytes());
        frame.extend_from_slice(b"web-map");
        let (length, payload) = split_frame_length(&frame).expect("complete frame");
        assert_eq!(length, 7);
        assert_eq!(payload, b"web-map");
        assert!(split_frame_length(&frame[..8]).is_none());
    }

    #[test]
    fn rejects_oversized_frames() {
        let frame = (MAX_FRAME_LEN as u32 + 1).to_be_bytes();
        assert!(split_frame_length(&frame).is_none());
    }

    #[test]
    fn parses_certificate_not_after() {
        let identity = Identity::self_signed(["localhost"]).expect("self signed identity");
        let certificate_chain = identity.certificate_chain();
        let certificate = certificate_chain.as_slice().first().expect("certificate");
        assert!(parse_not_after(certificate.to_pem().as_bytes()).is_ok());
    }

    #[test]
    fn wildcard_matches_only_single_leading_label() {
        assert!(wildcard_matches("*.example.com", "a.example.com"));
        assert!(!wildcard_matches("*.example.com", "example.com"));
        assert!(!wildcard_matches("*.example.com", "x.y.example.com"));
        assert!(!wildcard_matches("*.example.com", "xexample.com"));
        assert!(!wildcard_matches("*.example.com", "other.org"));
        assert!(!wildcard_matches("*.example.com", ".example.com"));
        assert!(!wildcard_matches("a.example.com", "b.example.com"));
        assert!(wildcard_matches("a.example.com", "a.example.com"));
    }

    #[test]
    fn extracts_dns_wildcard_and_ip_sans() {
        let entry = entry_from_sans(&["*.example.com", "localhost", "127.0.0.1", "::1"]);
        assert_eq!(entry.sans.wildcard, ["*.example.com"]);
        assert_eq!(entry.sans.dns, ["localhost"]);
        assert!(entry.sans.ip.contains(&IpAddr::from([127u8, 0, 0, 1])));
        assert!(entry.sans.ip.contains(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert_eq!(entry.sans.ip.len(), 2);
    }

    #[test]
    fn mismatched_cert_and_key_are_rejected() {
        let a = Identity::self_signed(["a.example.com"]).expect("identity a");
        let b = Identity::self_signed(["b.example.com"]).expect("identity b");
        let cert_pem = a.certificate_chain().as_slice()[0].to_pem();
        let key_pem = b.private_key().to_secret_pem();
        assert!(
            load_cert_entry_from_bytes(cert_pem.as_bytes(), key_pem.as_bytes(), &test_provider())
                .is_err()
        );
    }

    #[test]
    fn chooses_exact_then_wildcard_then_fallbacks() {
        let a = entry_from_sans(&["a.example.com"]);
        let wild = entry_from_sans(&["*.example.com"]);
        let ip = entry_from_sans(&["127.0.0.1"]);
        let t = table(vec![a.clone(), wild.clone(), ip.clone()], None);
        assert_eq!(choose_entry(&t, Some("a.example.com")), 0);
        assert_eq!(choose_entry(&t, Some("A.Example.COM")), 0);
        assert_eq!(choose_entry(&t, Some("b.example.com")), 1);
        // 裸域/多级/未知域名不命中 → 第一张带 IP SAN 的
        assert_eq!(choose_entry(&t, Some("example.com")), 2);
        assert_eq!(choose_entry(&t, Some("x.y.example.com")), 2);
        assert_eq!(choose_entry(&t, Some("unknown.tld")), 2);
        // 无 SNI 与 IP 字面量 SNI(rustls 会把后者归为无 SNI)→ 同样走兜底
        assert_eq!(choose_entry(&t, None), 2);
        assert_eq!(choose_entry(&t, Some("127.0.0.1")), 2);

        // default 标记优先于 IP SAN 兜底
        let t2 = table(vec![a.clone(), wild.clone(), ip.clone()], Some(1));
        assert_eq!(choose_entry(&t2, Some("unknown.tld")), 1);
        assert_eq!(choose_entry(&t2, None), 1);

        // 没有 IP SAN 时兜底第一张
        let t3 = table(vec![a.clone(), wild], None);
        assert_eq!(choose_entry(&t3, None), 0);
        assert_eq!(choose_entry(&t3, Some("none.example.org")), 0);
    }

    #[test]
    fn exact_match_preferred_over_wildcard_across_certificates() {
        let wild = entry_from_sans(&["*.example.com"]);
        let a = entry_from_sans(&["a.example.com"]);
        let t = table(vec![wild, a], None);
        assert_eq!(choose_entry(&t, Some("a.example.com")), 1);
        assert_eq!(choose_entry(&t, Some("b.example.com")), 0);
    }

    #[test]
    fn resolver_replaces_entry_by_index() {
        let a = entry_from_sans(&["a.example.com"]);
        let b = entry_from_sans(&["b.example.com"]);
        let resolver = MultiCertResolver::new(table(vec![a.clone(), b.clone()], None));
        assert_eq!(resolver.entry_fingerprint(0), Some(a.fingerprint));
        let replacement = entry_from_sans(&["a.example.com"]);
        assert_ne!(replacement.fingerprint, a.fingerprint);
        resolver.replace_entry(0, replacement.clone());
        assert_eq!(resolver.entry_fingerprint(0), Some(replacement.fingerprint));
        assert_eq!(resolver.entry_fingerprint(1), Some(b.fingerprint));
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
        let (runtime, server_config) = CertRuntime::load(config).await?;
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
}
