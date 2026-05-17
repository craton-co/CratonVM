//! JFR binary file writer.
//!
//! Implements the JFR v2.0 binary format used by OpenJDK's Flight Recorder.
//! The format consists of:
//!   1. A 68-byte file header
//!   2. Event records (variable-length, LEB128-encoded)
//!   3. A checkpoint section with constant pool entries
//!   4. A metadata section describing event types

use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

use crate::event::{EventInstance, EventTypeId, EventTypeRegistry, EventValue};
use crate::repository::EventRepository;

/// JFR file magic bytes: `FLR\0`
pub const JFR_MAGIC: [u8; 4] = [b'F', b'L', b'R', 0];

/// JFR format major version
pub const JFR_VERSION_MAJOR: u16 = 2;
/// JFR format minor version
pub const JFR_VERSION_MINOR: u16 = 0;

/// File header size in bytes.
/// 4 (magic) + 2 (major) + 2 (minor) + 8*7 (file_size, checkpoint_offset,
/// metadata_offset, start_time, duration, start_ticks, ticks_per_second) +
/// 1 (file_state) + 7 (padding) = 72
pub const HEADER_SIZE: u64 = 72;

/// File state: writing (incomplete)
pub const FILE_STATE_WRITING: u8 = 0;
/// File state: complete
pub const FILE_STATE_COMPLETE: u8 = 1;

/// Ticks per second (nanosecond resolution)
pub const TICKS_PER_SECOND: u64 = 1_000_000_000;

/// Errors that can occur during JFR dump.
#[derive(Debug)]
pub enum JfrDumpError {
    Io(io::Error),
    NotStopped,
    NoEvents,
}

impl std::fmt::Display for JfrDumpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JfrDumpError::Io(e) => write!(f, "I/O error: {}", e),
            JfrDumpError::NotStopped => write!(f, "recording must be stopped before dumping"),
            JfrDumpError::NoEvents => write!(f, "recording has no events"),
        }
    }
}

impl std::error::Error for JfrDumpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            JfrDumpError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for JfrDumpError {
    fn from(e: io::Error) -> Self {
        JfrDumpError::Io(e)
    }
}

// ---------------------------------------------------------------------------
// LEB128-style compressed int encoding used by JFR
// ---------------------------------------------------------------------------

/// Encode a u64 as a JFR compressed integer (LEB128 variant).
///
/// JFR uses an unsigned LEB128 encoding where each byte stores 7 bits of data
/// and the high bit indicates whether more bytes follow.
pub fn encode_compressed_int(value: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(10);
    let mut v = value;
    loop {
        let mut byte = (v & 0x7F) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if v == 0 {
            break;
        }
    }
    buf
}

/// Encode a signed i64 as a JFR compressed long (zigzag + LEB128).
pub fn encode_compressed_long(value: i64) -> Vec<u8> {
    // Zigzag encode: map signed to unsigned so small magnitudes produce small values
    let zigzag = ((value << 1) ^ (value >> 63)) as u64;
    encode_compressed_int(zigzag)
}

/// Decode a JFR compressed integer from a byte slice, returning `(value, bytes_consumed)`.
pub fn decode_compressed_int(data: &[u8]) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    for (i, &byte) in data.iter().enumerate() {
        result |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((result, i + 1));
        }
        shift += 7;
        if shift >= 70 {
            return None; // overflow
        }
    }
    None // ran out of bytes
}

/// Decode a JFR compressed signed long from a byte slice, returning `(value, bytes_consumed)`.
pub fn decode_compressed_long(data: &[u8]) -> Option<(i64, usize)> {
    let (zigzag, consumed) = decode_compressed_int(data)?;
    // Zigzag decode
    let value = ((zigzag >> 1) as i64) ^ (-((zigzag & 1) as i64));
    Some((value, consumed))
}

// ---------------------------------------------------------------------------
// Event serialization
// ---------------------------------------------------------------------------

/// Encode a single event field value into a byte buffer.
fn encode_event_value(value: &EventValue, buf: &mut Vec<u8>) {
    match value {
        EventValue::Long(v) => buf.extend_from_slice(&encode_compressed_long(*v)),
        EventValue::Int(v) => buf.extend_from_slice(&encode_compressed_long(*v as i64)),
        EventValue::Float(v) => buf.extend_from_slice(&v.to_bits().to_be_bytes()),
        EventValue::Double(v) => buf.extend_from_slice(&v.to_bits().to_be_bytes()),
        EventValue::Boolean(v) => buf.push(if *v { 1 } else { 0 }),
        EventValue::String(s) => {
            // JFR string encoding: length-prefixed UTF-8
            // Encoding type 3 = UTF-8 with length
            buf.push(3); // STRING_ENCODING_UTF8
            let bytes = s.as_bytes();
            buf.extend_from_slice(&encode_compressed_int(bytes.len() as u64));
            buf.extend_from_slice(bytes);
        }
        EventValue::Null => {
            // JFR null string: encoding type 0
            buf.push(0);
        }
    }
}

/// Serialize a single event into a self-contained byte record.
///
/// Layout: [size: compressed_int] [type_id: compressed_int] [start_ticks: compressed_long]
///         [duration_ticks: compressed_long] [thread_id: compressed_long] [fields...]
///
/// The `size` field encodes the total record length in bytes (including the
/// size field itself).  Because the size field is variable-length, we may need
/// to iterate to get a stable encoding.
fn serialize_event(
    type_id: EventTypeId,
    start_time: u64,
    end_time: u64,
    thread_id: u64,
    fields: &[EventValue],
) -> Vec<u8> {
    // Encode the body (everything after the size prefix)
    let mut body = Vec::new();
    body.extend_from_slice(&encode_compressed_int(type_id.0 as u64));
    body.extend_from_slice(&encode_compressed_long(start_time as i64));
    let duration = end_time.saturating_sub(start_time);
    body.extend_from_slice(&encode_compressed_long(duration as i64));
    body.extend_from_slice(&encode_compressed_long(thread_id as i64));
    for field in fields {
        encode_event_value(field, &mut body);
    }

    // Determine the size prefix.  The total record size includes the size
    // field itself, so we iterate: guess the size-of-size, check if it
    // remains stable, and fix up if needed.
    let body_len = body.len();
    let mut size_prefix_len = 1usize;
    loop {
        let total = size_prefix_len + body_len;
        let actual_prefix_len = encode_compressed_int(total as u64).len();
        if actual_prefix_len == size_prefix_len {
            let mut record = Vec::with_capacity(total);
            record.extend_from_slice(&encode_compressed_int(total as u64));
            record.extend_from_slice(&body);
            return record;
        }
        size_prefix_len = actual_prefix_len;
    }
}

// ---------------------------------------------------------------------------
// Metadata section
// ---------------------------------------------------------------------------

/// Type IDs used in the JFR metadata.
const METADATA_TYPE_ID: u64 = 0;
const CHECKPOINT_TYPE_ID: u64 = 1;

/// Write the metadata section describing all event types.
/// Returns the bytes of the metadata section.
fn build_metadata_section(registry: &EventTypeRegistry, start_time_ns: u64) -> Vec<u8> {
    // The metadata section is itself an event with type_id = METADATA_TYPE_ID.
    // It contains a description of all event types using a simplified encoding.
    //
    // Real JFR metadata uses a complex XML-like structure stored in binary.
    // We use a simplified but compatible format: a single metadata event containing
    // all type descriptors encoded as compressed fields.

    let mut body = Vec::new();

    // Metadata event header
    body.extend_from_slice(&encode_compressed_int(METADATA_TYPE_ID));
    body.extend_from_slice(&encode_compressed_long(start_time_ns as i64));
    // Duration = 0 for metadata
    body.extend_from_slice(&encode_compressed_long(0));

    // Number of type descriptors
    let types: Vec<_> = registry.iter().collect();
    body.extend_from_slice(&encode_compressed_int(types.len() as u64));

    for (id, event_type) in &types {
        // Type ID
        body.extend_from_slice(&encode_compressed_int(id.0 as u64));
        // Name (length-prefixed UTF-8)
        let name_bytes = event_type.name.as_bytes();
        body.extend_from_slice(&encode_compressed_int(name_bytes.len() as u64));
        body.extend_from_slice(name_bytes);
        // Category count + categories
        body.extend_from_slice(&encode_compressed_int(event_type.category.len() as u64));
        for cat in &event_type.category {
            let cat_bytes = cat.as_bytes();
            body.extend_from_slice(&encode_compressed_int(cat_bytes.len() as u64));
            body.extend_from_slice(cat_bytes);
        }
        // Description
        let desc_bytes = event_type.description.as_bytes();
        body.extend_from_slice(&encode_compressed_int(desc_bytes.len() as u64));
        body.extend_from_slice(desc_bytes);
        // Field count + fields
        body.extend_from_slice(&encode_compressed_int(event_type.fields.len() as u64));
        for field in &event_type.fields {
            let fname_bytes = field.name.as_bytes();
            body.extend_from_slice(&encode_compressed_int(fname_bytes.len() as u64));
            body.extend_from_slice(fname_bytes);
            let ftype_bytes = field.type_name.as_bytes();
            body.extend_from_slice(&encode_compressed_int(ftype_bytes.len() as u64));
            body.extend_from_slice(ftype_bytes);
            let fdesc_bytes = field.description.as_bytes();
            body.extend_from_slice(&encode_compressed_int(fdesc_bytes.len() as u64));
            body.extend_from_slice(fdesc_bytes);
        }
        // Flags: has_thread, has_stacktrace
        body.push(if event_type.has_thread { 1 } else { 0 });
        body.push(if event_type.has_stacktrace { 1 } else { 0 });
    }

    // Wrap in a size-prefixed record.  Because the size prefix is itself a
    // variable-length compressed int, we iterate until the prefix length is
    // stable (same pattern as `serialize_event`).
    let body_len = body.len();
    let mut size_prefix_len = 1usize;
    loop {
        let total = size_prefix_len + body_len;
        let actual_prefix_len = encode_compressed_int(total as u64).len();
        if actual_prefix_len == size_prefix_len {
            let mut record = Vec::with_capacity(total);
            record.extend_from_slice(&encode_compressed_int(total as u64));
            record.extend_from_slice(&body);
            return record;
        }
        size_prefix_len = actual_prefix_len;
    }
}

// ---------------------------------------------------------------------------
// Checkpoint section
// ---------------------------------------------------------------------------

/// Build a minimal checkpoint section.
/// In a full JFR implementation this contains constant pool entries (strings,
/// thread names, stack traces, etc.). We write a minimal checkpoint that
/// declares zero constant pools.
fn build_checkpoint_section(start_time_ns: u64) -> Vec<u8> {
    let mut body = Vec::new();

    // Checkpoint event type ID
    body.extend_from_slice(&encode_compressed_int(CHECKPOINT_TYPE_ID));
    // Timestamp
    body.extend_from_slice(&encode_compressed_long(start_time_ns as i64));
    // Duration = 0
    body.extend_from_slice(&encode_compressed_long(0));
    // Delta to next checkpoint: 0 (this is the only one)
    body.extend_from_slice(&encode_compressed_long(0));
    // Checkpoint type mask: 0 = flush
    body.extend_from_slice(&encode_compressed_int(0));
    // Number of constant pools: 0
    body.extend_from_slice(&encode_compressed_int(0));

    // Wrap in size-prefixed record.  Iterate until the size-prefix length is
    // stable (same pattern as `serialize_event`).
    let body_len = body.len();
    let mut size_prefix_len = 1usize;
    loop {
        let total = size_prefix_len + body_len;
        let actual_prefix_len = encode_compressed_int(total as u64).len();
        if actual_prefix_len == size_prefix_len {
            let mut record = Vec::with_capacity(total);
            record.extend_from_slice(&encode_compressed_int(total as u64));
            record.extend_from_slice(&body);
            return record;
        }
        size_prefix_len = actual_prefix_len;
    }
}

// ---------------------------------------------------------------------------
// File header
// ---------------------------------------------------------------------------

/// Write the 68-byte JFR file header.
fn write_header<W: Write>(
    writer: &mut W,
    file_size: u64,
    checkpoint_offset: u64,
    metadata_offset: u64,
    start_time_ns: u64,
    duration_ns: u64,
    file_state: u8,
) -> io::Result<()> {
    writer.write_all(&JFR_MAGIC)?;
    writer.write_all(&JFR_VERSION_MAJOR.to_be_bytes())?;
    writer.write_all(&JFR_VERSION_MINOR.to_be_bytes())?;
    writer.write_all(&file_size.to_be_bytes())?;
    writer.write_all(&checkpoint_offset.to_be_bytes())?;
    writer.write_all(&metadata_offset.to_be_bytes())?;
    writer.write_all(&start_time_ns.to_be_bytes())?;
    writer.write_all(&duration_ns.to_be_bytes())?;
    writer.write_all(&start_time_ns.to_be_bytes())?; // start_ticks == start_time for nanos
    writer.write_all(&TICKS_PER_SECOND.to_be_bytes())?;
    writer.write_all(&[file_state])?;
    writer.write_all(&[0u8; 7])?; // padding
    Ok(())
}

// ---------------------------------------------------------------------------
// Public dump API
// ---------------------------------------------------------------------------

/// Dump a recording's events to a JFR binary file.
///
/// The `repository` provides the buffered events and `registry` provides the
/// event type metadata.  `start_time_ns` and `duration_ns` are used for the
/// file header timestamps.
///
/// Returns the total number of bytes written.
pub fn dump_to_file(
    path: &Path,
    repository: &EventRepository,
    registry: &EventTypeRegistry,
    start_time_ns: u64,
    duration_ns: u64,
) -> Result<u64, JfrDumpError> {
    // Write to a sibling `<name>.jfr.part` file and atomically rename on
    // success.  If anything fails partway through, the prior `.jfr` file is
    // left intact and the `.part` scratch file is best-effort removed by the
    // RAII guard below.
    let part_path = path.with_extension("jfr.part");

    /// Best-effort cleanup of the in-progress `.part` file. `disarm()` is
    /// called once the rename succeeds; otherwise `drop` removes the file.
    struct PartGuard<'a> {
        path: &'a Path,
        armed: bool,
    }
    impl<'a> PartGuard<'a> {
        fn disarm(&mut self) {
            self.armed = false;
        }
    }
    impl<'a> Drop for PartGuard<'a> {
        fn drop(&mut self) {
            if self.armed {
                let _ = std::fs::remove_file(self.path);
            }
        }
    }
    let mut guard = PartGuard { path: &part_path, armed: true };

    let file_size = {
        let file = std::fs::File::create(&part_path)?;
        let mut writer = io::BufWriter::new(file);

        // Write placeholder header (will be updated at the end)
        write_header(&mut writer, 0, 0, 0, start_time_ns, duration_ns, FILE_STATE_WRITING)?;

        // Write events
        for event in repository.iter() {
            let record = serialize_event(
                event.type_id,
                event.start_time,
                event.end_time,
                event.thread_id,
                &event.fields,
            );
            writer.write_all(&record)?;
        }

        // Write checkpoint
        let checkpoint_offset = writer.seek(SeekFrom::Current(0))?;
        let checkpoint = build_checkpoint_section(start_time_ns);
        writer.write_all(&checkpoint)?;

        // Write metadata
        let metadata_offset = writer.seek(SeekFrom::Current(0))?;
        let metadata = build_metadata_section(registry, start_time_ns);
        writer.write_all(&metadata)?;

        // Compute final file size
        let file_size = writer.seek(SeekFrom::Current(0))?;

        // Rewrite header with correct offsets and file state
        writer.seek(SeekFrom::Start(0))?;
        write_header(
            &mut writer,
            file_size,
            checkpoint_offset,
            metadata_offset,
            start_time_ns,
            duration_ns,
            FILE_STATE_COMPLETE,
        )?;

        // Flush buffered writer into the OS file, then fsync to ensure the
        // bytes (including the rewritten header) are durable before rename.
        writer.flush()?;
        writer.get_ref().sync_all()?;
        file_size
    };

    // Atomically swap the completed `.part` file into the final path.
    std::fs::rename(&part_path, path)?;
    guard.disarm();
    Ok(file_size)
}

/// Read and validate the header from a JFR file. Returns the parsed header fields.
pub fn read_jfr_header(path: &Path) -> Result<JfrFileHeader, JfrDumpError> {
    let data = std::fs::read(path)?;
    if data.len() < HEADER_SIZE as usize {
        return Err(JfrDumpError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "file too small for JFR header",
        )));
    }
    let magic = [data[0], data[1], data[2], data[3]];
    if magic != JFR_MAGIC {
        return Err(JfrDumpError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid JFR magic bytes",
        )));
    }
    let major = u16::from_be_bytes([data[4], data[5]]);
    let minor = u16::from_be_bytes([data[6], data[7]]);
    let file_size = u64::from_be_bytes(data[8..16].try_into().unwrap());
    let checkpoint_offset = u64::from_be_bytes(data[16..24].try_into().unwrap());
    let metadata_offset = u64::from_be_bytes(data[24..32].try_into().unwrap());
    let start_time_ns = u64::from_be_bytes(data[32..40].try_into().unwrap());
    let duration_ns = u64::from_be_bytes(data[40..48].try_into().unwrap());
    let start_ticks = u64::from_be_bytes(data[48..56].try_into().unwrap());
    let ticks_per_second = u64::from_be_bytes(data[56..64].try_into().unwrap());
    let file_state = data[64];

    Ok(JfrFileHeader {
        magic,
        major,
        minor,
        file_size,
        checkpoint_offset,
        metadata_offset,
        start_time_ns,
        duration_ns,
        start_ticks,
        ticks_per_second,
        file_state,
    })
}

/// Parsed JFR file header.
#[derive(Debug, Clone)]
pub struct JfrFileHeader {
    pub magic: [u8; 4],
    pub major: u16,
    pub minor: u16,
    pub file_size: u64,
    pub checkpoint_offset: u64,
    pub metadata_offset: u64,
    pub start_time_ns: u64,
    pub duration_ns: u64,
    pub start_ticks: u64,
    pub ticks_per_second: u64,
    pub file_state: u8,
}

// ---------------------------------------------------------------------------
// Event reading (inverse of dump_to_file event section)
// ---------------------------------------------------------------------------

/// Decode a single `EventValue` from `data[pos..]` using the declared field
/// `type_name` ("int", "long", "float", "double", "boolean", "string").
///
/// Returns the decoded value plus the number of bytes consumed.
fn decode_event_value(
    data: &[u8],
    pos: usize,
    type_name: &str,
) -> Result<(EventValue, usize), JfrDumpError> {
    match type_name {
        "int" => {
            let (v, c) = decode_compressed_long(&data[pos..]).ok_or_else(|| {
                JfrDumpError::Io(io::Error::new(io::ErrorKind::InvalidData, "int decode failed"))
            })?;
            Ok((EventValue::Int(v as i32), c))
        }
        "long" => {
            let (v, c) = decode_compressed_long(&data[pos..]).ok_or_else(|| {
                JfrDumpError::Io(io::Error::new(io::ErrorKind::InvalidData, "long decode failed"))
            })?;
            Ok((EventValue::Long(v), c))
        }
        "float" => {
            if pos + 4 > data.len() {
                return Err(JfrDumpError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "float truncated",
                )));
            }
            let bits = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap());
            Ok((EventValue::Float(f32::from_bits(bits)), 4))
        }
        "double" => {
            if pos + 8 > data.len() {
                return Err(JfrDumpError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "double truncated",
                )));
            }
            let bits = u64::from_be_bytes(data[pos..pos + 8].try_into().unwrap());
            Ok((EventValue::Double(f64::from_bits(bits)), 8))
        }
        "boolean" => {
            if pos >= data.len() {
                return Err(JfrDumpError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "boolean truncated",
                )));
            }
            Ok((EventValue::Boolean(data[pos] != 0), 1))
        }
        "string" => {
            if pos >= data.len() {
                return Err(JfrDumpError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "string tag truncated",
                )));
            }
            let tag = data[pos];
            match tag {
                0 => Ok((EventValue::Null, 1)),
                3 => {
                    let (len, lc) = decode_compressed_int(&data[pos + 1..]).ok_or_else(|| {
                        JfrDumpError::Io(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "string length decode failed",
                        ))
                    })?;
                    let start = pos + 1 + lc;
                    let end = start + len as usize;
                    if end > data.len() {
                        return Err(JfrDumpError::Io(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "string body truncated",
                        )));
                    }
                    let s = std::str::from_utf8(&data[start..end]).map_err(|e| {
                        JfrDumpError::Io(io::Error::new(io::ErrorKind::InvalidData, e))
                    })?;
                    Ok((EventValue::String(std::sync::Arc::from(s)), 1 + lc + len as usize))
                }
                _ => Err(JfrDumpError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown string encoding tag {}", tag),
                ))),
            }
        }
        other => Err(JfrDumpError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported field type '{}'", other),
        ))),
    }
}

/// Read every event record from a previously-written JFR file. The provided
/// `registry` must describe each `type_id` found in the file; if it does not,
/// the first unknown event aborts the read.
///
/// Returns the list of decoded events in file order. This is the inverse of
/// [`dump_to_file`]'s event section and is used by
/// [`crate::stream::EventStream::open_repository`] to iterate a written
/// recording (`jdk.jfr.consumer.EventStream.openRepository`).
pub fn read_events(
    path: &Path,
    registry: &EventTypeRegistry,
) -> Result<Vec<EventInstance>, JfrDumpError> {
    let data = std::fs::read(path)?;
    if data.len() < HEADER_SIZE as usize {
        return Err(JfrDumpError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "file too small for JFR header",
        )));
    }
    let magic = [data[0], data[1], data[2], data[3]];
    if magic != JFR_MAGIC {
        return Err(JfrDumpError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid JFR magic bytes",
        )));
    }
    let checkpoint_offset =
        u64::from_be_bytes(data[16..24].try_into().unwrap()) as usize;

    // Walk records between header end and the checkpoint section. Each record
    // starts with a compressed_int size field that includes the size byte(s).
    let mut pos = HEADER_SIZE as usize;
    let mut out = Vec::new();
    while pos < checkpoint_offset {
        let (total_size, size_len) = decode_compressed_int(&data[pos..]).ok_or_else(|| {
            JfrDumpError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "record size decode failed",
            ))
        })?;
        let record_end = pos + total_size as usize;
        if record_end > checkpoint_offset {
            return Err(JfrDumpError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "record extends past checkpoint",
            )));
        }

        let mut rpos = pos + size_len;
        let (type_id_raw, tc) = decode_compressed_int(&data[rpos..]).ok_or_else(|| {
            JfrDumpError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "type_id decode failed",
            ))
        })?;
        rpos += tc;
        let type_id = EventTypeId(type_id_raw as u32);

        let (start_time, sc) = decode_compressed_long(&data[rpos..]).ok_or_else(|| {
            JfrDumpError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "start_time decode failed",
            ))
        })?;
        rpos += sc;

        let (duration, dc) = decode_compressed_long(&data[rpos..]).ok_or_else(|| {
            JfrDumpError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "duration decode failed",
            ))
        })?;
        rpos += dc;

        let (thread_id, tdc) = decode_compressed_long(&data[rpos..]).ok_or_else(|| {
            JfrDumpError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "thread_id decode failed",
            ))
        })?;
        rpos += tdc;

        let ty = registry.get(type_id).ok_or_else(|| {
            JfrDumpError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown event type id {}", type_id.0),
            ))
        })?;

        let mut fields = Vec::with_capacity(ty.fields.len());
        for field in &ty.fields {
            let (v, c) = decode_event_value(&data, rpos, &field.type_name)?;
            fields.push(v);
            rpos += c;
        }

        // rpos should now equal record_end; tolerate trailing padding just in case.
        out.push(EventInstance {
            type_id,
            start_time: start_time as u64,
            end_time: (start_time as u64).saturating_add(duration as u64),
            thread_id: thread_id as u64,
            fields,
        });

        pos = record_end;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventField, EventInstance, EventPeriod, EventType, EventTypeId, EventValue};
    use std::sync::Arc;

    // --- LEB128 encoding/decoding ---

    #[test]
    fn test_compressed_int_zero() {
        let enc = encode_compressed_int(0);
        assert_eq!(enc, vec![0]);
        let (val, consumed) = decode_compressed_int(&enc).unwrap();
        assert_eq!(val, 0);
        assert_eq!(consumed, 1);
    }

    #[test]
    fn test_compressed_int_small() {
        let enc = encode_compressed_int(42);
        assert_eq!(enc, vec![42]);
        let (val, consumed) = decode_compressed_int(&enc).unwrap();
        assert_eq!(val, 42);
        assert_eq!(consumed, 1);
    }

    #[test]
    fn test_compressed_int_127() {
        let enc = encode_compressed_int(127);
        assert_eq!(enc, vec![127]);
        let (val, consumed) = decode_compressed_int(&enc).unwrap();
        assert_eq!(val, 127);
        assert_eq!(consumed, 1);
    }

    #[test]
    fn test_compressed_int_128() {
        let enc = encode_compressed_int(128);
        assert_eq!(enc, vec![0x80, 0x01]);
        let (val, consumed) = decode_compressed_int(&enc).unwrap();
        assert_eq!(val, 128);
        assert_eq!(consumed, 2);
    }

    #[test]
    fn test_compressed_int_300() {
        let enc = encode_compressed_int(300);
        let (val, consumed) = decode_compressed_int(&enc).unwrap();
        assert_eq!(val, 300);
        assert_eq!(consumed, 2);
    }

    #[test]
    fn test_compressed_int_large() {
        let val = 1_000_000_000u64;
        let enc = encode_compressed_int(val);
        let (decoded, _) = decode_compressed_int(&enc).unwrap();
        assert_eq!(decoded, val);
    }

    #[test]
    fn test_compressed_int_max() {
        let val = u64::MAX;
        let enc = encode_compressed_int(val);
        let (decoded, _) = decode_compressed_int(&enc).unwrap();
        assert_eq!(decoded, val);
    }

    #[test]
    fn test_compressed_long_positive() {
        let enc = encode_compressed_long(42);
        let (val, _) = decode_compressed_long(&enc).unwrap();
        assert_eq!(val, 42);
    }

    #[test]
    fn test_compressed_long_negative() {
        let enc = encode_compressed_long(-1);
        let (val, _) = decode_compressed_long(&enc).unwrap();
        assert_eq!(val, -1);
    }

    #[test]
    fn test_compressed_long_zero() {
        let enc = encode_compressed_long(0);
        let (val, consumed) = decode_compressed_long(&enc).unwrap();
        assert_eq!(val, 0);
        assert_eq!(consumed, 1);
    }

    #[test]
    fn test_compressed_long_min() {
        let enc = encode_compressed_long(i64::MIN);
        let (val, _) = decode_compressed_long(&enc).unwrap();
        assert_eq!(val, i64::MIN);
    }

    #[test]
    fn test_compressed_long_max() {
        let enc = encode_compressed_long(i64::MAX);
        let (val, _) = decode_compressed_long(&enc).unwrap();
        assert_eq!(val, i64::MAX);
    }

    #[test]
    fn test_decode_compressed_int_empty() {
        assert!(decode_compressed_int(&[]).is_none());
    }

    #[test]
    fn test_decode_compressed_int_truncated() {
        // 0x80 means "more bytes follow" but there are none
        assert!(decode_compressed_int(&[0x80]).is_none());
    }

    // --- dump_to_file and header reading ---

    fn make_registry_with_one_type() -> (EventTypeRegistry, EventTypeId) {
        let mut reg = EventTypeRegistry::new();
        let id = reg.register(EventType {
            id: EventTypeId(0),
            name: "test.Event".into(),
            category: vec!["Test".into()],
            description: "A test event".into(),
            fields: vec![
                EventField::new("count", "int", "Count"),
                EventField::new("name", "string", "Name"),
            ],
            has_thread: true,
            has_stacktrace: false,
            period: EventPeriod::None,
            threshold: None,
        });
        (reg, id)
    }

    #[test]
    fn test_dump_creates_valid_jfr_file() {
        let (reg, type_id) = make_registry_with_one_type();
        let mut repo = EventRepository::new(100);
        repo.push(EventInstance {
            type_id,
            start_time: 1_000_000,
            end_time: 2_000_000,
            thread_id: 1,
            fields: vec![
                EventValue::Int(42),
                EventValue::String(Arc::from("hello")),
            ],
        });
        repo.push(EventInstance {
            type_id,
            start_time: 3_000_000,
            end_time: 4_000_000,
            thread_id: 2,
            fields: vec![
                EventValue::Int(99),
                EventValue::String(Arc::from("world")),
            ],
        });

        let dir = std::env::temp_dir().join("jfr_test_dump");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_dump.jfr");

        let file_size = dump_to_file(&path, &repo, &reg, 1_000_000, 3_000_000).unwrap();
        assert!(file_size >= HEADER_SIZE);

        // Verify header
        let header = read_jfr_header(&path).unwrap();
        assert_eq!(header.magic, JFR_MAGIC);
        assert_eq!(header.major, JFR_VERSION_MAJOR);
        assert_eq!(header.minor, JFR_VERSION_MINOR);
        assert_eq!(header.file_size, file_size);
        assert_eq!(header.file_state, FILE_STATE_COMPLETE);
        assert_eq!(header.ticks_per_second, TICKS_PER_SECOND);
        assert_eq!(header.start_time_ns, 1_000_000);
        assert_eq!(header.duration_ns, 3_000_000);

        // Checkpoint is after header + events but before metadata
        assert!(header.checkpoint_offset >= HEADER_SIZE);
        assert!(header.metadata_offset > header.checkpoint_offset);
        assert!(header.metadata_offset < file_size);

        // Clean up
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_dump_empty_events() {
        let (reg, _type_id) = make_registry_with_one_type();
        let repo = EventRepository::new(100);

        let dir = std::env::temp_dir().join("jfr_test_empty");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_empty.jfr");

        // Empty dump is still valid (just header + checkpoint + metadata)
        let file_size = dump_to_file(&path, &repo, &reg, 0, 0).unwrap();
        let header = read_jfr_header(&path).unwrap();
        assert_eq!(header.magic, JFR_MAGIC);
        assert_eq!(header.file_state, FILE_STATE_COMPLETE);
        assert_eq!(header.file_size, file_size);
        // Checkpoint immediately after header when no events
        assert_eq!(header.checkpoint_offset, HEADER_SIZE);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_dump_roundtrip_magic_bytes() {
        let (reg, type_id) = make_registry_with_one_type();
        let mut repo = EventRepository::new(10);
        repo.push(EventInstance {
            type_id,
            start_time: 100,
            end_time: 200,
            thread_id: 1,
            fields: vec![EventValue::Int(1), EventValue::from_str("x")],
        });

        let dir = std::env::temp_dir().join("jfr_test_magic");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_magic.jfr");

        dump_to_file(&path, &repo, &reg, 100, 100).unwrap();

        // Read raw bytes and verify magic
        let data = std::fs::read(&path).unwrap();
        assert_eq!(&data[0..4], &JFR_MAGIC);
        assert_eq!(u16::from_be_bytes([data[4], data[5]]), JFR_VERSION_MAJOR);
        assert_eq!(u16::from_be_bytes([data[6], data[7]]), JFR_VERSION_MINOR);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_dump_with_various_field_types() {
        let mut reg = EventTypeRegistry::new();
        let type_id = reg.register(EventType {
            id: EventTypeId(0),
            name: "test.AllFields".into(),
            category: vec!["Test".into()],
            description: "Event with all field types".into(),
            fields: vec![
                EventField::new("i", "int", ""),
                EventField::new("l", "long", ""),
                EventField::new("f", "float", ""),
                EventField::new("d", "double", ""),
                EventField::new("b", "boolean", ""),
                EventField::new("s", "string", ""),
            ],
            has_thread: false,
            has_stacktrace: false,
            period: EventPeriod::None,
            threshold: None,
        });

        let mut repo = EventRepository::new(10);
        repo.push(EventInstance {
            type_id,
            start_time: 500,
            end_time: 600,
            thread_id: 0,
            fields: vec![
                EventValue::Int(-7),
                EventValue::Long(i64::MAX),
                EventValue::Float(3.14),
                EventValue::Double(2.718281828),
                EventValue::Boolean(true),
                EventValue::String(Arc::from("test string")),
            ],
        });
        repo.push(EventInstance {
            type_id,
            start_time: 700,
            end_time: 800,
            thread_id: 0,
            fields: vec![
                EventValue::Int(0),
                EventValue::Long(0),
                EventValue::Float(0.0),
                EventValue::Double(0.0),
                EventValue::Boolean(false),
                EventValue::Null,
            ],
        });

        let dir = std::env::temp_dir().join("jfr_test_fields");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_fields.jfr");

        let file_size = dump_to_file(&path, &repo, &reg, 500, 300).unwrap();
        let header = read_jfr_header(&path).unwrap();
        assert_eq!(header.file_state, FILE_STATE_COMPLETE);
        assert_eq!(header.file_size, file_size);
        assert!(header.checkpoint_offset > HEADER_SIZE);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_dump_with_many_events() {
        let (reg, type_id) = make_registry_with_one_type();
        let mut repo = EventRepository::new(1000);
        for i in 0..500u64 {
            repo.push(EventInstance {
                type_id,
                start_time: i * 1000,
                end_time: i * 1000 + 500,
                thread_id: i % 4,
                fields: vec![
                    EventValue::Int(i as i32),
                    EventValue::from_str(&format!("event_{}", i)),
                ],
            });
        }

        let dir = std::env::temp_dir().join("jfr_test_many");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_many.jfr");

        let file_size = dump_to_file(&path, &repo, &reg, 0, 500_000).unwrap();
        let header = read_jfr_header(&path).unwrap();
        assert_eq!(header.file_state, FILE_STATE_COMPLETE);
        assert_eq!(header.file_size, file_size);
        // The file should be substantially larger than just the header
        assert!(file_size > HEADER_SIZE + 500);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_dump_event_data_between_header_and_checkpoint() {
        let (reg, type_id) = make_registry_with_one_type();
        let mut repo = EventRepository::new(10);
        repo.push(EventInstance {
            type_id,
            start_time: 100,
            end_time: 200,
            thread_id: 1,
            fields: vec![EventValue::Int(1), EventValue::from_str("a")],
        });

        let dir = std::env::temp_dir().join("jfr_test_layout");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_layout.jfr");

        dump_to_file(&path, &repo, &reg, 100, 100).unwrap();
        let header = read_jfr_header(&path).unwrap();

        // Events live between HEADER_SIZE and checkpoint_offset
        let event_region_size = header.checkpoint_offset - HEADER_SIZE;
        assert!(event_region_size > 0, "event region should be non-empty");

        // Read the event region
        let data = std::fs::read(&path).unwrap();
        let event_data = &data[HEADER_SIZE as usize..header.checkpoint_offset as usize];

        // First byte(s) of the event region should decode as a valid compressed int (size)
        let (size, consumed) = decode_compressed_int(event_data).unwrap();
        assert!(size > 0);
        assert!(consumed > 0);

        // The event type ID should follow the size
        let (decoded_type_id, _) = decode_compressed_int(&event_data[consumed..]).unwrap();
        assert_eq!(decoded_type_id, type_id.0 as u64);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_read_jfr_header_invalid_magic() {
        let dir = std::env::temp_dir().join("jfr_test_bad_magic");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("bad_magic.jfr");

        std::fs::write(&path, &[0u8; 68]).unwrap();
        let result = read_jfr_header(&path);
        assert!(result.is_err());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_read_jfr_header_file_too_small() {
        let dir = std::env::temp_dir().join("jfr_test_small");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("small.jfr");

        std::fs::write(&path, &[0u8; 10]).unwrap();
        let result = read_jfr_header(&path);
        assert!(result.is_err());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_serialize_event_basic() {
        let record = serialize_event(
            EventTypeId(5),
            1000,
            2000,
            1,
            &[EventValue::Int(42)],
        );
        // Should be non-empty and start with a size prefix
        assert!(!record.is_empty());
        let (size, consumed) = decode_compressed_int(&record).unwrap();
        assert_eq!(size, record.len() as u64);
        // After size, the type ID should be 5
        let (type_id, _) = decode_compressed_int(&record[consumed..]).unwrap();
        assert_eq!(type_id, 5);
    }

    #[test]
    fn test_dump_file_complete_lifecycle() {
        // Full lifecycle: create FlightRecorder, record events, dump, verify
        let mut fr = crate::create_flight_recorder();
        let rid = fr.new_recording(crate::recording::RecordingSettings::new("dump-test"));
        fr.start_recording(rid);

        crate::builtin::emit_gc_event(&mut fr, 1, "G1 Young", "Allocation Failure", 1_000_000, 500_000);
        crate::builtin::emit_thread_start_event(&mut fr, "main", "", 1, 2_000_000);
        crate::builtin::emit_class_load_event(&mut fr, "java/lang/Object", "bootstrap", "bootstrap", 3_000_000, 100_000);

        fr.stop_recording(rid);

        let rec = fr.get_recording_mut(rid).unwrap();
        assert_eq!(rec.event_count(), 3);

        let dir = std::env::temp_dir().join("jfr_test_lifecycle");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("lifecycle.jfr");

        // Get the events and dump them
        let events = rec.get_events();
        let start_time = events.iter().map(|e| e.start_time).min().unwrap_or(0);
        let end_time = events.iter().map(|e| e.end_time).max().unwrap_or(0);
        let duration = end_time.saturating_sub(start_time);

        // Create a temporary repo to hold the events for dumping
        let mut dump_repo = EventRepository::new(1000);
        for event in events {
            dump_repo.push(event.clone());
        }

        let file_size = dump_to_file(&path, &dump_repo, &fr.type_registry, start_time, duration).unwrap();
        let header = read_jfr_header(&path).unwrap();

        assert_eq!(header.magic, JFR_MAGIC);
        assert_eq!(header.major, 2);
        assert_eq!(header.minor, 0);
        assert_eq!(header.file_state, FILE_STATE_COMPLETE);
        assert_eq!(header.file_size, file_size);
        assert_eq!(header.ticks_per_second, TICKS_PER_SECOND);
        assert!(header.checkpoint_offset > HEADER_SIZE);
        assert!(header.metadata_offset > header.checkpoint_offset);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
