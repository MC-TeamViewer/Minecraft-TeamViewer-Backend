//! 证书运行时:多证书加载、SNI 选择、热轮换——WebTransport 门与裸 QUIC 门共享。
//!
//! 门无关:rustls 0.23 类型经 wtransport 再导出,与 quinn `QuicServerConfig`
//! 所需的是同一套类型。[`CertRuntime::load`] 返回已接管证书选择的裸 rustls
//! 配置;各门自行补足门专属设定(WT 门原样交给 wtransport,QUIC 门改写
//! `alpn_protocols` 后包装进 quinn)。

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::{Arc, RwLock as StdRwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context as AnyhowContext;
use tracing::{error, info, warn};
use wtransport::Identity;
use wtransport::tls::rustls::{
    ServerConfig as TlsServerConfig,
    crypto::{CryptoProvider, ring::default_provider as ring_provider},
    pki_types::{CertificateDer, pem::PemObject},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};

use x509_parser::prelude::{GeneralName, parse_x509_certificate, parse_x509_pem};

use crate::config::{CertIdentity, WebTransportConfig};

/// 管理多张证书的运行时:加载、按 SNI 选择、逐张热轮换。
///
/// 证书表被 TLS 握手同步读取(`ResolvesServerCert::resolve` 是同步方法),
/// 因此内部使用 `std::sync::RwLock` 并以不可变快照整体替换;热轮换直接更新
/// 共享 resolver,新握手即生效,已有连接保留旧 TLS 配置。
#[derive(Clone)]
pub(crate) struct CertRuntime {
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
    /// 加载证书表并构建 rustls TLS 配置(证书选择器已接管)。
    /// 返回的配置不带门专属设定,由调用门补足。
    pub(crate) async fn load(
        config: &WebTransportConfig,
    ) -> anyhow::Result<(Self, TlsServerConfig)> {
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
        Ok((
            Self {
                config: runtime_config,
                provider,
                resolver,
            },
            tls_config,
        ))
    }

    /// 逐张轮询证书文件;单张失败只影响该张,其余证书不受拖累。
    pub(crate) async fn reload(&self) {
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

    pub(crate) fn describe(&self) -> String {
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
        Arc::clone(&self.table.read().expect("cert table poisoned"))
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
        *self.table.write().expect("cert table poisoned") = Arc::new(snapshot);
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
        warn!(server_name = %name, "SNI 未匹配任何证书，回退到默认证书");
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
