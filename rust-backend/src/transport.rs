use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use axum::{
    extract::connect_info::Connected,
    serve::{IncomingStream, Listener},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
};

/// Per-TCP-connection counters below WebSocket framing and compression.
///
/// Counters are activated only after the HTTP 101 upgrade completes, so the
/// management "wire" layer contains WebSocket traffic but not the HTTP
/// handshake itself.
#[derive(Debug, Default)]
pub struct WireCounter {
    active: AtomicBool,
    ingress: AtomicU64,
    egress: AtomicU64,
}

impl WireCounter {
    pub fn activate(&self) {
        self.ingress.store(0, Ordering::Relaxed);
        self.egress.store(0, Ordering::Relaxed);
        self.active.store(true, Ordering::Release);
    }

    pub fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
    }

    pub fn totals(&self) -> (u64, u64) {
        (
            self.ingress.load(Ordering::Relaxed),
            self.egress.load(Ordering::Relaxed),
        )
    }

    fn record_ingress(&self, amount: usize) {
        if self.active.load(Ordering::Acquire) {
            self.ingress.fetch_add(amount as u64, Ordering::Relaxed);
        }
    }

    fn record_egress(&self, amount: usize) {
        if self.active.load(Ordering::Acquire) {
            self.egress.fetch_add(amount as u64, Ordering::Relaxed);
        }
    }
}

#[derive(Clone, Debug)]
pub struct TransportConnectInfo {
    pub remote_addr: SocketAddr,
    pub wire: Arc<WireCounter>,
}

pub struct CountingIo {
    inner: TcpStream,
    wire: Arc<WireCounter>,
}

impl AsyncRead for CountingIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &result {
            this.wire
                .record_ingress(buf.filled().len().saturating_sub(before));
        }
        result
    }
}

impl AsyncWrite for CountingIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(amount)) = result {
            this.wire.record_egress(amount);
            Poll::Ready(Ok(amount))
        } else {
            result
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write_vectored(cx, bufs);
        if let Poll::Ready(Ok(amount)) = result {
            this.wire.record_egress(amount);
            Poll::Ready(Ok(amount))
        } else {
            result
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

pub struct CountingListener {
    inner: TcpListener,
}

impl CountingListener {
    pub fn new(inner: TcpListener) -> Self {
        Self { inner }
    }
}

impl Listener for CountingListener {
    type Io = CountingIo;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.inner.accept().await {
                Ok((inner, remote_addr)) => {
                    let _ = inner.set_nodelay(true);
                    let wire = Arc::new(WireCounter::default());
                    return (CountingIo { inner, wire }, remote_addr);
                }
                Err(error) => {
                    tracing::warn!(%error, "TCP accept failed; retrying");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

impl Connected<IncomingStream<'_, CountingListener>> for TransportConnectInfo {
    fn connect_info(stream: IncomingStream<'_, CountingListener>) -> Self {
        Self {
            remote_addr: *stream.remote_addr(),
            wire: stream.io().wire.clone(),
        }
    }
}
