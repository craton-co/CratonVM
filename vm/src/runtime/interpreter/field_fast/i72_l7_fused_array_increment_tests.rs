//! Interpreter round i3 wave 72, lane L7: the fused array element
//! increment (`CRATONVM_INTERP_FUSED_ARRAY_INCREMENT`, `_VALUE`, `_TOP`;
//! `fused_array_increment_local` / `_top`). A hit stores what the unfused
//! `aload_N; iload_M; dup2; <x>aload; <operand>; <op>; [conv]; <x>astore`
//! stores (the wrapping result, narrowed by the element's own conversion)
//! and leaves the operand stack as the unfused group leaves it; every
//! decline writes nothing and pushes nothing. Each test sets the switches
//! it needs explicitly.
use super::*;
use crate::runtime::interpreter::IncrementOperand;
use std::sync::Arc;

type Decoded = (u8, IncrementOperand, u8, usize, FieldIncrementUse);

/// `aload_0; iload_1; dup2; <load>; [post]; <operand>; <arith>; [conv];
/// [pre]; <store>; return`.
#[allow(clippy::too_many_arguments)]
fn group_with_store(
    load: u8,
    post: Option<u8>,
    operand: &[u8],
    arith: u8,
    conv: Option<u8>,
    pre: Option<u8>,
    store: u8,
) -> Vec<u8> {
    let mut code = vec![0x2a, 0x1b, 0x5c, load];
    code.extend(post);
    code.extend_from_slice(operand);
    code.push(arith);
    code.extend(conv);
    code.extend(pre);
    code.push(store);
    code.push(0xb1);
    code
}

/// [`group_with_store`] with the load's own store (`load + 0x21`).
fn group(
    load: u8,
    post: Option<u8>,
    operand: &[u8],
    arith: u8,
    conv: Option<u8>,
    pre: Option<u8>,
) -> Vec<u8> {
    group_with_store(load, post, operand, arith, conv, pre, load + 0x21)
}

/// A static frame over `code`: local 0 = `arr`, local 1 = `index`, local
/// 2 = `3`; `max_locals` 4.
fn frame(code: Vec<u8>, arr: Value, index: i32) -> Frame {
    Frame::new(
        ClassId::new(0x00ab_0721),
        "cratonvm/test/i72l7/ArrayIncrement".to_string(),
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

fn decode(code: Vec<u8>, value: bool) -> Option<Decoded> {
    let f = frame(code, Value::Object(None), 0);
    let code_len = f.code.len() - 2;
    decode_array_increment(&f, &f.code, code_len, 2, value)
}

/// A fresh array of `et`, `len` elements, or `None` when the test heap is
/// exhausted.
fn array(zgc: &crate::memory::vm_heap::VmHeap, et: ArrayElementType, len: usize) -> Option<ObjectRef> {
    zgc.try_alloc_array(ClassId::new(0), et, len)
}

/// Element `index` of `arr` as the `*aload` `load` pushes it (an int, or
/// a long for `laload`), widened to `i64`.
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

/// Store `v` (an int, or a long for `lastore`) into element `index`.
fn set(zgc: &crate::memory::vm_heap::VmHeap, arr: ObjectRef, index: i32, store: u8, v: i64) {
    let (cv, kind) = if store == 0x50 {
        (CompactValue::long(v), ValueStack::KIND_MARK_LONG)
    } else {
        // Cast: test values are in the element's range.
        (CompactValue::int(v as i32), 0)
    };
    assert!(array_store_prim(zgc, arr, index, store, cv, kind));
}

fn body(
    zgc: &crate::memory::vm_heap::VmHeap,
    f: &Frame,
    arr: ObjectRef,
    index: i32,
    value: bool,
) -> Option<(usize, Option<IncrementOperand>)> {
    let code_len = f.code.len() - 2;
    let ptr = arr.as_ptr() as u64;
    fused_array_increment_body(
        zgc,
        f,
        code_len,
        2,
        ptr,
        index,
        array_increment_mode(value, false),
        false,
        true,
        NO_DECLINE_MEMO,
        body_test_vm(),
        &mut no_sites(),
    )
}

#[test]
fn the_decoder_takes_every_element_kind_with_its_own_conversion() {
    let iconst_1: &[u8] = &[0x04];
    for (load, conv) in [
        (0x2e_u8, None),
        (0x33, Some(0x91_u8)),
        (0x34, Some(0x92)),
        (0x35, Some(0x93)),
    ] {
        for arith in [0x60_u8, 0x64, 0x68, 0x7e, 0x80, 0x82] {
            let code = group(load, None, iconst_1, arith, conv, None);
            let end = code.len() - 1;
            assert_eq!(
                decode(code, false),
                Some((
                    load,
                    IncrementOperand::Int(1),
                    arith,
                    end,
                    FieldIncrementUse::Statement
                )),
                "{load:#x} {arith:#x}"
            );
        }
    }
    // `bipush -3`, `sipush 300`, `iload_2` (3), `iload 2`.
    let operands: [(&[u8], i32); 4] = [
        (&[0x10, 0xfd], -3),
        (&[0x11, 0x01, 0x2c], 300),
        (&[0x1c], 3),
        (&[0x15, 0x02], 3),
    ];
    for (operand, k) in operands {
        let code = group(0x2e, None, operand, 0x60, None, None);
        assert_eq!(
            decode(code, false).map(|d| d.1),
            Some(IncrementOperand::Int(k))
        );
    }
    for arith in [0x61_u8, 0x65, 0x69, 0x7f, 0x81, 0x83] {
        let code = group(0x2f, None, &[0x0a], arith, None, None);
        let end = code.len() - 1;
        assert_eq!(
            decode(code, false),
            Some((
                0x2f,
                IncrementOperand::Long(1),
                arith,
                end,
                FieldIncrementUse::Statement
            ))
        );
    }
}

#[test]
fn the_decoder_refuses_every_other_shape() {
    let refused = |code: Vec<u8>| assert_eq!(decode(code.clone(), true), None, "{code:x?}");
    // A narrow element without its conversion, or with another's.
    refused(group(0x33, None, &[0x04], 0x60, None, None));
    refused(group(0x33, None, &[0x04], 0x60, Some(0x92), None));
    refused(group(0x34, None, &[0x04], 0x60, Some(0x93), None));
    // An int element with a conversion (not a compound assignment).
    refused(group(0x2e, None, &[0x04], 0x60, Some(0x91), None));
    // `idiv` can throw; the shifts are not taken.
    refused(group(0x2e, None, &[0x04], 0x6c, None, None));
    refused(group(0x2e, None, &[0x04], 0x70, None, None));
    refused(group(0x2e, None, &[0x04], 0x78, None, None));
    // Float / double / reference elements.
    refused(group(0x30, None, &[0x0b], 0x62, None, None));
    refused(group(0x31, None, &[0x0e], 0x63, None, None));
    refused(group(0x32, None, &[0x01], 0x60, None, None));
    // Width mismatches between the element, the operand and the op.
    refused(group(0x2f, None, &[0x04], 0x61, None, None));
    refused(group(0x2f, None, &[0x0a], 0x60, None, None));
    refused(group(0x2e, None, &[0x0a], 0x60, None, None));
    refused(group(0x2e, None, &[0x04], 0x61, None, None));
    // Another array kind's store.
    refused(group_with_store(
        0x33,
        None,
        &[0x04],
        0x60,
        Some(0x91),
        None,
        0x4f,
    ));
    refused(group_with_store(
        0x2e,
        None,
        &[0x04],
        0x60,
        None,
        None,
        0x54,
    ));
    // A non-int operand local (local 0 is the array).
    refused(group(0x2e, None, &[0x1a], 0x60, None, None));
    // A duplicate of the wrong width, or two of them.
    refused(group(0x2e, Some(0x5e), &[0x04], 0x60, None, None));
    refused(group(0x2f, Some(0x5b), &[0x0a], 0x61, None, None));
    refused(group(0x2e, Some(0x5b), &[0x04], 0x60, None, Some(0x5b)));
    // Not a `dup2` at the group's pc.
    let mut code = group(0x2e, None, &[0x04], 0x60, None, None);
    code[2] = 0x59;
    refused(code);
}

#[test]
fn the_decoder_takes_the_value_forms_only_under_the_bit() {
    let post = group(0x2e, Some(0x5b), &[0x04], 0x60, None, None);
    let pre = group(0x33, None, &[0x04], 0x60, Some(0x91), Some(0x5b));
    let post_long = group(0x2f, Some(0x5e), &[0x0a], 0x61, None, None);
    let pre_long = group(0x2f, None, &[0x0a], 0x65, None, Some(0x5e));
    for code in [&post, &pre, &post_long, &pre_long] {
        assert_eq!(decode(code.clone(), false), None, "{code:x?}");
    }
    assert_eq!(
        decode(post.clone(), true).map(|d| d.4),
        Some(FieldIncrementUse::Old)
    );
    assert_eq!(
        decode(pre.clone(), true).map(|d| d.4),
        Some(FieldIncrementUse::New)
    );
    assert_eq!(
        decode(post_long.clone(), true).map(|d| d.4),
        Some(FieldIncrementUse::Old)
    );
    assert_eq!(
        decode(pre_long.clone(), true).map(|d| d.4),
        Some(FieldIncrementUse::New)
    );
    for code in [post, pre, post_long, pre_long] {
        let end = code.len() - 1;
        assert_eq!(decode(code, true).map(|d| d.3), Some(end));
    }
}

#[test]
fn a_hit_stores_what_the_unfused_group_stores() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let (imin, imax) = (i64::from(i32::MIN), i64::from(i32::MAX));
    // (element type, load, conversion, operand, arith, before, after)
    #[allow(clippy::type_complexity)]
    let cases: [(ArrayElementType, u8, Option<u8>, &[u8], u8, i64, i64); 14] = [
        (ArrayElementType::Int, 0x2e, None, &[0x04], 0x60, 41, 42),
        (ArrayElementType::Int, 0x2e, None, &[0x04], 0x60, imax, imin),
        (ArrayElementType::Int, 0x2e, None, &[0x04], 0x64, imin, imax),
        (ArrayElementType::Int, 0x2e, None, &[0x1c], 0x68, 7, 21),
        (
            ArrayElementType::Int,
            0x2e,
            None,
            &[0x10, 0x0c],
            0x7e,
            0x3a,
            0x08,
        ),
        (
            ArrayElementType::Int,
            0x2e,
            None,
            &[0x10, 0x0c],
            0x80,
            0x31,
            0x3d,
        ),
        (
            ArrayElementType::Int,
            0x2e,
            None,
            &[0x10, 0x0c],
            0x82,
            0x0f,
            0x03,
        ),
        (
            ArrayElementType::Byte,
            0x33,
            Some(0x91),
            &[0x04],
            0x60,
            127,
            -128,
        ),
        (
            ArrayElementType::Byte,
            0x33,
            Some(0x91),
            &[0x04],
            0x64,
            -128,
            127,
        ),
        (
            ArrayElementType::Char,
            0x34,
            Some(0x92),
            &[0x04],
            0x60,
            0xffff,
            0,
        ),
        (
            ArrayElementType::Char,
            0x34,
            Some(0x92),
            &[0x04],
            0x64,
            0,
            0xffff,
        ),
        (
            ArrayElementType::Short,
            0x35,
            Some(0x93),
            &[0x04],
            0x60,
            32767,
            -32768,
        ),
        (
            ArrayElementType::Long,
            0x2f,
            None,
            &[0x0a],
            0x61,
            i64::MAX,
            i64::MIN,
        ),
        (ArrayElementType::Long, 0x2f, None, &[0x0a], 0x65, 0, -1),
    ];
    for (et, load, conv, operand, arith, before, after) in cases {
        let Some(arr) = array(&zgc, et, 3) else {
            return;
        };
        set(&zgc, arr, 0, load + 0x21, 0);
        set(&zgc, arr, 1, load + 0x21, before);
        set(&zgc, arr, 2, load + 0x21, 0);
        let code = group(load, None, operand, arith, conv, None);
        let end = code.len() - 1;
        let f = frame(code, Value::Object(Some(arr)), 1);
        assert_eq!(
            body(&zgc, &f, arr, 1, false),
            Some((end, None)),
            "{load:#x} {arith:#x}"
        );
        assert_eq!(
            get(&zgc, arr, 1, load),
            Some(after),
            "{load:#x} {arith:#x} {before}"
        );
        // The neighbours are untouched.
        assert_eq!(get(&zgc, arr, 0, load), Some(0), "{load:#x}");
        assert_eq!(get(&zgc, arr, 2, load), Some(0), "{load:#x}");
    }
}

#[test]
fn every_decline_writes_nothing() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let (Some(ints), Some(bools)) = (
        array(&zgc, ArrayElementType::Int, 3),
        array(&zgc, ArrayElementType::Boolean, 3),
    ) else {
        return;
    };
    for i in 0..3 {
        set(&zgc, ints, i, 0x4f, 10 + i64::from(i));
    }
    let code = group(0x2e, None, &[0x04], 0x60, None, None);
    let f = frame(code, Value::Object(Some(ints)), 3);
    // Out of range either side (the unfused `iaload` raises the AIOOBE).
    assert_eq!(body(&zgc, &f, ints, 3, false), None);
    assert_eq!(body(&zgc, &f, ints, -1, false), None);
    assert_eq!(body(&zgc, &f, ints, i32::MIN, false), None);
    // A null array (the unfused `iaload` raises the NPE).
    let code_len = f.code.len() - 2;
    assert_eq!(
        fused_array_increment_body(
            &zgc,
            &f,
            code_len,
            2,
            0,
            0,
            0,
            false,
            true,
            NO_DECLINE_MEMO,
            body_test_vm(),
            &mut no_sites()
        ),
        None
    );
    // An element type the opcode does not name.
    let bytes = group(0x33, None, &[0x04], 0x60, Some(0x91), None);
    let fb = frame(bytes.clone(), Value::Object(Some(ints)), 0);
    assert_eq!(body(&zgc, &fb, ints, 0, false), None);
    let longs = group(0x2f, None, &[0x0a], 0x61, None, None);
    let fl = frame(longs, Value::Object(Some(ints)), 0);
    assert_eq!(body(&zgc, &fl, ints, 0, false), None);
    for i in 0..3 {
        assert_eq!(
            get(&zgc, ints, i, 0x2e),
            Some(10 + i64::from(i)),
            "element {i}"
        );
    }
    // A `boolean[]` under `baload`: declined, not reproduced.
    set(&zgc, bools, 0, 0x54, 1);
    let fz = frame(bytes, Value::Object(Some(bools)), 0);
    assert_eq!(body(&zgc, &fz, bools, 0, false), None);
    assert_eq!(get(&zgc, bools, 0, 0x33), Some(1));
}

#[test]
fn a_value_form_leaves_the_old_or_the_new_element() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let int = |n: i32| IncrementOperand::Int(n);
    // (element type, code, before, after, left)
    let cases: [(ArrayElementType, Vec<u8>, i64, i64, IncrementOperand); 5] = [
        (
            ArrayElementType::Int,
            group(0x2e, Some(0x5b), &[0x04], 0x60, None, None),
            41,
            42,
            int(41),
        ),
        (
            ArrayElementType::Int,
            group(0x2e, None, &[0x04], 0x60, None, Some(0x5b)),
            41,
            42,
            int(42),
        ),
        (
            ArrayElementType::Byte,
            group(0x33, Some(0x5b), &[0x04], 0x60, Some(0x91), None),
            127,
            -128,
            int(127),
        ),
        (
            ArrayElementType::Char,
            group(0x34, None, &[0x04], 0x64, Some(0x92), Some(0x5b)),
            0,
            0xffff,
            int(0xffff),
        ),
        (
            ArrayElementType::Long,
            group(0x2f, Some(0x5e), &[0x0a], 0x61, None, None),
            9,
            10,
            IncrementOperand::Long(9),
        ),
    ];
    for (et, code, before, after, left) in cases {
        let Some(arr) = array(&zgc, et, 2) else {
            return;
        };
        let load = code[3];
        set(&zgc, arr, 0, load + 0x21, before);
        let end = code.len() - 1;
        let f = frame(code.clone(), Value::Object(Some(arr)), 0);
        // Without the bit: a decline, nothing written.
        assert_eq!(body(&zgc, &f, arr, 0, false), None, "{code:x?}");
        assert_eq!(get(&zgc, arr, 0, load), Some(before), "{code:x?}");
        assert_eq!(
            body(&zgc, &f, arr, 0, true),
            Some((end, Some(left))),
            "{code:x?}"
        );
        assert_eq!(get(&zgc, arr, 0, load), Some(after), "{code:x?}");
    }
}

fn vm(on: bool, value: bool, top: bool) -> Arc<SharedVm> {
    let mut config = crate::config::VmConfig::default();
    config.interp_fused_array_increment = on;
    config.interp_fused_array_increment_value = value;
    config.interp_fused_array_increment_top = top;
    // Pinned too (wave 73), though no helper under test reads them: a
    // config built from the environment must not leak a default.
    config.interp_fused_array_increment_dup2 = false;
    config.interp_fused_array_accumulate = false;
    // Wave 74: the widened arithmetic and the no-dup entry, pinned off.
    config.interp_fused_array_increment_shift_div = false;
    config.interp_fused_array_update_no_dup = false;
    // Wave 75: the decline memo, pinned off.
    config.interp_fused_array_decline_memo = false;
    config.interp_fused_array_increment_array_operand = false;
    config.interp_fused_array_increment_static_operand = false;
    config.site_census = false;
    Arc::new(SharedVm::new(config))
}

#[test]
fn the_local_entry_reads_both_locals_and_pushes_only_a_value_form() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let Some(arr) = array(&zgc, ArrayElementType::Int, 2) else {
        return;
    };
    let statement = group(0x2e, None, &[0x04], 0x60, None, None);
    let post = group(0x2e, Some(0x5b), &[0x04], 0x60, None, None);
    for value in [false, true] {
        let shared = vm(true, value, false);
        set(&zgc, arr, 1, 0x4f, 5);
        let mut f = frame(statement.clone(), Value::Object(Some(arr)), 1);
        let code_len = f.code.len() - 2;
        assert_eq!(
            fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
            Some(statement.len() - 1)
        );
        assert_eq!(f.stack.len(), 0, "the statement form pushes nothing");
        assert_eq!(get(&zgc, arr, 1, 0x2e), Some(6));
        let mut f = frame(post.clone(), Value::Object(Some(arr)), 1);
        let code_len = f.code.len() - 2;
        let got =
            fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites());
        if value {
            assert_eq!(got, Some(post.len() - 1));
            assert_eq!(f.stack.len(), 1);
            assert_eq!(f.stack.pop_int_unchecked(), 6, "the old element");
            assert_eq!(get(&zgc, arr, 1, 0x2e), Some(7));
        } else {
            assert_eq!(got, None);
            assert_eq!(f.stack.len(), 0);
            assert_eq!(get(&zgc, arr, 1, 0x2e), Some(6));
        }
    }
    // A null array local: a decline (the unfused `iaload` throws).
    let shared = vm(true, true, false);
    let mut f = frame(statement, Value::Object(None), 1);
    let code_len = f.code.len() - 2;
    assert_eq!(
        fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
        None
    );
    assert_eq!(f.stack.len(), 0);
}

#[test]
fn the_top_entry_replaces_the_array_slot_and_needs_the_base_switch() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let Some(arr) = array(&zgc, ArrayElementType::Int, 2) else {
        return;
    };
    // `iload_1; dup2; iaload; iconst_1; iadd; [dup_x2]; iastore; return`,
    // the array pushed before it.
    let statement = group(0x2e, None, &[0x04], 0x60, None, None)[1..].to_vec();
    let pre = group(0x2e, None, &[0x04], 0x60, None, Some(0x5b))[1..].to_vec();
    for (code, value, want_left) in [(statement, false, None), (pre, true, Some(4))] {
        for on in [false, true] {
            set(&zgc, arr, 0, 0x4f, 3);
            let shared = vm(on, value, true);
            let mut f = frame(code.clone(), Value::Object(Some(arr)), 0);
            f.stack.push_int(9).unwrap();
            f.stack.push(Value::Object(Some(arr))).unwrap();
            let code_len = f.code.len() - 2;
            let got =
                fused_array_increment_top(&shared, &zgc, &mut f, code_len, 0, &mut no_sites());
            if !on {
                assert_eq!(got, None, "admitted only with the base switch");
                assert_eq!(f.stack.len(), 2, "the stack as it was");
                assert_eq!(get(&zgc, arr, 0, 0x2e), Some(3));
                continue;
            }
            assert_eq!(got, Some(code.len() - 1));
            assert_eq!(get(&zgc, arr, 0, 0x2e), Some(4));
            match want_left {
                None => assert_eq!(f.stack.len(), 1, "the array slot popped"),
                Some(v) => {
                    assert_eq!(f.stack.len(), 2, "the array slot replaced");
                    assert_eq!(f.stack.pop_int_unchecked(), v);
                }
            }
            assert_eq!(f.stack.pop_int_unchecked(), 9, "the slot below untouched");
        }
    }
    // Out of range: the stack as it was, nothing written.
    let shared = vm(true, false, true);
    let code = group(0x2e, None, &[0x04], 0x60, None, None)[1..].to_vec();
    let mut f = frame(code, Value::Object(Some(arr)), 2);
    f.stack.push(Value::Object(Some(arr))).unwrap();
    let code_len = f.code.len() - 2;
    assert_eq!(
        fused_array_increment_top(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
        None
    );
    assert_eq!(f.stack.len(), 1);
}

#[test]
fn the_dup2_entry_pops_both_slots_and_declines_a_category_two_top() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let Some(arr) = array(&zgc, ArrayElementType::Int, 4) else {
        return;
    };
    // `dup2; iaload; iconst_1; iadd; [dup_x2]; iastore; return`, the
    // array and the index pushed before it.
    let statement = group(0x2e, None, &[0x04], 0x60, None, None)[2..].to_vec();
    let post = group(0x2e, Some(0x5b), &[0x04], 0x60, None, None)[2..].to_vec();
    for (code, value, want_left) in [(statement.clone(), false, None), (post, true, Some(7))] {
        for on in [false, true] {
            set(&zgc, arr, 3, 0x4f, 7);
            let shared = vm(on, value, false);
            let mut f = frame(code.clone(), Value::Object(Some(arr)), 0);
            f.stack.push_int(9).unwrap();
            f.stack.push(Value::Object(Some(arr))).unwrap();
            f.stack.push_int(3).unwrap();
            let code_len = f.code.len() - 2;
            let got =
                fused_array_increment_dup2(&shared, &zgc, &mut f, code_len, 0, &mut no_sites());
            if !on {
                assert_eq!(got, None, "admitted only with the base switch");
                assert_eq!(f.stack.len(), 3, "the stack as it was");
                assert_eq!(get(&zgc, arr, 3, 0x2e), Some(7));
                continue;
            }
            assert_eq!(got, Some(code.len() - 1));
            assert_eq!(get(&zgc, arr, 3, 0x2e), Some(8));
            match want_left {
                None => assert_eq!(f.stack.len(), 1, "both slots popped"),
                Some(v) => {
                    assert_eq!(f.stack.len(), 2, "one value left");
                    assert_eq!(f.stack.pop_int_unchecked(), v);
                }
            }
            assert_eq!(f.stack.pop_int_unchecked(), 9, "the slot below untouched");
        }
    }
    // A long on top (a `dup2` of a category-2 value): declined. The loop
    // above left the element at 8 (its last hit); reset it so the
    // decline's "nothing written" is what the last assertion checks
    // (wave 73: the reset was missing, so the assertion read the 8 the
    // loop's last hit stored: `left: Some(8), right: Some(7)` on every
    // run that got this far).
    set(&zgc, arr, 3, 0x4f, 7);
    let shared = vm(true, false, false);
    let mut f = frame(statement, Value::Object(Some(arr)), 0);
    f.stack.push(Value::Object(Some(arr))).unwrap();
    f.stack.push_long(3).unwrap();
    let code_len = f.code.len() - 2;
    assert_eq!(
        fused_array_increment_dup2(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
        None
    );
    assert_eq!(f.stack.len(), 2);
    assert_eq!(get(&zgc, arr, 3, 0x2e), Some(7));
}
