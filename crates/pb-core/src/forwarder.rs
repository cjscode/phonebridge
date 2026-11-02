//! Local TCP forwarder: `127.0.0.1:<port>` → (tailnet) → `<phone>:5555`.
//!
//! adb can't use a userspace network, so we give it a loopback address. Every
//! connection adb opens here gets its own tailnet dial to the phone. adb uses a
//! single TCP "transport" connection per device and multiplexes everything on
//! it (shell, push, and scrcpy's video/audio/control streams, whether scrcpy
//! uses `adb reverse` or `adb forward`), so in practice there is one long-lived
//! connection. Counters make that verifiable.

use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinSet;

use crate::tailnet::DialError;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

pub trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> AsyncStream for T {}
pub type BoxStream = Box<dyn AsyncStream>;

/// Something that can open a byte stream to an address (the tailnet, or a fake in tests).
pub trait Dialer: Send + Sync + 'static {
    fn dial(&self, addr: SocketAddr) -> BoxFuture<Result<BoxStream, DialError>>;
}

/// Plain TCP dialer (tests, and a "direct" escape hatch for debugging).
pub struct TcpDialer;

impl Dialer for TcpDialer {
    fn dial(&self, addr: SocketAddr) -> BoxFuture<Result<BoxStream, DialError>> {
        Box::pin(async move {
            match tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(addr)).await {
                Err(_) => Err(DialError::Timeout),
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => Err(DialError::Refused),
                Ok(Err(e)) => Err(DialError::classify(&e.to_string())),
                Ok(Ok(s)) => {
                    let _ = s.set_nodelay(true);
                    Ok(Box::new(s) as BoxStream)
                }
            }
        })
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ForwarderEvent {
    Opened { id: u64 },
    Closed { id: u64, bytes_up: u64, bytes_down: u64, error: Option<String> },
    DialFailed { id: u64, error: DialError },
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct ForwarderStats {
    pub active: usize,
    pub total_connections: u64,
    /// Mac → phone.
    pub bytes_up: u64,
    /// Phone → Mac.
    pub bytes_down: u64,
    pub last_error: Option<String>,
}

#[derive(Default)]
struct Counters {
    active: AtomicUsize,
    total: AtomicU64,
    up: AtomicU64,
    down: AtomicU64,
    last_error: Mutex<Option<String>>,
}

pub struct Forwarder {
    local: SocketAddr,
    counters: Arc<Counters>,
    target: watch::Sender<SocketAddr>,
    events: broadcast::Sender<ForwarderEvent>,
    stop: watch::Sender<bool>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Ports adb treats as emulators on localhost; never use them.
const ADB_EMULATOR_PORTS: std::ops::RangeInclusive<u16> = 5554..=5585;

/// Bind 127.0.0.1 on `preferred` if free, else on a random port in 20000..60000.
pub async fn bind_loopback(preferred: Option<u16>) -> std::io::Result<TcpListener> {
    if let Some(p) = preferred.filter(|p| *p != 0 && !ADB_EMULATOR_PORTS.contains(p)) {
        if let Ok(l) = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, p)).await {
            return Ok(l);
        }
    }
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9e3779b97f4a7c15)
        ^ u64::from(std::process::id());
    for _ in 0..64 {
        // xorshift; no need for a real RNG here.
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let port = 20000 + (seed % 40000) as u16;
        if let Ok(l) = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).await {
            return Ok(l);
        }
    }
    TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).await
}

impl Forwarder {
    /// Start forwarding connections accepted on `listener` to `target` via `dialer`.
    pub fn start(listener: TcpListener, target: SocketAddr, dialer: Arc<dyn Dialer>) -> std::io::Result<Arc<Self>> {
        let local = listener.local_addr()?;
        let (target_tx, target_rx) = watch::channel(target);
        let (events, _) = broadcast::channel(64);
        let (stop_tx, stop_rx) = watch::channel(false);
        let counters = Arc::new(Counters::default());
        let fwd = Arc::new(Self {
            local,
            counters: counters.clone(),
            target: target_tx,
            events: events.clone(),
            stop: stop_tx,
            task: Mutex::new(None),
        });
        let handle = tokio::spawn(accept_loop(listener, target_rx, dialer, counters, events, stop_rx));
        *fwd.task.lock().unwrap() = Some(handle);
        tracing::info!("forwarder listening on {local} -> {target}");
        Ok(fwd)
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// The adb serial for this forwarder, e.g. "127.0.0.1:41234".
    pub fn adb_serial(&self) -> String {
        self.local.to_string()
    }

    pub fn target(&self) -> SocketAddr {
        *self.target.borrow()
    }

    /// Point new connections at a different address (e.g. phone IP changed).
    pub fn set_target(&self, addr: SocketAddr) {
        self.target.send_replace(addr);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ForwarderEvent> {
        self.events.subscribe()
    }

    pub fn stats(&self) -> ForwarderStats {
        let c = &self.counters;
        ForwarderStats {
            active: c.active.load(Ordering::Relaxed),
            total_connections: c.total.load(Ordering::Relaxed),
            bytes_up: c.up.load(Ordering::Relaxed),
            bytes_down: c.down.load(Ordering::Relaxed),
            last_error: c.last_error.lock().unwrap().clone(),
        }
    }

    /// Stop listening and drop all open connections.
    pub async fn stop(&self) {
        self.stop.send_replace(true);
        let handle = self.task.lock().unwrap().take();
        if let Some(h) = handle {
            let _ = h.await;
        }
    }
}

async fn accept_loop(
    listener: TcpListener,
    target: watch::Receiver<SocketAddr>,
    dialer: Arc<dyn Dialer>,
    counters: Arc<Counters>,
    events: broadcast::Sender<ForwarderEvent>,
    mut stop: watch::Receiver<bool>,
) {
    let mut conns = JoinSet::new();
    let mut next_id = 0u64;
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
            accepted = listener.accept() => {
                let (local, peer) = match accepted {
                    Ok(x) => x,
                    Err(e) => {
                        tracing::warn!("forwarder accept: {e}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                if !peer.ip().is_loopback() {
                    continue; // defence in depth; we only bind loopback
                }
                next_id += 1;
                let id = next_id;
                let _ = local.set_nodelay(true);
                let addr = *target.borrow();
                conns.spawn(handle_conn(id, local, addr, dialer.clone(), counters.clone(), events.clone()));
            }
        }
    }
    conns.shutdown().await;
}

async fn handle_conn(
    id: u64,
    local: TcpStream,
    target: SocketAddr,
    dialer: Arc<dyn Dialer>,
    c: Arc<Counters>,
    events: broadcast::Sender<ForwarderEvent>,
) {
    let remote = match dialer.dial(target).await {
        Ok(r) => r,
        Err(error) => {
            tracing::info!("forwarder #{id}: dial {target} failed: {error}");
            *c.last_error.lock().unwrap() = Some(format!("dial {target}: {error}"));
            let _ = events.send(ForwarderEvent::DialFailed { id, error });
            return; // dropping `local` closes it, so adb sees the failure
        }
    };
    c.active.fetch_add(1, Ordering::Relaxed);
    c.total.fetch_add(1, Ordering::Relaxed);
    let _ = events.send(ForwarderEvent::Opened { id });

    let (lr, lw) = tokio::io::split(local);
    let (rr, rw) = tokio::io::split(remote);
    let up = AtomicU64::new(0);
    let down = AtomicU64::new(0);
    let result = tokio::try_join!(pipe(lr, rw, &up, &c.up), pipe(rr, lw, &down, &c.down));

    c.active.fetch_sub(1, Ordering::Relaxed);
    let error = result.err().map(|e| e.to_string());
    if let Some(e) = &error {
        *c.last_error.lock().unwrap() = Some(e.clone());
    }
    let (bytes_up, bytes_down) = (up.load(Ordering::Relaxed), down.load(Ordering::Relaxed));
    tracing::info!("forwarder #{id}: closed (up {bytes_up} B, down {bytes_down} B{})",
        error.as_deref().map(|e| format!(", error: {e}")).unwrap_or_default());
    let _ = events.send(ForwarderEvent::Closed { id, bytes_up, bytes_down, error });
}

/// Copy until EOF, then half-close the writer.
async fn pipe<R, W>(mut r: R, mut w: W, conn: &AtomicU64, total: &AtomicU64) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            let _ = w.shutdown().await;
            return Ok(());
        }
        w.write_all(&buf[..n]).await?;
        conn.fetch_add(n as u64, Ordering::Relaxed);
        total.fetch_add(n as u64, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn echo_server() -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = l.accept().await.unwrap();
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn echoes_and_counts() {
        let target = echo_server().await;
        let fwd = Forwarder::start(bind_loopback(None).await.unwrap(), target, Arc::new(TcpDialer)).unwrap();
        let mut s = TcpStream::connect(fwd.local_addr()).await.unwrap();
        s.write_all(b"hello adb").await.unwrap();
        let mut buf = [0u8; 9];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello adb");
        s.shutdown().await.unwrap();
        let mut rest = Vec::new();
        s.read_to_end(&mut rest).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let st = fwd.stats();
        assert_eq!(st.total_connections, 1);
        assert_eq!(st.active, 0);
        assert_eq!(st.bytes_up, 9);
        assert_eq!(st.bytes_down, 9);
        fwd.stop().await;
    }

    #[tokio::test]
    async fn throughput_100mb() {
        let target = echo_server().await;
        let fwd = Forwarder::start(bind_loopback(None).await.unwrap(), target, Arc::new(TcpDialer)).unwrap();
        let s = TcpStream::connect(fwd.local_addr()).await.unwrap();
        let (mut r, mut w) = s.into_split();
        const N: usize = 100 * 1024 * 1024;
        let start = std::time::Instant::now();
        let writer = tokio::spawn(async move {
            let chunk = vec![7u8; 256 * 1024];
            let mut sent = 0;
            while sent < N {
                w.write_all(&chunk).await.unwrap();
                sent += chunk.len();
            }
            w.shutdown().await.unwrap();
        });
        let mut got = 0usize;
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = r.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            got += n;
        }
        writer.await.unwrap();
        assert_eq!(got, N);
        let secs = start.elapsed().as_secs_f64();
        eprintln!("forwarder loopback throughput: {:.0} MB/s", (N as f64 / 1e6) / secs);
        fwd.stop().await;
    }

    #[tokio::test]
    async fn refused_is_reported_and_closes_client() {
        // Find a port with nothing listening.
        let dead = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap()
        };
        let fwd = Forwarder::start(bind_loopback(None).await.unwrap(), dead, Arc::new(TcpDialer)).unwrap();
        let mut events = fwd.subscribe();
        let mut s = TcpStream::connect(fwd.local_addr()).await.unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(s.read(&mut buf).await.unwrap_or(0), 0, "client should see EOF");
        match events.recv().await.unwrap() {
            ForwarderEvent::DialFailed { error, .. } => assert_eq!(error, DialError::Refused),
            other => panic!("unexpected {other:?}"),
        }
        assert!(fwd.stats().last_error.unwrap().contains("refused"));
        fwd.stop().await;
    }

    #[tokio::test]
    async fn retarget_applies_to_new_connections() {
        let a = echo_server().await;
        let fwd = Forwarder::start(bind_loopback(None).await.unwrap(), a, Arc::new(TcpDialer)).unwrap();
        let b = echo_server().await;
        fwd.set_target(b);
        assert_eq!(fwd.target(), b);
        let mut s = TcpStream::connect(fwd.local_addr()).await.unwrap();
        s.write_all(b"x").await.unwrap();
        let mut buf = [0u8; 1];
        s.read_exact(&mut buf).await.unwrap();
        fwd.stop().await;
    }

    #[tokio::test]
    async fn stop_closes_active_connections() {
        let target = echo_server().await;
        let fwd = Forwarder::start(bind_loopback(None).await.unwrap(), target, Arc::new(TcpDialer)).unwrap();
        let mut s = TcpStream::connect(fwd.local_addr()).await.unwrap();
        s.write_all(b"x").await.unwrap();
        let mut buf = [0u8; 1];
        s.read_exact(&mut buf).await.unwrap();
        fwd.stop().await;
        let n = tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf)).await.unwrap().unwrap_or(0);
        assert_eq!(n, 0);
        // (Not asserting the port is closed: parallel tests may rebind it.)
    }

    #[tokio::test]
    async fn preferred_port_is_used_and_emulator_range_avoided() {
        let free = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        };
        let l = bind_loopback(Some(free)).await.unwrap();
        assert_eq!(l.local_addr().unwrap().port(), free);
        drop(l);
        let l = bind_loopback(Some(5555)).await.unwrap();
        assert!(!ADB_EMULATOR_PORTS.contains(&l.local_addr().unwrap().port()));
    }
}
