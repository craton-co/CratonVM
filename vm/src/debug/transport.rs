// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JDWP TCP transport layer.
//!
//! Listens for a single debugger connection, performs the JDWP handshake, and
//! then wraps the connection for packet I/O.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;

use crate::debug::protocol::{self, JdwpPacket};

/// The canonical 14-byte JDWP handshake string.
pub const JDWP_HANDSHAKE: &[u8; 14] = b"JDWP-Handshake";

/// Default JDWP listen port.
pub const DEFAULT_PORT: u16 = 5005;

// ---------------------------------------------------------------------------
// JdwpConnection
// ---------------------------------------------------------------------------

/// A connected, handshake-completed JDWP session.
pub struct JdwpConnection {
    stream: TcpStream,
}

impl JdwpConnection {
    /// Wrap an already-handshaked `TcpStream`.
    pub fn new(stream: TcpStream) -> Self {
        Self { stream }
    }

    /// Read one JDWP packet (blocking).
    pub fn read_packet(&mut self) -> io::Result<JdwpPacket> {
        protocol::read_packet(&mut self.stream)
    }

    /// Write one JDWP packet.
    pub fn write_packet(&mut self, packet: &JdwpPacket) -> io::Result<()> {
        protocol::write_packet(&mut self.stream, packet)
    }

    /// Set a read timeout on the underlying stream.
    pub fn set_read_timeout(&self, dur: Option<std::time::Duration>) -> io::Result<()> {
        self.stream.set_read_timeout(dur)
    }

    /// Try to clone the underlying stream (for separate reader/writer threads).
    pub fn try_clone(&self) -> io::Result<JdwpConnection> {
        Ok(JdwpConnection {
            stream: self.stream.try_clone()?,
        })
    }
}

// ---------------------------------------------------------------------------
// JdwpTransport
// ---------------------------------------------------------------------------

/// Listens for a single JDWP debugger attachment.
pub struct JdwpTransport {
    port: u16,
}

impl JdwpTransport {
    pub fn new(port: u16) -> Self {
        Self { port }
    }

    /// Bind and block until a debugger connects + handshakes.
    pub fn accept(&self) -> io::Result<JdwpConnection> {
        let listener = TcpListener::bind(("127.0.0.1", self.port))?;
        tracing::info!(port = self.port, "JDWP transport listening");

        let (stream, addr) = listener.accept()?;
        tracing::info!(?addr, "debugger connected");

        let conn = perform_handshake(stream)?;
        Ok(conn)
    }

    /// Spawn a background thread that waits for a debugger to connect.
    ///
    /// Returns a `Receiver` that will yield the connection once established.
    pub fn start_listener(port: u16) -> io::Result<mpsc::Receiver<io::Result<JdwpConnection>>> {
        let (tx, rx) = mpsc::channel();

        thread::Builder::new()
            .name("jdwp-listener".into())
            .spawn(move || {
                let transport = JdwpTransport::new(port);
                let result = transport.accept();
                let _ = tx.send(result);
            })?;

        Ok(rx)
    }
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

/// Perform the JDWP handshake on a raw `TcpStream`.
///
/// 1. Read 14 bytes from the client — must be `"JDWP-Handshake"`.
/// 2. Echo the same 14 bytes back.
pub fn perform_handshake(mut stream: TcpStream) -> io::Result<JdwpConnection> {
    let mut buf = [0u8; 14];
    stream.read_exact(&mut buf)?;
    if &buf != JDWP_HANDSHAKE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid JDWP handshake from client",
        ));
    }
    stream.write_all(JDWP_HANDSHAKE)?;
    stream.flush()?;
    tracing::debug!("JDWP handshake completed");
    Ok(JdwpConnection::new(stream))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_bytes_are_correct() {
        assert_eq!(JDWP_HANDSHAKE.len(), 14);
        assert_eq!(JDWP_HANDSHAKE, b"JDWP-Handshake");
    }

    #[test]
    fn handshake_roundtrip_over_tcp() {
        // Bind to an ephemeral port.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            perform_handshake(stream).unwrap()
        });

        // Client side: connect and send the handshake.
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        client.write_all(JDWP_HANDSHAKE).unwrap();
        client.flush().unwrap();
        let mut resp = [0u8; 14];
        client.read_exact(&mut resp).unwrap();
        assert_eq!(&resp, JDWP_HANDSHAKE);

        // Server thread should have succeeded.
        let _conn = server.join().unwrap();
    }

    #[test]
    fn bad_handshake_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            perform_handshake(stream)
        });

        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        client.write_all(b"NOT-A-HANDSHAK").unwrap();
        client.flush().unwrap();

        let result = server.join().unwrap();
        assert!(result.is_err());
    }
}
