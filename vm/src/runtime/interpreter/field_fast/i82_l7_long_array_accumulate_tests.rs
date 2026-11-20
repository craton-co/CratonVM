//! Interpreter round i3 wave 82, lane L7: the long and widening forms of
//! the fused array element accumulate
//! (`CRATONVM_INTERP_FUSED_ARRAY_ACCUMULATE_LONG`,
//! `fused_array_accumulate_long`). A hit stores what the unfused `lload_S;
//! aload_N; iload_M; <x>aload; [i2l;] <op>; lstore_Z` stores, kind mark
//! included, and a decline writes nothing. The helper does not read the
//! switch (the loop does), so these tests need none.
use super::*;
use std::sync::Arc;

/// A static frame over `code` with locals 0 = `arr`, 1 = `index`, and the
/// long accumulator `acc` in 2..3 (`max_locals` 4).
fn long_frame(code: Vec<u8>, arr: Value, index: Value, acc: Option<i64>) -> Frame {
    let mut f = Frame::new(
        ClassId::new(0x00ab_0882),
        "cratonvm/test/i82l7/LongAccumulate".to_string(),
        "m".to_string(),
        "([JI)V".to_string(),
        None,
        code,
        vec![],
        6,
        4,
        &[arr, index, Value::Int(0), Value::Int(0)],
    );
    if let Some(acc) = acc {
        f.set_local_unchecked(2, Value::Long(acc));
    }
    f
}

/// `code` followed by the two padding bytes a frame's code carries.
fn padded(code: &[u8]) -> Vec<u8> {
    let mut v = code.to_vec();
    v.extend_from_slice(&[0, 0]);
    v
}

fn vm() -> Arc<SharedVm> {
    let mut config = crate::config::VmConfig::default();
    config.site_census = false;
    Arc::new(SharedVm::new(config))
}

#[test]
fn the_long_decoder_takes_only_its_groups() {
    #[allow(clippy::type_complexity)]
    fn d(code: &[u8]) -> Option<(usize, usize, usize, u8, u8, usize, usize)> {
        decode_array_accumulate_long(&padded(code), code.len(), 0)
    }
    // `lload_2; aload_0; iload_1; laload; ladd; lstore_2`.
    assert_eq!(
        d(&[0x20, 0x2a, 0x1b, 0x2f, 0x61, 0x41]),
        Some((2, 0, 1, 0x2f, 0x61, 2, 6))
    );
    // `lload_0; aload_2; iload_3; iaload; i2l; lxor; lstore_1` (a
    // different target), and `caload` widened.
    assert_eq!(
        d(&[0x1e, 0x2c, 0x1d, 0x2e, 0x85, 0x83, 0x40]),
        Some((0, 2, 3, 0x2e, 0x83, 1, 7))
    );
    assert_eq!(
        d(&[0x1e, 0x2c, 0x1d, 0x34, 0x85, 0x61, 0x3f]),
        Some((0, 2, 3, 0x34, 0x61, 0, 7))
    );
    // An int arithmetic, `laload` with an `i2l`, an int load without
    // one, `faload` / `daload` / `aaload`, a throwing `ldiv` / `lrem`, a
    // shift, `istore_N` / `lstore n` as the store, an `iload_S` head.
    assert_eq!(d(&[0x20, 0x2a, 0x1b, 0x2f, 0x60, 0x41]), None);
    assert_eq!(d(&[0x20, 0x2a, 0x1b, 0x2f, 0x85, 0x61, 0x41]), None);
    assert_eq!(d(&[0x20, 0x2a, 0x1b, 0x2e, 0x61, 0x41]), None);
    assert_eq!(d(&[0x20, 0x2a, 0x1b, 0x30, 0x61, 0x41]), None);
    assert_eq!(d(&[0x20, 0x2a, 0x1b, 0x31, 0x61, 0x41]), None);
    assert_eq!(d(&[0x20, 0x2a, 0x1b, 0x32, 0x61, 0x41]), None);
    assert_eq!(d(&[0x20, 0x2a, 0x1b, 0x2f, 0x6d, 0x41]), None);
    assert_eq!(d(&[0x20, 0x2a, 0x1b, 0x2f, 0x71, 0x41]), None);
    assert_eq!(d(&[0x20, 0x2a, 0x1b, 0x2f, 0x79, 0x41]), None);
    assert_eq!(d(&[0x20, 0x2a, 0x1b, 0x2f, 0x61, 0x3e]), None);
    assert_eq!(d(&[0x20, 0x2a, 0x1b, 0x2f, 0x61, 0x37, 0x02]), None);
    assert_eq!(d(&[0x1c, 0x2a, 0x1b, 0x2f, 0x61, 0x41]), None);
    // Cut groups: the long form one byte short, the widening form's
    // store past `code_len`.
    assert_eq!(d(&[0x20, 0x2a, 0x1b, 0x2f, 0x61]), None);
    let widen = padded(&[0x1e, 0x2c, 0x1d, 0x2e, 0x85, 0x61, 0x3f]);
    assert_eq!(decode_array_accumulate_long(&widen, 6, 0), None);
}

#[test]
fn a_long_accumulate_hit_stores_the_unfused_result() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let shared = vm();
    // (element type, load, element as stored, element as `i2l` /
    // `laload` leaves it)
    let big: i64 = 0x1234_5678_9abc_def0;
    let kinds: [(ArrayElementType, u8, i64, i64); 6] = [
        (ArrayElementType::Long, 0x2f, big, big),
        (ArrayElementType::Int, 0x2e, -7, -7),
        (ArrayElementType::Byte, 0x33, -2, -2),
        (ArrayElementType::Boolean, 0x33, 1, 1),
        (ArrayElementType::Char, 0x34, 0xfffe, 0xfffe),
        (ArrayElementType::Short, 0x35, -3, -3),
    ];
    let acc: i64 = 1 << 40;
    for (et, load, stored, elem) in kinds {
        let Some(arr) = zgc.try_alloc_array(ClassId::new(0), et, 3) else {
            return;
        };
        let (cv, kind) = if load == 0x2f {
            (CompactValue::long(stored), ValueStack::KIND_MARK_LONG)
        } else {
            (CompactValue::int(stored as i32), 0)
        };
        assert!(
            array_store_prim(&zgc, arr, 1, load + 0x21, cv, kind),
            "{et:?}"
        );
        for (arith, want) in [
            (0x61_u8, acc.wrapping_add(elem)),
            (0x65, acc.wrapping_sub(elem)),
            (0x69, acc.wrapping_mul(elem)),
            (0x7f, acc & elem),
            (0x81, acc | elem),
            (0x83, acc ^ elem),
        ] {
            let mut code = vec![0x20, 0x2a, 0x1b, load];
            if load != 0x2f {
                code.push(0x85);
            }
            code.extend_from_slice(&[arith, 0x41, 0xb1]);
            let group = code.len() - 1;
            let mut f = long_frame(code, Value::Object(Some(arr)), Value::Int(1), Some(acc));
            let code_len = f.code.len() - 2;
            assert_eq!(
                fused_array_accumulate_long(&shared, &zgc, &mut f, code_len, 0),
                Some(group),
                "{et:?} {arith:#x}"
            );
            assert_eq!(f.stack.len(), 0, "nothing pushed");
            assert!(f.local_is_long_marked(2), "{et:?} {arith:#x}");
            assert_eq!(
                f.get_local_compact_unchecked(2).as_long_unchecked(),
                want,
                "{et:?} {arith:#x}"
            );
        }
    }
}

#[test]
fn every_long_accumulate_decline_writes_nothing() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let shared = vm();
    let Some(longs) = zgc.try_alloc_array(ClassId::new(0), ArrayElementType::Long, 2) else {
        return;
    };
    let code = || vec![0x20, 0x2a, 0x1b, 0x2f, 0x61, 0x41, 0xb1];
    let cases: [(Value, Value); 4] = [
        // Out of range either side (the unfused `laload` throws).
        (Value::Object(Some(longs)), Value::Int(2)),
        (Value::Object(Some(longs)), Value::Int(-1)),
        // A null array (the unfused `laload` throws the NPE).
        (Value::Object(None), Value::Int(0)),
        // An index local that is not an int.
        (Value::Object(Some(longs)), Value::Float(0.0)),
    ];
    for (arr, index) in cases {
        let mut f = long_frame(code(), arr, index, Some(5));
        let code_len = f.code.len() - 2;
        assert_eq!(
            fused_array_accumulate_long(&shared, &zgc, &mut f, code_len, 0),
            None
        );
        assert_eq!(f.stack.len(), 0);
        assert_eq!(f.get_local_compact_unchecked(2).as_long_unchecked(), 5);
    }
    // An accumulator slot without the `long` kind mark.
    let mut f = long_frame(code(), Value::Object(Some(longs)), Value::Int(0), None);
    let code_len = f.code.len() - 2;
    assert_eq!(
        fused_array_accumulate_long(&shared, &zgc, &mut f, code_len, 0),
        None
    );
    assert_eq!(f.get_local_compact_unchecked(2).as_int(), Some(0));
    // An `int[]` under `laload` (another element type).
    let Some(ints) = zgc.try_alloc_array(ClassId::new(0), ArrayElementType::Int, 2) else {
        return;
    };
    let mut f = long_frame(code(), Value::Object(Some(ints)), Value::Int(0), Some(5));
    let code_len = f.code.len() - 2;
    assert_eq!(
        fused_array_accumulate_long(&shared, &zgc, &mut f, code_len, 0),
        None
    );
    assert_eq!(f.get_local_compact_unchecked(2).as_long_unchecked(), 5);
}
