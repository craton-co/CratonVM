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
use std::sync::Arc;

use rustc_hash::FxHashMap;

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
///
/// Allocates a new `Vec<u8>`. Hot paths should prefer
/// [`write_compressed_int_into`] which appends into a caller-provided buffer.
pub fn encode_compressed_int(value: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(10);
    write_compressed_int_into(&mut buf, value);
    buf
}

/// Encode a signed i64 as a JFR compressed long (zigzag + LEB128).
///
/// Allocates a new `Vec<u8>`. Hot paths should prefer
/// [`write_compressed_long_into`] which appends into a caller-provided buffer.
pub fn encode_compressed_long(value: i64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(10);
    write_compressed_long_into(&mut buf, value);
    buf
}

/// J2 (round-2): append a JFR compressed-int encoding of `value` to `buf`
/// without allocating. Saves the per-call `Vec` allocation that
/// [`encode_compressed_int`] incurs.
#[inline]
pub fn write_compressed_int_into(buf: &mut Vec<u8>, value: u64) {
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
}

/// J2 (round-2): append a JFR compressed-long (zigzag + LEB128) encoding of
/// `value` to `buf` without allocating.
#[inline]
pub fn write_compressed_long_into(buf: &mut Vec<u8>, value: i64) {
    let zigzag = ((value << 1) ^ (value >> 63)) as u64;
    write_compressed_int_into(buf, zigzag);
}

/// Length of the compressed-int encoding of `value`, in bytes.
/// Used to size the `size` prefix of size-prefixed records before encoding.
#[inline]
fn compressed_int_len(value: u64) -> usize {
    let mut v = value;
    let mut n = 1usize;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
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

/// J1 (round-2): a string constant-pool used to dedupe repeated string field
/// payloads in an event chunk. Built once per dump from the events being
/// emitted; each unique `Arc<str>` / `&'static str` payload is assigned a
/// monotonically-increasing pool index. Event records then write the index
/// (a compressed-int, typically 1-2 bytes) instead of the full UTF-8 body,
/// and the checkpoint section emits the pool table once.
///
/// Lookups are by byte-slice value so the `String` and `Str` variants share
/// one pool entry when they carry the same payload.
///
/// Round-4 (2026-05-17): storage is now `Arc<[u8]>` instead of `Vec<u8>` —
/// one `Box`-style heap allocation per unique entry plus an inline 16-byte
/// shared `Arc` header, instead of the previous two `Vec<u8>` clones (one
/// for the hashmap key, one for the order vector). The `Arc` makes both the
/// map key and the order vector cheap to share without re-copying the bytes.
struct StringPool {
    /// Index lookup, keyed by interned byte content (Arc-shared with `order`).
    by_bytes: FxHashMap<Arc<[u8]>, u32>,
    /// Insertion-ordered entries — emitted to the checkpoint section.
    order: Vec<Arc<[u8]>>,
}

impl StringPool {
    fn new() -> Self {
        Self { by_bytes: FxHashMap::default(), order: Vec::new() }
    }

    /// Look up or insert `bytes`, returning its pool index.
    ///
    /// Round-4 fix: previously allocated `bytes.to_vec()` plus a second
    /// `.clone()` on every unique insert (two `Vec<u8>` allocations per
    /// unique string). Now we allocate `Arc<[u8]>` once and stash a single
    /// shared handle in both the index map and the order vector.
    fn intern(&mut self, bytes: &[u8]) -> u32 {
        if let Some(&idx) = self.by_bytes.get(bytes) {
            return idx;
        }
        let idx = self.order.len() as u32;
        let shared: Arc<[u8]> = Arc::from(bytes);
        self.by_bytes.insert(Arc::clone(&shared), idx);
        self.order.push(shared);
        idx
    }

    /// Look up `bytes`, returning the pool index if known. Used on the write
    /// path after the pool has been pre-populated by walking every event.
    fn get(&self, bytes: &[u8]) -> Option<u32> {
        self.by_bytes.get(bytes).copied()
    }

    fn len(&self) -> usize {
        self.order.len()
    }

    fn entries(&self) -> &[Arc<[u8]>] {
        &self.order
    }
}

/// Intern every string-typed payload in `fields` into `pool`. Used by
/// `dump_to_file` as the per-chunk pre-pass before serialization.
fn intern_event_strings(pool: &mut StringPool, fields: &[EventValue]) {
    for f in fields {
        if let Some(bytes) = f.as_str_bytes() {
            pool.intern(bytes);
        }
    }
}

/// Encode a single event field value into a byte buffer.
///
/// When a `StringPool` is provided, `String`/`Str` payloads are written as a
/// 1-byte string-encoding tag of `4` (pool index reference) followed by a
/// compressed-int pool index, instead of the inline UTF-8 form. Unknown
/// strings (shouldn't happen — pool is pre-populated) fall back to the
/// inline tag-3 form.
fn encode_event_value(value: &EventValue, buf: &mut Vec<u8>, pool: Option<&StringPool>) {
    match value {
        EventValue::Long(v) => write_compressed_long_into(buf, *v),
        EventValue::Int(v) => write_compressed_long_into(buf, *v as i64),
        EventValue::Float(v) => buf.extend_from_slice(&v.to_bits().to_be_bytes()),
        EventValue::Double(v) => buf.extend_from_slice(&v.to_bits().to_be_bytes()),
        EventValue::Boolean(v) => buf.push(if *v { 1 } else { 0 }),
        EventValue::String(s) => write_string_bytes(buf, s.as_bytes(), pool),
        EventValue::Str(s) => write_string_bytes(buf, s.as_bytes(), pool),
        EventValue::Null => {
            // JFR null string: encoding type 0
            buf.push(0);
        }
    }
}

/// Emit a string-typed field. With a pool, writes tag `4` + interned index;
/// without (or on miss), writes inline tag `3` + length-prefixed UTF-8.
#[inline]
fn write_string_bytes(buf: &mut Vec<u8>, bytes: &[u8], pool: Option<&StringPool>) {
    if let Some(p) = pool {
        if let Some(idx) = p.get(bytes) {
            // STRING_ENCODING_CONSTANT_POOL_REF = 4
            buf.push(4);
            write_compressed_int_into(buf, idx as u64);
            return;
        }
    }
    // Inline UTF-8 fallback: encoding type 3
    buf.push(3);
    write_compressed_int_into(buf, bytes.len() as u64);
    buf.extend_from_slice(bytes);
}

/// J2 (round-2): serialize one event into `scratch` (cleared first), then
/// prefix-and-flush to `writer` — no per-event Vec allocations on the hot path.
///
/// Layout: `[size: compressed_int] [type_id] [start_ticks] [duration_ticks]
///          [thread_id] [fields...]`
///
/// `scratch` only needs to be sized once at the call site (`Vec::with_capacity`
/// for a typical event payload); the function reuses its existing allocation
/// across every call.
fn serialize_event_into<W: Write>(
    scratch: &mut Vec<u8>,
    writer: &mut W,
    type_id: EventTypeId,
    start_time: u64,
    end_time: u64,
    thread_id: u64,
    fields: &[EventValue],
    pool: Option<&StringPool>,
) -> io::Result<()> {
    scratch.clear();
    write_compressed_int_into(scratch, type_id.0 as u64);
    write_compressed_long_into(scratch, start_time as i64);
    let duration = end_time.saturating_sub(start_time);
    write_compressed_long_into(scratch, duration as i64);
    write_compressed_long_into(scratch, thread_id as i64);
    for field in fields {
        encode_event_value(field, scratch, pool);
    }
    write_size_prefixed(writer, scratch)
}

/// Compute the size-prefix length that yields a stable total, then write
/// the prefix followed by the body.
///
/// JFR records are size-prefixed using a variable-length compressed-int that
/// *includes the size field itself* — so picking the prefix length requires
/// a fixpoint search. In practice this converges in at most one iteration
/// because the prefix is 1-2 bytes for bodies under ~16 KiB.
fn write_size_prefixed<W: Write>(writer: &mut W, body: &[u8]) -> io::Result<()> {
    let body_len = body.len();
    let mut size_prefix_len = 1usize;
    let total = loop {
        let total = size_prefix_len + body_len;
        let actual = compressed_int_len(total as u64);
        if actual == size_prefix_len {
            break total;
        }
        size_prefix_len = actual;
    };
    // Write the size prefix straight to the writer — small (<= 10 bytes) so
    // a stack array is fine and avoids touching the heap.
    let mut prefix = [0u8; 10];
    let mut len = 0usize;
    let mut v = total as u64;
    loop {
        let mut byte = (v & 0x7F) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        prefix[len] = byte;
        len += 1;
        if v == 0 {
            break;
        }
    }
    writer.write_all(&prefix[..len])?;
    writer.write_all(body)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Metadata section
// ---------------------------------------------------------------------------

/// Type IDs used in the JFR metadata.
const METADATA_TYPE_ID: u64 = 0;
const CHECKPOINT_TYPE_ID: u64 = 1;

/// Write the metadata section describing all event types directly into `writer`.
fn write_metadata_section<W: Write>(
    writer: &mut W,
    registry: &EventTypeRegistry,
    start_time_ns: u64,
) -> io::Result<()> {
    // The metadata section is itself an event with type_id = METADATA_TYPE_ID.
    // It contains a description of all event types using a simplified encoding.
    //
    // Real JFR metadata uses a complex XML-like structure stored in binary.
    // We use a simplified but compatible format: a single metadata event containing
    // all type descriptors encoded as compressed fields.

    let mut body = Vec::with_capacity(1024);

    // Metadata event header
    write_compressed_int_into(&mut body, METADATA_TYPE_ID);
    write_compressed_long_into(&mut body, start_time_ns as i64);
    // Duration = 0 for metadata
    write_compressed_long_into(&mut body, 0);

    // Number of type descriptors
    let types: Vec<_> = registry.iter().collect();
    write_compressed_int_into(&mut body, types.len() as u64);

    for (id, event_type) in &types {
        // Type ID
        write_compressed_int_into(&mut body, id.0 as u64);
        // Name (length-prefixed UTF-8)
        let name_bytes = event_type.name.as_bytes();
        write_compressed_int_into(&mut body, name_bytes.len() as u64);
        body.extend_from_slice(name_bytes);
        // Category count + categories
        write_compressed_int_into(&mut body, event_type.category.len() as u64);
        for cat in &event_type.category {
            let cat_bytes = cat.as_bytes();
            write_compressed_int_into(&mut body, cat_bytes.len() as u64);
            body.extend_from_slice(cat_bytes);
        }
        // Description
        let desc_bytes = event_type.description.as_bytes();
        write_compressed_int_into(&mut body, desc_bytes.len() as u64);
        body.extend_from_slice(desc_bytes);
        // Field count + fields
        write_compressed_int_into(&mut body, event_type.fields.len() as u64);
        for field in &event_type.fields {
            let fname_bytes = field.name.as_bytes();
            write_compressed_int_into(&mut body, fname_bytes.len() as u64);
            body.extend_from_slice(fname_bytes);
            let ftype_bytes = field.type_name.as_bytes();
            write_compressed_int_into(&mut body, ftype_bytes.len() as u64);
            body.extend_from_slice(ftype_bytes);
            let fdesc_bytes = field.description.as_bytes();
            write_compressed_int_into(&mut body, fdesc_bytes.len() as u64);
            body.extend_from_slice(fdesc_bytes);
        }
        // Flags: has_thread, has_stacktrace
        body.push(if event_type.has_thread { 1 } else { 0 });
        body.push(if event_type.has_stacktrace { 1 } else { 0 });
    }

    write_size_prefixed(writer, &body)
}

// ---------------------------------------------------------------------------
// Checkpoint section
// ---------------------------------------------------------------------------

/// Tag identifying the string-pool constant-pool in the checkpoint section.
/// Picked to be distinct from any built-in JFR type ID we use elsewhere.
const STRING_POOL_TYPE_ID: u64 = 2;

/// Write a checkpoint section into `writer`. When the supplied string pool is
/// non-empty, the section declares a single constant pool of type
/// `STRING_POOL_TYPE_ID`, containing every interned string as
/// `(index, length-prefixed-utf8-bytes)`. Event records that reference these
/// strings emit tag `4` + a compressed-int index (typically 1-2 bytes) instead
/// of the full UTF-8 body — the J1 win.
///
/// On an empty pool we still emit a valid checkpoint declaring zero pools, so
/// readers that look at the count are happy.
fn write_checkpoint_section<W: Write>(
    writer: &mut W,
    start_time_ns: u64,
    pool: &StringPool,
) -> io::Result<()> {
    let mut body = Vec::with_capacity(64 + pool.len() * 16);

    // Checkpoint event type ID
    write_compressed_int_into(&mut body, CHECKPOINT_TYPE_ID);
    // Timestamp
    write_compressed_long_into(&mut body, start_time_ns as i64);
    // Duration = 0
    write_compressed_long_into(&mut body, 0);
    // Delta to next checkpoint: 0 (this is the only one)
    write_compressed_long_into(&mut body, 0);
    // Checkpoint type mask: 0 = flush
    write_compressed_int_into(&mut body, 0);

    if pool.len() == 0 {
        // No constant pools.
        write_compressed_int_into(&mut body, 0);
    } else {
        // One constant pool: the string interning table.
        write_compressed_int_into(&mut body, 1);
        // Pool type ID.
        write_compressed_int_into(&mut body, STRING_POOL_TYPE_ID);
        // Entry count.
        write_compressed_int_into(&mut body, pool.len() as u64);
        for (idx, bytes) in pool.entries().iter().enumerate() {
            // (idx, len-prefixed UTF-8 bytes). `bytes: &Arc<[u8]>` derefs to
            // `&[u8]` for both `.len()` and the slice borrow.
            let slice: &[u8] = bytes;
            write_compressed_int_into(&mut body, idx as u64);
            write_compressed_int_into(&mut body, slice.len() as u64);
            body.extend_from_slice(slice);
        }
    }

    write_size_prefixed(writer, &body)
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
/// CRIT-fix (Bug 1, 2026-05-17): this function no longer drains the process-
/// wide per-thread ring registry. With multiple concurrent recordings, the
/// first dump would otherwise steal events that belonged to siblings (the
/// global ring is shared). Each recording must drain its OWN snapshot of
/// events before calling this function and pass them in via `extra_events`
/// (the FlightRecorder's `drain_per_thread_into_repository` fans the drain
/// out to every running recording, so the per-recording repository ALREADY
/// contains the drained events and callers from `dump_recording` should pass
/// `Vec::new()`). For standalone/test dumps that want a one-shot drain,
/// callers can do `global_ring_registry().drain_all()` and forward the
/// result as `extra_events`.
///
/// `extra_events` are sorted by `start_time` and filtered to known event
/// types, then serialized AFTER the in-repository events.
///
/// Returns the total number of bytes written.
pub fn dump_to_file(
    path: &Path,
    repository: &EventRepository,
    registry: &EventTypeRegistry,
    start_time_ns: u64,
    duration_ns: u64,
    extra_events: Vec<EventInstance>,
) -> Result<u64, JfrDumpError> {
    // --- Pre-process caller-supplied extra events ----------------------------
    // Cold path: dump frequency is on the order of seconds (typically at
    // recording stop). We pay an O(n log n) sort and a single linear filter.
    // The events were already drained by the caller (typically
    // `FlightRecorder::dump_recording`), which fans them out to every running
    // recording. Re-draining here would steal events from sibling recordings —
    // see Bug 1.
    //
    // Round-4 (2026-05-17) — sort rationale:
    //   The JFR v2 file format does NOT require global timestamp ordering
    //   within a chunk: consumers reconstruct order from the per-event
    //   `start_time` field on read. The sort here is purely a courtesy to
    //   human readers / older JMC versions that don't re-sort. Because dumps
    //   are cold-path, we keep it — but note that skipping it would still
    //   produce a spec-compliant file and would save the O(n log n) cost on
    //   very large drained sets. (Per-thread blocks remain monotonic in this
    //   sort because `sort_by_key` is stable.)
    let mut drained: Vec<EventInstance> = extra_events;
    drained.sort_by_key(|e| e.start_time);
    // Filter to events whose type_id is registered. Unknown type_ids cannot be
    // round-tripped through `read_events` (which looks up the type to decode
    // fields), so writing them would produce unreadable records.
    drained.retain(|e| registry.get(e.type_id).is_some());

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

    // J1 (round-2): walk every event we're about to emit and intern its string
    // payloads into a per-chunk constant pool. Subsequent serialization writes
    // a compressed-int pool index per string instead of the full UTF-8 body,
    // turning N copies of "Allocation Failure" into a single pool entry + N
    // 1-byte tag-and-index references.
    let mut string_pool = StringPool::new();
    for ev in repository.iter() {
        intern_event_strings(&mut string_pool, &ev.fields);
    }
    for ev in &drained {
        intern_event_strings(&mut string_pool, &ev.fields);
    }

    let file_size = {
        let file = std::fs::File::create(&part_path)?;
        let mut writer = io::BufWriter::new(file);

        // Write placeholder header (will be updated at the end)
        write_header(&mut writer, 0, 0, 0, start_time_ns, duration_ns, FILE_STATE_WRITING)?;

        // J1: emit the checkpoint section (containing the string pool) BEFORE
        // the events so readers can resolve constant-pool indices during the
        // forward sweep. `checkpoint_offset` is recorded for the header.
        let checkpoint_offset = writer.seek(SeekFrom::Current(0))?;
        write_checkpoint_section(&mut writer, start_time_ns, &string_pool)?;

        // J2 (round-2): one reusable scratch buffer for every event body. Sized
        // for a typical event payload up front; serialize_event_into clears
        // and refills it per call without re-allocating.
        let mut scratch: Vec<u8> = Vec::with_capacity(256);
        let pool_ref = if string_pool.len() == 0 { None } else { Some(&string_pool) };

        // Write events from the recording's repository first, preserving the
        // existing on-disk ordering relative to drained events.
        for event in repository.iter() {
            serialize_event_into(
                &mut scratch,
                &mut writer,
                event.type_id,
                event.start_time,
                event.end_time,
                event.thread_id,
                &event.fields,
                pool_ref,
            )?;
        }

        // Then write the drained per-thread events (already sorted by
        // start_time, already filtered to known types).
        for event in &drained {
            serialize_event_into(
                &mut scratch,
                &mut writer,
                event.type_id,
                event.start_time,
                event.end_time,
                event.thread_id,
                &event.fields,
                pool_ref,
            )?;
        }

        // Write metadata
        let metadata_offset = writer.seek(SeekFrom::Current(0))?;
        write_metadata_section(&mut writer, registry, start_time_ns)?;

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
/// `pool` provides the decoded constant pool for tag-4 (pool-reference)
/// strings. When `None`, only inline (tag-3) and null (tag-0) strings are
/// accepted; this preserves compatibility for files written before J1.
///
/// Returns the decoded value plus the number of bytes consumed.
fn decode_event_value(
    data: &[u8],
    pos: usize,
    type_name: &str,
    pool: Option<&[std::sync::Arc<str>]>,
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
                4 => {
                    // Pool-reference: compressed-int index into the chunk pool.
                    let (idx, ic) = decode_compressed_int(&data[pos + 1..]).ok_or_else(|| {
                        JfrDumpError::Io(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "pool index decode failed",
                        ))
                    })?;
                    let table = pool.ok_or_else(|| {
                        JfrDumpError::Io(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "pool-ref string but no constant pool was loaded",
                        ))
                    })?;
                    let entry = table.get(idx as usize).ok_or_else(|| {
                        JfrDumpError::Io(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("pool index {} out of range (size {})", idx, table.len()),
                        ))
                    })?;
                    Ok((EventValue::String(std::sync::Arc::clone(entry)), 1 + ic))
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

/// Parse a JFR checkpoint section starting at `data[offset..]`. Returns the
/// decoded string-pool entries (if any) and the byte length of the section.
///
/// We only extract the one constant pool we emit (STRING_POOL_TYPE_ID); other
/// pool types are skipped over their declared length. On a malformed
/// checkpoint we degrade to an empty pool rather than aborting the read.
fn parse_checkpoint_pool(
    data: &[u8],
    offset: usize,
) -> Result<(Vec<std::sync::Arc<str>>, usize), JfrDumpError> {
    if offset >= data.len() {
        return Ok((Vec::new(), 0));
    }
    let (record_size, size_len) = decode_compressed_int(&data[offset..]).ok_or_else(|| {
        JfrDumpError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "checkpoint record size decode failed",
        ))
    })?;
    let record_end = offset + record_size as usize;
    if record_end > data.len() {
        return Err(JfrDumpError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "checkpoint record extends past EOF",
        )));
    }
    let mut pos = offset + size_len;

    // Skip: type_id, timestamp, duration, delta, type_mask.
    for _ in 0..5 {
        let (_, c) = decode_compressed_long(&data[pos..]).ok_or_else(|| {
            JfrDumpError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "checkpoint header field decode failed",
            ))
        })?;
        pos += c;
    }
    // Number of constant pools.
    let (n_pools, c) = decode_compressed_int(&data[pos..]).ok_or_else(|| {
        JfrDumpError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "constant pool count decode failed",
        ))
    })?;
    pos += c;

    let mut strings: Vec<std::sync::Arc<str>> = Vec::new();
    for _ in 0..n_pools {
        let (pool_type, ptc) = decode_compressed_int(&data[pos..]).ok_or_else(|| {
            JfrDumpError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "pool type decode failed",
            ))
        })?;
        pos += ptc;
        let (n_entries, nec) = decode_compressed_int(&data[pos..]).ok_or_else(|| {
            JfrDumpError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "pool entry count decode failed",
            ))
        })?;
        pos += nec;

        if pool_type == STRING_POOL_TYPE_ID {
            strings.reserve(n_entries as usize);
            for _ in 0..n_entries {
                let (_idx, ic) = decode_compressed_int(&data[pos..]).ok_or_else(|| {
                    JfrDumpError::Io(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "pool entry index decode failed",
                    ))
                })?;
                pos += ic;
                let (slen, lc) = decode_compressed_int(&data[pos..]).ok_or_else(|| {
                    JfrDumpError::Io(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "pool entry length decode failed",
                    ))
                })?;
                pos += lc;
                let end = pos + slen as usize;
                if end > record_end {
                    return Err(JfrDumpError::Io(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "pool entry body extends past record",
                    )));
                }
                let s = std::str::from_utf8(&data[pos..end]).map_err(|e| {
                    JfrDumpError::Io(io::Error::new(io::ErrorKind::InvalidData, e))
                })?;
                strings.push(std::sync::Arc::from(s));
                pos = end;
            }
        } else {
            // Unknown pool type — we have no length field to skip, so abort
            // rather than misalign. Should not happen for files we wrote.
            return Err(JfrDumpError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown checkpoint pool type {}", pool_type),
            )));
        }
    }

    Ok((strings, record_end - offset))
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
    let metadata_offset =
        u64::from_be_bytes(data[24..32].try_into().unwrap()) as usize;

    // J1 (round-2): the writer emits checkpoint BEFORE events, so we parse the
    // pool first, then walk events between (checkpoint_end .. metadata_offset).
    // Files written by older versions place the checkpoint AFTER events; we
    // detect that layout by comparing offsets and fall back to the legacy walk.
    let (pool, events_start, events_end) =
        if checkpoint_offset >= HEADER_SIZE as usize && checkpoint_offset < metadata_offset {
            // Determine layout: if checkpoint sits right after the header (new
            // layout), parse the pool and walk events after it. Otherwise the
            // checkpoint is after the events (legacy) — events live between
            // HEADER_SIZE and checkpoint_offset.
            if checkpoint_offset == HEADER_SIZE as usize {
                let (p, len) = parse_checkpoint_pool(&data, checkpoint_offset)?;
                (p, checkpoint_offset + len, metadata_offset)
            } else {
                (Vec::new(), HEADER_SIZE as usize, checkpoint_offset)
            }
        } else {
            (Vec::new(), HEADER_SIZE as usize, checkpoint_offset)
        };
    let pool_ref: Option<&[std::sync::Arc<str>]> =
        if pool.is_empty() { None } else { Some(&pool) };

    // Walk records within [events_start, events_end). Each record starts with
    // a compressed_int size field that includes the size byte(s).
    let mut pos = events_start;
    let mut out = Vec::new();
    while pos < events_end {
        let (total_size, size_len) = decode_compressed_int(&data[pos..]).ok_or_else(|| {
            JfrDumpError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "record size decode failed",
            ))
        })?;
        let record_end = pos + total_size as usize;
        if record_end > events_end {
            return Err(JfrDumpError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "record extends past events region",
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
            let (v, c) = decode_event_value(&data, rpos, &field.type_name, pool_ref)?;
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

        let file_size = dump_to_file(&path, &repo, &reg, 1_000_000, 3_000_000, Vec::new()).unwrap();
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
        // Drain any per-thread ring residue from earlier tests so this test's
        // "empty" invariant (checkpoint immediately after header) is not
        // perturbed by the dump-path drain we now perform. We cannot guarantee
        // no concurrent test pushes between the drain and the dump call (the
        // ring registry is process-wide), so the checkpoint_offset assertion
        // is intentionally `>=` rather than `==`.
        let _ = crate::repository::global_ring_registry().drain_all();

        let (reg, _type_id) = make_registry_with_one_type();
        let repo = EventRepository::new(100);

        let dir = std::env::temp_dir().join("jfr_test_empty");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_empty.jfr");

        // Empty dump is still valid (just header + checkpoint + metadata)
        let file_size = dump_to_file(&path, &repo, &reg, 0, 0, Vec::new()).unwrap();
        let header = read_jfr_header(&path).unwrap();
        assert_eq!(header.magic, JFR_MAGIC);
        assert_eq!(header.file_state, FILE_STATE_COMPLETE);
        assert_eq!(header.file_size, file_size);
        // Checkpoint at or after header (could be after if a concurrent test
        // pushed compatible-typed events into the global ring just before our
        // dump path drained).
        assert!(header.checkpoint_offset >= HEADER_SIZE);

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

        dump_to_file(&path, &repo, &reg, 100, 100, Vec::new()).unwrap();

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

        let file_size = dump_to_file(&path, &repo, &reg, 500, 300, Vec::new()).unwrap();
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

        let file_size = dump_to_file(&path, &repo, &reg, 0, 500_000, Vec::new()).unwrap();
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
        // J1 (round-2): events now live between the checkpoint section (at
        // HEADER_SIZE) and `metadata_offset`. The checkpoint sits right after
        // the header so readers can resolve constant-pool string references
        // during the forward sweep.
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

        dump_to_file(&path, &repo, &reg, 100, 100, Vec::new()).unwrap();
        let header = read_jfr_header(&path).unwrap();

        // The checkpoint should sit right at HEADER_SIZE.
        assert_eq!(header.checkpoint_offset, HEADER_SIZE);
        assert!(header.metadata_offset > header.checkpoint_offset);

        // The event region lives between (checkpoint_end .. metadata_offset).
        // We don't compute checkpoint_end here directly; instead just read the
        // events back via `read_events` and assert they roundtrip.
        let events = read_events(&path, &reg).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].type_id, type_id);
        assert_eq!(events[0].start_time, 100);
        match &events[0].fields[1] {
            EventValue::String(s) => assert_eq!(s.as_ref(), "a"),
            other => panic!("expected interned String, got {:?}", other),
        }

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
        let mut scratch = Vec::with_capacity(64);
        let mut record: Vec<u8> = Vec::new();
        serialize_event_into(
            &mut scratch,
            &mut record,
            EventTypeId(5),
            1000,
            2000,
            1,
            &[EventValue::Int(42)],
            None,
        )
        .unwrap();
        // Should be non-empty and start with a size prefix
        assert!(!record.is_empty());
        let (size, consumed) = decode_compressed_int(&record).unwrap();
        assert_eq!(size, record.len() as u64);
        // After size, the type ID should be 5
        let (type_id, _) = decode_compressed_int(&record[consumed..]).unwrap();
        assert_eq!(type_id, 5);
    }

    #[test]
    fn test_dump_drains_per_thread_rings() {
        // Bug 1 fix: `dump_to_file` no longer drains the global ring itself;
        // callers (FlightRecorder::dump_recording) drain and fan out per
        // recording, then pass the events in via `extra_events`. This test
        // simulates that contract: push events, drain explicitly, then verify
        // the writer emits them when handed in.
        //
        // The global ring registry is process-wide, so this test:
        //   1. Drains the registry first to start from a clean baseline.
        //   2. Registers a type in its own private registry.
        //   3. Pushes events with that type_id via `push_to_thread_ring`.
        //   4. Drains them explicitly and passes them to `dump_to_file`.
        //   5. Reads the file back and verifies the pushed events are present.
        use crate::repository::{global_ring_registry, push_to_thread_ring};

        // Baseline drain — discard anything left over from prior tests.
        let _baseline = global_ring_registry().drain_all();

        let (reg, type_id) = make_registry_with_one_type();
        let repo = EventRepository::new(100);

        // Push three events with distinctive start_times. They will go through
        // the calling thread's shard in the global registry.
        let pushed_starts: [u64; 3] = [
            0xA0A0_0000_0011,
            0xA0A0_0000_0022,
            0xA0A0_0000_0033,
        ];
        for &start in &pushed_starts {
            push_to_thread_ring(EventInstance {
                type_id,
                start_time: start,
                end_time: start + 100,
                thread_id: 7,
                fields: vec![
                    EventValue::Int(start as i32),
                    EventValue::from_str("from-ring"),
                ],
            });
        }

        let dir = std::env::temp_dir().join("jfr_test_drain_rings");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("drain_rings.jfr");

        // Drain the global ring and hand the events to the writer — this is
        // the contract that `FlightRecorder::dump_recording` now follows.
        let drained = global_ring_registry().drain_all();
        let _file_size = dump_to_file(&path, &repo, &reg, 0, 0, drained).unwrap();

        // Header should be valid
        let header = read_jfr_header(&path).unwrap();
        assert_eq!(header.file_state, FILE_STATE_COMPLETE);
        assert!(header.checkpoint_offset > HEADER_SIZE,
            "events region should be non-empty after draining rings");

        // Read events back and verify each pushed event is present.
        let events = read_events(&path, &reg).unwrap();
        for &expected_start in &pushed_starts {
            let found = events.iter().any(|e|
                e.type_id == type_id && e.start_time == expected_start);
            assert!(
                found,
                "expected drained event with start_time {:#x} in file, got {} events",
                expected_start,
                events.len(),
            );
        }

        // A second dump with no extra events should not re-emit them.
        let path2 = dir.join("drain_rings_second.jfr");
        let repo2 = EventRepository::new(100);
        dump_to_file(&path2, &repo2, &reg, 0, 0, Vec::new()).unwrap();
        let events2 = read_events(&path2, &reg).unwrap();
        for &expected_start in &pushed_starts {
            let still_there = events2.iter().any(|e|
                e.type_id == type_id && e.start_time == expected_start);
            assert!(
                !still_there,
                "drained events must not reappear in a second dump (start={:#x})",
                expected_start,
            );
        }

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&path2);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_dump_sorts_drained_events_by_start_time() {
        // Push events with out-of-order start_times into a single thread shard
        // (which preserves push order). Verify the file emits them sorted.
        use crate::repository::{global_ring_registry, push_to_thread_ring};

        // Baseline drain.
        let _ = global_ring_registry().drain_all();

        let (reg, type_id) = make_registry_with_one_type();
        let repo = EventRepository::new(100);

        // Push in non-monotonic order — unique tag prefix so we can filter
        // out any other events that happen to share type_id.
        let tag: u64 = 0xB0B0_0000_0000;
        let push_order: [u64; 4] = [tag | 30, tag | 10, tag | 40, tag | 20];
        for &start in &push_order {
            push_to_thread_ring(EventInstance {
                type_id,
                start_time: start,
                end_time: start + 1,
                thread_id: 0,
                fields: vec![EventValue::Int(0), EventValue::from_str("s")],
            });
        }

        let dir = std::env::temp_dir().join("jfr_test_sort_drained");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("sort_drained.jfr");
        // Drain the pushed events and pass them in — see Bug 1 fix.
        let drained = global_ring_registry().drain_all();
        dump_to_file(&path, &repo, &reg, 0, 0, drained).unwrap();

        let events = read_events(&path, &reg).unwrap();
        // Pull out just the ones we pushed (by tag prefix) and check they are
        // strictly increasing in start_time on disk.
        let ours: Vec<u64> = events
            .iter()
            .filter(|e| e.type_id == type_id && (e.start_time & 0xFFFF_FFFF_FFFF_FF00) == tag)
            .map(|e| e.start_time)
            .collect();
        assert_eq!(ours.len(), push_order.len(),
            "expected all {} pushed events back, got {}: {:?}",
            push_order.len(), ours.len(), ours);
        let sorted = {
            let mut v = ours.clone();
            v.sort();
            v
        };
        assert_eq!(ours, sorted, "drained events should be written in start_time order");
        // Make sure the test actually exercises sorting (i.e. the push order
        // was not already monotonically increasing).
        assert_ne!(push_order.to_vec(), sorted,
            "test setup bug: push_order happens to equal sorted order");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
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

        let file_size = dump_to_file(&path, &dump_repo, &fr.type_registry, start_time, duration, Vec::new()).unwrap();
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
