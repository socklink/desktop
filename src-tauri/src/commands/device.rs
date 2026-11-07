//! Commands that expose information about the local device to the UI.

use crate::system;
use crate::app::AppState;
use tauri::{State, Emitter};

/// Returns the local IPv4 address used to reach the LAN.
#[tauri::command]
pub async fn get_device_ip() -> String {
    system::get_local_ip().unwrap_or_else(|| "127.0.0.1".to_string())
}

/// Returns this device's hostname.
#[tauri::command]
pub async fn get_device_name() -> String {
    system::get_system_name().unwrap_or_else(|| "Unknown".to_string())
}

#[tauri::command]
pub async fn set_device_incoming_files(
    device_id: String,
    enabled: bool,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let mut peers = state.trusted_peers.lock().unwrap();
    if let Some(peer) = peers.get(&device_id) {
        let mut updated_peer = peer.clone();
        updated_peer.incoming_files_enabled = enabled;
        peers.upsert(updated_peer)?;
    }
    Ok(())
}

#[tauri::command]
pub async fn set_device_terminal_access(
    device_id: String,
    enabled: bool,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    {
        let mut peers = state.trusted_peers.lock().unwrap();
        if let Some(peer) = peers.get(&device_id) {
            let mut updated_peer = peer.clone();
            updated_peer.terminal_access_enabled = enabled;
            peers.upsert(updated_peer)?;
        }
    }
    let _ = app.emit("terminal-access-changed", device_id);
    Ok(())
}

use crate::app::TransferProgress;

#[tauri::command]
pub async fn get_active_transfer(
    state: State<'_, AppState>,
) -> Result<Option<TransferProgress>, String> {
    Ok(state.active_transfer.lock().unwrap().clone())
}

#[tauri::command]
pub async fn set_device_audio_streaming(
    device_id: String,
    enabled: bool,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let peer_name = {
        let mut peers = state.trusted_peers.lock().unwrap();
        let name = peers.get(&device_id).map(|p| p.name.clone()).unwrap_or_default();
        if let Some(peer) = peers.get(&device_id) {
            let mut updated_peer = peer.clone();
            updated_peer.audio_streaming_enabled = enabled;
            let _ = peers.upsert(updated_peer);
        }
        name
    };

    let peer_ip = state.active_ips.lock().unwrap().get(&device_id).copied();
    let mut active_streams = state.active_streams.lock().unwrap();
    if let (Some(stream), Some(peer_ip)) = (active_streams.get_mut(&device_id), peer_ip) {
        use crate::network::protocol::ServerMessage;
        let mut audio_server = state.audio_stream_server.lock().unwrap();
        if enabled {
            match crate::network::session::ensure_audio_server(&mut audio_server, peer_ip) {
                Some((port, token)) => {
                    let msg = ServerMessage::AudioStreamInfo { enabled: true, port, token: Some(token) };
                    let _ = crate::network::protocol::write_line_json(stream, &msg);
                    let _ = notify_rust::Notification::new()
                        .summary("Listen Through Mobile")
                        .body(&format!("Audio is now streaming to {}", peer_name))
                        .show();
                }
                None => {
                    let msg = ServerMessage::AudioStreamInfo { enabled: false, port: 0, token: None };
                    let _ = crate::network::protocol::write_line_json(stream, &msg);
                }
            }
        } else {
            *audio_server = None;
            let msg = ServerMessage::AudioStreamInfo { enabled: false, port: 0, token: None };
            let _ = crate::network::protocol::write_line_json(stream, &msg);
        }
    }
    Ok(())
}

#[derive(Debug, serde::Serialize)]
pub struct MissingDependency {
    pub feature: String,
    pub title: String,
    pub package: String,
    pub command: String,
    pub description: String,
}

#[tauri::command]
pub async fn check_system_deps(feature: String) -> Result<Option<MissingDependency>, String> {
    #[cfg(target_os = "linux")]
    {
        use std::process::Command;
        if feature == "mic" {
            let pactl_ok = Command::new("pactl").arg("--version").output().map(|o| o.status.success()).unwrap_or(false);
            let pacat_ok = Command::new("pacat").arg("--version").output().map(|o| o.status.success()).unwrap_or(false);
            if !pactl_ok || !pacat_ok {
                return Ok(Some(MissingDependency {
                    feature: "mic".to_string(),
                    title: "PulseAudio Utilities Required".to_string(),
                    package: "pulseaudio-utils".to_string(),
                    command: "sudo apt install pulseaudio-utils".to_string(),
                    description: "Required to create the system 'Sync Microphone' virtual input device on Linux.".to_string(),
                }));
            }
        } else if feature == "camera" {
            let v4l2_ok = Command::new("v4l2-ctl").arg("--version").output().map(|o| o.status.success()).unwrap_or(false);
            let mod_ok = std::path::Path::new("/dev/video9").exists() || Command::new("modinfo").arg("v4l2loopback").output().map(|o| o.status.success()).unwrap_or(false);
            if !v4l2_ok || !mod_ok {
                return Ok(Some(MissingDependency {
                    feature: "camera".to_string(),
                    title: "V4L2 Loopback Module Required".to_string(),
                    package: "v4l2loopback-dkms v4l-utils".to_string(),
                    command: "sudo apt install v4l2loopback-dkms v4l-utils && sudo modprobe v4l2loopback exclusive_caps=1 card_label=\"Sync Camera\" video_nr=9".to_string(),
                    description: "Required to create the system '/dev/video9' virtual webcam device on Linux.".to_string(),
                }));
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        use cpal::traits::HostTrait;
        use cpal::traits::DeviceTrait;
        use std::process::Command;

        if feature == "mic" {
            let host = cpal::default_host();
            let has_vb_cable = host
                .output_devices()
                .ok()
                .map(|mut devs| {
                    devs.any(|d| {
                        d.name()
                            .map(|n| n.contains("CABLE Input") || n.contains("VB-Audio") || n.contains("Virtual"))
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false);

            if !has_vb_cable {
                return Ok(Some(MissingDependency {
                    feature: "mic".to_string(),
                    title: "VB-Audio Virtual Cable Required".to_string(),
                    package: "VBAudio.VBCable".to_string(),
                    command: "winget install VBAudio.VBCable".to_string(),
                    description: "Required to route the system 'Sync Microphone' virtual input device audio on Windows.".to_string(),
                }));
            }
        } else if feature == "camera" {
            let mut cmd = Command::new("reg");
            cmd.args(&["query", "HKCR\\CLSID\\{A3FCE0F5-3493-419F-958A-ABA1250EC20B}"]);
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x08000000);
            }
            let is_reg = cmd.output().map(|o| o.status.success()).unwrap_or(false);
            if !is_reg {
                return Ok(Some(MissingDependency {
                    feature: "camera".to_string(),
                    title: "OBS Virtual Camera Module Required".to_string(),
                    package: "obs-virtualcam-module64.dll".to_string(),
                    command: "powershell Start-Process regsvr32 -ArgumentList 'resources\\obs-virtualcam-module64.dll' -Verb RunAs".to_string(),
                    description: "Required to register the 'Sync Camera' DirectShow virtual webcam driver on Windows.".to_string(),
                }));
            }
        } else if feature == "audio" {
            let host = cpal::default_host();
            if host.default_output_device().is_none() {
                return Ok(Some(MissingDependency {
                    feature: "audio".to_string(),
                    title: "Audio Output Device Required".to_string(),
                    package: "Audio Endpoint".to_string(),
                    command: "control mmsys.cpl sounds".to_string(),
                    description: "No active audio playback device found on Windows for audio streaming.".to_string(),
                }));
            }
        }
    }

    let _ = feature;
    Ok(None)
}

#[tauri::command]
pub async fn auto_install_system_dep(app: tauri::AppHandle, feature: String) -> Result<(), String> {
    let _ = &app;
    #[cfg(target_os = "windows")]
    {
        use std::process::Command;
        if feature == "camera" {
            crate::commands::camera::prepare_windows_driver(&app)
        } else if feature == "mic" {
            let ps_script = r#"
                $installed = $false
                try {
                    $res = Start-Process winget -ArgumentList 'install', '--id', 'VB-Audio.Voicemeeter', '--accept-source-agreements', '--accept-package-agreements', '--silent' -Wait -PassThru
                    if ($res.ExitCode -eq 0) { $installed = $true }
                } catch {}

                if (-not $installed) {
                    $zip = "$env:TEMP\VoicemeeterSetup.zip"
                    $dest = "$env:TEMP\VoicemeeterSetup_dir"
                    Invoke-WebRequest -Uri "https://download.vb-audio.com/Download_CABLE/VoicemeeterSetup_v1122.zip" -OutFile $zip
                    # Only run the installer if the download is exactly the pinned artifact.
                    $expected = "4D63BFE2A5EAEA8CCF6EA8ABE27C053C05689CCC11BF365130C6B246973BA4FA"
                    if ((Get-FileHash -Path $zip -Algorithm SHA256).Hash -ne $expected) {
                        Remove-Item $zip -Force -ErrorAction SilentlyContinue
                        throw "Voicemeeter download failed checksum verification."
                    }
                    Expand-Archive -Path $zip -DestinationPath $dest -Force
                    $exe = (Get-ChildItem $dest -Filter "*Setup*.exe" | Select-Object -First 1).FullName
                    if ($exe) {
                        Start-Process $exe -ArgumentList "-i", "-h" -Wait
                    }
                }
            "#;

            let utf16_bytes: Vec<u8> = ps_script
                .encode_utf16()
                .flat_map(|u| u.to_le_bytes())
                .collect();
            use base64::{engine::general_purpose::STANDARD, Engine as _};
            let encoded_cmd = STANDARD.encode(&utf16_bytes);

            let status = Command::new("powershell")
                .args(&[
                    "-NoProfile",
                    "-Command",
                    &format!("Start-Process powershell -ArgumentList '-NoProfile', '-EncodedCommand', '{}' -Verb RunAs -Wait", encoded_cmd),
                ])
                .status()
                .map_err(|e| format!("Failed to launch installer: {e}"))?;

            if !status.success() {
                return Err("Installation was cancelled or encountered an error.".into());
            }
            Ok(())
        } else {
            Ok(())
        }
    }
    #[cfg(target_os = "linux")]
    {
        use std::process::Command;
        if feature == "camera" {
            let status = Command::new("pkexec")
                .args(&["sh", "-c", "apt-get update && apt-get install -y v4l2loopback-dkms v4l-utils && modprobe v4l2loopback exclusive_caps=1 card_label=\"Sync Camera\" video_nr=9"])
                .status()
                .map_err(|e| format!("Failed to run pkexec: {e}"))?;
            if !status.success() {
                return Err("Failed to install/load v4l2loopback module.".into());
            }
            Ok(())
        } else if feature == "mic" {
            let status = Command::new("pkexec")
                .args(&["sh", "-c", "apt-get update && apt-get install -y pulseaudio-utils"])
                .status()
                .map_err(|e| format!("Failed to run pkexec: {e}"))?;
            if !status.success() {
                return Err("Failed to install pulseaudio-utils.".into());
            }
            Ok(())
        } else {
            Ok(())
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = (app, feature);
        Ok(())
    }
}
