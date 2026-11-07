//! Line-delimited JSON handshake protocol.

use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex, OnceLock};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    Hello {
        device_id: String,
        name: String,
        token: Option<String>,
    },
    /// Step 1 of pairing: commitment (hex SHA-256) to the phone's nonce.
    PairRequest {
        device_id: String,
        name: String,
        commit: String,
    },
    /// Step 3 of pairing: the phone reveals the nonce it committed to.
    PairReveal {
        nonce: String,
    },
    ClipboardUpdate {
        text: String,
    },
    ClipboardImage {
        base64_data: String,
    },
    MediaCommand {
        command: String,
        value: Option<f64>,
    },
    /// Begin a chunked file upload from the phone.
    FileStart {
        transfer_id: String,
        filename: String,
        total_bytes: u64,
    },
    FileChunk {
        transfer_id: String,
        base64_data: String,
    },
    FileEnd {
        transfer_id: String,
        sha256: String,
    },
    FileAbort {
        transfer_id: String,
    },
    CameraStreamStarted {
        port: u16,
        #[serde(default)]
        use_adb: bool,
        /// Secret the desktop must present when it connects to the phone's stream.
        #[serde(default)]
        token: String,
    },

    CameraStreamStopped,
    MicStreamStarted {
        port: u16,
        #[serde(default = "default_sample_rate")]
        sample_rate: u32,
        #[serde(default = "default_channels")]
        channels: u16,
        #[serde(default)]
        use_adb: bool,
        #[serde(default)]
        token: String,
    },
    MicStreamStopped,
    AudioStreamRequest {
        start: bool,
    },
    CameraConfigState {
        is_front: bool,
        resolution: String,
        fps: i32,
        rotation: i32,
        #[serde(default)]
        use_adb: bool,
    },
    Unpair,
}

fn default_sample_rate() -> u32 {
    44100
}

fn default_channels() -> u16 {
    1
}

#[derive(Debug, Serialize, Clone)]
#[serde(tag = "type")]
pub enum ServerMessage {
    HelloOk {
        device_id: String,
        name: String,
    },
    PairRequired,
    /// Step 2 of pairing: the desktop's nonce.
    PairChallenge {
        nonce: String,
    },
    PairAccepted {
        device_id: String,
        name: String,
        token: String,
    },
    PairRejected {
        reason: String,
    },
    ClipboardUpdate {
        text: String,
    },
    /// Begin a chunked file download to the phone.
    FileStart {
        transfer_id: String,
        filename: String,
        total_bytes: u64,
    },
    FileChunk {
        transfer_id: String,
        base64_data: String,
    },
    FileEnd {
        transfer_id: String,
        sha256: String,
    },
    FileAbort {
        transfer_id: String,
    },
    MediaState(crate::system::media::MediaState),
    SystemVolumeUpdate {
        volume: f64,
        muted: bool,
    },
    TerminalServerInfo {
        enabled: bool,
        port: u16,
        username: String,
        password: Option<String>,
    },
    StartCameraStream,
    StopCameraStream,
    UpdateCameraConfig {
        #[serde(skip_serializing_if = "Option::is_none")]
        is_front: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        resolution: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        fps: Option<i32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        rotation: Option<i32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        use_adb: Option<bool>,
    },
    RequestCameraConfig,
    StartMicStream,
    StopMicStream,
    AudioStreamInfo {
        enabled: bool,
        port: u16,
        /// Secret that must be the request path of the audio stream URL.
        #[serde(skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
    Unpair,
}

/// Largest handshake line accepted before a peer is authenticated.
pub const MAX_HANDSHAKE_LINE: u64 = 8 * 1024;

/// Read one bounded handshake line. Used before authentication, so the size is capped.
pub fn read_line_json<T: DeserializeOwned>(reader: &mut impl BufRead) -> Result<Option<T>, String> {
    let mut line = String::new();
    let read = reader
        .by_ref()
        .take(MAX_HANDSHAKE_LINE)
        .read_line(&mut line)
        .map_err(|e| format!("Could not read protocol line: {e}"))?;
    if read == 0 {
        return Ok(None);
    }
    if !line.ends_with('\n') {
        return Err("Protocol line too long or unterminated".to_string());
    }
    serde_json::from_str(line.trim())
        .map(Some)
        .map_err(|e| format!("Could not parse protocol message: {e}"))
}

/// Per-connection write locks. A connection's socket is cloned into several
/// writers (session, commands, file sender); each line must reach the wire whole,
/// so every write takes the lock of its connection.
fn connection_lock(stream: &TcpStream) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<SocketAddr, Arc<Mutex<()>>>>> = OnceLock::new();
    let mut locks = LOCKS.get_or_init(Default::default).lock().unwrap();
    // Bound the table: closed connections leave an unreferenced entry behind.
    if locks.len() > 64 {
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
    }
    match stream.peer_addr() {
        Ok(addr) => locks.entry(addr).or_default().clone(),
        Err(_) => Arc::new(Mutex::new(())),
    }
}

pub fn write_line_json<T: Serialize>(writer: &mut TcpStream, message: &T) -> Result<(), String> {
    let mut raw = serde_json::to_vec(message)
        .map_err(|e| format!("Could not serialize protocol message: {e}"))?;
    raw.push(b'\n');
    let lock = connection_lock(writer);
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    writer
        .write_all(&raw)
        .and_then(|_| writer.flush())
        .map_err(|e| format!("Could not write protocol message: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    #[test]
    fn concurrent_writers_never_interleave_lines() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();

        let big = "A".repeat(400 * 1024);
        let writers: Vec<_> = (0..2)
            .map(|_| {
                let mut stream = client.try_clone().unwrap();
                let big = big.clone();
                std::thread::spawn(move || {
                    for _ in 0..40 {
                        let msg = ServerMessage::FileChunk {
                            transfer_id: "t".into(),
                            base64_data: big.clone(),
                        };
                        write_line_json(&mut stream, &msg).unwrap();
                    }
                })
            })
            .collect();

        let mut reader = BufReader::new(server);
        for _ in 0..80 {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(v["base64_data"].as_str().unwrap().len(), big.len());
        }
        for w in writers {
            w.join().unwrap();
        }
    }
}
