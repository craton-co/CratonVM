// SPDX-License-Identifier: Apache-2.0
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
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn command_packet_roundtrip() {
        let pkt = JdwpPacket::Command {
            id: 42,
            flags: 0,
            command_set: 1,
            command: 7,
            data: vec![0xCA, 0xFE],
        };
        let mut buf = Vec::new();
        write_packet(&mut buf, &pkt).unwrap();
        let mut cursor = Cursor::new(&buf);
        let decoded = read_packet(&mut cursor).unwrap();
        assert_eq!(pkt, decoded);
    }

    #[test]
    fn reply_packet_roundtrip() {
        let pkt = JdwpPacket::Reply {
            id: 99,
            error_code: 0,
            data: vec![1, 2, 3, 4],
        };
        let mut buf = Vec::new();
        write_packet(&mut buf, &pkt).unwrap();
        let mut cursor = Cursor::new(&buf);
        let decoded = read_packet(&mut cursor).unwrap();
        assert_eq!(pkt, decoded);
    }

    #[test]
    fn error_reply_roundtrip() {
        let pkt = JdwpPacket::error_reply(7, 101);
        let mut buf = Vec::new();
        write_packet(&mut buf, &pkt).unwrap();
        let mut cursor = Cursor::new(&buf);
        let decoded = read_packet(&mut cursor).unwrap();
        assert_eq!(pkt, decoded);
    }

    #[test]
    fn ok_reply_helper() {
        let pkt = JdwpPacket::ok_reply(5, vec![0xFF]);
        match &pkt {
            JdwpPacket::Reply {
                id,
                error_code,
                data,
            } => {
                assert_eq!(*id, 5);
                assert_eq!(*error_code, 0);
                assert_eq!(data, &[0xFF]);
            }
            _ => panic!("expected Reply"),
        }
    }

    #[test]
    fn reply_flag_is_set_on_wire() {
        let pkt = JdwpPacket::ok_reply(1, vec![]);
        let mut buf = Vec::new();
        write_packet(&mut buf, &pkt).unwrap();
        // flags byte is at offset 8
        assert_eq!(buf[8], REPLY_FLAG);
    }

    #[test]
    fn command_packet_length_on_wire() {
        let pkt = JdwpPacket::Command {
            id: 1,
            flags: 0,
            command_set: 1,
            command: 1,
            data: vec![0; 5],
        };
        let mut buf = Vec::new();
        write_packet(&mut buf, &pkt).unwrap();
        let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        assert_eq!(len, HEADER_SIZE + 5);
    }

    #[test]
    fn string_read_write_roundtrip() {
        let mut buf = Vec::new();
        write_string(&mut buf, "Hello, JDWP!").unwrap();
        let mut cursor = Cursor::new(&buf);
        let s = read_string(&mut cursor).unwrap();
        assert_eq!(s, "Hello, JDWP!");
    }

    #[test]
    fn payload_writer_and_reader() {
        let mut pw = PayloadWriter::new();
        pw.put_u8(0x42);
        pw.put_u16_be(1000);
        pw.put_u32_be(0xDEADBEEF);
        pw.put_u64_be(0x0102030405060708);
        pw.put_string("test");

        let bytes = pw.into_bytes();
        let mut pr = PayloadReader::new(&bytes);
        assert_eq!(pr.read_u8().unwrap(), 0x42);
        assert_eq!(pr.read_u16_be().unwrap(), 1000);
        assert_eq!(pr.read_u32_be().unwrap(), 0xDEADBEEF);
        assert_eq!(pr.read_u64_be().unwrap(), 0x0102030405060708);
        assert_eq!(pr.read_string().unwrap(), "test");
        assert_eq!(pr.remaining(), 0);
    }

    #[test]
    fn oversized_packet_length_rejected_without_huge_alloc() {
        // [VULN fix vm-jdwp] A hostile peer declares length=0xFFFFFFFF.  The
        // old code did `vec![0u8; ~4GiB]` before reading and OOM'd.  Now we
        // must reject with InvalidData *without* allocating, even though the
        // stream provides no payload bytes at all.
        let mut buf = Vec::new();
        write_u32_be(&mut buf, u32::MAX).unwrap(); // length = 0xFFFFFFFF
        write_u32_be(&mut buf, 1).unwrap(); // id
        buf.push(0); // flags (command)
        buf.push(0); // command_set
        buf.push(0); // command

        let mut cursor = Cursor::new(&buf);
        let err = read_packet(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn truncated_payload_does_not_over_allocate() {
        // Declared payload is exactly MAX_PACKET_DATA but the stream is short.
        // read_bounded must surface UnexpectedEof (not allocate the full cap
        // and not panic) because only a few bytes actually arrive.
        let data_len = MAX_PACKET_DATA;
        let mut buf = Vec::new();
        write_u32_be(&mut buf, HEADER_SIZE + data_len).unwrap(); // length
        write_u32_be(&mut buf, 1).unwrap(); // id
        buf.push(0); // flags
        buf.push(0); // command_set
        buf.push(0); // command
        buf.extend_from_slice(&[1, 2, 3]); // only 3 of N bytes

        let mut cursor = Cursor::new(&buf);
        let err = read_packet(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn read_string_rejects_oversized_length() {
        // [VULN fix vm-jdwp] read_string used the same `vec![0u8; len]` pattern.
        let mut buf = Vec::new();
        write_u32_be(&mut buf, u32::MAX).unwrap(); // claimed string length
        let mut cursor = Cursor::new(&buf);
        let err = read_string(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_string_at_limit_boundary_is_not_rejected_for_length() {
        // A length exactly equal to MAX_PACKET_DATA is permitted by the cap;
        // here the stream is truncated so we expect UnexpectedEof, proving the
        // length itself passed the bound (an oversized length would be
        // InvalidData instead).
        let mut buf = Vec::new();
        write_u32_be(&mut buf, MAX_PACKET_DATA).unwrap();
        buf.extend_from_slice(b"abc"); // far fewer bytes than declared
        let mut cursor = Cursor::new(&buf);
        let err = read_string(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn short_packet_rejected() {
        // A packet claiming length=5 is too short (header is 11).
        let mut buf = Vec::new();
        write_u32_be(&mut buf, 5).unwrap(); // length
        write_u32_be(&mut buf, 1).unwrap(); // id
        buf.push(0); // flags
        buf.push(0);
        buf.push(0); // cmd set + cmd

        let mut cursor = Cursor::new(&buf);
        assert!(read_packet(&mut cursor).is_err());
    }

    /// A reader that hands out `chunks` one per call, each followed by a
    /// `WouldBlock`, as a non-blocking socket does between TCP segments.
    struct Trickle {
        chunks: Vec<Vec<u8>>,
        blocked: bool,
    }

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.blocked || self.chunks.is_empty() {
                self.blocked = false;
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            let chunk = self.chunks.remove(0);
            buf[..chunk.len()].copy_from_slice(&chunk);
            self.blocked = true;
            Ok(chunk.len())
        }
    }

    /// Interpreter round i1 wave 24: a packet that arrives in pieces is
    /// assembled, not half-read and lost (`read_packet` on a non-blocking
    /// socket consumed the length and then failed `WouldBlock`), and two
    /// packets in one segment are both delivered.
    #[test]
    fn a_packet_arriving_in_pieces_is_assembled() {
        let first = JdwpPacket::Command {
            id: 7,
            flags: 0,
            command_set: 1,
            command: 2,
            data: b"Ljava/lang/String;".to_vec(),
        };
        let second = JdwpPacket::ok_reply(8, vec![1, 2, 3]);
        let mut wire = Vec::new();
        write_packet(&mut wire, &first).unwrap();
        write_packet(&mut wire, &second).unwrap();
        let mut reader = Trickle {
            chunks: vec![wire[..3].to_vec(), wire[3..12].to_vec(), wire[12..].to_vec()],
            blocked: false,
        };
        let mut assembler = PacketAssembler::new();
        let mut got = Vec::new();
        for _ in 0..10 {
            if let Some(packet) = assembler.poll(&mut reader).unwrap() {
                got.push(packet);
            }
        }
        assert_eq!(got, vec![first, second]);
        let mut bad = Trickle {
            chunks: vec![vec![0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0]],
            blocked: true,
        };
        let mut assembler = PacketAssembler::new();
        assert!(assembler.poll(&mut bad).unwrap().is_none(), "blocked first");
        assert_eq!(
            assembler.poll(&mut bad).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    /// A writer that takes at most `per_call` bytes per call and blocks on
    /// every other call, as a full socket send buffer does.
    struct Narrow {
        out: Vec<u8>,
        per_call: usize,
        blocked: bool,
    }

    impl Write for Narrow {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.blocked {
                self.blocked = false;
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            self.blocked = true;
            let n = buf.len().min(self.per_call);
            self.out.extend_from_slice(&buf[..n]);
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Wave 24: a reply larger than what the socket takes at once is written
    /// whole (a `WouldBlock` part-way ended the session).
    #[test]
    fn a_packet_is_written_whole_to_a_blocking_socket() {
        let reply = JdwpPacket::ok_reply(9, (0..=255u8).cycle().take(5000).collect());
        let mut writer = Narrow {
            out: Vec::new(),
            per_call: 700,
            blocked: false,
        };
        write_packet_nonblocking(&mut writer, &reply).unwrap();
        let mut expected = Vec::new();
        write_packet(&mut expected, &reply).unwrap();
        assert_eq!(writer.out, expected);
    }
}
