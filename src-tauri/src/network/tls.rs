//! TLS termination for the sync and terminal TCP servers.
//!
//! The desktop owns a long-lived self-signed certificate. Phones do not validate it
//! against a CA; they pin its SHA-256 fingerprint while pairing (and the pairing
//! code shown to the user is bound to that fingerprint, see `network::pairing`).
//!
//! The session/terminal servers keep working with plain [`TcpStream`]s. A proxy
//! terminates TLS on the public socket and forwards the decrypted bytes to a
//! loopback-only listener, announcing the real peer address in a one-line
//! preamble (`PROXY <secret> <ip>\n`). The secret is random per process, so a
//! local process that merely finds the loopback port cannot spoof a peer IP.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use rand::RngExt as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager};

/// A client must complete the TLS handshake within this time.
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(10);
/// Total simultaneous proxied connections.
const MAX_CONNECTIONS: usize = 64;
/// Simultaneous proxied connections from one IP address.
const MAX_CONNECTIONS_PER_IP: usize = 8;

pub struct TlsIdentity {
    pub config: Arc<ServerConfig>,
    /// SHA-256 of the DER certificate.
    pub fingerprint: Vec<u8>,
}

static IDENTITY: OnceLock<TlsIdentity> = OnceLock::new();
static PROXY_SECRET: OnceLock<String> = OnceLock::new();

/// Load (or create on first run) the desktop's TLS identity.
pub fn init(app: &AppHandle) -> Result<&'static TlsIdentity, String> {
    if let Some(id) = IDENTITY.get() {
        return Ok(id);
    }
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Could not resolve app data dir: {e}"))?;
    fs::create_dir_all(&dir).map_err(|e| format!("Could not create app data dir: {e}"))?;
    let identity = load_or_create(&dir)?;
    Ok(IDENTITY.get_or_init(|| identity))
}

/// The identity created by [`init`]. Panics if called before `init`.
pub fn identity() -> &'static TlsIdentity {
    IDENTITY.get().expect("TLS identity not initialised")
}

pub fn proxy_secret() -> &'static str {
    PROXY_SECRET.get_or_init(|| {
        rand::rng()
            .sample_iter(&rand::distr::Alphanumeric)
            .take(48)
            .map(char::from)
            .collect()
    })
}

fn load_or_create(dir: &Path) -> Result<TlsIdentity, String> {
    let cert_path = dir.join("tls_cert.der");
    let key_path = dir.join("tls_key.der");

    let (cert_der, key_der) = match (fs::read(&cert_path), fs::read(&key_path)) {
        (Ok(c), Ok(k)) if !c.is_empty() && !k.is_empty() => (c, k),
        _ => {
            let certified = rcgen::generate_simple_self_signed(vec!["pzync".to_string()])
                .map_err(|e| format!("Could not generate TLS certificate: {e}"))?;
            let cert_der = certified.cert.der().to_vec();
            let key_der = certified.signing_key.serialize_der();
            fs::write(&cert_path, &cert_der)
                .map_err(|e| format!("Could not save TLS certificate: {e}"))?;
            write_private(&key_path, &key_der)
                .map_err(|e| format!("Could not save TLS key: {e}"))?;
            (cert_der, key_der)
        }
    };

    identity_from_der(cert_der, key_der)
}

fn identity_from_der(cert_der: Vec<u8>, key_der: Vec<u8>) -> Result<TlsIdentity, String> {
    let fingerprint = Sha256::digest(&cert_der).to_vec();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS protocol setup failed: {e}"))?
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert_der)],
            PrivateKeyDer::from(PrivatePkcs8KeyDer::from(key_der)),
        )
        .map_err(|e| format!("TLS certificate setup failed: {e}"))?;
    Ok(TlsIdentity {
        config: Arc::new(config),
        fingerprint,
    })
}

fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(data)
    }
    #[cfg(not(unix))]
    {
        fs::write(path, data)
    }
}

// ─── Connection limiting ─────────────────────────────────────────────────────

#[derive(Default)]
struct Limiter {
    total: AtomicUsize,
    per_ip: Mutex<HashMap<IpAddr, usize>>,
}

struct Permit {
    limiter: Arc<Limiter>,
    ip: IpAddr,
}

impl Limiter {
    fn acquire(self: &Arc<Self>, ip: IpAddr) -> Option<Permit> {
        let mut map = self.per_ip.lock().unwrap();
        if self.total.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
            return None;
        }
        let n = map.entry(ip).or_insert(0);
        if *n >= MAX_CONNECTIONS_PER_IP {
            return None;
        }
        *n += 1;
        self.total.fetch_add(1, Ordering::Relaxed);
        Some(Permit { limiter: self.clone(), ip })
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut map = self.limiter.per_ip.lock().unwrap();
        if let Some(n) = map.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                map.remove(&self.ip);
            }
        }
        self.limiter.total.fetch_sub(1, Ordering::Relaxed);
    }
}

// ─── Proxy ───────────────────────────────────────────────────────────────────

/// Accept TCP on `public`, terminate TLS, and forward plaintext to
/// `127.0.0.1:inner_port`. Runs until `stop` is set.
pub fn run_proxy(
    public: TcpListener,
    inner_port: u16,
    config: Arc<ServerConfig>,
    stop: Arc<AtomicBool>,
) {
    let limiter = Arc::new(Limiter::default());
    public.set_nonblocking(true).ok();
    while !stop.load(Ordering::Relaxed) {
        match public.accept() {
            Ok((tcp, addr)) => {
                let ip = addr.ip();
                let Some(permit) = limiter.acquire(ip) else {
                    eprintln!("[tls] Connection limit reached, dropping {ip}");
                    continue;
                };
                let config = config.clone();
                thread::spawn(move || {
                    let _permit = permit;
                    if let Err(e) = pump(tcp, ip, inner_port, config) {
                        eprintln!("[tls] connection from {ip} ended: {e}");
                    }
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                eprintln!("[tls] accept error: {e}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

fn pump(
    tcp: TcpStream,
    ip: IpAddr,
    inner_port: u16,
    config: Arc<ServerConfig>,
) -> Result<(), String> {
    tcp.set_nonblocking(false).map_err(|e| e.to_string())?;
    tcp.set_nodelay(true).ok();
    tcp.set_read_timeout(Some(Duration::from_millis(20)))
        .map_err(|e| e.to_string())?;
    tcp.set_write_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;

    let conn = ServerConnection::new(config).map_err(|e| e.to_string())?;
    let mut tls = StreamOwned::new(conn, tcp);

    let mut inner = TcpStream::connect(("127.0.0.1", inner_port)).map_err(|e| e.to_string())?;
    inner.set_nodelay(true).ok();
    inner
        .set_write_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    inner
        .set_read_timeout(Some(Duration::from_millis(2)))
        .map_err(|e| e.to_string())?;
    inner
        .write_all(format!("PROXY {} {}\n", proxy_secret(), ip).as_bytes())
        .map_err(|e| e.to_string())?;

    let started = Instant::now();
    let mut from_tls = vec![0u8; 64 * 1024];
    let mut from_inner = vec![0u8; 64 * 1024];

    let result = loop {
        match tls.read(&mut from_tls) {
            Ok(0) => break Ok(()),
            Ok(n) => {
                if let Err(e) = inner.write_all(&from_tls[..n]) {
                    break Err(format!("inner write: {e}"));
                }
            }
            Err(ref e) if is_timeout(e) => {}
            Err(e) => break Err(format!("tls read: {e}")),
        }

        if tls.conn.is_handshaking() && started.elapsed() > HANDSHAKE_DEADLINE {
            break Err("TLS handshake timed out".to_string());
        }

        match inner.read(&mut from_inner) {
            Ok(0) => break Ok(()),
            Ok(n) => {
                if let Err(e) = tls.write_all(&from_inner[..n]) {
                    break Err(format!("tls write: {e}"));
                }
                match tls.flush() {
                    Ok(()) => {}
                    Err(ref e) if is_timeout(e) => {}
                    Err(e) => break Err(format!("tls flush: {e}")),
                }
            }
            Err(ref e) if is_timeout(e) => {}
            Err(e) => break Err(format!("inner read: {e}")),
        }
    };

    tls.conn.send_close_notify();
    let _ = tls.flush();
    let _ = tls.sock.shutdown(std::net::Shutdown::Both);
    let _ = inner.shutdown(std::net::Shutdown::Both);
    result
}

/// Read and validate the proxy preamble on a freshly accepted loopback connection.
/// Returns the real peer IP. Must be called before wrapping the stream in a reader.
pub fn read_proxy_preamble(stream: &mut TcpStream) -> Result<IpAddr, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let mut line = Vec::with_capacity(96);
    let mut byte = [0u8; 1];
    loop {
        stream
            .read_exact(&mut byte)
            .map_err(|e| format!("preamble read failed: {e}"))?;
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
        if line.len() > 160 {
            return Err("preamble too long".to_string());
        }
    }
    parse_preamble(&String::from_utf8_lossy(&line), proxy_secret())
}

fn parse_preamble(line: &str, secret: &str) -> Result<IpAddr, String> {
    let mut parts = line.trim().split(' ');
    if parts.next() != Some("PROXY") {
        return Err("missing proxy preamble".to_string());
    }
    let given = parts.next().unwrap_or("");
    if !ct_eq(given.as_bytes(), secret.as_bytes()) {
        return Err("bad proxy secret".to_string());
    }
    parts
        .next()
        .and_then(|ip| ip.parse::<IpAddr>().ok())
        .ok_or_else(|| "bad proxy ip".to_string())
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preamble_accepts_valid_and_rejects_bad_secret() {
        let ip = parse_preamble("PROXY s3cret 192.168.1.20", "s3cret").unwrap();
        assert_eq!(ip, "192.168.1.20".parse::<IpAddr>().unwrap());
        assert!(parse_preamble("PROXY wrong 192.168.1.20", "s3cret").is_err());
        assert!(parse_preamble("HELLO s3cret 192.168.1.20", "s3cret").is_err());
        assert!(parse_preamble("PROXY s3cret notanip", "s3cret").is_err());
    }

    #[test]
    fn limiter_enforces_per_ip_cap() {
        let l = Arc::new(Limiter::default());
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let permits: Vec<_> = (0..MAX_CONNECTIONS_PER_IP).map(|_| l.acquire(ip).unwrap()).collect();
        assert!(l.acquire(ip).is_none());
        assert!(l.acquire("10.0.0.2".parse().unwrap()).is_some());
        drop(permits);
        assert!(l.acquire(ip).is_some());
    }

    #[test]
    fn identity_generation_produces_stable_fingerprint() {
        let certified = rcgen::generate_simple_self_signed(vec!["pzync".to_string()]).unwrap();
        let cert = certified.cert.der().to_vec();
        let key = certified.signing_key.serialize_der();
        let id = identity_from_der(cert.clone(), key).unwrap();
        assert_eq!(id.fingerprint, Sha256::digest(&cert).to_vec());
        assert_eq!(id.fingerprint.len(), 32);
    }

    #[test]
    fn proxy_roundtrip_with_real_tls_client() {
        use rustls::pki_types::ServerName;
        use rustls::{ClientConfig, ClientConnection, RootCertStore};

        let certified = rcgen::generate_simple_self_signed(vec!["pzync".to_string()]).unwrap();
        let cert_der = certified.cert.der().to_vec();
        let id = identity_from_der(cert_der.clone(), certified.signing_key.serialize_der()).unwrap();

        // Inner echo server: checks the preamble, then upper-cases what it reads.
        let inner = TcpListener::bind("127.0.0.1:0").unwrap();
        let inner_port = inner.local_addr().unwrap().port();
        let echo = thread::spawn(move || {
            let (mut s, _) = inner.accept().unwrap();
            let ip = read_proxy_preamble(&mut s).unwrap();
            assert_eq!(ip, "127.0.0.1".parse::<IpAddr>().unwrap());
            let mut buf = [0u8; 5];
            s.read_exact(&mut buf).unwrap();
            s.write_all(&buf.to_ascii_uppercase()).unwrap();
        });

        let public = TcpListener::bind("127.0.0.1:0").unwrap();
        let public_port = public.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let cfg = id.config.clone();
        let proxy = thread::spawn(move || run_proxy(public, inner_port, cfg, stop2));

        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from(cert_der)).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let client_cfg = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let conn = ClientConnection::new(
            Arc::new(client_cfg),
            ServerName::try_from("pzync").unwrap(),
        )
        .unwrap();
        let tcp = TcpStream::connect(("127.0.0.1", public_port)).unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut tls = StreamOwned::new(conn, tcp);
        tls.write_all(b"hello").unwrap();
        let mut out = [0u8; 5];
        tls.read_exact(&mut out).unwrap();
        assert_eq!(&out, b"HELLO");

        echo.join().unwrap();
        stop.store(true, Ordering::Relaxed);
        proxy.join().unwrap();
    }

    #[test]
    fn plaintext_client_is_not_served() {
        let certified = rcgen::generate_simple_self_signed(vec!["pzync".to_string()]).unwrap();
        let id = identity_from_der(
            certified.cert.der().to_vec(),
            certified.signing_key.serialize_der(),
        )
        .unwrap();
        let inner = TcpListener::bind("127.0.0.1:0").unwrap();
        let inner_port = inner.local_addr().unwrap().port();
        let public = TcpListener::bind("127.0.0.1:0").unwrap();
        let public_port = public.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let (stop2, cfg) = (stop.clone(), id.config.clone());
        let proxy = thread::spawn(move || run_proxy(public, inner_port, cfg, stop2));

        let mut c = TcpStream::connect(("127.0.0.1", public_port)).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        c.write_all(b"{\"type\":\"Hello\"}\n").unwrap();
        let mut buf = [0u8; 64];
        // Server answers with a TLS alert or closes; it never echoes protocol data.
        let n = c.read(&mut buf).unwrap_or(0);
        assert!(n == 0 || buf[0] == 0x15, "unexpected plaintext response");

        stop.store(true, Ordering::Relaxed);
        proxy.join().unwrap();
    }

    /// Manual interop helper: serves the proxy so a non-Rust TLS client (the Android
    /// app's PinnedTls) can be tested against it. Run with
    /// `SOCKLINK_INTEROP_PORT=19443 SOCKLINK_INTEROP_FP=/path cargo test --lib interop_server -- --ignored`.
    #[test]
    #[ignore]
    fn interop_server() {
        let port: u16 = std::env::var("SOCKLINK_INTEROP_PORT").unwrap().parse().unwrap();
        let fp_path = std::env::var("SOCKLINK_INTEROP_FP").unwrap();
        let certified = rcgen::generate_simple_self_signed(vec!["pzync".to_string()]).unwrap();
        let id = identity_from_der(
            certified.cert.der().to_vec(),
            certified.signing_key.serialize_der(),
        )
        .unwrap();
        let hex: String = id.fingerprint.iter().map(|b| format!("{b:02x}")).collect();

        let inner = TcpListener::bind("127.0.0.1:0").unwrap();
        let inner_port = inner.local_addr().unwrap().port();
        thread::spawn(move || {
            for stream in inner.incoming() {
                let mut s = stream.unwrap();
                thread::spawn(move || {
                    if read_proxy_preamble(&mut s).is_err() {
                        return;
                    }
                    let mut buf = [0u8; 64];
                    while let Ok(n) = s.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        if s.write_all(&buf[..n].to_ascii_uppercase()).is_err() {
                            break;
                        }
                    }
                });
            }
        });

        let public = TcpListener::bind(("127.0.0.1", port)).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let cfg = id.config.clone();
        thread::spawn(move || run_proxy(public, inner_port, cfg, stop2));
        fs::write(&fp_path, hex).unwrap();
        for _ in 0..120 {
            if std::path::Path::new(&format!("{fp_path}.stop")).exists() {
                break;
            }
            thread::sleep(Duration::from_millis(500));
        }
        stop.store(true, Ordering::Relaxed);
    }
}
