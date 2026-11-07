use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tauri::{Emitter, Manager, State};
use crate::app::AppState;

#[tauri::command]
pub async fn get_camera_stream_state(
    state: State<'_, AppState>,
) -> Result<bool, String> {
    let running_guard = state.virtual_camera_running.lock().unwrap();
    Ok(running_guard.is_some())
}

#[tauri::command]
pub async fn toggle_camera_stream(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    device_id: String,
    start: bool,
) -> Result<(), String> {
    if !start {
        let _ = stop_virtual_camera(state.clone()).await;
        let _ = app.emit("camera-stream-state-changed", serde_json::json!({
            "streaming": false,
        }));
    }

    let mut active_streams = state.active_streams.lock().unwrap();
    if let Some(stream) = active_streams.get_mut(&device_id) {
        let message = if start {
            crate::network::protocol::ServerMessage::StartCameraStream
        } else {
            crate::network::protocol::ServerMessage::StopCameraStream
        };
        let _ = crate::network::protocol::write_line_json(stream, &message);
        Ok(())
    } else if !start {
        Ok(())
    } else {
        Err("Device is not connected".into())
    }
}

#[tauri::command]
pub async fn update_camera_config(
    state: State<'_, AppState>,
    device_id: String,
    is_front: Option<bool>,
    resolution: Option<String>,
    fps: Option<i32>,
    rotation: Option<i32>,
    use_adb: Option<bool>,
) -> Result<(), String> {
    let mut active_streams = state.active_streams.lock().unwrap();
    if let Some(stream) = active_streams.get_mut(&device_id) {
        let message = crate::network::protocol::ServerMessage::UpdateCameraConfig {
            is_front,
            resolution,
            fps,
            rotation,
            use_adb,
        };
        crate::network::protocol::write_line_json(stream, &message)?;
        Ok(())
    } else {
        Err("Device is not connected".into())
    }
}

#[tauri::command]
pub async fn request_camera_config(
    state: State<'_, AppState>,
    device_id: String,
) -> Result<(), String> {
    let mut active_streams = state.active_streams.lock().unwrap();
    if let Some(stream) = active_streams.get_mut(&device_id) {
        let message = crate::network::protocol::ServerMessage::RequestCameraConfig;
        crate::network::protocol::write_line_json(stream, &message)?;
        Ok(())
    } else {
        Err("Device is not connected".into())
    }
}

fn create_command(program: &str) -> std::process::Command {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new(program);
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        cmd
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new(program)
    }
}

fn find_adb_path() -> String {
    let adb_name = if cfg!(target_os = "windows") { "adb.exe" } else { "adb" };

    // 1. Try raw "adb" in PATH
    if create_command(adb_name).arg("--version").output().is_ok() {
        return adb_name.to_string();
    }

    // 2. Try standard Android SDK paths
    #[cfg(not(target_os = "windows"))]
    if let Ok(home) = std::env::var("HOME") {
        let sdk_path = format!("{}/Android/Sdk/platform-tools/adb", home);
        if std::path::Path::new(&sdk_path).exists() {
            return sdk_path;
        }
        // Check other common Unix/macOS locations
        for path in &[
            "/usr/bin/adb", 
            "/usr/local/bin/adb", 
            "/opt/android-sdk/platform-tools/adb",
            "/Library/Android/sdk/platform-tools/adb" // macOS default
        ] {
            if std::path::Path::new(path).exists() {
                return path.to_string();
            }
        }
    }

    #[cfg(target_os = "windows")]
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        let sdk_path = format!("{}\\Android\\Sdk\\platform-tools\\adb.exe", local_app_data);
        if std::path::Path::new(&sdk_path).exists() {
            return sdk_path;
        }
    }

    adb_name.to_string()
}

/// Called from the network session (not exposed to the webview).
pub async fn start_virtual_camera(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    ip: String,
    port: u16,
    use_adb: bool,
    token: String,
) -> Result<(), String> {
    let mut running_guard = state.virtual_camera_running.lock().unwrap();
    if running_guard.is_some() {
        return Ok(()); // Already running
    }

    if use_adb {
        let adb_cmd = find_adb_path();
        println!("[camera] Setting up ADB port forwarding: {} forward tcp:40000 tcp:{}", adb_cmd, port);
        let output = create_command(&adb_cmd)
            .args(&["forward", "tcp:40000", &format!("tcp:{}", port)])
            .output();
        match output {
            Ok(out) if out.status.success() => {
                println!("[camera] ADB port forwarding set up successfully");
            }
            Ok(out) => {
                let err = String::from_utf8_lossy(&out.stderr).to_string();
                eprintln!("[camera] ADB port forwarding failed: {}", err);
                return Err(format!("ADB forward failed: {}", err));
            }
            Err(e) => {
                eprintln!("[camera] Failed to execute adb command (path: {}): {}", adb_cmd, e);
                return Err(format!("Failed to execute adb (path: {}): {e}. Please ensure ADB is installed.", adb_cmd));
            }
        }
    }

    let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel::<()>();
    *running_guard = Some(cancel_tx);

    let (preview_tx, _) = tokio::sync::broadcast::channel::<Vec<u8>>(2);
    let preview_tx_server = preview_tx.clone();

    // Start local preview relay server on 127.0.0.1:40001
    tokio::spawn(async move {
        if let Ok(listener) = tokio::net::TcpListener::bind("127.0.0.1:40001").await {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut rx = preview_tx_server.subscribe();
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    let header = "HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\nCache-Control: no-cache, no-store\r\nConnection: close\r\n\r\n";
                    if socket.write_all(header.as_bytes()).await.is_err() {
                        return;
                    }
                    let boundary_prefix = b"--frame\r\nContent-Type: image/jpeg\r\nContent-Length: ";
                    let crlf2 = b"\r\n\r\n";
                    while let Ok(jpeg) = rx.recv().await {
                        if socket.write_all(boundary_prefix).await.is_err() { break; }
                        if socket.write_all(jpeg.len().to_string().as_bytes()).await.is_err() { break; }
                        if socket.write_all(crlf2).await.is_err() { break; }
                        if socket.write_all(&jpeg).await.is_err() { break; }
                        if socket.write_all(b"\r\n").await.is_err() { break; }
                    }
                });
            }
        }
    });

    let virtual_camera_running = state.virtual_camera_running.clone();
    let app_handle = app.clone();
    let latest_frame = state.latest_camera_frame.clone();
    tokio::spawn(async move {
        println!("[camera] Starting virtual camera background thread...");

        #[cfg(target_os = "linux")]
        {
            let _ = app_handle.emit(
                "virtual-camera-state-changed",
                serde_json::json!({
                    "active": false,
                    "error": "Preparing driver (check for password prompt)...",
                }),
            );
            if let Err(e) = prepare_linux_driver() {
                eprintln!("[camera] Driver preparation failed: {e}");
                let _ = app_handle.emit(
                    "virtual-camera-state-changed",
                    serde_json::json!({
                        "active": false,
                        "error": format!("Driver initialization failed: {e}"),
                    }),
                );
                
                let state = app_handle.state::<AppState>();
                let mut active_streams = state.active_streams.lock().unwrap();
                for stream in active_streams.values_mut() {
                    let message = crate::network::protocol::ServerMessage::StopCameraStream;
                    let _ = crate::network::protocol::write_line_json(stream, &message);
                }
                let mut guard = state.virtual_camera_running.lock().unwrap();
                *guard = None;
                return;
            }
        }

        #[cfg(target_os = "windows")]
        {
            let _ = app_handle.emit(
                "virtual-camera-state-changed",
                serde_json::json!({
                    "active": false,
                    "error": "Preparing driver (check for administrator prompt)...",
                }),
            );
            if let Err(e) = prepare_windows_driver(&app_handle) {
                eprintln!("[camera] Driver preparation failed: {e}");
                let _ = app_handle.emit(
                    "virtual-camera-state-changed",
                    serde_json::json!({
                        "active": false,
                        "error": format!("Driver initialization failed: {e}"),
                    }),
                );
                
                let state = app_handle.state::<AppState>();
                let mut active_streams = state.active_streams.lock().unwrap();
                for stream in active_streams.values_mut() {
                    let message = crate::network::protocol::ServerMessage::StopCameraStream;
                    let _ = crate::network::protocol::write_line_json(stream, &message);
                }
                let mut guard = state.virtual_camera_running.lock().unwrap();
                *guard = None;
                return;
            }
        }

        let target_ip = if use_adb { "127.0.0.1".to_string() } else { ip };
        let target_port = if use_adb { 40000 } else { port };

        let mut retry_count = 0;
        let max_retries = 5;

        loop {
            // Check if cancelled before starting loop iteration
            let is_cancelled = match cancel_rx.try_recv() {
                Ok(_) | Err(tokio::sync::oneshot::error::TryRecvError::Closed) => true,
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => false,
            };
            if is_cancelled {
                println!("[camera] Virtual camera stream cancelled.");
                break;
            }

            println!("[camera] Connecting to camera stream (attempt {}/{})...", retry_count + 1, max_retries);
            
            match run_virtual_camera_loop(app_handle.clone(), target_ip.clone(), target_port, token.clone(), latest_frame.clone(), preview_tx.clone(), &mut cancel_rx).await {
                Ok(_) => {
                    println!("[camera] Virtual camera stream closed.");
                    break;
                }
                Err(e) => {
                    eprintln!("[camera] Virtual camera loop error: {e}");
                    
                    // Check if cancelled during the loop
                    let is_cancelled_during = match cancel_rx.try_recv() {
                        Ok(_) | Err(tokio::sync::oneshot::error::TryRecvError::Closed) => true,
                        Err(tokio::sync::oneshot::error::TryRecvError::Empty) => false,
                    };
                    if is_cancelled_during {
                        println!("[camera] Virtual camera stream cancelled after error.");
                        break;
                    }

                    retry_count += 1;
                    if retry_count >= max_retries {
                        let _ = app_handle.emit(
                            "virtual-camera-state-changed",
                            serde_json::json!({
                                "active": false,
                                "error": format!("Failed after {} attempts: {}", max_retries, e),
                            }),
                        );
                        break;
                    }

                    let _ = app_handle.emit(
                        "virtual-camera-state-changed",
                        serde_json::json!({
                            "active": false,
                            "error": format!("Connection lost. Retrying in 2s ({}/{})...", retry_count, max_retries),
                        }),
                    );

                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        }
        println!("[camera] Virtual camera background thread stopped.");
        
        let mut guard = virtual_camera_running.lock().unwrap();
        *guard = None;
        drop(guard);

        if use_adb {
            let adb_cmd = find_adb_path();
            println!("[camera] Removing ADB port forwarding: {} forward --remove tcp:40000", adb_cmd);
            let _ = create_command(&adb_cmd)
                .args(&["forward", "--remove", "tcp:40000"])
                .output();
        }

        let _ = app_handle.emit(
            "virtual-camera-state-changed",
            serde_json::json!({
                "active": false,
                "error": null,
            }),
        );
    });

    Ok(())
}


#[tauri::command]
pub async fn stop_virtual_camera(state: State<'_, AppState>) -> Result<(), String> {
    let mut running_guard = state.virtual_camera_running.lock().unwrap();
    if let Some(cancel_tx) = running_guard.take() {
        let _ = cancel_tx.send(());
    }
    Ok(())
}

async fn run_virtual_camera_loop(
    app: tauri::AppHandle,
    ip: String,
    port: u16,
    token: String,
    latest_frame: std::sync::Arc<std::sync::Mutex<Option<Vec<u8>>>>,
    preview_tx: tokio::sync::broadcast::Sender<Vec<u8>>,
    cancel_rx: &mut tokio::sync::oneshot::Receiver<()>,
) -> Result<(), String> {
    let url = format!("{}:{}", ip, port);
    
    let stream_future = tokio::net::TcpStream::connect(&url);
    let mut stream = tokio::time::timeout(std::time::Duration::from_secs(5), stream_future)
        .await
        .map_err(|_| format!("Connection timeout to MJPEG server at {url}"))?
        .map_err(|e| format!("Failed to connect to MJPEG server at {url}: {e}"))?;

    let request = format!(
        "GET /{} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        token, url
    );
    let write_future = stream.write_all(request.as_bytes());
    tokio::time::timeout(std::time::Duration::from_secs(5), write_future)
        .await
        .map_err(|_| "Timeout sending HTTP GET request".to_string())?
        .map_err(|e| format!("Failed to send HTTP GET: {e}"))?;

    let mut reader = tokio::io::BufReader::with_capacity(65536, stream);
    let mut buffer = Vec::with_capacity(131072);
    let mut chunk = vec![0u8; 65536];

    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(2);
    let app_worker = app.clone();

    // Spawn a dedicated blocking worker for fast JPEG decoding & virtual camera feeding
    let worker_handle = tokio::task::spawn_blocking(move || {
        let mut current_dim: Option<(u32, u32)> = None;
        #[cfg(target_os = "linux")]
        let mut linux_cam: Option<linux_cam::LinuxVirtualCamera> = None;
        #[cfg(target_os = "linux")]
        let mut yuyv_converter = FastRgbToYuyv::new(0, 0);

        #[cfg(target_os = "windows")]
        let mut win_cam: Option<win_cam::WinVirtualCamera> = None;
        #[cfg(target_os = "windows")]
        let mut nv12_converter = FastRgbToNv12::new(0, 0);

        let mut rgb_buffer: Vec<u8> = Vec::new();

        while let Some(jpeg_bytes) = frame_rx.blocking_recv() {
            let mut decoder = zune_jpeg::JpegDecoder::new(&jpeg_bytes);
            if decoder.decode_headers().is_err() {
                continue;
            }
            let info = match decoder.info() {
                Some(i) => i,
                None => continue,
            };
            let w = info.width as usize;
            let h = info.height as usize;
            let req_size = decoder.output_buffer_size().unwrap_or(w * h * 3);
            if rgb_buffer.len() < req_size {
                rgb_buffer.resize(req_size, 0);
            }
            if decoder.decode_into(&mut rgb_buffer).is_err() {
                continue;
            }

            let new_dim = (w as u32, h as u32);
            if current_dim != Some(new_dim) {
                current_dim = Some(new_dim);
                #[cfg(target_os = "linux")]
                {
                    linux_cam = None;
                    match linux_cam::LinuxVirtualCamera::create(w as u32, h as u32) {
                        Ok(c) => {
                            linux_cam = Some(c);
                            let _ = app_worker.emit(
                                "virtual-camera-state-changed",
                                serde_json::json!({
                                    "active": true,
                                    "error": null,
                                }),
                            );
                        }
                        Err(e) => {
                            eprintln!("[camera] Failed to create virtual camera: {e}");
                            let _ = app_worker.emit(
                                "virtual-camera-state-changed",
                                serde_json::json!({
                                    "active": false,
                                    "error": format!("Failed to create virtual camera: {e}"),
                                }),
                            );
                        }
                    }
                }
                #[cfg(target_os = "windows")]
                {
                    win_cam = None;
                    match win_cam::WinVirtualCamera::create(w as u32, h as u32) {
                        Ok(c) => {
                            win_cam = Some(c);
                            let _ = app_worker.emit(
                                "virtual-camera-state-changed",
                                serde_json::json!({
                                    "active": true,
                                    "error": null,
                                }),
                            );
                        }
                        Err(e) => {
                            eprintln!("[camera] Failed to create virtual camera: {e}");
                            let _ = app_worker.emit(
                                "virtual-camera-state-changed",
                                serde_json::json!({
                                    "active": false,
                                    "error": format!("Failed to create virtual camera: {e}"),
                                }),
                            );
                        }
                    }
                }
            }

            #[cfg(target_os = "linux")]
            {
                if let Some(ref mut c) = linux_cam {
                    let yuyv = yuyv_converter.convert(&rgb_buffer, w, h);
                    let _ = c.send(yuyv);
                }
            }

            #[cfg(target_os = "windows")]
            {
                if let Some(ref mut c) = win_cam {
                    let nv12 = nv12_converter.convert(&rgb_buffer, w, h);
                    let _ = c.send(nv12);
                }
            }
        }
    });

    let mut result = Ok(());

    loop {
        tokio::select! {
            _ = &mut *cancel_rx => {
                break;
            }
            read_res = tokio::time::timeout(std::time::Duration::from_secs(15), reader.read(&mut chunk)) => {
                let bytes_read = match read_res {
                    Ok(Ok(n)) => n,
                    Ok(Err(e)) => {
                        result = Err(format!("Read error: {e}"));
                        break;
                    }
                    Err(_) => {
                        result = Err("Read timeout: no camera data received for 15 seconds".into());
                        break;
                    }
                };
                if bytes_read == 0 {
                    result = Err("Camera stream ended by peer".into());
                    break;
                }
                buffer.extend_from_slice(&chunk[..bytes_read]);

                while let Some(start_idx) = find_subsequence(&buffer, &[0xFF, 0xD8]) {
                    if start_idx > 0 {
                        buffer.drain(0..start_idx);
                        continue;
                    }

                    if let Some(end_idx) = find_subsequence(&buffer, &[0xFF, 0xD9]) {
                        let actual_end_idx = end_idx + 2;
                        let jpeg_bytes = buffer[0..actual_end_idx].to_vec();

                        {
                            let mut guard = latest_frame.lock().unwrap();
                            *guard = Some(jpeg_bytes.clone());
                        }

                        // Send to local preview relay server
                        let _ = preview_tx.send(jpeg_bytes.clone());

                        // Try sending to the background decode/virtualcam worker
                        // If worker is busy decoding the previous frame, drop this one to prevent TCP stall and lag
                        let _ = frame_tx.try_send(jpeg_bytes);

                        buffer.drain(0..actual_end_idx);
                    } else {
                        break;
                    }
                }

                if find_subsequence(&buffer, &[0xFF, 0xD8]).is_none() && buffer.len() > 4096 {
                    buffer.drain(0..buffer.len() - 1);
                }
            }
        }
    }

    // Drop sender to signal worker thread to terminate
    drop(frame_tx);
    let _ = worker_handle.await;

    {
        let mut guard = latest_frame.lock().unwrap();
        *guard = None;
    }
    result
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

pub struct FastRgbToNv12 {
    buffer: Vec<u8>,
}

impl FastRgbToNv12 {
    pub fn new(w: usize, h: usize) -> Self {
        let y_size = w * h;
        let uv_size = y_size / 2;
        Self {
            buffer: vec![0u8; y_size + uv_size],
        }
    }

    #[inline]
    pub fn convert(&mut self, rgb: &[u8], w: usize, h: usize) -> &[u8] {
        let y_size = w * h;
        let total_size = y_size + (y_size / 2);
        if self.buffer.len() != total_size {
            self.buffer.resize(total_size, 0);
        }

        let (y_plane, uv_plane) = self.buffer.split_at_mut(y_size);

        let row_pair_size = w * 2;
        let rgb_pair_size = row_pair_size * 3;

        for (pair_idx, (y_2rows, uv_row)) in y_plane.chunks_exact_mut(row_pair_size).zip(uv_plane.chunks_exact_mut(w)).enumerate() {
            let rgb_offset = pair_idx * rgb_pair_size;
            if rgb_offset + rgb_pair_size > rgb.len() {
                break;
            }

            let (y_row0, y_row1) = y_2rows.split_at_mut(w);
            let rgb0 = &rgb[rgb_offset..rgb_offset + w * 3];
            let rgb1 = &rgb[rgb_offset + w * 3..rgb_offset + rgb_pair_size];

            for x in (0..w).step_by(2) {
                let x3_0 = x * 3;
                let x3_1 = x3_0 + 3;

                let r00 = rgb0[x3_0] as i32;
                let g00 = rgb0[x3_0 + 1] as i32;
                let b00 = rgb0[x3_0 + 2] as i32;

                let r01 = rgb0[x3_1] as i32;
                let g01 = rgb0[x3_1 + 1] as i32;
                let b01 = rgb0[x3_1 + 2] as i32;

                let r10 = rgb1[x3_0] as i32;
                let g10 = rgb1[x3_0 + 1] as i32;
                let b10 = rgb1[x3_0 + 2] as i32;

                let r11 = rgb1[x3_1] as i32;
                let g11 = rgb1[x3_1 + 1] as i32;
                let b11 = rgb1[x3_1 + 2] as i32;

                // Standard Rec.601 limited video range
                let y00 = ((66 * r00 + 129 * g00 + 25 * b00 + 128) >> 8) + 16;
                let y01 = ((66 * r01 + 129 * g01 + 25 * b01 + 128) >> 8) + 16;
                let y10 = ((66 * r10 + 129 * g10 + 25 * b10 + 128) >> 8) + 16;
                let y11 = ((66 * r11 + 129 * g11 + 25 * b11 + 128) >> 8) + 16;

                y_row0[x] = y00.clamp(0, 255) as u8;
                y_row0[x + 1] = y01.clamp(0, 255) as u8;
                y_row1[x] = y10.clamp(0, 255) as u8;
                y_row1[x + 1] = y11.clamp(0, 255) as u8;

                // Average 2x2 chroma subsampling
                let r_avg = (r00 + r01 + r10 + r11 + 2) >> 2;
                let g_avg = (g00 + g01 + g10 + g11 + 2) >> 2;
                let b_avg = (b00 + b01 + b10 + b11 + 2) >> 2;

                let u = ((-38 * r_avg - 74 * g_avg + 112 * b_avg + 128) >> 8) + 128;
                let v = ((112 * r_avg - 94 * g_avg - 18 * b_avg + 128) >> 8) + 128;

                uv_row[x] = u.clamp(0, 255) as u8;
                uv_row[x + 1] = v.clamp(0, 255) as u8;
            }
        }
        &self.buffer
    }
}


#[allow(dead_code)]
pub struct FastRgbToYuyv {
    buffer: Vec<u8>,
}

#[allow(dead_code)]
impl FastRgbToYuyv {
    pub fn new(w: usize, h: usize) -> Self {
        Self {
            buffer: vec![0u8; w * h * 2],
        }
    }

    #[inline]
    pub fn convert(&mut self, rgb: &[u8], w: usize, h: usize) -> &[u8] {
        let expected_len = w * h * 2;
        if self.buffer.len() != expected_len {
            self.buffer.resize(expected_len, 0);
        }
        let yuyv = &mut self.buffer;

        for chunk_idx in 0..(w * h / 2) {
            let rgb_idx1 = chunk_idx * 6;
            let rgb_idx2 = rgb_idx1 + 3;
            let yuyv_idx = chunk_idx * 4;

            let r1 = rgb[rgb_idx1] as i32;
            let g1 = rgb[rgb_idx1 + 1] as i32;
            let b1 = rgb[rgb_idx1 + 2] as i32;

            let r2 = rgb[rgb_idx2] as i32;
            let g2 = rgb[rgb_idx2 + 1] as i32;
            let b2 = rgb[rgb_idx2 + 2] as i32;

            let y0 = ((66 * r1 + 129 * g1 + 25 * b1 + 128) >> 8) + 16;
            let y1 = ((66 * r2 + 129 * g2 + 25 * b2 + 128) >> 8) + 16;

            let r_avg = (r1 + r2) >> 1;
            let g_avg = (g1 + g2) >> 1;
            let b_avg = (b1 + b2) >> 1;

            let u = ((-38 * r_avg - 74 * g_avg + 112 * b_avg + 128) >> 8) + 128;
            let v = ((112 * r_avg - 94 * g_avg - 18 * b_avg + 128) >> 8) + 128;

            yuyv[yuyv_idx] = y0.clamp(0, 255) as u8;
            yuyv[yuyv_idx + 1] = u.clamp(0, 255) as u8;
            yuyv[yuyv_idx + 2] = y1.clamp(0, 255) as u8;
            yuyv[yuyv_idx + 3] = v.clamp(0, 255) as u8;
        }

        &self.buffer
    }
}

// ---------------------------------------------------------------------------
// Virtual camera driver registration check
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub fn is_driver_registered() -> bool {
    #[cfg(target_os = "windows")]
    {
        is_windows_driver_registered()
    }
    #[cfg(target_os = "macos")]
    {
        is_macos_driver_registered()
    }
    #[cfg(target_os = "linux")]
    {
        is_linux_driver_registered()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

#[cfg(target_os = "linux")]
fn is_linux_driver_registered() -> bool {
    std::path::Path::new("/dev/video9").exists()
}

#[cfg(target_os = "macos")]
fn is_macos_driver_registered() -> bool {
    false
}

#[cfg(target_os = "linux")]
fn prepare_linux_driver() -> Result<(), String> {
    if !std::path::Path::new("/dev/video9").exists() {
        println!("[camera] /dev/video9 does not exist. Loading v4l2loopback module...");
        let status = create_command("pkexec")
            .args(&["modprobe", "v4l2loopback", "exclusive_caps=1", "card_label=Sync Camera", "video_nr=9"])
            .status()
            .map_err(|e| format!("Failed to run pkexec modprobe: {e}"))?;
        if !status.success() {
            return Err("Failed to load v4l2loopback module".into());
        }
    }

    // Check permissions and fix if needed before running v4l2-ctl
    let test_open = std::fs::OpenOptions::new().write(true).open("/dev/video9");
    if let Err(ref e) = test_open {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            println!("[camera] Permission denied for /dev/video9. Attempting to fix permissions using pkexec chmod...");
            let chmod_status = create_command("pkexec")
                .args(&["chmod", "0666", "/dev/video9"])
                .status()
                .map_err(|e| format!("Failed to run pkexec chmod: {e}"))?;
            if !chmod_status.success() {
                return Err("Failed to set write permissions on /dev/video9".into());
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn is_windows_driver_registered() -> bool {
    let output = create_command("reg")
        .args(&["query", "HKCR\\CLSID\\{A3FCE0F5-3493-419F-958A-ABA1250EC20B}\\InprocServer32", "/ve"])
        .output();

    if let Ok(out) = output {
        if out.status.success() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            for line in stdout.lines() {
                if line.contains("REG_SZ") {
                    let parts: Vec<&str> = line.split("REG_SZ").collect();
                    if let Some(path_str) = parts.get(1) {
                        let path = path_str.trim().trim_start_matches(r"\\?\");
                        if !path.is_empty() && std::path::Path::new(path).exists() {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

#[cfg(target_os = "windows")]
fn is_windows_driver_renamed() -> bool {
    let output = create_command("reg")
        .args(&[
            "query",
            "HKLM\\SOFTWARE\\Classes\\CLSID\\{860BB310-5D01-11d0-BD3B-00A0C911CE86}\\Instance\\{A3FCE0F5-3493-419F-958A-ABA1250EC20B}",
            "/v",
            "FriendlyName",
        ])
        .output();

    if let Ok(out) = output {
        if out.status.success() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            return stdout.contains("Sync Camera");
        }
    }
    false
}

#[cfg(target_os = "windows")]
pub fn prepare_windows_driver(app: &tauri::AppHandle) -> Result<(), String> {
    if is_windows_driver_registered() {
        println!("[camera] Virtual camera driver is registered and verified on disk. Ready for streaming.");
        return Ok(());
    }

    use tauri::path::BaseDirectory;
    let mut dll_path = app
        .path()
        .resolve("resources/obs-virtualcam-module64.dll", BaseDirectory::Resource)
        .ok();

    if dll_path.as_ref().map(|p| !p.exists()).unwrap_or(true) {
        let dev_path = std::path::PathBuf::from("src-tauri/resources/obs-virtualcam-module64.dll");
        if dev_path.exists() {
            dll_path = Some(dev_path);
        } else {
            let res_path = std::path::PathBuf::from("resources/obs-virtualcam-module64.dll");
            if res_path.exists() {
                dll_path = Some(res_path);
            }
        }
    }

    let dll_path = match dll_path {
        Some(p) if p.exists() => {
            let canonical = std::fs::canonicalize(&p).unwrap_or(p);
            let path_str = canonical.to_string_lossy().to_string();
            let clean_path = path_str.trim_start_matches(r"\\?\").to_string();
            std::path::PathBuf::from(clean_path)
        }
        _ => {
            return Err("Bundled virtual camera module obs-virtualcam-module64.dll not found.".into());
        }
    };

    println!("[camera] Registering virtual camera driver at {:?} with elevated prompt...", dll_path);

    let ps_script = format!(
        "regsvr32.exe /s \"{}\"; \
         Set-ItemProperty -Path 'HKLM:\\SOFTWARE\\Classes\\CLSID\\{{860BB310-5D01-11d0-BD3B-00A0C911CE86}}\\Instance\\{{A3FCE0F5-3493-419F-958A-ABA1250EC20B}}' -Name 'FriendlyName' -Value 'Sync Camera' -Force -ErrorAction SilentlyContinue; \
         Set-ItemProperty -Path 'HKLM:\\SOFTWARE\\Classes\\CLSID\\{{A3FCE0F5-3493-419F-958A-ABA1250EC20B}}' -Name '(Default)' -Value 'Sync Camera' -Force -ErrorAction SilentlyContinue",
        dll_path.to_string_lossy().replace('\'', "''")
    );

    let utf16_bytes: Vec<u8> = ps_script
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let encoded_cmd = STANDARD.encode(&utf16_bytes);

    let status = create_command("powershell")
        .args(&[
            "-NoProfile",
            "-Command",
            &format!("Start-Process powershell -ArgumentList '-NoProfile', '-EncodedCommand', '{}' -Verb RunAs -Wait", encoded_cmd),
        ])
        .status()
        .map_err(|e| format!("Failed to launch elevated registration: {e}"))?;

    if !status.success() {
        return Err("Registration failed or was cancelled by the user.".into());
    }

    if !is_windows_driver_registered() {
        return Err("DLL registration command completed, but virtual camera driver is not registered. Please ensure Administrator permission was granted.".into());
    }

    println!("[camera] Virtual camera driver registered successfully!");
    Ok(())
}

#[cfg(target_os = "linux")]
mod linux_cam {
    use std::fs::File;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use super::create_command;

    pub struct LinuxVirtualCamera(File);

    impl LinuxVirtualCamera {
        pub fn create(w: u32, h: u32) -> Result<Self, String> {
            // Set the video format on the device BEFORE opening the file writer.
            // Setting format fails with EBUSY if a writer is already holding the device descriptor.
            let format_status = create_command("v4l2-ctl")
                .args(&[
                    "-d", "/dev/video9",
                    "--set-fmt-video-out",
                    &format!("width={},height={},pixelformat=YUYV", w, h)
                ])
                .status()
                .map_err(|e| format!("Failed to run v4l2-ctl: {e}"))?;

            if !format_status.success() {
                eprintln!("[camera] Warning: v4l2-ctl returned non-zero status");
            }

            // Open the file descriptor to write MJPEG/YUYV frames.
            let file = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(0x800) // O_NONBLOCK (2048) on Linux
                .open("/dev/video9")
                .map_err(|e| format!("Failed to open /dev/video9 for writing: {e}"))?;

            Ok(LinuxVirtualCamera(file))
        }

        pub fn send(&mut self, yuyv: &[u8]) -> Result<(), String> {
            match self.0.write_all(yuyv) {
                Ok(_) => {
                    let _ = self.0.flush();
                    Ok(())
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // Consumer is slow, drop the frame to prevent freezing
                    Ok(())
                }
                Err(e) => Err(e.to_string()),
            }
        }
    }
}

#[cfg(target_os = "windows")]
mod win_cam {
    use virtualcam::{Camera, PixelFormat};

    pub struct WinVirtualCamera(Camera);

    impl WinVirtualCamera {
        pub fn create(w: u32, h: u32) -> Result<Self, String> {
            let cam = Camera::builder(w, h, 30.0)
                .format(PixelFormat::NV12)
                .build()
                .map_err(|e| format!("Failed to create Windows virtual camera: {e}"))?;
            Ok(WinVirtualCamera(cam))
        }

        pub fn send(&mut self, nv12: &[u8]) -> Result<(), String> {
            self.0.send(nv12).map_err(|e| e.to_string())
        }
    }
}

#[tauri::command]
pub async fn get_latest_frame(
    state: State<'_, AppState>,
) -> Result<Option<String>, String> {
    let guard = state.latest_camera_frame.lock().unwrap();
    if let Some(ref bytes) = *guard {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        Ok(Some(STANDARD.encode(bytes)))
    } else {
        Ok(None)
    }
}

