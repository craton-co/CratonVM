// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JFR binary file writer.
//!
//! Implements CratonVM's JFR-inspired chunk format.
//! The format consists of:
//!   1. A 72-byte file header
//!   2. Event records (variable-length, LEB128-encoded)
//!   3. A checkpoint section with constant pool entries
//!   4. A metadata section describing event types

use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use rustc_hash::FxHashMap;

use crate::event::{EventInstance, EventTypeId, EventTypeRegistry, EventValue, FieldKind};
use crate::repository::EventRepository;

/// JFR file magic bytes: `FLR\0`
pub const JFR_MAGIC: [u8; 4] = [b'F', b'L', b'R', 0];

/// JFR format major version
pub const JFR_VERSION_MAJOR: u16 = 2;
/// JFR format minor version.
///
/// `0`: events store absolute `start_time` (nanos since epoch) as a
///      compressed-long. The varint is typically 6-9 bytes since modern
///      epoch nanos are > 2^60.
/// `1`: round-5 Fix 3. Events store `start_time` as a delta from the
///      chunk's `start_time_ns` (taken from the header). Within a chunk
///      (typically < 1 second of wall time), deltas fit in 1-3 varint
///      bytes, shrinking total event-region size by ~30-40%. `end_time`
///      remains relative to `start_time` via the existing `duration`
///      field — no change there.
///
/// Readers detect the format from the header's `minor` field and add
/// `chunk_start_time` back when decoding `start_time`.
pub const JFR_VERSION_MINOR: u16 = 1;

/// Previous minor value that wrote absolute timestamps. Retained so the
/// reader can still parse files produced by pre-round-5 writers.
pub const JFR_VERSION_MINOR_ABSOLUTE_TS: u16 = 0;

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

/// Maximum size (in bytes) of a `.jfr` file that the readers will load into
/// memory. `read_events` / `read_jfr_header` slurp the whole file via
/// `std::fs::read`; without a cap, pointing them at an arbitrarily large
/// (possibly hostile) file would allocate unbounded memory and could OOM the
/// process. 2 GiB is far above any realistic single-chunk dump this writer
/// produces while still bounding the worst case. Oversized files are rejected
/// with an `InvalidData` error before any bytes are read.
pub const MAX_JFR_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

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

/// Shared core of the JFR compressed-int (LEB128-variant) encoder: writes the
/// encoding of `value` into the fixed-size stack buffer `out` and returns the
/// number of bytes written. A compressed u64 is at most 10 bytes, so a
/// `[u8; 10]` buffer can never overflow.
///
/// This is the single source of truth for the byte-level encoding; both
/// [`write_compressed_int_into`] (Vec-append) and [`write_size_prefixed`]
/// (stack-buffer-then-`write_all`) build on it so the encoding logic is not
/// duplicated.
#[inline]
fn encode_compressed_int_to_buf(out: &mut [u8; 10], value: u64) -> usize {
    let mut v = value;
    let mut len = 0usize;
    loop {
        let mut byte = (v & 0x7F) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out[len] = byte;
        len += 1;
        if v == 0 {
            break;
        }
    }
    len
}

/// J2 (round-2): append a JFR compressed-int encoding of `value` to `buf`
/// without allocating. Saves the per-call `Vec` allocation that
/// [`encode_compressed_int`] incurs.
#[inline]
pub fn write_compressed_int_into(buf: &mut Vec<u8>, value: u64) {
    let mut tmp = [0u8; 10];
    let len = encode_compressed_int_to_buf(&mut tmp, value);
    buf.extend_from_slice(&tmp[..len]);
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
        // Reject a varint that encodes more than 64 bits: once `shift` has
        // reached 64 there is no room left for any payload bits, so a
        // 10th continuation byte (or any byte at `shift >= 64`) is a
        // malformed encoding that would otherwise wrap/garble the u64.
        if shift >= 64 {
            return None; // overflow: varint encodes more than 64 bits
        }
        result |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((result, i + 1));
        }
        shift += 7;
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
        Self {
            by_bytes: FxHashMap::default(),
            order: Vec::new(),
        }
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

fn event_is_writable(event: &EventInstance, registry: &EventTypeRegistry) -> bool {
    let Some(event_type) = registry.get(event.type_id) else {
        tracing::debug!(
            type_id = event.type_id.0,
            "dropping JFR event with unknown type id before dump"
        );
        return false;
    };
    if event.end_time < event.start_time {
        tracing::debug!(
            type_id = event.type_id.0,
            start_time = event.start_time,
            end_time = event.end_time,
            "dropping JFR event whose end_time predates start_time"
        );
        return false;
    }
    if event.fields.len() != event_type.fields.len() {
        tracing::debug!(
            type_id = event.type_id.0,
            expected = event_type.fields.len(),
            actual = event.fields.len(),
            "dropping JFR event with field count mismatch before dump"
        );
        return false;
    }
    for (idx, (field, value)) in event_type
        .fields
        .iter()
        .zip(event.fields.iter())
        .enumerate()
    {
        let Some(declared) = FieldKind::from_declared(&field.type_name) else {
            tracing::debug!(
                type_id = event.type_id.0,
                field_index = idx,
                declared = %field.type_name,
                "dropping JFR event whose registered field type is unsupported"
            );
            return false;
        };
        let actual = FieldKind::of_value(value);
        if !actual.matches_declared(declared) {
            tracing::debug!(
                type_id = event.type_id.0,
                field_index = idx,
                declared = ?declared,
                actual = ?actual,
                "dropping JFR event with field shape mismatch before dump"
            );
            return false;
        }
    }
    true
}

/// Encode a single event field value into a byte buffer.
///
/// When a `StringPool` is provided, `String`/`Str` payloads are written as a
/// 1-byte string-encoding tag of `4` (pool index reference) followed by a
/// compressed-int pool index, instead of the inline UTF-8 form. Unknown
/// strings (shouldn't happen — pool is pre-populated) fall back to the
/// inline tag-3 form.
///
/// `declared` is the field's registry-declared [`FieldKind`] (or `None` when
/// the caller could not resolve it — e.g. a unit test that hands the writer a
/// type the registry never knew about). It is consulted *only* for the
/// [`EventValue::Null`] case, to make Null self-describing on read.
///
/// LOW fix (S?, 2026-06-17): previously `Null` always emitted a single `0x00`
/// byte regardless of the declared field kind. For a `string` field that is
/// the correct JFR null-string tag and the reader decodes it back to `Null`.
/// But for a fixed-width numeric/boolean field the reader (`decode_event_value`)
/// reads 4 (`float`), 8 (`double`) or 1 (`int`/`long`/`boolean`) bytes from the
/// declared kind — so a 1-byte Null *desynchronised the rest of the chunk*
/// (`float`/`double` read 3-7 bytes of the *next* field's payload). The
/// `int`/`long` case happened to consume exactly 1 byte (compressed-long `0`)
/// so it "worked" but only by accident.
///
/// Fix: for a Null in a numeric/boolean field, emit the kind-appropriate
/// fixed-width zero so the value round-trips *deterministically* to a typed
/// zero (`Int(0)`/`Long(0)`/`Float(0.0)`/`Double(0.0)`/`Boolean(false)`) and
/// the reader stays byte-aligned. Null in a `string` field (or when `declared`
/// is unknown) keeps the canonical 1-byte null-string tag, which the reader
/// already round-trips to [`EventValue::Null`].
fn encode_event_value(
    value: &EventValue,
    buf: &mut Vec<u8>,
    pool: Option<&StringPool>,
    declared: Option<FieldKind>,
) {
    match value {
        EventValue::Long(v) => write_compressed_long_into(buf, *v),
        EventValue::Int(v) => write_compressed_long_into(buf, *v as i64),
        EventValue::Float(v) => buf.extend_from_slice(&v.to_bits().to_be_bytes()),
        EventValue::Double(v) => buf.extend_from_slice(&v.to_bits().to_be_bytes()),
        EventValue::Boolean(v) => buf.push(if *v { 1 } else { 0 }),
        EventValue::String(s) => write_string_bytes(buf, s.as_bytes(), pool),
        EventValue::Str(s) => write_string_bytes(buf, s.as_bytes(), pool),
        EventValue::Null => {
            // Self-describing Null: emit the fixed-width zero that the reader
            // expects for this field's declared kind, so the decode stays
            // byte-aligned and the value round-trips deterministically.
            match declared {
                Some(FieldKind::Int) | Some(FieldKind::Long) => {
                    // compressed-long 0 (single 0x00 byte) → decodes to 0.
                    write_compressed_long_into(buf, 0);
                }
                Some(FieldKind::Float) => {
                    // 4 big-endian bytes of 0.0f.
                    buf.extend_from_slice(&0f32.to_bits().to_be_bytes());
                }
                Some(FieldKind::Double) => {
                    // 8 big-endian bytes of 0.0.
                    buf.extend_from_slice(&0f64.to_bits().to_be_bytes());
                }
                Some(FieldKind::Boolean) => {
                    buf.push(0);
                }
                // string field, the Null wildcard, or unknown declared kind:
                // canonical JFR null-string tag (encoding type 0). The reader
                // decodes this back to EventValue::Null for string fields.
                Some(FieldKind::String) | Some(FieldKind::Null) | None => {
                    buf.push(0);
                }
            }
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
/// Layout: `[size: compressed_int] [type_id] [start_delta] [duration_ticks]
///          [thread_id] [fields...]`
///
/// Round-5 Fix 3: `start_delta = start_time - chunk_start_time` so that
/// in-chunk timestamps (typically < 1 second apart) varint-encode to 1-3
/// bytes instead of 6-9 bytes for absolute epoch-nanos. `chunk_start_time`
/// comes from the header. Defensive saturating subtraction handles the
/// (theoretically impossible) case of an event timestamped before the
/// chunk start — it clamps to 0 rather than wrapping to a huge value.
///
/// `scratch` only needs to be sized once at the call site (`Vec::with_capacity`
/// for a typical event payload); the function reuses its existing allocation
/// across every call.
///
/// `field_kinds` is the per-field registry-declared [`FieldKind`] slice for
/// this event type (looked up once by the caller). It is consulted only to
/// make [`EventValue::Null`] self-describing on read (see `encode_event_value`).
/// `None`, or a slice shorter than `fields`, falls back to the canonical
/// 1-byte null tag for any unmapped Null — which keeps existing behaviour for
/// callers that cannot resolve the declared kinds (e.g. unit tests).
fn serialize_event_into<W: Write>(
    scratch: &mut Vec<u8>,
    writer: &mut W,
    type_id: EventTypeId,
    start_time: u64,
    end_time: u64,
    thread_id: u64,
    fields: &[EventValue],
    pool: Option<&StringPool>,
    chunk_start_time: u64,
    field_kinds: Option<&[FieldKind]>,
) -> io::Result<()> {
    scratch.clear();
    write_compressed_int_into(scratch, type_id.0 as u64);
    // Round-5 Fix 3: delta from chunk start. Saturating subtraction ensures
    // we never wrap into negative i64 territory if a stray event arrives
    // with a timestamp slightly before chunk_start_time (clock skew across
    // threads, monotonic-clock backsteps).
    let start_delta = start_time.saturating_sub(chunk_start_time);
    write_compressed_long_into(scratch, start_delta as i64);
    let duration = end_time.saturating_sub(start_time);
    write_compressed_long_into(scratch, duration as i64);
    write_compressed_long_into(scratch, thread_id as i64);
    for (i, field) in fields.iter().enumerate() {
        // Resolve the declared kind for field `i`; None when unavailable so
        // Null falls back to the canonical null tag.
        let declared = field_kinds.and_then(|k| k.get(i).copied());
        encode_event_value(field, scratch, pool, declared);
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
    // a stack array is fine and avoids touching the heap. Encoding goes
    // through the shared `encode_compressed_int_to_buf` helper so the
    // LEB128 byte logic is not duplicated against `write_compressed_int_into`.
    let mut prefix = [0u8; 10];
    let len = encode_compressed_int_to_buf(&mut prefix, total as u64);
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
///
/// ========================================================================
/// FORMAT-FIDELITY GAP (S3, 2026-06-10; re-confirmed 2026-06-17)
/// ========================================================================
/// **This is a SIMPLIFIED CUSTOM metadata encoding, NOT stock JFR binary
/// metadata.** Read this before assuming a produced `.jfr` is portable.
///
/// Stock JFR stores type/field descriptors as a binary-encoded,
/// constant-pool-referenced, XML-like element tree (`Metadata` event with a
/// nested `<class>/<field>/<setting>` structure keyed into the chunk's string
/// pool) that JDK Mission Control (JMC) and the `jfr` CLI parse to fully
/// describe every event type. What we emit here instead is a flat,
/// length-prefixed list of `(type_id, name, categories, description,
/// fields[(name,type_name,description)], has_thread, has_stacktrace)` records
/// (see the body below) — a bespoke layout that is **simpler to write and
/// read but is NOT the stock wire format**.
///
/// Consequences:
///   * **Round-trips through THIS crate's own [`read_events`] reader** — the
///     writer and reader agree on this layout, and the round-trip tests in
///     this module exercise it. Do not change one side without the other.
///   * **NOT loadable by stock JMC / `jfr print` as fully-described types** —
///     external tools cannot parse this metadata block, so they cannot render
///     typed event/field views from our files.
///
/// FOLLOW-UP — **DONE 2026-08-13, as a second writer rather than a rewrite.**
/// [`crate::jdk_chunk`] emits the stock format (binary element-tree metadata,
/// 68-byte header, unsigned-LEB128 integers) and every operator-visible dump
/// now goes through it: `FlightRecorder::dump_recording` — the single path
/// behind `jdk.jfr.Recording.dump`, the `JFR.dump` jcmd and the CLI's
/// exit-time dump — and `phase::write_jfr_report`. Verified against the JDK's
/// own `RecordingFile`, `jfr summary` and `jfr print`.
///
/// This writer was NOT migrated, because it is one half of a matching pair with
/// [`read_events`] and the fuzz target, and nothing in production writes it any
/// more. **Do not add a new production writer here**: a `.jfr` an operator or an
/// external tool will open belongs in `jdk_chunk`. If this format's remaining
/// in-crate round trip is ever retired, delete the pair together.
/// ========================================================================
///
/// LIMITATION (S2, 2026-06-10): the per-type `has_stacktrace` flag is written
/// here faithfully (and ~20 built-in types declare `has_stacktrace: true`),
/// but **no stack trace is ever captured or serialized**. [`EventInstance`]
/// has no stack-trace representation and no `emit_*` helper records a real call
/// stack — events such as `jdk.ExecutionSample` carry a caller-supplied,
/// pre-formatted `stackTrace` *string field* instead of a JFR `stackTrace`
/// constant-pool reference. Consequently JMC's stack-trace / call-tree views
/// would be empty even if the file were JMC-loadable. The flag describes the
/// event type's intent, not a captured stack.
fn write_metadata_section<W: Write>(
    writer: &mut W,
    registry: &EventTypeRegistry,
    start_time_ns: u64,
) -> io::Result<()> {
    // The metadata section is itself an event with type_id = METADATA_TYPE_ID.
    // It contains a description of all event types using a simplified encoding.
    //
    // NOTE: this is the bespoke (non-stock) metadata layout. It round-trips
    // through this crate's own `read_events` reader but is NOT JMC/`jfr`
    // loadable. See the prominent FORMAT-FIDELITY GAP (S3) block on the
    // function doc above, plus the stack-trace (S2) limitation. A stock-JFR
    // metadata writer is a documented follow-up; do not change this layout
    // without updating the reader and the round-trip tests in lockstep.

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

/// Write the 72-byte JFR file header.
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
/// Round-9 HIGH-4 fix (2026-05-24): `extra_events` and the repository's
/// own events are MERGE-SORTED by absolute `start_time` and written as
/// one globally-monotonic stream. Previously each source was written in
/// its own block, producing per-event timestamps that were non-monotonic
/// across shards — which broke the JMC timeline view, especially in
/// combination with the round-5 delta-timestamp wire format. The merged
/// sort is O(N log N) on a cold path; dumps are rare enough that the
/// trade-off is acceptable in exchange for a JMC-correct file.
///
/// Returns the total number of bytes written.
/// Round-5: `durable` controls whether the writer issues `sync_all` (fsync)
/// before renaming the `.part` file to its final `.jfr` name.
///
/// Pass `true` for user-initiated dumps (recording stop, `dump_recording`)
/// where the operator expects bytes-on-disk semantics. Pass `false` for
/// periodic snapshot paths where the cost of fsync per dump dominates and
/// the dump is best-effort. The OS will still flush dirty pages on its
/// own schedule.
pub fn dump_to_file(
    path: &Path,
    repository: &EventRepository,
    registry: &EventTypeRegistry,
    start_time_ns: u64,
    duration_ns: u64,
    extra_events: Vec<EventInstance>,
    durable: bool,
) -> Result<u64, JfrDumpError> {
    // --- Build the chunk's event list, globally sorted by start_time --------
    //
    // Cold path: dump frequency is on the order of seconds (typically at
    // recording stop). We pay an O(N log N) sort over (repository + extra)
    // and a single linear filter.
    //
    // Round-9 HIGH-4 fix (2026-05-24):
    //   Previously this function sorted `extra_events` on its own and then
    //   wrote them after the repository's events in two separate loops. With
    //   the per-thread-ring drain pattern the repository ALREADY contains
    //   events from many shards (`FlightRecorder::drain_per_thread_into_repository`
    //   fans them out per recording), and shard order is "producer-insertion
    //   per shard, then concatenated" with "no global ordering enforced
    //   across shards" — i.e. the on-disk timestamps were non-monotonic
    //   *within* a chunk. JDK Mission Control's timeline view requires
    //   per-event `start_time` to be globally monotonic for the chunk; the
    //   round-5 delta-encoding wire-format change (`JFR_VERSION_MINOR = 1`)
    //   makes the non-monotonicity worse on JMC versions that decode deltas
    //   against the previous event's tick instead of `chunk_start_time`.
    //
    //   New shape: collect references from both sources into a single
    //   `Vec<&EventInstance>` (just pointer-sized, no event clones), sort by
    //   absolute `start_time` (already-resolved by both the per-recording
    //   repository and the caller-side drain so we have a uniform key),
    //   then serialize in one pass. The repository is iterated by reference
    //   so its events are NOT cloned; only the `extra_events` Vec is owned
    //   here.
    //
    //   Cost: ~16 bytes per event for the pointer Vec plus O(N log N) for
    //   the sort. Dumps are cold-path and trade this for a JMC-correct
    //   on-disk timeline.
    let mut extra: Vec<EventInstance> = extra_events;
    // Filter to events whose metadata and payload shape are registered and
    // decodable. Unknown type IDs, wrong field counts, mismatched field
    // variants, and inverted timestamps cannot be round-tripped through
    // `read_events`, so writing them would produce unreadable records. Apply
    // the same filter to repository events below so a poisoned repository
    // cannot corrupt the chunk while extras are cleaned.
    extra.retain(|e| event_is_writable(e, registry));
    // Collect refs from both sources into one Vec for the merge-sort pass.
    // The repository iter and `extra` slice both borrow for the rest of the
    // function — no event-by-event clones.
    let mut chunk_events: Vec<&EventInstance> = Vec::with_capacity(repository.len() + extra.len());
    chunk_events.extend(repository.iter().filter(|e| event_is_writable(e, registry)));
    chunk_events.extend(extra.iter());
    // Stable sort by `start_time` so equal-timestamp events keep their
    // per-shard relative order — this matches what JMC expects for events
    // emitted by the same thread within one tick.
    chunk_events.sort_by_key(|e| e.start_time);

    // Round-10 LOW-2 fix (delta-timestamp underflow): per-event `start_time`
    // is written as `start_time - chunk_start_time` (see `serialize_event_into`)
    // and the reader reconstructs `delta + chunk_start_time`. If any event's
    // absolute `start_time` is earlier than the caller-supplied `start_time_ns`,
    // the saturating subtraction clamps its delta to 0 and read-back silently
    // shifts it forward to `chunk_start_time`. Make the chunk's tick base the
    // true minimum over the entire serialized set (repository + extra) so no
    // event can predate it. `chunk_events` is sorted ascending by `start_time`,
    // so the first element is that minimum; clamp down to it (but never above
    // the caller's nominal start, which is the empty-chunk fallback). The same
    // `chunk_start_time` is written into the header's `start_time_ns` field, so
    // the reader's reconstruction stays exact.
    let chunk_start_time = chunk_events
        .first()
        .map_or(start_time_ns, |first| first.start_time.min(start_time_ns));

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
    let mut guard = PartGuard {
        path: &part_path,
        armed: true,
    };

    // J1 (round-2): walk every event we're about to emit and intern its string
    // payloads into a per-chunk constant pool. Subsequent serialization writes
    // a compressed-int pool index per string instead of the full UTF-8 body,
    // turning N copies of "Allocation Failure" into a single pool entry + N
    // 1-byte tag-and-index references.
    //
    // Round-9 HIGH-4 (2026-05-24): walk the merged `chunk_events` rather
    // than the two original sources separately — same set of strings,
    // single pass.
    let mut string_pool = StringPool::new();
    for ev in &chunk_events {
        intern_event_strings(&mut string_pool, &ev.fields);
    }

    let file_size = {
        let file = std::fs::File::create(&part_path)?;
        let mut writer = io::BufWriter::new(file);

        // Write placeholder header (will be updated at the end). The header's
        // `start_time_ns` field carries `chunk_start_time` (the true minimum
        // event tick) so the reader's delta reconstruction is exact — see the
        // underflow fix above.
        write_header(
            &mut writer,
            0,
            0,
            0,
            chunk_start_time,
            duration_ns,
            FILE_STATE_WRITING,
        )?;

        // J1: emit the checkpoint section (containing the string pool) BEFORE
        // the events so readers can resolve constant-pool indices during the
        // forward sweep. `checkpoint_offset` is recorded for the header.
        let checkpoint_offset = writer.seek(SeekFrom::Current(0))?;
        write_checkpoint_section(&mut writer, chunk_start_time, &string_pool)?;

        // J2 (round-2): one reusable scratch buffer for every event body. Sized
        // for a typical event payload up front; serialize_event_into clears
        // and refills it per call without re-allocating.
        let mut scratch: Vec<u8> = Vec::with_capacity(256);
        let pool_ref = if string_pool.len() == 0 {
            None
        } else {
            Some(&string_pool)
        };

        // LOW fix (2026-06-17): precompute the per-type declared FieldKind
        // vectors once so a Null field in a numeric/boolean slot can emit the
        // kind-appropriate fixed-width zero (self-describing Null) instead of a
        // single 0x00 byte that desynchronises the reader. Keyed by type_id;
        // an unmapped/unknown kind ("string" or unrecognised) leaves the slot
        // as FieldKind::String so Null keeps the canonical 1-byte null tag.
        let mut kinds_by_type: FxHashMap<EventTypeId, Vec<FieldKind>> = FxHashMap::default();
        for event in &chunk_events {
            kinds_by_type.entry(event.type_id).or_insert_with(|| {
                registry
                    .get(event.type_id)
                    .map(|ty| {
                        ty.fields
                            .iter()
                            .map(|f| {
                                FieldKind::from_declared(&f.type_name).unwrap_or(FieldKind::String)
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            });
        }

        // Round-9 HIGH-4 (2026-05-24): write the globally-sorted merged
        // event stream in one pass. `chunk_events` already holds refs
        // from both the recording's repository and the caller-supplied
        // `extra_events`, sorted by absolute `start_time`. This is what
        // produces a JMC-correct monotonic timeline within the chunk.
        //
        // Round-5 Fix 3: pass `chunk_start_time` as the chunk start so each
        // event's `start_time` is written as a delta. This is the writer
        // half of the JFR_VERSION_MINOR=1 wire-format change. `chunk_start_time`
        // is the true minimum over the serialized set, so no delta underflows.
        for event in &chunk_events {
            let field_kinds = kinds_by_type.get(&event.type_id).map(|v| v.as_slice());
            serialize_event_into(
                &mut scratch,
                &mut writer,
                event.type_id,
                event.start_time,
                event.end_time,
                event.thread_id,
                &event.fields,
                pool_ref,
                chunk_start_time,
                field_kinds,
            )?;
        }

        // Write metadata
        let metadata_offset = writer.seek(SeekFrom::Current(0))?;
        write_metadata_section(&mut writer, registry, chunk_start_time)?;

        // Compute final file size
        let file_size = writer.seek(SeekFrom::Current(0))?;

        // Rewrite header with correct offsets and file state
        writer.seek(SeekFrom::Start(0))?;
        write_header(
            &mut writer,
            file_size,
            checkpoint_offset,
            metadata_offset,
            chunk_start_time,
            duration_ns,
            FILE_STATE_COMPLETE,
        )?;

        // Flush buffered writer into the OS file. When `durable`, also
        // fsync to ensure the bytes (including the rewritten header) are
        // on-disk before rename. The fsync is skipped for periodic
        // snapshot dumps where best-effort durability is acceptable.
        writer.flush()?;
        if durable {
            writer.get_ref().sync_all()?;
        }
        file_size
    };

    // Atomically swap the completed `.part` file into the final path.
    std::fs::rename(&part_path, path)?;
    guard.disarm();
    Ok(file_size)
}

/// Read an entire JFR file into memory, rejecting any file larger than
/// [`MAX_JFR_FILE_BYTES`] *before* allocating.
///
/// SECURITY (DoS hardening): `read_events` / `read_jfr_header` operate on
/// untrusted `.jfr` files and load the whole file via `std::fs::read`, which
/// allocates a buffer sized to the file. A hostile or accidentally-huge file
/// could otherwise exhaust memory. We stat the file first and bail with an
/// `InvalidData` error if it exceeds the cap, so the oversized allocation
/// never happens. (Files that grow between the stat and the read are still
/// bounded by the OS-level read; the stat is the cheap first line of defence.)
fn read_jfr_file_capped(path: &Path) -> Result<Vec<u8>, JfrDumpError> {
    let len = std::fs::metadata(path)?.len();
    if len > MAX_JFR_FILE_BYTES {
        return Err(JfrDumpError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "JFR file is {} bytes, exceeds maximum of {} bytes",
                len, MAX_JFR_FILE_BYTES
            ),
        )));
    }
    Ok(std::fs::read(path)?)
}

/// Read and validate the header from a JFR file. Returns the parsed header fields.
pub fn read_jfr_header(path: &Path) -> Result<JfrFileHeader, JfrDumpError> {
    let data = read_jfr_file_capped(path)?;
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
                JfrDumpError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "int decode failed",
                ))
            })?;
            Ok((EventValue::Int(v as i32), c))
        }
        "long" => {
            let (v, c) = decode_compressed_long(&data[pos..]).ok_or_else(|| {
                JfrDumpError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "long decode failed",
                ))
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
                    // checked_add: `len` is an attacker-controlled compressed-int;
                    // an unchecked `+` would wrap `usize` past the bounds check.
                    let end = (start as u64)
                        .checked_add(len)
                        .and_then(|e| usize::try_from(e).ok())
                        .ok_or_else(|| {
                            JfrDumpError::Io(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "string length overflow",
                            ))
                        })?;
                    if end > data.len() {
                        return Err(JfrDumpError::Io(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "string body truncated",
                        )));
                    }
                    let s = std::str::from_utf8(&data[start..end]).map_err(|e| {
                        JfrDumpError::Io(io::Error::new(io::ErrorKind::InvalidData, e))
                    })?;
                    // `end - pos` == `1 + lc + len` but cannot overflow: `end`
                    // was validated above and `end >= pos`.
                    Ok((EventValue::String(std::sync::Arc::from(s)), end - pos))
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
    // checked_add: `record_size` is an attacker-controlled compressed-int; a
    // huge value would wrap `usize` and slip past the `> data.len()` check.
    let record_end = (offset as u64)
        .checked_add(record_size)
        .and_then(|e| usize::try_from(e).ok())
        .ok_or_else(|| {
            JfrDumpError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "checkpoint record size overflow",
            ))
        })?;
    if record_end > data.len() {
        return Err(JfrDumpError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "checkpoint record extends past EOF",
        )));
    }
    let mut pos = offset + size_len;

    // SECURITY: all header/pool field decodes below must stay inside this
    // record. `record_end` is already validated `<= data.len()`, but decoding
    // against `&data[pos..]` would let a malformed checkpoint read varints out
    // of the following section (events/metadata). Slice against `record_end`
    // so a decode can never cross the record boundary. `pos` starts at
    // `offset + size_len <= record_end` and only ever advances by bytes a
    // decode reported it consumed from a `record_end`-bounded slice, so each
    // `&data[pos..record_end]` below has `pos <= record_end` (a valid range).

    // Skip: type_id, timestamp, duration, delta, type_mask.
    for _ in 0..5 {
        let (_, c) = decode_compressed_long(&data[pos..record_end]).ok_or_else(|| {
            JfrDumpError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "checkpoint header field decode failed",
            ))
        })?;
        pos += c;
    }
    // Number of constant pools.
    let (n_pools, c) = decode_compressed_int(&data[pos..record_end]).ok_or_else(|| {
        JfrDumpError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "constant pool count decode failed",
        ))
    })?;
    pos += c;

    let mut strings: Vec<std::sync::Arc<str>> = Vec::new();
    for _ in 0..n_pools {
        let (pool_type, ptc) = decode_compressed_int(&data[pos..record_end]).ok_or_else(|| {
            JfrDumpError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "pool type decode failed",
            ))
        })?;
        pos += ptc;
        let (n_entries, nec) = decode_compressed_int(&data[pos..record_end]).ok_or_else(|| {
            JfrDumpError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "pool entry count decode failed",
            ))
        })?;
        pos += nec;

        if pool_type == STRING_POOL_TYPE_ID {
            // `n_entries` is decoded straight from the (untrusted) file. A
            // crafted small file can encode a huge count and trigger a
            // multi-GB `reserve` / OOM. Each entry needs at least one byte
            // on the wire (its index varint), so a count larger than the
            // bytes remaining in the record is structurally impossible —
            // reject it instead of trusting it.
            let remaining = record_end.saturating_sub(pos);
            if n_entries as u64 > remaining as u64 {
                return Err(JfrDumpError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "pool entry count {} exceeds {} bytes remaining in record",
                        n_entries, remaining
                    ),
                )));
            }
            strings.reserve(n_entries as usize);
            for _ in 0..n_entries {
                let (_idx, ic) =
                    decode_compressed_int(&data[pos..record_end]).ok_or_else(|| {
                        JfrDumpError::Io(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "pool entry index decode failed",
                        ))
                    })?;
                pos += ic;
                let (slen, lc) =
                    decode_compressed_int(&data[pos..record_end]).ok_or_else(|| {
                        JfrDumpError::Io(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "pool entry length decode failed",
                        ))
                    })?;
                pos += lc;
                // checked_add: `slen` is an attacker-controlled compressed-int;
                // an unchecked `+` would wrap `usize` past the bounds check.
                let end = (pos as u64)
                    .checked_add(slen)
                    .and_then(|e| usize::try_from(e).ok())
                    .ok_or_else(|| {
                        JfrDumpError::Io(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "pool entry length overflow",
                        ))
                    })?;
                if end > record_end {
                    return Err(JfrDumpError::Io(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "pool entry body extends past record",
                    )));
                }
                let s = std::str::from_utf8(&data[pos..end])
                    .map_err(|e| JfrDumpError::Io(io::Error::new(io::ErrorKind::InvalidData, e)))?;
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
    let data = read_jfr_file_capped(path)?;
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
    // Round-5 Fix 3: pick up the minor version so we know whether the event
    // `start_time` field encodes an absolute timestamp (minor=0) or a delta
    // from `chunk_start_time` (minor=1). The chunk_start_time lives at bytes
    // 32..40 (the header's `start_time_ns` field).
    let minor = u16::from_be_bytes([data[6], data[7]]);
    let chunk_start_time = u64::from_be_bytes(data[32..40].try_into().unwrap());
    let timestamps_are_deltas = minor >= 1;
    let checkpoint_offset = u64::from_be_bytes(data[16..24].try_into().unwrap()) as usize;
    let metadata_offset = u64::from_be_bytes(data[24..32].try_into().unwrap()) as usize;

    // SECURITY: `checkpoint_offset` and `metadata_offset` come straight from the
    // untrusted 72-byte header. They become the `events_start` / `events_end`
    // bounds and seed the `parse_checkpoint_pool` walk; a crafted file can set
    // either past EOF, which would later slice `&data[pos..]` with
    // `pos > data.len()` and panic. Reject any offset that points past the end
    // of the file before they are used.
    if checkpoint_offset > data.len() {
        return Err(JfrDumpError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "checkpoint_offset extends past EOF",
        )));
    }
    if metadata_offset > data.len() {
        return Err(JfrDumpError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "metadata_offset extends past EOF",
        )));
    }

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
    // SECURITY (defense-in-depth): the walk loop below indexes `&data[pos..]`
    // for `pos < events_end`. `checkpoint_offset` / `metadata_offset` are
    // already validated `<= data.len()` above, but clamp here too so the loop
    // bound is provably in-bounds regardless of which layout branch produced
    // `events_end`.
    let events_end = events_end.min(data.len());
    let pool_ref: Option<&[std::sync::Arc<str>]> = if pool.is_empty() { None } else { Some(&pool) };

    // Walk records within [events_start, events_end). Each record starts with
    // a compressed_int size field that includes the size byte(s).
    let mut pos = events_start;
    let mut out = Vec::new();
    while pos < events_end {
        let (total_size, size_len) =
            decode_compressed_int(&data[pos..events_end]).ok_or_else(|| {
                JfrDumpError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "record size decode failed",
                ))
            })?;
        let min_event_header_size = size_len as u64 + 4;
        if total_size < min_event_header_size {
            return Err(JfrDumpError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "record too small for event header",
            )));
        }
        // checked_add: `total_size` is an attacker-controlled compressed-int;
        // an unchecked `+` would wrap `usize` past the bounds check below.
        let record_end = (pos as u64)
            .checked_add(total_size)
            .and_then(|e| usize::try_from(e).ok())
            .ok_or_else(|| {
                JfrDumpError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "record size overflow",
                ))
            })?;
        if record_end > events_end {
            return Err(JfrDumpError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "record extends past events region",
            )));
        }

        let mut rpos = pos + size_len;
        let (type_id_raw, tc) =
            decode_compressed_int(&data[rpos..record_end]).ok_or_else(|| {
                JfrDumpError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "type_id decode failed",
                ))
            })?;
        rpos += tc;
        let type_id = EventTypeId(type_id_raw as u32);

        let (start_time_raw, sc) =
            decode_compressed_long(&data[rpos..record_end]).ok_or_else(|| {
                JfrDumpError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "start_time decode failed",
                ))
            })?;
        rpos += sc;
        // Round-5 Fix 3: minor=1 stores the on-wire field as a delta from
        // the chunk's start_time; re-add it. minor=0 (legacy) stores
        // absolute values — pass through unchanged so old `.jfr` files
        // produced by pre-round-5 writers still decode correctly.
        let start_time = if timestamps_are_deltas {
            (start_time_raw as u64).saturating_add(chunk_start_time) as i64
        } else {
            start_time_raw
        };

        let (duration, dc) = decode_compressed_long(&data[rpos..record_end]).ok_or_else(|| {
            JfrDumpError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "duration decode failed",
            ))
        })?;
        rpos += dc;

        let (thread_id, tdc) =
            decode_compressed_long(&data[rpos..record_end]).ok_or_else(|| {
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

        let mut fields = crate::event::EventFields::with_capacity(ty.fields.len());
        // B1 (2026-06-10): bound every field decode to this record's own
        // declared size. The `record_end > events_end` guard above only
        // validates the *declared* `total_size`; it does NOT guarantee the
        // decoded fields stay within `[pos, record_end)`. A malformed record
        // can declare a small `total_size` yet contain a tag-3 inline string
        // whose length runs past `record_end` into the next record's bytes —
        // yielding silently-wrong decoded values (it does not crash, since all
        // decodes are bounded by the slice length and `pos = record_end`
        // re-syncs each iteration). Slicing against `record_end` makes a field
        // decode unable to cross the record boundary, matching the bounding
        // `parse_checkpoint_pool` already applies to checkpoint fields.
        // `record_end <= events_end <= data.len()` (validated above) so the
        // slice is in range, and `rpos` starts at `pos + size_len <= record_end`
        // and only advances by bytes a `record_end`-bounded decode consumed.
        let record_slice = &data[..record_end];
        for field in &ty.fields {
            let (v, c) = decode_event_value(record_slice, rpos, &field.type_name, pool_ref)?;
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
    use crate::event::{
        EventField, EventInstance, EventPeriod, EventType, EventTypeId, EventValue,
    };
    use smallvec::smallvec;
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

    /// A crafted checkpoint section that declares a huge `n_entries` while
    /// carrying only a few bytes must be rejected, not trigger a multi-GB
    /// `reserve`. `parse_checkpoint_pool` bounds `n_entries` against the
    /// bytes remaining in the record.
    #[test]
    fn test_checkpoint_pool_rejects_oversized_entry_count() {
        let mut body: Vec<u8> = Vec::new();
        // 5 checkpoint header longs: type_id, timestamp, duration, delta, mask.
        for _ in 0..5 {
            body.extend_from_slice(&encode_compressed_long(0));
        }
        // n_pools = 1
        body.extend_from_slice(&encode_compressed_int(1));
        // pool_type = STRING_POOL_TYPE_ID
        body.extend_from_slice(&encode_compressed_int(STRING_POOL_TYPE_ID));
        // n_entries = absurdly large, far beyond the bytes that follow.
        body.extend_from_slice(&encode_compressed_int(1_000_000_000));
        // No entry bytes follow at all.

        // Prepend the record-size prefix. The size field counts itself, so
        // for a body this small a single-byte prefix suffices.
        let mut record: Vec<u8> = Vec::new();
        let total = body.len() + 1;
        assert!(total < 0x80, "fixture small enough for 1-byte size prefix");
        record.extend_from_slice(&encode_compressed_int(total as u64));
        record.extend_from_slice(&body);

        let err =
            parse_checkpoint_pool(&record, 0).expect_err("oversized n_entries must be rejected");
        match err {
            JfrDumpError::Io(e) => {
                assert_eq!(e.kind(), io::ErrorKind::InvalidData);
            }
            other => panic!("expected Io(InvalidData) parse error, got {:?}", other),
        }
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

    fn make_registry_with_zero_field_type() -> (EventTypeRegistry, EventTypeId) {
        let mut reg = EventTypeRegistry::new();
        let id = reg.register(EventType {
            id: EventTypeId(0),
            name: "test.EmptyEvent".into(),
            category: vec!["Test".into()],
            description: "empty event".into(),
            fields: Vec::new(),
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
            fields: smallvec![EventValue::Int(42), EventValue::String(Arc::from("hello")),],
        });
        repo.push(EventInstance {
            type_id,
            start_time: 3_000_000,
            end_time: 4_000_000,
            thread_id: 2,
            fields: smallvec![EventValue::Int(99), EventValue::String(Arc::from("world")),],
        });

        let dir = std::env::temp_dir().join("jfr_test_dump");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_dump.jfr");

        let file_size =
            dump_to_file(&path, &repo, &reg, 1_000_000, 3_000_000, Vec::new(), false).unwrap();
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
        // Serialize against other global-ring tests so a concurrent emit/drain
        // cannot perturb our "empty" invariant.
        let _g = crate::repository::jfr_test_guard();
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
        let file_size = dump_to_file(&path, &repo, &reg, 0, 0, Vec::new(), false).unwrap();
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
            fields: smallvec![EventValue::Int(1), EventValue::from_str("x")],
        });

        let dir = std::env::temp_dir().join("jfr_test_magic");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_magic.jfr");

        dump_to_file(&path, &repo, &reg, 100, 100, Vec::new(), false).unwrap();

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
            fields: smallvec![
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
            fields: smallvec![
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

        let file_size = dump_to_file(&path, &repo, &reg, 500, 300, Vec::new(), false).unwrap();
        let header = read_jfr_header(&path).unwrap();
        assert_eq!(header.file_state, FILE_STATE_COMPLETE);
        assert_eq!(header.file_size, file_size);
        // J1 (round-2) layout: checkpoint section sits immediately after the
        // file header (so readers resolve the string pool before walking
        // events). Previously the writer emitted events first and the
        // checkpoint last, so `checkpoint_offset > HEADER_SIZE` held.
        assert_eq!(header.checkpoint_offset, HEADER_SIZE);
        assert!(header.metadata_offset > header.checkpoint_offset);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// LOW fix (2026-06-17) regression: a `Null` in a numeric/float/double/
    /// boolean field followed by MORE fields must NOT desync the chunk. Before
    /// the fix, Null always emitted a single `0x00` byte, so a `float`/`double`
    /// Null left the reader 3-7 bytes short and it mis-decoded the *next*
    /// field's bytes. This test places a Null in every non-last fixed-width
    /// slot (each followed by a sentinel field) and asserts both the Null
    /// round-trips to the kind's typed zero AND every following sentinel
    /// decodes to its written value.
    #[test]
    fn test_null_numeric_field_roundtrips_without_desync() {
        let mut reg = EventTypeRegistry::new();
        let type_id = reg.register(EventType {
            id: EventTypeId(0),
            name: "test.NullFields".into(),
            category: vec!["Test".into()],
            description: "Null in non-last fixed-width fields".into(),
            // Every numeric/bool field is followed by a distinctive sentinel
            // so a width desync would corrupt the sentinel's decoded value.
            fields: vec![
                EventField::new("i", "int", ""),
                EventField::new("after_i", "int", ""),
                EventField::new("l", "long", ""),
                EventField::new("after_l", "int", ""),
                EventField::new("f", "float", ""),
                EventField::new("after_f", "int", ""),
                EventField::new("d", "double", ""),
                EventField::new("after_d", "int", ""),
                EventField::new("b", "boolean", ""),
                EventField::new("after_b", "int", ""),
                EventField::new("s", "string", ""),
                EventField::new("after_s", "int", ""),
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
            fields: smallvec![
                EventValue::Null,    // i   -> Int(0)
                EventValue::Int(11), // after_i
                EventValue::Null,    // l   -> Long(0)
                EventValue::Int(22), // after_l
                EventValue::Null,    // f   -> Float(0.0)
                EventValue::Int(33), // after_f
                EventValue::Null,    // d   -> Double(0.0)
                EventValue::Int(44), // after_d
                EventValue::Null,    // b   -> Boolean(false)
                EventValue::Int(55), // after_b
                EventValue::Null,    // s   -> Null (string null tag)
                EventValue::Int(66), // after_s
            ],
        });

        let dir = std::env::temp_dir().join("jfr_test_null_fields");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_null_fields.jfr");

        dump_to_file(&path, &repo, &reg, 500, 300, Vec::new(), false).unwrap();
        let events = read_events(&path, &reg).unwrap();
        assert_eq!(events.len(), 1, "exactly one event round-tripped");
        let f = &events[0].fields;
        assert_eq!(f.len(), 12, "all 12 fields decoded — no truncation");

        // The numeric/bool Nulls decode to the declared kind's typed zero.
        assert!(
            matches!(f[0], EventValue::Int(0)),
            "int null -> Int(0): {:?}",
            f[0]
        );
        assert!(
            matches!(f[2], EventValue::Long(0)),
            "long null -> Long(0): {:?}",
            f[2]
        );
        assert!(
            matches!(f[4], EventValue::Float(v) if v == 0.0),
            "float null -> Float(0.0): {:?}",
            f[4]
        );
        assert!(
            matches!(f[6], EventValue::Double(v) if v == 0.0),
            "double null -> Double(0.0): {:?}",
            f[6]
        );
        assert!(
            matches!(f[8], EventValue::Boolean(false)),
            "boolean null -> Boolean(false): {:?}",
            f[8]
        );
        // The string Null keeps the canonical null tag and round-trips to Null.
        assert!(
            matches!(f[10], EventValue::Null),
            "string null -> Null: {:?}",
            f[10]
        );

        // Every sentinel that FOLLOWS a Null decodes to its written value —
        // this is the desync canary. Any width mismatch would corrupt these.
        assert!(
            matches!(f[1], EventValue::Int(11)),
            "after_i intact: {:?}",
            f[1]
        );
        assert!(
            matches!(f[3], EventValue::Int(22)),
            "after_l intact: {:?}",
            f[3]
        );
        assert!(
            matches!(f[5], EventValue::Int(33)),
            "after_f intact: {:?}",
            f[5]
        );
        assert!(
            matches!(f[7], EventValue::Int(44)),
            "after_d intact: {:?}",
            f[7]
        );
        assert!(
            matches!(f[9], EventValue::Int(55)),
            "after_b intact: {:?}",
            f[9]
        );
        assert!(
            matches!(f[11], EventValue::Int(66)),
            "after_s intact: {:?}",
            f[11]
        );

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
                fields: smallvec![
                    EventValue::Int(i as i32),
                    EventValue::from_str(&format!("event_{}", i)),
                ],
            });
        }

        let dir = std::env::temp_dir().join("jfr_test_many");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_many.jfr");

        let file_size = dump_to_file(&path, &repo, &reg, 0, 500_000, Vec::new(), false).unwrap();
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
            fields: smallvec![EventValue::Int(1), EventValue::from_str("a")],
        });

        let dir = std::env::temp_dir().join("jfr_test_layout");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_layout.jfr");

        dump_to_file(&path, &repo, &reg, 100, 100, Vec::new(), false).unwrap();
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

    /// Build a syntactically valid 72-byte JFR header with caller-chosen
    /// `checkpoint_offset` (bytes 16..24) and `metadata_offset` (bytes 24..32).
    /// All other fields are zeroed except the magic and minor version.
    fn make_header_with_offsets(checkpoint_offset: u64, metadata_offset: u64) -> Vec<u8> {
        let mut h = vec![0u8; HEADER_SIZE as usize];
        h[0..4].copy_from_slice(&JFR_MAGIC);
        // minor version 0 (bytes 6..8): absolute timestamps, simplest path.
        h[16..24].copy_from_slice(&checkpoint_offset.to_be_bytes());
        h[24..32].copy_from_slice(&metadata_offset.to_be_bytes());
        h
    }

    /// SECURITY regression: a header whose `metadata_offset` points past the
    /// end of the file must return `Err`, not panic on an out-of-bounds slice.
    #[test]
    fn test_read_events_metadata_offset_past_eof() {
        let dir = std::env::temp_dir().join("jfr_test_meta_oob");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("meta_oob.jfr");

        // checkpoint at header end (legacy/empty layout), metadata far past EOF.
        let data = make_header_with_offsets(HEADER_SIZE, 0xFFFF_FFFF);
        std::fs::write(&path, &data).unwrap();

        let reg = EventTypeRegistry::new();
        let result = read_events(&path, &reg);
        assert!(
            result.is_err(),
            "metadata_offset past EOF must error, not panic"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// SECURITY regression: a header whose `checkpoint_offset` points past the
    /// end of the file must return `Err`, not panic. This offset seeds both the
    /// `parse_checkpoint_pool` walk and the legacy `events_end` bound.
    #[test]
    fn test_read_events_checkpoint_offset_past_eof() {
        let dir = std::env::temp_dir().join("jfr_test_ckpt_oob");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("ckpt_oob.jfr");

        // checkpoint past EOF, metadata larger still (so the layout branch that
        // treats checkpoint as events_end is taken).
        let data = make_header_with_offsets(0xFFFF_FFFF, 0xFFFF_FFFF);
        std::fs::write(&path, &data).unwrap();

        let reg = EventTypeRegistry::new();
        let result = read_events(&path, &reg);
        assert!(
            result.is_err(),
            "checkpoint_offset past EOF must error, not panic"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_read_events_rejects_record_smaller_than_event_header() {
        let dir = std::env::temp_dir().join("jfr_test_event_header_too_small");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("event_header_too_small.jfr");

        let (reg, type_id) = make_registry_with_zero_field_type();
        let mut data = make_header_with_offsets(HEADER_SIZE + 1, HEADER_SIZE + 1);
        data.extend_from_slice(&encode_compressed_int(1));
        data.extend_from_slice(&encode_compressed_int(type_id.0 as u64));
        data.extend_from_slice(&encode_compressed_long(0));
        data.extend_from_slice(&encode_compressed_long(0));
        data.extend_from_slice(&encode_compressed_long(0));
        std::fs::write(&path, &data).unwrap();

        let result = read_events(&path, &reg);
        assert!(
            result.is_err(),
            "event record shorter than the fixed event header must be rejected"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_read_events_header_decode_bounded_by_record() {
        let dir = std::env::temp_dir().join("jfr_test_event_header_bounded");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("event_header_bounded.jfr");

        let (reg, type_id) = make_registry_with_zero_field_type();
        let mut record = Vec::new();
        write_compressed_int_into(&mut record, 5);
        write_compressed_int_into(&mut record, type_id.0 as u64);
        write_compressed_long_into(&mut record, 0);
        write_compressed_long_into(&mut record, 0);
        record.push(0x80);

        let record_end = HEADER_SIZE as usize + record.len();
        let mut data = make_header_with_offsets(record_end as u64, record_end as u64);
        data.extend_from_slice(&record);
        data.push(0);
        std::fs::write(&path, &data).unwrap();

        let result = read_events(&path, &reg);
        assert!(
            result.is_err(),
            "event header varints must not read past record_end"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// SECURITY regression: a checkpoint record that is in-bounds but whose
    /// internal varint fields claim to extend toward the following section must
    /// not read past `record_end`. We craft a checkpoint whose declared record
    /// size is small but whose header-field varints would otherwise consume
    /// bytes beyond the record. The bounded decode must return `Err` cleanly.
    #[test]
    fn test_parse_checkpoint_pool_fields_bounded_by_record() {
        // Lay out a file: 72-byte header, then a checkpoint record at
        // HEADER_SIZE whose declared size is just 1 byte (only the size field),
        // leaving no room for the 5 mandatory header-field varints. The decode
        // against `&data[pos..record_end]` must fail rather than reading into
        // whatever follows.
        let dir = std::env::temp_dir().join("jfr_test_ckpt_bounded");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("ckpt_bounded.jfr");

        let mut data = make_header_with_offsets(HEADER_SIZE, HEADER_SIZE + 8);
        // Checkpoint record: size = 1 (consumes only the size byte). The five
        // header-field decodes then see an empty `&data[pos..record_end]`.
        data.extend_from_slice(&encode_compressed_int(1));
        // Trailing bytes that, if the decode were unbounded, would be misread
        // as header fields. They must be ignored.
        data.extend_from_slice(&[0x7F, 0x7F, 0x7F, 0x7F, 0x7F, 0x7F, 0x7F]);

        std::fs::write(&path, &data).unwrap();

        let reg = EventTypeRegistry::new();
        // Must return Err (record-bounded decode fails) rather than silently
        // reading the trailing bytes or panicking.
        let result = read_events(&path, &reg);
        assert!(
            result.is_err(),
            "checkpoint field decode must stay within record"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// B1 (2026-06-10) regression: a record whose declared `total_size` is
    /// honest but that contains a tag-3 inline string whose *declared length*
    /// runs past `record_end` must error, not silently read into the following
    /// record. Before the fix the field decode ran against the full file slice
    /// (`&data`), so an over-long string length was satisfied by the next
    /// record's bytes — yielding a silently-wrong decoded value. The decode is
    /// now bounded by `&data[..record_end]`, so it returns "string body
    /// truncated".
    #[test]
    fn test_read_events_field_decode_bounded_by_record() {
        let dir = std::env::temp_dir().join("jfr_test_field_bounded");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("field_bounded.jfr");

        // One event type with a single `string` field.
        let mut reg = EventTypeRegistry::new();
        let type_id = reg.register(EventType {
            id: EventTypeId(0),
            name: "test.StrEvent".into(),
            category: vec!["Test".into()],
            description: "string field event".into(),
            fields: vec![EventField::new("name", "string", "Name")],
            has_thread: true,
            has_stacktrace: false,
            period: EventPeriod::None,
            threshold: None,
        });

        // Build the corrupt record body (everything after the size prefix).
        // Layout: [type_id][start_delta][duration][thread_id][string field].
        // The string field is tag 3 (inline) with a declared length of 64,
        // but we provide NO payload bytes inside the record — so the declared
        // length crosses `record_end` into the trailing bytes appended below.
        let mut inner = Vec::new();
        write_compressed_int_into(&mut inner, type_id.0 as u64); // type_id
        write_compressed_long_into(&mut inner, 0); // start_delta
        write_compressed_long_into(&mut inner, 0); // duration
        write_compressed_long_into(&mut inner, 0); // thread_id
        inner.push(3); // STRING tag 3 (inline length-prefixed)
        write_compressed_int_into(&mut inner, 64); // declared len, no payload

        // Size-prefix the record (size includes the prefix itself).
        let mut record = Vec::new();
        {
            let body_len = inner.len();
            let mut prefix_len = 1usize;
            let total = loop {
                let t = prefix_len + body_len;
                let actual = compressed_int_len(t as u64);
                if actual == prefix_len {
                    break t;
                }
                prefix_len = actual;
            };
            write_compressed_int_into(&mut record, total as u64);
            record.extend_from_slice(&inner);
        }
        let record_end_offset = HEADER_SIZE as usize + record.len();

        // Trailing bytes that the over-long string would read into if the
        // decode were unbounded. Make them valid UTF-8 so that, pre-fix, the
        // decode would *succeed* with a wrong value rather than fail — proving
        // the bound (not an unrelated error) is what stops the cross-read.
        let trailing = vec![b'A'; 64];

        // metadata_offset == checkpoint_offset == record_end_offset so the
        // events region is exactly [HEADER_SIZE, record_end_offset): the single
        // corrupt record, with `trailing` living past `events_end`.
        let mut data = make_header_with_offsets(record_end_offset as u64, record_end_offset as u64);
        data.extend_from_slice(&record);
        data.extend_from_slice(&trailing);
        std::fs::write(&path, &data).unwrap();

        let result = read_events(&path, &reg);
        assert!(
            result.is_err(),
            "field decode must stay within the record boundary, got {:?}",
            result.map(|v| v.len())
        );

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
            0,
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

        // Serialize against other tests that drain the global registry, so a
        // concurrent `drain_all()` cannot steal the events we push below.
        let _g = crate::repository::jfr_test_guard();

        // Baseline drain — discard anything left over from prior tests.
        let _baseline = global_ring_registry().drain_all();

        let (reg, type_id) = make_registry_with_one_type();
        let repo = EventRepository::new(100);

        // Push three events with distinctive start_times. They will go through
        // the calling thread's shard in the global registry.
        let pushed_starts: [u64; 3] = [0xA0A0_0000_0011, 0xA0A0_0000_0022, 0xA0A0_0000_0033];
        for &start in &pushed_starts {
            push_to_thread_ring(EventInstance {
                type_id,
                start_time: start,
                end_time: start + 100,
                thread_id: 7,
                fields: smallvec![
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
        let _file_size = dump_to_file(&path, &repo, &reg, 0, 0, drained, false).unwrap();

        // Header should be valid.
        // J1 (round-2) layout: checkpoint section now precedes the event
        // region (so readers resolve the string pool before walking events),
        // meaning `checkpoint_offset == HEADER_SIZE` regardless of how many
        // events were drained. The previous `> HEADER_SIZE` assertion was
        // checking the old "events first, checkpoint last" layout. Verify
        // there *are* events by asserting metadata_offset sits past the
        // checkpoint payload plus the serialized events.
        let header = read_jfr_header(&path).unwrap();
        assert_eq!(header.file_state, FILE_STATE_COMPLETE);
        assert_eq!(header.checkpoint_offset, HEADER_SIZE);
        assert!(
            header.metadata_offset > header.checkpoint_offset,
            "metadata should sit past checkpoint + events region"
        );

        // Read events back and verify each pushed event is present.
        let events = read_events(&path, &reg).unwrap();
        for &expected_start in &pushed_starts {
            let found = events
                .iter()
                .any(|e| e.type_id == type_id && e.start_time == expected_start);
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
        dump_to_file(&path2, &repo2, &reg, 0, 0, Vec::new(), false).unwrap();
        let events2 = read_events(&path2, &reg).unwrap();
        for &expected_start in &pushed_starts {
            let still_there = events2
                .iter()
                .any(|e| e.type_id == type_id && e.start_time == expected_start);
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

        // Serialize against other tests that drain the global registry, so a
        // concurrent `drain_all()` cannot steal the events we push below.
        let _g = crate::repository::jfr_test_guard();

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
                fields: smallvec![EventValue::Int(0), EventValue::from_str("s")],
            });
        }

        let dir = std::env::temp_dir().join("jfr_test_sort_drained");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("sort_drained.jfr");
        // Drain the pushed events and pass them in — see Bug 1 fix.
        let drained = global_ring_registry().drain_all();
        dump_to_file(&path, &repo, &reg, 0, 0, drained, false).unwrap();

        let events = read_events(&path, &reg).unwrap();
        // Pull out just the ones we pushed (by tag prefix) and check they are
        // strictly increasing in start_time on disk.
        let ours: Vec<u64> = events
            .iter()
            .filter(|e| e.type_id == type_id && (e.start_time & 0xFFFF_FFFF_FFFF_FF00) == tag)
            .map(|e| e.start_time)
            .collect();
        assert_eq!(
            ours.len(),
            push_order.len(),
            "expected all {} pushed events back, got {}: {:?}",
            push_order.len(),
            ours.len(),
            ours
        );
        let sorted = {
            let mut v = ours.clone();
            v.sort();
            v
        };
        assert_eq!(
            ours, sorted,
            "drained events should be written in start_time order"
        );
        // Make sure the test actually exercises sorting (i.e. the push order
        // was not already monotonically increasing).
        assert_ne!(
            push_order.to_vec(),
            sorted,
            "test setup bug: push_order happens to equal sorted order"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// Round-9 HIGH-4 fix (2026-05-24): events that originate from the
    /// per-recording repository AND the caller-supplied `extra_events` must
    /// be merge-sorted by `start_time` and written as one monotonic stream.
    /// Previously the repository block was written first (in insertion order
    /// across shards) and the `extra_events` block second — a JMC reader
    /// would see a per-event timestamp that went backwards in the middle of
    /// the chunk.
    #[test]
    fn test_dump_merges_repository_and_extra_events_by_start_time() {
        let (reg, type_id) = make_registry_with_one_type();

        // Repository holds an early and a late event; `extra_events` carries
        // a middle one. Without the cross-shard sort, the on-disk order
        // would be [early, late, middle] — i.e. non-monotonic.
        let mut repo = EventRepository::new(10);
        repo.push(EventInstance {
            type_id,
            start_time: 100,
            end_time: 110,
            thread_id: 1,
            fields: smallvec![EventValue::Int(0), EventValue::from_str("early")],
        });
        repo.push(EventInstance {
            type_id,
            start_time: 500,
            end_time: 510,
            thread_id: 1,
            fields: smallvec![EventValue::Int(2), EventValue::from_str("late")],
        });
        let extra = vec![EventInstance {
            type_id,
            start_time: 300,
            end_time: 310,
            thread_id: 2,
            fields: smallvec![EventValue::Int(1), EventValue::from_str("middle")],
        }];

        let dir = std::env::temp_dir().join("jfr_test_merge_sort");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("merge_sort.jfr");
        dump_to_file(&path, &repo, &reg, 0, 1000, extra, false).unwrap();

        // The reader walks records in file order, so the resulting `Vec`
        // preserves on-disk order. Assert the three known events come out
        // sorted by `start_time`.
        let events = read_events(&path, &reg).unwrap();
        let starts: Vec<u64> = events.iter().map(|e| e.start_time).collect();
        // Filter to our three known starts in case any cross-test ring
        // stragglers leaked through.
        let ours: Vec<u64> = starts
            .iter()
            .copied()
            .filter(|s| *s == 100 || *s == 300 || *s == 500)
            .collect();
        assert_eq!(
            ours,
            vec![100, 300, 500],
            "merge sort should produce a monotonic on-disk timeline across the repository and extra events",
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_dump_filters_malformed_repository_and_extra_events() {
        let (reg, type_id) = make_registry_with_one_type();

        let mut repo = EventRepository::new(10);
        repo.push(EventInstance {
            type_id,
            start_time: 100,
            end_time: 110,
            thread_id: 1,
            fields: smallvec![EventValue::Int(1), EventValue::from_str("valid")],
        });
        repo.push(EventInstance {
            type_id: EventTypeId(0xF00D),
            start_time: 120,
            end_time: 130,
            thread_id: 1,
            fields: smallvec![EventValue::Int(2), EventValue::from_str("unknown")],
        });
        repo.push(EventInstance {
            type_id,
            start_time: 140,
            end_time: 150,
            thread_id: 1,
            fields: smallvec![EventValue::Int(3)],
        });

        let extra = vec![
            EventInstance {
                type_id,
                start_time: 160,
                end_time: 155,
                thread_id: 2,
                fields: smallvec![EventValue::Int(4), EventValue::from_str("bad-time")],
            },
            EventInstance {
                type_id,
                start_time: 180,
                end_time: 190,
                thread_id: 2,
                fields: smallvec![EventValue::Double(5.0), EventValue::from_str("bad-shape")],
            },
        ];

        let dir = std::env::temp_dir().join("jfr_test_filter_malformed");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("filter_malformed.jfr");
        dump_to_file(&path, &repo, &reg, 0, 200, extra, false).unwrap();

        let events = read_events(&path, &reg).unwrap();
        let ours: Vec<&EventInstance> = events
            .iter()
            .filter(|e| e.type_id == type_id && e.start_time >= 100 && e.start_time <= 190)
            .collect();
        assert_eq!(ours.len(), 1);
        assert_eq!(ours[0].start_time, 100);
        assert!(matches!(ours[0].fields[0], EventValue::Int(1)));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_dump_file_complete_lifecycle() {
        // Round-4/5 (C33): `emit_*` writes into the per-thread ring rather
        // than the recording's repository. `drain_per_thread_into_repository`
        // (or `dump_recording`, which calls it internally) is required to
        // make events visible to `event_count()` / `get_events()`. We baseline
        // the ring first so cross-test stragglers don't inflate our count.
        //
        // Round-5 wire-format change: the writer now emits
        // `JFR_VERSION_MINOR = 1` (delta-encoded timestamps). The previous
        // assertion `header.minor == 0` was stale.
        // Serialize against other global-ring tests so a concurrent drain can't
        // steal our emitted events before we drain them into the repository.
        let _g = crate::repository::jfr_test_guard();

        let mut fr = crate::create_flight_recorder();
        let rid = fr.new_recording(crate::recording::RecordingSettings::new("dump-test"));
        fr.start_recording(rid);

        let _ = crate::repository::global_ring_registry().drain_all();

        crate::builtin::emit_gc_event(
            &mut fr,
            1,
            "G1 Young",
            "Allocation Failure",
            1_000_000,
            500_000,
        );
        crate::builtin::emit_thread_start_event(&mut fr, "main", "", 1, 2_000_000);
        crate::builtin::emit_class_load_event(
            &mut fr,
            "java/lang/Object",
            "bootstrap",
            "bootstrap",
            3_000_000,
            100_000,
        );

        // Drain ring shards into the recording's repository before stopping
        // so the snapshot below reflects the three emitted events.
        fr.drain_per_thread_into_repository();
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

        let file_size = dump_to_file(
            &path,
            &dump_repo,
            &fr.type_registry,
            start_time,
            duration,
            Vec::new(),
            false,
        )
        .unwrap();
        let header = read_jfr_header(&path).unwrap();

        assert_eq!(header.magic, JFR_MAGIC);
        assert_eq!(header.major, JFR_VERSION_MAJOR);
        assert_eq!(header.minor, JFR_VERSION_MINOR);
        assert_eq!(header.file_state, FILE_STATE_COMPLETE);
        assert_eq!(header.file_size, file_size);
        assert_eq!(header.ticks_per_second, TICKS_PER_SECOND);
        // J1 (round-2) layout: checkpoint section sits immediately after the
        // header. Previously the writer placed it after the event region.
        assert_eq!(header.checkpoint_offset, HEADER_SIZE);
        assert!(header.metadata_offset > header.checkpoint_offset);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
