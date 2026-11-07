//! Application bootstrap.
//!
//! This is the single place where all the background services
//! (discovery broadcaster, TCP server, ...) are wired together and
//! where the Tauri builder is configured.

use crate::commands;
use crate::network::pairing::PairingManager;
use crate::network::session::SessionContext;
use crate::network::{self, DEFAULT_DISCOVERY_PORT, DEFAULT_TCP_PORT};
use crate::system::{get_or_create_device_id, get_system_name, TrustedPeers};
use std::collections::{HashMap, HashSet};
use std::net::Shutdown;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager};

const TRAY_ID: &str = "main";
pub const PEERS_CHANGED_EVENT: &str = "trusted-peers-changed";

fn setup_error(message: String) -> Box<dyn std::error::Error> {
    Box::new(std::io::Error::new(std::io::ErrorKind::Other, message))
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct TransferProgress {
    pub device_id: String,
    pub filename: String,
    pub bytes_received: u64,
    pub total_bytes: u64,
    pub speed_bytes_per_sec: f64,
    pub status: String,
}

pub struct AppState {
    pub trusted_peers: Arc<Mutex<TrustedPeers>>,
    pub pairing: Arc<PairingManager>,
    /// Controls whether UDP discovery packets are sent. Toggle via the
    /// `set_broadcasting` command.
    pub broadcasting: Arc<AtomicBool>,
    pub active_connections: Arc<Mutex<HashSet<String>>>,
    pub active_streams: Arc<Mutex<HashMap<String, std::net::TcpStream>>>,
    /// Real (pre-proxy) IP of each connected peer.
    pub active_ips: Arc<Mutex<HashMap<String, std::net::IpAddr>>>,
    pub last_clipboard: Arc<Mutex<String>>,
    pub copied_files: Arc<Mutex<Vec<String>>>,
    /// Serializes all OS-level clipboard access (polling thread vs image writes).
    /// On Windows, only one thread can hold the clipboard open at a time.
    pub clipboard_lock: Arc<Mutex<()>>,
    pub tray_menu: tauri::menu::Menu<tauri::Wry>,
    pub active_transfer: Arc<Mutex<Option<TransferProgress>>>,
    pub terminal_server: Arc<Mutex<Option<crate::system::terminal::TerminalServerManager>>>,
    pub audio_stream_server: Arc<Mutex<Option<crate::system::audio::AudioStreamServer>>>,
    pub virtual_camera_running: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    pub latest_camera_frame: Arc<Mutex<Option<Vec<u8>>>>,
    pub virtual_mic_running: Arc<Mutex<Option<crate::system::virtual_mic::VirtualMicrophone>>>,
}

impl AppState {
    pub fn is_broadcasting(&self) -> bool {
        self.broadcasting.load(Ordering::Relaxed)
    }
    pub fn set_broadcasting(&self, enabled: bool) {
        self.broadcasting.store(enabled, Ordering::Relaxed);
    }
}

/// Start all background networking services used to connect the
/// desktop with the Android peer.
fn start_background_services(app: &tauri::AppHandle, state: &AppState) -> Result<(), String> {
    let device_id = get_or_create_device_id(app)?;
    let name = get_system_name().unwrap_or_else(|| "Desktop".to_string());
    network::tls::init(app)?;

    network::discovery::start_broadcast(
        DEFAULT_DISCOVERY_PORT,
        DEFAULT_TCP_PORT,
        device_id.clone(),
        name.clone(),
        state.broadcasting.clone(),
    );
    network::tcp::start_server(
        DEFAULT_TCP_PORT,
        SessionContext {
            app: app.clone(),
            device_id,
            name,
            trusted_peers: state.trusted_peers.clone(),
            pairing: state.pairing.clone(),
            active_connections: state.active_connections.clone(),
            active_streams: state.active_streams.clone(),
            active_ips: state.active_ips.clone(),
            last_clipboard: state.last_clipboard.clone(),
            clipboard_lock: state.clipboard_lock.clone(),
        },
    );
    Ok(())
}

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Listener,
};
use tauri_plugin_autostart::MacosLauncher;
use clipboard_rs::{Clipboard, ClipboardContext};

pub fn refresh_tray_menu(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    
    // Clear the existing menu in-place
    if let Ok(items) = state.tray_menu.items() {
        for item in &items {
            let _ = state.tray_menu.remove(item);
        }
    }
    
    // Re-populate the menu in-place
    populate_tray_menu(app, &state.tray_menu)
        .map_err(|e| format!("Could not populate tray menu: {e}"))
}

fn populate_tray_menu(app: &AppHandle, menu: &Menu<tauri::Wry>) -> tauri::Result<()> {
    let show_i = MenuItem::with_id(app, "show", "Show", true, None::<&str>)?;
    let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;

    menu.append(&show_i)?;
    menu.append(&separator)?;

    let state = app.state::<AppState>();
    {
        let transfer = state.active_transfer.lock().unwrap();
        if let Some(ref progress) = *transfer {
            let label = match progress.status.as_str() {
                "Decoding" => format!("Decoding file: {}…", progress.filename),
                "Verifying" => format!("Verifying file: {}…", progress.filename),
                "Saving" => format!("Saving file: {}…", progress.filename),
                _ => format!("Receiving file: {}…", progress.filename),
            };
            let receiving_i = MenuItem::with_id(
                app,
                "receiving_file",
                label,
                true,
                None::<&str>,
            )?;
            menu.append(&receiving_i)?;
            let separator = PredefinedMenuItem::separator(app)?;
            menu.append(&separator)?;
        }
    }
    let active = state
        .active_connections
        .lock()
        .expect("active connections mutex poisoned");
    let mut peers = state
        .trusted_peers
        .lock()
        .expect("trusted peers mutex poisoned")
        .all();
    peers.sort_by_key(|peer| peer.name.to_lowercase());

    if peers.is_empty() {
        let no_devices =
            MenuItem::with_id(app, "no_devices", "No paired devices", false, None::<&str>)?;
        menu.append(&no_devices)?;
    } else {
        for peer in peers {
            let label = if active.contains(&peer.device_id) {
                format!("{} (Connected)", peer.name)
            } else {
                format!("{} (Disconnected)", peer.name)
            };
            let is_connected = active.contains(&peer.device_id);
            let disconnect = MenuItem::with_id(
                app,
                format!("disconnect:{}", peer.device_id),
                "Disconnect",
                is_connected,
                None::<&str>,
            )?;
            let pick_files = MenuItem::with_id(
                app,
                format!("pick_files:{}", peer.device_id),
                "Send Files…",
                is_connected,
                None::<&str>,
            )?;
            let copied_files = state.copied_files.lock().expect("copied files mutex poisoned");
            let mut file_items = Vec::new();
            if !copied_files.is_empty() && is_connected {
                let file_name = std::path::Path::new(&copied_files[0])
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let send_file = MenuItem::with_id(
                    app,
                    format!("send_file:{}", peer.device_id),
                    format!("Send Copied: {}", file_name),
                    true,
                    None::<&str>,
                ).unwrap();
                file_items.push(send_file);
            }
            let mut refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> = vec![&disconnect, &pick_files];
            for item in &file_items {
                refs.push(item);
            }
            let device_menu = Submenu::with_items(app, label, true, &refs)?;
            menu.append(&device_menu)?;
        }
    }

    let separator = PredefinedMenuItem::separator(app)?;
    menu.append(&separator)?;
    menu.append(&quit_i)?;
    Ok(())
}

pub fn update_tray_progress(app: &AppHandle, progress_pct: f64) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let width = 32;
        let height = 32;
        let mut rgba = vec![0u8; (width * height * 4) as usize];
        
        let cx = 15.5;
        let cy = 15.5;
        let r_mid = 13.0; // middle radius
        let thickness = 3.0; // ring thickness
        
        for y in 0..height {
            for x in 0..width {
                let dx = x as f64 - cx;
                let dy = y as f64 - cy;
                let dist = (dx * dx + dy * dy).sqrt();
                
                // Calculate distance to the ring centerline
                let dist_to_ring = (dist - r_mid).abs();
                
                // Soft edge profile for the ring
                let mut ring_intensity = 0.0;
                let half_thickness = thickness / 2.0;
                if dist_to_ring < half_thickness - 0.5 {
                    ring_intensity = 1.0;
                } else if dist_to_ring < half_thickness + 0.5 {
                    ring_intensity = half_thickness + 0.5 - dist_to_ring;
                }
                
                if ring_intensity > 0.0 {
                    // Calculate angle starting from top (0) going clockwise to 2*PI
                    let mut angle = dx.atan2(-dy);
                    if angle < 0.0 {
                        angle += 2.0 * std::f64::consts::PI;
                    }
                    
                    let target_angle = (progress_pct / 100.0) * 2.0 * std::f64::consts::PI;
                    
                    let idx = ((y * width + x) * 4) as usize;
                    if angle <= target_angle {
                        // Cyan: rgb(6, 182, 212)
                        rgba[idx] = 6;
                        rgba[idx + 1] = 182;
                        rgba[idx + 2] = 212;
                    } else {
                        // Dark grey: rgb(63, 63, 70)
                        rgba[idx] = 63;
                        rgba[idx + 1] = 63;
                        rgba[idx + 2] = 70;
                    }
                    rgba[idx + 3] = (ring_intensity * 255.0) as u8;
                }
            }
        }
        
        let img = tauri::image::Image::new_owned(rgba, width, height);
        let _ = tray.set_icon(Some(img));
    }
}

pub fn generate_logo_image(width: u32, height: u32, is_dark_theme: bool) -> tauri::image::Image<'static> {
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    
    // Scale factors based on 120x120 viewBox
    let scale_x = width as f64 / 120.0;
    let scale_y = height as f64 / 120.0;
    
    let cx = 60.0 * scale_x;
    let cy = 86.0 * scale_y;
    
    let r1 = 53.0 * scale_x;
    let r2 = 27.0 * scale_x;
    let r3 = 6.0 * scale_x;
    
    let w = 10.0 * scale_x;
    let half_w = w / 2.0;
    
    // Color: White for dark theme, `#151515` (21, 21, 21) for light theme
    let (r_val, g_val, b_val) = if is_dark_theme {
        (255, 255, 255)
    } else {
        (21, 21, 21)
    };
    
    for y in 0..height {
        for x in 0..width {
            let px = x as f64 + 0.5;
            let py = y as f64 + 0.5;
            
            // 1. Center dot
            let dx = px - cx;
            let dy = py - cy;
            let dist_dot = (dx * dx + dy * dy).sqrt();
            let intensity_dot = (r3 + 0.5 - dist_dot).clamp(0.0, 1.0);
            
            // 2. Inner wave (r2)
            let dist_wave2 = if py <= cy {
                (dist_dot - r2).abs()
            } else {
                let dl_x = px - (cx - r2);
                let dl_y = py - cy;
                let dr_x = px - (cx + r2);
                let dr_y = py - cy;
                let dist_left = (dl_x * dl_x + dl_y * dl_y).sqrt();
                let dist_right = (dr_x * dr_x + dr_y * dr_y).sqrt();
                dist_left.min(dist_right)
            };
            let intensity_wave2 = (half_w + 0.5 - dist_wave2).clamp(0.0, 1.0);
            
            // 3. Outer wave (r1)
            let dist_wave1 = if py <= cy {
                (dist_dot - r1).abs()
            } else {
                let dl_x = px - (cx - r1);
                let dl_y = py - cy;
                let dr_x = px - (cx + r1);
                let dr_y = py - cy;
                let dist_left = (dl_x * dl_x + dl_y * dl_y).sqrt();
                let dist_right = (dr_x * dr_x + dr_y * dr_y).sqrt();
                dist_left.min(dist_right)
            };
            let intensity_wave1 = (half_w + 0.5 - dist_wave1).clamp(0.0, 1.0);
            
            // Combine intensities (screen / max)
            let intensity = intensity_dot.max(intensity_wave2).max(intensity_wave1);
            
            if intensity > 0.0 {
                let idx = ((y * width + x) * 4) as usize;
                rgba[idx] = r_val;
                rgba[idx + 1] = g_val;
                rgba[idx + 2] = b_val;
                rgba[idx + 3] = (intensity * 255.0) as u8;
            }
        }
    }
    
    tauri::image::Image::new_owned(rgba, width, height)
}

pub fn restore_default_tray_icon(app: &AppHandle) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let is_dark = app.get_webview_window("main")
            .and_then(|w| w.theme().ok())
            .map(|t| t == tauri::Theme::Dark)
            .unwrap_or(true);
        let icon = generate_logo_image(32, 32, is_dark);
        let _ = tray.set_icon(Some(icon));
    }
}

pub fn disconnect_peer(device_id: &str, state: &AppState) {
    if let Some(stream) = state
        .active_streams
        .lock()
        .expect("active streams mutex poisoned")
        .remove(device_id)
    {
        let _ = stream.shutdown(Shutdown::Both);
    }
    state
        .active_connections
        .lock()
        .expect("active connections mutex poisoned")
        .remove(device_id);
}

/// Raw bytes per `FileChunk` message (about 340 KB once base64-encoded).
const FILE_CHUNK_BYTES: usize = 256 * 1024;

/// Outgoing files are sent one at a time.
static SEND_LOCK: Mutex<()> = Mutex::new(());
static NEXT_TRANSFER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Stream a file to the specified peer as a series of small chunk messages.
/// Chunks are ordinary protocol lines, so other traffic is never blocked behind
/// the transfer and a stalled peer fails on a write timeout instead of hanging.
fn send_file_to_device(path_str: &str, device_id: &str, _state: &AppState, app: &AppHandle) {
    let path_str = path_str.to_string();
    let device_id = device_id.to_string();
    let app_clone = app.clone();

    std::thread::spawn(move || {
        let _one_at_a_time = SEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let state = app_clone.state::<AppState>();
        let result = send_file_chunks(&path_str, &device_id, &state, &app_clone);
        if let Err(e) = result {
            eprintln!("[file] Sending '{}' failed: {}", path_str, e);
        }
        reset_active_transfer(&state, &app_clone, &device_id);
    });
}

fn send_file_chunks(path_str: &str, device_id: &str, state: &AppState, app: &AppHandle) -> Result<(), String> {
    use crate::network::protocol::{write_line_json, ServerMessage};
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let path = std::path::Path::new(path_str);
    let file_name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
    let mut file = std::fs::File::open(path).map_err(|e| format!("open failed: {e}"))?;
    let file_size = file.metadata().map_err(|e| format!("metadata failed: {e}"))?.len();

    // Own handle on the socket: the shared map lock is not held while sending.
    let mut stream = {
        let streams = state.active_streams.lock().unwrap();
        streams
            .get(device_id)
            .ok_or_else(|| format!("no active stream for device {device_id}"))?
            .try_clone()
            .map_err(|e| format!("could not clone stream: {e}"))?
    };

    let transfer_id = format!(
        "{:x}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        NEXT_TRANSFER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );

    write_line_json(
        &mut stream,
        &ServerMessage::FileStart {
            transfer_id: transfer_id.clone(),
            filename: file_name.clone(),
            total_bytes: file_size,
        },
    )?;

    {
        let mut transfer = state.active_transfer.lock().unwrap();
        *transfer = Some(TransferProgress {
            device_id: device_id.to_string(),
            filename: file_name.clone(),
            bytes_received: 0,
            total_bytes: file_size,
            speed_bytes_per_sec: 0.0,
            status: "Sending".to_string(),
        });
    }
    let _ = refresh_tray_menu(app);
    let _ = app.emit("file-transfer-started", device_id);

    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; FILE_CHUNK_BYTES];
    let mut sent: u64 = 0;
    let start_time = std::time::Instant::now();
    let mut last_emit = start_time;

    let outcome: Result<(), String> = (|| {
        while sent < file_size {
            if crate::network::session::outgoing_aborted(&transfer_id) {
                return Err("peer aborted the transfer".to_string());
            }
            // Fill the buffer (File::read may return short reads); never read past
            // the announced size if the file grows while sending.
            let want = ((file_size - sent) as usize).min(FILE_CHUNK_BYTES);
            let mut filled = 0;
            while filled < want {
                match file.read(&mut buffer[filled..want]) {
                    Ok(0) => return Err("file shrank while sending".to_string()),
                    Ok(n) => filled += n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(format!("read failed: {e}")),
                }
            }
            hasher.update(&buffer[..filled]);
            write_line_json(
                &mut stream,
                &ServerMessage::FileChunk {
                    transfer_id: transfer_id.clone(),
                    base64_data: STANDARD.encode(&buffer[..filled]),
                },
            )?;
            sent += filled as u64;
            if last_emit.elapsed() >= std::time::Duration::from_millis(150) {
                last_emit = std::time::Instant::now();
                update_send_progress(device_id, sent, start_time, state, app);
            }
        }
        let sha256 = hasher.clone().finalize().iter().map(|b| format!("{:02x}", b)).collect::<String>();
        write_line_json(
            &mut stream,
            &ServerMessage::FileEnd { transfer_id: transfer_id.clone(), sha256 },
        )
    })();

    if let Err(e) = outcome {
        let _ = write_line_json(&mut stream, &ServerMessage::FileAbort { transfer_id });
        return Err(e);
    }
    println!("[file] Successfully sent '{}' to {}", file_name, device_id);
    Ok(())
}

fn update_send_progress(
    peer_device_id: &str,
    bytes_sent: u64,
    start_time: std::time::Instant,
    state: &AppState,
    app: &AppHandle,
) {
    let mut transfer = state.active_transfer.lock().unwrap();
    if let Some(ref mut progress) = *transfer {
        if progress.device_id == peer_device_id {
            progress.bytes_received = bytes_sent;
            let elapsed = start_time.elapsed().as_secs_f64();
            if elapsed > 0.0 {
                progress.speed_bytes_per_sec = (bytes_sent as f64) / elapsed;
            }
            progress.status = "Sending".to_string();
            let _ = app.emit("file-transfer-progress", progress.clone());
            
            let progress_pct = if progress.total_bytes > 0 {
                (progress.bytes_received as f64 / progress.total_bytes as f64 * 100.0).min(100.0)
            } else {
                0.0
            };
            update_tray_progress(app, progress_pct);
        }
    }
}

fn reset_active_transfer(state: &AppState, app: &AppHandle, device_id: &str) {
    {
        let mut transfer = state.active_transfer.lock().unwrap();
        *transfer = None;
    }
    let _ = refresh_tray_menu(app);
    let _ = app.emit("file-transfer-finished", device_id);
    restore_default_tray_icon(app);
}

/// Entry point invoked from `main.rs` (and the mobile entry point).
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mut builder = tauri::Builder::default();

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        builder = builder.plugin(tauri_plugin_updater::Builder::new().build());
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }));
    }

    builder
        .plugin(tauri_plugin_log::Builder::new().build())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        .setup(|app| {
            crate::system::virtual_mic::cleanup_pulse_modules();
            let trusted_peers = Arc::new(Mutex::new(
                TrustedPeers::load(app.handle()).map_err(setup_error)?,
            ));
            let pairing = Arc::new(PairingManager::default());
            let tray_menu = Menu::new(app)?;
            let state = AppState {
                trusted_peers,
                pairing,
                broadcasting: Arc::new(AtomicBool::new(true)),
                active_connections: Arc::new(Mutex::new(HashSet::new())),
                active_streams: Arc::new(Mutex::new(HashMap::new())),
                active_ips: Arc::new(Mutex::new(HashMap::new())),
                last_clipboard: Arc::new(Mutex::new(String::new())),
                copied_files: Arc::new(Mutex::new(Vec::new())),
                clipboard_lock: Arc::new(Mutex::new(())),
                tray_menu: tray_menu.clone(),
                active_transfer: Arc::new(Mutex::new(None)),
                terminal_server: Arc::new(Mutex::new(None)),
                audio_stream_server: Arc::new(Mutex::new(None)),
                virtual_camera_running: Arc::new(Mutex::new(None)),
                latest_camera_frame: Arc::new(Mutex::new(None)),
                virtual_mic_running: Arc::new(Mutex::new(None)),
            };
            start_background_services(app.handle(), &state).map_err(setup_error)?;
            let last_clipboard_clone = state.last_clipboard.clone();
            let copied_files_clone = state.copied_files.clone();
            let clipboard_lock_clone = state.clipboard_lock.clone();
            app.manage(state);

            let app_handle = app.handle().clone();
            let app_handle_for_files = app.handle().clone();
            std::thread::spawn(move || {
                let clipboard_ctx = ClipboardContext::new().unwrap();
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    // Hold clipboard_lock for the entire poll cycle so that any concurrent
                    // image write (session.rs) waits until we release before calling
                    // OpenClipboard. This prevents OSError 1418 on Windows.
                    let _lock = clipboard_lock_clone.lock().unwrap();

                    // 1. Text polling
                    if let Ok(text) = clipboard_ctx.get_text() {
                        let mut last = last_clipboard_clone.lock().unwrap();
                        let normalized_text = text.replace("\r\n", "\n");
                        let normalized_last = last.replace("\r\n", "\n");
                        if normalized_text != normalized_last && !normalized_text.is_empty() {
                            *last = normalized_text.clone();
                            let _ = app_handle.emit("desktop-clipboard-update", &normalized_text);
                        }
                    }
                    
                    // 2. Files polling
                    let should_refresh = if let Ok(files) = clipboard_ctx.get_files() {
                        let paths: Vec<String> = files.into_iter().map(|s| {
                            let s = s.trim_start_matches("file://");
                            urlencoding::decode(s).unwrap_or(std::borrow::Cow::Borrowed(s)).to_string()
                        }).collect();
                        
                        let mut last_files = copied_files_clone.lock().unwrap();
                        if paths != *last_files {
                            println!("[clipboard] Detected files copied: {:?}", paths);
                            *last_files = paths;
                            true
                        } else {
                            false
                        }
                        // lock is dropped here
                    } else {
                        let mut last_files = copied_files_clone.lock().unwrap();
                        if !last_files.is_empty() {
                            println!("[clipboard] Files cleared from clipboard");
                            last_files.clear();
                            true
                        } else {
                            false
                        }
                        // lock is dropped here
                    };
                    // Release clipboard_lock before refreshing the tray (no clipboard access there)
                    drop(_lock);

                    if should_refresh {
                        if let Err(e) = refresh_tray_menu(&app_handle_for_files) {
                            eprintln!("[clipboard] Failed to refresh tray menu: {e}");
                        }
                    }
                }
            });

            if let Some(window) = app.get_webview_window("main") {
                let window_clone = window.clone();
                let app_handle = app.handle().clone();
                window.on_window_event(move |event| {
                    match event {
                        tauri::WindowEvent::CloseRequested { api, .. } => {
                            api.prevent_close();
                            window_clone.hide().unwrap();
                        }
                        tauri::WindowEvent::ThemeChanged(_) => {
                            restore_default_tray_icon(&app_handle);
                        }
                        _ => {}
                    }
                });

                // Show the window on normal startup (when --minimized is not present)
                let args: Vec<String> = std::env::args().collect();
                if !args.iter().any(|arg| arg == "--minimized") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }

            populate_tray_menu(app.handle(), &tray_menu).map_err(|e| setup_error(e.to_string()))?;

            let is_dark = app.get_webview_window("main")
                .and_then(|w| w.theme().ok())
                .map(|t| t == tauri::Theme::Dark)
                .unwrap_or(true);
            let initial_icon = generate_logo_image(32, 32, is_dark);

            TrayIconBuilder::with_id(TRAY_ID)
                .icon(initial_icon)
                .menu(&tray_menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "quit" => {
                        let state = app.state::<AppState>();
                        if let Ok(mut mic_guard) = state.virtual_mic_running.lock() {
                            *mic_guard = None;
                        }
                        crate::system::virtual_mic::cleanup_pulse_modules();
                        app.exit(0);
                    }
                    "show" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "receiving_file" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                            let _ = app.emit("navigate-to-transfer-page", ());
                        }
                    }
                    id if id.starts_with("disconnect:") => {
                        let device_id = id.trim_start_matches("disconnect:");
                        let state = app.state::<AppState>();
                        disconnect_peer(device_id, &state);
                        let _ = app.emit("active-connections-changed", ());
                        let _ = refresh_tray_menu(app);
                    }
                    id if id.starts_with("send_file:") => {
                        let device_id = id.trim_start_matches("send_file:");
                        let state = app.state::<AppState>();
                        let file_path = {
                            let files = state.copied_files.lock().unwrap();
                            if !files.is_empty() {
                                Some(files[0].clone())
                            } else {
                                None
                            }
                        };
                        
                        if let Some(path_str) = file_path {
                            send_file_to_device(&path_str, device_id, &state, app);
                        }
                    }
                    id if id.starts_with("pick_files:") => {
                        let device_id = id.trim_start_matches("pick_files:").to_string();
                        let app_handle = app.clone();
                        use tauri_plugin_dialog::DialogExt;
                        app.dialog().file()
                            .add_filter("All Files", &["*"])
                            .pick_files(move |paths| {
                                if let Some(paths) = paths {
                                    let state = app_handle.state::<AppState>();
                                    for path in paths {
                                        let path_str = path.to_string();
                                        send_file_to_device(&path_str, &device_id, &state, &app_handle);
                                    }
                                }
                            });
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| match event {
                    TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } => {
                        let app = tray.app_handle();
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    _ => {}
                })
                .build(app)?;

            let app_handle = app.handle().clone();
            app.listen("active-connections-changed", move |_| {
                let _ = refresh_tray_menu(&app_handle);
            });

            let app_handle = app.handle().clone();
            app.listen(PEERS_CHANGED_EVENT, move |_| {
                let _ = refresh_tray_menu(&app_handle);
            });

            Ok(())
        })
        .register_uri_scheme_protocol("sync-stream", |ctx, _request| {
            let app_handle = ctx.app_handle();
            let state = app_handle.state::<AppState>();
            let frame_bytes = {
                let guard = state.latest_camera_frame.lock().unwrap();
                guard.clone()
            };

            match frame_bytes {
                Some(bytes) => {
                    tauri::http::Response::builder()
                        .header("Content-Type", "image/jpeg")
                        .header("Cache-Control", "no-cache, no-store, must-revalidate")
                        .header("Pragma", "no-cache")
                        .header("Expires", "0")
                        .body(bytes)
                        .unwrap()
                }
                None => {
                    tauri::http::Response::builder()
                        .status(404)
                        .body(Vec::new())
                        .unwrap()
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::greet::greet,
            commands::device::get_device_ip,
            commands::device::get_device_name,
            commands::pairing::list_pending_pair_requests,
            commands::pairing::accept_pair_request,
            commands::pairing::reject_pair_request,
            commands::pairing::list_trusted_peers,
            commands::pairing::unpair_peer,
            commands::discovery::set_broadcasting,
            commands::discovery::get_broadcasting,
            commands::clipboard::set_device_clipboard_sync,
            commands::media::set_device_media_controls,
            commands::media::set_device_volume_sync,
            commands::device::set_device_incoming_files,
            commands::device::set_device_terminal_access,
            commands::device::set_device_audio_streaming,
            commands::device::get_active_transfer,
            commands::device::check_system_deps,
            commands::device::auto_install_system_dep,
            commands::updater::relaunch_app,
            commands::camera::get_camera_stream_state,
            commands::camera::toggle_camera_stream,
            commands::camera::update_camera_config,
            commands::camera::request_camera_config,
            commands::camera::stop_virtual_camera,
            commands::camera::get_latest_frame,
            commands::mic::get_mic_stream_state,
            commands::mic::toggle_mic_stream,
            commands::mic::stop_virtual_mic,
            commands::mic::set_virtual_mic_muted,
            commands::mic::set_virtual_mic_volume,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| match event {
            tauri::RunEvent::Exit | tauri::RunEvent::ExitRequested { .. } => {
                let state = app_handle.state::<AppState>();
                if let Ok(mut mic_guard) = state.virtual_mic_running.lock() {
                    *mic_guard = None;
                }
                crate::system::virtual_mic::cleanup_pulse_modules();
            }
            _ => {}
        });
}
