// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Fuzz target for the **JFR chunk / event wire format** (`jfr/src/dump.rs`).
//!
//! A `.jfr` recording is untrusted input the moment anything other than the
//! VM that wrote it opens one — `jdk.jfr.consumer.EventStream.openRepository`
//! and `RecordingFile` both land in this decoder. The format is
//! offset-driven (a 72-byte header whose `checkpoint_offset` /
//! `metadata_offset` fields steer every subsequent slice) and
//! varint-driven (record sizes, type ids, timestamps, and string lengths
//! are all LEB128 compressed ints), which is the classic shape for
//! out-of-bounds slicing and integer-overflow bugs.
//!
//! Surface under test:
//!   * `cratonvm_jfr::dump::{encode_compressed_int, encode_compressed_long,
//!     write_compressed_int_into, write_compressed_long_into,
//!     decode_compressed_int, decode_compressed_long}`
//!     (`jfr/src/dump.rs:114`, `:124`, `:162`, `:171`, `:190`, `:211`).
//!   * `cratonvm_jfr::read_jfr_header` (`jfr/src/dump.rs:1033`).
//!   * `cratonvm_jfr::read_events` (`jfr/src/dump.rs:1394`) — the offset
//!     walk, checkpoint constant-pool parse, per-record size accounting,
//!     and per-field value decode.
//!
//! Both file-reading entry points take a `&Path`, so the harness stages the
//! fuzzer's bytes in a single reused temp file. The file is truncated and
//! rewritten per iteration and removed at the end, so the harness holds no
//! growing state.
//!
//! Per-input layout:
//!   * bytes 0..  — the JFR chunk body. The harness prepends the 4-byte
//!     `FLR\0` magic so the fuzzer never has to rediscover it and every
//!     mutation lands in the header fields or the record stream. The same
//!     bytes also drive the varint round-trip phase.
//!
//! Assertions beyond panic-freedom:
//!   * varint encode → decode is the identity, and reports exactly the
//!     bytes it wrote;
//!   * a decoded varint never claims more than 10 bytes (a u64 cannot need
//!     more) and never more than the slice it was given;
//!   * a varint decode is independent of trailing bytes;
//!   * `read_events` never returns an event whose `type_id` is absent from
//!     the registry it was handed — with an empty registry that means it
//!     must return `Err` or an empty vector, never a defaulted event;
//!   * the number of decoded events is bounded by the file size, since the
//!     decoder itself requires every record to be at least 5 bytes.
//!
//! Run with:
//!   cargo +nightly fuzz run fuzz_jfr_chunk

#![no_main]

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;

use cratonvm_jfr::dump;
use cratonvm_jfr::event::{EventField, EventPeriod, EventType, EventTypeId, EventTypeRegistry};

/// `MAX_JFR_FILE_BYTES` in `jfr/src/dump.rs` is 2 GiB; that is a
/// production ceiling, not a useful fuzzing one. 1 MiB already admits tens
/// of thousands of records while keeping the per-iteration file write
/// cheap.
const MAX_INPUT: usize = 1024 * 1024;

/// `HEADER_SIZE` from `jfr/src/dump.rs:51`. Both file entry points reject
/// anything shorter before touching a single field, so smaller inputs only
/// exercise the length check.
const JFR_HEADER_SIZE: usize = 72;

/// The 4-byte magic the harness prepends (`jfr/src/dump.rs:23`).
const JFR_MAGIC: [u8; 4] = [b'F', b'L', b'R', 0];

/// A JFR compressed int carries 7 payload bits per byte, so a `u64` needs
/// at most 10 bytes. The decoder rejects anything longer
/// (`shift >= 64` guard); this target asserts that bound holds.
const MAX_VARINT_BYTES: usize = 10;

/// The decoder requires every event record to carry at least a size varint
/// plus four more header bytes, so a file can never hold more records than
/// a fifth of its length. Used as the event-count bound.
const MIN_EVENT_RECORD_BYTES: usize = 5;

/// Registry describing the event shapes the decoder may materialise.
///
/// Built once per process: `read_events` only takes `&EventTypeRegistry`,
/// and rebuilding it per iteration would allocate a dozen `String`s for no
/// coverage. Two types cover the scalar decode path and the string path
/// (inline tag-3 and checkpoint-pool tag-4).
fn registry() -> &'static EventTypeRegistry {
    static REGISTRY: OnceLock<EventTypeRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut r = EventTypeRegistry::new();
        r.register(EventType {
            id: EventTypeId(0),
            name: "FuzzScalar".to_string(),
            category: vec!["Fuzz".to_string()],
            description: "scalar field decode path".to_string(),
            fields: vec![
                EventField::new("l", "long", "long field"),
                EventField::new("i", "int", "int field"),
                EventField::new("b", "boolean", "boolean field"),
                EventField::new("f", "float", "float field"),
                EventField::new("d", "double", "double field"),
            ],
            has_thread: true,
            has_stacktrace: false,
            period: EventPeriod::None,
            threshold: None,
        });
        r.register(EventType {
            id: EventTypeId(0),
            name: "FuzzString".to_string(),
            category: vec!["Fuzz".to_string()],
            description: "string field decode path".to_string(),
            fields: vec![EventField::new("s", "string", "string field")],
            has_thread: true,
            has_stacktrace: false,
            period: EventPeriod::None,
            threshold: None,
        });
        r
    })
}

/// Path of the single staging file this target reuses. One per process so
/// concurrent `cargo fuzz` jobs do not collide.
fn staging_path() -> &'static PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        std::env::temp_dir().join(format!("cratonvm-fuzz-jfr-{}.jfr", std::process::id()))
    })
}

/// Assert the compressed-int codec is a bijection on the values it accepts
/// and that the decoder's reported length is exactly what the encoder
/// wrote.
fn assert_varint_round_trip(value: u64) {
    let encoded = dump::encode_compressed_int(value);
    assert!(
        encoded.len() <= MAX_VARINT_BYTES,
        "compressed int encoding of {value} took {} bytes",
        encoded.len()
    );

    // The Vec-appending variant must agree byte for byte with the
    // allocating one — they share an encoder core, and a divergence would
    // silently corrupt every hot-path record.
    let mut appended = Vec::with_capacity(MAX_VARINT_BYTES);
    dump::write_compressed_int_into(&mut appended, value);
    assert_eq!(
        appended, encoded,
        "write_compressed_int_into disagrees with encode_compressed_int for {value}"
    );

    match dump::decode_compressed_int(&encoded) {
        Some((decoded, consumed)) => {
            assert_eq!(decoded, value, "compressed int round trip changed the value");
            assert_eq!(
                consumed,
                encoded.len(),
                "compressed int decode consumed {consumed} of {} encoded bytes",
                encoded.len()
            );
        }
        None => panic!("compressed int decoder rejected its own encoding of {value}"),
    }
}

/// Same contract for the zigzag-encoded signed variant.
fn assert_varlong_round_trip(value: i64) {
    let encoded = dump::encode_compressed_long(value);
    assert!(
        encoded.len() <= MAX_VARINT_BYTES,
        "compressed long encoding of {value} took {} bytes",
        encoded.len()
    );

    let mut appended = Vec::with_capacity(MAX_VARINT_BYTES);
    dump::write_compressed_long_into(&mut appended, value);
    assert_eq!(
        appended, encoded,
        "write_compressed_long_into disagrees with encode_compressed_long for {value}"
    );

    match dump::decode_compressed_long(&encoded) {
        Some((decoded, consumed)) => {
            assert_eq!(
                decoded, value,
                "compressed long round trip changed the value"
            );
            assert_eq!(
                consumed,
                encoded.len(),
                "compressed long decode consumed {consumed} of {} encoded bytes",
                encoded.len()
            );
        }
        None => panic!("compressed long decoder rejected its own encoding of {value}"),
    }
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() || data.len() > MAX_INPUT {
        return;
    }

    // -------------------------------------------------------------------
    // Phase A — varint edge cases.
    //
    // Every 8-byte window of the input is read as a u64 and pushed through
    // the encoder and back, which reaches the 1-byte, 10-byte, and
    // shift-boundary cases far faster than waiting for the mutator to
    // stumble onto them. Capped at 8 windows so a 1 MiB input does not
    // turn into 128 k round trips — past the first few values this adds
    // allocations, not coverage.
    // -------------------------------------------------------------------
    for chunk in data.chunks_exact(8).take(8) {
        let raw = u64::from_le_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
        ]);
        assert_varint_round_trip(raw);
        assert_varlong_round_trip(raw as i64);
    }
    // Fixed boundary values, cheap enough to check unconditionally.
    for &v in &[0u64, 1, 0x7F, 0x80, u32::MAX as u64, u64::MAX] {
        assert_varint_round_trip(v);
    }
    for &v in &[0i64, -1, i64::MIN, i64::MAX] {
        assert_varlong_round_trip(v);
    }

    // Decoding raw fuzzer bytes: the decoder must stay inside the slice it
    // was given, never claim an impossible length, and never depend on
    // bytes past the varint it decoded.
    if let Some((value, consumed)) = dump::decode_compressed_int(data) {
        assert!(
            consumed <= MAX_VARINT_BYTES,
            "decode_compressed_int claimed {consumed} bytes for a u64"
        );
        assert!(
            consumed <= data.len(),
            "decode_compressed_int consumed {consumed} bytes from a {}-byte slice",
            data.len()
        );
        assert!(consumed > 0, "decode_compressed_int consumed nothing");
        assert_eq!(
            dump::decode_compressed_int(&data[..consumed]),
            Some((value, consumed)),
            "decode_compressed_int result depends on bytes past the varint"
        );
    }
    if let Some((_, consumed)) = dump::decode_compressed_long(data) {
        assert!(
            consumed <= MAX_VARINT_BYTES && consumed <= data.len() && consumed > 0,
            "decode_compressed_long reported an impossible length {consumed}"
        );
    }

    // -------------------------------------------------------------------
    // Phase B — whole-chunk decode. Skipped for inputs too short to form a
    // header; those only exercise the length guard, which Phase A already
    // covers far more cheaply.
    // -------------------------------------------------------------------
    if data.len() + JFR_MAGIC.len() < JFR_HEADER_SIZE {
        return;
    }

    let path = staging_path();
    {
        let Ok(mut f) = std::fs::File::create(path) else {
            return;
        };
        if f.write_all(&JFR_MAGIC).is_err() || f.write_all(data).is_err() || f.flush().is_err() {
            let _ = std::fs::remove_file(path);
            return;
        }
    }
    let file_len = JFR_MAGIC.len() + data.len();

    // Header parse: a successful parse must have matched the magic, since
    // that is the only thing the reader validates before returning.
    if let Ok(header) = cratonvm_jfr::read_jfr_header(path) {
        assert_eq!(
            header.magic, JFR_MAGIC,
            "read_jfr_header returned a header whose magic it should have rejected"
        );
    }

    // Full event walk against a populated registry.
    if let Ok(events) = cratonvm_jfr::read_events(path, registry()) {
        assert!(
            events.len() <= file_len / MIN_EVENT_RECORD_BYTES,
            "decoded {} events from a {file_len}-byte file, but the decoder requires \
             at least {MIN_EVENT_RECORD_BYTES} bytes per record",
            events.len()
        );
        for e in &events {
            assert!(
                registry().get(e.type_id).is_some(),
                "read_events materialised an event of unregistered type {}",
                e.type_id.0
            );
        }
    }

    // ...and against an empty registry, where the decoder is documented to
    // abort on the first event it sees. Any `Ok` must therefore be empty —
    // never a defaulted or field-less event.
    let empty_registry = EventTypeRegistry::new();
    if let Ok(events) = cratonvm_jfr::read_events(path, &empty_registry) {
        assert!(
            events.is_empty(),
            "read_events returned {} events against an empty registry",
            events.len()
        );
    }

    let _ = std::fs::remove_file(path);
});
