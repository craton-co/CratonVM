//! Interpreter round i3 wave 76, lane L7: the decline memo tested inside
//! the three array increment entries (`memo_says_declined`), the wide-local
//! element read's one entry (`array_load_wide_locals_at`), and the two
//! deferred operands of the fused array increment: another array element
//! (`CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_ARRAY_OPERAND`) and a quickened
//! static (`CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_STATIC_OPERAND`). Each
//! test sets every switch it depends on explicitly.
use super::*;
use crate::classloading::resolution::CachedBytecodeMethod;
use std::sync::Arc;

const CLASS: u32 = 0x00ab_0761;

fn vm(memo: bool, array_operand: bool, static_operand: bool) -> Arc<SharedVm> {
    let mut config = crate::config::VmConfig::default();
    config.interp_fused_array_increment = true;
    config.interp_fused_array_increment_value = true;
    config.interp_fused_array_increment_top = true;
    config.interp_fused_array_increment_dup2 = true;
    config.interp_fused_array_accumulate = false;
    config.interp_fused_array_increment_shift_div = true;
    config.interp_fused_array_update_no_dup = true;
    config.interp_fused_array_decline_memo = memo;
    config.interp_fused_array_increment_array_operand = array_operand;
    config.interp_fused_array_increment_static_operand = static_operand;
    config.interp_fused_wide_locals = true;
    config.site_census = false;
    Arc::new(SharedVm::new(config))
}

/// An owned static frame over `code` (class `CLASS`), `max_locals` 4,
/// `args` in its first locals.
fn frame(code: &[u8], args: &[Value]) -> Frame {
    Frame::new(
        ClassId::new(CLASS),
        "cratonvm/test/i76l7/ArrayOperand".to_string(),
        "m".to_string(),
        "([II[II)V".to_string(),
        None,
        code.to_vec(),
        vec![],
        6,
        4,
        args,
    )
}

/// The same frame pushed from a fresh `CachedBytecodeMethod` (so it keeps
/// a decline memo).
fn cached_frame(code: &[u8], args: &[Value]) -> Frame {
    let cm = Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: ClassId::new(CLASS),
            class_name: Arc::from("cratonvm/test/i76l7/ArrayOperandCached"),
            method_name: Arc::from("m"),
            method_descriptor: Arc::from("([II[II)V"),
            source_file: None,
            code: crate::runtime::frame::padded_bytecode(code),
            exception_table: Arc::from(vec![].as_slice()),
            max_stack: 6,
            max_locals: 4,
            num_params: 4,
            is_synchronized: false,
            is_static: true,
        },
    ));
    let (mut locals_pool, mut stacks_pool) = (Vec::new(), Vec::new());
    Frame::new_pooled_cached(cm, args, &mut locals_pool, &mut stacks_pool)
}

fn array(zgc: &crate::memory::vm_heap::VmHeap, et: ArrayElementType, len: usize) -> Option<ObjectRef> {
    zgc.try_alloc_array(ClassId::new(0), et, len)
}

/// Element `index` of `arr` as `load` pushes it, widened to `i64`.
fn get(zgc: &crate::memory::vm_heap::VmHeap, arr: ObjectRef, index: i32, load: u8) -> Option<i64> {
    let mut s = ValueStack::new(4);
    if !array_load_prim(zgc, &mut s, arr, index, load) {
        return None;
    }
    if load == 0x2f {
        Some(s.pop_compact().as_long_unchecked())
    } else {
        s.pop_compact().as_int().map(i64::from)
    }
}

/// Store `v` at `index` of `arr` with the `*astore` matching `load`.
fn set(zgc: &crate::memory::vm_heap::VmHeap, arr: ObjectRef, index: i32, load: u8, v: i64) {
    let (cv, kind) = if load == 0x2f {
        (CompactValue::long(v), ValueStack::KIND_MARK_LONG)
    } else {
        // Cast: test values fit the element width.
        (CompactValue::int(v as i32), 0)
    };
    assert!(array_store_prim(zgc, arr, index, load + 0x21, cv, kind));
}

/// `a` in local 0, `j` in 1, `b` in 2, `k` in 3.
fn args(a: Option<ObjectRef>, j: i32, b: Option<ObjectRef>, k: i32) -> [Value; 4] {
    [
        Value::Object(a),
        Value::Int(j),
        Value::Object(b),
        Value::Int(k),
    ]
}

fn local(shared: &SharedVm, zgc: &crate::memory::vm_heap::VmHeap, f: &mut Frame, pc: usize) -> Option<usize> {
    let code_len = f.code.len() - 2;
    fused_array_increment_local(shared, zgc, f, code_len, pc, &mut no_sites())
}

/// `a[j] += b[k]`: `aload_0; iload_1; dup2; iaload; aload_2; iload_3;
/// iaload; iadd; iastore; return`.
const ADD_ELEMENT: [u8; 10] = [0x2a, 0x1b, 0x5c, 0x2e, 0x2c, 0x1d, 0x2e, 0x60, 0x4f, 0xb1];
/// `a[j] += SF`: `aload_0; iload_1; dup2; iaload; getstatic #1; iadd;
/// iastore; return`.
const ADD_STATIC: [u8; 10] = [0x2a, 0x1b, 0x5c, 0x2e, 0xb2, 0x00, 0x01, 0x60, 0x4f, 0xb1];
/// `a[j]++`.
const INCREMENT: [u8; 8] = [0x2a, 0x1b, 0x5c, 0x2e, 0x04, 0x60, 0x4f, 0xb1];

#[test]
fn the_deferred_operands_decode_only_under_their_bits() {
    let f = frame(&ADD_ELEMENT, &args(None, 0, None, 0));
    let code_len = f.code.len() - 2;
    let base = array_increment_mode(false, true);
    assert_eq!(
        decode_array_increment_group(&f, &f.code, code_len, 2, base),
        None
    );
    assert_eq!(
        decode_array_increment_group(&f, &f.code, code_len, 2, base | ARRAY_INC_ARRAY_OPERAND),
        Some((
            0x2e,
            ArrayIncOperand::Element {
                array_local: 2,
                index_local: 3,
                load: 0x2e
            },
            0x60,
            9,
            FieldIncrementUse::Statement
        ))
    );
    // The static bit does not open the element form, nor the reverse.
    assert_eq!(
        decode_array_increment_group(&f, &f.code, code_len, 2, base | ARRAY_INC_STATIC_OPERAND),
        None
    );
    let f = frame(&ADD_STATIC, &args(None, 0, None, 0));
    let code_len = f.code.len() - 2;
    assert_eq!(
        decode_array_increment_group(&f, &f.code, code_len, 2, base | ARRAY_INC_ARRAY_OPERAND),
        None
    );
    assert_eq!(
        decode_array_increment_group(&f, &f.code, code_len, 2, base | ARRAY_INC_STATIC_OPERAND),
        Some((
            0x2e,
            ArrayIncOperand::Static { cp_index: 1 },
            0x60,
            9,
            FieldIncrementUse::Statement
        ))
    );
    // The tests' view never answers a deferred operand.
    assert_eq!(
        decode_array_increment_mode(&f, &f.code, code_len, 2, base | ARRAY_INC_STATIC_OPERAND),
        None
    );
    // An operand load of another width (`laload` under an int group), a
    // wide operand local (`aload 2`), an `iinc`-free index expression:
    // refused on the bytes.
    let all = base | ARRAY_INC_ARRAY_OPERAND | ARRAY_INC_STATIC_OPERAND;
    for code in [
        vec![0x2a, 0x1b, 0x5c, 0x2e, 0x2c, 0x1d, 0x2f, 0x60, 0x4f, 0xb1],
        vec![
            0x2a, 0x1b, 0x5c, 0x2e, 0x19, 0x02, 0x1d, 0x2e, 0x60, 0x4f, 0xb1,
        ],
        vec![
            0x2a, 0x1b, 0x5c, 0x2e, 0x2c, 0x1d, 0x04, 0x64, 0x2e, 0x60, 0x4f, 0xb1,
        ],
    ] {
        let f = frame(&code, &args(None, 0, None, 0));
        let code_len = f.code.len() - 2;
        assert_eq!(
            decode_array_increment_group(&f, &f.code, code_len, 2, all),
            None,
            "{code:x?}"
        );
    }
    // A wave-75 operand decodes exactly as before under the new bits.
    let f = frame(&INCREMENT, &args(None, 0, None, 0));
    let code_len = f.code.len() - 2;
    assert_eq!(
        decode_array_increment_group(&f, &f.code, code_len, 2, all),
        Some((
            0x2e,
            ArrayIncOperand::Ready(super::super::IncrementOperand::Int(1)),
            0x60,
            7,
            FieldIncrementUse::Statement
        ))
    );
}

#[test]
fn an_element_operand_is_added_only_under_the_switch() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let (Some(a), Some(b)) = (
        array(&zgc, ArrayElementType::Int, 4),
        array(&zgc, ArrayElementType::Int, 4),
    ) else {
        return;
    };
    set(&zgc, a, 1, 0x2e, 5);
    set(&zgc, b, 3, 0x2e, 7);
    for on in [false, true] {
        let shared = vm(false, on, false);
        let mut f = frame(&ADD_ELEMENT, &args(Some(a), 1, Some(b), 3));
        let before = get(&zgc, a, 1, 0x2e);
        let got = local(&shared, &zgc, &mut f, 0);
        assert_eq!(f.stack.len(), 0);
        if on {
            assert_eq!(got, Some(9));
            assert_eq!(get(&zgc, a, 1, 0x2e), before.map(|v| v + 7));
        } else {
            assert_eq!(got, None);
            assert_eq!(get(&zgc, a, 1, 0x2e), before);
        }
    }
    // The stack-top and `dup2` entries take it too.
    let shared = vm(false, true, false);
    let mut f = frame(&ADD_ELEMENT[1..], &args(Some(a), 1, Some(b), 3));
    f.stack.push(Value::Object(Some(a))).unwrap();
    let code_len = f.code.len() - 2;
    let top = fused_array_increment_top(&shared, &zgc, &mut f, code_len, 0, &mut no_sites());
    assert_eq!(top, Some(8));
    assert_eq!(f.stack.len(), 0, "the array slot is consumed");
    assert_eq!(get(&zgc, a, 1, 0x2e), Some(19));
    let mut f = frame(&ADD_ELEMENT[2..], &args(Some(a), 1, Some(b), 3));
    f.stack.push(Value::Object(Some(a))).unwrap();
    f.stack.push_int(1).unwrap();
    let code_len = f.code.len() - 2;
    let dup2 = fused_array_increment_dup2(&shared, &zgc, &mut f, code_len, 0, &mut no_sites());
    assert_eq!(dup2, Some(7));
    assert_eq!(f.stack.len(), 0);
    assert_eq!(get(&zgc, a, 1, 0x2e), Some(26));
}

#[test]
fn element_operands_alias_widen_and_decline_as_the_unfused_group() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let (Some(a), Some(bytes), Some(la), Some(lb)) = (
        array(&zgc, ArrayElementType::Int, 4),
        array(&zgc, ArrayElementType::Byte, 4),
        array(&zgc, ArrayElementType::Long, 4),
        array(&zgc, ArrayElementType::Long, 4),
    ) else {
        return;
    };
    let shared = vm(false, true, false);
    // `a[j] += a[j]`: both reads precede the store.
    set(&zgc, a, 2, 0x2e, 5);
    let alias = [0x2a, 0x1b, 0x5c, 0x2e, 0x2a, 0x1b, 0x2e, 0x60, 0x4f, 0xb1];
    let mut f = frame(&alias, &args(Some(a), 2, None, 0));
    assert_eq!(local(&shared, &zgc, &mut f, 0), Some(9));
    assert_eq!(get(&zgc, a, 2, 0x2e), Some(10));
    // A `byte[]` operand into an `int[]` element: sign-extended.
    set(&zgc, bytes, 0, 0x33, -3);
    let widen = [0x2a, 0x1b, 0x5c, 0x2e, 0x2c, 0x1d, 0x33, 0x60, 0x4f, 0xb1];
    let mut f = frame(&widen, &args(Some(a), 2, Some(bytes), 0));
    assert_eq!(local(&shared, &zgc, &mut f, 0), Some(9));
    assert_eq!(get(&zgc, a, 2, 0x2e), Some(7));
    // `la[j] += lb[k]` over `long[]`s, and `la[j] -= lb[k]`.
    set(&zgc, la, 1, 0x2f, i64::MAX);
    set(&zgc, lb, 3, 0x2f, 2);
    let ladd = [0x2a, 0x1b, 0x5c, 0x2f, 0x2c, 0x1d, 0x2f, 0x61, 0x50, 0xb1];
    let mut f = frame(&ladd, &args(Some(la), 1, Some(lb), 3));
    assert_eq!(local(&shared, &zgc, &mut f, 0), Some(9));
    assert_eq!(
        get(&zgc, la, 1, 0x2f),
        Some(i64::MIN + 1),
        "wrapping, as ladd"
    );
    // `a[j] = a[j] + b[j]` (the no-dup form, `twoLoadsMix`).
    set(&zgc, bytes, 2, 0x33, 4);
    let no_dup = [
        0x2a, 0x1b, 0x2a, 0x1b, 0x2e, 0x2c, 0x1b, 0x33, 0x60, 0x4f, 0xb1,
    ];
    let mut f = frame(&no_dup, &args(Some(a), 2, Some(bytes), 0));
    assert_eq!(local(&shared, &zgc, &mut f, 0), Some(10));
    assert_eq!(get(&zgc, a, 2, 0x2e), Some(11));
    // Every operand refusal declines having written nothing: out of
    // range either side, a null operand array, another element type.
    for (b, k) in [
        (Some(bytes), 4),
        (Some(bytes), -1),
        (None, 0),
        (Some(la), 0),
    ] {
        let mut f = frame(&widen, &args(Some(a), 2, b, k));
        assert_eq!(local(&shared, &zgc, &mut f, 0), None, "{k}");
        assert_eq!(f.stack.len(), 0);
        assert_eq!(get(&zgc, a, 2, 0x2e), Some(11));
    }
}

#[test]
fn an_operand_decline_is_not_remembered() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let (Some(a), Some(b)) = (
        array(&zgc, ArrayElementType::Int, 4),
        array(&zgc, ArrayElementType::Int, 2),
    ) else {
        return;
    };
    let shared = vm(true, true, true);
    // The operand index out of range: per execution.
    let mut f = cached_frame(&ADD_ELEMENT, &args(Some(a), 1, Some(b), 3));
    assert_eq!(local(&shared, &zgc, &mut f, 0), None);
    assert!(!array_increment_declined(&f, 0));
    // A cold static site: the unfused `getstatic` fills it.
    let mut f = cached_frame(&ADD_STATIC, &args(Some(a), 1, None, 0));
    assert_eq!(local(&shared, &zgc, &mut f, 0), None);
    assert!(!array_increment_declined(&f, 0));
    // With both switches off the same bytes are a shape decline, and
    // remembered (wave 75).
    let shared = vm(true, false, false);
    let mut f = cached_frame(&ADD_STATIC, &args(Some(a), 1, None, 0));
    assert_eq!(local(&shared, &zgc, &mut f, 0), None);
    assert!(array_increment_declined(&f, 0));
}

fn static_field(declaring: ClassId, desc_byte: u8, is_volatile: bool) -> ResolvedField {
    ResolvedField {
        declaring_class_id: declaring,
        field_index: 0,
        is_static: true,
        is_volatile,
        is_final: false,
        is_reference: false,
        desc_byte,
    }
}

/// Fill `#1`'s static site with `field`, then run the local entry over
/// `code` until a fill survives the epochs other tests move (as the
/// wave-5 `getstatic` site tests do); the entry's answer when it served,
/// `None` when no fill stuck or it declined.
fn run_with_site(
    shared: &SharedVm,
    zgc: &crate::memory::vm_heap::VmHeap,
    code: &[u8],
    a: ObjectRef,
    field: &ResolvedField,
) -> Option<usize> {
    for _ in 0..1000 {
        let mut sites = FieldSiteCache::new();
        sites.put(
            ClassId::new(CLASS),
            1,
            FieldSiteCache::epochs_now(),
            field.clone(),
        );
        if sites.get(ClassId::new(CLASS), 1).is_none() {
            continue;
        }
        let mut f = frame(code, &args(Some(a), 1, None, 0));
        let code_len = f.code.len() - 2;
        return fused_array_increment_local(shared, zgc, &mut f, code_len, 0, &mut sites);
    }
    None
}

#[test]
fn a_static_operand_is_read_from_a_filled_site_of_the_groups_width() {
    if cratonvm_classloading::any_class_redefined() {
        return; // the site tables are latched off for this process
    }
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let (Some(a), Some(bytes), Some(la)) = (
        array(&zgc, ArrayElementType::Int, 4),
        array(&zgc, ArrayElementType::Byte, 4),
        array(&zgc, ArrayElementType::Long, 4),
    ) else {
        return;
    };
    let shared = vm(false, false, true);
    let decl = ClassId::new(90_761);
    crate::vm::set_static_shared(&shared, decl, 0, Value::Int(6));
    // `a[j] += SF` (an `I` static, volatile or not).
    for volatile in [false, true] {
        let before = get(&zgc, a, 1, 0x2e);
        let got = run_with_site(
            &shared,
            &zgc,
            &ADD_STATIC,
            a,
            &static_field(decl, b'I', volatile),
        );
        if got.is_some() {
            assert_eq!(got, Some(9));
            assert_eq!(get(&zgc, a, 1, 0x2e), before.map(|v| v + 6));
        }
    }
    // A `B` static into a `byte[]` element, narrowed by the group's `i2b`.
    set(&zgc, bytes, 1, 0x33, 125);
    let byte_group = [
        0x2a, 0x1b, 0x5c, 0x33, 0xb2, 0x00, 0x01, 0x60, 0x91, 0x54, 0xb1,
    ];
    let byte_field = static_field(decl, b'B', false);
    if run_with_site(&shared, &zgc, &byte_group, bytes, &byte_field).is_some() {
        assert_eq!(get(&zgc, bytes, 1, 0x33), Some(-125), "125 + 6 as i2b");
    }
    // A `J` static into a `long[]` element (an `Int` slot widens).
    let long_group = [0x2a, 0x1b, 0x5c, 0x2f, 0xb2, 0x00, 0x01, 0x61, 0x50, 0xb1];
    let long_field = static_field(decl, b'J', false);
    if run_with_site(&shared, &zgc, &long_group, la, &long_field).is_some() {
        assert_eq!(get(&zgc, la, 1, 0x2f), Some(6));
    }
    // Another width declines, nothing written: a `J` static under an int
    // group, an `I` static under a long one, a reference.
    let before = get(&zgc, a, 1, 0x2e);
    assert_eq!(
        run_with_site(
            &shared,
            &zgc,
            &ADD_STATIC,
            a,
            &static_field(decl, b'J', false)
        ),
        None
    );
    assert_eq!(get(&zgc, a, 1, 0x2e), before);
    assert_eq!(
        run_with_site(
            &shared,
            &zgc,
            &long_group,
            la,
            &static_field(decl, b'I', false)
        ),
        None
    );
    let mut reference = static_field(decl, b'L', false);
    reference.is_reference = true;
    assert_eq!(
        run_with_site(&shared, &zgc, &ADD_STATIC, a, &reference),
        None
    );
    assert_eq!(get(&zgc, a, 1, 0x2e), before);
    // Without the switch a filled site is not consulted.
    let off = vm(false, false, false);
    assert_eq!(
        run_with_site(&off, &zgc, &ADD_STATIC, a, &static_field(decl, b'I', false)),
        None
    );
    assert_eq!(get(&zgc, a, 1, 0x2e), before);
}

#[test]
fn a_remembered_pc_is_declined_inside_each_entry() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let Some(a) = array(&zgc, ArrayElementType::Int, 4) else {
        return;
    };
    for memo in [false, true] {
        let shared = vm(memo, false, false);
        let mut f = cached_frame(&INCREMENT, &args(Some(a), 1, None, 0));
        // A bit set by an earlier decline (here by hand).
        note_array_increment_decline(&f, 0);
        assert!(array_increment_declined(&f, 0));
        let before = get(&zgc, a, 1, 0x2e);
        let got = local(&shared, &zgc, &mut f, 0);
        if memo {
            assert_eq!(got, None, "the entry tests the memo first");
            assert_eq!(get(&zgc, a, 1, 0x2e), before);
        } else {
            assert_eq!(got, Some(7), "without the switch the bit is not read");
            assert_eq!(get(&zgc, a, 1, 0x2e), before.map(|v| v + 1));
        }
        // The stack-top entry at its `iload_1`.
        let mut f = cached_frame(&INCREMENT[1..], &args(Some(a), 1, None, 0));
        f.stack.push(Value::Object(Some(a))).unwrap();
        note_array_increment_decline(&f, 0);
        let code_len = f.code.len() - 2;
        let got =
            fused_array_increment_top(&shared, &zgc, &mut f, code_len, 0, &mut no_sites());
        assert_eq!(got.is_some(), !memo);
        // The `dup2` entry at its `dup2`.
        let mut f = cached_frame(&INCREMENT[2..], &args(Some(a), 1, None, 0));
        f.stack.push(Value::Object(Some(a))).unwrap();
        f.stack.push_int(1).unwrap();
        note_array_increment_decline(&f, 0);
        let code_len = f.code.len() - 2;
        let got =
            fused_array_increment_dup2(&shared, &zgc, &mut f, code_len, 0, &mut no_sites());
        assert_eq!(got.is_some(), !memo);
        assert_eq!(f.stack.len(), if memo { 2 } else { 0 });
    }
}

#[test]
fn the_wide_local_entry_tests_the_heap_and_reads_as_the_body() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let Some(ints) = array(&zgc, ArrayElementType::Int, 3) else {
        return;
    };
    set(&zgc, ints, 1, 0x2e, -7);
    let shared = vm(false, false, false);
    // `aload_0; iload_3; iaload` through the wide entry's short-index arm.
    let code = [0x2a, 0x1d, 0x2e, 0xb1];
    let mut f = frame(&code, &args(Some(ints), 0, None, 1));
    let code_len = f.code.len() - 2;
    assert_eq!(
        array_load_wide_locals_at(&shared, None, &mut f, code_len, 1, 0),
        None
    );
    assert_eq!(f.stack.len(), 0);
    assert_eq!(
        array_load_wide_locals_at(&shared, Some(&zgc), &mut f, code_len, 1, 0),
        Some(3)
    );
    assert_eq!(f.stack.pop_compact().as_int(), Some(-7));
}
