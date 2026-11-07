//! JSON-backed store for paired devices.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedPeer {
    pub device_id: String,
    pub name: String,
    /// Legacy plaintext token from older versions. Migrated to `token_hash` on load
    /// and never written back.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
    /// Hex SHA-256 of the pairing token. The server only needs to verify tokens, so
    /// the token itself is not stored.
    #[serde(default)]
    pub token_hash: String,
    pub last_seen: i64,
    #[serde(default = "default_clipboard_sync")]
    pub clipboard_sync_enabled: bool,
    #[serde(default = "default_media_controls")]
    pub media_controls_enabled: bool,
    #[serde(default = "default_volume_sync")]
    pub volume_sync_enabled: bool,
    #[serde(default = "default_incoming_files")]
    pub incoming_files_enabled: bool,
    #[serde(default = "default_terminal_access")]
    pub terminal_access_enabled: bool,
    #[serde(default)]
    pub audio_streaming_enabled: bool,
}

pub fn hash_token(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl TrustedPeer {
    /// A freshly paired peer with the default permissions.
    pub fn new(device_id: String, name: String, token: &str, last_seen: i64) -> Self {
        Self {
            device_id,
            name,
            token: String::new(),
            token_hash: hash_token(token),
            last_seen,
            clipboard_sync_enabled: default_clipboard_sync(),
            media_controls_enabled: default_media_controls(),
            volume_sync_enabled: default_volume_sync(),
            incoming_files_enabled: default_incoming_files(),
            terminal_access_enabled: default_terminal_access(),
            audio_streaming_enabled: false,
        }
    }

    /// Constant-time check of a presented token against the stored hash.
    pub fn token_matches(&self, token: &str) -> bool {
        if self.token_hash.is_empty() {
            return false;
        }
        let presented = hash_token(token);
        let a = presented.as_bytes();
        let b = self.token_hash.as_bytes();
        a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }
}

fn default_clipboard_sync() -> bool {
    true
}

fn default_media_controls() -> bool {
    true
}

fn default_volume_sync() -> bool {
    true
}

fn default_incoming_files() -> bool {
    true
}

fn default_terminal_access() -> bool {
    false
}


#[derive(Debug)]
pub struct TrustedPeers {
    path: PathBuf,
    peers: HashMap<String, TrustedPeer>,
}

impl TrustedPeers {
    pub fn load(app: &AppHandle) -> Result<Self, String> {
        let dir = app
            .path()
            .app_data_dir()
            .map_err(|e| format!("Could not resolve app data dir: {e}"))?;
        fs::create_dir_all(&dir).map_err(|e| format!("Could not create app data dir: {e}"))?;
        let path = dir.join("trusted_peers.json");
        let peers = match fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str::<Vec<TrustedPeer>>(&raw)
                .unwrap_or_default()
                .into_iter()
                .map(|peer| (peer.device_id.clone(), peer))
                .collect(),
            Err(_) => HashMap::new(),
        };
        let mut store = Self { path, peers };
        // Migrate plaintext tokens written by older versions.
        let mut migrated = false;
        for peer in store.peers.values_mut() {
            if !peer.token.is_empty() {
                if peer.token_hash.is_empty() {
                    peer.token_hash = hash_token(&peer.token);
                }
                peer.token.clear();
                migrated = true;
            }
        }
        if migrated {
            let _ = store.save();
        }
        Ok(store)
    }

    pub fn get(&self, device_id: &str) -> Option<&TrustedPeer> {
        self.peers.get(device_id)
    }

    pub fn all(&self) -> Vec<TrustedPeer> {
        self.peers.values().cloned().collect()
    }

    pub fn upsert(&mut self, peer: TrustedPeer) -> Result<(), String> {
        self.peers.insert(peer.device_id.clone(), peer);
        self.save()
    }

    pub fn touch(&mut self, device_id: &str) -> Result<(), String> {
        if let Some(peer) = self.peers.get_mut(device_id) {
            peer.last_seen = now_unix();
            self.save()?;
        }
        Ok(())
    }

    pub fn remove(&mut self, device_id: &str) -> Result<(), String> {
        self.peers.remove(device_id);
        self.save()
    }

    fn save(&self) -> Result<(), String> {
        let peers: Vec<_> = self.peers.values().cloned().collect();
        let raw = serde_json::to_string_pretty(&peers)
            .map_err(|e| format!("Could not serialize trusted peers: {e}"))?;
        // Write to a temp file (owner-only on Unix) and rename, so a crash cannot leave
        // a truncated store and other users cannot read it.
        let tmp = self.path.with_extension("json.tmp");
        write_private(&tmp, raw.as_bytes())
            .and_then(|_| fs::rename(&tmp, &self.path))
            .map_err(|e| format!("Could not save trusted peers: {e}"))
    }
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
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
        let mut f = fs::File::create(path)?;
        f.write_all(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_verified_by_hash_and_not_stored() {
        let p = TrustedPeer::new("d".into(), "n".into(), "secret-token", 0);
        assert!(p.token.is_empty());
        assert!(p.token_matches("secret-token"));
        assert!(!p.token_matches("secret-tokeN"));
        assert!(!p.token_matches(""));
        let json = serde_json::to_string(&p).unwrap();
        assert!(!json.contains("secret-token"));
    }

    #[test]
    fn empty_hash_never_matches() {
        let mut p = TrustedPeer::new("d".into(), "n".into(), "x", 0);
        p.token_hash.clear();
        assert!(!p.token_matches("x"));
        assert!(!p.token_matches(""));
    }

    #[test]
    fn new_peers_default_permissions() {
        let p = TrustedPeer::new("d".into(), "n".into(), "x", 0);
        assert!(p.incoming_files_enabled);
        assert!(!p.terminal_access_enabled);
        assert!(!p.audio_streaming_enabled);
    }
}
