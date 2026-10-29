// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Writer for the **JDK's own** JFR chunk format — the one
//! `jdk.jfr.consumer.RecordingFile`, `jfr print`/`jfr summary` and JDK Mission
//! Control read.
//!
//! This is deliberately a separate module from [`crate::dump`]. That module
//! implements CratonVM's *internal* JFR-inspired format: a 72-byte header, a
//! zigzag compressed-long encoding, a bespoke checkpoint/metadata section, and
//! a matching Rust reader ([`crate::read_events`]) used by the phase reporter
//! and the `reader` crate. The two formats share the `FLR\0` magic and the
//! `2.x` version but agree on nothing after that, which is why a `.jfr` file
//! CratonVM produced used to fail in the JDK's parser with
//! `IOException: Unknown string encoding 17` — the reader's cursor was
//! mis-framed from the first record onward, so the byte where it expected a
//! string-encoding tag was some other field's payload.
//!
//! Everything below is derived from the JDK 25 sources, and each rule names the
//! class that enforces it so it can be re-checked against a newer JDK:
//!
//! * **Header is 68 bytes**, not 72 (`ChunkHeader.HEADER_SIZE`). The first 64
//!   bytes happen to have the same layout as CratonVM's internal header; the
//!   difference is the trailing `int` (file state at byte 64, flag byte at 67)
//!   where the internal format has `u8` + 7 bytes of padding.
//! * **`fileState == 0` means finished** (`ChunkHeader.refresh` sets
//!   `finished = fileState1 == 0`). The internal format uses `1` for complete,
//!   i.e. exactly inverted — on its own that made every chunk look like a live
//!   recording still being appended to.
//! * **`chunkSize` must equal the file size** or `ChunkHeader.isLastChunk()`
//!   never reports the last chunk and the parser walks off the end.
//! * **`metadataPosition != 0`** or `refresh()` throws
//!   "No metadata event found in finished chunk".
//! * **`constantPoolPosition == 0` is legal and means "no constant pools"**:
//!   `ChunkParser.fillConstantPools(0)` starts with
//!   `thisCP = constantPoolPosition + absoluteChunkStart` and loops
//!   `while (thisCP != abortCP …)` with `abortCP == 0`, so a zero position
//!   skips the pool walk entirely. This writer emits no checkpoint event and
//!   therefore no constant-pool-backed field.
//! * **Integers are unsigned LEB128 with a 9-byte cap** whose 9th byte carries
//!   8 raw bits (`RecordingInput.readLong`). NOT zigzag — the internal format's
//!   `write_compressed_long_into` is zigzag and cannot be reused here.
//! * **An event record is** `varint size` (self-inclusive) + `varint typeId` +
//!   `varint startTicks` + `varint durationTicks` + fields, and the parser
//!   resynchronises with `input.position(pos + size)`, so `size` must be exact
//!   (`ChunkParser.readEvent`, `EventParser.parse`).
//! * **Field order is positional**: `EventParser` reads field 0 as
//!   `startTime` and field 1 as `duration` when the type declares a field
//!   named `duration`, then the declared fields in metadata order.
//!
//! # What this writer does not model
//!
//! Event types are written with the fields `startTime`, `duration` and the
//! CratonVM-declared payload fields only — no `eventThread` and no
//! `stackTrace`. Both are constant-pool references in the JDK's format and
//! would require a checkpoint event; CratonVM records neither for the events
//! that reach this writer. Consumers see `RecordedEvent.getThread() == null`
//! and `getStackTrace() == null`, which is the same answer the JDK gives for
//! its own event types that omit those fields.

use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

use rustc_hash::FxHashMap;

use crate::dump::JfrDumpError;
use crate::event::{EventInstance, EventTypeId, EventTypeRegistry, EventValue, FieldKind};
use crate::repository::EventRepository;

/// `ChunkHeader.FILE_MAGIC`.
pub const JDK_FILE_MAGIC: [u8; 4] = [b'F', b'L', b'R', 0];
/// Chunk format major version. `ChunkHeader` accepts 1 and 2 and rejects
/// everything else.
pub const JDK_MAJOR: u16 = 2;
/// Chunk format minor version, matching what JDK 25 itself writes.
pub const JDK_MINOR: u16 = 1;
/// `ChunkHeader.HEADER_SIZE`.
pub const JDK_HEADER_SIZE: u64 = 68;
/// `ChunkHeader.METADATA_TYPE_ID`.
const METADATA_TYPE_ID: u64 = 0;
/// The metadata id carried by the single metadata event this writer emits.
/// Any non-zero value works; the parser only compares it against the previous
/// chunk's to decide whether to re-read the type table.
const METADATA_ID: u64 = 1;
/// Nanosecond tick resolution — `TimeConverter`'s divisor is
/// `ticksPerSecond / 1_000_000_000`, so this makes one tick one nanosecond.
const JDK_TICKS_PER_SECOND: u64 = 1_000_000_000;
/// `ChunkHeader.MASK_FINAL_CHUNK`, set in the flag byte at offset 67.
const FLAG_FINAL_CHUNK: u8 = 1 << 1;
/// `ChunkHeader.UPDATING_CHUNK_HEADER` — written into the file-state byte while
/// the chunk is incomplete so a reader that somehow sees the partial file waits
/// instead of parsing garbage.
const FILE_STATE_UPDATING: u8 = 255;
/// File state for a finished chunk (`ChunkHeader.refresh`: `finished == 0`).
const FILE_STATE_FINISHED: u8 = 0;

/// `StringParser.Encoding.NULL`.
const STRING_NULL: u8 = 0;
/// `StringParser.Encoding.EMPTY_STRING`.
const STRING_EMPTY: u8 = 1;
/// `StringParser.Encoding.UT8_BYTE_ARRAY`.
const STRING_UTF8: u8 = 3;

/// `Type.SUPER_TYPE_EVENT` — `MetadataReader.declareTypes` compares the
/// `superType` attribute against this exact string to decide that a `<class>`
/// element describes an event type rather than a plain value type.
const SUPER_TYPE_EVENT: &str = "jdk.jfr.Event";

// Chunk-local metadata type ids. They only have to be internally consistent:
// the metadata event declares them and every field references them by number.
// Ids 0 and 1 are reserved by the format for the metadata and checkpoint
// events, so nothing may be allocated there.
const TYPE_ID_BOOLEAN: u64 = 2;
const TYPE_ID_INT: u64 = 3;
const TYPE_ID_LONG: u64 = 4;
const TYPE_ID_FLOAT: u64 = 5;
const TYPE_ID_DOUBLE: u64 = 6;
const TYPE_ID_STRING: u64 = 7;
/// First id handed to an event type. Kept clear of the primitive block above.
const FIRST_EVENT_TYPE_ID: u64 = 100;

/// Append `value` using the JDK's compressed-integer encoding: unsigned
/// LEB128, at most 9 bytes, with the 9th byte carrying 8 raw bits.
///
/// This is the exact inverse of `RecordingInput.readLong`, which is also what
/// `readInt`/`readShort`/`readChar` funnel through — so one encoder covers
/// every varint-shaped value in the format.
pub fn write_varint(buf: &mut Vec<u8>, value: u64) {
    let mut v = value;
    for _ in 0..8 {
        if v < 0x80 {
            buf.push(v as u8);
            return;
        }
        buf.push(((v & 0x7F) as u8) | 0x80);
        v >>= 7;
    }
    // `readLong` reads the 9th byte raw, so the remaining 8 bits go out
    // unmasked and without a continuation bit.
    buf.push((v & 0xFF) as u8);
}

/// Byte length of [`write_varint`]'s encoding of `value`.
fn varint_len(value: u64) -> usize {
    let mut v = value;
    for n in 1..=8usize {
        if v < 0x80 {
            return n;
        }
        v >>= 7;
    }
    9
}

/// Prefix `body` with the self-inclusive `size` varint that
/// `ChunkParser.readEvent` uses to skip to the next record, and return the
/// complete record.
///
/// `size` counts its own encoding, so the length has to be solved for: adding
/// a byte to the size prefix can push the total across a 7-bit boundary and
/// lengthen the prefix again. Iterating to a fixed point is exact and
/// terminates in at most two steps.
fn size_prefixed(body: &[u8]) -> Vec<u8> {
    let mut prefix_len = varint_len(body.len() as u64 + 1);
    loop {
        let total = body.len() + prefix_len;
        let needed = varint_len(total as u64);
        if needed == prefix_len {
            let mut record = Vec::with_capacity(total);
            write_varint(&mut record, total as u64);
            debug_assert_eq!(record.len(), prefix_len);
            record.extend_from_slice(body);
            return record;
        }
        prefix_len = needed;
    }
}

// ---------------------------------------------------------------------------
// Metadata element tree
// ---------------------------------------------------------------------------

/// One node of the metadata element tree that `MetadataReader.createElement`
/// reconstructs: a pooled name, a list of pooled attribute key/value pairs and
/// child elements.
struct Element {
    name: String,
    attributes: Vec<(String, String)>,
    children: Vec<Element>,
}

impl Element {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            attributes: Vec::new(),
            children: Vec::new(),
        }
    }

    fn attr(mut self, name: &str, value: impl std::fmt::Display) -> Self {
        self.attributes.push((name.to_owned(), value.to_string()));
        self
    }

    fn child(mut self, child: Element) -> Self {
        self.children.push(child);
        self
    }

    fn collect_strings(&self, pool: &mut StringPool) {
        pool.intern(&self.name);
        for (key, value) in &self.attributes {
            pool.intern(key);
            pool.intern(value);
        }
        for child in &self.children {
            child.collect_strings(pool);
        }
    }

    fn write(&self, buf: &mut Vec<u8>, pool: &StringPool) {
        write_varint(buf, pool.index(&self.name) as u64);
        write_varint(buf, self.attributes.len() as u64);
        for (key, value) in &self.attributes {
            write_varint(buf, pool.index(key) as u64);
            write_varint(buf, pool.index(value) as u64);
        }
        write_varint(buf, self.children.len() as u64);
        for child in &self.children {
            child.write(buf, pool);
        }
    }
}

/// Insertion-ordered string pool for the metadata event. `MetadataReader`
/// reads the pool as a flat list and resolves every name/attribute by index,
/// so order only has to match between the two halves of this module.
#[derive(Default)]
struct StringPool {
    strings: Vec<String>,
    index: FxHashMap<String, usize>,
}

impl StringPool {
    fn intern(&mut self, s: &str) {
        if !self.index.contains_key(s) {
            self.index.insert(s.to_owned(), self.strings.len());
            self.strings.push(s.to_owned());
        }
    }

    fn index(&self, s: &str) -> usize {
        // Every string is interned by `collect_strings` before `write` runs;
        // a miss would mean the two passes disagree, which is a bug in this
        // module rather than anything the caller can cause.
        self.index
            .get(s)
            .copied()
            .expect("metadata string pool missing an interned string")
    }

    fn write(&self, buf: &mut Vec<u8>) {
        write_varint(buf, self.strings.len() as u64);
        for s in &self.strings {
            // `MetadataReader` builds its `StringParser` with a null constant
            // lookup, so the pool-reference encoding (2) is not available
            // here; the inline UTF-8 form is.
            buf.push(STRING_UTF8);
            let bytes = s.as_bytes();
            write_varint(buf, bytes.len() as u64);
            buf.extend_from_slice(bytes);
        }
    }
}

/// The metadata type id a [`FieldKind`] is written as.
fn kind_type_id(kind: FieldKind) -> u64 {
    match kind {
        FieldKind::Int => TYPE_ID_INT,
        FieldKind::Long => TYPE_ID_LONG,
        FieldKind::Float => TYPE_ID_FLOAT,
        FieldKind::Double => TYPE_ID_DOUBLE,
        FieldKind::Boolean => TYPE_ID_BOOLEAN,
        // `Null` is the runtime wildcard, never a declared field type; a
        // string slot is the one encoding that carries its own null tag.
        FieldKind::String | FieldKind::Null => TYPE_ID_STRING,
    }
}

/// The JFR type name for a primitive metadata type id.
fn primitive_type_name(id: u64) -> &'static str {
    match id {
        TYPE_ID_BOOLEAN => "boolean",
        TYPE_ID_INT => "int",
        TYPE_ID_LONG => "long",
        TYPE_ID_FLOAT => "float",
        TYPE_ID_DOUBLE => "double",
        // `ParserFactory.createPrimitiveParser` switches on exactly this
        // spelling to pick `StringParser`.
        _ => "java.lang.String",
    }
}

/// A writable event type: the chunk-local type id plus the field kinds, in the
/// order the payload writes them.
struct WritableType {
    chunk_type_id: u64,
    field_kinds: Vec<FieldKind>,
}

/// Build the metadata event body (everything `MetadataReader` consumes: the
/// string pool followed by the root element tree) for `types`, and return it
/// alongside the per-CratonVM-type layout the event writer needs.
fn build_metadata(
    types: &[(EventTypeId, &crate::event::EventType)],
) -> (Vec<u8>, FxHashMap<EventTypeId, WritableType>) {
    let mut metadata = Element::new("metadata");
    let mut layouts: FxHashMap<EventTypeId, WritableType> = FxHashMap::default();

    // Primitive value types first. `MetadataReader.defineTypes` resolves every
    // field's `class` attribute against the declared type table, so each
    // referenced primitive needs its own `<class>` element. Declaring the full
    // set unconditionally costs a few dozen bytes and removes an ordering
    // dependency between field scanning and type declaration.
    for id in [
        TYPE_ID_BOOLEAN,
        TYPE_ID_INT,
        TYPE_ID_LONG,
        TYPE_ID_FLOAT,
        TYPE_ID_DOUBLE,
        TYPE_ID_STRING,
    ] {
        metadata = metadata.child(
            Element::new("class")
                .attr("name", primitive_type_name(id))
                .attr("id", id),
        );
    }

    let mut next_type_id = FIRST_EVENT_TYPE_ID;
    for (registry_id, event_type) in types {
        let chunk_type_id = next_type_id;
        next_type_id += 1;

        let mut class = Element::new("class")
            .attr("name", &event_type.name)
            .attr("superType", SUPER_TYPE_EVENT)
            .attr("id", chunk_type_id);

        // `EventParser` reads field 0 as startTime and — because the type
        // declares a field literally named "duration" — field 1 as the
        // duration. Both are ticks, and both must come first.
        class = class
            .child(
                Element::new("field")
                    .attr("name", "startTime")
                    .attr("class", TYPE_ID_LONG),
            )
            .child(
                Element::new("field")
                    .attr("name", "duration")
                    .attr("class", TYPE_ID_LONG),
            );

        let mut field_kinds = Vec::with_capacity(event_type.fields.len());
        for field in &event_type.fields {
            // `writable_types` only admits types whose every field maps to a
            // known kind, so this cannot fall back silently.
            let kind = FieldKind::from_declared(&field.type_name).unwrap_or(FieldKind::String);
            field_kinds.push(kind);
            class = class.child(
                Element::new("field")
                    .attr("name", &field.name)
                    .attr("class", kind_type_id(kind)),
            );
        }

        metadata = metadata.child(class);
        layouts.insert(
            *registry_id,
            WritableType {
                chunk_type_id,
                field_kinds,
            },
        );
    }

    // `MetadataReader` reads `root.elements("region").getFirst()` without a
    // presence check, so the region element is mandatory. UTC with no DST is
    // the honest answer for a writer that stores absolute epoch nanoseconds.
    let root = Element::new("root").child(metadata).child(
        Element::new("region")
            .attr("locale", "")
            .attr("gmtOffset", 0)
            .attr("dst", 0),
    );

    let mut pool = StringPool::default();
    root.collect_strings(&mut pool);

    let mut body = Vec::with_capacity(4096);
    pool.write(&mut body);
    root.write(&mut body, &pool);
    (body, layouts)
}

/// Append one field value in the JDK's on-wire form for `declared`.
///
/// The declared kind, not the runtime variant, decides the width — that is the
/// whole point of the metadata contract, and it is what keeps a `Null` from
/// desynchronising the record.
fn write_field(buf: &mut Vec<u8>, value: &EventValue, declared: FieldKind) {
    match (declared, value) {
        // `readInt()` is `(int) readLong()`, so writing the value zero-extended
        // through u32 round-trips every i32 including negatives, in at most 5
        // bytes instead of the 9 a sign-extended i64 would need.
        (FieldKind::Int, EventValue::Int(v)) => write_varint(buf, *v as u32 as u64),
        (FieldKind::Int, EventValue::Long(v)) => write_varint(buf, *v as i32 as u32 as u64),
        (FieldKind::Long, EventValue::Long(v)) => write_varint(buf, *v as u64),
        (FieldKind::Long, EventValue::Int(v)) => write_varint(buf, *v as i64 as u64),
        (FieldKind::Float, EventValue::Float(v)) => {
            // `readFloat` is `intBitsToFloat(readRawInt())` — 4 raw big-endian
            // bytes, not a varint.
            buf.extend_from_slice(&v.to_bits().to_be_bytes());
        }
        (FieldKind::Double, EventValue::Double(v)) => {
            buf.extend_from_slice(&v.to_bits().to_be_bytes());
        }
        (FieldKind::Boolean, EventValue::Boolean(v)) => buf.push(u8::from(*v)),
        (FieldKind::String, _) => match value.as_str_bytes() {
            Some([]) => buf.push(STRING_EMPTY),
            Some(bytes) => {
                buf.push(STRING_UTF8);
                write_varint(buf, bytes.len() as u64);
                buf.extend_from_slice(bytes);
            }
            None => buf.push(STRING_NULL),
        },
        // Every remaining pair is a Null (the runtime wildcard) or a variant
        // the emit-time validator already rejected. Write the declared kind's
        // zero so the record stays exactly as long as the metadata says.
        (FieldKind::Int, _) | (FieldKind::Long, _) => write_varint(buf, 0),
        (FieldKind::Float, _) => buf.extend_from_slice(&0f32.to_bits().to_be_bytes()),
        (FieldKind::Double, _) => buf.extend_from_slice(&0f64.to_bits().to_be_bytes()),
        (FieldKind::Boolean, _) => buf.push(0),
        (FieldKind::Null, _) => buf.push(STRING_NULL),
    }
}

/// Write the 68-byte chunk header.
fn write_header<W: Write>(
    writer: &mut W,
    chunk_size: u64,
    constant_pool_position: u64,
    metadata_position: u64,
    start_nanos: u64,
    duration_nanos: u64,
    file_state: u8,
) -> io::Result<()> {
    writer.write_all(&JDK_FILE_MAGIC)?;
    writer.write_all(&JDK_MAJOR.to_be_bytes())?;
    writer.write_all(&JDK_MINOR.to_be_bytes())?;
    writer.write_all(&chunk_size.to_be_bytes())?;
    writer.write_all(&constant_pool_position.to_be_bytes())?;
    writer.write_all(&metadata_position.to_be_bytes())?;
    writer.write_all(&start_nanos.to_be_bytes())?;
    writer.write_all(&duration_nanos.to_be_bytes())?;
    // Event ticks are written relative to the chunk, so the chunk's own tick
    // origin is zero while `startNanos` above carries the epoch instant.
    // `TimeConverter.convertTimestamp(t) = startNanos + (t - startTicks)`.
    writer.write_all(&0u64.to_be_bytes())?;
    writer.write_all(&JDK_TICKS_PER_SECOND.to_be_bytes())?;
    // Bytes 64..68 are one `int` in the JDK's layout: the file-state byte at
    // 64 (`FILE_STATE_POSITION`) and the flag byte at 67
    // (`FLAG_BYTE_POSITION`); the two in between are unused.
    writer.write_all(&[file_state, 0, 0, FLAG_FINAL_CHUNK])?;
    Ok(())
}

/// Select the event types this writer can describe, in a stable order.
///
/// A type is writable when every declared field maps to a [`FieldKind`]. A
/// type with an unmappable field is dropped along with its events rather than
/// written with a field the reader would mis-frame — the same rule
/// [`crate::dump`] applies to its own format.
fn writable_types<'a>(
    events: &[&EventInstance],
    registry: &'a EventTypeRegistry,
) -> Vec<(EventTypeId, &'a crate::event::EventType)> {
    let mut seen: Vec<EventTypeId> = Vec::new();
    let mut out: Vec<(EventTypeId, &crate::event::EventType)> = Vec::new();
    for event in events {
        if seen.contains(&event.type_id) {
            continue;
        }
        seen.push(event.type_id);
        let Some(event_type) = registry.get(event.type_id) else {
            continue;
        };
        if event_type
            .fields
            .iter()
            .any(|f| FieldKind::from_declared(&f.type_name).is_none())
        {
            tracing::debug!(
                type_id = event.type_id.0,
                name = %event_type.name,
                "omitting JFR event type with an unmappable field type from the JDK-format chunk"
            );
            continue;
        }
        out.push((event.type_id, event_type));
    }
    out
}

/// Write `repository`'s events (plus `extra_events`) to `path` as a single
/// JDK-format chunk.
///
/// The signature mirrors [`crate::dump::dump_to_file`] so the two writers can
/// be swapped at a call site. Like that writer this one stages the bytes in a
/// sibling `.jfr.part` file and renames on success, so a reader never sees a
/// partial chunk. Returns the number of bytes written.
pub fn dump_to_file(
    path: &Path,
    repository: &EventRepository,
    registry: &EventTypeRegistry,
    start_time_ns: u64,
    duration_ns: u64,
    extra_events: Vec<EventInstance>,
    durable: bool,
) -> Result<u64, JfrDumpError> {
    let extra = extra_events;
    let mut chunk_events: Vec<&EventInstance> = Vec::with_capacity(repository.len() + extra.len());
    chunk_events.extend(repository.iter());
    chunk_events.extend(extra.iter());
    // A chunk's events must be monotonic in `startTime` for JMC's timeline and
    // for `EventStream`'s ordered mode; a stable sort keeps same-tick events in
    // emission order.
    chunk_events.sort_by_key(|e| e.start_time);

    // Event ticks are chunk-relative (see `write_header`), so the tick origin
    // has to be at or below every event's start or the subtraction would
    // saturate and silently move events forward in time.
    let chunk_start_nanos = chunk_events
        .first()
        .map_or(start_time_ns, |first| first.start_time.min(start_time_ns));

    let types = writable_types(&chunk_events, registry);
    let (metadata_body, layouts) = build_metadata(&types);

    let part_path = path.with_extension("jfr.part");

    /// Best-effort removal of the staging file on any early return.
    struct PartGuard<'a> {
        path: &'a Path,
        armed: bool,
    }
    impl PartGuard<'_> {
        fn disarm(&mut self) {
            self.armed = false;
        }
    }
    impl Drop for PartGuard<'_> {
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

    let chunk_size = {
        let file = std::fs::File::create(&part_path)?;
        let mut writer = io::BufWriter::new(file);

        write_header(
            &mut writer,
            0,
            0,
            0,
            chunk_start_nanos,
            duration_ns,
            FILE_STATE_UPDATING,
        )?;

        let mut body: Vec<u8> = Vec::with_capacity(256);
        for event in &chunk_events {
            let Some(layout) = layouts.get(&event.type_id) else {
                continue;
            };
            // A registered type whose event carries the wrong number of values
            // would shift every later field; drop the event, not the chunk.
            if event.fields.len() != layout.field_kinds.len() {
                tracing::debug!(
                    type_id = event.type_id.0,
                    expected = layout.field_kinds.len(),
                    actual = event.fields.len(),
                    "dropping JFR event with a field-count mismatch from the JDK-format chunk"
                );
                continue;
            }
            body.clear();
            write_varint(&mut body, layout.chunk_type_id);
            write_varint(
                &mut body,
                event.start_time.saturating_sub(chunk_start_nanos),
            );
            write_varint(&mut body, event.end_time.saturating_sub(event.start_time));
            for (value, kind) in event.fields.iter().zip(layout.field_kinds.iter()) {
                write_field(&mut body, value, *kind);
            }
            writer.write_all(&size_prefixed(&body))?;
        }

        let metadata_position = writer.stream_position()?;
        body.clear();
        write_varint(&mut body, METADATA_TYPE_ID);
        write_varint(&mut body, 0); // startTime, in chunk-relative ticks
        write_varint(&mut body, 0); // duration
        write_varint(&mut body, METADATA_ID);
        body.extend_from_slice(&metadata_body);
        writer.write_all(&size_prefixed(&body))?;

        let chunk_size = writer.stream_position()?;

        writer.seek(SeekFrom::Start(0))?;
        write_header(
            &mut writer,
            chunk_size,
            // No checkpoint event is written, and a zero position is how the
            // format says "there are no constant pools".
            0,
            metadata_position,
            chunk_start_nanos,
            duration_ns,
            FILE_STATE_FINISHED,
        )?;

        writer.flush()?;
        if durable {
            writer.get_ref().sync_all()?;
        }
        chunk_size
    };

    std::fs::rename(&part_path, path)?;
    guard.disarm();
    Ok(chunk_size)
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// A field value decoded from a JDK-format chunk.
#[derive(Debug, Clone, PartialEq)]
pub enum ChunkValue {
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Boolean(bool),
    /// A string field. `None` is the format's null-string tag.
    String(Option<String>),
}

/// One event record decoded from a JDK-format chunk.
#[derive(Debug, Clone)]
pub struct ChunkEvent {
    pub type_name: String,
    /// Chunk-relative ticks, i.e. nanoseconds since the chunk's `start_nanos`.
    pub start_ticks: u64,
    pub duration_ticks: u64,
    pub fields: Vec<(String, ChunkValue)>,
}

/// A JDK-format chunk, decoded.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub start_nanos: u64,
    pub duration_nanos: u64,
    pub events: Vec<ChunkEvent>,
}

/// Cursor over a chunk's bytes with the format's own integer decoding.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        Self { data, pos }
    }

    fn byte(&mut self) -> Result<u8, JfrDumpError> {
        let value = *self.data.get(self.pos).ok_or_else(truncated)?;
        self.pos += 1;
        Ok(value)
    }

    /// The inverse of [`write_varint`], transcribed from
    /// `RecordingInput.readLong` — including the 9th byte being read raw.
    fn varint(&mut self) -> Result<u64, JfrDumpError> {
        let mut result: u64 = 0;
        for shift in 0..8u32 {
            let byte = self.byte()?;
            result += u64::from(byte & 0x7F) << (7 * shift);
            if byte & 0x80 == 0 {
                return Ok(result);
            }
        }
        let byte = self.byte()?;
        Ok(result + (u64::from(byte) << 56))
    }

    fn raw(&mut self, len: usize) -> Result<&'a [u8], JfrDumpError> {
        let end = self.pos.checked_add(len).ok_or_else(truncated)?;
        let slice = self.data.get(self.pos..end).ok_or_else(truncated)?;
        self.pos = end;
        Ok(slice)
    }

    /// A string in any of the encodings this format uses inline.
    fn string(&mut self) -> Result<Option<String>, JfrDumpError> {
        match self.byte()? {
            STRING_NULL => Ok(None),
            STRING_EMPTY => Ok(Some(String::new())),
            STRING_UTF8 => {
                let len = usize::try_from(self.varint()?).map_err(|_| truncated())?;
                let bytes = self.raw(len)?;
                Ok(Some(String::from_utf8_lossy(bytes).into_owned()))
            }
            // 4 = CHAR_ARRAY, which is what the JDK's own metadata writer uses:
            // a varint length followed by one varint per UTF-16 code unit.
            4 => {
                let len = usize::try_from(self.varint()?).map_err(|_| truncated())?;
                let mut units = Vec::with_capacity(len);
                for _ in 0..len {
                    units.push(self.varint()? as u16);
                }
                Ok(Some(String::from_utf16_lossy(&units)))
            }
            other => Err(JfrDumpError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported inline string encoding {other}"),
            ))),
        }
    }
}

fn truncated() -> JfrDumpError {
    JfrDumpError::Io(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "JDK-format JFR chunk ended mid-record",
    ))
}

fn invalid(message: impl Into<String>) -> JfrDumpError {
    JfrDumpError::Io(io::Error::new(io::ErrorKind::InvalidData, message.into()))
}

/// A `<class>` element from the metadata event.
struct MetadataType {
    name: String,
    /// `(field name, referenced type id)`, in payload order.
    fields: Vec<(String, u64)>,
    is_event: bool,
}

/// One node of the metadata element tree, with strings already resolved.
struct RawElement {
    name: String,
    attributes: Vec<(String, String)>,
    children: Vec<RawElement>,
}

impl RawElement {
    fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a RawElement> {
        self.children.iter().filter(move |child| child.name == name)
    }
}

fn read_element(
    cursor: &mut Cursor<'_>,
    pool: &[Option<String>],
) -> Result<RawElement, JfrDumpError> {
    let pooled = |index: u64| -> Result<String, JfrDumpError> {
        pool.get(index as usize)
            .cloned()
            .flatten()
            .ok_or_else(|| invalid(format!("metadata string index {index} is out of range")))
    };
    let name = pooled(cursor.varint()?)?;
    let attribute_count = cursor.varint()?;
    let mut attributes = Vec::with_capacity(attribute_count as usize);
    for _ in 0..attribute_count {
        let key = pooled(cursor.varint()?)?;
        let value = pooled(cursor.varint()?)?;
        attributes.push((key, value));
    }
    let child_count = cursor.varint()?;
    let mut children = Vec::with_capacity(child_count as usize);
    for _ in 0..child_count {
        children.push(read_element(cursor, pool)?);
    }
    Ok(RawElement {
        name,
        attributes,
        children,
    })
}

/// Decode a JDK-format chunk written by [`dump_to_file`].
///
/// This exists so the writer's output can be asserted on *content* from a plain
/// `cargo test`, where no JDK is available to run `RecordingFile` — and so the
/// two halves are not the only check on each other: the writer is independently
/// validated against the JDK's own parser, and this reader is validated against
/// the writer.
///
/// Only what [`dump_to_file`] emits is supported. In particular there is no
/// constant-pool/checkpoint handling and no struct or array field support, so
/// this will not read an arbitrary HotSpot recording — for that, use the JDK.
pub fn read_chunk(path: &Path) -> Result<Chunk, JfrDumpError> {
    let len = std::fs::metadata(path)?.len();
    if len > crate::dump::MAX_JFR_FILE_BYTES {
        return Err(invalid(format!(
            "JFR file is {len} bytes, exceeds maximum of {} bytes",
            crate::dump::MAX_JFR_FILE_BYTES
        )));
    }
    let data = std::fs::read(path)?;
    if data.len() < JDK_HEADER_SIZE as usize {
        return Err(invalid("file is smaller than a JDK chunk header"));
    }
    if data[0..4] != JDK_FILE_MAGIC {
        return Err(invalid("not a Flight Recorder file"));
    }
    let major = u16::from_be_bytes([data[4], data[5]]);
    if major != 1 && major != 2 {
        return Err(invalid(format!("unsupported chunk major version {major}")));
    }
    let chunk_size = u64::from_be_bytes(data[8..16].try_into().unwrap());
    let metadata_position = u64::from_be_bytes(data[24..32].try_into().unwrap());
    let start_nanos = u64::from_be_bytes(data[32..40].try_into().unwrap());
    let duration_nanos = u64::from_be_bytes(data[40..48].try_into().unwrap());
    if chunk_size != data.len() as u64 {
        return Err(invalid(format!(
            "chunkSize {chunk_size} disagrees with the file size {}",
            data.len()
        )));
    }
    if metadata_position == 0 || metadata_position >= chunk_size {
        return Err(invalid(format!(
            "metadataPosition {metadata_position} is outside the chunk"
        )));
    }

    // --- metadata event ---------------------------------------------------
    let mut cursor = Cursor::new(&data, metadata_position as usize);
    cursor.varint()?; // size
    let type_id = cursor.varint()?;
    if type_id != METADATA_TYPE_ID {
        return Err(invalid(format!(
            "expected the metadata event at metadataPosition, found type id {type_id}"
        )));
    }
    cursor.varint()?; // start time
    cursor.varint()?; // duration
    cursor.varint()?; // metadata id
    let pool_size = cursor.varint()?;
    let mut pool: Vec<Option<String>> = Vec::with_capacity(pool_size as usize);
    for _ in 0..pool_size {
        pool.push(cursor.string()?);
    }
    let root = read_element(&mut cursor, &pool)?;
    let metadata = root
        .children_named("metadata")
        .next()
        .ok_or_else(|| invalid("metadata event has no <metadata> element"))?;

    let mut types: FxHashMap<u64, MetadataType> = FxHashMap::default();
    for class in metadata.children_named("class") {
        let name = class
            .attribute("name")
            .ok_or_else(|| invalid("a <class> element has no name"))?
            .to_owned();
        let id: u64 = class
            .attribute("id")
            .and_then(|id| id.parse().ok())
            .ok_or_else(|| invalid(format!("<class name=\"{name}\"> has no usable id")))?;
        let is_event = class.attribute("superType") == Some(SUPER_TYPE_EVENT);
        let mut fields = Vec::new();
        for field in class.children_named("field") {
            let field_name = field
                .attribute("name")
                .ok_or_else(|| invalid("a <field> element has no name"))?
                .to_owned();
            let field_type: u64 = field
                .attribute("class")
                .and_then(|id| id.parse().ok())
                .ok_or_else(|| invalid(format!("field {field_name} has no usable type id")))?;
            fields.push((field_name, field_type));
        }
        types.insert(
            id,
            MetadataType {
                name,
                fields,
                is_event,
            },
        );
    }

    // --- event records ----------------------------------------------------
    let mut events = Vec::new();
    let mut position = JDK_HEADER_SIZE as usize;
    while (position as u64) < chunk_size {
        let mut cursor = Cursor::new(&data, position);
        let size = usize::try_from(cursor.varint()?).map_err(|_| truncated())?;
        if size == 0 {
            return Err(invalid("an event may not have zero size"));
        }
        let type_id = cursor.varint()?;
        let next = position
            .checked_add(size)
            .filter(|next| *next <= data.len())
            .ok_or_else(truncated)?;
        if let Some(event_type) = types.get(&type_id).filter(|ty| ty.is_event) {
            let start_ticks = cursor.varint()?;
            // `startTime` is field 0 and `duration` field 1 by position; the
            // writer always declares both, so both are always present.
            let duration_ticks = cursor.varint()?;
            let mut fields = Vec::new();
            for (field_name, field_type) in event_type.fields.iter().skip(2) {
                let type_name = types
                    .get(field_type)
                    .map(|ty| ty.name.as_str())
                    .ok_or_else(|| {
                        invalid(format!("field type id {field_type} is not declared"))
                    })?;
                let value = match type_name {
                    "int" => ChunkValue::Int(cursor.varint()? as u32 as i32),
                    "long" => ChunkValue::Long(cursor.varint()? as i64),
                    "boolean" => ChunkValue::Boolean(cursor.byte()? != 0),
                    "float" => ChunkValue::Float(f32::from_be_bytes(
                        cursor.raw(4)?.try_into().map_err(|_| truncated())?,
                    )),
                    "double" => ChunkValue::Double(f64::from_be_bytes(
                        cursor.raw(8)?.try_into().map_err(|_| truncated())?,
                    )),
                    "java.lang.String" => ChunkValue::String(cursor.string()?),
                    other => {
                        return Err(invalid(format!(
                            "field {field_name} has unsupported type {other}"
                        )))
                    }
                };
                fields.push((field_name.clone(), value));
            }
            if cursor.pos != next {
                return Err(invalid(format!(
                    "event of type {} consumed {} bytes of a {size}-byte record",
                    event_type.name,
                    cursor.pos - position
                )));
            }
            events.push(ChunkEvent {
                type_name: event_type.name.clone(),
                start_ticks,
                duration_ticks,
                fields,
            });
        }
        position = next;
    }

    Ok(Chunk {
        start_nanos,
        duration_nanos,
        events,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventField, EventPeriod, EventType};
    use smallvec::smallvec;

    /// The encoder is the inverse of `RecordingInput.readLong`; decode with a
    /// transcription of that method so a test failure means the encoder is
    /// wrong rather than that both halves drifted together.
    fn read_varint(data: &[u8], pos: &mut usize) -> u64 {
        let mut ret: u64 = 0;
        for i in 0..8u32 {
            let b = data[*pos];
            *pos += 1;
            ret += ((b & 0x7F) as u64) << (7 * i);
            if b & 0x80 == 0 {
                return ret;
            }
        }
        let b = data[*pos];
        *pos += 1;
        ret + ((b as u64) << 56)
    }

    #[test]
    fn varint_matches_the_jdk_reader_for_boundary_values() {
        for value in [
            0u64,
            1,
            0x7F,
            0x80,
            300,
            0x3FFF,
            0x4000,
            u32::MAX as u64,
            i64::MAX as u64,
            u64::MAX,
        ] {
            let mut buf = Vec::new();
            write_varint(&mut buf, value);
            assert_eq!(buf.len(), varint_len(value), "length disagrees for {value}");
            let mut pos = 0;
            assert_eq!(read_varint(&buf, &mut pos), value, "round trip for {value}");
            assert_eq!(pos, buf.len(), "trailing bytes for {value}");
        }
    }

    /// 127 is the last single-byte value and 128 the first two-byte one — the
    /// exact bytes are pinned because the whole format hangs off this encoding.
    #[test]
    fn varint_encodes_the_documented_bytes() {
        let mut buf = Vec::new();
        write_varint(&mut buf, 127);
        assert_eq!(buf, vec![0x7F]);
        buf.clear();
        write_varint(&mut buf, 128);
        assert_eq!(buf, vec![0x80, 0x01]);
        buf.clear();
        // -1 as a long: all 64 bits set, i.e. the 9-byte form with the last
        // byte raw.
        write_varint(&mut buf, u64::MAX);
        assert_eq!(
            buf,
            vec![0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]
        );
    }

    #[test]
    fn size_prefix_is_self_inclusive_and_exact() {
        for body_len in [0usize, 1, 100, 125, 126, 127, 128, 200, 16_500] {
            let body = vec![0u8; body_len];
            let record = size_prefixed(&body);
            let mut pos = 0;
            let size = read_varint(&record, &mut pos);
            assert_eq!(
                size as usize,
                record.len(),
                "size must equal the whole record for body_len={body_len}"
            );
        }
    }

    fn registry_with_one_type() -> (EventTypeRegistry, EventTypeId) {
        let mut registry = EventTypeRegistry::new();
        let id = registry.register(EventType {
            id: EventTypeId::INVALID,
            name: "ProbeEvent".to_owned(),
            category: vec!["Test".to_owned()],
            description: "probe".to_owned(),
            fields: vec![
                EventField::new("capacity", "int", "size"),
                EventField::new("label", "string", "text"),
                EventField::new("pooled", "boolean", "flag"),
                EventField::new("stamp", "long", "when"),
                EventField::new("ratio", "double", "fraction"),
            ],
            has_thread: true,
            has_stacktrace: false,
            period: EventPeriod::BeginEnd,
            threshold: None,
        });
        (registry, id)
    }

    /// The invariants `ChunkHeader` enforces, asserted on the bytes rather
    /// than through a Rust reader that could share the writer's mistakes.
    #[test]
    fn header_satisfies_the_jdk_chunk_invariants() {
        let dir = std::env::temp_dir().join(format!("jfrk-hdr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h.jfr");
        let (registry, type_id) = registry_with_one_type();
        let mut repo = EventRepository::new(16);
        repo.push(EventInstance {
            type_id,
            start_time: 5_000,
            end_time: 5_500,
            thread_id: 1,
            fields: smallvec![
                EventValue::Int(4096),
                EventValue::from_str("direct"),
                EventValue::Boolean(true),
                EventValue::Long(-7),
                EventValue::Double(0.75),
            ],
        });
        let written = dump_to_file(&path, &repo, &registry, 5_000, 500, Vec::new(), false).unwrap();
        let bytes = std::fs::read(&path).unwrap();

        assert_eq!(&bytes[0..4], &JDK_FILE_MAGIC);
        assert_eq!(u16::from_be_bytes([bytes[4], bytes[5]]), JDK_MAJOR);
        let chunk_size = u64::from_be_bytes(bytes[8..16].try_into().unwrap());
        assert_eq!(
            chunk_size,
            bytes.len() as u64,
            "chunkSize must be the file size"
        );
        assert_eq!(chunk_size, written);
        assert_eq!(
            u64::from_be_bytes(bytes[16..24].try_into().unwrap()),
            0,
            "no checkpoint event is written, so constantPoolPosition must be 0"
        );
        let metadata_position = u64::from_be_bytes(bytes[24..32].try_into().unwrap());
        assert!(metadata_position >= JDK_HEADER_SIZE && metadata_position < chunk_size);
        assert_eq!(
            u64::from_be_bytes(bytes[48..56].try_into().unwrap()),
            0,
            "event ticks are chunk-relative, so startTicks must be 0"
        );
        assert_eq!(
            u64::from_be_bytes(bytes[56..64].try_into().unwrap()),
            JDK_TICKS_PER_SECOND
        );
        assert_eq!(bytes[64], FILE_STATE_FINISHED, "0 means finished, not 1");
        assert_eq!(bytes[67] & FLAG_FINAL_CHUNK, FLAG_FINAL_CHUNK);

        // Walking the records with the format's own size prefixes must land
        // exactly on the metadata event and then exactly on the end of file.
        let mut pos = JDK_HEADER_SIZE as usize;
        let mut event_records = 0;
        let mut metadata_seen = false;
        while pos < bytes.len() {
            let record_start = pos;
            let size = read_varint(&bytes, &mut pos) as usize;
            assert!(size > 0, "an event may not have zero size");
            let type_id = read_varint(&bytes, &mut pos);
            if type_id == METADATA_TYPE_ID {
                assert_eq!(
                    record_start as u64, metadata_position,
                    "the metadata event must sit at metadataPosition"
                );
                metadata_seen = true;
            } else {
                event_records += 1;
            }
            pos = record_start + size;
        }
        assert_eq!(pos, bytes.len(), "record sizes must tile the chunk exactly");
        assert!(metadata_seen);
        assert_eq!(event_records, 1);
        std::fs::remove_file(&path).ok();
    }

    /// An empty recording still has to produce a chunk the JDK can open: the
    /// doc that prompted this writer records that an empty dump was already
    /// unreadable, which is what localised the defect to the chunk framing.
    #[test]
    fn an_empty_recording_still_writes_a_parseable_chunk() {
        let dir = std::env::temp_dir().join(format!("jfrk-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("e.jfr");
        let registry = EventTypeRegistry::new();
        let repo = EventRepository::new(4);
        dump_to_file(&path, &repo, &registry, 1_000, 0, Vec::new(), false).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let chunk_size = u64::from_be_bytes(bytes[8..16].try_into().unwrap());
        assert_eq!(chunk_size, bytes.len() as u64);
        let metadata_position = u64::from_be_bytes(bytes[24..32].try_into().unwrap());
        assert_eq!(
            metadata_position, JDK_HEADER_SIZE,
            "with no events the metadata event follows the header directly"
        );
        assert_ne!(
            metadata_position, 0,
            "a zero metadataPosition is rejected as a truncated chunk"
        );
        std::fs::remove_file(&path).ok();
    }

    /// A `Null` in a fixed-width slot must occupy the declared width. This is
    /// the failure mode that turns one bad field into an unreadable chunk.
    #[test]
    fn null_values_keep_the_declared_field_width() {
        for (kind, expected_len) in [
            (FieldKind::Int, 1),
            (FieldKind::Long, 1),
            (FieldKind::Float, 4),
            (FieldKind::Double, 8),
            (FieldKind::Boolean, 1),
            (FieldKind::String, 1),
        ] {
            let mut buf = Vec::new();
            write_field(&mut buf, &EventValue::Null, kind);
            assert_eq!(buf.len(), expected_len, "null width for {kind:?}");
        }
    }

    /// Full round trip through [`read_chunk`], on every field type the writer
    /// supports plus the awkward values: negatives, a null string, an empty
    /// string, and two event types in one chunk.
    #[test]
    fn every_field_type_round_trips_through_the_chunk_reader() {
        let dir = std::env::temp_dir().join(format!("jfrk-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rt.jfr");
        let (registry, probe) = registry_with_one_type();
        let mut registry = registry;
        let other = registry.register(EventType {
            id: EventTypeId::INVALID,
            name: "io.netty.AllocateChunk".to_owned(),
            category: vec!["Test".to_owned()],
            description: String::new(),
            fields: vec![EventField::new("pooled", "boolean", "")],
            has_thread: false,
            has_stacktrace: false,
            period: EventPeriod::None,
            threshold: None,
        });
        let mut repo = EventRepository::new(16);
        let base = 1_760_000_000_000_000_000u64;
        repo.push(EventInstance {
            type_id: probe,
            start_time: base,
            end_time: base + 500,
            thread_id: 1,
            fields: smallvec![
                EventValue::Int(4096),
                EventValue::from_str("direct"),
                EventValue::Boolean(true),
                EventValue::Long(1_234_567_890_123),
                EventValue::Double(0.75),
            ],
        });
        repo.push(EventInstance {
            type_id: probe,
            start_time: base + 1_000,
            end_time: base + 1_000,
            thread_id: 1,
            fields: smallvec![
                EventValue::Int(-7),
                EventValue::Null,
                EventValue::Boolean(false),
                EventValue::Long(-9_000_000_000),
                EventValue::Double(-1.5),
            ],
        });
        repo.push(EventInstance {
            type_id: probe,
            start_time: base + 2_000,
            end_time: base + 2_000,
            thread_id: 1,
            fields: smallvec![
                EventValue::Int(i32::MAX),
                EventValue::from_str(""),
                EventValue::Boolean(true),
                EventValue::Long(i64::MIN),
                EventValue::Double(0.0),
            ],
        });
        repo.push(EventInstance {
            type_id: other,
            start_time: base + 3_000,
            end_time: base + 3_000,
            thread_id: 2,
            fields: smallvec![EventValue::Boolean(true)],
        });
        dump_to_file(&path, &repo, &registry, base, 3_000, Vec::new(), false).unwrap();

        let chunk = read_chunk(&path).expect("the writer's own output must be readable");
        assert_eq!(chunk.start_nanos, base);
        assert_eq!(chunk.events.len(), 4);

        let first = &chunk.events[0];
        assert_eq!(first.type_name, "ProbeEvent");
        assert_eq!(first.start_ticks, 0);
        assert_eq!(first.duration_ticks, 500);
        assert_eq!(
            first.fields,
            vec![
                ("capacity".to_owned(), ChunkValue::Int(4096)),
                (
                    "label".to_owned(),
                    ChunkValue::String(Some("direct".to_owned()))
                ),
                ("pooled".to_owned(), ChunkValue::Boolean(true)),
                ("stamp".to_owned(), ChunkValue::Long(1_234_567_890_123)),
                ("ratio".to_owned(), ChunkValue::Double(0.75)),
            ]
        );

        let second = &chunk.events[1];
        assert_eq!(second.start_ticks, 1_000);
        assert_eq!(second.fields[0].1, ChunkValue::Int(-7));
        // A `Null` in a string slot round-trips as the format's null string;
        // in a numeric slot it round-trips as that type's zero.
        assert_eq!(second.fields[1].1, ChunkValue::String(None));
        assert_eq!(second.fields[3].1, ChunkValue::Long(-9_000_000_000));
        assert_eq!(second.fields[4].1, ChunkValue::Double(-1.5));

        let third = &chunk.events[2];
        assert_eq!(third.fields[0].1, ChunkValue::Int(i32::MAX));
        assert_eq!(third.fields[1].1, ChunkValue::String(Some(String::new())));
        assert_eq!(third.fields[3].1, ChunkValue::Long(i64::MIN));

        let fourth = &chunk.events[3];
        assert_eq!(fourth.type_name, "io.netty.AllocateChunk");
        assert_eq!(
            fourth.fields,
            vec![("pooled".to_owned(), ChunkValue::Boolean(true))]
        );
        std::fs::remove_file(&path).ok();
    }

    /// An event type with no payload fields at all: the record is just its two
    /// implicit tick fields. `RecordedEvent.objectAt` decides how far to shift
    /// its indices from `objects.length + 2 == fields.size()`, so a zero-length
    /// payload is the boundary case for that arithmetic — and several of
    /// CratonVM's own built-in types declare no fields.
    #[test]
    fn an_event_type_with_no_payload_fields_round_trips() {
        let dir = std::env::temp_dir().join(format!("jfrk-nofield-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nofield.jfr");
        let mut registry = EventTypeRegistry::new();
        let bare = registry.register(EventType {
            id: EventTypeId::INVALID,
            name: "cratonvm.Bare".to_owned(),
            category: vec!["Test".to_owned()],
            description: String::new(),
            fields: Vec::new(),
            has_thread: false,
            has_stacktrace: false,
            period: EventPeriod::None,
            threshold: None,
        });
        let mut repo = EventRepository::new(4);
        repo.push(EventInstance {
            type_id: bare,
            start_time: 9_000,
            end_time: 9_250,
            thread_id: 1,
            fields: smallvec![],
        });
        dump_to_file(&path, &repo, &registry, 9_000, 250, Vec::new(), false).unwrap();
        let chunk = read_chunk(&path).expect("a field-less event type must round trip");
        assert_eq!(chunk.events.len(), 1);
        assert_eq!(chunk.events[0].type_name, "cratonvm.Bare");
        assert_eq!(chunk.events[0].duration_ticks, 250);
        assert!(chunk.events[0].fields.is_empty());
        std::fs::remove_file(&path).ok();
    }

    /// The reader must reject the framing mistakes that made the pre-fix format
    /// unreadable, rather than returning a plausible-looking empty chunk.
    #[test]
    fn reader_rejects_a_chunk_whose_header_lies() {
        let dir = std::env::temp_dir().join(format!("jfrk-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.jfr");
        let (registry, _) = registry_with_one_type();
        let repo = EventRepository::new(4);
        dump_to_file(&path, &repo, &registry, 1_000, 0, Vec::new(), false).unwrap();
        let good = std::fs::read(&path).unwrap();

        // A zero metadataPosition is what `ChunkHeader.refresh` treats as a
        // truncated chunk.
        let mut no_metadata = good.clone();
        no_metadata[24..32].copy_from_slice(&0u64.to_be_bytes());
        std::fs::write(&path, &no_metadata).unwrap();
        assert!(
            read_chunk(&path).is_err(),
            "a zero metadataPosition must be rejected"
        );

        // A chunkSize that disagrees with the file size is what stops
        // `isLastChunk()` from ever terminating.
        let mut wrong_size = good.clone();
        wrong_size[8..16].copy_from_slice(&(good.len() as u64 + 16).to_be_bytes());
        std::fs::write(&path, &wrong_size).unwrap();
        assert!(
            read_chunk(&path).is_err(),
            "a wrong chunkSize must be rejected"
        );

        std::fs::write(&path, &good).unwrap();
        assert!(
            read_chunk(&path).is_ok(),
            "the unmodified chunk must still read"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn negative_ints_and_longs_round_trip_through_the_reader_encoding() {
        let mut buf = Vec::new();
        write_field(&mut buf, &EventValue::Int(-5), FieldKind::Int);
        let mut pos = 0;
        assert_eq!(read_varint(&buf, &mut pos) as u32 as i32, -5);
        buf.clear();
        write_field(&mut buf, &EventValue::Long(-5), FieldKind::Long);
        pos = 0;
        assert_eq!(read_varint(&buf, &mut pos) as i64, -5);
    }
}
