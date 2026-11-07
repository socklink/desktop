//! Networking layer.
//!
//! This module groups everything that has to do with talking to other
//! devices on the local network:
//!
//! * [`discovery`] – UDP broadcast so that an Android device can find
//!   this desktop on the LAN.
//! * [`tcp`] – TCP server that accepts the actual sync connections.
//! * [`message`] – wire-format types shared by both sides.

pub mod discovery;
pub mod message;
pub mod pairing;
pub mod protocol;
pub mod session;
pub mod tcp;
pub mod tls;

/// Default UDP port used for the discovery broadcast.
pub const DEFAULT_DISCOVERY_PORT: u16 = 8200;

/// Default TCP port used for the sync connection.
pub const DEFAULT_TCP_PORT: u16 = 8080;
