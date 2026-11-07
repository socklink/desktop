#[cfg(not(target_os = "windows"))]
use std::io::Write;
#[cfg(not(target_os = "windows"))]
use std::process::{Command, Stdio};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use tauri::{AppHandle, Emitter};

pub struct VirtualMicrophone {
    running: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    volume: Arc<Mutex<f64>>,
    null_sink_id: Option<String>,
    remap_source_id: Option<String>,
    pacat_child: Arc<Mutex<Option<Child>>>,
}

#[cfg(not(target_os = "windows"))]
fn run_command(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("Failed to execute {program}: {e}"))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

#[cfg(not(target_os = "windows"))]
fn play_mic_audio_linux(
    app: AppHandle,
    connect_addr: String,
    token: String,
    sample_rate: u32,
    channels: u16,
    running: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    volume: Arc<Mutex<f64>>,
    pacat_child: Arc<Mutex<Option<Child>>>,
) {
    let mut child = match Command::new("pacat")
        .arg("--playback")
        .arg("--device=SyncMicSink")
        .arg(format!("--rate={sample_rate}"))
        .arg(format!("--channels={channels}"))
        .arg("--format=s16le")
        .arg("--latency-msec=20")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[virtual_mic] Failed to start pacat: {e}");
            app.emit(
                "virtual-mic-state-changed",
                serde_json::json!({ "active": false, "error": format!("Failed to start pacat: {e}") }),
            )
            .ok();
            return;
        }
    };

    let mut stdin = match child.stdin.take() {
        Some(s) => s,
        None => {
            app.emit(
                "virtual-mic-state-changed",
                serde_json::json!({ "active": false, "error": "Failed to capture pacat stdin" }),
            )
            .ok();
            return;
        }
    };

    if let Ok(mut g) = pacat_child.lock() {
        *g = Some(child);
    }

    use std::io::Read;
    use std::net::TcpStream;
    use std::time::Duration;

    let stream_res = TcpStream::connect(&connect_addr);
    let mut stream = match stream_res {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[virtual_mic] Failed to connect to audio server at {connect_addr}: {e}");
            app.emit(
                "virtual-mic-state-changed",
                serde_json::json!({ "active": false, "error": format!("Connection failed: {e}") }),
            )
            .ok();
            return;
        }
    };

    stream.set_read_timeout(Some(Duration::from_millis(500))).ok();
    stream.set_nodelay(true).ok();
    // First line authenticates us to the phone's mic server.
    if std::io::Write::write_all(&mut stream, format!("{token}\n").as_bytes()).is_err() {
        eprintln!("[virtual_mic] Failed to send stream token");
        return;
    }

    app.emit(
        "virtual-mic-state-changed",
        serde_json::json!({ "active": true, "error": null }),
    )
    .ok();

    let mut buf = [0u8; 1024];
    let mut sample_count = 0u32;
    let mut sum_sq = 0.0f64;

    while running.load(Ordering::Relaxed) {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let is_muted = muted.load(Ordering::Relaxed);
                let vol = *volume.lock().unwrap();

                let mut processed_buf = buf[..n].to_vec();

                for chunk in processed_buf.chunks_exact_mut(2) {
                    let raw_sample = i16::from_le_bytes([chunk[0], chunk[1]]);
                    let sample_val = if is_muted {
                        0f64
                    } else {
                        (raw_sample as f64) * vol
                    };

                    let clamped = sample_val.clamp(-32768.0, 32767.0) as i16;
                    let bytes = clamped.to_le_bytes();
                    chunk[0] = bytes[0];
                    chunk[1] = bytes[1];

                    sum_sq += (clamped as f64) * (clamped as f64);
                    sample_count += 1;
                }

                if stdin.write_all(&processed_buf).is_err() {
                    break;
                }
                let _ = stdin.flush();

                if sample_count >= (sample_rate / 20) {
                    let rms = (sum_sq / (sample_count as f64)).sqrt();
                    let level = (rms / 32768.0).clamp(0.0, 1.0);
                    app.emit("mic-audio-level", level).ok();

                    sample_count = 0;
                    sum_sq = 0.0;
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => {
                eprintln!("[virtual_mic] Read error: {e}");
                break;
            }
        }
    }

    app.emit(
        "virtual-mic-state-changed",
        serde_json::json!({ "active": false, "error": null }),
    )
    .ok();
    app.emit("mic-audio-level", 0.0).ok();
}

#[cfg(target_os = "windows")]
fn play_mic_audio_windows(
    app: AppHandle,
    connect_addr: String,
    token: String,
    sample_rate: u32,
    _channels: u16,
    running: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    volume: Arc<Mutex<f64>>,
) {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::io::Read;
    use std::net::TcpStream;
    use std::time::Duration;

    let host = cpal::default_host();
    let device = host
        .output_devices()
        .ok()
        .and_then(|mut dev_iter| {
            dev_iter.find(|d| {
                d.name()
                    .map(|n| n.contains("CABLE Input") || n.contains("VB-Audio") || n.contains("Virtual"))
                    .unwrap_or(false)
            })
        })
        .or_else(|| host.default_output_device());

    let device = match device {
        Some(d) => d,
        None => {
            eprintln!("[virtual_mic] No output audio device available");
            app.emit(
                "virtual-mic-state-changed",
                serde_json::json!({ "active": false, "error": "No output audio device available" }),
            )
            .ok();
            return;
        }
    };

    let supported_config = match device.default_output_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[virtual_mic] Failed to get default output config: {e}");
            app.emit(
                "virtual-mic-state-changed",
                serde_json::json!({ "active": false, "error": format!("Failed to get device config: {e}") }),
            )
            .ok();
            return;
        }
    };

    let out_channels = supported_config.channels();
    let out_sample_rate = supported_config.sample_rate().0;
    let config: cpal::StreamConfig = supported_config.into();

    let (sample_tx, sample_rx) = std::sync::mpsc::sync_channel::<f32>((out_sample_rate as usize * out_channels as usize).max(8192));

    let stream = device.build_output_stream(
        &config,
        move |data: &mut [f32], _: &_| {
            for sample in data.iter_mut() {
                *sample = sample_rx.try_recv().unwrap_or(0.0);
            }
        },
        |err| eprintln!("[virtual_mic] Output stream error: {err}"),
        None,
    );

    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[virtual_mic] Failed to build audio output stream: {e}");
            app.emit(
                "virtual-mic-state-changed",
                serde_json::json!({ "active": false, "error": format!("Failed to create audio stream: {e}") }),
            )
            .ok();
            return;
        }
    };

    if let Err(e) = stream.play() {
        eprintln!("[virtual_mic] Failed to play stream: {e}");
        return;
    }

    let stream_res = TcpStream::connect(&connect_addr);
    let mut tcp_stream = match stream_res {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[virtual_mic] Failed to connect to audio server at {connect_addr}: {e}");
            app.emit(
                "virtual-mic-state-changed",
                serde_json::json!({ "active": false, "error": format!("Connection failed: {e}") }),
            )
            .ok();
            return;
        }
    };

    tcp_stream.set_read_timeout(Some(Duration::from_millis(500))).ok();
    tcp_stream.set_nodelay(true).ok();
    if std::io::Write::write_all(&mut tcp_stream, format!("{token}\n").as_bytes()).is_err() {
        eprintln!("[virtual_mic] Failed to send stream token");
        return;
    }

    app.emit(
        "virtual-mic-state-changed",
        serde_json::json!({ "active": true, "error": null }),
    )
    .ok();

    let mut buf = [0u8; 1024];
    let mut sample_count = 0u32;
    let mut sum_sq = 0.0f64;

    let mut last_sample = 0.0f32;
    let mut phase = 0.0f64;
    let step = sample_rate as f64 / out_sample_rate as f64;

    while running.load(Ordering::Relaxed) {
        match tcp_stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let is_muted = muted.load(Ordering::Relaxed);
                let vol = *volume.lock().unwrap();

                for chunk in buf[..n].chunks_exact(2) {
                    let raw_sample = i16::from_le_bytes([chunk[0], chunk[1]]);
                    let sample_val = if is_muted {
                        0f64
                    } else {
                        (raw_sample as f64) * vol
                    };

                    let clamped = sample_val.clamp(-32768.0, 32767.0);
                    let curr_normalized = (clamped / 32768.0) as f32;

                    // Dynamically resample and map channels to output device
                    while phase < 1.0 {
                        let interpolated = last_sample + (curr_normalized - last_sample) * (phase as f32);
                        for _ in 0..out_channels {
                            let _ = sample_tx.try_send(interpolated);
                        }
                        phase += step;
                    }
                    phase -= 1.0;
                    last_sample = curr_normalized;

                    sum_sq += clamped * clamped;
                    sample_count += 1;
                }

                if sample_count >= (sample_rate / 20) {
                    let rms = (sum_sq / (sample_count as f64)).sqrt();
                    let level = (rms / 32768.0).clamp(0.0, 1.0);
                    app.emit("mic-audio-level", level).ok();

                    sample_count = 0;
                    sum_sq = 0.0;
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => {
                eprintln!("[virtual_mic] Read error: {e}");
                break;
            }
        }
    }

    app.emit(
        "virtual-mic-state-changed",
        serde_json::json!({ "active": false, "error": null }),
    )
    .ok();
    app.emit("mic-audio-level", 0.0).ok();
}

impl VirtualMicrophone {
    pub fn create(
        app: AppHandle,
        ip: String,
        port: u16,
        sample_rate: u32,
        channels: u16,
        use_adb: bool,
        token: String,
    ) -> Result<Self, String> {
        #[cfg(not(target_os = "windows"))]
        let (null_sink_id, remap_source_id) = Self::setup_pulse_modules()?;

        #[cfg(target_os = "windows")]
        let (null_sink_id, remap_source_id) = (None, None);

        let connect_addr = if use_adb {
            format!("127.0.0.1:{port}")
        } else {
            format!("{ip}:{port}")
        };

        let pacat_child = Arc::new(Mutex::new(None));
        let running = Arc::new(AtomicBool::new(true));
        let muted = Arc::new(AtomicBool::new(false));
        let volume = Arc::new(Mutex::new(1.0f64));

        let r_clone = running.clone();
        let m_clone = muted.clone();
        let v_clone = volume.clone();

        #[cfg(not(target_os = "windows"))]
        {
            let pc_clone = pacat_child.clone();
            thread::spawn(move || {
                play_mic_audio_linux(
                    app,
                    connect_addr,
                    token,
                    sample_rate,
                    channels,
                    r_clone,
                    m_clone,
                    v_clone,
                    pc_clone,
                );
            });
        }

        #[cfg(target_os = "windows")]
        {
            thread::spawn(move || {
                play_mic_audio_windows(
                    app,
                    connect_addr,
                    token,
                    sample_rate,
                    channels,
                    r_clone,
                    m_clone,
                    v_clone,
                );
            });
        }

        Ok(Self {
            running,
            muted,
            volume,
            null_sink_id,
            remap_source_id,
            pacat_child,
        })
    }

    #[cfg(not(target_os = "windows"))]
    fn setup_pulse_modules() -> Result<(Option<String>, Option<String>), String> {
        cleanup_pulse_modules();
        let sink_out = run_command(
            "pactl",
            &[
                "load-module",
                "module-null-sink",
                "sink_name=SyncMicSink",
                "sink_properties=device.description=\"Sync_Microphone_Sink\"",
            ],
        );

        let sink_id = match sink_out {
            Ok(id) => Some(id),
            Err(e) => {
                eprintln!("[virtual_mic] Notice: null-sink load result: {e}");
                None
            }
        };

        let source_out = run_command(
            "pactl",
            &[
                "load-module",
                "module-remap-source",
                "source_name=SyncMic",
                "master=SyncMicSink.monitor",
                "source_properties=device.description=\"Sync Microphone\"",
            ],
        );

        let source_id = match source_out {
            Ok(id) => Some(id),
            Err(e) => {
                eprintln!("[virtual_mic] Notice: remap-source load result: {e}");
                None
            }
        };

        Ok((sink_id, source_id))
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub fn set_volume(&self, volume: f64) {
        let mut guard = self.volume.lock().unwrap();
        *guard = volume.clamp(0.0, 2.0);
    }
}

pub fn cleanup_pulse_modules() {
    #[cfg(not(target_os = "windows"))]
    {
        let output = match run_command("pactl", &["list", "modules", "short"]) {
            Ok(out) => out,
            Err(_) => return,
        };

        let mut sources_to_unload = Vec::new();
        let mut sinks_to_unload = Vec::new();

        for line in output.lines() {
            if line.contains("SyncMic") || line.contains("SyncMicSink") || line.contains("Sync_Microphone_Sink") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if !parts.is_empty() {
                    let id = parts[0];
                    if line.contains("module-remap-source") || (parts.len() > 1 && parts[1].contains("remap-source")) {
                        sources_to_unload.push(id.to_string());
                    } else if line.contains("module-null-sink") || (parts.len() > 1 && parts[1].contains("null-sink")) {
                        sinks_to_unload.push(id.to_string());
                    }
                }
            }
        }

        for id in sources_to_unload {
            let _ = run_command("pactl", &["unload-module", &id]);
        }
        for id in sinks_to_unload {
            let _ = run_command("pactl", &["unload-module", &id]);
        }
    }
}

impl Drop for VirtualMicrophone {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);

        if let Ok(mut child_guard) = self.pacat_child.lock() {
            if let Some(mut child) = child_guard.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }

        #[cfg(not(target_os = "windows"))]
        {
            if let Some(ref source_id) = self.remap_source_id {
                let _ = run_command("pactl", &["unload-module", source_id]);
            }

            if let Some(ref sink_id) = self.null_sink_id {
                let _ = run_command("pactl", &["unload-module", sink_id]);
            }

            cleanup_pulse_modules();
        }
    }
}
