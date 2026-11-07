//! Per-connection TCP handshake state machine.

use std::io::{BufReader, Read};
use std::net::{IpAddr, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager};

use crate::app::{refresh_tray_menu, AppState, PEERS_CHANGED_EVENT, update_tray_progress, restore_default_tray_icon};
use crate::network::pairing::{nonce_commitment, sas_code, PairDecision, PairRequestEvent, PairingManager};
use crate::network::protocol::{read_line_json, write_line_json, ClientMessage, ServerMessage};
use crate::system::{now_unix, TrustedPeer, TrustedPeers};

#[derive(Clone)]
pub struct SessionContext {
    pub app: AppHandle,
    pub device_id: String,
    pub name: String,
    pub trusted_peers: Arc<Mutex<TrustedPeers>>,
    pub pairing: Arc<PairingManager>,
    pub active_connections: Arc<Mutex<std::collections::HashSet<String>>>,
    pub active_streams: Arc<Mutex<std::collections::HashMap<String, TcpStream>>>,
    pub active_ips: Arc<Mutex<std::collections::HashMap<String, IpAddr>>>,
    pub last_clipboard: Arc<Mutex<String>>,
    /// Shared with the clipboard polling thread — acquire before any clipboard write
    /// so we never collide with the watcher's OpenClipboard call (OSError 1418).
    pub clipboard_lock: Arc<Mutex<()>>,
}

struct ActiveConnectionGuard {
    device_id: String,
    active_connections: Arc<Mutex<std::collections::HashSet<String>>>,
    active_streams: Arc<Mutex<std::collections::HashMap<String, TcpStream>>>,
    active_ips: Arc<Mutex<std::collections::HashMap<String, IpAddr>>>,
    app: AppHandle,
}

impl ActiveConnectionGuard {
    fn new(
        device_id: String,
        stream: &TcpStream,
        remote_ip: IpAddr,
        active_connections: Arc<Mutex<std::collections::HashSet<String>>>,
        active_streams: Arc<Mutex<std::collections::HashMap<String, TcpStream>>>,
        active_ips: Arc<Mutex<std::collections::HashMap<String, IpAddr>>>,
        app: AppHandle,
    ) -> Self {
        active_ips
            .lock()
            .expect("active ips mutex poisoned")
            .insert(device_id.clone(), remote_ip);
        {
            let mut active = active_connections
                .lock()
                .expect("active connections mutex poisoned");
            active.insert(device_id.clone());
        }
        if let Ok(stream) = stream.try_clone() {
            let mut streams = active_streams
                .lock()
                .expect("active streams mutex poisoned");
            streams.insert(device_id.clone(), stream);
        }
        app.emit("active-connections-changed", ()).ok();
        Self {
            device_id,
            active_connections,
            active_streams,
            active_ips,
            app,
        }
    }
}

impl Drop for ActiveConnectionGuard {
    fn drop(&mut self) {
        {
            let mut active = self
                .active_connections
                .lock()
                .expect("active connections mutex poisoned");
            active.remove(&self.device_id);
        }
        {
            let mut streams = self
                .active_streams
                .lock()
                .expect("active streams mutex poisoned");
            streams.remove(&self.device_id);
        }
        self.active_ips
            .lock()
            .expect("active ips mutex poisoned")
            .remove(&self.device_id);
        revoke_terminal_access(&self.app, &self.device_id);
        self.app.emit("active-connections-changed", ()).ok();
    }
}

pub fn handle_client(stream: TcpStream, remote_ip: IpAddr, ctx: SessionContext) {
    if let Err(e) = run_session(stream, remote_ip, ctx) {
        eprintln!("[tcp] Session ended: {e}");
    }
}

use tauri::Listener;
use tauri_plugin_clipboard_manager::ClipboardExt;

/// Unauthenticated peers get this long per handshake message.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Authenticated sessions may idle this long between messages.
const SESSION_TIMEOUT: Duration = Duration::from_secs(120);

fn run_session(stream: TcpStream, remote_ip: IpAddr, ctx: SessionContext) -> Result<(), String> {
    stream
        .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(|e| format!("Could not set read timeout: {e}"))?;
    let mut writer = stream
        .try_clone()
        .map_err(|e| format!("Could not clone TCP stream: {e}"))?;
    let mut reader = BufReader::new(stream);

    let first: ClientMessage = read_line_json(&mut reader)?
        .ok_or_else(|| "Peer closed connection before handshake".to_string())?;

    let (peer_device_id, peer_name) = match first {
        ClientMessage::Hello {
            device_id,
            name,
            token,
        } => {
            if validate_token(&ctx, &device_id, token.as_deref())? {
                write_line_json(
                    &mut writer,
                    &ServerMessage::HelloOk {
                        device_id: ctx.device_id.clone(),
                        name: ctx.name.clone(),
                    },
                )?;
                println!("[tcp] Trusted peer connected: {name} ({device_id}) from {remote_ip}");
                let _guard = ActiveConnectionGuard::new(
                    device_id.clone(),
                    &writer,
                    remote_ip,
                    ctx.active_connections.clone(),
                    ctx.active_streams.clone(),
                    ctx.active_ips.clone(),
                    ctx.app.clone(),
                );
                return keep_session_open(&mut reader, &mut writer, &ctx, device_id, remote_ip);
            }

            write_line_json(&mut writer, &ServerMessage::PairRequired)?;
            (device_id, name)
        }
        ClientMessage::PairRequest {
            device_id,
            name,
            commit,
        } => {
            return finish_pair_request(&mut reader, &mut writer, &ctx, device_id, name, commit, remote_ip);
        }
        _ => {
            return Err("Expected Hello or PairRequest".to_string());
        }
    };

    let next: ClientMessage = read_line_json(&mut reader)?
        .ok_or_else(|| "Peer closed before pair request".to_string())?;
    match next {
        ClientMessage::PairRequest {
            device_id,
            name,
            commit,
        } if device_id == peer_device_id && name == peer_name => {
            finish_pair_request(&mut reader, &mut writer, &ctx, device_id, name, commit, remote_ip)
        }
        _ => Err("Expected PairRequest after PairRequired".to_string()),
    }
}

fn validate_token(
    ctx: &SessionContext,
    device_id: &str,
    token: Option<&str>,
) -> Result<bool, String> {
    let Some(token) = token else {
        return Ok(false);
    };
    let mut peers = ctx
        .trusted_peers
        .lock()
        .expect("trusted peers mutex poisoned");
    if peers
        .get(device_id)
        .map(|peer| peer.token_matches(token))
        .unwrap_or(false)
    {
        peers.touch(device_id)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

fn finish_pair_request(
    reader: &mut BufReader<TcpStream>,
    writer: &mut TcpStream,
    ctx: &SessionContext,
    device_id: String,
    name: String,
    commit: String,
    remote_ip: IpAddr,
) -> Result<(), String> {
    // Commit-reveal: the phone is bound to its nonce before it learns ours.
    let desktop_nonce = random_hex(16);
    write_line_json(
        writer,
        &ServerMessage::PairChallenge {
            nonce: desktop_nonce.clone(),
        },
    )?;
    let reveal: ClientMessage = read_line_json(reader)?
        .ok_or_else(|| "Peer closed before revealing nonce".to_string())?;
    let phone_nonce = match reveal {
        ClientMessage::PairReveal { nonce } => nonce,
        _ => return Err("Expected PairReveal".to_string()),
    };
    if nonce_commitment(&phone_nonce) != commit.to_ascii_lowercase() {
        return Err("Pairing nonce does not match commitment".to_string());
    }

    let code = sas_code(
        &device_id,
        &ctx.device_id,
        &phone_nonce,
        &desktop_nonce,
        &crate::network::tls::identity().fingerprint,
    );
    let event = PairRequestEvent {
        device_id: device_id.clone(),
        name: name.clone(),
        code,
        ip: remote_ip.to_string(),
    };
    let (tx, rx) = mpsc::channel();
    if let Err(reason) = ctx.pairing.begin(remote_ip, event.clone(), tx) {
        let _ = write_line_json(writer, &ServerMessage::PairRejected { reason: reason.clone() });
        return Err(reason);
    }
    ctx.app
        .emit("pair-request", &event)
        .map_err(|e| format!("Could not emit pair-request event: {e}"))?;

    let decision = match rx.recv_timeout(SESSION_TIMEOUT) {
        Ok(decision) => decision,
        Err(_) => {
            ctx.pairing.remove(&device_id);
            return Err("Pair request timed out".to_string());
        }
    };

    match decision {
        PairDecision::Accept => {
            let token = generate_token();
            {
                let mut peers = ctx
                    .trusted_peers
                    .lock()
                    .expect("trusted peers mutex poisoned");
                peers.upsert(TrustedPeer::new(
                    device_id.clone(),
                    name.clone(),
                    &token,
                    now_unix(),
                ))?;
            }
            let _ = ctx.app.emit(PEERS_CHANGED_EVENT, ());
            write_line_json(
                writer,
                &ServerMessage::PairAccepted {
                    device_id: ctx.device_id.clone(),
                    name: ctx.name.clone(),
                    token,
                },
            )?;
            let _guard = ActiveConnectionGuard::new(
                device_id.clone(),
                writer,
                remote_ip,
                ctx.active_connections.clone(),
                ctx.active_streams.clone(),
                ctx.active_ips.clone(),
                ctx.app.clone(),
            );
            keep_session_open(reader, writer, ctx, device_id, remote_ip)
        }
        PairDecision::Reject => write_line_json(
            writer,
            &ServerMessage::PairRejected {
                reason: "Rejected on desktop".to_string(),
            },
        ),
    }
}

fn keep_session_open(
    mut reader: &mut BufReader<TcpStream>,
    writer: &mut TcpStream,
    ctx: &SessionContext,
    peer_device_id: String,
    peer_ip: IpAddr,
) -> Result<(), String> {
    let writer_clone = Arc::new(Mutex::new(writer.try_clone().map_err(|e| e.to_string())?));
    // Now authenticated: allow normal idle time between messages.
    reader
        .get_ref()
        .set_read_timeout(Some(SESSION_TIMEOUT))
        .map_err(|e| format!("Could not set read timeout: {e}"))?;
    // A peer that stops draining the socket must not hang file sends forever.
    reader
        .get_ref()
        .set_write_timeout(Some(SESSION_TIMEOUT))
        .map_err(|e| format!("Could not set write timeout: {e}"))?;

    // Send initial TerminalServerInfo
    {
        let msg = get_terminal_info(&ctx.app, &peer_device_id, peer_ip);
        if let Ok(mut w) = writer_clone.lock() {
            let _ = write_line_json(&mut *w, &msg);
        }
    }

    // Request initial Camera configuration from peer
    {
        let msg = ServerMessage::RequestCameraConfig;
        if let Ok(mut w) = writer_clone.lock() {
            let _ = write_line_json(&mut *w, &msg);
        }
    }

    // Bug 3 fix: push the current system volume immediately on connect so the
    // Android VolumeProviderCompat is created before any hardware-key press.
    {
        if let Some((vol, muted)) = crate::system::media::get_system_volume() {
            let msg = ServerMessage::SystemVolumeUpdate { volume: vol, muted };
            if let Ok(mut w) = writer_clone.lock() {
                let _ = write_line_json(&mut *w, &msg);
            }
        }
    }

    {
        let is_audio_enabled = ctx
            .trusted_peers
            .lock()
            .unwrap()
            .get(&peer_device_id)
            .map(|p| p.audio_streaming_enabled)
            .unwrap_or(false);

        if is_audio_enabled {
            let app_state = ctx.app.state::<AppState>();
            let mut audio_server = app_state.audio_stream_server.lock().unwrap();
            let info = ensure_audio_server(&mut audio_server, peer_ip);

            if let Some((port, token)) = info {
                let msg = ServerMessage::AudioStreamInfo { enabled: true, port, token: Some(token) };
                if let Ok(mut w) = writer_clone.lock() {
                    let _ = write_line_json(&mut *w, &msg);
                }
            }
        }
    }

    let peer_id_for_term = peer_device_id.clone();
    let app_for_term = ctx.app.clone();
    let writer_for_term = writer_clone.clone();
    let term_event_id = ctx.app.listen("terminal-access-changed", move |event| {
        if let Ok(target_device_id) = serde_json::from_str::<String>(event.payload()) {
            if target_device_id == peer_id_for_term {
                let msg = get_terminal_info(&app_for_term, &peer_id_for_term, peer_ip);
                if let Ok(mut w) = writer_for_term.lock() {
                    let _ = write_line_json(&mut *w, &msg);
                }
            }
        }
    });

    let trusted_peers_clone = ctx.trusted_peers.clone();
    let peer_device_id_clone = peer_device_id.clone();
    let closure_writer_clone = writer_clone.clone();
    let event_id = ctx.app.listen("desktop-clipboard-update", move |event| {
        let is_sync_enabled = trusted_peers_clone
            .lock()
            .unwrap()
            .get(&peer_device_id_clone)
            .map(|p| p.clipboard_sync_enabled)
            .unwrap_or(false);
        if is_sync_enabled {
            let payload = event.payload();
            if let Ok(text) = serde_json::from_str::<String>(payload) {
                let msg = ServerMessage::ClipboardUpdate { text };
                if let Ok(mut w) = closure_writer_clone.lock() {
                    let _ = write_line_json(&mut *w, &msg);
                }
            }
        }
    });

    let media_writer_clone = writer_clone.clone();
    let media_running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let media_running_clone = media_running.clone();
    let media_trusted_peers = ctx.trusted_peers.clone();
    let media_peer_id = peer_device_id.clone();
    std::thread::spawn(move || {
        #[derive(PartialEq)]
        struct MediaKey {
            title: String,
            artist: String,
            album: String,
            is_playing: bool,
            volume_pct: i32,
            player: String,
        }
        let mut last_key: Option<MediaKey> = None;
        let mut last_position_push = std::time::Instant::now();
        let mut last_system_volume: Option<(f64, bool)> = None;

        while media_running_clone.load(std::sync::atomic::Ordering::Relaxed) {
            let volume_enabled = media_trusted_peers
                .lock()
                .unwrap()
                .get(&media_peer_id)
                .map(|p| p.volume_sync_enabled)
                .unwrap_or(false);

            if volume_enabled {
                if let Some((vol, muted)) = crate::system::media::get_system_volume() {
                    let current = Some((vol, muted));
                    if last_system_volume != current {
                        let msg = ServerMessage::SystemVolumeUpdate { volume: vol, muted };
                        if let Ok(mut w) = media_writer_clone.lock() {
                            let _ = write_line_json(&mut *w, &msg);
                        }
                        last_system_volume = current;
                    }
                }
            } else {
                last_system_volume = None;
            }
            // Re-check the feature flag each tick so toggling takes effect immediately
            let media_enabled = media_trusted_peers
                .lock()
                .unwrap()
                .get(&media_peer_id)
                .map(|p| p.media_controls_enabled)
                .unwrap_or(false);

            if !media_enabled {
                // If we had previously sent state, send an empty clear so Android dismisses
                if last_key.is_some() {
                    last_key = None;
                    let empty = crate::system::media::MediaState {
                        title: String::new(),
                        artist: String::new(),
                        album: String::new(),
                        is_playing: false,
                        volume: 1.0,
                        position_us: 0,
                        length_us: 0,
                        player: String::new(),
                    };
                    if let Ok(mut w) = media_writer_clone.lock() {
                        let _ = write_line_json(&mut *w, &ServerMessage::MediaState(empty));
                    }
                }
                std::thread::sleep(Duration::from_millis(1000));
                continue;
            }

            if let Some(state) = crate::system::media::get_media_state() {
                let key = MediaKey {
                    title: state.title.clone(),
                    artist: state.artist.clone(),
                    album: state.album.clone(),
                    is_playing: state.is_playing,
                    // Round to 2 decimal places so minor float drift doesn't trigger sends
                    volume_pct: (state.volume * 100.0).round() as i32,
                    player: state.player.clone(),
                };
                // Send if core metadata changed, or if position is stale (> 3s) during playback
                let meta_changed = last_key.as_ref().map(|k| k != &key).unwrap_or(true);
                let position_due = state.is_playing
                    && last_position_push.elapsed() >= Duration::from_secs(3);

                if meta_changed || position_due {
                    let msg = ServerMessage::MediaState(state);
                    if let Ok(mut w) = media_writer_clone.lock() {
                        let _ = write_line_json(&mut *w, &msg);
                    }
                    last_key = Some(key);
                    last_position_push = std::time::Instant::now();
                }
            } else if last_key.is_some() {
                // Player disappeared — send an empty state to clear the notification
                last_key = None;
                let empty = crate::system::media::MediaState {
                    title: String::new(),
                    artist: String::new(),
                    album: String::new(),
                    is_playing: false,
                    volume: 1.0,
                    position_us: 0,
                    length_us: 0,
                    player: String::new(),
                };
                let msg = ServerMessage::MediaState(empty);
                if let Ok(mut w) = media_writer_clone.lock() {
                    let _ = write_line_json(&mut *w, &msg);
                }
            }
            std::thread::sleep(Duration::from_millis(1000));
        }
    });

    let mut incoming: Option<IncomingTransfer> = None;
    let mut raw_line: Vec<u8> = Vec::new();
    let result = loop {
        let read = match read_session_line(&mut reader, &mut raw_line) {
            Ok(n) => n,
            Err(e) => {
                break Err(format!("Could not read from authenticated session: {e}"));
            }
        };
        if read == 0 {
            break Ok(());
        }
        let line = String::from_utf8_lossy(&raw_line).into_owned();

        match serde_json::from_str::<ClientMessage>(line.trim()) {
            Ok(ClientMessage::ClipboardUpdate { text }) => {
                let is_sync_enabled = ctx
                    .trusted_peers
                    .lock()
                    .unwrap()
                    .get(&peer_device_id)
                    .map(|p| p.clipboard_sync_enabled)
                    .unwrap_or(false);
                if is_sync_enabled {
                    println!(
                        "[tcp] Received clipboard update from peer: {} bytes",
                        text.len()
                    );
                    
                    let normalized = text.replace("\r\n", "\n");
                    {
                        let mut last = ctx.last_clipboard.lock().unwrap();
                        *last = normalized.clone();
                    }

                    let clipboard = ctx.app.clipboard();
                    let _ = clipboard.write_text(normalized.clone());
                    
                    // Wait up to 200ms for the clipboard to register the update to prevent feedback loop
                    let start = std::time::Instant::now();
                    while start.elapsed() < Duration::from_millis(200) {
                        if let Ok(curr) = clipboard.read_text() {
                            if curr.replace("\r\n", "\n") == normalized {
                                break;
                            }
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                } else {
                    println!("[tcp] Ignored clipboard update from peer (sync disabled)");
                }
            }
            Ok(ClientMessage::ClipboardImage { base64_data }) => {
                let is_sync_enabled = ctx
                    .trusted_peers
                    .lock()
                    .unwrap()
                    .get(&peer_device_id)
                    .map(|p| p.clipboard_sync_enabled)
                    .unwrap_or(false);
                if is_sync_enabled {
                    println!(
                        "[tcp] Received clipboard image update from peer: {} bytes base64",
                        base64_data.len()
                    );
                    if let Ok(bytes) = STANDARD.decode(&base64_data) {
                        // Use the `image` crate to decode PNG → RGBA pixels, then write
                        // via arboard (tauri-plugin-clipboard-manager) which is far more
                        // reliable on Windows than clipboard_rs (avoids OSError 1418 caused
                        // by the clipboard thread not having an open handle).
                        use image::GenericImageView;
                        match image::load_from_memory_with_format(&bytes, image::ImageFormat::Png) {
                            Ok(img) => {
                                let rgba: Vec<u8> = img.pixels()
                                    .flat_map(|(_, _, pixel)| pixel.0)
                                    .collect();
                                let (w, h) = img.dimensions();
                                let tauri_image = tauri::image::Image::new_owned(rgba, w, h);
                                // Hold clipboard_lock so the 200ms polling thread
                                // (which uses clipboard_rs) cannot call OpenClipboard
                                // concurrently. This prevents OSError 1418 on Windows.
                                let _cb_lock = ctx.clipboard_lock.lock().unwrap();
                                match ctx.app.clipboard().write_image(&tauri_image) {
                                    Ok(_) => println!("[tcp] Successfully set image to desktop clipboard"),
                                    Err(e) => eprintln!("[tcp] Failed to write image to clipboard: {e}"),
                                }
                            }
                            Err(e) => {
                                eprintln!("[tcp] Failed to decode clipboard image PNG: {e}");
                            }
                        }
                    }
                } else {
                    println!("[tcp] Ignored clipboard image from peer (sync disabled)");
                }
            }
            Ok(ClientMessage::MediaCommand { command, value }) => {
                let media_enabled = ctx
                    .trusted_peers
                    .lock()
                    .unwrap()
                    .get(&peer_device_id)
                    .map(|p| p.media_controls_enabled)
                    .unwrap_or(false);
                let volume_enabled = ctx
                    .trusted_peers
                    .lock()
                    .unwrap()
                    .get(&peer_device_id)
                    .map(|p| p.volume_sync_enabled)
                    .unwrap_or(false);

                let is_volume_cmd = command == "SetSystemVolume" || command == "SystemVolumeUp" || command == "SystemVolumeDown";
                let allowed = if is_volume_cmd { volume_enabled } else { media_enabled };

                if allowed {
                    if let Err(e) = crate::system::media::send_media_command(&command, value) {
                        println!("[tcp] Media/volume command error: {e}");
                    }
                } else {
                    println!("[tcp] Ignored {} command from peer (disabled on desktop)", command);
                }
            }
            Ok(ClientMessage::FileStart { transfer_id, filename, total_bytes }) => {
                if let Some(old) = incoming.take() {
                    discard_incoming(old);
                }
                let (enabled, peer_name) = ctx
                    .trusted_peers
                    .lock()
                    .unwrap()
                    .get(&peer_device_id)
                    .map(|p| (p.incoming_files_enabled, p.name.clone()))
                    .unwrap_or((false, "A paired device".to_string()));
                if !enabled {
                    println!("[tcp] Ignored incoming file from peer (incoming files disabled)");
                    let _ = write_line_json(writer, &ServerMessage::FileAbort { transfer_id });
                    // Tell the user why nothing arrived; otherwise the rejection is silent.
                    let _ = notify_rust::Notification::new()
                        .summary("File Blocked")
                        .body(&format!(
                            "{peer_name} tried to send a file. Turn on \"Receive Files\" for this device to accept it."
                        ))
                        .show();
                } else {
                    match begin_incoming(&ctx.app, &peer_device_id, transfer_id.clone(), &filename, total_bytes) {
                        Ok(t) => incoming = Some(t),
                        Err(e) => {
                            eprintln!("[tcp] Cannot receive file '{filename}': {e}");
                            let _ = write_line_json(writer, &ServerMessage::FileAbort { transfer_id });
                        }
                    }
                }
            }
            Ok(ClientMessage::FileChunk { transfer_id, base64_data }) => {
                let failed = match incoming.as_mut() {
                    Some(t) if t.id == transfer_id => append_chunk(&ctx.app, &peer_device_id, t, &base64_data).err(),
                    _ => None, // stale chunk of an aborted/ignored transfer
                };
                if let Some(e) = failed {
                    eprintln!("[tcp] Incoming file failed: {e}");
                    if let Some(t) = incoming.take() {
                        discard_incoming(t);
                    }
                    finish_transfer_ui(&ctx.app, &peer_device_id);
                    let _ = write_line_json(writer, &ServerMessage::FileAbort { transfer_id });
                }
            }
            Ok(ClientMessage::FileEnd { transfer_id, sha256 }) => {
                if incoming.as_ref().map(|t| t.id == transfer_id).unwrap_or(false) {
                    let t = incoming.take().unwrap();
                    match finish_incoming(t, &sha256) {
                        Ok(name) => {
                            println!("[tcp] Saved incoming file '{name}'");
                            let _ = notify_rust::Notification::new()
                                .summary("File Received")
                                .body(&format!("Saved to Downloads: {name}"))
                                .show();
                        }
                        Err(e) => {
                            eprintln!("[tcp] Incoming file rejected: {e}");
                            let _ = write_line_json(writer, &ServerMessage::FileAbort { transfer_id });
                        }
                    }
                    finish_transfer_ui(&ctx.app, &peer_device_id);
                }
            }
            Ok(ClientMessage::FileAbort { transfer_id }) => {
                note_outgoing_abort(&transfer_id);
                if incoming.as_ref().map(|t| t.id == transfer_id).unwrap_or(false) {
                    println!("[tcp] Peer aborted file transfer");
                    discard_incoming(incoming.take().unwrap());
                    finish_transfer_ui(&ctx.app, &peer_device_id);
                }
            }
            Ok(ClientMessage::CameraStreamStarted { port, use_adb, token }) => {
                let peer_ip = peer_ip.to_string();
                println!("[tcp] Received CameraStreamStarted from peer: {}:{} (use_adb: {})", peer_ip, port, use_adb);
                let _ = ctx.app.emit("camera-stream-state-changed", serde_json::json!({
                    "streaming": true,
                    "ip": peer_ip,
                    "port": port,
                    "use_adb": use_adb,
                }));
                let app_c = ctx.app.clone();
                let ip_c = peer_ip.clone();
                tauri::async_runtime::spawn(async move {
                    let state = app_c.state::<AppState>();
                    let _ = crate::commands::camera::start_virtual_camera(app_c.clone(), state, ip_c, port, use_adb, token).await;
                });
            }

            Ok(ClientMessage::CameraStreamStopped) => {
                println!("[tcp] Received CameraStreamStopped from peer");
                let _ = ctx.app.emit("camera-stream-state-changed", serde_json::json!({
                    "streaming": false,
                }));
                let app_c = ctx.app.clone();
                tauri::async_runtime::spawn(async move {
                    let state = app_c.state::<AppState>();
                    let _ = crate::commands::camera::stop_virtual_camera(state).await;
                });
            }
            Ok(ClientMessage::CameraConfigState { is_front, resolution, fps, rotation, use_adb }) => {
                println!("[tcp] Received CameraConfigState from peer: is_front={}, res={}, fps={}, rot={}, use_adb={}", is_front, resolution, fps, rotation, use_adb);
                let _ = ctx.app.emit("camera-config-changed", serde_json::json!({
                    "deviceId": peer_device_id,
                    "is_front": is_front,
                    "resolution": resolution,
                    "fps": fps,
                    "rotation": rotation,
                    "use_adb": use_adb,
                }));
            }
            Ok(ClientMessage::MicStreamStarted { port, sample_rate, channels, use_adb, token }) => {
                let peer_ip = peer_ip.to_string();
                println!("[tcp] Received MicStreamStarted from peer: {}:{}", peer_ip, port);
                let _ = ctx.app.emit("mic-stream-state-changed", serde_json::json!({
                    "streaming": true,
                    "ip": peer_ip,
                    "port": port,
                    "sample_rate": sample_rate,
                    "channels": channels,
                    "use_adb": use_adb,
                }));
                let app_c = ctx.app.clone();
                let ip_c = peer_ip.clone();
                tauri::async_runtime::spawn(async move {
                    let state = app_c.state::<AppState>();
                    let _ = crate::commands::mic::start_virtual_mic(app_c.clone(), state, ip_c, port, sample_rate, channels, use_adb, token).await;
                });
            }
            Ok(ClientMessage::MicStreamStopped) => {
                println!("[tcp] Received MicStreamStopped from peer");
                let _ = ctx.app.emit("mic-stream-state-changed", serde_json::json!({
                    "streaming": false,
                }));
                let _ = ctx.app.emit("mic-audio-level", 0.0);
                let app_c = ctx.app.clone();
                tauri::async_runtime::spawn(async move {
                    let state = app_c.state::<AppState>();
                    let _ = crate::commands::mic::stop_virtual_mic(state).await;
                });
            }
            Ok(ClientMessage::AudioStreamRequest { start }) => {
                handle_audio_request(ctx, writer, &peer_device_id, peer_ip, start);
            }
            Ok(ClientMessage::Unpair) => {
                println!("[tcp] Received Unpair request from peer");
                let _ = ctx.trusted_peers.lock().unwrap().remove(&peer_device_id);
                let _ = ctx.app.emit(PEERS_CHANGED_EVENT, ());
                let _ = refresh_tray_menu(&ctx.app);
                break Ok(());
            }
            Ok(_) => println!("[tcp] Authenticated session payload: {}", line.trim()),
            Err(e) => println!(
                "[tcp] Unknown message from peer: {}; raw={}",
                e,
                line.trim()
            ),
        }
    };

    let _ = ctx.app.emit("camera-stream-state-changed", serde_json::json!({
        "streaming": false,
    }));
    let _ = ctx.app.emit("mic-stream-state-changed", serde_json::json!({
        "streaming": false,
    }));
    let _ = ctx.app.emit("mic-audio-level", 0.0);
    {
        let app_c = ctx.app.clone();
        tauri::async_runtime::spawn(async move {
            let state = app_c.state::<AppState>();
            let _ = crate::commands::camera::stop_virtual_camera(state.clone()).await;
            let _ = crate::commands::mic::stop_virtual_mic(state.clone()).await;
        });
    }
    if let Some(t) = incoming.take() {
        discard_incoming(t);
    }
    media_running.store(false, std::sync::atomic::Ordering::Relaxed);
    ctx.app.unlisten(event_id);
    ctx.app.unlisten(term_event_id);
    {
        let app_state = ctx.app.state::<AppState>();
        let mut transfer = app_state.active_transfer.lock().unwrap();
        if let Some(ref progress) = *transfer {
            if progress.device_id == peer_device_id {
                *transfer = None;
                let _ = ctx.app.emit("file-transfer-finished", &peer_device_id);
                let _ = refresh_tray_menu(&ctx.app);
                restore_default_tray_icon(&ctx.app);
            }
        }
    }
    {
        let app_state = ctx.app.state::<AppState>();
        *app_state.audio_stream_server.lock().unwrap() = None;
    }
    result
}

fn random_hex(bytes: usize) -> String {
    use rand::RngExt as _;
    let raw: Vec<u8> = (0..bytes).map(|_| rand::rng().random::<u8>()).collect();
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

fn generate_token() -> String {
    // 256 bits from the thread CSPRNG (rather than UUIDv4s, which carry fixed bits).
    STANDARD.encode(
        (0..32)
            .map(|_| {
                use rand::RngExt as _;
                rand::rng().random::<u8>()
            })
            .collect::<Vec<u8>>(),
    )
}

/// Reduce a peer-supplied name to a single safe path component.
///
/// Drops any directory part (both `/` and `\\` separators), control characters and
/// Windows-forbidden characters, trailing dots/spaces, and neutralises reserved
/// Windows device names. Rejects names that end up empty.
pub(crate) fn sanitize_filename(raw: &str) -> Result<String, String> {
    let last = raw.rsplit(|c| c == '/' || c == '\\').next().unwrap_or("");
    let mut cleaned: String = last
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        .collect();
    cleaned = cleaned.trim_matches(|c: char| c == '.' || c.is_whitespace()).to_string();
    if cleaned.is_empty() {
        return Err("Invalid file name".to_string());
    }
    // Keep it under common filesystem limits (255 bytes) on a char boundary.
    while cleaned.len() > 200 {
        cleaned.pop();
    }
    let base = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (base.len() == 4
            && (base.starts_with("COM") || base.starts_with("LPT"))
            && base.chars().last().map_or(false, |c| c.is_ascii_digit()));
    if reserved {
        cleaned.insert(0, '_');
    }
    Ok(cleaned)
}

/// Ids of our outgoing transfers that the peer asked us to stop.
static ABORTED_OUTGOING: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn note_outgoing_abort(id: &str) {
    let mut aborted = ABORTED_OUTGOING.lock().unwrap();
    if aborted.len() >= 32 {
        aborted.remove(0);
    }
    aborted.push(id.to_string());
}

/// True if the peer aborted outgoing transfer `id`.
pub(crate) fn outgoing_aborted(id: &str) -> bool {
    ABORTED_OUTGOING.lock().unwrap().iter().any(|a| a == id)
}

/// Largest single message accepted from an authenticated peer (file data arrives
/// in small chunks; this leaves room for a clipboard image).
const MAX_SESSION_LINE: u64 = 64 * 1024 * 1024;

/// Read one newline-terminated message into `buf` (without unbounded growth).
/// Returns 0 on a clean end of stream.
fn read_session_line(reader: &mut BufReader<TcpStream>, buf: &mut Vec<u8>) -> Result<usize, String> {
    use std::io::BufRead;
    buf.clear();
    let n = reader
        .by_ref()
        .take(MAX_SESSION_LINE + 1)
        .read_until(b'\n', buf)
        .map_err(|e| format!("{e}"))?;
    if n == 0 {
        return Ok(0);
    }
    if buf.last() != Some(&b'\n') {
        return Err(if n as u64 > MAX_SESSION_LINE {
            "Message exceeds the maximum allowed size".to_string()
        } else {
            "Connection closed in the middle of a message".to_string()
        });
    }
    Ok(n)
}

/// A file being received from the phone, written straight to disk.
struct IncomingTransfer {
    id: String,
    file: std::fs::File,
    path: std::path::PathBuf,
    hasher: Sha256,
    received: u64,
    total: u64,
    started: std::time::Instant,
    last_emit: std::time::Instant,
}

fn begin_incoming(
    app: &AppHandle,
    peer_device_id: &str,
    id: String,
    filename: &str,
    total: u64,
) -> Result<IncomingTransfer, String> {
    let (file, path) = create_download_file(app, filename)?;
    let shown = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(filename)
        .to_string();
    println!("[tcp] File transfer started: {shown} ({total} bytes)");
    {
        let state = app.state::<AppState>();
        *state.active_transfer.lock().unwrap() = Some(crate::app::TransferProgress {
            device_id: peer_device_id.to_string(),
            filename: shown,
            bytes_received: 0,
            total_bytes: total,
            speed_bytes_per_sec: 0.0,
            status: "Receiving".to_string(),
        });
    }
    let _ = refresh_tray_menu(app);
    let _ = app.emit("file-transfer-started", peer_device_id);
    let now = std::time::Instant::now();
    Ok(IncomingTransfer {
        id,
        file,
        path,
        hasher: Sha256::new(),
        received: 0,
        total,
        started: now,
        last_emit: now,
    })
}

fn append_chunk(
    app: &AppHandle,
    peer_device_id: &str,
    t: &mut IncomingTransfer,
    base64_data: &str,
) -> Result<(), String> {
    use std::io::Write as _;
    let bytes = STANDARD
        .decode(base64_data)
        .map_err(|e| format!("Invalid base64 chunk: {e}"))?;
    t.received += bytes.len() as u64;
    if t.received > t.total {
        return Err("Received more data than announced".to_string());
    }
    t.hasher.update(&bytes);
    t.file
        .write_all(&bytes)
        .map_err(|e| format!("Failed to write file to disk: {e}"))?;

    if t.last_emit.elapsed() >= Duration::from_millis(150) {
        t.last_emit = std::time::Instant::now();
        let state = app.state::<AppState>();
        let mut transfer = state.active_transfer.lock().unwrap();
        if let Some(ref mut progress) = *transfer {
            if progress.device_id == peer_device_id {
                progress.bytes_received = t.received;
                let elapsed = t.started.elapsed().as_secs_f64();
                if elapsed > 0.0 {
                    progress.speed_bytes_per_sec = t.received as f64 / elapsed;
                }
                progress.status = "Receiving".to_string();
                let _ = app.emit("file-transfer-progress", progress.clone());
                let pct = if t.total > 0 { t.received as f64 / t.total as f64 * 100.0 } else { 0.0 };
                update_tray_progress(app, pct.min(100.0));
            }
        }
    }
    Ok(())
}

/// Verify size and checksum; on any mismatch the partial file is removed.
fn finish_incoming(t: IncomingTransfer, expected_sha256: &str) -> Result<String, String> {
    let IncomingTransfer { file, path, hasher, received, total, .. } = t;
    let result = (|| {
        if received != total {
            return Err(format!("Incomplete file: got {received} of {total} bytes"));
        }
        let computed = hasher
            .finalize()
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>();
        if !computed.eq_ignore_ascii_case(expected_sha256) {
            return Err(format!(
                "SHA-256 verification failed: expected {expected_sha256}, got {computed}"
            ));
        }
        file.sync_all().map_err(|e| format!("Failed to flush file: {e}"))?;
        Ok(path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&path);
    }
    result
}

fn discard_incoming(t: IncomingTransfer) {
    let IncomingTransfer { file, path, .. } = t;
    drop(file);
    let _ = std::fs::remove_file(path);
}

fn finish_transfer_ui(app: &AppHandle, peer_device_id: &str) {
    {
        let state = app.state::<AppState>();
        let mut transfer = state.active_transfer.lock().unwrap();
        if transfer.as_ref().map(|p| p.device_id == peer_device_id).unwrap_or(false) {
            *transfer = None;
        }
    }
    let _ = refresh_tray_menu(app);
    let _ = app.emit("file-transfer-finished", peer_device_id);
    restore_default_tray_icon(app);
}

/// Create a fresh file in the downloads directory for a name chosen by the peer.
fn create_download_file(app: &AppHandle, filename: &str) -> Result<(std::fs::File, std::path::PathBuf), String> {
    let download_dir = app
        .path()
        .download_dir()
        .map_err(|e| format!("Could not resolve download dir: {e}"))?;

    // The file name comes from the peer: reduce it to a bare, safe name so it can
    // never point outside the download directory.
    let safe_name = sanitize_filename(filename)?;
    let path = std::path::Path::new(&safe_name);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_string();
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_string();

    // create_new makes the "does it exist" check and the create atomic (no overwrite,
    // no following a pre-planted symlink).
    let mut counter = 0;
    let (file, file_path) = loop {
        let candidate = if counter == 0 {
            safe_name.clone()
        } else if extension.is_empty() {
            format!("{stem} ({counter})")
        } else {
            format!("{stem} ({counter}).{extension}")
        };
        let candidate_path = download_dir.join(&candidate);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate_path)
        {
            Ok(f) => break (f, candidate_path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                counter += 1;
                if counter > 10_000 {
                    return Err("Too many files with the same name".to_string());
                }
            }
            Err(e) => return Err(format!("Failed to create file: {e}")),
        }
    };

    // Defence in depth: the resolved parent must still be the download directory.
    if file_path.parent() != Some(download_dir.as_path()) {
        drop(file);
        let _ = std::fs::remove_file(&file_path);
        return Err("Refusing to write outside the download directory".to_string());
    }
    Ok((file, file_path))
}

/// Start (or reuse) the audio server for `peer_ip`. The server only serves that
/// address and only with its token; a different peer gets a fresh server.
pub(crate) fn ensure_audio_server(
    slot: &mut Option<crate::system::audio::AudioStreamServer>,
    peer_ip: IpAddr,
) -> Option<(u16, String)> {
    if let Some(existing) = slot.as_ref() {
        if existing.allowed_ip == peer_ip {
            return Some((existing.port, existing.token.clone()));
        }
    }
    *slot = None;
    match crate::system::audio::AudioStreamServer::start(peer_ip) {
        Ok(server) => {
            let info = (server.port, server.token.clone());
            *slot = Some(server);
            Some(info)
        }
        Err(e) => {
            eprintln!("[audio] Failed to start audio server: {e}");
            None
        }
    }
}

/// A phone asked to start/stop listening to this PC's audio. Starting needs the
/// desktop user's approval unless audio was already allowed for this device.
fn handle_audio_request(
    ctx: &SessionContext,
    writer: &mut TcpStream,
    peer_device_id: &str,
    peer_ip: IpAddr,
    start: bool,
) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};

    let disabled = ServerMessage::AudioStreamInfo { enabled: false, port: 0, token: None };
    let (peer_name, already_allowed) = {
        let peers = ctx.trusted_peers.lock().unwrap();
        match peers.get(peer_device_id) {
            Some(p) => (p.name.clone(), p.audio_streaming_enabled),
            None => return,
        }
    };

    let set_flag = |value: bool| {
        let mut peers = ctx.trusted_peers.lock().unwrap();
        if let Some(peer) = peers.get(peer_device_id) {
            let mut updated = peer.clone();
            updated.audio_streaming_enabled = value;
            let _ = peers.upsert(updated);
        }
    };

    if !start {
        set_flag(false);
        let _ = ctx.app.emit(PEERS_CHANGED_EVENT, ());
        let state = ctx.app.state::<AppState>();
        *state.audio_stream_server.lock().unwrap() = None;
        let _ = write_line_json(writer, &disabled);
        return;
    }

    if !already_allowed {
        let approved = ctx
            .app
            .dialog()
            .message(format!(
                "{peer_name} wants to listen to this computer's audio.\n\nAllow it?"
            ))
            .title("Listen Through Mobile")
            .buttons(MessageDialogButtons::YesNo)
            .blocking_show();
        if !approved {
            let _ = write_line_json(writer, &disabled);
            return;
        }
    }

    let info = {
        let state = ctx.app.state::<AppState>();
        let mut audio_server = state.audio_stream_server.lock().unwrap();
        ensure_audio_server(&mut audio_server, peer_ip)
    };
    match info {
        Some((port, token)) => {
            set_flag(true);
            let _ = ctx.app.emit(PEERS_CHANGED_EVENT, ());
            let _ = write_line_json(
                writer,
                &ServerMessage::AudioStreamInfo { enabled: true, port, token: Some(token) },
            );
            let _ = notify_rust::Notification::new()
                .summary("Listen Through Mobile")
                .body(&format!("Audio is now streaming to {peer_name}"))
                .show();
        }
        None => {
            let _ = write_line_json(writer, &disabled);
        }
    }
}

/// Issues (or withholds) terminal credentials for one peer.
///
/// Credentials are only produced when the desktop user enabled terminal access for
/// this device. The server is started on first need, and each device gets its own
/// token that is bound to the IP of its control session.
fn get_terminal_info(
    app: &tauri::AppHandle,
    peer_device_id: &str,
    peer_ip: std::net::IpAddr,
) -> ServerMessage {
    let state = app.state::<AppState>();
    let enabled = state
        .trusted_peers
        .lock()
        .unwrap()
        .get(peer_device_id)
        .map(|p| p.terminal_access_enabled)
        .unwrap_or(false);

    if !enabled {
        revoke_terminal_access(app, peer_device_id);
        return ServerMessage::TerminalServerInfo {
            enabled: false,
            port: 0,
            username: peer_device_id.to_string(),
            password: None,
        };
    }

    let mut server = state.terminal_server.lock().unwrap();
    if server.is_none() {
        match crate::system::terminal::TerminalServerManager::start() {
            Ok(s) => *server = Some(s),
            Err(e) => {
                eprintln!("[terminal] Failed to start terminal server: {e}");
                return ServerMessage::TerminalServerInfo {
                    enabled: false,
                    port: 0,
                    username: peer_device_id.to_string(),
                    password: None,
                };
            }
        }
    }
    let srv = server.as_ref().expect("terminal server just started");
    ServerMessage::TerminalServerInfo {
        enabled: true,
        port: srv.port,
        username: peer_device_id.to_string(),
        password: Some(srv.grant(peer_device_id, peer_ip)),
    }
}

/// Withdraw a device's terminal access and stop the server when nobody has access.
pub fn revoke_terminal_access(app: &tauri::AppHandle, device_id: &str) {
    let state = app.state::<AppState>();
    let mut server = state.terminal_server.lock().unwrap();
    if let Some(srv) = server.as_ref() {
        srv.revoke(device_id);
        if !srv.has_grants() {
            *server = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_filename;

    #[test]
    fn strips_directories() {
        assert_eq!(sanitize_filename("../../.config/autostart/x.desktop").unwrap(), "x.desktop");
        assert_eq!(sanitize_filename("/home/u/.bashrc").unwrap(), "bashrc");
        assert_eq!(sanitize_filename("..\\..\\Startup\\evil.bat").unwrap(), "evil.bat");
        assert_eq!(sanitize_filename("C:\\Windows\\a.txt").unwrap(), "a.txt");
    }

    #[test]
    fn rejects_empty_and_dots() {
        assert!(sanitize_filename("").is_err());
        assert!(sanitize_filename("..").is_err());
        assert!(sanitize_filename("/").is_err());
        assert!(sanitize_filename("...   ").is_err());
    }

    #[test]
    fn neutralises_reserved_and_control_chars() {
        assert_eq!(sanitize_filename("CON.txt").unwrap(), "_CON.txt");
        assert_eq!(sanitize_filename("lpt1").unwrap(), "_lpt1");
        assert_eq!(sanitize_filename("a\0b\n.txt").unwrap(), "ab.txt");
    }

    #[test]
    fn keeps_normal_names() {
        assert_eq!(sanitize_filename("photo (1).jpg").unwrap(), "photo (1).jpg");
        assert_eq!(sanitize_filename("résumé.pdf").unwrap(), "résumé.pdf");
    }
}
