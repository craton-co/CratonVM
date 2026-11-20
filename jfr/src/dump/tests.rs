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
