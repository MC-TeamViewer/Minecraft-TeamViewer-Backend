use std::{
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context as AnyhowContext;
use bytes::{Buf, Bytes, BytesMut};
use futures_util::{Sink, Stream};
use std::future::Future;

use tokio::sync::{RwLock, mpsc};
use tracing::{debug, error, info, warn};
use wtransport::{
    Endpoint, Identity, ServerConfig,
    endpoint::{IncomingSession, endpoint_side::Server as ServerEndpointSide},
    error::{StreamReadError, StreamWriteError},
    stream::{RecvStream, SendStream},
};
use x509_parser::prelude::{parse_x509_certificate, parse_x509_pem};

use crate::{
    config::WebTransportConfig,
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
    let runtime = CertRuntime::load(&config).await?;
    let server_config = runtime.initial_server_config().await?;
    let endpoint =
        Endpoint::server(server_config).context("failed to bind WebTransport UDP endpoint")?;
    info!(
        address = %config.bind_address,
        cert = %config.cert_path.display(),
        key = %config.key_path.display(),
        "WebTransport endpoint listening"
    );
    let mut reload_interval = tokio::time::interval(Duration::from_secs(config.poll_interval_sec));
    reload_interval.tick().await;
    loop {
        tokio::select! {
            _ = reload_interval.tick() => {
                if !runtime.reload(&endpoint).await {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
            incoming = endpoint.accept() => {
                let state = state.clone();
                let runtime = runtime.clone();
                tokio::spawn(async move {
                    if let Err(error) = accept_session(incoming, state, runtime).await {
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
    _runtime: CertRuntime,
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

#[derive(Clone)]
struct CertRuntime {
    config: WebTransportConfig,
    fingerprint: Arc<RwLock<CertFingerprint>>,
}

struct LoadedCert {
    identity: Identity,
    fingerprint: CertFingerprint,
    not_after: SystemTime,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct CertFingerprint {
    cert: u64,
    key: u64,
}

impl CertRuntime {
    async fn load(config: &WebTransportConfig) -> anyhow::Result<Self> {
        let loaded = load_identity(config)
            .await
            .context("invalid WebTransport certificate")?;
        Ok(Self {
            config: config.clone(),
            fingerprint: Arc::new(RwLock::new(loaded.fingerprint)),
        })
    }

    async fn initial_server_config(&self) -> anyhow::Result<ServerConfig> {
        let loaded = load_identity(&self.config).await?;
        Ok(Self::build_server_config(&self.config, loaded.identity))
    }

    fn build_server_config(config: &WebTransportConfig, identity: Identity) -> ServerConfig {
        ServerConfig::builder()
            .with_bind_address(config.bind_address)
            .with_identity(identity)
            .build()
    }

    async fn reload(&self, endpoint: &Endpoint<ServerEndpointSide>) -> bool {
        match load_identity(&self.config).await {
            Ok(loaded) => {
                let current = *self.fingerprint.read().await;
                if current == loaded.fingerprint {
                    return true;
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
                    error!(cert = %self.config.cert_path.display(), "WebTransport certificate expires inside renewWindowSec; keeping current certificate");
                    return true;
                }
                *self.fingerprint.write().await = loaded.fingerprint;
                if let Err(error) = self.apply_reload(loaded.identity, endpoint).await {
                    error!(%error, "failed to apply WebTransport certificate; keeping current certificate");
                    return false;
                }
                info!(cert = %self.config.cert_path.display(), "WebTransport certificate reloaded");
                true
            }
            Err(error) => {
                warn!(%error, "WebTransport certificate reload failed; keeping current certificate");
                true
            }
        }
    }

    async fn apply_reload(
        &self,
        identity: Identity,
        endpoint: &Endpoint<ServerEndpointSide>,
    ) -> anyhow::Result<()> {
        let server_config = Self::build_server_config(&self.config, identity);
        endpoint.reload_config(server_config, false)?;
        Ok(())
    }
}

async fn load_identity(config: &WebTransportConfig) -> anyhow::Result<LoadedCert> {
    let cert_bytes = tokio::fs::read(&config.cert_path)
        .await
        .with_context(|| format!("read {}", config.cert_path.display()))?;
    let key_bytes = tokio::fs::read(&config.key_path)
        .await
        .with_context(|| format!("read {}", config.key_path.display()))?;
    let identity = Identity::load_pemfiles(&config.cert_path, &config.key_path)
        .await
        .context("parse WebTransport certificate")?;
    let not_after =
        parse_not_after(&cert_bytes).context("parse WebTransport certificate expiry")?;
    Ok(LoadedCert {
        identity,
        fingerprint: CertFingerprint {
            cert: fingerprint(&cert_bytes),
            key: fingerprint(&key_bytes),
        },
        not_after,
    })
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
}
