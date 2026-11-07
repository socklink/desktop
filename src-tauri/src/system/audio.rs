use std::io::Write;
use std::io::Read;
use std::net::{IpAddr, TcpListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub struct AudioStreamServer {
    running: Arc<AtomicBool>,
    pub port: u16,
    /// Secret that must be the request path (`GET /<token>`).
    pub token: String,
    /// Only this address may connect: the phone that asked for the stream.
    pub allowed_ip: IpAddr,
}

/// Read the HTTP request head and require `GET /<token>`.
fn authorize(socket: &mut std::net::TcpStream, token: &str) -> bool {
    socket.set_read_timeout(Some(Duration::from_secs(3))).ok();
    let mut buf = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    while buf.len() < 2048 && !buf.ends_with(b"\r\n\r\n") {
        match socket.read(&mut byte) {
            Ok(1) => buf.push(byte[0]),
            _ => break,
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let path = head
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("GET /"))
        .and_then(|l| l.split(' ').next())
        .unwrap_or("");
    let ok = path.len() == token.len()
        && path
            .bytes()
            .zip(token.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0;
    if ok {
        socket.set_read_timeout(None).ok();
    } else {
        let _ = socket.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    }
    ok
}

fn wav_header(sample_rate: u32, channels: u16) -> [u8; 44] {
    let bits_per_sample: u16 = 16;
    let byte_rate = sample_rate * (channels as u32) * (bits_per_sample as u32 / 8);
    let block_align = channels * (bits_per_sample / 8);
    let data_size: u32 = 0xFFFF_FFFF;

    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36_u32.wrapping_add(data_size)).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes());
    h[22..24].copy_from_slice(&channels.to_le_bytes());
    h[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    h[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    h[32..34].copy_from_slice(&block_align.to_le_bytes());
    h[34..36].copy_from_slice(&bits_per_sample.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_size.to_le_bytes());
    h
}

#[cfg(target_os = "windows")]
fn convert_f32_to_stereo_i16(data: &[f32], channels: usize, pcm: &mut Vec<u8>) {
    if channels == 2 {
        pcm.reserve(data.len() * 2);
        for &sample in data {
            let s = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
            pcm.extend_from_slice(&s.to_le_bytes());
        }
    } else if channels == 1 {
        pcm.reserve(data.len() * 4);
        for &sample in data {
            let s = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
            let bytes = s.to_le_bytes();
            pcm.extend_from_slice(&bytes);
            pcm.extend_from_slice(&bytes);
        }
    } else if channels > 2 {
        let frames = data.len() / channels;
        pcm.reserve(frames * 4);
        for chunk in data.chunks_exact(channels) {
            let left = (chunk[0].clamp(-1.0, 1.0) * 32767.0) as i16;
            let right = (chunk[1].clamp(-1.0, 1.0) * 32767.0) as i16;
            pcm.extend_from_slice(&left.to_le_bytes());
            pcm.extend_from_slice(&right.to_le_bytes());
        }
    }
}

#[cfg(target_os = "windows")]
fn convert_i16_to_stereo_i16(data: &[i16], channels: usize, pcm: &mut Vec<u8>) {
    if channels == 2 {
        pcm.reserve(data.len() * 2);
        for &sample in data {
            pcm.extend_from_slice(&sample.to_le_bytes());
        }
    } else if channels == 1 {
        pcm.reserve(data.len() * 4);
        for &sample in data {
            let bytes = sample.to_le_bytes();
            pcm.extend_from_slice(&bytes);
            pcm.extend_from_slice(&bytes);
        }
    } else if channels > 2 {
        let frames = data.len() / channels;
        pcm.reserve(frames * 4);
        for chunk in data.chunks_exact(channels) {
            pcm.extend_from_slice(&chunk[0].to_le_bytes());
            pcm.extend_from_slice(&chunk[1].to_le_bytes());
        }
    }
}

#[cfg(target_os = "windows")]
fn convert_u16_to_stereo_i16(data: &[u16], channels: usize, pcm: &mut Vec<u8>) {
    if channels == 2 {
        pcm.reserve(data.len() * 2);
        for &sample in data {
            let s = (sample as i32 - 32768) as i16;
            pcm.extend_from_slice(&s.to_le_bytes());
        }
    } else if channels == 1 {
        pcm.reserve(data.len() * 4);
        for &sample in data {
            let s = (sample as i32 - 32768) as i16;
            let bytes = s.to_le_bytes();
            pcm.extend_from_slice(&bytes);
            pcm.extend_from_slice(&bytes);
        }
    } else if channels > 2 {
        let frames = data.len() / channels;
        pcm.reserve(frames * 4);
        for chunk in data.chunks_exact(channels) {
            let left = (chunk[0] as i32 - 32768) as i16;
            let right = (chunk[1] as i32 - 32768) as i16;
            pcm.extend_from_slice(&left.to_le_bytes());
            pcm.extend_from_slice(&right.to_le_bytes());
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn stream_audio_linux(mut socket: std::net::TcpStream, running: Arc<AtomicBool>) {
    use std::process::{Command, Stdio};
    use std::io::Read;

    let monitor = "@DEFAULT_SINK@.monitor";
    let parec = Command::new("parec")
        .arg("--rate=44100")
        .arg("--channels=2")
        .arg("--format=s16le")
        .arg("--latency-msec=5")
        .arg("--process-time-msec=5")
        .arg("-d")
        .arg(monitor)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();

    let mut parec_child = match parec {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[audio] Failed to start parec: {e}");
            let _ = socket.write_all(b"HTTP/1.1 500 Internal Server Error\r\n\r\n");
            return;
        }
    };

    let mut parec_out = match parec_child.stdout.take() {
        Some(out) => out,
        None => {
            let _ = parec_child.kill();
            let _ = socket.write_all(b"HTTP/1.1 500 Internal Server Error\r\n\r\n");
            return;
        }
    };

    let headers = "HTTP/1.1 200 OK\r\n\
                   Content-Type: audio/wav\r\n\
                   Connection: close\r\n\
                   Cache-Control: no-cache\r\n\r\n";
    if socket.write_all(headers.as_bytes()).is_err() {
        let _ = parec_child.kill();
        return;
    }

    if socket.write_all(&wav_header(44100, 2)).is_err() {
        let _ = parec_child.kill();
        return;
    }

    let mut buf = [0u8; 512];
    while running.load(Ordering::Relaxed) {
        match parec_out.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if socket.write_all(&buf[..n]).is_err() {
                    break;
                }
                let _ = socket.flush();
            }
            Err(_) => break,
        }
    }
    let _ = parec_child.kill();
    let _ = parec_child.wait();
}

#[cfg(target_os = "windows")]
fn stream_audio_windows(mut socket: std::net::TcpStream, running: Arc<AtomicBool>) {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let host = cpal::default_host();
    let device = match host.default_output_device() {
        Some(d) => d,
        None => {
            eprintln!("[audio] No default output device found");
            let _ = socket.write_all(b"HTTP/1.1 500 Internal Server Error\r\n\r\n");
            return;
        }
    };

    let default_config = match device.default_output_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[audio] Failed to get default output config: {e}");
            let _ = socket.write_all(b"HTTP/1.1 500 Internal Server Error\r\n\r\n");
            return;
        }
    };

    let in_sample_rate = default_config.sample_rate().0;
    let in_channels = default_config.channels() as usize;
    let sample_format = default_config.sample_format();
    let stream_config: cpal::StreamConfig = default_config.into();

    let out_channels: u16 = 2; // Standard stereo for mobile client
    let out_sample_rate = in_sample_rate;

    println!("[audio] Setting up loopback: rate={in_sample_rate}, channels={in_channels}, format={sample_format:?}");

    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(100);
    let err_fn = |err| eprintln!("[audio] Loopback stream error: {err}");

    let stream = match sample_format {
        cpal::SampleFormat::F32 => {
            let tx = tx.clone();
            device.build_input_stream(
                &stream_config,
                move |data: &[f32], _: &_| {
                    let mut pcm = Vec::new();
                    convert_f32_to_stereo_i16(data, in_channels, &mut pcm);
                    let _ = tx.try_send(pcm);
                },
                err_fn,
                None,
            )
        }
        cpal::SampleFormat::I16 => {
            let tx = tx.clone();
            device.build_input_stream(
                &stream_config,
                move |data: &[i16], _: &_| {
                    let mut pcm = Vec::new();
                    convert_i16_to_stereo_i16(data, in_channels, &mut pcm);
                    let _ = tx.try_send(pcm);
                },
                err_fn,
                None,
            )
        }
        cpal::SampleFormat::U16 => {
            let tx = tx.clone();
            device.build_input_stream(
                &stream_config,
                move |data: &[u16], _: &_| {
                    let mut pcm = Vec::new();
                    convert_u16_to_stereo_i16(data, in_channels, &mut pcm);
                    let _ = tx.try_send(pcm);
                },
                err_fn,
                None,
            )
        }
        _ => {
            eprintln!("[audio] Unsupported sample format: {sample_format:?}");
            let _ = socket.write_all(b"HTTP/1.1 500 Internal Server Error\r\n\r\n");
            return;
        }
    };

    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[audio] Failed to build loopback stream: {e}");
            let _ = socket.write_all(b"HTTP/1.1 500 Internal Server Error\r\n\r\n");
            return;
        }
    };

    if let Err(e) = stream.play() {
        eprintln!("[audio] Failed to play stream: {e}");
        let _ = socket.write_all(b"HTTP/1.1 500 Internal Server Error\r\n\r\n");
        return;
    }

    let headers = "HTTP/1.1 200 OK\r\n\
                   Content-Type: audio/wav\r\n\
                   Connection: close\r\n\
                   Cache-Control: no-cache\r\n\r\n";
    if socket.write_all(headers.as_bytes()).is_err() {
        return;
    }

    if socket.write_all(&wav_header(out_sample_rate, out_channels)).is_err() {
        return;
    }

    println!("[audio] Loopback stream active on Windows (rate: {out_sample_rate}Hz)");

    while running.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(bytes) => {
                if socket.write_all(&bytes).is_err() {
                    break;
                }
                let _ = socket.flush();
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

impl AudioStreamServer {
    pub fn start(allowed_ip: IpAddr) -> Result<Self, String> {
        #[cfg(target_os = "windows")]
        {
            use cpal::traits::{DeviceTrait, HostTrait};
            let host = cpal::default_host();
            let device = host
                .default_output_device()
                .ok_or_else(|| "No default audio output device found on Windows".to_string())?;
            let _ = device
                .default_output_config()
                .map_err(|e| format!("Default audio output config unsupported: {e}"))?;
        }

        let listener = TcpListener::bind("0.0.0.0:0").map_err(|e| format!("Bind failed: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        listener.set_nonblocking(true).map_err(|e| format!("Set nonblocking failed: {e}"))?;
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();
        let token: String = {
            use rand::RngExt as _;
            rand::rng()
                .sample_iter(&rand::distr::Alphanumeric)
                .take(43)
                .map(char::from)
                .collect()
        };
        let token_for_thread = token.clone();

        thread::spawn(move || {
            loop {
                if !running_clone.load(Ordering::Relaxed) {
                    break;
                }
                match listener.accept() {
                    Ok((mut socket, addr)) => {
                        if addr.ip() != allowed_ip {
                            eprintln!("[audio] Rejected connection from {addr}");
                            continue;
                        }
                        socket.set_nonblocking(false).ok();
                        socket.set_nodelay(true).ok();

                        let r_clone = running_clone.clone();
                        let token = token_for_thread.clone();
                        thread::spawn(move || {
                            if !authorize(&mut socket, &token) {
                                eprintln!("[audio] Rejected unauthenticated request from {addr}");
                                return;
                            }
                            println!("[audio] Client connected from {addr}");
                            #[cfg(not(target_os = "windows"))]
                            stream_audio_linux(socket, r_clone);

                            #[cfg(target_os = "windows")]
                            stream_audio_windows(socket, r_clone);

                            println!("[audio] Stream ended");
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(100));
                    }
                    Err(e) => {
                        eprintln!("[audio] Accept failed: {e}");
                        thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        });

        println!("[audio] AudioStreamServer started on port {port}");
        Ok(Self { running, port, token, allowed_ip })
    }
}

impl Drop for AudioStreamServer {
    fn drop(&mut self) {
        println!("[audio] AudioStreamServer stopping");
        self.running.store(false, Ordering::Relaxed);
    }
}
