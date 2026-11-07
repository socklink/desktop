use tauri::{AppHandle, Emitter, State};
use crate::app::AppState;

fn create_command(program: &str) -> std::process::Command {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new(program);
        cmd.creation_flags(0x08000000);
        cmd
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new(program)
    }
}

fn find_adb_path() -> String {
    let adb_name = if cfg!(target_os = "windows") { "adb.exe" } else { "adb" };

    if create_command(adb_name).arg("--version").output().is_ok() {
        return adb_name.to_string();
    }

    #[cfg(not(target_os = "windows"))]
    if let Ok(home) = std::env::var("HOME") {
        let sdk_path = format!("{}/Android/Sdk/platform-tools/adb", home);
        if std::path::Path::new(&sdk_path).exists() {
            return sdk_path;
        }
        for path in &[
            "/usr/bin/adb",
            "/usr/local/bin/adb",
            "/opt/android-sdk/platform-tools/adb",
            "/Library/Android/sdk/platform-tools/adb"
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

#[tauri::command]
pub async fn get_mic_stream_state(
    state: State<'_, AppState>,
) -> Result<bool, String> {
    let running_guard = state.virtual_mic_running.lock().unwrap();
    Ok(running_guard.is_some())
}

#[tauri::command]
pub async fn toggle_mic_stream(
    app: AppHandle,
    state: State<'_, AppState>,
    device_id: String,
    start: bool,
) -> Result<(), String> {
    if !start {
        let _ = stop_virtual_mic(state.clone()).await;
        let _ = app.emit("mic-stream-state-changed", serde_json::json!({
            "streaming": false,
        }));
        let _ = app.emit("mic-audio-level", 0.0);
    }

    let mut active_streams = state.active_streams.lock().unwrap();
    if let Some(stream) = active_streams.get_mut(&device_id) {
        let message = if start {
            crate::network::protocol::ServerMessage::StartMicStream
        } else {
            crate::network::protocol::ServerMessage::StopMicStream
        };
        let _ = crate::network::protocol::write_line_json(stream, &message);
        Ok(())
    } else if !start {
        Ok(())
    } else {
        Err("Device is not connected".into())
    }
}

/// Called from the network session (not exposed to the webview).
pub async fn start_virtual_mic(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    ip: String,
    port: u16,
    sample_rate: u32,
    channels: u16,
    use_adb: bool,
    token: String,
) -> Result<(), String> {
    let mut running_guard = state.virtual_mic_running.lock().unwrap();
    if running_guard.is_some() {
        return Ok(());
    }

    if use_adb {
        let adb_cmd = find_adb_path();
        let _ = create_command(&adb_cmd)
            .args(&["forward", "tcp:40002", &format!("tcp:{}", port)])
            .output();
    }

    let mic = crate::system::virtual_mic::VirtualMicrophone::create(
        app,
        ip,
        if use_adb { 40002 } else { port },
        sample_rate,
        channels,
        use_adb,
        token,
    )?;

    *running_guard = Some(mic);
    Ok(())
}

#[tauri::command]
pub async fn stop_virtual_mic(
    state: State<'_, AppState>,
) -> Result<(), String> {
    let mut running_guard = state.virtual_mic_running.lock().unwrap();
    *running_guard = None;
    let adb_cmd = find_adb_path();
    let _ = create_command(&adb_cmd)
        .args(&["forward", "--remove", "tcp:40002"])
        .output();
    crate::system::virtual_mic::cleanup_pulse_modules();
    Ok(())
}

#[tauri::command]
pub async fn set_virtual_mic_muted(
    state: State<'_, AppState>,
    muted: bool,
) -> Result<(), String> {
    let running_guard = state.virtual_mic_running.lock().unwrap();
    if let Some(ref mic) = *running_guard {
        mic.set_muted(muted);
    }
    Ok(())
}

#[tauri::command]
pub async fn set_virtual_mic_volume(
    state: State<'_, AppState>,
    volume: f64,
) -> Result<(), String> {
    let running_guard = state.virtual_mic_running.lock().unwrap();
    if let Some(ref mic) = *running_guard {
        mic.set_volume(volume);
    }
    Ok(())
}
