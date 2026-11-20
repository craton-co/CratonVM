use super::*;

// ---- Unit tests for helper functions --------------------------------

/// gc-common w5-e (`common-w4e-object-size-reports-legacy-footprint`): a
/// class with a registered compact layout is dumped at `HEADER_SIZE +
/// compact body`; anything the layout does not describe keeps the legacy
/// `HEADER_SIZE + fields * SLOT_SIZE`, byte for byte.
#[test]
fn class_instance_size_reports_the_compact_body() {
    use cratonvm_types::{FieldStorageKind, HEADER_SIZE, SLOT_SIZE};
    // A fresh domain and a class id far above any test VM's class count,
    // so the process-global layout registry has no other owner for it.
    const CID: u32 = 200_003;
    let domain = cratonvm_types::next_layout_domain();
    cratonvm_types::register_class_layout(
        domain,
        CID,
        Arc::new(cratonvm_types::CompactLayout {
            field_disps: (8..16).collect(),
            is_ref: vec![false; 8],
            field_kinds: vec![FieldStorageKind::Boolean; 8],
            ref_disps: Vec::new(),
            total_size: 16,
        }),
    );

    let legacy = HEADER_SIZE + 8 * SLOT_SIZE;
    let expected = if cratonvm_types::compact_ref_fields_enabled() {
        cratonvm_types::COMPACT_HEADER_SIZE + 8
    } else {
        legacy
    };
    assert_eq!(
        class_instance_size(domain, ClassId::new(CID), 8),
        expected,
        "eight booleans: a packed 8-byte body, not 8 * 16"
    );
    assert_eq!(
        class_instance_size(domain, ClassId::new(CID), 3),
        HEADER_SIZE + 3 * SLOT_SIZE,
        "a field count the layout does not describe is legacy"
    );
    assert_eq!(
        class_instance_size(domain, ClassId::new(CID + 1), 2),
        HEADER_SIZE + 2 * SLOT_SIZE,
        "a class with no layout is legacy"
    );
    cratonvm_types::unregister_class_layout(CID);
}

#[test]
fn s42_descriptor_to_hprof_type() {
    assert_eq!(descriptor_to_hprof_type("I"), HPROF_INT);
    assert_eq!(descriptor_to_hprof_type("J"), HPROF_LONG);
    assert_eq!(descriptor_to_hprof_type("B"), HPROF_BYTE);
    assert_eq!(descriptor_to_hprof_type("C"), HPROF_CHAR);
    assert_eq!(descriptor_to_hprof_type("D"), HPROF_DOUBLE);
    assert_eq!(descriptor_to_hprof_type("F"), HPROF_FLOAT);
    assert_eq!(descriptor_to_hprof_type("S"), HPROF_SHORT);
    assert_eq!(descriptor_to_hprof_type("Z"), HPROF_BOOLEAN);
    assert_eq!(descriptor_to_hprof_type("Ljava/lang/String;"), HPROF_OBJECT);
    assert_eq!(descriptor_to_hprof_type("[I"), HPROF_OBJECT);
}

#[test]
fn s42_array_element_to_hprof() {
    assert_eq!(
        array_element_to_hprof(ArrayElementType::Boolean),
        HPROF_BOOLEAN
    );
    assert_eq!(array_element_to_hprof(ArrayElementType::Int), HPROF_INT);
    assert_eq!(array_element_to_hprof(ArrayElementType::Long), HPROF_LONG);
    assert_eq!(
        array_element_to_hprof(ArrayElementType::Double),
        HPROF_DOUBLE
    );
    assert_eq!(
        array_element_to_hprof(ArrayElementType::Reference),
        HPROF_OBJECT
    );
}

#[test]
fn s42_hprof_type_sizes() {
    assert_eq!(hprof_type_size(HPROF_BOOLEAN), 1);
    assert_eq!(hprof_type_size(HPROF_BYTE), 1);
    assert_eq!(hprof_type_size(HPROF_CHAR), 2);
    assert_eq!(hprof_type_size(HPROF_SHORT), 2);
    assert_eq!(hprof_type_size(HPROF_INT), 4);
    assert_eq!(hprof_type_size(HPROF_FLOAT), 4);
    assert_eq!(hprof_type_size(HPROF_LONG), 8);
    assert_eq!(hprof_type_size(HPROF_DOUBLE), 8);
    assert_eq!(hprof_type_size(HPROF_OBJECT), 8);
}

#[test]
fn s42_class_obj_id_deterministic() {
    let id1 = class_obj_id_for(ClassId::new(1));
    let id2 = class_obj_id_for(ClassId::new(2));
    assert_ne!(id1, id2);
    assert_eq!(id1, CLASS_OBJ_ID_BASE + 1);
    assert_eq!(id2, CLASS_OBJ_ID_BASE + 2);
}

// ---- Observability audit (2026-07-26) regression coverage -----------

/// `array_class_name_for` must produce the internal-form array class
/// name so `write_obj_array_dump` can find the real array class in the
/// class store instead of labelling `String[]` as `java.lang.String`.
#[test]
fn obsaudit_array_class_name_for_internal_form() {
    assert_eq!(
        array_class_name_for("java/lang/String"),
        "[Ljava/lang/String;"
    );
    assert_eq!(
        array_class_name_for("java/lang/Object"),
        "[Ljava/lang/Object;"
    );
    // Nested arrays prepend a single `[` rather than re-wrapping in `L..;`
    assert_eq!(
        array_class_name_for("[Ljava/lang/String;"),
        "[[Ljava/lang/String;"
    );
    assert_eq!(array_class_name_for("[I"), "[[I");
}

/// CLASS_DUMP's instance-size slot must describe CratonVM's real object
/// footprint (for a LEGACY class, a `HEADER_SIZE` header + one 16-byte cell
/// per instance field; a compact class is covered by
/// `class_instance_size_reports_the_compact_body`), not HotSpot's packed
/// field-byte sum. Guards the fix for MAT reporting an
/// eight-`boolean` class as 8 bytes when it occupies `HEADER_SIZE + 128`.
#[test]
fn obsaudit_instance_byte_size_uses_real_footprint() {
    // Direct arithmetic check of the size formula the dumper now writes.
    // A class with N instance fields occupies HEADER_SIZE + N*SLOT_SIZE,
    // regardless of the fields' Java widths.
    let header = cratonvm_types::HEADER_SIZE;
    let slot = cratonvm_types::SLOT_SIZE;
    // 32 -> 24 when `forwarding_ptr` folded into the mark word, 24 -> 16
    // when the identity hash followed it and the kind/element_type/gc_age/
    // gc_flags quartet joined them in the mark word's bits 48..62. The
    // tripwire has now done its job twice — each time naming the dumper
    // doc above that had to move with it — so it keeps its literal rather
    // than becoming self-proving.
    assert_eq!(
        header, 16,
        "object header size changed; update the dumper doc"
    );
    assert_eq!(slot, 16, "field slot size changed; update the dumper doc");

    // The old (wrong) computation summed hprof_type_size, which would
    // give 8 bytes for eight booleans. The new one must not.
    let old_style: usize = (0..8).map(|_| hprof_type_size(HPROF_BOOLEAN)).sum();
    assert_eq!(old_style, 8);
    let new_style = header + 8 * slot;
    assert_eq!(new_style, header + 128);
    assert_ne!(old_style, new_style);
}

/// INSTANCE_DUMP field values are emitted leaf-class-first per the HPROF
/// spec ("this class, followed by super class"). This exercises the
/// grouping/reversal logic directly: heap slots are read root-first, but
/// the wire bytes must come out leaf-first.
#[test]
fn obsaudit_instance_field_groups_emit_leaf_first() {
    // Simulate the per-class byte groups the dumper builds while walking
    // the hierarchy root-first: Object (no fields), Super (one int = 0xAA),
    // Leaf (one int = 0xBB).
    let mut per_class: Vec<Vec<u8>> = Vec::new();
    per_class.push(Vec::new()); // java/lang/Object — no instance fields
    let mut sup = Vec::new();
    write_value_to_vec(&mut sup, HPROF_INT, &cratonvm_types::Value::Int(0xAA));
    per_class.push(sup);
    let mut leaf = Vec::new();
    write_value_to_vec(&mut leaf, HPROF_INT, &cratonvm_types::Value::Int(0xBB));
    per_class.push(leaf);

    let mut field_data = Vec::new();
    for group in per_class.iter().rev() {
        field_data.extend_from_slice(group);
    }

    assert_eq!(field_data.len(), 8, "two int fields = 8 bytes");
    let first =
        u32::from_be_bytes([field_data[0], field_data[1], field_data[2], field_data[3]]);
    let second =
        u32::from_be_bytes([field_data[4], field_data[5], field_data[6], field_data[7]]);
    assert_eq!(first, 0xBB, "leaf class's field must be written first");
    assert_eq!(second, 0xAA, "super class's field must follow the leaf's");
}

/// The in-memory segment threshold bounds peak RSS during a dump. The one
/// production caller is the OOM path, so a gibibyte-sized staging buffer
/// would fail exactly when it is needed.
#[test]
fn obsaudit_segment_threshold_is_bounded() {
    assert!(
        MAX_SEGMENT_SIZE <= 64 * 1024 * 1024,
        "MAX_SEGMENT_SIZE ({MAX_SEGMENT_SIZE}) is large enough to fail the \
         OutOfMemoryError dump path it exists to serve"
    );
    assert!(
        (MAX_SEGMENT_SIZE as u64) < u32::MAX as u64,
        "a segment larger than the u32 HPROF record-length field cannot be written"
    );
}

#[test]
fn s42_segment_builder_basic() {
    let mut seg = SegmentBuilder::new();
    assert_eq!(seg.len(), 0);
    seg.push_u8(0xFF);
    seg.push_u16(0x1234);
    seg.push_u32(0xDEADBEEF);
    seg.push_u64(0x0102030405060708);
    // 1 + 2 + 4 + 8 = 15
    assert_eq!(seg.len(), 15);
}

#[test]
fn s42_segment_builder_flush_writes_record() {
    let mut seg = SegmentBuilder::new();
    seg.push_u8(GC_INSTANCE_DUMP);
    seg.push_u64(0x42);

    let mut out = Vec::new();
    seg.flush(&mut out).unwrap();

    // Record: tag(1) + timestamp(4) + length(4) + body(9)
    assert_eq!(out[0], HprofWriter::HPROF_HEAP_DUMP_SEGMENT);
    let body_len = u32::from_be_bytes([out[5], out[6], out[7], out[8]]);
    assert_eq!(body_len, 9); // 1 + 8
    assert_eq!(seg.len(), 0); // cleared after flush
}

#[test]
fn s42_segment_builder_empty_flush_noop() {
    let mut seg = SegmentBuilder::new();
    let mut out = Vec::new();
    seg.flush(&mut out).unwrap();
    assert!(out.is_empty());
}

#[test]
fn s42_write_value_for_type_int() {
    let mut seg = SegmentBuilder::new();
    write_value_for_type(&mut seg, HPROF_INT, &cratonvm_types::Value::Int(0x12345678));
    assert_eq!(seg.len(), 4);
    assert_eq!(&seg.buf, &0x12345678u32.to_be_bytes());
}

#[test]
fn s42_write_value_for_type_long() {
    let mut seg = SegmentBuilder::new();
    write_value_for_type(
        &mut seg,
        HPROF_LONG,
        &cratonvm_types::Value::Long(123456789012345),
    );
    assert_eq!(seg.len(), 8);
    assert_eq!(&seg.buf, &(123456789012345u64).to_be_bytes());
}

#[test]
fn s42_write_value_for_type_object_null() {
    let mut seg = SegmentBuilder::new();
    write_value_for_type(&mut seg, HPROF_OBJECT, &cratonvm_types::Value::Object(None));
    assert_eq!(seg.len(), 8);
    assert_eq!(&seg.buf, &0u64.to_be_bytes());
}

#[test]
fn s42_write_value_for_type_float() {
    let mut seg = SegmentBuilder::new();
    let f: f32 = 3.14;
    write_value_for_type(&mut seg, HPROF_FLOAT, &cratonvm_types::Value::Float(f));
    assert_eq!(seg.len(), 4);
    assert_eq!(&seg.buf, &f.to_bits().to_be_bytes());
}

#[test]
fn s42_write_value_for_type_double() {
    let mut seg = SegmentBuilder::new();
    let d: f64 = 2.71828;
    write_value_for_type(&mut seg, HPROF_DOUBLE, &cratonvm_types::Value::Double(d));
    assert_eq!(seg.len(), 8);
    assert_eq!(&seg.buf, &d.to_bits().to_be_bytes());
}

#[test]
fn s42_write_value_for_type_byte() {
    let mut seg = SegmentBuilder::new();
    write_value_for_type(&mut seg, HPROF_BYTE, &cratonvm_types::Value::Int(42));
    assert_eq!(seg.len(), 1);
    assert_eq!(seg.buf[0], 42);
}

#[test]
fn s42_write_value_for_type_short() {
    let mut seg = SegmentBuilder::new();
    write_value_for_type(&mut seg, HPROF_SHORT, &cratonvm_types::Value::Int(0x7FFF));
    assert_eq!(seg.len(), 2);
    assert_eq!(&seg.buf, &0x7FFFu16.to_be_bytes());
}

#[test]
fn s42_write_value_for_type_boolean() {
    let mut seg = SegmentBuilder::new();
    write_value_for_type(&mut seg, HPROF_BOOLEAN, &cratonvm_types::Value::Int(1));
    assert_eq!(seg.len(), 1);
    assert_eq!(seg.buf[0], 1);
}

#[test]
fn s42_write_value_to_vec_all_types() {
    // Verify write_value_to_vec produces the same result as write_value_for_type
    for (htype, val, expected_size) in [
        (HPROF_BYTE, cratonvm_types::Value::Int(99), 1),
        (HPROF_BOOLEAN, cratonvm_types::Value::Int(1), 1),
        (HPROF_SHORT, cratonvm_types::Value::Int(1000), 2),
        (HPROF_CHAR, cratonvm_types::Value::Int(65), 2),
        (HPROF_INT, cratonvm_types::Value::Int(42), 4),
        (HPROF_FLOAT, cratonvm_types::Value::Float(1.0), 4),
        (HPROF_LONG, cratonvm_types::Value::Long(999), 8),
        (HPROF_DOUBLE, cratonvm_types::Value::Double(1.0), 8),
        (HPROF_OBJECT, cratonvm_types::Value::Object(None), 8),
    ] {
        let mut buf = Vec::new();
        write_value_to_vec(&mut buf, htype, &val);
        assert_eq!(
            buf.len(),
            expected_size,
            "type={} expected size={}",
            htype,
            expected_size
        );
    }
}

// ---- Integration tests using SharedVm ------------------------------

/// Helper: create a VM with at least one loaded class for dump tests.
fn make_test_vm() -> Arc<SharedVm> {
    let config = crate::config::VmConfig {
        use_synthetic_jdk: true,
        ..Default::default()
    };
    let vm = Arc::new(crate::vm::SharedVm::new(config));
    // Load a class so the class store is non-empty
    let _ = vm.load_class_concurrent("java/lang/Object");
    vm
}

#[test]
fn s42_full_dump_header_and_end_present() {
    let vm = make_test_vm();

    let mut output = Vec::new();
    let mut dumper = HprofDumper::new(&vm);
    dumper.write_all(&mut output).unwrap();

    // Verify header magic
    let magic = HprofWriter::HPROF_MAGIC.as_bytes();
    assert_eq!(&output[..magic.len()], magic);

    // Verify null terminator after magic
    assert_eq!(output[magic.len()], 0);

    // Verify identifier size = 8
    let id_size_offset = magic.len() + 1;
    let id_size = u32::from_be_bytes([
        output[id_size_offset],
        output[id_size_offset + 1],
        output[id_size_offset + 2],
        output[id_size_offset + 3],
    ]);
    assert_eq!(id_size, 8);

    // Verify HEAP_DUMP_END is the last record
    let end_tag = HprofWriter::HPROF_HEAP_DUMP_END;
    // The last 9 bytes should be: tag(1) + timestamp(4) + length(4)=0
    let tail = &output[output.len() - 9..];
    assert_eq!(tail[0], end_tag);
    let end_len = u32::from_be_bytes([tail[5], tail[6], tail[7], tail[8]]);
    assert_eq!(end_len, 0);
}

#[test]
fn s42_dump_contains_string_records() {
    let vm = make_test_vm();

    let mut output = Vec::new();
    let mut dumper = HprofDumper::new(&vm);
    dumper.write_all(&mut output).unwrap();

    // Scan for at least one STRING_IN_UTF8 record
    let header_size = HprofWriter::header_size();
    let mut found_string = false;
    let mut pos = header_size;
    while pos + 9 <= output.len() {
        let tag = output[pos];
        let body_len = u32::from_be_bytes([
            output[pos + 5],
            output[pos + 6],
            output[pos + 7],
            output[pos + 8],
        ]) as usize;
        if tag == HprofWriter::HPROF_UTF8 {
            found_string = true;
            break;
        }
        pos += 9 + body_len;
    }
    assert!(
        found_string,
        "Dump should contain at least one STRING_IN_UTF8 record"
    );
}

#[test]
fn s42_dump_contains_load_class_records() {
    let vm = make_test_vm();

    let mut output = Vec::new();
    let mut dumper = HprofDumper::new(&vm);
    dumper.write_all(&mut output).unwrap();

    let header_size = HprofWriter::header_size();
    let mut load_class_count = 0u32;
    let mut pos = header_size;
    while pos + 9 <= output.len() {
        let tag = output[pos];
        let body_len = u32::from_be_bytes([
            output[pos + 5],
            output[pos + 6],
            output[pos + 7],
            output[pos + 8],
        ]) as usize;
        if tag == HprofWriter::HPROF_LOAD_CLASS {
            load_class_count += 1;
        }
        pos += 9 + body_len;
    }
    assert!(
        load_class_count > 0,
        "Dump should contain LOAD_CLASS records"
    );
}

#[test]
fn s42_dump_contains_heap_dump_segment() {
    let vm = make_test_vm();

    let mut output = Vec::new();
    let mut dumper = HprofDumper::new(&vm);
    dumper.write_all(&mut output).unwrap();

    let header_size = HprofWriter::header_size();
    let mut found_segment = false;
    let mut pos = header_size;
    while pos + 9 <= output.len() {
        let tag = output[pos];
        let body_len = u32::from_be_bytes([
            output[pos + 5],
            output[pos + 6],
            output[pos + 7],
            output[pos + 8],
        ]) as usize;
        if tag == HprofWriter::HPROF_HEAP_DUMP_SEGMENT {
            found_segment = true;
            break;
        }
        pos += 9 + body_len;
    }
    assert!(
        found_segment,
        "Dump should contain at least one HEAP_DUMP_SEGMENT"
    );
}

#[test]
fn s42_dump_contains_class_dump_subrecords() {
    let vm = make_test_vm();

    let mut output = Vec::new();
    let mut dumper = HprofDumper::new(&vm);
    dumper.write_all(&mut output).unwrap();

    // Find a HEAP_DUMP_SEGMENT and look for CLASS_DUMP (0x20) sub-records
    let header_size = HprofWriter::header_size();
    let mut found_class_dump = false;
    let mut pos = header_size;
    while pos + 9 <= output.len() {
        let tag = output[pos];
        let body_len = u32::from_be_bytes([
            output[pos + 5],
            output[pos + 6],
            output[pos + 7],
            output[pos + 8],
        ]) as usize;
        if tag == HprofWriter::HPROF_HEAP_DUMP_SEGMENT && body_len > 0 {
            // Scan sub-records in segment body
            let body_start = pos + 9;
            let body_end = body_start + body_len;
            if body_start < output.len() && output[body_start] == GC_CLASS_DUMP {
                found_class_dump = true;
                break;
            }
            // Also scan further into the body for CLASS_DUMP tags
            let mut sub_pos = body_start;
            while sub_pos < body_end {
                if output[sub_pos] == GC_CLASS_DUMP {
                    found_class_dump = true;
                    break;
                }
                sub_pos += 1;
            }
            if found_class_dump {
                break;
            }
        }
        pos += 9 + body_len;
    }
    assert!(
        found_class_dump,
        "Dump should contain CLASS_DUMP sub-records in HEAP_DUMP_SEGMENT"
    );
}

#[test]
fn s42_dump_stack_trace_present() {
    let vm = make_test_vm();

    let mut output = Vec::new();
    let mut dumper = HprofDumper::new(&vm);
    dumper.write_all(&mut output).unwrap();

    let header_size = HprofWriter::header_size();
    let mut found_trace = false;
    let mut pos = header_size;
    while pos + 9 <= output.len() {
        let tag = output[pos];
        let body_len = u32::from_be_bytes([
            output[pos + 5],
            output[pos + 6],
            output[pos + 7],
            output[pos + 8],
        ]) as usize;
        if tag == HprofWriter::HPROF_TRACE {
            found_trace = true;
            break;
        }
        pos += 9 + body_len;
    }
    assert!(
        found_trace,
        "Dump should contain at least one STACK_TRACE record"
    );
}

#[test]
fn s42_dump_to_file_succeeds() {
    let vm = make_test_vm();

    let tmp = std::env::temp_dir().join("cratonvm_s42_test.hprof");
    let path = tmp.to_str().unwrap();
    // gci i3/hd: `dump_heap` refuses an existing file, as HotSpot does.
    let _ = std::fs::remove_file(&tmp);
    let size = dump_heap(&vm, path, ThreadId(0)).expect("dump_heap should succeed");
    assert!(size > 0, "Dump file should be non-empty");

    // Verify the file starts with HPROF magic
    let data = std::fs::read(&tmp).unwrap();
    let magic = HprofWriter::HPROF_MAGIC.as_bytes();
    assert_eq!(&data[..magic.len()], magic);

    // Clean up
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn s42_dump_record_sequence_valid() {
    // Verify the record sequence: STRING* LOAD_CLASS* TRACE* SEGMENT+ END
    let vm = make_test_vm();

    let mut output = Vec::new();
    let mut dumper = HprofDumper::new(&vm);
    dumper.write_all(&mut output).unwrap();

    let header_size = HprofWriter::header_size();
    let mut tags = Vec::new();
    let mut pos = header_size;
    while pos + 9 <= output.len() {
        let tag = output[pos];
        let body_len = u32::from_be_bytes([
            output[pos + 5],
            output[pos + 6],
            output[pos + 7],
            output[pos + 8],
        ]) as usize;
        tags.push(tag);
        pos += 9 + body_len;
    }

    // Last tag must be HEAP_DUMP_END
    assert_eq!(*tags.last().unwrap(), HprofWriter::HPROF_HEAP_DUMP_END);

    // Must contain at least one HEAP_DUMP_SEGMENT
    assert!(
        tags.contains(&HprofWriter::HPROF_HEAP_DUMP_SEGMENT),
        "Must contain HEAP_DUMP_SEGMENT"
    );

    // First tags should be STRING records
    assert_eq!(tags[0], HprofWriter::HPROF_UTF8);

    // Must contain LOAD_CLASS and TRACE records
    assert!(tags.contains(&HprofWriter::HPROF_LOAD_CLASS));
    assert!(tags.contains(&HprofWriter::HPROF_TRACE));
}

#[test]
fn s42_intern_deduplicates_strings() {
    let vm = make_test_vm();
    let mut dumper = HprofDumper::new(&vm);

    let id1 = dumper.intern("java/lang/Object");
    let id2 = dumper.intern("java/lang/Object");
    let id3 = dumper.intern("java/lang/String");
    assert_eq!(id1, id2);
    assert_ne!(id1, id3);
}

#[test]
fn s42_class_serial_deterministic() {
    let vm = make_test_vm();
    let mut dumper = HprofDumper::new(&vm);

    let s1 = dumper.class_serial(ClassId::new(10));
    let s2 = dumper.class_serial(ClassId::new(10));
    let s3 = dumper.class_serial(ClassId::new(20));
    assert_eq!(s1, s2);
    assert_ne!(s1, s3);
    assert!(s1 >= 1);
    assert!(s3 >= 1);
}

/// gc-common w2-c: the skip test is half-open `[start, end)`, like the
/// published TLAB tails it is fed from.
#[test]
fn w2c_skip_span_is_half_open() {
    let vm = make_test_vm();
    let mut dumper = HprofDumper::new(&vm);
    assert!(!dumper.in_skip_span(0x1000), "no spans, nothing skipped");
    dumper.skip_spans = vec![(0x1000, 0x1040), (0x2000, 0x2008)];
    assert!(dumper.in_skip_span(0x1000));
    assert!(dumper.in_skip_span(0x1038));
    assert!(!dumper.in_skip_span(0x1040), "end is exclusive");
    assert!(!dumper.in_skip_span(0x0ff8));
    assert!(dumper.in_skip_span(0x2000));
    assert!(!dumper.in_skip_span(0x2008));
}

/// gc-common w2-c: an object the heap walk yields inside a reserved TLAB
/// tail (a frozen peer's un-retired tail, at the dump's pause) is not
/// emitted as an instance.
#[test]
fn w2c_walk_drops_objects_inside_a_reserved_tlab_tail() {
    let vm = make_test_vm();
    let obj = vm.mem.heap.alloc_object(ClassId::new(0), 1);
    let addr = obj.as_ptr() as usize;
    let mut needle = vec![GC_INSTANCE_DUMP];
    needle.extend_from_slice(&(addr as u64).to_be_bytes());
    let count = |out: &[u8]| {
        out.windows(needle.len())
            .filter(|w| *w == needle.as_slice())
            .count()
    };

    let mut skipped = Vec::new();
    let mut dumper = HprofDumper::new(&vm);
    dumper.skip_spans = vec![(addr, addr + 8)];
    dumper.write_all(&mut skipped).unwrap();
    assert_eq!(
        count(&skipped),
        0,
        "an object inside a reserved tail must not be dumped"
    );

    // The same dump without the span names it, whenever this backend's
    // walk reports a freshly allocated object at all.
    let mut plain = Vec::new();
    HprofDumper::new(&vm).write_all(&mut plain).unwrap();
    let listed = vm
        .mem
        .heap
        .walk_objects()
        .iter()
        .any(|(p, _)| *p as usize == addr);
    if listed {
        assert!(count(&plain) >= 1, "the unskipped dump must name it");
    }
    let _keep = obj;
}

/// gci i2/jn (`gci-i1jn-heap-dump-omits-in-native-band-roots-FIXED-20261012`):
/// the dump reads the JNI in-native bands only inside its own pause and
/// only with the band at the pause on; a dumper made with `new` (no pause)
/// reads none, whatever the switch, and still writes a whole dump.
#[test]
fn gci_i2jn_dump_reads_in_native_bands_only_inside_its_pause() {
    assert!(in_native_band_roots_readable(true, true));
    assert!(!in_native_band_roots_readable(false, true), "no pause: a band can end under the read");
    assert!(!in_native_band_roots_readable(true, false), "band at the pause off: no band recorded");
    assert!(!in_native_band_roots_readable(false, false));

    let vm = make_test_vm();
    let dumper = HprofDumper::new(&vm);
    assert!(!dumper.in_pause, "a bare dumper holds no pause");
    assert!(dumper.in_native_band_roots().is_empty());
    let mut out = Vec::new();
    HprofDumper::new(&vm).write_all(&mut out).unwrap();
    assert!(!out.is_empty());
}

/// gci i3/hd (`gci-i2jn-heap-dump-omits-frozen-compiled-peers-roots-FIXED-20261012`):
/// frozen peers' frames are read only inside the dump's own pause and only
/// when it froze someone; a bare dumper (no pause, no frozen set) reads
/// none even when handed tids.
#[test]
fn gci_i3hd_frozen_peer_roots_only_inside_the_pause() {
    assert!(frozen_peer_roots_readable(true, 1));
    assert!(!frozen_peer_roots_readable(true, 0), "nobody frozen");
    assert!(!frozen_peer_roots_readable(false, 3), "no pause: nobody is held");
    let vm = make_test_vm();
    let mut dumper = HprofDumper::new(&vm);
    assert!(dumper.frozen_peer_tids.is_empty());
    dumper.frozen_peer_tids = vec![u32::MAX];
    let (frames, fields) = dumper.frozen_peer_roots();
    assert!(frames.is_empty() && fields.is_empty());
}

/// gci i4/hd (`gci-i2jn-heap-dump-omits-frozen-compiled-peers-roots-FIXED-20261012`,
/// the compiled half): the words the dump's take-over kept are Java-frame
/// roots only inside the dump's own pause with someone frozen, never one
/// inside a reserved TLAB tail; and the dump writes them.
#[test]
fn gci_i4hd_frozen_peer_words_are_java_frame_roots_inside_the_pause() {
    let vm = make_test_vm();
    let a = vm.mem.heap.alloc_object(ClassId::new(0), 1);
    let b = vm.mem.heap.alloc_object(ClassId::new(0), 1);
    let (aa, ba) = (a.as_ptr() as usize, b.as_ptr() as usize);
    let mut dumper = HprofDumper::new(&vm);
    assert!(dumper.frozen_peer_words.is_empty());
    dumper.frozen_peer_words = vec![a, b];
    dumper.frozen_peer_tids = vec![u32::MAX];
    assert!(dumper.frozen_peer_word_roots().is_empty(), "no pause: nobody is held");
    dumper.in_pause = true;
    let got: Vec<usize> = dumper
        .frozen_peer_word_roots()
        .iter()
        .map(|o| o.as_ptr() as usize)
        .collect();
    assert_eq!(got, vec![aa, ba]);
    dumper.frozen_peer_tids.clear();
    assert!(dumper.frozen_peer_word_roots().is_empty(), "nobody frozen");
    dumper.frozen_peer_tids = vec![u32::MAX];
    dumper.skip_spans = vec![(ba, ba + 8)];
    let got: Vec<usize> = dumper
        .frozen_peer_word_roots()
        .iter()
        .map(|o| o.as_ptr() as usize)
        .collect();
    assert_eq!(got, vec![aa], "a word inside a reserved tail is no object");
    dumper.skip_spans.clear();
    let mut out = Vec::new();
    dumper.write_all(&mut out).unwrap();
    let mut needle = vec![GC_ROOT_JAVA_FRAME];
    needle.extend_from_slice(&(aa as u64).to_be_bytes());
    assert!(
        out.windows(needle.len()).any(|w| w == needle.as_slice()),
        "the dump names the frozen word as a Java-frame root"
    );
    let _keep = (a, b);
}

/// gci i4/hd (review item 6): every boot-loader class is a sticky root;
/// the name rules stay.
#[test]
fn gci_i4hd_sticky_class_roots_are_the_boot_loaders() {
    use cratonvm_types::ClassLoaderId;
    assert!(sticky_class_root("jdk/internal/misc/Unsafe", ClassLoaderId::Bootstrap));
    assert!(sticky_class_root("sun/nio/cs/UTF_8", ClassLoaderId::Bootstrap));
    assert!(sticky_class_root("java/lang/Object", ClassLoaderId::Application));
    assert!(sticky_class_root("[I", ClassLoaderId::Application));
    assert!(!sticky_class_root("com/example/App", ClassLoaderId::Application));
    assert!(!sticky_class_root("jdk/Foo", ClassLoaderId::UserDefined(7)));
}

/// gci i4/hd (review item 4): the per-class layout is built once and
/// reused; the array class id once per component.
#[test]
fn gci_i4hd_dump_caches_layouts_and_array_class_ids() {
    let vm = make_test_vm();
    let object = vm
        .classes
        .class_manager
        .read()
        .find_unique_class_by_name("java/lang/Object")
        .expect("loaded");
    let dumper = HprofDumper::new(&vm);
    let first = dumper.instance_layout(object);
    let again = dumper.instance_layout(object);
    assert!(std::rc::Rc::ptr_eq(&first, &again), "built once per class");
    assert_eq!(first.len(), 1, "Object's chain is itself");
    let a = dumper.array_class_obj_id(object);
    assert_eq!(dumper.array_class_obj_id(object), a);
    assert_eq!(dumper.array_class_cache.borrow().len(), 1);
}

/// gci i4/hd: both dump pauses keep the frozen peers' compiled words
/// (source pin; a frozen OS thread cannot be staged in a unit test).
#[test]
fn gci_i4hd_dump_pauses_keep_frozen_peer_words() {
    let src = include_str!("../hprof.rs").replace("\r\n", "\n");
    let ds = &src[src.find("\nimpl<'a> DumpSafepoint<'a> {").expect("DumpSafepoint")..];
    let ds = &ds[..ds.find("\n}\n").expect("end of impl")];
    assert_eq!(ds.matches("NonMovingPause::request_dump_words(").count(), 2);
    assert!(!ds.contains("NonMovingPause::request("));
}

/// gci i3/hd: HotSpot's `KlassInfoEntry::compare` -- more bytes first; on
/// a tie arrays before instances, then by name.
#[test]
fn gci_i3hd_histogram_order_is_hotspots() {
    use std::cmp::Ordering;
    assert_eq!(
        hotspot_histogram_order((100, false, "b"), (50, true, "a")),
        Ordering::Less
    );
    assert_eq!(
        hotspot_histogram_order((50, false, "a"), (50, true, "[B")),
        Ordering::Greater,
        "an array class first on a tie"
    );
    assert_eq!(
        hotspot_histogram_order((50, false, "java.lang.String"), (50, false, "java.lang.Object")),
        Ordering::Greater
    );
    assert_eq!(
        hotspot_histogram_order((50, true, "[C"), (50, true, "[B")),
        Ordering::Greater
    );
    assert_eq!(hotspot_histogram_order((1, false, "x"), (1, false, "x")), Ordering::Equal);
}

/// gci i3/hd: a histogram row's external name -- a primitive array by its
/// element letter, a reference array by its array class (the header holds
/// the COMPONENT id), an instance by its own class.
#[test]
fn gci_i3hd_histogram_names() {
    let vm = make_test_vm();
    let cm = vm.classes.class_manager.read();
    let object = cm
        .find_unique_class_by_name("java/lang/Object")
        .expect("the test VM loads java/lang/Object");
    let base = Some("java.base@25");
    let (n, m) = histo_name_and_module(&cm, HistoKey::PrimArray(ArrayElementType::Byte as u8), base);
    assert_eq!((n.as_str(), m.as_deref()), ("[B", Some("java.base@25")));
    let (n, _) = histo_name_and_module(&cm, HistoKey::PrimArray(ArrayElementType::Long as u8), base);
    assert_eq!(n, "[J");
    let (n, _) = histo_name_and_module(&cm, HistoKey::Instance(object.as_u32()), base);
    assert_eq!(n, "java.lang.Object");
    let (n, _) = histo_name_and_module(&cm, HistoKey::RefArray(object.as_u32()), base);
    assert_eq!(n, "[Ljava.lang.Object;");
    let (n, m) = histo_name_and_module(&cm, HistoKey::Instance(u32::MAX - 1), base);
    assert_eq!((n.as_str(), m), ("class#4294967294", None));
    for (et, letter) in [(4u8, 'Z'), (5, 'C'), (6, 'F'), (7, 'D'), (8, 'B'), (9, 'S'), (10, 'I'), (11, 'J')] {
        assert_eq!(prim_array_letter(et), letter);
    }
}

/// gci i3/hd (`gci-i2oo-jcmd-heap-walks-run-without-a-safepoint-FIXED-20261012`):
/// the attach histogram runs in a pause (granted at once with no mutator)
/// and keys a primitive array apart from instances.
#[test]
fn gci_i3hd_class_histogram_for_dcmd_walks_in_a_pause() {
    let vm = make_test_vm();
    let object = vm
        .classes
        .class_manager
        .read()
        .find_unique_class_by_name("java/lang/Object")
        .expect("loaded");
    let a = vm.mem.heap.alloc_object(object, 0);
    let b = vm.mem.heap.alloc_object(object, 0);
    let arr = vm.mem.heap.alloc_array(object, ArrayElementType::Byte, 16);
    let listed = |o: ObjectRef| {
        vm.mem
            .heap
            .walk_objects()
            .iter()
            .any(|(p, _)| std::ptr::eq(*p, o.as_ptr()))
    };
    let rows = class_histogram_for_dcmd(&vm).expect("a pause with no mutator is granted");
    if listed(a) && listed(b) {
        let row = rows
            .iter()
            .find(|e| e.class_name.starts_with("java.lang.Object"))
            .expect("an Object row");
        assert!(row.instance_count >= 2);
    }
    if listed(arr) {
        assert!(rows.iter().any(|e| e.class_name.starts_with("[B")));
    }
    for w in rows.windows(2) {
        assert!(w[0].total_bytes >= w[1].total_bytes, "largest first");
    }
    // The pause was released: a second one is granted too.
    assert!(class_histogram_for_dcmd(&vm).is_ok());
    let _keep = (a, b, arr);
}

/// gci i3/hd: the attach dump writes this file's HPROF inside a pause, and
/// an uncreatable path fails before any pause is requested.
#[test]
fn gci_i3hd_dump_heap_for_dcmd_writes_and_refuses_a_bad_path() {
    let vm = make_test_vm();
    let tmp = std::env::temp_dir().join("cratonvm_gci_i3hd_dcmd.hprof");
    let path = tmp.to_str().unwrap();
    let size = dump_heap_for_dcmd(&vm, path, false, true).expect("dump");
    let data = std::fs::read(&tmp).unwrap();
    assert_eq!(data.len() as u64, size);
    assert!(data.starts_with(HprofWriter::HPROF_MAGIC.as_bytes()));
    let _ = std::fs::remove_file(&tmp);
    let err = dump_heap_for_dcmd(&vm, "/nonexistent/dir/for/gci/i3hd/x.hprof", false, true)
        .unwrap_err();
    assert!(err.contains("Failed to write heap dump to"), "{err}");
}

/// The payload of a stored-block gzip stream, checked as a reader checks
/// it (header, block lengths, CRC-32, ISIZE). Panics on a malformed one.
fn gunzip_stored(data: &[u8]) -> Vec<u8> {
    assert_eq!(&data[..4], &[0x1f, 0x8b, 0x08, 0x00], "gzip header, deflate, no flags");
    let mut at = 10;
    let mut out = Vec::new();
    loop {
        let head = data[at];
        assert_eq!(head & 0b110, 0, "BTYPE 00 (stored)");
        let len = u16::from_le_bytes([data[at + 1], data[at + 2]]);
        let nlen = u16::from_le_bytes([data[at + 3], data[at + 4]]);
        assert_eq!(nlen, !len);
        at += 5;
        out.extend_from_slice(&data[at..at + usize::from(len)]);
        at += usize::from(len);
        if head & 1 == 1 {
            break;
        }
    }
    let crc = u32::from_le_bytes(data[at..at + 4].try_into().unwrap());
    let isize = u32::from_le_bytes(data[at + 4..at + 8].try_into().unwrap());
    assert_eq!(at + 8, data.len(), "nothing after the trailer");
    assert_eq!(crc, !gzip_crc_update(0xFFFF_FFFF, &out));
    assert_eq!(isize as usize, out.len());
    out
}

/// gci i5/hd (review item 10): the CRC is IEEE CRC-32 (the RFC 1952 check
/// value), and the stored-block writer frames any length -- empty, one
/// byte, exactly one block, a block and a byte -- as a stream that reads
/// back byte for byte.
#[test]
fn gci_i5hd_gzip_stored_writer_frames_every_length() {
    assert_eq!(!gzip_crc_update(0xFFFF_FFFF, b"123456789"), 0xCBF4_3926);
    assert_eq!(!gzip_crc_update(0xFFFF_FFFF, b""), 0);
    let block = GZIP_STORED_BLOCK;
    for n in [0usize, 1, block - 1, block, block + 1, 200_000] {
        // Truncation intended: a byte pattern.
        let input: Vec<u8> = (0..n).map(|i| (i * 31 % 251) as u8).collect();
        let mut z = GzipStoredWriter::new(Vec::new()).unwrap();
        // Odd chunk sizes, so blocks fill across `write` calls.
        for chunk in input.chunks(7_777) {
            z.write_all(chunk).unwrap();
        }
        let gz = z.finish().unwrap();
        assert_eq!(gunzip_stored(&gz), input, "n={n}");
    }
}

/// gci i5/hd (review item 10, i4 open item 6): `-gz` writes a gzip file
/// holding the HPROF dump, its size is the file's; without `overwrite`
/// an existing file is refused and left alone.
#[test]
fn gci_i5hd_dcmd_dump_gzips_and_opens_exclusively() {
    let vm = make_test_vm();
    let tmp = std::env::temp_dir().join("cratonvm_gci_i5hd_dcmd.hprof.gz");
    let path = tmp.to_str().unwrap();
    let _ = std::fs::remove_file(&tmp);
    let size = dump_heap_for_dcmd(&vm, path, true, false).expect("dump");
    let data = std::fs::read(&tmp).unwrap();
    assert_eq!(data.len() as u64, size);
    let hprof = gunzip_stored(&data);
    assert!(hprof.starts_with(HprofWriter::HPROF_MAGIC.as_bytes()));
    let err = dump_heap_for_dcmd(&vm, path, false, false).unwrap_err();
    assert!(err.contains("Failed to write heap dump to"), "{err}");
    assert_eq!(std::fs::read(&tmp).unwrap(), data, "the existing file is untouched");
    let again = dump_heap_for_dcmd(&vm, path, false, true).expect("overwrite");
    assert!(std::fs::read(&tmp).unwrap().starts_with(HprofWriter::HPROF_MAGIC.as_bytes()));
    assert!(again > 0);
    let _ = std::fs::remove_file(&tmp);
}

/// gci i3/hd: each THREAD_OBJ root names its own thread's serial, and a
/// dead thread's mirror is no root (the serials used to be paired with a
/// second, filtered list by index).
#[test]
fn gci_i3hd_thread_roots_name_their_own_serial() {
    let vm = make_test_vm();
    let reg = &vm.threads.thread_registry;
    let dead = vm.mem.heap.alloc_object(ClassId::new(0), 1);
    let live = vm.mem.heap.alloc_object(ClassId::new(0), 1);
    reg.register(ThreadId(9_101), "gci-i3hd-dead", Some(dead));
    reg.mark_dead(ThreadId(9_101));
    reg.register(ThreadId(9_102), "gci-i3hd-live", Some(live));
    let names = reg.all_thread_names();
    let serial = names
        .iter()
        .position(|(t, _)| *t == ThreadId(9_102))
        .expect("registered") as u32
        + 1;
    let mut out = Vec::new();
    HprofDumper::new(&vm).write_all(&mut out).unwrap();
    let root = |obj: ObjectRef| {
        let mut n = vec![GC_ROOT_THREAD_OBJ];
        n.extend_from_slice(&(obj.as_ptr() as u64).to_be_bytes());
        n
    };
    let mut want = root(live);
    want.extend_from_slice(&serial.to_be_bytes());
    want.extend_from_slice(&serial.to_be_bytes());
    assert!(out.windows(want.len()).any(|w| w == want.as_slice()), "live thread at its serial");
    let gone = root(dead);
    assert!(
        !out.windows(gone.len()).any(|w| w == gone.as_slice()),
        "a dead thread's mirror is not a thread root"
    );
}

/// gci i5/hd (review item 5): a Java-frame root from a thread's own
/// snapshot names that thread's serial, not serial 1.
#[test]
fn gci_i5hd_snapshot_frame_roots_name_their_threads_serial() {
    let vm = make_test_vm();
    let reg = &vm.threads.thread_registry;
    // Three holders: at most one of them can sit at serial 1, so the
    // others tell the fix from the old constant.
    let mut held = Vec::new();
    for k in 0..3u64 {
        let tid = ThreadId(9_201 + k);
        let obj = vm.mem.heap.alloc_object(ClassId::new(0), 1);
        reg.register(tid, "gci-i5hd-holder", None);
        reg.set_root_snapshot(tid, Arc::new(parking_lot::Mutex::new(vec![obj])));
        held.push((tid, obj));
    }
    let names = reg.all_thread_names();
    let mut out = Vec::new();
    HprofDumper::new(&vm).write_all(&mut out).unwrap();
    for &(tid, obj) in &held {
        let serial = names
            .iter()
            .position(|(t, _)| *t == tid)
            .expect("registered") as u32
            + 1;
        let mut want = vec![GC_ROOT_JAVA_FRAME];
        want.extend_from_slice(&(obj.as_ptr() as u64).to_be_bytes());
        want.extend_from_slice(&serial.to_be_bytes());
        want.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        assert!(
            out.windows(want.len()).any(|w| w == want.as_slice()),
            "the held object is a Java-frame root of its own thread's serial {serial}"
        );
    }
    let _keep = held;
}

/// gci i3/hd: the OOME dump (`dump_heap`) refuses an existing file and
/// leaves it untouched, answering HotSpot's reason.
#[test]
fn gci_i3hd_oome_dump_refuses_an_existing_file() {
    let vm = make_test_vm();
    let tmp = std::env::temp_dir().join("cratonvm_gci_i3hd_oome_exists.hprof");
    std::fs::write(&tmp, b"keep").unwrap();
    let err = dump_heap(&vm, tmp.to_str().unwrap(), ThreadId(0)).unwrap_err();
    assert_eq!(err, "File exists");
    assert_eq!(std::fs::read(&tmp).unwrap(), b"keep");
    let _ = std::fs::remove_file(&tmp);
    let nf = io::Error::from(io::ErrorKind::NotFound);
    assert_eq!(open_error_reason(&nf), "No such file or directory");
}

/// gci i47w/dh47: `HotSpotDiagnosticMXBean.dumpHeap`'s dump writes an
/// HPROF file and collects first only when `live`; an existing file is
/// HotSpot's bare `File exists`, left untouched, and nothing is collected
/// (HotSpot's `DumpWriter` opens before `VM_HeapDumper` runs).
#[test]
fn gci_i47w_dh47_mxbean_dump_opens_exclusively_before_collecting() {
    let vm = make_test_vm();
    let tmp = std::env::temp_dir().join("cratonvm_gci_i47w_dh47_mxbean.hprof");
    let path = tmp.to_str().unwrap();
    let _ = std::fs::remove_file(&tmp);
    for live in [true, false] {
        let _ = std::fs::remove_file(&tmp);
        let mut collected = 0;
        let size = dump_heap_for_mxbean(&vm, path, live, &mut || collected += 1)
            .expect("dump");
        assert_eq!(collected, usize::from(live), "live={live}");
        let data = std::fs::read(&tmp).unwrap();
        assert!(size > 0);
        assert_eq!(data.len() as u64, size);
        assert!(data.starts_with(HprofWriter::HPROF_MAGIC.as_bytes()));
    }
    let before = std::fs::read(&tmp).unwrap();
    let mut collected = 0;
    let err = dump_heap_for_mxbean(&vm, path, true, &mut || collected += 1).unwrap_err();
    assert_eq!(err, "File exists");
    assert_eq!(collected, 0, "no collection for a refused file");
    assert_eq!(std::fs::read(&tmp).unwrap(), before, "the existing file is untouched");
    let _ = std::fs::remove_file(&tmp);
}

/// gci i47w/dh47: source witness -- the MXBean path opens with
/// `create_new` before its collection and its pause, and answers the bare
/// reason (`open_error_reason`), not the `jcmd` text.
#[test]
fn gci_i47w_dh47_mxbean_dump_source_order() {
    let src = include_str!("../hprof.rs");
    let start = src
        .find("pub(crate) fn dump_heap_for_mxbean(")
        .expect("dump_heap_for_mxbean");
    let body = &src[start..start + src[start..].find("\n}\n").expect("end")];
    let open = body.find(".create_new(true)").expect("exclusive open");
    let reason = body.find("open_error_reason(&e)").expect("bare reason");
    let collect = body.find("collect();").expect("collect");
    let pause = body.find("DumpSafepoint::request_outside(").expect("pause");
    assert!(open < reason && reason < collect && collect < pause);
    assert!(!body.contains("Failed to write heap dump"));
}

/// gci i3/hd: source witness -- both attach walks take the pause before
/// they walk, and neither falls back to an unpaused walk.
#[test]
fn gci_i3hd_attach_walks_hold_the_pause() {
    let src = include_str!("../hprof.rs");
    // gci i14/hd second pass: the bodies moved to the `_live` variants.
    for f in ["pub(crate) fn dump_heap_for_dcmd_live(", "pub(crate) fn class_histogram_for_dcmd_live("] {
        let start = src.find(f).expect(f);
        let body = &src[start..];
        let body = &body[..body.find("\n}\n").expect("end")];
        let pause = body.find("DumpSafepoint::request_outside(vm, ").expect("pause");
        let refuse = body.find("dcmd_no_pause_message()").expect("refusal");
        // gci i14/hd: the histogram visits (`for_each_object`).
        let walk = body
            .find("write_dump_in(")
            .or_else(|| body.find(".walk_objects()"))
            .or_else(|| body.find(".for_each_object("))
            .or_else(|| body.find("walk_heap_for_dcmd("))
            .expect("walk");
        assert!(pause < refuse && refuse < walk, "{f}");
    }
}

/// gci i9/hj (`gci-i8hd-jvmti-tag-functions-and-oome-dump-outside-a-pause-FIXED-20261017`
/// item 3): the OOME dump's pause request waits GC-safe between refusals,
/// bounded, and only a caller with a slice retries. Live half: on a test VM
/// a granted request calls no slice; a refused one called at least one
/// before its unpaused fallback.
#[test]
fn gci_i9hj_oom_dump_retries_its_pause_gc_safe() {
    let vm = make_test_vm();
    let mut slices = 0u32;
    let mut slice = || slices += 1;
    let granted = DumpSafepoint::request_waiting(&vm, ThreadId(0), Some(&mut slice as &mut dyn FnMut()))
        .pause
        .is_some();
    if granted {
        assert_eq!(slices, 0, "a granted pause waits no slice");
    } else {
        assert!(slices >= 1, "a refused pause waits GC-safe before the fallback");
    }
    // Source half.
    // gci i10/al: the escapes were literal line breaks (a text-mode
    // rewrite turned `\r\n` into one); same meaning on an LF file, and
    // CRLF-safe again.
    let src = include_str!("../hprof.rs").replace("\r\n", "\n");
    let rw = &src[src.find("    fn request_waiting(").expect("request_waiting")..];
    let rw = &rw[..rw.find("\n    }\n").expect("end")];
    let req = rw.find("request_dump_words(vm, initiator)").expect("request");
    let none = rw.find("gc_safe_slice.as_deref_mut() else").expect("no slice: one request");
    let bound = rw.find(">= deadline").expect("bounded");
    let wait = rw.find("slice();").expect("the GC-safe wait");
    assert!(req < none && none < bound && bound < wait);
    assert!(rw.contains("std::time::Instant::now() + OOM_DUMP_PAUSE_RETRY"));
    let dh = &src[src.find("pub fn dump_heap(").expect("dump_heap")..];
    let dh = &dh[..dh.find("\n}\n").expect("end")];
    assert!(dh.contains("dump_heap_waiting(vm, path, initiator, None, false)"));
    let oom = &src[src.find("pub(crate) fn dump_heap_on_oom(").expect("dump_heap_on_oom")..];
    let oom = &oom[..oom.find("\n}\n").expect("end")];
    assert!(oom.contains("dump_heap_waiting(vm, path, initiator, Some(gc_safe_slice), gzip)"));
}

// ---- gci i14/hd (`gci-i14hd-g1-heap-diagnostics-review-findings-FIXED-20261016`) ----

/// A G1 test VM (64 MiB) with `java/lang/Object` loaded.
fn make_g1_test_vm() -> Arc<SharedVm> {
    let config = crate::config::VmConfig {
        use_synthetic_jdk: true,
        gc_algorithm: crate::config::GcAlgorithm::G1,
        max_heap_size: 64 * 1024 * 1024,
        ..Default::default()
    };
    let vm = Arc::new(crate::vm::SharedVm::new(config));
    let _ = vm.load_class_concurrent("java/lang/Object");
    vm
}

fn object_class(vm: &SharedVm) -> ClassId {
    vm.classes
        .class_manager
        .read()
        .find_unique_class_by_name("java/lang/Object")
        .expect("the test VM loads java/lang/Object")
}

/// Item 3: the binary search answers exactly as the per-object linear
/// scan it replaced, half-open, on unsorted input once sorted.
#[test]
fn gci_i14hd_sorted_spans_answer_as_the_linear_scan() {
    let spans = sorted_spans(vec![(0x3000, 0x3010), (0x1000, 0x1040), (0x2000, 0x2008)]);
    assert_eq!(spans, vec![(0x1000, 0x1040), (0x2000, 0x2008), (0x3000, 0x3010)]);
    let linear = |a: usize| spans.iter().any(|&(s, e)| a >= s && a < e);
    for a in (0x0ff0..0x3020).step_by(4) {
        assert_eq!(in_sorted_spans(&spans, a), linear(a), "{a:#x}");
    }
    assert!(!in_sorted_spans(&[], 0x1000));
}

/// Item 5: HotSpot's `calculate_array_max_length` and the sub-record
/// shapes the streamed segment is sized from.
#[test]
fn gci_i14hd_array_max_length_is_hotspots() {
    let max = u32::MAX as usize;
    assert_eq!(hprof_array_max_length(10, 8, 25), 10);
    assert_eq!(hprof_array_max_length(max, 8, 25), (max - 25) / 8);
    assert_eq!(hprof_array_max_length(max - 18, 1, 18), max - 18, "exactly fits");
    assert_eq!(hprof_array_max_length(max - 17, 1, 18), max - 18, "one byte over");
    assert_eq!(array_subrecord_shape(ArrayElementType::Reference), (25, 8));
    assert_eq!(array_subrecord_shape(ArrayElementType::Char), (18, 2));
    assert_eq!(array_subrecord_shape(ArrayElementType::Boolean), (18, 1));
    assert_eq!(array_subrecord_len(ArrayElementType::Long, 3), 18 + 24);
    assert_eq!(hotspot_array_type_name(ArrayElementType::Reference), "object");
    assert_eq!(hotspot_array_type_name(ArrayElementType::Int), "int");
}

/// Item 5: the in-place payload read writes the bytes the per-element
/// encoder writes: big-endian, bytes as they are.
#[test]
fn gci_i14hd_prim_elements_are_big_endian() {
    let ints: [i32; 3] = [1, -2, 0x0102_0304];
    let mut out = Vec::new();
    // SAFETY: 3 elements of 4 bytes in a live local array.
    unsafe { push_prim_elements_be(ints.as_ptr() as *const u8, 4, 3, &mut out) };
    let mut want = Vec::new();
    for v in ints {
        want.extend_from_slice(&(v as u32).to_be_bytes());
    }
    assert_eq!(out, want);
    let bytes = [7u8, 8, 9];
    out.clear();
    // SAFETY: as above, 3 one-byte elements.
    unsafe { push_prim_elements_be(bytes.as_ptr(), 1, 3, &mut out) };
    assert_eq!(out, vec![7, 8, 9]);
}

/// Item 1: a TLAB filler is neither an object nor a program `int[]`.
#[test]
fn gci_i14hd_tlab_filler_is_recognised() {
    let filler = cratonvm_gc::tlab::TLAB_FILLER_CLASS_ID;
    assert!(is_tlab_filler(filler, ObjectKind::Array));
    assert!(!is_tlab_filler(filler, ObjectKind::Object));
    assert!(!is_tlab_filler(ClassId::new(0), ObjectKind::Array));
}

/// Item 4: G1's visitor yields exactly `walk_objects`' list, a humongous
/// array included.
#[test]
fn gci_i14hd_g1_visitor_yields_the_walk() {
    let vm = make_g1_test_vm();
    let object = object_class(&vm);
    let mut keep = Vec::new();
    for _ in 0..64 {
        keep.push(vm.mem.heap.alloc_object(object, 0));
    }
    // 1.2 MB of int: humongous at the 64 MiB heap's region size.
    keep.push(vm.mem.heap.alloc_array(object, ArrayElementType::Int, 300_000));
    let listed = vm.mem.heap.walk_objects();
    let mut visited = Vec::new();
    vm.mem
        .heap
        .for_each_object(&mut |p: *mut u8, s: usize| visited.push((p, s)));
    assert!(!visited.is_empty());
    assert_eq!(visited, listed);
    drop(keep);
}

/// Items 1 and 2: the dump writes no record for a TLAB filler or an
/// object whose class the store does not hold (it would name a class
/// object with no CLASS_DUMP), and the histogram prints no `class#<id>`
/// row; an ordinary object is still dumped. On the default backend and
/// on G1.
#[test]
fn gci_i14hd_dump_skips_fillers_and_classless_objects() {
    for vm in [make_test_vm(), make_g1_test_vm()] {
        let object = object_class(&vm);
        let live = vm.mem.heap.alloc_object(object, 0);
        let filler = vm.mem.heap.alloc_array(
            cratonvm_gc::tlab::TLAB_FILLER_CLASS_ID,
            ArrayElementType::Int,
            6,
        );
        let classless = vm.mem.heap.alloc_object(ClassId::new(0x00AB_CDEF), 0);
        let listed: Vec<usize> = vm
            .mem
            .heap
            .walk_objects()
            .iter()
            .map(|(p, _)| *p as usize)
            .collect();
        let mut out = Vec::new();
        HprofDumper::new(&vm).write_all(&mut out).unwrap();
        let count = |tag: u8, o: ObjectRef| {
            let mut needle = vec![tag];
            needle.extend_from_slice(&(o.as_ptr() as u64).to_be_bytes());
            out.windows(needle.len())
                .filter(|w| *w == needle.as_slice())
                .count()
        };
        assert_eq!(count(GC_PRIM_ARRAY_DUMP, filler), 0, "a TLAB filler is no int[]");
        assert_eq!(count(GC_INSTANCE_DUMP, classless), 0, "no record names a missing class");
        if listed.contains(&(live.as_ptr() as usize)) {
            assert!(count(GC_INSTANCE_DUMP, live) >= 1, "an ordinary object is dumped");
        }
        let rows = class_histogram_for_dcmd(&vm).expect("a pause with no mutator is granted");
        assert!(
            rows.iter().all(|e| !e.class_name.starts_with("class#")),
            "no row for a class the store does not hold"
        );
        let _keep = (live, filler, classless);
    }
}

/// Item 5: an array larger than a segment is written as its own segment
/// whose sub-record is byte for byte the in-memory one (primitive: read
/// in place; reference: the shared encoder), on a G1 heap where both are
/// humongous.
#[test]
fn gci_i14hd_large_array_streams_the_same_record() {
    let vm = make_g1_test_vm();
    let object = object_class(&vm);
    let target = vm.mem.heap.alloc_object(object, 0);
    let n = 3 * 1024 * 1024; // 12 MiB of int
    let ints = vm.mem.heap.alloc_array(object, ArrayElementType::Int, n);
    for (i, v) in [(0usize, 1i32), (1, -2), (n / 2, 0x0102_0304), (n - 1, 7)] {
        vm.mem
            .heap
            .set_array_element(ints, i, cratonvm_types::Value::Int(v))
            .unwrap();
    }
    let m = 1_100_000; // 8.8 MB of references
    let refs = vm.mem.heap.alloc_array(object, ArrayElementType::Reference, m);
    vm.mem
        .heap
        .set_array_element(refs, 5, cratonvm_types::Value::Object(Some(target)))
        .unwrap();
    assert!(array_subrecord_len(ArrayElementType::Int, n) > MAX_SEGMENT_SIZE);
    assert!(array_subrecord_len(ArrayElementType::Reference, m) > MAX_SEGMENT_SIZE);
    let dumper = HprofDumper::new(&vm);
    for (arr, reference) in [(ints, false), (refs, true)] {
        let mut seg = SegmentBuilder::new();
        if reference {
            dumper.write_obj_array_dump(&mut seg, arr);
        } else {
            dumper.write_prim_array_dump(&mut seg, arr);
        }
        let mut streamed = Vec::new();
        dumper.write_array_segment_streamed(&mut streamed, arr).unwrap();
        assert_eq!(streamed[0], HprofWriter::HPROF_HEAP_DUMP_SEGMENT);
        let body = u32::from_be_bytes([streamed[5], streamed[6], streamed[7], streamed[8]]) as usize;
        assert_eq!(body, seg.len(), "reference={reference}");
        assert!(streamed[9..] == seg.buf[..], "reference={reference}: same sub-record bytes");
    }
    let _keep = (target, ints, refs);
}


/// Second pass, item 10: the CLASS_DUMP loader id is the mirror's
/// `classLoader` referent, 0 without a mirror, a slot or a loader.
#[test]
fn gci_i14hd_class_dump_names_the_loader_object() {
    let vm = make_g1_test_vm();
    let heap = &vm.mem.heap;
    let holder = heap.alloc_object(ClassId::new(0x00AB_CDE0), 1);
    let loader = heap.alloc_object(ClassId::new(0x00AB_CDE1), 0);
    assert_eq!(class_loader_obj_id(heap, None, Some(0)), 0);
    assert_eq!(class_loader_obj_id(heap, Some(holder), None), 0);
    assert_eq!(class_loader_obj_id(heap, Some(holder), Some(0)), 0, "a null loader is the boot loader");
    heap.set_field(holder, 0, cratonvm_types::Value::Object(Some(loader)));
    assert_eq!(class_loader_obj_id(heap, Some(holder), Some(0)), loader.as_ptr() as u64);
    let _keep = (holder, loader);
}

/// Second pass, item 4 on Generational: the visitor (young listed, old in
/// 64 Ki slices) yields exactly `walk_objects`' list, across a slice
/// boundary.
#[test]
fn gci_i14hd_generational_visitor_yields_the_walk() {
    let vm = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig {
        use_synthetic_jdk: true,
        gc_algorithm: crate::config::GcAlgorithm::Generational,
        max_heap_size: 64 * 1024 * 1024,
        ..Default::default()
    }));
    let _ = vm.load_class_concurrent("java/lang/Object");
    let object = object_class(&vm);
    let mut keep = Vec::new();
    for _ in 0..16 {
        keep.push(vm.mem.heap.alloc_object(object, 0));
    }
    for _ in 0..70_000 {
        match vm.mem.heap.try_alloc_object_old(object, 0) {
            Some(o) => keep.push(o),
            None => break,
        }
    }
    let listed = vm.mem.heap.walk_objects();
    let mut visited = Vec::new();
    vm.mem
        .heap
        .for_each_object(&mut |p: *mut u8, s: usize| visited.push((p, s)));
    assert_eq!(visited.len(), listed.len());
    assert!(visited == listed, "same objects, same order");
    drop(keep);
}

/// gci i18/dc (`gci-i3hd-heap-walks-and-diagnostics-review-findings` item
/// 5): the serial map is the thread loops' `i + 1` over the name list.
#[test]
fn gci_i18dc_thread_serials_follow_the_name_list() {
    let names = vec![
        (ThreadId(40), "a".to_string()),
        (ThreadId(7), "b".to_string()),
        (ThreadId(12), "c".to_string()),
    ];
    let serials = thread_serials(&names);
    assert_eq!(serials.len(), 3);
    assert_eq!(serials[&ThreadId(40)], 1);
    assert_eq!(serials[&ThreadId(7)], 2);
    assert_eq!(serials[&ThreadId(12)], 3);
    assert!(!serials.contains_key(&ThreadId(99)));
}

/// gci i18/dc (review item 5's remainder): a frozen peer's roots carry
/// that peer's thread id, and the dump names its serial on the
/// thread-block root (it named serial 1).
#[test]
fn gci_i18dc_frozen_peer_roots_name_their_threads_serial() {
    let vm = make_test_vm();
    let reg = &vm.threads.thread_registry;
    if reg.current_os_tid_claimed() != Some(false) {
        return; // no OS-tid backend here, or this OS thread is claimed already
    }
    for k in 0..3u64 {
        reg.register(ThreadId(9_300 + k), "gci-i18dc-other", None);
    }
    let tid = ThreadId(9_310);
    reg.register(tid, "gci-i18dc-peer", None);
    reg.set_os_tid_current(tid);
    let Some(os_tid) = reg
        .thread_dump_entries()
        .iter()
        .find(|e| e.thread_id == tid)
        .and_then(|e| e.os_tid)
    else {
        return;
    };
    let held = vm.mem.heap.alloc_object(ClassId::new(0), 1);
    let mut thread = crate::threading::jvm_thread::JvmThread::new(tid, "gci-i18dc-peer");
    thread.jit_pending_exception = Some(held);
    reg.set_gc_block_state(tid, thread.gc_block_state.clone());
    reg.set_jvm_thread_addr(tid, &thread as *const _ as usize);
    let serial = thread_serials(&reg.all_thread_names())[&tid];

    let mut dumper = HprofDumper::new(&vm);
    dumper.frozen_peer_tids = vec![os_tid];
    let (frames, fields) = dumper.frozen_peer_roots();
    assert!(frames.is_empty() && fields.is_empty(), "no pause: nobody is held");
    dumper.in_pause = true;
    let (frames, fields) = dumper.frozen_peer_roots();
    assert!(frames.is_empty(), "the peer has no interpreter frame");
    assert!(fields.contains(&(tid, held)), "the field root carries its owner");
    assert!(fields.iter().all(|(owner, _)| *owner == tid));

    let mut out = Vec::new();
    dumper.write_all(&mut out).unwrap();
    let mut want = vec![GC_ROOT_THREAD_BLOCK];
    want.extend_from_slice(&(held.as_ptr() as u64).to_be_bytes());
    want.extend_from_slice(&serial.to_be_bytes());
    assert!(
        out.windows(want.len()).any(|w| w == want.as_slice()),
        "the thread-block root names the peer's serial {serial}"
    );
    reg.set_jvm_thread_addr(tid, 0);
    drop(thread);
}

/// gci i18/dc: the in-native band roots are read per owning thread
/// (`scan_in_native_bands_by_owner`) and written at that thread's serial.
#[test]
fn gci_i18dc_in_native_band_roots_are_read_per_owner() {
    let src = include_str!("../hprof.rs").replace("\r\n", "\n");
    // One method's text: from its signature to the first line that closes
    // an impl-level item.
    let method = |sig: &str| -> String {
        let start = src.find(sig).expect(sig);
        let rest = &src[start..];
        rest[..rest.find("\n    \u{7d}\n").expect("its end")].to_string()
    };
    let body = method("\n    fn in_native_band_roots(&self)");
    assert!(body.contains(".scan_in_native_bands_by_owner(&probe, &mut each)"));
    assert!(!body.contains(".scan_in_native_bands(&probe"));
    assert!(body.contains("companions.borrow_mut().drain(..)"));
    let roots = method("\n    fn write_gc_roots(&self");
    assert!(roots.contains("for &(tid, obj) in &self.in_native_band_roots()"));
    assert!(roots.contains("for &(tid, obj) in &frame_roots"));
    assert!(roots.contains("for &(tid, obj) in &field_roots"));
    assert!(roots.contains("seg.push_u32(serial_of(tid));"));
}
