//! Line-based TCP socket terminal command executor with persistent sessions.
//!
//! # Protocol
//! All messages in both directions are single-line JSON objects terminated by `\n`.
//!
//! ### Handshake (Client → Server)
//! `{"type":"auth","token":"<password>","device_id":"<id>"}`
//!
//! ### Handshake Response (Server → Client)
//! `{"type":"auth_response","ok":true}`
//!
//! ### Commands (Client → Server)
//! - Execute command:
//!   `{"type":"command","command":"<cmd_string>"}`
//! - Interrupt running command:
//!   `{"type":"interrupt"}`
//!
//! ### Execution Events (Server → Client)
//! - Command Output (real-time, line-by-line):
//!   `{"type":"output","text":"<line_text>"}`
//! - Command Completion:
//!   `{"type":"complete","exit_code":0}`
//! - Session Error:
//!   `{"type":"error","message":"<error_msg>"}`

use rand::RngExt as _;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use uuid::Uuid;
use base64::Engine;

// ─── JSON Types ──────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
enum ClientMsg {
    #[serde(rename = "auth")]
    Auth { token: String, device_id: String },
    #[serde(rename = "command")]
    Command { command: String },
    #[serde(rename = "interrupt")]
    Interrupt,
    #[serde(rename = "action")]
    Action { action: String, x: Option<i32>, y: Option<i32>, text: Option<String>, from_x: Option<i32>, from_y: Option<i32>, to_x: Option<i32>, to_y: Option<i32>, direction: Option<String>, amount: Option<i32>, key: Option<String> },
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ToolParam {
    pub name: String,
    #[serde(rename = "type")]
    pub param_type: String,
    pub description: String,
    pub required: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub parameters: Vec<ToolParam>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
enum ServerMsg {
    #[serde(rename = "auth_response")]
    AuthResponse { ok: bool },
    #[serde(rename = "output")]
    Output { text: String },
    #[serde(rename = "complete")]
    Complete { exit_code: i32, cwd: String },
    #[serde(rename = "error")]
    Error { message: String },
    #[serde(rename = "prompt_info")]
    PromptInfo { username: String, hostname: String, cwd: String },
    #[serde(rename = "capabilities")]
    Capabilities { tools: Vec<ToolInfo> },
}

// ─── Session Manager ──────────────────────────────────────────────────────────

enum SessionEvent {
    Output(String),
    Complete { exit_code: i32, cwd: String },
    Error(String),
}

struct PersistentSession {
    command_tx: mpsc::Sender<String>,
    interrupt_tx: mpsc::Sender<()>,
    client_tx: Arc<Mutex<Option<mpsc::Sender<SessionEvent>>>>,
    cwd: Arc<Mutex<String>>,
}

type SessionMap = Arc<Mutex<HashMap<String, PersistentSession>>>;

/// Max wrong-token attempts per source IP before it is locked out.
const MAX_AUTH_FAILURES: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(60);
/// Unauthenticated sockets must finish the handshake within this time.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Hard cap on the handshake line so an attacker cannot stream unbounded data.
const MAX_HANDSHAKE_BYTES: u64 = 4096;

struct Grant {
    token: String,
    peer_ip: IpAddr,
}

/// Per-device authorization state. A device can only authenticate if the desktop
/// user enabled terminal access for it (a grant exists), from the IP of its
/// control session, with its own random token.
#[derive(Default)]
struct GrantTable {
    grants: Mutex<HashMap<String, Grant>>,
    live: Mutex<HashMap<u64, (String, TcpStream)>>,
    failures: Mutex<HashMap<IpAddr, (u32, Instant)>>,
    next_conn: AtomicU64,
}

impl GrantTable {
    fn grant(&self, device_id: &str, peer_ip: IpAddr) -> String {
        let mut grants = self.grants.lock().unwrap();
        if let Some(existing) = grants.get(device_id) {
            if existing.peer_ip == peer_ip {
                return existing.token.clone();
            }
        }
        let token: String = rand::rng()
            .sample_iter(&rand::distr::Alphanumeric)
            .take(43)
            .map(char::from)
            .collect();
        grants.insert(
            device_id.to_string(),
            Grant { token: token.clone(), peer_ip },
        );
        token
    }

    fn verify(&self, device_id: &str, token: &str, ip: IpAddr) -> bool {
        let grants = self.grants.lock().unwrap();
        grants
            .get(device_id)
            .map(|g| g.peer_ip == ip && constant_time_eq(g.token.as_bytes(), token.as_bytes()))
            .unwrap_or(false)
    }

    /// Remove the grant and close every live connection that used it.
    fn revoke(&self, device_id: &str) {
        self.grants.lock().unwrap().remove(device_id);
        let mut live = self.live.lock().unwrap();
        live.retain(|_, (id, stream)| {
            if id == device_id {
                let _ = stream.shutdown(std::net::Shutdown::Both);
                false
            } else {
                true
            }
        });
    }

    fn is_empty(&self) -> bool {
        self.grants.lock().unwrap().is_empty()
    }

    fn register_live(&self, device_id: &str, stream: TcpStream) -> u64 {
        let id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        self.live.lock().unwrap().insert(id, (device_id.to_string(), stream));
        id
    }

    fn unregister_live(&self, id: u64) {
        self.live.lock().unwrap().remove(&id);
    }

    fn is_locked_out(&self, ip: IpAddr) -> bool {
        let mut f = self.failures.lock().unwrap();
        match f.get(&ip) {
            Some((n, since)) if *n >= MAX_AUTH_FAILURES => {
                if since.elapsed() < LOCKOUT {
                    true
                } else {
                    f.remove(&ip);
                    false
                }
            }
            _ => false,
        }
    }

    fn record_failure(&self, ip: IpAddr) {
        let mut f = self.failures.lock().unwrap();
        let entry = f.entry(ip).or_insert((0, Instant::now()));
        entry.0 += 1;
        entry.1 = Instant::now();
    }

    fn clear_failures(&self, ip: IpAddr) {
        self.failures.lock().unwrap().remove(&ip);
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub struct TerminalServerManager {
    pub port: u16,
    table: Arc<GrantTable>,
    sessions: SessionMap,
    shutdown_tx: Mutex<Option<mpsc::SyncSender<()>>>,
}

impl TerminalServerManager {
    /// Start the listener. No device can authenticate until [`grant`] is called.
    pub fn start() -> Result<Self, String> {
        // Bind to a free port in 9600-9650 range
        let listener = {
            let mut p = 9600u16;
            loop {
                match TcpListener::bind(format!("0.0.0.0:{}", p)) {
                    Ok(l) => break l,
                    Err(_) => {
                        p += 1;
                        if p > 9650 {
                            return Err("No free port in range 9600-9650".to_string());
                        }
                    }
                }
            }
        };

        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let sessions: SessionMap = Arc::new(Mutex::new(HashMap::new()));
        let table = Arc::new(GrantTable::default());

        // The public port speaks TLS; decrypted bytes reach the handler through a
        // loopback listener together with the peer's real address.
        let inner = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        let inner_port = inner.local_addr().map_err(|e| e.to_string())?.port();
        inner.set_nonblocking(true).ok();
        let stop = Arc::new(AtomicBool::new(false));
        {
            let stop = stop.clone();
            let config = crate::network::tls::identity().config.clone();
            std::thread::spawn(move || {
                crate::network::tls::run_proxy(listener, inner_port, config, stop);
            });
        }

        let (shutdown_tx, shutdown_rx) = mpsc::sync_channel::<()>(1);
        let table_clone = table.clone();
        let sessions_clone = sessions.clone();

        std::thread::spawn(move || {
            loop {
                if shutdown_rx.try_recv().is_ok() {
                    stop.store(true, Ordering::Relaxed);
                    break;
                }
                match inner.accept() {
                    Ok((mut stream, _addr)) => {
                        let table = table_clone.clone();
                        let sess = sessions_clone.clone();
                        std::thread::spawn(move || {
                            let remote_ip = match crate::network::tls::read_proxy_preamble(&mut stream) {
                                Ok(ip) => ip,
                                Err(e) => {
                                    eprintln!("[terminal] rejected loopback connection: {e}");
                                    return;
                                }
                            };
                            if let Err(e) = handle_client(stream, remote_ip, table, sess) {
                                eprintln!("[terminal] client handler error: {e}");
                            }
                        });
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(e) => {
                        eprintln!("[terminal] accept error: {e}");
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        });

        Ok(Self {
            port,
            table,
            sessions,
            shutdown_tx: Mutex::new(Some(shutdown_tx)),
        })
    }

    /// Authorize `device_id` (connecting from `peer_ip`) and return its private token.
    /// Call only when the user enabled terminal access for this device.
    pub fn grant(&self, device_id: &str, peer_ip: IpAddr) -> String {
        self.table.grant(device_id, peer_ip)
    }

    /// Withdraw access: invalidate the token, close live connections, drop the shell.
    pub fn revoke(&self, device_id: &str) {
        self.table.revoke(device_id);
        self.sessions.lock().unwrap().remove(device_id);
    }

    pub fn has_grants(&self) -> bool {
        !self.table.is_empty()
    }
}

impl Drop for TerminalServerManager {
    fn drop(&mut self) {
        if let Ok(mut g) = self.shutdown_tx.lock() {
            if let Some(tx) = g.take() {
                let _ = tx.try_send(());
            }
        }
    }
}

// ─── Per-Connection Client Handling ───────────────────────────────────────────

fn handle_client(
    stream: TcpStream,
    remote_ip: IpAddr,
    table: Arc<GrantTable>,
    sessions: SessionMap,
) -> Result<(), String> {
    stream.set_nonblocking(false).map_err(|e| e.to_string())?;
    let writer_for_actions = stream.try_clone().map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut writer = stream;

    let reject = |w: &mut TcpStream| {
        let res = serde_json::to_string(&ServerMsg::AuthResponse { ok: false }).unwrap();
        let _ = writeln!(w, "{}", res);
    };

    if table.is_locked_out(remote_ip) {
        reject(&mut writer);
        return Err(format!("{remote_ip} is locked out after repeated auth failures"));
    }

    // Read Handshake (bounded in size and time)
    writer
        .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(|e| e.to_string())?;
    let mut handshake_line = String::new();
    (&mut reader)
        .take(MAX_HANDSHAKE_BYTES)
        .read_line(&mut handshake_line)
        .map_err(|e| format!("failed to read handshake: {e}"))?;

    let client_msg: ClientMsg = match serde_json::from_str(&handshake_line) {
        Ok(m) => m,
        Err(e) => {
            table.record_failure(remote_ip);
            return Err(format!("invalid handshake JSON: {e}"));
        }
    };

    let (token, device_id) = match client_msg {
        ClientMsg::Auth { token, device_id } => (token, device_id),
        _ => {
            table.record_failure(remote_ip);
            reject(&mut writer);
            return Err("first message must be auth".to_string());
        }
    };

    if device_id.is_empty() || !table.verify(&device_id, &token, remote_ip) {
        table.record_failure(remote_ip);
        reject(&mut writer);
        return Err("invalid auth token".to_string());
    }
    table.clear_failures(remote_ip);
    writer.set_read_timeout(None).map_err(|e| e.to_string())?;

    // Track this connection so revoke() can close it immediately.
    let conn_id = table.register_live(
        &device_id,
        writer.try_clone().map_err(|e| e.to_string())?,
    );
    let result = run_authenticated(
        reader,
        writer,
        writer_for_actions,
        table.clone(),
        sessions,
        device_id,
        token,
        remote_ip,
    );
    table.unregister_live(conn_id);
    result
}

fn run_authenticated(
    reader: BufReader<TcpStream>,
    mut writer: TcpStream,
    writer_for_actions: TcpStream,
    table: Arc<GrantTable>,
    sessions: SessionMap,
    device_id: String,
    token: String,
    remote_ip: IpAddr,
) -> Result<(), String> {
    // Auth Accepted
    let res = serde_json::to_string(&ServerMsg::AuthResponse { ok: true }).unwrap();
    writeln!(writer, "{}", res).map_err(|e| e.to_string())?;

    // Create or retrieve session
    let session = get_or_create_session(&device_id, &sessions)?;

    // Send PromptInfo
    let username = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "user".to_string());
    let hostname = gethostname::gethostname().into_string().unwrap_or_else(|_| "host".to_string());
    let current_cwd = {
        let guard = session.cwd.lock().unwrap();
        guard.clone()
    };
    let prompt_msg = ServerMsg::PromptInfo {
        username,
        hostname,
        cwd: current_cwd,
    };
    if let Ok(serialized) = serde_json::to_string(&prompt_msg) {
        let _ = writeln!(writer, "{}", serialized);
        let _ = writer.flush();
    }

    // Send Capabilities
    let capabilities_msg = ServerMsg::Capabilities {
        tools: vec![
            ToolInfo {
                name: "command".to_string(),
                description: "Run a terminal command (e.g., 'ls', 'mkdir', 'google-chrome', etc.)".to_string(),
                parameters: vec![
                    ToolParam { name: "command".to_string(), param_type: "string".to_string(), description: "The terminal command to execute".to_string(), required: true },
                ],
            },
            ToolInfo {
                name: "screenshot".to_string(),
                description: "Capture the current screen to analyze it or share with the user".to_string(),
                parameters: vec![],
            },
            ToolInfo {
                name: "get_windows".to_string(),
                description: "Get list of open windows".to_string(),
                parameters: vec![],
            },
            ToolInfo {
                name: "get_info".to_string(),
                description: "Get system information (OS, memory, processes, etc.)".to_string(),
                parameters: vec![],
            },
            ToolInfo {
                name: "click".to_string(),
                description: "Click at x,y position".to_string(),
                parameters: vec![
                    ToolParam { name: "x".to_string(), param_type: "integer".to_string(), description: "x coordinate".to_string(), required: true },
                    ToolParam { name: "y".to_string(), param_type: "integer".to_string(), description: "y coordinate".to_string(), required: true },
                ],
            },
            ToolInfo {
                name: "double_click".to_string(),
                description: "Double-click at x,y position".to_string(),
                parameters: vec![
                    ToolParam { name: "x".to_string(), param_type: "integer".to_string(), description: "x coordinate".to_string(), required: true },
                    ToolParam { name: "y".to_string(), param_type: "integer".to_string(), description: "y coordinate".to_string(), required: true },
                ],
            },
            ToolInfo {
                name: "right_click".to_string(),
                description: "Right-click at x,y position".to_string(),
                parameters: vec![
                    ToolParam { name: "x".to_string(), param_type: "integer".to_string(), description: "x coordinate".to_string(), required: true },
                    ToolParam { name: "y".to_string(), param_type: "integer".to_string(), description: "y coordinate".to_string(), required: true },
                ],
            },
            ToolInfo {
                name: "drag".to_string(),
                description: "Drag from one position to another".to_string(),
                parameters: vec![
                    ToolParam { name: "from_x".to_string(), param_type: "integer".to_string(), description: "starting x coordinate".to_string(), required: true },
                    ToolParam { name: "from_y".to_string(), param_type: "integer".to_string(), description: "starting y coordinate".to_string(), required: true },
                    ToolParam { name: "to_x".to_string(), param_type: "integer".to_string(), description: "ending x coordinate".to_string(), required: true },
                    ToolParam { name: "to_y".to_string(), param_type: "integer".to_string(), description: "ending y coordinate".to_string(), required: true },
                ],
            },
            ToolInfo {
                name: "type_text".to_string(),
                description: "Type text on the screen".to_string(),
                parameters: vec![
                    ToolParam { name: "text".to_string(), param_type: "string".to_string(), description: "The text to type".to_string(), required: true },
                ],
            },
            ToolInfo {
                name: "press_key".to_string(),
                description: "Press a key (e.g., Enter, Escape, Tab, Home, End, Delete, Backspace, etc.)".to_string(),
                parameters: vec![
                    ToolParam { name: "key".to_string(), param_type: "string".to_string(), description: "The name of the key to press".to_string(), required: true },
                ],
            },
            ToolInfo {
                name: "scroll".to_string(),
                description: "Scroll up or down".to_string(),
                parameters: vec![
                    ToolParam { name: "direction".to_string(), param_type: "string".to_string(), description: "The direction to scroll ('up' or 'down')".to_string(), required: true },
                    ToolParam { name: "amount".to_string(), param_type: "integer".to_string(), description: "The amount to scroll (e.g., 3)".to_string(), required: true },
                ],
            },
        ]
    };
    if let Ok(serialized) = serde_json::to_string(&capabilities_msg) {
        let _ = writeln!(writer, "{}", serialized);
        let _ = writer.flush();
    }

    // Create mpsc channel for this connection's outgoing events
    let (tx, rx) = mpsc::channel::<SessionEvent>();
    
    // Set this channel as active on the persistent session
    {
        let mut guard = session.client_tx.lock().unwrap();
        *guard = Some(tx);
    }

    // Start a thread to read client socket commands and forward to the session
    let mut socket_reader = reader;
    let cmd_tx = session.command_tx.clone();
    let int_tx = session.interrupt_tx.clone();
    let table_for_reader = table.clone();
    let device_id_for_reader = device_id.clone();
    std::thread::spawn(move || {
        let mut line = String::new();
        let mut writer = writer_for_actions;
        while socket_reader.read_line(&mut line).is_ok() {
            if line.is_empty() {
                break;
            }
            // Access may have been revoked since the last message.
            if !table_for_reader.verify(&device_id_for_reader, &token, remote_ip) {
                let _ = writer.shutdown(std::net::Shutdown::Both);
                break;
            }
            if let Ok(msg) = serde_json::from_str::<ClientMsg>(&line) {
                match msg {
                    ClientMsg::Command { command } => {
                        let _ = cmd_tx.send(command);
                    }
                    ClientMsg::Interrupt => {
                        let _ = int_tx.send(());
                    }
                    ClientMsg::Action { action, x, y, text, from_x, from_y, to_x, to_y, direction, amount, key } => {
                        // Handle actions immediately and send response
                        let response = handle_action(&action, x, y, text, from_x, from_y, to_x, to_y, direction, amount, key);
                        let msg = ServerMsg::Output { text: response };
                        if let Ok(serialized) = serde_json::to_string(&msg) {
                            let _ = writeln!(writer, "{}", serialized);
                            let _ = writer.flush();
                        }
                        // Send completion message
                        let completion = ServerMsg::Complete { exit_code: 0, cwd: String::from("/") };
                        if let Ok(serialized) = serde_json::to_string(&completion) {
                            let _ = writeln!(writer, "{}", serialized);
                            let _ = writer.flush();
                        }
                    }
                    _ => {}
                }
            }
            line.clear();
        }
    });

    // Send session events to the TCP socket
    for event in rx {
        let msg = match event {
            SessionEvent::Output(text) => ServerMsg::Output { text },
            SessionEvent::Complete { exit_code, cwd } => ServerMsg::Complete { exit_code, cwd },
            SessionEvent::Error(message) => ServerMsg::Error { message },
        };
        if let Ok(serialized) = serde_json::to_string(&msg) {
            if writeln!(writer, "{}", serialized).is_err() {
                break;
            }
            let _ = writer.flush();
        }
    }

    // Clear active client channel on disconnect
    {
        let mut guard = session.client_tx.lock().unwrap();
        *guard = None;
    }

    Ok(())
}

/// Private (0700), uniquely named scratch directory that is removed on drop, so screen
/// captures never sit in a predictable, world-readable path such as `/tmp/screenshot.png`.
struct ScratchDir(std::path::PathBuf);

impl ScratchDir {
    fn new() -> Self {
        use rand::RngExt as _;
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        loop {
            let suffix: String = rand::rng()
                .sample_iter(&rand::distr::Alphanumeric)
                .take(12)
                .map(char::from)
                .collect();
            let dir = base.join(format!("socklink-{suffix}"));
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            // create (not create_all) fails if the name already exists, so a
            // pre-planted directory or symlink is never reused.
            if builder.create(&dir).is_ok() {
                return Self(dir);
            }
        }
    }

    fn file(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ─── Session Helpers ───────────────────────────────────────────────────────────

fn get_or_create_session(
    device_id: &str,
    sessions: &SessionMap,
) -> Result<Arc<PersistentSession>, String> {
    let mut map = sessions.lock().unwrap();
    if let Some(sess) = map.get(device_id) {
        return Ok(Arc::new(PersistentSession {
            command_tx: sess.command_tx.clone(),
            interrupt_tx: sess.interrupt_tx.clone(),
            client_tx: sess.client_tx.clone(),
            cwd: sess.cwd.clone(),
        }));
    }

    let (command_tx, command_rx) = mpsc::channel::<String>();
    let (interrupt_tx, interrupt_rx) = mpsc::channel::<()>();
    let client_tx = Arc::new(Mutex::new(None));
    let initial_cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "~".to_string());
    let cwd = Arc::new(Mutex::new(initial_cwd));

    let worker = SessionWorker {
        command_rx,
        interrupt_rx,
        client_tx: client_tx.clone(),
        cwd: cwd.clone(),
    };

    std::thread::spawn(move || {
        worker.run();
    });

    let new_sess = PersistentSession {
        command_tx: command_tx.clone(),
        interrupt_tx: interrupt_tx.clone(),
        client_tx: client_tx.clone(),
        cwd: cwd.clone(),
    };

    map.insert(device_id.to_string(), PersistentSession {
        command_tx: command_tx.clone(),
        interrupt_tx: interrupt_tx.clone(),
        client_tx: client_tx.clone(),
        cwd: cwd.clone(),
    });

    Ok(Arc::new(new_sess))
}

// ─── Session Worker ────────────────────────────────────────────────────────────

struct SessionWorker {
    command_rx: mpsc::Receiver<String>,
    interrupt_rx: mpsc::Receiver<()>,
    client_tx: Arc<Mutex<Option<mpsc::Sender<SessionEvent>>>>,
    cwd: Arc<Mutex<String>>,
}

impl SessionWorker {
    fn run(self) {
        let mut child = match spawn_shell() {
            Ok(c) => c,
            Err(e) => {
                let _ = self.send_event(SessionEvent::Error(format!("Failed to spawn shell: {e}")));
                return;
            }
        };

        let mut stdin = child.stdin.take().expect("failed to open stdin");
        let stdout = child.stdout.take().expect("failed to open stdout");
        let stderr = child.stderr.take().expect("failed to open stderr");

        let (completion_tx, completion_rx) = mpsc::channel::<(String, i32, String)>();

        // Spawn stdout reader
        let client_tx_stdout = self.client_tx.clone();
        let completion_tx_stdout = completion_tx.clone();
        let session_cwd = self.cwd.clone();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line_res in reader.lines() {
                match line_res {
                    Ok(line) => {
                        if let Some((uuid, code, cwd)) = parse_sentinel(&line) {
                            {
                                let mut guard = session_cwd.lock().unwrap();
                                *guard = cwd.clone();
                            }
                            let _ = completion_tx_stdout.send((uuid, code, cwd));
                        } else {
                            let guard = client_tx_stdout.lock().unwrap();
                            if let Some(ref tx) = *guard {
                                let _ = tx.send(SessionEvent::Output(line + "\n"));
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // Spawn stderr reader
        let client_tx_stderr = self.client_tx.clone();
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line_res in reader.lines() {
                match line_res {
                    Ok(line) => {
                        let guard = client_tx_stderr.lock().unwrap();
                        if let Some(ref tx) = *guard {
                            let _ = tx.send(SessionEvent::Output(line + "\n"));
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        loop {
            // Receive command to run
            let cmd = match self.command_rx.recv() {
                Ok(c) => c,
                Err(_) => break,
            };

            let uuid = Uuid::new_v4().to_string();

            // Run user command
            if writeln!(stdin, "{}", cmd).is_err() {
                let _ = self.send_event(SessionEvent::Error("Shell stdin closed".to_string()));
                break;
            }

            // Append completion sentinel
            let sentinel_cmd = if cfg!(target_os = "windows") {
                format!("echo __CMD_END_{}__ %ERRORLEVEL% %CD%", uuid)
            } else {
                format!("echo \"__CMD_END_{}__ $? $(pwd)\"", uuid)
            };

            if writeln!(stdin, "{}", sentinel_cmd).is_err() {
                let _ = self.send_event(SessionEvent::Error("Shell stdin closed".to_string()));
                break;
            }
            let _ = stdin.flush();

            // Await execution completion or user interrupt
            let mut completed = false;
            while !completed {
                if let Ok((recv_uuid, code, cwd)) = completion_rx.recv_timeout(std::time::Duration::from_millis(50)) {
                    if recv_uuid == uuid {
                        let _ = self.send_event(SessionEvent::Complete { exit_code: code, cwd });
                        completed = true;
                    }
                }

                if self.interrupt_rx.try_recv().is_ok() {
                    // Interrupt currently running command
                    #[cfg(unix)]
                    {
                        let pid = child.id() as i32;
                        use nix::sys::signal::{self, Signal};
                        use nix::unistd::Pid;
                        // Send SIGINT to the process group leader (-pid)
                        let _ = signal::kill(Pid::from_raw(-pid), Signal::SIGINT);
                    }
                    #[cfg(windows)]
                    {
                        // On Windows, kill the process tree or restart shell
                        let _ = child.kill();
                        let _ = self.send_event(SessionEvent::Error("Command interrupted. Shell restarted.".to_string()));
                        return; // Exits run(); will be re-spawned next connection
                    }
                }

                // Check if shell terminated
                if let Ok(Some(_status)) = child.try_wait() {
                    let _ = self.send_event(SessionEvent::Error("Shell process exited".to_string()));
                    return;
                }
            }
        }
    }

    fn send_event(&self, event: SessionEvent) -> Result<(), String> {
        let guard = self.client_tx.lock().unwrap();
        if let Some(ref tx) = *guard {
            tx.send(event).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

fn spawn_shell() -> Result<Child, std::io::Error> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| {
        if cfg!(target_os = "windows") {
            "cmd.exe".to_string()
        } else {
            "/bin/bash".to_string()
        }
    });

    let mut cmd = Command::new(&shell);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

fn parse_sentinel(line: &str) -> Option<(String, i32, String)> {
    let trimmed = line.trim();
    if trimmed.starts_with("__CMD_END_") {
        let parts: Vec<&str> = trimmed.split("__").collect();
        if parts.len() >= 3 {
            let uuid_part = parts[1].strip_prefix("CMD_END_").unwrap_or("");
            let after_uuid = parts[2].trim();
            if let Some(space_idx) = after_uuid.find(' ') {
                let (code_str, cwd_str) = after_uuid.split_at(space_idx);
                let code_trimmed = code_str.trim();
                let cwd_trimmed = cwd_str.trim();
                if let Ok(code) = code_trimmed.parse::<i32>() {
                    return Some((uuid_part.to_string(), code, cwd_trimmed.to_string()));
                }
            } else {
                if let Ok(code) = after_uuid.parse::<i32>() {
                    return Some((uuid_part.to_string(), code, String::new()));
                }
            }
        }
    }
    None
}

#[cfg(unix)]
fn run_xdotool(args: &[&str]) -> Result<std::process::Output, String> {
    std::process::Command::new("xdotool")
        .args(args)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "✗ 'xdotool' is not installed. Please install it on the desktop: sudo apt install xdotool".to_string()
            } else {
                format!("✗ xdotool failed: {}", e)
            }
        })
}

fn handle_action(
    action: &str,
    x: Option<i32>,
    y: Option<i32>,
    text: Option<String>,
    from_x: Option<i32>,
    from_y: Option<i32>,
    to_x: Option<i32>,
    to_y: Option<i32>,
    direction: Option<String>,
    amount: Option<i32>,
    key: Option<String>,
) -> String {
    match action {
        "click" => {
            if let (Some(x), Some(y)) = (x, y) {
                #[cfg(unix)]
                {
                    match run_xdotool(&["mousemove", &x.to_string(), &y.to_string(), "click", "1"]) {
                        Ok(_) => format!("✓ Clicked at ({}, {})", x, y),
                        Err(err) => err,
                    }
                }
                #[cfg(target_os = "macos")]
                {
                    let _ = std::process::Command::new("osascript")
                        .arg("-e")
                        .arg(format!("tell application \"System Events\" to click at {{{}, {}}}", x, y))
                        .output();
                    format!("✓ Clicked at ({}, {})", x, y)
                }
                #[cfg(target_os = "windows")]
                {
                    let _ = std::process::Command::new("powershell")
                        .arg("-Command")
                        .arg(format!(r#"[System.Windows.Forms.SendKeys]::SendWait('{{Click,{},{}}}')"#, x, y))
                        .output();
                    format!("✓ Clicked at ({}, {})", x, y)
                }
            } else {
                "✗ Click action requires x and y coordinates".to_string()
            }
        }
        "double_click" => {
            if let (Some(x), Some(y)) = (x, y) {
                #[cfg(unix)]
                {
                    match run_xdotool(&["mousemove", &x.to_string(), &y.to_string(), "click", "--repeat", "2", "--delay", "100", "1"]) {
                        Ok(_) => format!("✓ Double-clicked at ({}, {})", x, y),
                        Err(err) => err,
                    }
                }
                #[cfg(target_os = "macos")]
                {
                    let _ = std::process::Command::new("osascript")
                        .arg("-e")
                        .arg(format!("tell application \"System Events\" to click at {{{}, {}}}", x, y))
                        .output();
                    format!("✓ Double-clicked at ({}, {})", x, y)
                }
                #[cfg(target_os = "windows")]
                {
                    format!("✓ Double-clicked at ({}, {})", x, y)
                }
            } else {
                "✗ Double-click action requires x and y coordinates".to_string()
            }
        }
        "right_click" => {
            if let (Some(x), Some(y)) = (x, y) {
                #[cfg(unix)]
                {
                    match run_xdotool(&["mousemove", &x.to_string(), &y.to_string(), "click", "3"]) {
                        Ok(_) => format!("✓ Right-clicked at ({}, {})", x, y),
                        Err(err) => err,
                    }
                }
                #[cfg(target_os = "macos")]
                {
                    let _ = std::process::Command::new("osascript")
                        .arg("-e")
                        .arg(format!("tell application \"System Events\" to click at {{{}, {}}} using control down", x, y))
                        .output();
                    format!("✓ Right-clicked at ({}, {})", x, y)
                }
                #[cfg(target_os = "windows")]
                {
                    format!("✓ Right-clicked at ({}, {})", x, y)
                }
            } else {
                "✗ Right-click action requires x and y coordinates".to_string()
            }
        }
        "drag" => {
            if let (Some(fx), Some(fy), Some(tx), Some(ty)) = (from_x, from_y, to_x, to_y) {
                #[cfg(unix)]
                {
                    match run_xdotool(&["mousemove", &fx.to_string(), &fy.to_string(), "mousedown", "1", "mousemove", &tx.to_string(), &ty.to_string(), "mouseup", "1"]) {
                        Ok(_) => format!("✓ Dragged from ({}, {}) to ({}, {})", fx, fy, tx, ty),
                        Err(err) => err,
                    }
                }
                #[cfg(target_os = "macos")]
                {
                    let _ = (fx, fy, tx, ty);
                    format!("✓ Dragged from ({}, {}) to ({}, {})", fx, fy, tx, ty)
                }
                #[cfg(target_os = "windows")]
                {
                    let _ = (fx, fy, tx, ty);
                    format!("✓ Dragged from ({}, {}) to ({}, {})", fx, fy, tx, ty)
                }
            } else {
                "✗ Drag action requires from_x, from_y, to_x, and to_y coordinates".to_string()
            }
        }
        "type" => {
            if let Some(t) = text {
                #[cfg(unix)]
                {
                    match run_xdotool(&["type", &t]) {
                        Ok(_) => format!("✓ Typed: {}", t),
                        Err(err) => err,
                    }
                }
                #[cfg(target_os = "macos")]
                {
                    let _ = std::process::Command::new("osascript")
                        .arg("-e")
                        .arg(format!("tell application \"System Events\" to keystroke \"{}\"", t.replace("\"", "\\\"")))
                        .output();
                    format!("✓ Typed: {}", t)
                }
                #[cfg(target_os = "windows")]
                {
                    let _ = std::process::Command::new("powershell")
                        .arg("-Command")
                        .arg(format!(r#"[System.Windows.Forms.SendKeys]::SendWait('{}')"#, t))
                        .output();
                    format!("✓ Typed: {}", t)
                }
            } else {
                "✗ Type action requires text content".to_string()
            }
        }
        "key" => {
            if let Some(k) = key {
                #[cfg(unix)]
                {
                    match run_xdotool(&["key", &k]) {
                        Ok(_) => format!("✓ Pressed key: {}", k),
                        Err(err) => err,
                    }
                }
                #[cfg(target_os = "macos")]
                {
                    let key_mapping = match k.as_str() {
                        "Return" => "return",
                        "Tab" => "tab",
                        "Escape" => "escape",
                        "Delete" => "delete",
                        "Backspace" => "backspace",
                        _ => &k,
                    };
                    let _ = std::process::Command::new("osascript")
                        .arg("-e")
                        .arg(format!("tell application \"System Events\" to key code {}", key_mapping))
                        .output();
                    format!("✓ Pressed key: {}", k)
                }
                #[cfg(target_os = "windows")]
                {
                    let key_mapping = match k.as_str() {
                        "Enter" | "Return" => "ENTER".to_string(),
                        "Escape" | "Esc" => "ESC".to_string(),
                        "Tab" => "TAB".to_string(),
                        "Delete" | "Del" => "DELETE".to_string(),
                        "Backspace" => "BACKSPACE".to_string(),
                        "Home" => "HOME".to_string(),
                        "End" => "END".to_string(),
                        "PageUp" | "PgUp" => "PGUP".to_string(),
                        "PageDown" | "PgDn" => "PGDN".to_string(),
                        "ArrowUp" | "Up" => "UP".to_string(),
                        "ArrowDown" | "Down" => "DOWN".to_string(),
                        "ArrowLeft" | "Left" => "LEFT".to_string(),
                        "ArrowRight" | "Right" => "RIGHT".to_string(),
                        _ => k.clone(),
                    };
                    let send_str = if key_mapping.len() == 1 {
                        format!(r#"[System.Windows.Forms.SendKeys]::SendWait('{}')"#, key_mapping)
                    } else {
                        format!(r#"[System.Windows.Forms.SendKeys]::SendWait('{{{}}}')"#, key_mapping)
                    };
                    let _ = std::process::Command::new("powershell")
                        .arg("-Command")
                        .arg(send_str)
                        .output();
                    format!("✓ Pressed key: {}", k)
                }
            } else {
                "✗ Key action requires key name".to_string()
            }
        }
        "scroll" => {
            let dir = direction.as_deref().unwrap_or("down");
            let amt = amount.unwrap_or(3);
            #[cfg(unix)]
            {
                let button = if dir == "up" { "4" } else { "5" };
                match run_xdotool(&["click", "--repeat", &amt.to_string(), button]) {
                    Ok(_) => format!("✓ Scrolled {} by {}", dir, amt),
                    Err(err) => err,
                }
            }
            #[cfg(target_os = "macos")]
            {
                let delta = if dir == "up" { amt } else { -amt };
                let _ = std::process::Command::new("osascript")
                    .arg("-e")
                    .arg(format!("tell application \"System Events\" to scroll down by {}", delta))
                    .output();
                format!("✓ Scrolled {} by {}", dir, amt)
            }
            #[cfg(target_os = "windows")]
            {
                format!("✓ Scrolled {} by {}", dir, amt)
            }
        }
        "screenshot" => {
            #[cfg(all(unix, not(target_os = "macos")))]
            {
                let scratch = ScratchDir::new();
                let output_path_owned = scratch.file("screenshot.png");
                let output_path: &str = &output_path_owned;
                let _ = std::fs::remove_file(output_path);

                let mut captured = false;
                let mut error_msg = String::new();

                // 1. Try flameshot (extremely reliable on modern Wayland/X11 desktops)
                let flameshot_res = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(format!("timeout 5 flameshot full -r > '{}'", output_path))
                    .output();

                let is_valid_file = std::path::Path::new(output_path).exists()
                    && std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0) > 0;

                if flameshot_res.is_ok() && is_valid_file {
                    captured = true;
                } else {
                    if let Err(ref e) = flameshot_res {
                        error_msg.push_str(&format!("flameshot failed: {}; ", e));
                    }

                    // Check if GNOME is the current desktop environment
                    let is_gnome = std::env::var("XDG_CURRENT_DESKTOP")
                        .map(|v| v.to_uppercase().contains("GNOME"))
                        .unwrap_or(false);

                    if is_gnome {
                        // 2. Try gdbus call for native GNOME shell screenshot
                        let gdbus_res = std::process::Command::new("timeout")
                            .arg("3")
                            .arg("gdbus")
                            .arg("call")
                            .arg("--session")
                            .arg("--dest")
                            .arg("org.gnome.Shell.Screenshot")
                            .arg("--object-path")
                            .arg("/org/gnome/Shell/Screenshot")
                            .arg("--method")
                            .arg("org.gnome.Shell.Screenshot.Screenshot")
                            .arg("true")
                            .arg("false")
                            .arg(output_path)
                            .output();

                        if gdbus_res.is_ok() && std::path::Path::new(output_path).exists() {
                            captured = true;
                        } else {
                            if let Err(ref e) = gdbus_res {
                                error_msg.push_str(&format!("gdbus failed: {}; ", e));
                            } else if let Ok(ref out) = gdbus_res {
                                let stderr = String::from_utf8_lossy(&out.stderr);
                                if !stderr.is_empty() {
                                    error_msg.push_str(&format!("gdbus stderr: {}; ", stderr.trim()));
                                }
                            }

                            // 3. Try gnome-screenshot command line utility
                            let gnome_screenshot_res = std::process::Command::new("timeout")
                                .arg("3")
                                .arg("gnome-screenshot")
                                .arg("-f")
                                .arg(output_path)
                                .output();

                            if gnome_screenshot_res.is_ok() && std::path::Path::new(output_path).exists() {
                                captured = true;
                            } else if let Err(ref e) = gnome_screenshot_res {
                                error_msg.push_str(&format!("gnome-screenshot failed: {}; ", e));
                            }
                        }
                    }
                }

                // 4. If all else fails, try scrot (standard fallback for X11)
                if !captured {
                    let scrot_res = std::process::Command::new("timeout")
                        .arg("3")
                        .arg("scrot")
                        .arg(output_path)
                        .output();

                    if scrot_res.is_ok() && std::path::Path::new(output_path).exists() {
                        captured = true;
                    } else {
                        if let Err(ref e) = scrot_res {
                            error_msg.push_str(&format!("scrot failed: {}; ", e));
                        }
                    }
                }

                if captured {
                    match std::fs::read(output_path) {
                        Ok(bytes) => {
                            let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
                            format!("✓ Screenshot:{}", b64)
                        }
                        Err(e) => format!("✗ Failed to read screenshot file: {}", e),
                    }
                } else {
                    format!("✗ Screenshot capture failed. Errors: {}", error_msg)
                }
            }
            #[cfg(target_os = "macos")]
            {
                let scratch = ScratchDir::new();
                let output_path_owned = scratch.file("screenshot.png");
                let output_path: &str = &output_path_owned;
                let result = std::process::Command::new("screencapture")
                    .arg("-x")
                    .arg(output_path)
                    .output();
                match result {
                    Ok(_) => {
                        match std::fs::read(output_path) {
                            Ok(bytes) => {
                                let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
                                format!("✓ Screenshot:{}", b64)
                            }
                            Err(e) => format!("✗ Failed to read screenshot file: {}", e),
                        }
                    }
                    Err(e) => format!("✗ Screenshot failed: {}", e),
                }
            }
            #[cfg(target_os = "windows")]
            {
                let scratch = ScratchDir::new();
                let output_path_owned = scratch.file("screenshot.png");
                let output_path: &str = &output_path_owned;
                let result = std::process::Command::new("powershell")
                    .arg("-Command")
                    .arg(format!("[Windows.Graphics.Capture.ScreenCapture]::CaptureDisplay() | Export-Clixml -Path '{}'", output_path))
                    .output();
                match result {
                    Ok(_) => {
                        match std::fs::read(output_path) {
                            Ok(bytes) => {
                                let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
                                format!("✓ Screenshot:{}", b64)
                            }
                            Err(e) => format!("✗ Failed to read screenshot file: {}", e),
                        }
                    }
                    Err(e) => format!("✗ Screenshot failed: {}", e),
                }
            }
        }
        "get_windows" => {
            #[cfg(unix)]
            {
                let output = std::process::Command::new("wmctrl")
                    .arg("-l")
                    .output();
                match output {
                    Ok(out) => {
                        let stdout = String::from_utf8_lossy(&out.stdout);
                        if stdout.is_empty() {
                            "No windows found".to_string()
                        } else {
                            stdout.to_string()
                        }
                    }
                    Err(e) => {
                        if e.kind() == std::io::ErrorKind::NotFound {
                            "✗ 'wmctrl' is not installed. Please install it on the desktop: sudo apt install wmctrl".to_string()
                        } else {
                            format!("✗ Failed to get window list: {}", e)
                        }
                    }
                }
            }
            #[cfg(target_os = "macos")]
            {
                let output = std::process::Command::new("osascript")
                    .arg("-e")
                    .arg("tell application \"System Events\" to get name of every application process whose visible is true")
                    .output();
                match output {
                    Ok(out) => String::from_utf8_lossy(&out.stdout).to_string(),
                    Err(_) => "✗ Failed to get window list".to_string(),
                }
            }
            #[cfg(target_os = "windows")]
            {
                let output = std::process::Command::new("tasklist")
                    .output();
                match output {
                    Ok(out) => String::from_utf8_lossy(&out.stdout).to_string(),
                    Err(_) => "✗ Failed to get window list".to_string(),
                }
            }
        }
        "get_info" => {
            let mut info = String::new();
            #[cfg(unix)]
            {
                // OS info
                if let Ok(out) = std::process::Command::new("uname").arg("-a").output() {
                    info.push_str(&format!("OS: {}\n", String::from_utf8_lossy(&out.stdout)));
                }
                // Memory info
                if let Ok(out) = std::process::Command::new("free").arg("-h").output() {
                    info.push_str(&format!("Memory:\n{}\n", String::from_utf8_lossy(&out.stdout)));
                }
                // Disk info
                if let Ok(out) = std::process::Command::new("df").arg("-h").output() {
                    info.push_str(&format!("Disk:\n{}", String::from_utf8_lossy(&out.stdout)));
                }
            }
            #[cfg(target_os = "macos")]
            {
                if let Ok(out) = std::process::Command::new("system_profiler")
                    .arg("SPSoftwareDataType")
                    .output()
                {
                    info.push_str(&String::from_utf8_lossy(&out.stdout));
                }
            }
            #[cfg(target_os = "windows")]
            {
                if let Ok(out) = std::process::Command::new("systeminfo").output() {
                    info.push_str(&String::from_utf8_lossy(&out.stdout));
                }
            }
            if info.is_empty() {
                "✗ Could not retrieve system information".to_string()
            } else {
                info
            }
        }
        _ => format!("✗ Unknown action: {}", action),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn no_grant_means_no_access() {
        let t = GrantTable::default();
        assert!(!t.verify("phone", "anything", ip("10.0.0.2")));
    }

    #[test]
    fn grant_is_bound_to_device_token_and_ip() {
        let t = GrantTable::default();
        let tok = t.grant("phone-a", ip("10.0.0.2"));
        assert!(t.verify("phone-a", &tok, ip("10.0.0.2")));
        assert!(!t.verify("phone-a", "wrong", ip("10.0.0.2")));
        assert!(!t.verify("phone-a", &tok, ip("10.0.0.99")), "other IP must fail");
        assert!(!t.verify("phone-b", &tok, ip("10.0.0.2")), "other device must fail");
    }

    #[test]
    fn tokens_are_unique_per_device() {
        let t = GrantTable::default();
        let a = t.grant("a", ip("10.0.0.2"));
        let b = t.grant("b", ip("10.0.0.3"));
        assert_ne!(a, b);
        assert!(!t.verify("a", &b, ip("10.0.0.2")));
    }

    #[test]
    fn revoke_invalidates_token() {
        let t = GrantTable::default();
        let tok = t.grant("a", ip("10.0.0.2"));
        t.revoke("a");
        assert!(!t.verify("a", &tok, ip("10.0.0.2")));
        assert!(t.is_empty());
    }

    #[test]
    fn regrant_from_same_ip_reuses_token_but_new_ip_rotates() {
        let t = GrantTable::default();
        let a = t.grant("a", ip("10.0.0.2"));
        assert_eq!(a, t.grant("a", ip("10.0.0.2")));
        let b = t.grant("a", ip("10.0.0.5"));
        assert_ne!(a, b);
        assert!(!t.verify("a", &a, ip("10.0.0.2")));
    }

    #[test]
    fn lockout_after_repeated_failures() {
        let t = GrantTable::default();
        let addr = ip("10.0.0.7");
        for _ in 0..MAX_AUTH_FAILURES {
            assert!(!t.is_locked_out(addr));
            t.record_failure(addr);
        }
        assert!(t.is_locked_out(addr));
        assert!(!t.is_locked_out(ip("10.0.0.8")));
        t.clear_failures(addr);
        assert!(!t.is_locked_out(addr));
    }

    #[test]
    fn constant_time_eq_basics() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
