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
