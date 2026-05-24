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
            format!("packet length {} is less than header size {}", length, HEADER_SIZE),
        ));
    }
    let id = read_u32_be(reader)?;
    let flags = read_u8(reader)?;

    let data_len = (length - HEADER_SIZE) as usize;

    if flags & REPLY_FLAG != 0 {
        let error_code = read_u16_be(reader)?;
        let mut data = vec![0u8; data_len];
        reader.read_exact(&mut data)?;
        Ok(JdwpPacket::Reply {
            id,
            error_code,
            data,
        })
    } else {
        let command_set = read_u8(reader)?;
        let command = read_u8(reader)?;
        let mut data = vec![0u8; data_len];
        reader.read_exact(&mut data)?;
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
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
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
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "payload too short"));
        }
        let v = self.data[self.pos];
        self.pos += 1;
        Ok(v)
    }

    pub fn read_u16_be(&mut self) -> io::Result<u16> {
        if self.remaining() < 2 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "payload too short"));
        }
        let v = u16::from_be_bytes([self.data[self.pos], self.data[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }

    pub fn read_u32_be(&mut self) -> io::Result<u32> {
        if self.remaining() < 4 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "payload too short"));
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
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "payload too short"));
        }
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&self.data[self.pos..self.pos + 8]);
        self.pos += 8;
        Ok(u64::from_be_bytes(buf))
    }

    pub fn read_string(&mut self) -> io::Result<String> {
        let len = self.read_u32_be()? as usize;
        if self.remaining() < len {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "payload too short"));
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
            JdwpPacket::Reply { id, error_code, data } => {
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
    fn short_packet_rejected() {
        // A packet claiming length=5 is too short (header is 11).
        let mut buf = Vec::new();
        write_u32_be(&mut buf, 5).unwrap(); // length
        write_u32_be(&mut buf, 1).unwrap(); // id
        buf.push(0);                        // flags
        buf.push(0); buf.push(0);           // cmd set + cmd

        let mut cursor = Cursor::new(&buf);
        assert!(read_packet(&mut cursor).is_err());
    }
}
