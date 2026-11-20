//! Interpreter round i3 wave 74, lane L7: the fused array element
//! increment's widened arithmetic
//! (`CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_SHIFT_DIV`,
//! [`ARRAY_INC_SHIFT_DIV`]) and its `dup2`-less local entry
//! (`CRATONVM_INTERP_FUSED_ARRAY_UPDATE_NO_DUP`, [`ARRAY_INC_NO_DUP`]).
//! A shift / division hit stores what the unfused `ishl` / `idiv` /
//! ... stores; a zero divisor declines having written nothing (the
//! unfused `idiv` throws); without the bit the decoder answers exactly as
//! before; the no-dup group is decoded as its `dup2` twin. Each test sets
//! the switches it needs explicitly.
use super::*;
use crate::runtime::interpreter::IncrementOperand;
use std::sync::Arc;

type Decoded = (u8, IncrementOperand, u8, usize, FieldIncrementUse);

/// A static frame over `code`: local 0 = `arr`, local 1 = `index`, local
/// 2 = `3`; `max_locals` 4.
fn frame(code: Vec<u8>, arr: Value, index: i32) -> Frame {
    Frame::new(
        ClassId::new(0x00ab_0741),
        "cratonvm/test/i74l7/ArrayShiftDiv".to_string(),
        "m".to_string(),
        "([III)V".to_string(),
        None,
        code,
        vec![],
        6,
        4,
        &[arr, Value::Int(index), Value::Int(3)],
    )
}

/// `aload_0; iload_1; dup2; <load>; <operand>; <arith>; [conv];
/// <load's store>; return`.
fn group(load: u8, operand: &[u8], arith: u8, conv: Option<u8>) -> Vec<u8> {
    let mut code = vec![0x2a, 0x1b, 0x5c, load];
    code.extend_from_slice(operand);
    code.push(arith);
    code.extend(conv);
    code.push(load + 0x21);
    code.push(0xb1);
    code
}

fn decode(code: &[u8], p: usize, mode: u8) -> Option<Decoded> {
    let f = frame(code.to_vec(), Value::Object(None), 0);
    let code_len = f.code.len() - 2;
    decode_array_increment_mode(&f, &f.code, code_len, p, mode)
}

fn array(zgc: &crate::memory::vm_heap::VmHeap, et: ArrayElementType, len: usize) -> Option<ObjectRef> {
    zgc.try_alloc_array(ClassId::new(0), et, len)
}

fn get(zgc: &crate::memory::vm_heap::VmHeap, arr: ObjectRef, index: i32, load: u8) -> Option<i64> {
    let mut s = ValueStack::new(4);
    if !array_load_prim(zgc, &mut s, arr, index, load) {
        return None;
    }
    let cv = s.pop_compact();
    if load == 0x2f {
        Some(cv.as_long_unchecked())
    } else {
        cv.as_int().map(i64::from)
    }
}

fn set(zgc: &crate::memory::vm_heap::VmHeap, arr: ObjectRef, index: i32, store: u8, v: i64) {
    let (cv, kind) = if store == 0x50 {
        (CompactValue::long(v), ValueStack::KIND_MARK_LONG)
    } else {
        // Cast: test values are in the element's range.
        (CompactValue::int(v as i32), 0)
    };
    assert!(array_store_prim(zgc, arr, index, store, cv, kind));
}

fn body(zgc: &crate::memory::vm_heap::VmHeap, f: &Frame, arr: ObjectRef, index: i32, mode: u8) -> Option<usize> {
    let code_len = f.code.len() - 2;
    let ptr = arr.as_ptr() as u64;
    fused_array_increment_body(
        zgc,
        f,
        code_len,
        2,
        ptr,
        index,
        mode,
        false,
        true,
        NO_DECLINE_MEMO,
        body_test_vm(),
        &mut no_sites(),
    )
    .map(|r| r.0)
}

#[test]
fn the_decoder_takes_the_shifts_and_divisions_only_under_the_bit() {
    for arith in [0x6c_u8, 0x70, 0x78, 0x7a, 0x7c] {
        let code = group(0x2e, &[0x04], arith, None);
        let end = code.len() - 1;
        assert_eq!(decode(&code, 2, 0), None, "{arith:#x}");
        assert_eq!(decode(&code, 2, ARRAY_INC_VALUE), None, "{arith:#x}");
        assert_eq!(
            decode(&code, 2, ARRAY_INC_SHIFT_DIV),
            Some((
                0x2e,
                IncrementOperand::Int(1),
                arith,
                end,
                FieldIncrementUse::Statement
            )),
            "{arith:#x}"
        );
    }
    // A narrow element keeps its own conversion.
    let code = group(0x33, &[0x04], 0x78, Some(0x91));
    assert!(decode(&code, 2, ARRAY_INC_SHIFT_DIV).is_some());
    assert_eq!(
        decode(&group(0x33, &[0x04], 0x78, None), 2, ARRAY_INC_SHIFT_DIV),
        None
    );
    // Long divisions take a long operand; long shifts an int count.
    for arith in [0x6d_u8, 0x71] {
        let code = group(0x2f, &[0x0a], arith, None);
        let end = code.len() - 1;
        assert_eq!(decode(&code, 2, 0), None);
        assert_eq!(
            decode(&code, 2, ARRAY_INC_SHIFT_DIV),
            Some((
                0x2f,
                IncrementOperand::Long(1),
                arith,
                end,
                FieldIncrementUse::Statement
            ))
        );
        assert_eq!(
            decode(&group(0x2f, &[0x04], arith, None), 2, ARRAY_INC_SHIFT_DIV),
            None
        );
    }
    for arith in [0x79_u8, 0x7b, 0x7d] {
        let code = group(0x2f, &[0x10, 0x41], arith, None);
        let end = code.len() - 1;
        assert_eq!(decode(&code, 2, 0), None);
        assert_eq!(
            decode(&code, 2, ARRAY_INC_SHIFT_DIV),
            Some((
                0x2f,
                IncrementOperand::Long(65),
                arith,
                end,
                FieldIncrementUse::Statement
            ))
        );
        // A long operand under a long shift is not a verified shape.
        assert_eq!(
            decode(&group(0x2f, &[0x0a], arith, None), 2, ARRAY_INC_SHIFT_DIV),
            None
        );
    }
    // An int count does not open the other long arithmetic.
    assert_eq!(
        decode(&group(0x2f, &[0x04], 0x61, None), 2, ARRAY_INC_SHIFT_DIV),
        None
    );
    // A long shift under an int element, an int shift under a long one.
    assert_eq!(
        decode(&group(0x2e, &[0x04], 0x79, None), 2, ARRAY_INC_SHIFT_DIV),
        None
    );
    assert_eq!(
        decode(&group(0x2f, &[0x04], 0x78, None), 2, ARRAY_INC_SHIFT_DIV),
        None
    );
    // Float arithmetic stays refused.
    assert_eq!(
        decode(&group(0x2e, &[0x04], 0x6e, None), 2, ARRAY_INC_SHIFT_DIV),
        None
    );
}

#[test]
fn the_widened_decoder_answers_every_old_hit_alike() {
    let loads: [u8; 6] = [0x2e, 0x2f, 0x33, 0x34, 0x35, 0x30];
    let operands: [&[u8]; 7] = [
        &[0x02],
        &[0x04],
        &[0x10, 0xfe],
        &[0x1c],
        &[0x0a],
        &[0x09],
        &[0x15, 0x02],
    ];
    let convs: [Option<u8>; 4] = [None, Some(0x91), Some(0x92), Some(0x93)];
    let mut hits = 0usize;
    for load in loads {
        for operand in operands {
            for arith in 0x60_u8..=0x83 {
                for conv in convs {
                    let code = group(load, operand, arith, conv);
                    for value in [0, ARRAY_INC_VALUE] {
                        if let Some(old) = decode(&code, 2, value) {
                            hits += 1;
                            assert_eq!(
                                decode(&code, 2, value | ARRAY_INC_SHIFT_DIV),
                                Some(old),
                                "{code:x?}"
                            );
                        }
                    }
                }
            }
        }
    }
    assert!(hits > 0);
}

#[test]
fn the_no_dup_group_decodes_as_its_dup2_twin() {
    let tails: [Vec<u8>; 5] = [
        vec![0x2e, 0x04, 0x60, 0x4f],
        vec![0x2e, 0x1c, 0x68, 0x4f],
        vec![0x33, 0x04, 0x64, 0x91, 0x54],
        vec![0x2e, 0x04, 0x60, 0x5b, 0x4f],
        vec![0x2e, 0x04, 0x78, 0x4f],
    ];
    let modes = [
        0,
        ARRAY_INC_VALUE,
        ARRAY_INC_SHIFT_DIV,
        ARRAY_INC_VALUE | ARRAY_INC_SHIFT_DIV,
    ];
    for tail in tails {
        for mode in modes {
            // `aload_0; iload_1; dup2; <tail>; return` against
            // `aload_0; iload_1; aload_0; iload_1; <tail>; return`.
            let mut dup = vec![0x2a, 0x1b, 0x5c];
            dup.extend_from_slice(&tail);
            dup.push(0xb1);
            let mut no_dup = vec![0x2a, 0x1b, 0x2a, 0x1b];
            no_dup.extend_from_slice(&tail);
            no_dup.push(0xb1);
            let want = decode(&dup, 2, mode).map(|d| (d.0, d.1, d.2, d.3 + 1, d.4));
            assert_eq!(
                decode(&no_dup, 4, mode | ARRAY_INC_NO_DUP),
                want,
                "{tail:x?} {mode}"
            );
            // Without the bit the `*aload` is not a `dup2`.
            assert_eq!(decode(&no_dup, 4, mode), None);
        }
    }
}

#[test]
fn a_shift_or_division_hit_stores_what_the_unfused_group_stores() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let (imin, imax) = (i64::from(i32::MIN), i64::from(i32::MAX));
    // (element type, load, conversion, operand, arith, before, after)
    #[allow(clippy::type_complexity)]
    let cases: [(ArrayElementType, u8, Option<u8>, &[u8], u8, i64, i64); 19] = [
        (ArrayElementType::Int, 0x2e, None, &[0x04], 0x78, 3, 6),
        // The count's low five bits: 33 -> 1, -1 -> 31.
        (ArrayElementType::Int, 0x2e, None, &[0x10, 0x21], 0x78, 3, 6),
        (
            ArrayElementType::Int,
            0x2e,
            None,
            &[0x10, 0xff],
            0x78,
            1,
            imin,
        ),
        (ArrayElementType::Int, 0x2e, None, &[0x04], 0x7a, -7, -4),
        (ArrayElementType::Int, 0x2e, None, &[0x04], 0x7c, -2, imax),
        (ArrayElementType::Int, 0x2e, None, &[0x06], 0x6c, -7, -2),
        (ArrayElementType::Int, 0x2e, None, &[0x06], 0x70, -7, -1),
        (ArrayElementType::Int, 0x2e, None, &[0x1c], 0x6c, 10, 3),
        (ArrayElementType::Int, 0x2e, None, &[0x02], 0x6c, imin, imin),
        (ArrayElementType::Int, 0x2e, None, &[0x02], 0x70, imin, 0),
        (
            ArrayElementType::Byte,
            0x33,
            Some(0x91),
            &[0x04],
            0x78,
            64,
            -128,
        ),
        (
            ArrayElementType::Char,
            0x34,
            Some(0x92),
            &[0x04],
            0x7a,
            0xffff,
            0x7fff,
        ),
        (
            ArrayElementType::Short,
            0x35,
            Some(0x93),
            &[0x05],
            0x6c,
            -32768,
            -16384,
        ),
        (ArrayElementType::Long, 0x2f, None, &[0x04], 0x79, 1, 2),
        // The count's low six bits: 65 -> 1.
        (
            ArrayElementType::Long,
            0x2f,
            None,
            &[0x10, 0x41],
            0x79,
            1,
            2,
        ),
        (ArrayElementType::Long, 0x2f, None, &[0x04], 0x7b, -3, -2),
        (
            ArrayElementType::Long,
            0x2f,
            None,
            &[0x04],
            0x7d,
            -2,
            i64::MAX,
        ),
        (
            ArrayElementType::Long,
            0x2f,
            None,
            &[0x0a],
            0x6d,
            i64::MIN,
            i64::MIN,
        ),
        (ArrayElementType::Long, 0x2f, None, &[0x0a], 0x71, -9, 0),
    ];
    for (et, load, conv, operand, arith, before, after) in cases {
        let Some(arr) = array(&zgc, et, 3) else {
            return;
        };
        for i in 0..3 {
            set(&zgc, arr, i, load + 0x21, if i == 1 { before } else { 0 });
        }
        let code = group(load, operand, arith, conv);
        let end = code.len() - 1;
        let f = frame(code, Value::Object(Some(arr)), 1);
        // Without the bit: a decline, nothing written.
        assert_eq!(body(&zgc, &f, arr, 1, 0), None, "{load:#x} {arith:#x}");
        assert_eq!(
            get(&zgc, arr, 1, load),
            Some(before),
            "{load:#x} {arith:#x}"
        );
        assert_eq!(
            body(&zgc, &f, arr, 1, ARRAY_INC_SHIFT_DIV),
            Some(end),
            "{load:#x} {arith:#x}"
        );
        assert_eq!(
            get(&zgc, arr, 1, load),
            Some(after),
            "{load:#x} {arith:#x} {before}"
        );
        assert_eq!(get(&zgc, arr, 0, load), Some(0));
        assert_eq!(get(&zgc, arr, 2, load), Some(0));
    }
}

#[test]
fn a_zero_divisor_declines_having_written_nothing() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let (Some(ints), Some(longs)) = (
        array(&zgc, ArrayElementType::Int, 2),
        array(&zgc, ArrayElementType::Long, 2),
    ) else {
        return;
    };
    set(&zgc, ints, 0, 0x4f, 7);
    set(&zgc, longs, 0, 0x50, 7);
    for arith in [0x6c_u8, 0x70] {
        let f = frame(
            group(0x2e, &[0x03], arith, None),
            Value::Object(Some(ints)),
            0,
        );
        assert_eq!(body(&zgc, &f, ints, 0, ARRAY_INC_SHIFT_DIV), None);
        assert_eq!(get(&zgc, ints, 0, 0x2e), Some(7));
    }
    for arith in [0x6d_u8, 0x71] {
        let f = frame(
            group(0x2f, &[0x09], arith, None),
            Value::Object(Some(longs)),
            0,
        );
        assert_eq!(body(&zgc, &f, longs, 0, ARRAY_INC_SHIFT_DIV), None);
        assert_eq!(get(&zgc, longs, 0, 0x2f), Some(7));
    }
}

fn vm(no_dup: bool, shift_div: bool) -> Arc<SharedVm> {
    let mut config = crate::config::VmConfig::default();
    config.interp_fused_array_increment = true;
    config.interp_fused_array_increment_value = true;
    config.interp_fused_array_increment_top = false;
    config.interp_fused_array_increment_dup2 = false;
    config.interp_fused_array_accumulate = false;
    config.interp_fused_array_decline_memo = false;
    config.interp_fused_array_increment_array_operand = false;
    config.interp_fused_array_increment_static_operand = false;
    config.interp_fused_array_increment_shift_div = shift_div;
    config.interp_fused_array_update_no_dup = no_dup;
    config.site_census = false;
    Arc::new(SharedVm::new(config))
}

#[test]
fn the_local_entry_serves_the_no_dup_update_only_under_its_switch() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let Some(arr) = array(&zgc, ArrayElementType::Int, 2) else {
        return;
    };
    // `a[i] = a[i] + 1` over locals 0 / 1.
    let plus = vec![0x2a, 0x1b, 0x2a, 0x1b, 0x2e, 0x04, 0x60, 0x4f, 0xb1];
    for on in [false, true] {
        set(&zgc, arr, 1, 0x4f, 5);
        let shared = vm(on, false);
        let mut f = frame(plus.clone(), Value::Object(Some(arr)), 1);
        let code_len = f.code.len() - 2;
        let got =
            fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites());
        assert_eq!(f.stack.len(), 0);
        if on {
            assert_eq!(got, Some(8));
            assert_eq!(get(&zgc, arr, 1, 0x2e), Some(6));
        } else {
            assert_eq!(got, None);
            assert_eq!(get(&zgc, arr, 1, 0x2e), Some(5));
        }
    }
    let shared = vm(true, false);
    // Another index local, another array local: not the same element.
    for code in [
        vec![0x2a, 0x1b, 0x2a, 0x1c, 0x2e, 0x04, 0x60, 0x4f, 0xb1],
        vec![0x2a, 0x1b, 0x2b, 0x1b, 0x2e, 0x04, 0x60, 0x4f, 0xb1],
    ] {
        set(&zgc, arr, 1, 0x4f, 5);
        let mut f = frame(code, Value::Object(Some(arr)), 1);
        let code_len = f.code.len() - 2;
        assert_eq!(
            fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
            None
        );
        assert_eq!(get(&zgc, arr, 1, 0x2e), Some(5));
    }
    // `x = a[i] = a[i] + 1` (the value form): the new element is left.
    let pre = vec![0x2a, 0x1b, 0x2a, 0x1b, 0x2e, 0x04, 0x60, 0x5b, 0x4f, 0xb1];
    set(&zgc, arr, 1, 0x4f, 5);
    let mut f = frame(pre, Value::Object(Some(arr)), 1);
    let code_len = f.code.len() - 2;
    assert_eq!(
        fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
        Some(9)
    );
    assert_eq!(f.stack.len(), 1);
    assert_eq!(f.stack.pop_int_unchecked(), 6);
    // `a[i] = a[i] << 1`: needs the shift switch too.
    let shl = vec![0x2a, 0x1b, 0x2a, 0x1b, 0x2e, 0x04, 0x78, 0x4f, 0xb1];
    for shift_div in [false, true] {
        set(&zgc, arr, 1, 0x4f, 5);
        let shared = vm(true, shift_div);
        let mut f = frame(shl.clone(), Value::Object(Some(arr)), 1);
        let code_len = f.code.len() - 2;
        let got =
            fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites());
        assert_eq!(got, shift_div.then_some(8));
        assert_eq!(
            get(&zgc, arr, 1, 0x2e),
            Some(if shift_div { 10 } else { 5 })
        );
    }
    // Out of range and a null array: declines (the unfused `iaload`
    // throws), nothing written, nothing pushed.
    set(&zgc, arr, 1, 0x4f, 5);
    let mut f = frame(plus.clone(), Value::Object(Some(arr)), 2);
    let code_len = f.code.len() - 2;
    assert_eq!(
        fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
        None
    );
    assert_eq!(f.stack.len(), 0);
    let mut f = frame(plus, Value::Object(None), 1);
    let code_len = f.code.len() - 2;
    assert_eq!(
        fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
        None
    );
    assert_eq!(f.stack.len(), 0);
    assert_eq!(get(&zgc, arr, 1, 0x2e), Some(5));
}
