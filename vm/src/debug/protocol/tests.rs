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
        chunks: vec![
            wire[..3].to_vec(),
            wire[3..12].to_vec(),
            wire[12..].to_vec(),
        ],
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
