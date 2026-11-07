//! Mediates pending pair requests between TCP sessions and the Tauri UI.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::mpsc::Sender;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest, Sha256};

/// Pair requests that may wait for the user at the same time.
const MAX_PENDING: usize = 3;
/// After a rejected or timed-out request, the same IP must wait this long.
const COOLDOWN: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairRequestEvent {
    pub device_id: String,
    pub name: String,
    pub code: String,
    /// Address the request came from, so the user can recognise it.
    pub ip: String,
}

#[derive(Debug, Clone, Copy)]
pub enum PairDecision {
    Accept,
    Reject,
}

struct PendingPairRequest {
    event: PairRequestEvent,
    ip: IpAddr,
    decision: Sender<PairDecision>,
}

#[derive(Default)]
struct Inner {
    pending: HashMap<String, PendingPairRequest>,
    cooldown: HashMap<IpAddr, Instant>,
}

#[derive(Default)]
pub struct PairingManager {
    inner: Mutex<Inner>,
}

impl PairingManager {
    /// Register a pending request, refusing floods: at most [`MAX_PENDING`] at once,
    /// one per IP and per device id, and a cooldown after a rejected request.
    pub fn begin(
        &self,
        ip: IpAddr,
        event: PairRequestEvent,
        decision: Sender<PairDecision>,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("pairing mutex poisoned");
        inner.cooldown.retain(|_, at| at.elapsed() < COOLDOWN);
        if inner.cooldown.contains_key(&ip) {
            return Err("Too many pairing attempts, try again shortly".to_string());
        }
        if inner.pending.len() >= MAX_PENDING {
            return Err("Too many pending pair requests".to_string());
        }
        if inner
            .pending
            .values()
            .any(|p| p.ip == ip || p.event.device_id == event.device_id)
        {
            return Err("A pair request from this device is already pending".to_string());
        }
        inner.pending.insert(
            event.device_id.clone(),
            PendingPairRequest { event, ip, decision },
        );
        Ok(())
    }

    pub fn resolve(&self, device_id: &str, decision: PairDecision) -> Result<(), String> {
        let pending = {
            let mut inner = self.inner.lock().expect("pairing mutex poisoned");
            let pending = inner
                .pending
                .remove(device_id)
                .ok_or_else(|| "No pending pair request for that device".to_string())?;
            if matches!(decision, PairDecision::Reject) {
                inner.cooldown.insert(pending.ip, Instant::now());
            }
            pending
        };
        pending
            .decision
            .send(decision)
            .map_err(|_| "Pairing session is no longer active".to_string())
    }

    pub fn list(&self) -> Vec<PairRequestEvent> {
        self.inner
            .lock()
            .expect("pairing mutex poisoned")
            .pending
            .values()
            .map(|p| p.event.clone())
            .collect()
    }

    /// Drop a request that ended without a user decision (timeout, disconnect).
    pub fn remove(&self, device_id: &str) {
        let mut inner = self.inner.lock().expect("pairing mutex poisoned");
        if let Some(p) = inner.pending.remove(device_id) {
            inner.cooldown.insert(p.ip, Instant::now());
        }
    }
}

/// Short authentication string the user compares on both screens.
///
/// It binds the desktop's TLS certificate fingerprint and both parties' nonces, so a
/// man-in-the-middle that terminates TLS with its own certificate makes the two
/// screens show different codes. The nonces are exchanged commit-then-reveal (the
/// phone commits to its nonce before it sees the desktop's), which stops an active
/// attacker from grinding nonces until the 6 digits match.
pub fn sas_code(
    phone_id: &str,
    desktop_id: &str,
    phone_nonce: &str,
    desktop_nonce: &str,
    desktop_cert_fingerprint: &[u8],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"pzync-sas-v1\0");
    for field in [
        phone_id.as_bytes(),
        desktop_id.as_bytes(),
        phone_nonce.as_bytes(),
        desktop_nonce.as_bytes(),
        desktop_cert_fingerprint,
    ] {
        hasher.update((field.len() as u32).to_be_bytes());
        hasher.update(field);
    }
    let digest = hasher.finalize();
    let value = digest
        .iter()
        .take(8)
        .fold(0u64, |acc, byte| (acc << 8) | u64::from(*byte));
    format!("{:06}", value % 1_000_000)
}

/// Commitment to a nonce: lowercase hex SHA-256.
pub fn nonce_commitment(nonce: &str) -> String {
    Sha256::digest(nonce.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn ev(id: &str) -> PairRequestEvent {
        PairRequestEvent {
            device_id: id.into(),
            name: "n".into(),
            code: "000000".into(),
            ip: "10.0.0.1".into(),
        }
    }

    #[test]
    fn sas_matches_cross_platform_vector() {
        // The Android unit test asserts the same vector.
        let fp: Vec<u8> = (0u8..32).collect();
        assert_eq!(sas_code("phone-1", "desktop-1", "aabbcc", "ddeeff", &fp), SAS_VECTOR);
    }
    const SAS_VECTOR: &str = "929466";

    #[test]
    fn sas_depends_on_fingerprint() {
        let a = sas_code("p", "d", "n1", "n2", &[1; 32]);
        let b = sas_code("p", "d", "n1", "n2", &[2; 32]);
        assert_ne!(a, b);
    }

    #[test]
    fn commitment_is_hex_sha256() {
        assert_eq!(
            nonce_commitment("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn begin_enforces_limits_and_cooldown() {
        let m = PairingManager::default();
        let ip1: IpAddr = "10.0.0.1".parse().unwrap();
        let ip2: IpAddr = "10.0.0.2".parse().unwrap();
        let (tx, _rx) = channel();
        assert!(m.begin(ip1, ev("a"), tx.clone()).is_ok());
        assert!(m.begin(ip1, ev("b"), tx.clone()).is_err(), "same ip");
        assert!(m.begin(ip2, ev("a"), tx.clone()).is_err(), "same device id");
        assert!(m.begin(ip2, ev("b"), tx.clone()).is_ok());
        assert!(m.begin("10.0.0.3".parse().unwrap(), ev("c"), tx.clone()).is_ok());
        assert!(m.begin("10.0.0.4".parse().unwrap(), ev("d"), tx.clone()).is_err(), "max pending");
        // reject -> cooldown for that ip
        m.resolve("a", PairDecision::Reject).ok();
        assert!(m.begin(ip1, ev("e"), tx).is_err(), "cooldown after reject");
    }
}
