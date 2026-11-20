//! Interpreter round i2 wave 68, lane L7: the fused instance-field int
//! increment (`CRATONVM_INTERP_FUSED_FIELD_INCREMENT`,
//! `fused_field_increment`). A hit stores what the unfused `aload_N; dup;
//! getfield; <operand>; iadd | isub; putfield` stores (the wrapping int
//! result, in the receiver's field) and leaves the operand stack as it
//! found it; every decline writes nothing.
use super::*;
use crate::runtime::interpreter::site_cache::PinnedSites;

const CP: u16 = 0x0102;

/// The site table every test here runs against: entries a test fills
/// are pinned, so another test thread moving the process epochs between
/// the fill and the helper's lookup cannot turn an expected hit into a
/// cold-site decline (wave 73, lane L7; page
/// `i70-orch-fused-increment-unit-tests-fail-intermittently-in-the-full-suite-20261103`).
type PinnedFast = PinnedSites<FastFieldSite>;

/// `<load>; dup; getfield #0x0102; <operand>; <arith>; putfield #0x0102;
/// return`.
fn group(load: u8, operand: &[u8], arith: u8) -> Vec<u8> {
    let mut code = vec![load, 0x59, 0xb4, 0x01, 0x02];
    code.extend_from_slice(operand);
    code.extend_from_slice(&[arith, 0xb5, 0x01, 0x02, 0xb1]);
    code
}

/// An instance `(I)V` frame named `method` over `code`: local 0 =
/// `recv`, local 1 = `5`, `max_locals` 3.
fn frame(class_id: ClassId, method: &str, code: Vec<u8>, recv: Value) -> Frame {
    Frame::new(
        class_id,
        "cratonvm/test/i68l7/FieldIncrement".to_string(),
        method.to_string(),
        "(I)V".to_string(),
        None,
        code,
        vec![],
        4,
        3,
        &[recv, Value::Int(5)],
    )
}

/// Fill `CP` of `class_id` with `site` and pin it (re-filled by `run` /
/// `run_mode` if the process epochs retire it); `false` when the epochs
/// kept moving (other tests defining classes) and every fill was dropped.
fn fill(sites: &mut PinnedFast, class_id: ClassId, site: FastFieldSite) -> bool {
    sites.pin(class_id, CP, site)
}

/// A legacy one-field receiver of `class` on `zgc` holding `Int(value)`,
/// with the site `fill_site` would record for it; `None` when the
/// allocation failed or came out compact (a layout another test
/// registered for the id).
fn legacy_receiver(zgc: &crate::memory::vm_heap::VmHeap, class: u32, value: i32) -> Option<(u64, FastFieldSite)> {
    let obj = zgc.try_alloc_object(ClassId::new(class), 1)?;
    let ptr = obj.as_ptr() as u64;
    // SAFETY: a fresh allocation starts with its header.
    let header = unsafe { &*(ptr as *const ObjectHeader) };
    if cratonvm_types::is_compact_object(header) {
        return None;
    }
    // SAFETY: field 0's 16-byte cell of the live legacy object.
    unsafe {
        cratonvm_types::write_value_atomic(
            (ptr as *mut u8).add(HEADER_SIZE) as *mut Value,
            Value::Int(value),
        )
    };
    Some((
        ptr,
        FastFieldSite {
            receiver_class_id: header.class_id,
            num_slots: header.num_slots(),
            offset: u32::try_from(HEADER_SIZE).ok()?,
            storage: None,
            field_index: 0,
            desc_byte: b'I',
            is_reference: false,
            final_put: FastFieldSite::PUT_ANY,
        },
    ))
}

/// The legacy field's current value.
fn legacy_value(ptr: u64) -> Option<Value> {
    // SAFETY: as in `legacy_receiver`.
    unsafe {
        cratonvm_types::read_value_checked_atomic(
            (ptr as *mut u8).add(HEADER_SIZE) as *const Value
        )
    }
}

fn run(zgc: &crate::memory::vm_heap::VmHeap, sites: &mut PinnedFast, f: &mut Frame) -> Option<usize> {
    run_mode(zgc, sites, f, FIELD_INC_ON)
}

#[test]
fn the_decoder_reads_every_operand_and_both_arithmetic_opcodes() {
    let class_id = ClassId::new(0x00ab_0681);
    // (operand bytes, the int it pushes)
    let operands: [(&[u8], i32); 8] = [
        (&[0x02], -1),
        (&[0x04], 1),
        (&[0x08], 5),
        (&[0x10, 0x80], -128),
        (&[0x11, 0x80, 0x00], -32768),
        (&[0x11, 0x03, 0xe8], 1000),
        (&[0x15, 0x01], 5),
        (&[0x1b], 5),
    ];
    for load in 0x2a..=0x2d_u8 {
        for (operand, k) in operands {
            for (arith, subtract) in [(0x60_u8, false), (0x64_u8, true)] {
                let code = group(load, operand, arith);
                let end = code.len() - 1;
                let f = frame(class_id, "m", code, Value::Object(None));
                let code_len = f.code.len() - 2;
                assert_eq!(
                    decode_field_increment(&f, &f.code, code_len, 0, false, false),
                    Some((
                        CP,
                        usize::from(load - 0x2a),
                        crate::runtime::interpreter::IncrementOperand::Int(k),
                        subtract,
                        None,
                        end
                    )),
                    "{load:#x} {operand:?} {arith:#x}"
                );
            }
        }
    }
}

#[test]
fn the_decoder_refuses_every_other_shape() {
    let class_id = ClassId::new(0x00ab_0682);
    let refuses = |code: Vec<u8>| {
        let f = frame(class_id, "m", code, Value::Object(None));
        let code_len = f.code.len() - 2;
        assert_eq!(
            decode_field_increment(&f, &f.code, code_len, 0, false, false),
            None
        );
    };
    // `aload n` (0x19), `iload_0`: not the short reference load.
    refuses(group(0x19, &[0x04], 0x60));
    refuses(group(0x1a, &[0x04], 0x60));
    // No `dup`, or `dup_x1`.
    refuses(vec![
        0x2a, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01, 0x02, 0xb1,
    ]);
    refuses(vec![
        0x2a, 0x5a, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01, 0x02, 0xb1,
    ]);
    // Another arithmetic opcode (`imul`, `ladd`), another operand (`ldc`,
    // `aload_0`), an int local past `max_locals`, a non-int local.
    refuses(group(0x2a, &[0x04], 0x68));
    refuses(group(0x2a, &[0x04], 0x61));
    refuses(group(0x2a, &[0x12, 0x01], 0x60));
    refuses(group(0x2a, &[0x2a], 0x60));
    refuses(group(0x2a, &[0x15, 0xff], 0x60));
    refuses(group(0x2a, &[0x1a], 0x60));
    // A `putfield` of another pool index, a `putstatic`, a `getfield`.
    refuses(vec![
        0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01, 0x03, 0xb1,
    ]);
    refuses(vec![
        0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb3, 0x01, 0x02, 0xb1,
    ]);
    refuses(vec![
        0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb4, 0x01, 0x02, 0xb1,
    ]);
    // Cut by the end of the code (the padding is never read as the tail).
    refuses(vec![0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01]);
    refuses(vec![0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60]);
}

#[test]
fn a_legacy_receiver_is_incremented_in_place() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let class_id = ClassId::new(0x00ab_0683);
    let cases: [(&[u8], u8, i32, i32); 3] = [
        (&[0x04], 0x60, 41, 42),
        (&[0x10, 0x07], 0x64, 10, 3),
        (&[0x1b], 0x60, i32::MAX, i32::MAX.wrapping_add(5)),
    ];
    for (operand, arith, start, want) in cases {
        let Some((ptr, site)) = legacy_receiver(&zgc, 6881, start) else {
            return;
        };
        let mut sites = PinnedFast::new();
        if !fill(&mut sites, class_id, site) {
            return;
        }
        // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
        let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
        let code = group(0x2a, operand, arith);
        let end = code.len() - 1;
        let mut f = frame(class_id, "m", code, recv);
        f.stack.push_int(42).unwrap();
        assert_eq!(
            run(&zgc, &mut sites, &mut f),
            Some(end),
            "lands on the `return`"
        );
        assert_eq!(legacy_value(ptr), Some(Value::Int(want)));
        assert_eq!(f.stack.len(), 1, "nothing pushed or popped");
    }
}

#[test]
fn a_compact_int_field_is_incremented_in_place() {
    const CLASS: u32 = 6882;
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    cratonvm_types::register_class_layout(
        zgc.layout_domain(),
        CLASS,
        std::sync::Arc::new(cratonvm_types::CompactLayout {
            field_disps: vec![8],
            is_ref: vec![false],
            field_kinds: vec![FieldStorageKind::Int],
            ref_disps: vec![],
            total_size: 16,
        }),
    );
    let Some(obj) = zgc.try_alloc_object(ClassId::new(CLASS), 1) else {
        return;
    };
    let ptr = obj.as_ptr() as u64;
    // SAFETY: a fresh allocation starts with its header.
    let header = unsafe { &*(ptr as *const ObjectHeader) };
    if !cratonvm_types::is_compact_object(header) {
        return;
    }
    // SAFETY: displacement 8 is the layout's 4-byte `Int` slot.
    let fp = unsafe { (ptr as *mut u8).add(8) };
    // SAFETY: as above.
    unsafe { store_u32(fp, 99) };
    let site = FastFieldSite {
        receiver_class_id: header.class_id,
        num_slots: header.num_slots(),
        offset: 8,
        storage: Some(FieldStorageKind::Int),
        field_index: 0,
        desc_byte: b'I',
        is_reference: false,
        final_put: FastFieldSite::PUT_ANY,
    };
    let class_id = ClassId::new(0x00ab_0684);
    let mut sites = PinnedFast::new();
    if !fill(&mut sites, class_id, site) {
        return;
    }
    let mut f = frame(
        class_id,
        "m",
        group(0x2a, &[0x04], 0x64),
        Value::Object(Some(obj)),
    );
    assert!(run(&zgc, &mut sites, &mut f).is_some());
    // SAFETY: as above.
    assert_eq!(unsafe { load_u32(fp) } as i32, 98);
}

#[test]
fn every_decline_writes_nothing() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let class_id = ClassId::new(0x00ab_0685);
    let Some((ptr, site)) = legacy_receiver(&zgc, 6883, 7) else {
        return;
    };
    // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
    let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
    let code = || group(0x2a, &[0x04], 0x60);
    let unchanged = || assert_eq!(legacy_value(ptr), Some(Value::Int(7)));

    // A cold site: the unfused `getfield` resolves and fills.
    let mut empty = PinnedFast::new();
    assert_eq!(
        run(&zgc, &mut empty, &mut frame(class_id, "m", code(), recv)),
        None
    );
    unchanged();

    let mut sites = PinnedFast::new();
    if !fill(&mut sites, class_id, site) {
        return;
    }
    // A null receiver: the unfused `getfield` throws the NPE.
    let mut null = frame(class_id, "m", code(), Value::Object(None));
    assert_eq!(run(&zgc, &mut sites, &mut null), None);
    // A non-reference local 0.
    let mut int0 = frame(class_id, "m", code(), Value::Int(3));
    assert_eq!(run(&zgc, &mut sites, &mut int0), None);
    // A watch armed, or the field-phase instrument.
    let mut f = frame(class_id, "m", code(), recv);
    let code_len = f.code.len() - 2;
    assert_eq!(
        fused_field_increment_on(
            &zgc,
            &mut sites,
            &mut f,
            code_len,
            0,
            FIELD_INC_ON,
            Some(true)
        ),
        None
    );
    assert_eq!(
        fused_field_increment_on(
            &zgc,
            &mut sites,
            &mut f,
            code_len,
            0,
            FIELD_INC_ON | FIELD_INC_PHASES,
            Some(false)
        ),
        None
    );
    unchanged();

    // Not an `I` field (a narrow field needs its narrowing store).
    for desc in *b"SBCZJFL" {
        let mut other = PinnedFast::new();
        if fill(
            &mut other,
            class_id,
            FastFieldSite {
                desc_byte: desc,
                ..site
            },
        ) {
            assert_eq!(run(&zgc, &mut other, &mut f), None, "{}", desc as char);
        }
    }
    unchanged();
    // A final field: never from another class, only from `<init>` of
    // its own.
    let mut never = PinnedFast::new();
    let never_site = FastFieldSite {
        final_put: FastFieldSite::PUT_NEVER,
        ..site
    };
    if fill(&mut never, class_id, never_site) {
        assert_eq!(run(&zgc, &mut never, &mut f), None);
    }
    unchanged();
    let mut init_only = PinnedFast::new();
    let init_site = FastFieldSite {
        final_put: FastFieldSite::PUT_INIT_ONLY,
        ..site
    };
    if fill(&mut init_only, class_id, init_site) {
        assert_eq!(run(&zgc, &mut init_only, &mut f), None);
        unchanged();
        let mut ctor = frame(class_id, "<init>", code(), recv);
        assert!(run(&zgc, &mut init_only, &mut ctor).is_some());
        assert_eq!(legacy_value(ptr), Some(Value::Int(8)));
    }
}

#[test]
fn a_receiver_of_another_shape_declines() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let class_id = ClassId::new(0x00ab_0686);
    let Some((ptr, site)) = legacy_receiver(&zgc, 6884, 7) else {
        return;
    };
    let mut sites = PinnedFast::new();
    // The site names another receiver class: `field_ptr_for` refuses.
    let other = FastFieldSite {
        receiver_class_id: ClassId::new(6885),
        ..site
    };
    if !fill(&mut sites, class_id, other) {
        return;
    }
    // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
    let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
    let mut f = frame(class_id, "m", group(0x2a, &[0x04], 0x60), recv);
    assert_eq!(run(&zgc, &mut sites, &mut f), None);
    assert_eq!(legacy_value(ptr), Some(Value::Int(7)));
}

// ── Wave 69, lane L7: `CRATONVM_INTERP_FUSED_FIELD_INCREMENT_WIDE` and
// `CRATONVM_INTERP_FUSED_INCREMENT_EARLY_DECLINE` ──

/// The wave-69 widened mode.
const WIDE: u8 = FIELD_INC_ON | FIELD_INC_WIDE;

/// `<load>; dup; getfield #0x0102; <operand>; <arith>; <tail...>;
/// putfield #0x0102; return`.
fn wide_group(load: u8, operand: &[u8], arith: u8, tail: &[u8]) -> Vec<u8> {
    let mut code = vec![load, 0x59, 0xb4, 0x01, 0x02];
    code.extend_from_slice(operand);
    code.push(arith);
    code.extend_from_slice(tail);
    code.extend_from_slice(&[0xb5, 0x01, 0x02, 0xb1]);
    code
}

fn run_mode(
    zgc: &crate::memory::vm_heap::VmHeap,
    sites: &mut PinnedFast,
    f: &mut Frame,
    mode: u8,
) -> Option<usize> {
    let code_len = f.code.len() - 2;
    sites.run(|cache| fused_field_increment_on(zgc, cache, f, code_len, 0, mode, Some(false)))
}

/// A compact one-field receiver of `class` whose only field has storage
/// `kind` at displacement 8, with the site `fill_site` would record for
/// it under descriptor `desc`; `None` when the allocation failed or came
/// out legacy.
fn compact_receiver(
    zgc: &crate::memory::vm_heap::VmHeap,
    class: u32,
    kind: FieldStorageKind,
    desc: u8,
) -> Option<(ObjectRef, *mut u8, FastFieldSite)> {
    cratonvm_types::register_class_layout(
        zgc.layout_domain(),
        class,
        std::sync::Arc::new(cratonvm_types::CompactLayout {
            field_disps: vec![8],
            is_ref: vec![false],
            field_kinds: vec![kind],
            ref_disps: vec![],
            total_size: 16,
        }),
    );
    let obj = zgc.try_alloc_object(ClassId::new(class), 1)?;
    let ptr = obj.as_ptr() as u64;
    // SAFETY: a fresh allocation starts with its header.
    let header = unsafe { &*(ptr as *const ObjectHeader) };
    if !cratonvm_types::is_compact_object(header) {
        return None;
    }
    // SAFETY: displacement 8 is the layout's slot for the one field.
    let fp = unsafe { (ptr as *mut u8).add(8) };
    let site = FastFieldSite {
        receiver_class_id: header.class_id,
        num_slots: header.num_slots(),
        offset: 8,
        storage: Some(kind),
        field_index: 0,
        desc_byte: desc,
        is_reference: false,
        final_put: FastFieldSite::PUT_ANY,
    };
    Some((obj, fp, site))    }

#[test]
fn a_compact_narrow_field_wraps_as_the_unfused_group() {
    // (class, storage, descriptor, conversion, stored bits, operand,
    //  arith, the field's value after, as the quickened getfield reads it)
    let cases: [(u32, FieldStorageKind, u8, u8, u16, &[u8], u8, i32); 6] = [
        (
            6901,
            FieldStorageKind::Byte,
            b'B',
            0x91,
            0x7f,
            &[0x04],
            0x60,
            -128,
        ),
        (
            6902,
            FieldStorageKind::Byte,
            b'B',
            0x91,
            0x80,
            &[0x04],
            0x64,
            127,
        ),
        (
            6903,
            FieldStorageKind::Char,
            b'C',
            0x92,
            0,
            &[0x04],
            0x64,
            65535,
        ),
        (
            6904,
            FieldStorageKind::Char,
            b'C',
            0x92,
            0xffff,
            &[0x04],
            0x60,
            0,
        ),
        (
            6905,
            FieldStorageKind::Short,
            b'S',
            0x93,
            0x7fff,
            &[0x04],
            0x60,
            -32768,
        ),
        (
            6906,
            FieldStorageKind::Short,
            b'S',
            0x93,
            5,
            &[0x11, 0x03, 0xe8],
            0x64,
            -995,
        ),
    ];
    for (class, kind, desc, conv, bits, operand, arith, want) in cases {
        let zgc = VmHeap::from_zgc_capacity(1 << 20);
        let Some((obj, fp, site)) = compact_receiver(&zgc, class, kind, desc) else {
            continue;
        };
        let class_id = ClassId::new(0x00ab_0690 + class);
        let mut sites = PinnedFast::new();
        if !fill(&mut sites, class_id, site) {
            continue;
        }
        let read = || -> i32 {
            // SAFETY: `fp` is the layout's slot of the live object, of
            // the width its storage kind names.
            match kind {
                FieldStorageKind::Byte => i32::from((unsafe { load_u8(fp) }) as i8),
                FieldStorageKind::Char => i32::from(unsafe { load_u16(fp) }),
                _ => i32::from((unsafe { load_u16(fp) }) as i16),
            }
        };
        // SAFETY: as above.
        unsafe {
            if kind == FieldStorageKind::Byte {
                store_u8(fp, bits as u8);
            } else {
                store_u16(fp, bits);
            }
        }
        let before = read();
        let code = wide_group(0x2a, operand, arith, &[conv]);
        let end = code.len() - 1;
        let mut f = frame(class_id, "m", code, Value::Object(Some(obj)));
        // Without the wide switch the wave-68 rule (`I` only) declines.
        assert_eq!(run_mode(&zgc, &mut sites, &mut f, FIELD_INC_ON), None);
        assert_eq!(read(), before, "a decline writes nothing");
        assert_eq!(run_mode(&zgc, &mut sites, &mut f, WIDE), Some(end));
        assert_eq!(
            read(),
            want,
            "{} {conv:#x} {bits:#x} {arith:#x}",
            desc as char
        );
        assert_eq!(f.stack.len(), 0, "nothing pushed or popped");
    }
}

#[test]
fn a_legacy_narrow_field_is_stored_narrowed() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let class_id = ClassId::new(0x00ab_0697);
    // (descriptor, conversion, current cell, arith, want)
    let cases: [(u8, u8, Value, u8, i32); 4] = [
        (b'B', 0x91, Value::Int(127), 0x60, -128),
        (b'C', 0x92, Value::Int(0), 0x64, 65535),
        (b'S', 0x93, Value::Int(-32768), 0x64, 32767),
        (b'B', 0x91, Value::Object(None), 0x64, -1),
    ];
    for (desc, conv, current, arith, want) in cases {
        let Some((ptr, site)) = legacy_receiver(&zgc, 6907, 0) else {
            return;
        };
        // SAFETY: field 0's 16-byte cell of the live legacy object.
        unsafe {
            cratonvm_types::write_value_atomic(
                (ptr as *mut u8).add(HEADER_SIZE) as *mut Value,
                current,
            )
        };
        let mut sites = PinnedFast::new();
        if !fill(
            &mut sites,
            class_id,
            FastFieldSite {
                desc_byte: desc,
                ..site
            },
        ) {
            return;
        }
        // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
        let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
        let code = wide_group(0x2a, &[0x04], arith, &[conv]);
        let end = code.len() - 1;
        let mut f = frame(class_id, "m", code, recv);
        assert_eq!(run_mode(&zgc, &mut sites, &mut f, WIDE), Some(end));
        assert_eq!(
            legacy_value(ptr),
            Some(Value::Int(want)),
            "{}",
            desc as char
        );
    }
}

#[test]
fn a_long_field_carries_and_borrows() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    // Compact: `lconst_1; ladd` over the 32-bit carry and the top.
    if let Some((obj, fp, site)) = compact_receiver(&zgc, 6908, FieldStorageKind::Long, b'J') {
        let class_id = ClassId::new(0x00ab_0698);
        let mut sites = PinnedFast::new();
        if fill(&mut sites, class_id, site) {
            // (stored, operand, arith, want)
            let cases: [(i64, u8, u8, i64); 4] = [
                (0xffff_ffff, 0x0a, 0x61, 0x1_0000_0000),
                (i64::MAX, 0x0a, 0x61, i64::MIN),
                (i64::MIN, 0x0a, 0x65, i64::MAX),
                (42, 0x09, 0x65, 42),
            ];
            for (stored, operand, arith, want) in cases {
                // SAFETY: displacement 8 is the layout's 8-byte `Long` slot.
                unsafe { store_u64(fp, stored as u64) };
                let code = wide_group(0x2a, &[operand], arith, &[]);
                let end = code.len() - 1;
                let mut f = frame(class_id, "m", code, Value::Object(Some(obj)));
                assert_eq!(run_mode(&zgc, &mut sites, &mut f, FIELD_INC_ON), None);
                // SAFETY: as above.
                assert_eq!(unsafe { load_u64(fp) } as i64, stored);
                assert_eq!(run_mode(&zgc, &mut sites, &mut f, WIDE), Some(end));
                // SAFETY: as above.
                assert_eq!(unsafe { load_u64(fp) } as i64, want, "{stored:#x}");
            }
            // An int operation on a `J` field, a long one on an `I` field.
            let mut f = frame(
                class_id,
                "m",
                group(0x2a, &[0x04], 0x60),
                Value::Object(Some(obj)),
            );
            assert_eq!(run_mode(&zgc, &mut sites, &mut f, WIDE), None);
        }
    }
    // Legacy: a `Long` cell, and an `Int` the quickened read widens.
    let class_id = ClassId::new(0x00ab_0699);
    for (current, want) in [
        (Value::Long(0x7fff_ffff), 0x8000_0000_i64),
        (Value::Int(-1), 0),
        (Value::Object(None), 1),
    ] {
        let Some((ptr, site)) = legacy_receiver(&zgc, 6909, 0) else {
            return;
        };
        // SAFETY: field 0's 16-byte cell of the live legacy object.
        unsafe {
            cratonvm_types::write_value_atomic(
                (ptr as *mut u8).add(HEADER_SIZE) as *mut Value,
                current,
            )
        };
        let mut sites = PinnedFast::new();
        if !fill(
            &mut sites,
            class_id,
            FastFieldSite {
                desc_byte: b'J',
                ..site
            },
        ) {
            return;
        }
        // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
        let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
        let mut f = frame(class_id, "m", wide_group(0x2a, &[0x0a], 0x61, &[]), recv);
        assert!(run_mode(&zgc, &mut sites, &mut f, WIDE).is_some());
        assert_eq!(legacy_value(ptr), Some(Value::Long(want)));
        // The same long group on an `I` field declines.
        let mut int_sites = PinnedFast::new();
        if fill(&mut int_sites, class_id, site) {
            assert_eq!(run_mode(&zgc, &mut int_sites, &mut f, WIDE), None);
            assert_eq!(legacy_value(ptr), Some(Value::Long(want)));
        }
    }
}

#[test]
fn a_long_local_operand_is_read_only_when_marked() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let class_id = ClassId::new(0x00ab_069a);
    let Some((ptr, site)) = legacy_receiver(&zgc, 6910, 0) else {
        return;
    };
    // SAFETY: field 0's 16-byte cell of the live legacy object.
    unsafe {
        cratonvm_types::write_value_atomic(
            (ptr as *mut u8).add(HEADER_SIZE) as *mut Value,
            Value::Long(10),
        )
    };
    let mut sites = PinnedFast::new();
    if !fill(
        &mut sites,
        class_id,
        FastFieldSite {
            desc_byte: b'J',
            ..site
        },
    ) {
        return;
    }
    // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
    let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
    // An instance `(J)V` frame: local 0 the receiver, locals 1-2 a long.
    let mut f = Frame::new(
        class_id,
        "cratonvm/test/i68l7/FieldIncrement".to_string(),
        "m".to_string(),
        "(J)V".to_string(),
        None,
        wide_group(0x2a, &[0x1f], 0x65, &[]),
        vec![],
        6,
        3,
        &[recv, Value::Long(3)],
    );
    let hit = run_mode(&zgc, &mut sites, &mut f, WIDE);
    if f.local_is_long_marked(1) {
        assert!(hit.is_some());
        assert_eq!(legacy_value(ptr), Some(Value::Long(7)));
    } else {
        assert_eq!(hit, None);
        assert_eq!(legacy_value(ptr), Some(Value::Long(10)));
    }
    // `lload_1` over the frame whose local 1 is an int declines.
    let mut g = frame(class_id, "m", wide_group(0x2a, &[0x1f], 0x65, &[]), recv);
    let before = legacy_value(ptr);
    assert_eq!(run_mode(&zgc, &mut sites, &mut g, WIDE), None);
    assert_eq!(legacy_value(ptr), before);
}

#[test]
fn a_conversion_not_the_fields_own_declines() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let class_id = ClassId::new(0x00ab_069b);
    let Some((ptr, site)) = legacy_receiver(&zgc, 6911, 7) else {
        return;
    };
    // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
    let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
    for (desc, conv) in [
        (b'B', 0x92_u8),
        (b'B', 0x93),
        (b'C', 0x91),
        (b'S', 0x92),
        (b'I', 0x91),
        (b'Z', 0x91),
        (b'J', 0x91),
    ] {
        let mut sites = PinnedFast::new();
        if !fill(
            &mut sites,
            class_id,
            FastFieldSite {
                desc_byte: desc,
                ..site
            },
        ) {
            continue;
        }
        let mut f = frame(
            class_id,
            "m",
            wide_group(0x2a, &[0x04], 0x60, &[conv]),
            recv,
        );
        assert_eq!(
            run_mode(&zgc, &mut sites, &mut f, WIDE),
            None,
            "{} {conv:#x}",
            desc as char
        );
    }
    // A narrow field without its conversion (what javac never emits).
    for desc in *b"BCSZ" {
        let mut sites = PinnedFast::new();
        if fill(
            &mut sites,
            class_id,
            FastFieldSite {
                desc_byte: desc,
                ..site
            },
        ) {
            let mut f = frame(class_id, "m", group(0x2a, &[0x04], 0x60), recv);
            assert_eq!(
                run_mode(&zgc, &mut sites, &mut f, WIDE),
                None,
                "{}",
                desc as char
            );
        }
    }
    assert_eq!(legacy_value(ptr), Some(Value::Int(7)));
}

/// Groups the early-decline screen must agree with the body on.
fn screen_corpus() -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let operands: [&[u8]; 12] = [
        &[0x02],
        &[0x04],
        &[0x10, 0x80],
        &[0x11, 0x03, 0xe8],
        &[0x15, 0x01],
        &[0x15, 0x09],
        &[0x1b],
        &[0x09],
        &[0x0a],
        &[0x16, 0x01],
        &[0x1f],
        &[0x12, 0x01],
    ];
    for operand in operands {
        for arith in [0x60_u8, 0x64, 0x61, 0x65, 0x68] {
            for tail in [&[][..], &[0x91][..], &[0x92][..], &[0x93][..], &[0x88][..]] {
                out.push(wide_group(0x2a, operand, arith, tail));
            }
        }
    }
    // A `dup_x2` where the operand would be (never fused).
    out.push(vec![
        0x2a, 0x59, 0xb4, 0x01, 0x02, 0x5b, 0x04, 0x60, 0xb5, 0x01, 0x02, 0xb1,
    ]);
    // The value forms (wave 71, `FIELD_INC_VALUE`): post / pre over an
    // int, a long and a byte group.
    out.push(vec![
        0x2a, 0x59, 0xb4, 0x01, 0x02, 0x5a, 0x04, 0x60, 0xb5, 0x01, 0x02, 0xb1,
    ]);
    out.push(vec![
        0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60, 0x5a, 0xb5, 0x01, 0x02, 0xb1,
    ]);
    out.push(vec![
        0x2a, 0x59, 0xb4, 0x01, 0x02, 0x5d, 0x0a, 0x61, 0xb5, 0x01, 0x02, 0xb1,
    ]);
    out.push(vec![
        0x2a, 0x59, 0xb4, 0x01, 0x02, 0x0a, 0x61, 0x5d, 0xb5, 0x01, 0x02, 0xb1,
    ]);
    out.push(vec![
        0x2a, 0x59, 0xb4, 0x01, 0x02, 0x5a, 0x04, 0x60, 0x91, 0xb5, 0x01, 0x02, 0xb1,
    ]);
    out.push(vec![
        0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60, 0x91, 0x5a, 0xb5, 0x01, 0x02, 0xb1,
    ]);
    // A value form whose duplicate is the other width.
    out.push(vec![
        0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60, 0x5d, 0xb5, 0x01, 0x02, 0xb1,
    ]);
    // Another `putfield` index, a `putstatic`, cut short.
    out.push(vec![
        0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01, 0x03, 0xb1,
    ]);
    out.push(vec![
        0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb3, 0x01, 0x02, 0xb1,
    ]);
    out.push(vec![0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01]);
    out
}

#[test]
fn the_early_decline_screen_changes_no_answer() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let class_id = ClassId::new(0x00ab_069c);
    let Some((ptr, site)) = legacy_receiver(&zgc, 6912, 41) else {
        return;
    };
    // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
    let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
    let reset = || {
        // SAFETY: field 0's 16-byte cell of the live legacy object.
        unsafe {
            cratonvm_types::write_value_atomic(
                (ptr as *mut u8).add(HEADER_SIZE) as *mut Value,
                Value::Int(41),
            )
        }
    };
    for desc in *b"IBCSJ" {
        let mut sites = PinnedFast::new();
        if !fill(
            &mut sites,
            class_id,
            FastFieldSite {
                desc_byte: desc,
                ..site
            },
        ) {
            continue;
        }
        for code in screen_corpus() {
            let mut f = frame(class_id, "m", code.clone(), recv);
            for base in [FIELD_INC_ON, WIDE, WIDE | FIELD_INC_VALUE] {
                reset();
                let plain = run_mode(&zgc, &mut sites, &mut f, base);
                let plain_value = legacy_value(ptr);
                reset();
                let screened =
                    run_mode(&zgc, &mut sites, &mut f, base | FIELD_INC_EARLY_DECLINE);
                assert_eq!(
                    (plain, plain_value),
                    (screened, legacy_value(ptr)),
                    "{} {base:#x} {code:x?}",
                    desc as char
                );
                if plain.is_some() {
                    let code_len = f.code.len() - 2;
                    let wide = base & FIELD_INC_WIDE != 0;
                    let value = base & FIELD_INC_VALUE != 0;
                    assert!(field_increment_screen(
                        &f.code, code_len, 0, wide, false, value, false
                    ));
                }
            }
        }
    }
}

#[test]
fn the_screen_refuses_on_bytes_alone() {
    let class_id = ClassId::new(0x00ab_069d);
    let screen = |code: Vec<u8>, wide: bool| {
        let f = frame(class_id, "m", code, Value::Object(None));
        let code_len = f.code.len() - 2;
        field_increment_screen(&f.code, code_len, 0, wide, false, false, false)
    };
    assert!(screen(group(0x2a, &[0x04], 0x60), false));
    assert!(
        screen(group(0x2d, &[0x15, 0x09], 0x64), false),
        "the local is not read"
    );
    // A `dup_x2` after the getfield, `b++` without the wide switch, `l +=
    // 1` likewise.
    assert!(!screen(
        vec![0x2a, 0x59, 0xb4, 0x01, 0x02, 0x5b, 0x04, 0x60, 0xb5, 0x01, 0x02, 0xb1],
        true
    ));
    assert!(!screen(wide_group(0x2a, &[0x04], 0x60, &[0x91]), false));
    assert!(screen(wide_group(0x2a, &[0x04], 0x60, &[0x91]), true));
    assert!(!screen(wide_group(0x2a, &[0x0a], 0x61, &[]), false));
    assert!(screen(wide_group(0x2a, &[0x0a], 0x61, &[]), true));
    // `aload n`, another index, cut short.
    assert!(!screen(group(0x19, &[0x04], 0x60), true));
    assert!(!screen(
        vec![0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01, 0x03, 0xb1],
        true
    ));
    assert!(!screen(
        vec![0x2a, 0x59, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01],
        true
    ));
}

#[test]
fn the_mode_byte_packs_the_switches() {
    assert_eq!(
        field_increment_mode(false, true, true, true, true, true, true),
        0
    );
    assert_eq!(
        field_increment_mode(true, false, false, false, false, false, false),
        FIELD_INC_ON
    );
    assert_eq!(
        field_increment_mode(true, true, true, true, true, false, false),
        FIELD_INC_ON
            | FIELD_INC_CENSUS
            | FIELD_INC_PHASES
            | FIELD_INC_WIDE
            | FIELD_INC_EARLY_DECLINE
    );
}

// ── Wave 70, lane L7: `CRATONVM_INTERP_FUSED_FIELD_INCREMENT_NO_DUP` ──

/// `<load>; <load>; getfield #0x0102; <operand>; <arith>; <tail...>;
/// putfield #<put_lo>; return` (`count = count + k`).
fn no_dup_group(load: u8, operand: &[u8], arith: u8, tail: &[u8], put_lo: u8) -> Vec<u8> {
    let mut code = vec![load, load, 0xb4, 0x01, 0x02];
    code.extend_from_slice(operand);
    code.push(arith);
    code.extend_from_slice(tail);
    code.extend_from_slice(&[0xb5, 0x01, put_lo, 0xb1]);
    code
}

#[test]
fn the_mode_byte_carries_the_no_dup_bit() {
    assert_eq!(
        field_increment_mode(false, false, false, false, false, true, false),
        0
    );
    assert_eq!(
        field_increment_mode(true, false, false, false, false, true, false),
        FIELD_INC_ON | FIELD_INC_NO_DUP
    );
    // The bit is its own: no other switch's bit is shared with it.
    for other in [
        FIELD_INC_CENSUS,
        FIELD_INC_PHASES,
        FIELD_INC_WIDE,
        FIELD_INC_EARLY_DECLINE,
    ] {
        assert_eq!(other & FIELD_INC_NO_DUP, 0);
    }
    assert_eq!(FIELD_INC_ON & FIELD_INC_NO_DUP, 0);
}

#[test]
fn the_decoder_takes_the_repeated_load_only_under_no_dup() {
    let class_id = ClassId::new(0x00ab_0701);
    for load in 0x2a..=0x2d_u8 {
        for (arith, subtract) in [(0x60_u8, false), (0x64_u8, true)] {
            let code = no_dup_group(load, &[0x04], arith, &[], 0x02);
            let end = code.len() - 1;
            let f = frame(class_id, "m", code, Value::Object(None));
            let code_len = f.code.len() - 2;
            assert_eq!(
                decode_field_increment(&f, &f.code, code_len, 0, false, false),
                None
            );
            assert_eq!(
                decode_field_increment(&f, &f.code, code_len, 0, false, true),
                Some((
                    CP,
                    usize::from(load - 0x2a),
                    crate::runtime::interpreter::IncrementOperand::Int(1),
                    subtract,
                    None,
                    end
                )),
                "{load:#x} {arith:#x}"
            );
        }
    }
    let refuses = |code: Vec<u8>| {
        let f = frame(class_id, "m", code, Value::Object(None));
        let code_len = f.code.len() - 2;
        assert_eq!(
            decode_field_increment(&f, &f.code, code_len, 0, true, true),
            None
        );
    };
    // Two different locals (`this.a = other.a + 1` is not one receiver).
    refuses(vec![
        0x2a, 0x2b, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01, 0x02, 0xb1,
    ]);
    // `this.a = this.b + 1`: two different pool indexes.
    refuses(no_dup_group(0x2a, &[0x04], 0x60, &[], 0x03));
    // `aload n` twice: not the short load.
    refuses(vec![
        0x19, 0x19, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01, 0x02, 0xb1,
    ]);
    // The dup form is still taken with the bit set.
    let f = frame(
        class_id,
        "m",
        group(0x2a, &[0x04], 0x60),
        Value::Object(None),
    );
    let code_len = f.code.len() - 2;
    assert!(decode_field_increment(&f, &f.code, code_len, 0, false, true).is_some());
}

#[test]
fn the_no_dup_group_stores_what_the_unfused_group_stores() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let class_id = ClassId::new(0x00ab_0702);
    // (current, operand, arith, want)
    let cases: [(i32, &[u8], u8, i32); 4] = [
        (7, &[0x04], 0x60, 8),
        (i32::MAX, &[0x04], 0x60, i32::MIN),
        (i32::MIN, &[0x04], 0x64, i32::MAX),
        (10, &[0x1b], 0x64, 5),
    ];
    for (current, operand, arith, want) in cases {
        let Some((ptr, site)) = legacy_receiver(&zgc, 7002, current) else {
            return;
        };
        let mut sites = PinnedFast::new();
        if !fill(&mut sites, class_id, site) {
            return;
        }
        // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
        let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
        let code = no_dup_group(0x2a, operand, arith, &[], 0x02);
        let end = code.len() - 1;
        let mut f = frame(class_id, "m", code, recv);
        // Without the bit the prefix is not the helper's: a decline.
        assert_eq!(run_mode(&zgc, &mut sites, &mut f, FIELD_INC_ON), None);
        assert_eq!(
            legacy_value(ptr),
            Some(Value::Int(current)),
            "a decline writes nothing"
        );
        for extra in [0, FIELD_INC_EARLY_DECLINE] {
            // SAFETY: field 0's 16-byte cell of the live legacy object.
            unsafe {
                cratonvm_types::write_value_atomic(
                    (ptr as *mut u8).add(HEADER_SIZE) as *mut Value,
                    Value::Int(current),
                )
            };
            let mode = FIELD_INC_ON | FIELD_INC_NO_DUP | extra;
            assert_eq!(run_mode(&zgc, &mut sites, &mut f, mode), Some(end));
            assert_eq!(
                legacy_value(ptr),
                Some(Value::Int(want)),
                "{current} {arith:#x}"
            );
            assert_eq!(f.stack.len(), 0, "nothing pushed or popped");
        }
    }
}

#[test]
fn the_no_dup_group_declines_as_the_dup_group_declines() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let class_id = ClassId::new(0x00ab_0703);
    let mode = FIELD_INC_ON | FIELD_INC_NO_DUP;
    // A null receiver: nothing written, the unfused `getfield` throws.
    let mut sites = PinnedFast::new();
    let code = no_dup_group(0x2a, &[0x04], 0x60, &[], 0x02);
    let mut f = frame(class_id, "m", code, Value::Object(None));
    assert_eq!(run_mode(&zgc, &mut sites, &mut f, mode), None);
    let Some((ptr, site)) = legacy_receiver(&zgc, 7003, 41) else {
        return;
    };
    if !fill(&mut sites, class_id, site) {
        return;
    }
    // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
    let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
    // Two fields (`a = b + 1`), a watch armed, the field-phase instrument,
    // a narrow conversion without the wide switch.
    let mut two = frame(
        class_id,
        "m",
        no_dup_group(0x2a, &[0x04], 0x60, &[], 0x03),
        recv,
    );
    assert_eq!(run_mode(&zgc, &mut sites, &mut two, mode), None);
    assert_eq!(
        run_mode(&zgc, &mut sites, &mut two, mode | FIELD_INC_EARLY_DECLINE),
        None
    );
    let mut f = frame(
        class_id,
        "m",
        no_dup_group(0x2a, &[0x04], 0x60, &[], 0x02),
        recv,
    );
    let code_len = f.code.len() - 2;
    let watched =
        fused_field_increment_on(&zgc, &mut sites, &mut f, code_len, 0, mode, Some(true));
    assert_eq!(watched, None);
    assert_eq!(
        run_mode(&zgc, &mut sites, &mut f, mode | FIELD_INC_PHASES),
        None
    );
    let code = no_dup_group(0x2a, &[0x04], 0x60, &[0x91], 0x02);
    let mut narrow = frame(class_id, "m", code, recv);
    assert_eq!(run_mode(&zgc, &mut sites, &mut narrow, mode), None);
    assert_eq!(legacy_value(ptr), Some(Value::Int(41)));
}

#[test]
fn the_screen_takes_the_repeated_load_only_under_no_dup() {
    let class_id = ClassId::new(0x00ab_0704);
    let screen = |code: Vec<u8>, no_dup: bool| {
        let f = frame(class_id, "m", code, Value::Object(None));
        let code_len = f.code.len() - 2;
        field_increment_screen(&f.code, code_len, 0, false, no_dup, false, false)
    };
    assert!(!screen(no_dup_group(0x2b, &[0x04], 0x60, &[], 0x02), false));
    assert!(screen(no_dup_group(0x2b, &[0x04], 0x60, &[], 0x02), true));
    assert!(!screen(no_dup_group(0x2b, &[0x04], 0x60, &[], 0x03), true));
    assert!(!screen(
        vec![0x2a, 0x2b, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01, 0x02, 0xb1],
        true
    ));
    assert!(screen(group(0x2a, &[0x04], 0x60), true));
}

// ── Round i3 wave 71, lane L7: the loop's no-dup screen and
// `CRATONVM_INTERP_FUSED_FIELD_INCREMENT_VALUE` ──

#[test]
fn the_no_dup_loop_screen_passes_one_field_and_refuses_two() {
    let screen = |code: Vec<u8>| no_dup_group_may_fuse(&code[..], 0, false);
    // One field: every operand form, both operations, the narrowing and
    // long shapes (the screen takes the widest rule).
    let operands: [&[u8]; 5] = [
        &[0x04],
        &[0x10, 0x80],
        &[0x11, 0x03, 0xe8],
        &[0x15, 0x01],
        &[0x1b],
    ];
    for operand in operands {
        for arith in [0x60_u8, 0x64] {
            assert!(screen(no_dup_group(0x2a, operand, arith, &[], 0x02)));
            assert!(screen(no_dup_group(0x2a, operand, arith, &[0x91], 0x02)));
            // `this.a = this.b + k`: the `putfield` names another index.
            assert!(!screen(no_dup_group(0x2a, operand, arith, &[], 0x03)));
            assert!(!screen(no_dup_group(0x2a, operand, arith, &[0x91], 0x03)));
        }
    }
    assert!(screen(no_dup_group(0x2b, &[0x0a], 0x61, &[], 0x02)));
    assert!(screen(no_dup_group(0x2b, &[0x16, 0x01], 0x65, &[], 0x02)));
    assert!(!screen(no_dup_group(0x2b, &[0x0a], 0x61, &[], 0x03)));
    // `this.a = this.b + this.c` (the operand is a load), a `putstatic`,
    // and a group cut by the end of the code.
    assert!(!screen(no_dup_group(0x2a, &[0x2a], 0x60, &[], 0x02)));
    // The arithmetic opcode is the helper's to check (`imul` passes the
    // loop screen and the decoder refuses it).
    assert!(screen(no_dup_group(0x2a, &[0x04], 0x68, &[], 0x02)));
    assert!(!screen(vec![
        0x2a, 0x2a, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb3, 0x01, 0x02, 0xb1
    ]));
    assert!(!screen(vec![
        0x2a, 0x2a, 0xb4, 0x01, 0x02, 0x04, 0x60, 0xb5, 0x01
    ]));
    assert!(!screen(vec![0x2a, 0x2a, 0xb4, 0x01, 0x02]));
}

#[test]
fn the_no_dup_loop_screen_never_refuses_what_the_helper_fuses() {
    let class_id = ClassId::new(0x00ab_0711);
    for code in screen_corpus() {
        // The corpus is `dup`-prefixed; the same groups opened
        // `aload_0; aload_0`.
        let mut code = code;
        code[1] = code[0];
        let f = frame(class_id, "m", code.clone(), Value::Object(None));
        let code_len = f.code.len() - 2;
        for wide in [false, true] {
            if decode_field_increment(&f, &f.code, code_len, 0, wide, true).is_some() {
                assert!(no_dup_group_may_fuse(&code[..], 0, false), "{code:x?}");
            }
        }
    }
}

#[test]
fn the_mode_byte_carries_the_value_bit() {
    assert_eq!(
        field_increment_mode(false, false, false, false, false, false, true),
        0
    );
    assert_eq!(
        field_increment_mode(true, false, false, false, false, false, true),
        FIELD_INC_ON | FIELD_INC_VALUE
    );
    for other in [
        FIELD_INC_CENSUS,
        FIELD_INC_PHASES,
        FIELD_INC_WIDE,
        FIELD_INC_EARLY_DECLINE,
        FIELD_INC_NO_DUP,
        FIELD_INC_ON,
    ] {
        assert_eq!(other & FIELD_INC_VALUE, 0);
    }
}

/// `aload_0; dup; getfield #0x0102; [dup_x1 | dup2_x1]; <operand>;
/// <arith>; <tail...>; putfield #0x0102; return`: the post-increment
/// whose old value is used (`post` is the duplicate opcode).
fn post_group(post: u8, operand: &[u8], arith: u8, tail: &[u8]) -> Vec<u8> {
    let mut code = vec![0x2a, 0x59, 0xb4, 0x01, 0x02, post];
    code.extend_from_slice(operand);
    code.push(arith);
    code.extend_from_slice(tail);
    code.extend_from_slice(&[0xb5, 0x01, 0x02, 0xb1]);
    code
}

/// The pre-increment whose new value is used: `tail` ends with the
/// duplicate (`dup_x1` / `dup2_x1`).
fn pre_group(operand: &[u8], arith: u8, tail: &[u8]) -> Vec<u8> {
    wide_group(0x2a, operand, arith, tail)
}

#[test]
fn the_decoder_takes_the_value_forms_only_under_the_bit() {
    let class_id = ClassId::new(0x00ab_0712);
    type Use = FieldIncrementUse;
    let int1 = crate::runtime::interpreter::IncrementOperand::Int(1);
    let long1 = crate::runtime::interpreter::IncrementOperand::Long(1);
    // (code, wide needed, operand, conversion, use)
    let cases: Vec<(Vec<u8>, bool, _, Option<u8>, Use)> = vec![
        (
            post_group(0x5a, &[0x04], 0x60, &[]),
            false,
            int1,
            None,
            Use::Old,
        ),
        (
            pre_group(&[0x04], 0x60, &[0x5a]),
            false,
            int1,
            None,
            Use::New,
        ),
        (
            post_group(0x5a, &[0x04], 0x60, &[0x91]),
            true,
            int1,
            Some(0x91),
            Use::Old,
        ),
        (
            pre_group(&[0x04], 0x60, &[0x93, 0x5a]),
            true,
            int1,
            Some(0x93),
            Use::New,
        ),
        (
            post_group(0x5d, &[0x0a], 0x61, &[]),
            true,
            long1,
            None,
            Use::Old,
        ),
        (
            pre_group(&[0x0a], 0x61, &[0x5d]),
            true,
            long1,
            None,
            Use::New,
        ),
    ];
    for (code, needs_wide, operand, narrow, used) in cases {
        let end = code.len() - 1;
        let f = frame(class_id, "m", code.clone(), Value::Object(None));
        let code_len = f.code.len() - 2;
        let decode = |wide: bool, value: bool| {
            decode_field_increment_used(&f, &f.code, code_len, 0, wide, false, value, None)
        };
        assert_eq!(decode(true, false), None, "no bit: {code:x?}");
        if needs_wide {
            assert_eq!(decode(false, true), None, "no wide: {code:x?}");
        }
        assert_eq!(
            decode(true, true),
            Some((CP, 0, operand, false, narrow, end, used)),
            "{code:x?}"
        );
        // The early-decline screen passes what the decoder takes.
        assert!(field_increment_screen(
            &f.code, code_len, 0, true, false, true, false
        ));
        assert!(!field_increment_screen(
            &f.code, code_len, 0, true, false, false, false
        ));
    }
    // The statement form is still a statement under the bit.
    let f = frame(
        class_id,
        "m",
        group(0x2a, &[0x04], 0x60),
        Value::Object(None),
    );
    let code_len = f.code.len() - 2;
    assert_eq!(
        decode_field_increment_used(&f, &f.code, code_len, 0, true, false, true, None)
            .map(|d| d.6),
        Some(Use::Statement)
    );
    let refuses = |code: Vec<u8>| {
        let f = frame(class_id, "m", code.clone(), Value::Object(None));
        let code_len = f.code.len() - 2;
        assert_eq!(
            decode_field_increment_used(&f, &f.code, code_len, 0, true, true, true, None),
            None,
            "{code:x?}"
        );
    };
    // A duplicate of the other width, both duplicates, `dup_x2`, `dup`.
    refuses(post_group(0x5d, &[0x04], 0x60, &[]));
    refuses(post_group(0x5a, &[0x0a], 0x61, &[]));
    refuses(pre_group(&[0x04], 0x60, &[0x5d]));
    refuses(pre_group(&[0x0a], 0x61, &[0x5a]));
    refuses(post_group(0x5a, &[0x04], 0x60, &[0x5a]));
    refuses(post_group(0x5b, &[0x04], 0x60, &[]));
    refuses(pre_group(&[0x04], 0x60, &[0x59]));
    // The value forms follow `dup` only, never the `aload_N; aload_N`
    // prefix (not measured; `x = count = count + 1` is rare).
    let mut no_dup_pre = pre_group(&[0x04], 0x60, &[0x5a]);
    no_dup_pre[1] = 0x2a;
    refuses(no_dup_pre);
    // Another index after the duplicate.
    refuses(vec![
        0x2a, 0x59, 0xb4, 0x01, 0x02, 0x5a, 0x04, 0x60, 0xb5, 0x01, 0x03, 0xb1,
    ]);
}

#[test]
fn a_value_form_hit_leaves_what_the_unfused_group_leaves() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let class_id = ClassId::new(0x00ab_0713);
    let value = FIELD_INC_ON | FIELD_INC_VALUE;
    // (code, field before, field after, the int left on the stack)
    let cases: [(Vec<u8>, i32, i32, i32); 4] = [
        (post_group(0x5a, &[0x04], 0x60, &[]), 41, 42, 41),
        (pre_group(&[0x04], 0x60, &[0x5a]), 41, 42, 42),
        (
            post_group(0x5a, &[0x04], 0x64, &[]),
            i32::MIN,
            i32::MAX,
            i32::MIN,
        ),
        (
            pre_group(&[0x1b], 0x60, &[0x5a]),
            i32::MAX,
            i32::MIN.wrapping_add(4),
            i32::MIN + 4,
        ),
    ];
    for (code, before, after, left) in cases {
        let Some((ptr, site)) = legacy_receiver(&zgc, 7101, before) else {
            return;
        };
        let mut sites = PinnedFast::new();
        if !fill(&mut sites, class_id, site) {
            return;
        }
        // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
        let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
        let end = code.len() - 1;
        let mut f = frame(class_id, "m", code.clone(), recv);
        f.stack.push_int(7).unwrap();
        // Without the bit: a decline, nothing written, nothing pushed.
        assert_eq!(run_mode(&zgc, &mut sites, &mut f, FIELD_INC_ON), None);
        assert_eq!(legacy_value(ptr), Some(Value::Int(before)));
        assert_eq!(f.stack.len(), 1);
        for extra in [0, FIELD_INC_EARLY_DECLINE] {
            // SAFETY: field 0's 16-byte cell of the live legacy object.
            unsafe {
                cratonvm_types::write_value_atomic(
                    (ptr as *mut u8).add(HEADER_SIZE) as *mut Value,
                    Value::Int(before),
                )
            };
            assert_eq!(run_mode(&zgc, &mut sites, &mut f, value | extra), Some(end));
            assert_eq!(legacy_value(ptr), Some(Value::Int(after)), "{code:x?}");
            assert_eq!(f.stack.len(), 2, "one value pushed");
            assert_eq!(f.stack.pop_int_unchecked(), left, "{code:x?}");
            assert_eq!(f.stack.pop_int_unchecked(), 7, "the slot below untouched");
            f.stack.push_int(7).unwrap();
        }
    }
}

#[test]
fn a_narrow_value_form_leaves_the_extended_old_or_the_narrowed_new() {
    // (class, storage, descriptor, conversion, stored bits, post?, the
    //  field after as the quickened getfield reads it, the int left)
    let cases: [(u32, FieldStorageKind, u8, u8, u16, bool, i32, i32); 4] = [
        (
            7102,
            FieldStorageKind::Byte,
            b'B',
            0x91,
            0x7f,
            false,
            -128,
            -128,
        ),
        (
            7103,
            FieldStorageKind::Byte,
            b'B',
            0x91,
            0x7f,
            true,
            -128,
            127,
        ),
        (
            7104,
            FieldStorageKind::Char,
            b'C',
            0x92,
            0xffff,
            true,
            0,
            65535,
        ),
        (
            7105,
            FieldStorageKind::Short,
            b'S',
            0x93,
            0x7fff,
            false,
            -32768,
            -32768,
        ),
    ];
    for (class, kind, desc, conv, bits, post, want, left) in cases {
        let zgc = VmHeap::from_zgc_capacity(1 << 20);
        let Some((obj, fp, site)) = compact_receiver(&zgc, class, kind, desc) else {
            continue;
        };
        let class_id = ClassId::new(0x00ab_0720 + class);
        let mut sites = PinnedFast::new();
        if !fill(&mut sites, class_id, site) {
            continue;
        }
        // SAFETY: `fp` is the layout's slot of the live object, of the
        // width its storage kind names.
        unsafe {
            if kind == FieldStorageKind::Byte {
                store_u8(fp, bits as u8);
            } else {
                store_u16(fp, bits);
            }
        }
        let code = if post {
            post_group(0x5a, &[0x04], 0x60, &[conv])
        } else {
            pre_group(&[0x04], 0x60, &[conv, 0x5a])
        };
        let end = code.len() - 1;
        let mut f = frame(class_id, "m", code, Value::Object(Some(obj)));
        let mode = FIELD_INC_ON | FIELD_INC_WIDE | FIELD_INC_VALUE;
        assert_eq!(run_mode(&zgc, &mut sites, &mut f, mode), Some(end));
        // SAFETY: as above.
        let read = match kind {
            FieldStorageKind::Byte => i32::from((unsafe { load_u8(fp) }) as i8),
            FieldStorageKind::Char => i32::from(unsafe { load_u16(fp) }),
            _ => i32::from((unsafe { load_u16(fp) }) as i16),
        };
        assert_eq!(read, want, "{} post={post}", desc as char);
        assert_eq!(f.stack.len(), 1);
        assert_eq!(
            f.stack.pop_int_unchecked(),
            left,
            "{} post={post}",
            desc as char
        );
    }
}

#[test]
fn a_long_value_form_leaves_one_long_slot() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let class_id = ClassId::new(0x00ab_0714);
    let mode = FIELD_INC_ON | FIELD_INC_WIDE | FIELD_INC_VALUE;
    // (code, before, after, left)
    let cases: [(Vec<u8>, i64, i64, i64); 2] = [
        (
            post_group(0x5d, &[0x0a], 0x61, &[]),
            0xffff_ffff,
            0x1_0000_0000,
            0xffff_ffff,
        ),
        (
            pre_group(&[0x0a], 0x65, &[0x5d]),
            i64::MIN,
            i64::MAX,
            i64::MAX,
        ),
    ];
    for (code, before, after, left) in cases {
        let Some((ptr, site)) = legacy_receiver(&zgc, 7106, 0) else {
            return;
        };
        // SAFETY: field 0's 16-byte cell of the live legacy object.
        unsafe {
            cratonvm_types::write_value_atomic(
                (ptr as *mut u8).add(HEADER_SIZE) as *mut Value,
                Value::Long(before),
            )
        };
        let mut sites = PinnedFast::new();
        if !fill(
            &mut sites,
            class_id,
            FastFieldSite {
                desc_byte: b'J',
                ..site
            },
        ) {
            return;
        }
        // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
        let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
        let end = code.len() - 1;
        let mut f = frame(class_id, "m", code, recv);
        assert_eq!(run_mode(&zgc, &mut sites, &mut f, mode), Some(end));
        assert_eq!(legacy_value(ptr), Some(Value::Long(after)));
        assert_eq!(
            f.stack.len(),
            1,
            "one slot, as the quickened `J` getfield pushes"
        );
        let (_, kind) = f.stack.peek_with_kind_unchecked();
        assert_eq!(kind, ValueStack::KIND_MARK_LONG);
        assert_eq!(f.stack.pop_long_unchecked(), left);
    }
}

#[test]
fn a_value_form_declines_on_a_full_stack_having_written_nothing() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let class_id = ClassId::new(0x00ab_0715);
    let Some((ptr, site)) = legacy_receiver(&zgc, 7107, 41) else {
        return;
    };
    let mut sites = PinnedFast::new();
    if !fill(&mut sites, class_id, site) {
        return;
    }
    // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
    let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
    let mode = FIELD_INC_ON | FIELD_INC_VALUE;
    let mut f = frame(class_id, "m", pre_group(&[0x04], 0x60, &[0x5a]), recv);
    while f.stack.has_room() {
        f.stack.push_int(0).unwrap();
    }
    let full = f.stack.len();
    assert_eq!(run_mode(&zgc, &mut sites, &mut f, mode), None);
    assert_eq!(legacy_value(ptr), Some(Value::Int(41)), "nothing stored");
    assert_eq!(f.stack.len(), full);
    // The statement form pushes nothing, so a full stack does not matter.
    let mut g = frame(class_id, "m", group(0x2a, &[0x04], 0x60), recv);
    while g.stack.has_room() {
        g.stack.push_int(0).unwrap();
    }
    assert!(run_mode(&zgc, &mut sites, &mut g, mode).is_some());
    assert_eq!(legacy_value(ptr), Some(Value::Int(42)));
}

#[test]
fn a_value_form_declines_as_the_statement_form_declines() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let class_id = ClassId::new(0x00ab_0716);
    let mode = FIELD_INC_ON | FIELD_INC_VALUE;
    // A null receiver: nothing pushed, the unfused `getfield` throws.
    let mut sites = PinnedFast::new();
    let mut null = frame(
        class_id,
        "m",
        post_group(0x5a, &[0x04], 0x60, &[]),
        Value::Object(None),
    );
    assert_eq!(run_mode(&zgc, &mut sites, &mut null, mode), None);
    assert_eq!(null.stack.len(), 0);
    let Some((ptr, site)) = legacy_receiver(&zgc, 7108, 41) else {
        return;
    };
    if !fill(&mut sites, class_id, site) {
        return;
    }
    // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
    let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
    let mut f = frame(class_id, "m", post_group(0x5a, &[0x04], 0x60, &[]), recv);
    let code_len = f.code.len() - 2;
    // A watch armed, the field-phase instrument, a final field.
    let watched =
        fused_field_increment_on(&zgc, &mut sites, &mut f, code_len, 0, mode, Some(true));
    assert_eq!(watched, None);
    assert_eq!(
        run_mode(&zgc, &mut sites, &mut f, mode | FIELD_INC_PHASES),
        None
    );
    let mut never = PinnedFast::new();
    let never_site = FastFieldSite {
        final_put: FastFieldSite::PUT_NEVER,
        ..site
    };
    if fill(&mut never, class_id, never_site) {
        assert_eq!(run_mode(&zgc, &mut never, &mut f, mode), None);
    }
    assert_eq!(f.stack.len(), 0, "nothing pushed by a decline");
    assert_eq!(legacy_value(ptr), Some(Value::Int(41)));
}

// ── Interpreter round i3 wave 83, lane L7:
// `CRATONVM_INTERP_FUSED_INCREMENT_LDC2W` (`FIELD_INC_LDC2W`) ──

/// The thread's numeric-constant table, pinned as `PinnedFast` is.
type PinnedConsts = PinnedSites<
    crate::runtime::interpreter::site_cache::PrimitiveConstant,
    crate::runtime::interpreter::site_cache::NameEpochs,
>;

/// The pool index of the `ldc2_w` the wave-83 groups load.
const K: u16 = 0x0007;

/// [`run_mode`] over both tables, with [`FIELD_INC_LDC2W`] as `ldc2w`:
/// the two pinned tables are moved into one `InterpSiteTables` for the
/// call and back after it.
fn run_ldc2w(
    zgc: &crate::memory::vm_heap::VmHeap,
    sites: &mut PinnedFast,
    consts: &mut PinnedConsts,
    f: &mut Frame,
    mode: u8,
    ldc2w: bool,
) -> Option<usize> {
    let code_len = f.code.len() - 2;
    let mode = u16::from(mode) | (if ldc2w { FIELD_INC_LDC2W } else { 0 });
    consts.run(|ccache| {
        sites.run(|scache| {
            let mut tables = InterpSiteTables::new();
            std::mem::swap(&mut tables.fast_field_sites, scache);
            std::mem::swap(&mut tables.prim_const_sites, ccache);
            let next =
                fused_field_increment(zgc, &mut tables, f, code_len, 0, mode, Some(false));
            std::mem::swap(&mut tables.fast_field_sites, scache);
            std::mem::swap(&mut tables.prim_const_sites, ccache);
            next
        })
    })
}

/// A legacy `J` receiver holding `Long(current)`, its site pinned in a
/// fresh field table, and a fresh constant table holding `constant` at
/// [`K`]; `None` when an allocation or a pin did not take.
fn ldc2w_fixture(
    zgc: &crate::memory::vm_heap::VmHeap,
    class_id: ClassId,
    current: i64,
    constant: crate::runtime::interpreter::site_cache::PrimitiveConstant,
) -> Option<(u64, Value, PinnedFast, PinnedConsts)> {
    let (ptr, site) = legacy_receiver(zgc, 6983, 0)?;
    // SAFETY: field 0's 16-byte cell of the live legacy object.
    unsafe {
        cratonvm_types::write_value_atomic(
            (ptr as *mut u8).add(HEADER_SIZE) as *mut Value,
            Value::Long(current),
        )
    };
    let mut sites = PinnedFast::new();
    if !fill(
        &mut sites,
        class_id,
        FastFieldSite {
            desc_byte: b'J',
            ..site
        },
    ) {
        return None;
    }
    let mut consts = PinnedConsts::new();
    if !consts.pin(class_id, K, constant) {
        return None;
    }
    // SAFETY: `ptr` is the live object `legacy_receiver` allocated.
    let recv = Value::Object(Some(unsafe { ObjectRef::from_raw(ptr as *mut u8) }));
    Some((ptr, recv, sites, consts))
}

#[test]
fn an_ldc2w_long_operand_is_added_only_under_its_bit_and_wide() {
    use crate::runtime::interpreter::site_cache::PrimitiveConstant;
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let class_id = ClassId::new(0x00ab_0883);
    // (arith, constant, current, want): `ladd` / `lsub` wrap as the
    // unfused pair does.
    let cases: [(u8, i64, i64, i64); 4] = [
        (0x61, 1_000_000, 5, 1_000_005),
        (0x65, 1024, 0, -1024),
        (0x61, 1, i64::MAX, i64::MIN),
        (0x65, i64::MIN, 0, i64::MIN),
    ];
    for (arith, k, current, want) in cases {
        let Some((ptr, recv, mut sites, mut consts)) =
            ldc2w_fixture(&zgc, class_id, current, PrimitiveConstant::Long(k))
        else {
            return;
        };
        let code = wide_group(0x2a, &[0x14, 0x00, 0x07], arith, &[]);
        let end = code.len() - 1;
        let mut f = frame(class_id, "m", code, recv);
        // Off (the bit clear), or the bit without the wide switch, or
        // the screen without the bit: declined, nothing written.
        assert_eq!(
            run_ldc2w(&zgc, &mut sites, &mut consts, &mut f, WIDE, false),
            None
        );
        assert_eq!(
            run_ldc2w(&zgc, &mut sites, &mut consts, &mut f, FIELD_INC_ON, true),
            None
        );
        let early = WIDE | FIELD_INC_EARLY_DECLINE;
        assert_eq!(
            run_ldc2w(&zgc, &mut sites, &mut consts, &mut f, early, false),
            None
        );
        assert_eq!(legacy_value(ptr), Some(Value::Long(current)));
        assert_eq!(f.stack.len(), 0, "nothing pushed by a decline");
        // On: the constant is added (the screen passes it too).
        assert_eq!(
            run_ldc2w(&zgc, &mut sites, &mut consts, &mut f, early, true),
            Some(end)
        );
        assert_eq!(legacy_value(ptr), Some(Value::Long(want)), "{k}");
        assert_eq!(f.stack.len(), 0, "the statement form pushes nothing");
    }
}

#[test]
fn an_ldc2w_miss_a_double_or_an_int_entry_declines() {
    use crate::runtime::interpreter::site_cache::PrimitiveConstant;
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let class_id = ClassId::new(0x00ab_0884);
    for constant in [PrimitiveConstant::Double(2.0), PrimitiveConstant::Int(3)] {
        let Some((ptr, recv, mut sites, mut consts)) =
            ldc2w_fixture(&zgc, class_id, 7, constant)
        else {
            return;
        };
        let code = wide_group(0x2a, &[0x14, 0x00, 0x07], 0x61, &[]);
        let mut f = frame(class_id, "m", code, recv);
        assert_eq!(
            run_ldc2w(&zgc, &mut sites, &mut consts, &mut f, WIDE, true),
            None
        );
        assert_eq!(legacy_value(ptr), Some(Value::Long(7)));
        // Another pool index than the one the table holds: a miss.
        let mut other = frame(
            class_id,
            "m",
            wide_group(0x2a, &[0x14, 0x00, 0x08], 0x61, &[]),
            recv,
        );
        assert_eq!(
            run_ldc2w(&zgc, &mut sites, &mut consts, &mut other, WIDE, true),
            None
        );
        assert_eq!(legacy_value(ptr), Some(Value::Long(7)));
    }
}

#[test]
fn an_ldc2w_pre_increment_leaves_the_new_value() {
    use crate::runtime::interpreter::site_cache::PrimitiveConstant;
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let class_id = ClassId::new(0x00ab_0885);
    let Some((ptr, recv, mut sites, mut consts)) =
        ldc2w_fixture(&zgc, class_id, 40, PrimitiveConstant::Long(2))
    else {
        return;
    };
    // `return this.n += 2L`: `... ldc2_w; ladd; dup2_x1; putfield`.
    let code = wide_group(0x2a, &[0x14, 0x00, 0x07], 0x61, &[0x5d]);
    let end = code.len() - 1;
    let mut f = frame(class_id, "m", code, recv);
    let mode = WIDE | FIELD_INC_VALUE;
    assert_eq!(
        run_ldc2w(&zgc, &mut sites, &mut consts, &mut f, mode, false),
        None
    );
    assert_eq!(
        run_ldc2w(&zgc, &mut sites, &mut consts, &mut f, mode, true),
        Some(end)
    );
    assert_eq!(legacy_value(ptr), Some(Value::Long(42)));
    assert_eq!(f.stack.len(), 1, "the new value, one slot");
}

#[test]
fn the_screen_takes_an_ldc2w_operand_only_under_its_flag_and_wide() {
    let class_id = ClassId::new(0x00ab_0886);
    let f = frame(
        class_id,
        "m",
        wide_group(0x2a, &[0x14, 0x00, 0x07], 0x61, &[]),
        Value::Object(None),
    );
    let code_len = f.code.len() - 2;
    assert!(field_increment_screen(
        &f.code, code_len, 0, true, false, false, true
    ));
    assert!(!field_increment_screen(
        &f.code, code_len, 0, true, false, false, false
    ));
    assert!(!field_increment_screen(
        &f.code, code_len, 0, false, false, false, true
    ));
    // Cut off inside the index: refused.
    assert!(!field_increment_screen(
        &f.code, 5, 0, true, false, false, true
    ));
    // The decoder agrees: without a table the operand is refused.
    assert!(
        decode_field_increment_used(&f, &f.code, code_len, 0, true, false, false, None)
            .is_none()
    );
}
