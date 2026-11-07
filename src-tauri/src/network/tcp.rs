//! TCP server that accepts incoming sync connections from peers.
//!
//! Peers connect to the public port over TLS (see [`crate::network::tls`]). The
//! decrypted stream reaches this module on a loopback-only listener, preceded by a
//! preamble that carries the peer's real address.

use std::net::TcpListener;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread;

use crate::network::session::{handle_client, SessionContext};
use crate::network::tls;

/// Spawn the TLS listener on `0.0.0.0:port` and the loopback session listener.
/// Each accepted session is handled in its own thread by [`handle_client`].
pub fn start_server(port: u16, ctx: SessionContext) {
    let public = match TcpListener::bind(format!("0.0.0.0:{port}")) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[tcp] Failed to bind 0.0.0.0:{port}: {e}");
            return;
        }
    };
    let inner = match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[tcp] Failed to bind loopback listener: {e}");
            return;
        }
    };
    let inner_port = match inner.local_addr() {
        Ok(a) => a.port(),
        Err(e) => {
            eprintln!("[tcp] Could not read loopback port: {e}");
            return;
        }
    };

    println!("[tcp] Listening on 0.0.0.0:{port} (TLS)");

    let config = tls::identity().config.clone();
    thread::spawn(move || {
        tls::run_proxy(public, inner_port, config, Arc::new(AtomicBool::new(false)));
    });

    thread::spawn(move || {
        for stream in inner.incoming() {
            match stream {
                Ok(mut stream) => {
                    let session_ctx = ctx.clone();
                    thread::spawn(move || {
                        let remote_ip = match tls::read_proxy_preamble(&mut stream) {
                            Ok(ip) => ip,
                            Err(e) => {
                                eprintln!("[tcp] Rejected loopback connection: {e}");
                                return;
                            }
                        };
                        println!("[tcp] New connection from {remote_ip}");
                        handle_client(stream, remote_ip, session_ctx);
                    });
                }
                Err(e) => eprintln!("[tcp] Accept error: {e}"),
            }
        }
    });
}
