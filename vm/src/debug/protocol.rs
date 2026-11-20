// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JDWP packet format — serialization and deserialization.
//!
//! Every JDWP packet has an 11-byte header:
//!
//! | Offset | Size | Field        |
//! |--------|------|--------------|
//! | 0      | 4    | length (BE)  |
//! | 4      | 4    | id (BE)      |
//! | 8      | 1    | flags        |
//! | 9      | 2    | varies       |
//!
//! For **command** packets the last two bytes are `command_set` (1 byte) and
//! `command` (1 byte).  For **reply** packets `flags` has bit 0x80 set and
//! the last two bytes are an `error_code` (u16 BE).

use std::io::{self, Read, Write};

/// Flag that marks a reply packet.
pub const REPLY_FLAG: u8 = 0x80;

/// Minimum packet size (header only, no payload).
pub const HEADER_SIZE: u32 = 11;

/// Upper bound on a single packet payload (and on an individual JDWP string),
/// in bytes.  The JDWP wire format carries lengths as a 32-bit field, so a
/// hostile or buggy peer can declare a payload of up to ~4 GiB.  Without a
/// cap, `read_packet`/`read_string` would `vec![0u8; declared_len]` up front
/// and exhaust memory before a single byte is read (a trivial pre-auth DoS).
/// We clamp to a generous-but-sane ceiling; legitimate JDWP traffic
/// (stack frames, variable slots, class lists) is comfortably under this.
///
/// [VULN fix vm-jdwp] bound attacker-controlled allocation sizes.
pub const MAX_PACKET_DATA: u32 = 64 * 1024 * 1024; // 64 MiB

/// Read exactly `len` bytes from `reader` into a freshly-allocated buffer,
/// but cap the up-front allocation at `MAX_PACKET_DATA` and grow the buffer
/// incrementally as bytes actually arrive.  This prevents a peer from forcing
/// a multi-gigabyte allocation merely by *declaring* a huge length: a buffer
/// is only as large as the data the peer is actually willing to send.
///
/// Returns `InvalidData` if `len` exceeds `MAX_PACKET_DATA`, and
/// `UnexpectedEof` (via `read_to_end` semantics) if the stream ends early.
///
/// [VULN fix vm-jdwp] bounded reader replaces `vec![0u8; len]` before read.
fn read_bounded<R: Read>(reader: &mut R, len: usize) -> io::Result<Vec<u8>> {
    if len > MAX_PACKET_DATA as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "declared payload length {} exceeds maximum {}",
                len, MAX_PACKET_DATA
            ),
        ));
    }
    // `len` is now known to be <= MAX_PACKET_DATA, so the with_capacity reserve
    // is bounded.  `take(len)` guarantees we never read past the declared size,
    // and `read_to_end` grows the Vec only as real bytes arrive.
    let mut buf = Vec::with_capacity(len);
    let read = reader.take(len as u64).read_to_end(&mut buf)?;
    if read != len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("expected {} payload bytes, got {}", len, read),
        ));
    }
    Ok(buf)
}

// ---------------------------------------------------------------------------
// JdwpPacket
// ---------------------------------------------------------------------------

/// A fully-parsed JDWP packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JdwpPacket {
    Command {
        id: u32,
        flags: u8,
        command_set: u8,
        command: u8,
        data: Vec<u8>,
    },
    Reply {
        id: u32,
        error_code: u16,
        data: Vec<u8>,
    },
}

impl JdwpPacket {
    /// Build a reply with no error.
    pub fn ok_reply(id: u32, data: Vec<u8>) -> Self {
        Self::Reply {
            id,
            error_code: 0,
            data,
        }
    }

    /// Build an error reply.
    pub fn error_reply(id: u32, error_code: u16) -> Self {
        Self::Reply {
            id,
            error_code,
            data: Vec::new(),
        }
    }

    /// Packet id regardless of variant.
    pub fn id(&self) -> u32 {
        match self {
            Self::Command { id, .. } | Self::Reply { id, .. } => *id,
        }
    }
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// Read a single JDWP packet from `reader`.
pub fn read_packet<R: Read>(reader: &mut R) -> io::Result<JdwpPacket> {
    let length = read_u32_be(reader)?;
    if length < HEADER_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "packet length {} is less than header size {}",
                length, HEADER_SIZE
            ),
        ));
    }
    let id = read_u32_be(reader)?;
    let flags = read_u8(reader)?;

    let data_len = (length - HEADER_SIZE) as usize;

    if flags & REPLY_FLAG != 0 {
        let error_code = read_u16_be(reader)?;
        // [VULN fix vm-jdwp] bound the payload allocation: a peer declaring
        // length=0xFFFFFFFF used to force a ~4 GiB `vec![0u8; data_len]` here
        // before any byte was read.  read_bounded clamps + grows incrementally.
        let data = read_bounded(reader, data_len)?;
        Ok(JdwpPacket::Reply {
            id,
            error_code,
            data,
        })
    } else {
        let command_set = read_u8(reader)?;
        let command = read_u8(reader)?;
        // [VULN fix vm-jdwp] same bound for command payloads (see above).
        let data = read_bounded(reader, data_len)?;
        Ok(JdwpPacket::Command {
            id,
            flags,
            command_set,
            command,
            data,
        })
    }
}

// ---------------------------------------------------------------------------
// A non-blocking stream (interpreter round i1 wave 24)
// ---------------------------------------------------------------------------
//
// The JDWP server (`debug::run_jdwp_server`) sets its socket non-blocking so
// it can interleave commands with events. `read_packet` and `write_packet`
// are made of several `read_exact` / `write_all` calls, and on a
// non-blocking socket each of them may stop with `WouldBlock` part-way: the
// bytes a read had consumed, or a write had sent, were lost, so a packet
// that arrived in two TCP segments — or a reply larger than the socket's
// send buffer (`AllClassesWithGeneric` of a few thousand classes,
// `VisibleClasses`) — desynchronised the stream or ended the session. The
// server reads through a [`PacketAssembler`] and writes whole packets with
// [`write_packet_nonblocking`].

/// How long [`write_packet_nonblocking`] waits for a debugger that reads
/// nothing before it gives the connection up.
const WRITE_STALL_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Collects the bytes a non-blocking stream delivers until they make whole
/// JDWP packets.
#[derive(Debug, Default)]
pub struct PacketAssembler {
    buf: Vec<u8>,
}

impl PacketAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// The next whole packet: one already buffered, else what `reader` has
    /// now. `Ok(None)` when no whole packet has arrived yet (`reader` would
    /// block); an error when the stream failed or closed, or declared a
    /// malformed length.
    pub fn poll<R: Read>(&mut self, reader: &mut R) -> io::Result<Option<JdwpPacket>> {
        if let Some(packet) = self.take_packet()? {
            return Ok(Some(packet));
        }
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
                Ok(n) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    if let Some(packet) = self.take_packet()? {
                        return Ok(Some(packet));
                    }
                }
                // A read timeout ends as `WouldBlock` on Unix and as
                // `TimedOut` on Windows (wave 25: the server reads with one,
                // `debug::SERVER_READ_WAIT`).
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None)
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// A whole packet from the front of the buffer, if one is there.
    fn take_packet(&mut self) -> io::Result<Option<JdwpPacket>> {
        let Some(head) = self.buf.get(..4) else {
            return Ok(None);
        };
        let mut length = [0u8; 4];
        length.copy_from_slice(head);
        let length = u32::from_be_bytes(length);
        if length < HEADER_SIZE || length - HEADER_SIZE > MAX_PACKET_DATA {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("malformed packet length {length}"),
            ));
        }
        let length = length as usize; // Widening: bounded above
        if self.buf.len() < length {
            return Ok(None);
        }
        let packet: Vec<u8> = self.buf.drain(..length).collect();
        read_packet(&mut packet.as_slice()).map(Some)
    }
}

/// Write `packet` whole to a non-blocking `writer`: encoded first, then
/// written with the `WouldBlock`s waited out (a debugger that reads nothing
/// for [`WRITE_STALL_LIMIT`] ends the session, `TimedOut`).
pub fn write_packet_nonblocking<W: Write>(writer: &mut W, packet: &JdwpPacket) -> io::Result<()> {
    let mut bytes = Vec::new();
    write_packet(&mut bytes, packet)?;
    let mut written = 0;
    let mut stalled_since: Option<std::time::Instant> = None;
    while written < bytes.len() {
        match writer.write(&bytes[written..]) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
            Ok(n) => {
                written += n;
                stalled_since = None;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let since = *stalled_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() > WRITE_STALL_LIMIT {
                    return Err(io::Error::from(io::ErrorKind::TimedOut));
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    loop {
        match writer.flush() {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            other => return other,
        }
    }
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// Write a JDWP packet to `writer`.
pub fn write_packet<W: Write>(writer: &mut W, packet: &JdwpPacket) -> io::Result<()> {
    match packet {
        JdwpPacket::Command {
            id,
            flags,
            command_set,
            command,
            data,
        } => {
            let length = HEADER_SIZE + data.len() as u32;
            write_u32_be(writer, length)?;
            write_u32_be(writer, *id)?;
            write_u8(writer, *flags)?;
            write_u8(writer, *command_set)?;
            write_u8(writer, *command)?;
            writer.write_all(data)?;
        }
        JdwpPacket::Reply {
            id,
            error_code,
            data,
        } => {
            let length = HEADER_SIZE + data.len() as u32;
            write_u32_be(writer, length)?;
            write_u32_be(writer, *id)?;
            write_u8(writer, REPLY_FLAG)?;
            write_u16_be(writer, *error_code)?;
            writer.write_all(data)?;
        }
    }
    writer.flush()
}

// ---------------------------------------------------------------------------
// Primitive helpers (public so other modules can build/parse payloads)
// ---------------------------------------------------------------------------

pub fn read_u8<R: Read>(r: &mut R) -> io::Result<u8> {
    let mut buf = [0u8; 1];
    r.read_exact(&mut buf)?;
    Ok(buf[0])
}

pub fn read_u16_be<R: Read>(r: &mut R) -> io::Result<u16> {
    let mut buf = [0u8; 2];
    r.read_exact(&mut buf)?;
    Ok(u16::from_be_bytes(buf))
}

pub fn read_u32_be<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(u32::from_be_bytes(buf))
}

pub fn read_u64_be<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(u64::from_be_bytes(buf))
}

pub fn read_string<R: Read>(r: &mut R) -> io::Result<String> {
    let len = read_u32_be(r)? as usize;
    // [VULN fix vm-jdwp] previously `vec![0u8; len]` allocated up to ~4 GiB
    // from an attacker-controlled wire u32 before reading.  read_bounded caps
    // the length at MAX_PACKET_DATA and grows the buffer only as bytes arrive.
    let buf = read_bounded(r, len)?;
    String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn write_u8<W: Write>(w: &mut W, v: u8) -> io::Result<()> {
    w.write_all(&[v])
}

pub fn write_u16_be<W: Write>(w: &mut W, v: u16) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

pub fn write_u32_be<W: Write>(w: &mut W, v: u32) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

pub fn write_u64_be<W: Write>(w: &mut W, v: u64) -> io::Result<()> {
    w.write_all(&v.to_be_bytes())
}

pub fn write_string<W: Write>(w: &mut W, s: &str) -> io::Result<()> {
    write_u32_be(w, s.len() as u32)?;
    w.write_all(s.as_bytes())
}

// ---------------------------------------------------------------------------
// Payload builder helpers
// ---------------------------------------------------------------------------

/// Convenience wrapper for building a reply payload in-memory.
pub struct PayloadWriter {
    buf: Vec<u8>,
}

impl PayloadWriter {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn put_u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn put_u16_be(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn put_u32_be(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn put_u64_be(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn put_string(&mut self, s: &str) {
        self.put_u32_be(s.len() as u32);
        self.buf.extend_from_slice(s.as_bytes());
    }

    pub fn put_bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

/// Convenience wrapper for reading fields from a payload slice.
pub struct PayloadReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> PayloadReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    pub fn read_u8(&mut self) -> io::Result<u8> {
        if self.remaining() < 1 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "payload too short",
            ));
        }
        let v = self.data[self.pos];
        self.pos += 1;
        Ok(v)
    }

    pub fn read_u16_be(&mut self) -> io::Result<u16> {
        if self.remaining() < 2 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "payload too short",
            ));
        }
        let v = u16::from_be_bytes([self.data[self.pos], self.data[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }

    pub fn read_u32_be(&mut self) -> io::Result<u32> {
        if self.remaining() < 4 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "payload too short",
            ));
        }
        let v = u32::from_be_bytes([
            self.data[self.pos],
            self.data[self.pos + 1],
            self.data[self.pos + 2],
            self.data[self.pos + 3],
        ]);
        self.pos += 4;
        Ok(v)
    }

    pub fn read_u64_be(&mut self) -> io::Result<u64> {
        if self.remaining() < 8 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "payload too short",
            ));
        }
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&self.data[self.pos..self.pos + 8]);
        self.pos += 8;
        Ok(u64::from_be_bytes(buf))
    }

    pub fn read_string(&mut self) -> io::Result<String> {
        let len = self.read_u32_be()? as usize;
        if self.remaining() < len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "payload too short",
            ));
        }
        let s = String::from_utf8(self.data[self.pos..self.pos + len].to_vec())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.pos += len;
        Ok(s)
    }

    /// The next `len` bytes (interpreter round i5 wave 104, lane L1: a
    /// `RedefineClasses` class file).
    pub fn read_bytes(&mut self, len: usize) -> io::Result<&'a [u8]> {
        if self.remaining() < len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "payload too short",
            ));
        }
        let bytes = &self.data[self.pos..self.pos + len];
        self.pos += len;
        Ok(bytes)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
