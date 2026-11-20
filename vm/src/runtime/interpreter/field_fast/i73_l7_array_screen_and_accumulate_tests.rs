//! Interpreter round i3 wave 73, lane L7: the fused array element
//! accumulate (`CRATONVM_INTERP_FUSED_ARRAY_ACCUMULATE`,
//! `fused_array_accumulate`). (The wave-73 byte screen's two tests left
//! with the screen in wave 75.) An accumulate hit stores what the
//! unfused `iload_S; aload_N; iload_M; <x>aload; <op>; istore_Z` stores,
//! and a decline writes nothing. Each test sets the switches it needs
//! explicitly.
use super::*;
use std::sync::Arc;

/// A static frame over `code` with `args` in its first locals,
/// `max_locals` 4.
fn frame_with(code: Vec<u8>, args: &[Value]) -> Frame {
    Frame::new(
        ClassId::new(0x00ab_0731),
        "cratonvm/test/i73l7/ArrayScreen".to_string(),
        "m".to_string(),
        "([III)V".to_string(),
        None,
        code,
        vec![],
        6,
        4,
        args,
    )
}

/// `code` followed by the two padding bytes a frame's code carries.
fn padded(code: &[u8]) -> Vec<u8> {
    let mut v = code.to_vec();
    v.extend_from_slice(&[0, 0]);
    v
}

fn vm(accumulate: bool) -> Arc<SharedVm> {
    let mut config = crate::config::VmConfig::default();
    config.interp_fused_array_accumulate = accumulate;
    config.site_census = false;
    Arc::new(SharedVm::new(config))
}

/// `iload_3; aload_0; iload_2; <load>; <arith>; <store>; return` over
/// locals 0 = `arr`, 1 = 0, 2 = `index`, 3 = `acc`.
fn accumulate_frame(
    load: u8,
    arith: u8,
    store: u8,
    arr: Value,
    index: Value,
    acc: i32,
) -> Frame {
    frame_with(
        vec![0x1d, 0x2a, 0x1c, load, arith, store, 0xb1],
        &[arr, Value::Int(0), index, Value::Int(acc)],
    )
}

#[test]
fn the_accumulate_decoder_takes_only_its_group() {
    #[allow(clippy::type_complexity)]
    fn d(code: &[u8]) -> Option<(usize, usize, usize, u8, u8, usize)> {
        decode_array_accumulate(&padded(code), code.len(), 0)
    }
    assert_eq!(
        d(&[0x1d, 0x2a, 0x1c, 0x2e, 0x60, 0x3e]),
        Some((3, 0, 2, 0x2e, 0x60, 3))
    );
    assert_eq!(
        d(&[0x1a, 0x2d, 0x1b, 0x34, 0x82, 0x3c]),
        Some((0, 3, 1, 0x34, 0x82, 1))
    );
    // laload / faload / aaload, a throwing or a shift arithmetic, a
    // long arithmetic, a store that is not `istore_0..3`, a cut group.
    assert_eq!(d(&[0x1d, 0x2a, 0x1c, 0x2f, 0x61, 0x3e]), None);
    assert_eq!(d(&[0x1d, 0x2a, 0x1c, 0x30, 0x62, 0x3e]), None);
    assert_eq!(d(&[0x1d, 0x2a, 0x1c, 0x32, 0x60, 0x3e]), None);
    assert_eq!(d(&[0x1d, 0x2a, 0x1c, 0x2e, 0x6c, 0x3e]), None);
    assert_eq!(d(&[0x1d, 0x2a, 0x1c, 0x2e, 0x78, 0x3e]), None);
    assert_eq!(d(&[0x1d, 0x2a, 0x1c, 0x2e, 0x61, 0x3e]), None);
    assert_eq!(d(&[0x1d, 0x2a, 0x1c, 0x2e, 0x60, 0x36, 0x05]), None);
    assert_eq!(d(&[0x1d, 0x2a, 0x1c, 0x2e, 0x60]), None);
    assert_eq!(
        decode_array_accumulate(&padded(&[0x1d, 0x2a, 0x1c, 0x2e, 0x60, 0x3e]), 5, 0),
        None
    );
}

#[test]
fn an_accumulate_hit_stores_the_unfused_result() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let shared = vm(true);
    // (element type, load, element bits as stored, element as loaded)
    let kinds: [(ArrayElementType, u8, i64, i32); 5] = [
        (ArrayElementType::Int, 0x2e, -7, -7),
        (ArrayElementType::Byte, 0x33, -2, -2),
        (ArrayElementType::Boolean, 0x33, 1, 1),
        (ArrayElementType::Char, 0x34, 0xfffe, 0xfffe),
        (ArrayElementType::Short, 0x35, -3, -3),
    ];
    for (et, load, stored, elem) in kinds {
        let Some(arr) = zgc.try_alloc_array(ClassId::new(0), et, 3) else {
            return;
        };
        let store_op = load + 0x21;
        let (cv, kind) = (CompactValue::int(stored as i32), 0);
        assert!(array_store_prim(&zgc, arr, 1, store_op, cv, kind), "{et:?}");
        for (arith, want) in [
            (0x60_u8, 100i32.wrapping_add(elem)),
            (0x64, 100i32.wrapping_sub(elem)),
            (0x68, 100i32.wrapping_mul(elem)),
            (0x7e, 100 & elem),
            (0x80, 100 | elem),
            (0x82, 100 ^ elem),
        ] {
            // Into the accumulator (`s += a[i]`) and into another local
            // (`t = s + a[i]`, local 1).
            for (store, z) in [(0x3e_u8, 3usize), (0x3c, 1)] {
                let mut f = accumulate_frame(
                    load,
                    arith,
                    store,
                    Value::Object(Some(arr)),
                    Value::Int(1),
                    100,
                );
                let code_len = f.code.len() - 2;
                assert_eq!(
                    fused_array_accumulate(&shared, &zgc, &mut f, code_len, 0),
                    Some(6),
                    "{et:?} {arith:#x}"
                );
                assert_eq!(f.stack.len(), 0, "nothing pushed");
                assert_eq!(
                    f.get_local_compact_unchecked(z).as_int(),
                    Some(want),
                    "{et:?} {arith:#x}"
                );
            }
        }
    }
}

#[test]
fn every_accumulate_decline_writes_nothing() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let shared = vm(true);
    let Some(ints) = zgc.try_alloc_array(ClassId::new(0), ArrayElementType::Int, 2) else {
        return;
    };
    let cases: [(u8, Value, Value); 6] = [
        // Out of range either side (the unfused `iaload` throws).
        (0x2e, Value::Object(Some(ints)), Value::Int(2)),
        (0x2e, Value::Object(Some(ints)), Value::Int(-1)),
        // A null array (the unfused `iaload` throws the NPE).
        (0x2e, Value::Object(None), Value::Int(0)),
        // An element type the opcode does not name.
        (0x33, Value::Object(Some(ints)), Value::Int(0)),
        (0x34, Value::Object(Some(ints)), Value::Int(0)),
        // An index local that is not an int.
        (0x2e, Value::Object(Some(ints)), Value::Float(0.0)),
    ];
    for (load, arr, index) in cases {
        let mut f = accumulate_frame(load, 0x60, 0x3e, arr, index, 5);
        let code_len = f.code.len() - 2;
        assert_eq!(
            fused_array_accumulate(&shared, &zgc, &mut f, code_len, 0),
            None,
            "{load:#x}"
        );
        assert_eq!(f.stack.len(), 0);
        assert_eq!(
            f.get_local_compact_unchecked(3).as_int(),
            Some(5),
            "{load:#x}"
        );
    }
    // An accumulator local that is not an int.
    let mut f = frame_with(
        vec![0x1d, 0x2a, 0x1c, 0x2e, 0x60, 0x3e, 0xb1],
        &[
            Value::Object(Some(ints)),
            Value::Int(0),
            Value::Int(0),
            Value::Float(1.0),
        ],
    );
    let code_len = f.code.len() - 2;
    assert_eq!(
        fused_array_accumulate(&shared, &zgc, &mut f, code_len, 0),
        None
    );
    assert_eq!(f.stack.len(), 0);
}
