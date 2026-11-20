//! Interpreter round i3 wave 77, lane L7: the deferred operands' miss
//! budget (`CRATONVM_INTERP_FUSED_ARRAY_OPERAND_MISS_BUDGET`) and the
//! element operand's wide-local and index-expression forms
//! (`CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_ARRAY_OPERAND_WIDE`). Each test
//! sets every switch it depends on explicitly.
use super::*;
use crate::classloading::resolution::CachedBytecodeMethod;
use std::sync::Arc;

const CLASS: u32 = 0x00ab_0771;

fn vm(memo: bool, budget: bool, wide: bool) -> Arc<SharedVm> {
    let mut config = crate::config::VmConfig::default();
    config.interp_fused_array_increment = true;
    config.interp_fused_array_increment_value = true;
    config.interp_fused_array_increment_top = true;
    config.interp_fused_array_increment_dup2 = true;
    config.interp_fused_array_accumulate = false;
    config.interp_fused_array_increment_shift_div = true;
    config.interp_fused_array_update_no_dup = true;
    config.interp_fused_array_decline_memo = memo;
    config.interp_fused_array_increment_array_operand = true;
    config.interp_fused_array_increment_static_operand = true;
    config.interp_fused_array_operand_miss_budget = budget;
    config.interp_fused_array_increment_array_operand_wide = wide;
    config.interp_fused_wide_locals = true;
    config.site_census = false;
    Arc::new(SharedVm::new(config))
}

/// An owned static frame over `code`, `max_locals` 8, `args` in its
/// first locals.
fn frame(code: &[u8], args: &[Value]) -> Frame {
    Frame::new(
        ClassId::new(CLASS),
        "cratonvm/test/i77l7/ArrayOperand".to_string(),
        "m".to_string(),
        "()V".to_string(),
        None,
        code.to_vec(),
        vec![],
        6,
        8,
        args,
    )
}

/// The same frame pushed from a fresh `CachedBytecodeMethod` (so it keeps
/// a decline memo and a miss counter).
fn cached_frame(code: &[u8], args: &[Value]) -> Frame {
    let cm = Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: ClassId::new(CLASS),
            class_name: Arc::from("cratonvm/test/i77l7/ArrayOperandCached"),
            method_name: Arc::from("m"),
            method_descriptor: Arc::from("()V"),
            source_file: None,
            code: crate::runtime::frame::padded_bytecode(code),
            exception_table: Arc::from(vec![].as_slice()),
            max_stack: 6,
            max_locals: 8,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
        },
    ));
    let (mut locals_pool, mut stacks_pool) = (Vec::new(), Vec::new());
    Frame::new_pooled_cached(cm, args, &mut locals_pool, &mut stacks_pool)
}

fn ints(zgc: &crate::memory::vm_heap::VmHeap, len: usize) -> Option<ObjectRef> {
    zgc.try_alloc_array(ClassId::new(0), ArrayElementType::Int, len)
}

fn get(zgc: &crate::memory::vm_heap::VmHeap, arr: ObjectRef, index: i32) -> Option<i32> {
    let mut s = ValueStack::new(4);
    if !array_load_prim(zgc, &mut s, arr, index, 0x2e) {
        return None;
    }
    s.pop_compact().as_int()
}

fn set(zgc: &crate::memory::vm_heap::VmHeap, arr: ObjectRef, index: i32, v: i32) {
    assert!(array_store_prim(
        zgc,
        arr,
        index,
        0x4f,
        CompactValue::int(v),
        0
    ));
}

fn local(shared: &SharedVm, zgc: &crate::memory::vm_heap::VmHeap, f: &mut Frame, pc: usize) -> Option<usize> {
    let code_len = f.code.len() - 2;
    fused_array_increment_local(shared, zgc, f, code_len, pc, &mut FieldSiteCache::new())
}

fn misses_allocated(f: &Frame) -> bool {
    f.cached_method()
        .and_then(|cm| cm.array_inc_declined.get())
        .is_some_and(|memo| memo.operand_misses.get().is_some())
}

/// `a[j] += SF`: `aload_0; iload_1; dup2; iaload; getstatic #1; iadd;
/// iastore; return`.
const ADD_STATIC: [u8; 10] = [0x2a, 0x1b, 0x5c, 0x2e, 0xb2, 0x00, 0x01, 0x60, 0x4f, 0xb1];
/// `a[j] += b[k]`: `aload_0; iload_1; dup2; iaload; aload_2; iload_3;
/// iaload; iadd; iastore; return`.
const ADD_ELEMENT: [u8; 10] = [0x2a, 0x1b, 0x5c, 0x2e, 0x2c, 0x1d, 0x2e, 0x60, 0x4f, 0xb1];

#[test]
fn a_cold_static_operand_retires_its_pc_only_under_the_budget() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let Some(a) = ints(&zgc, 4) else {
        return;
    };
    for budget in [false, true] {
        let shared = vm(true, budget, false);
        // A site that never fills (an empty table every time, as a
        // `<clinit>` access leaves it).
        let mut f = cached_frame(&ADD_STATIC, &[Value::Object(Some(a)), Value::Int(1)]);
        for _ in 1..ARRAY_OPERAND_MISS_BUDGET {
            assert_eq!(local(&shared, &zgc, &mut f, 0), None);
        }
        assert!(!array_increment_declined(&f, 0), "below the budget");
        assert_eq!(misses_allocated(&f), budget);
        assert_eq!(local(&shared, &zgc, &mut f, 0), None);
        assert_eq!(
            array_increment_declined(&f, 0),
            budget,
            "the budget-th miss"
        );
        assert_eq!(f.stack.len(), 0);
    }
}

#[test]
fn a_missing_element_operand_counts_too_and_saturates() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let (Some(a), Some(b)) = (ints(&zgc, 4), ints(&zgc, 2)) else {
        return;
    };
    let shared = vm(true, true, false);
    // `b[3]` is out of range: a per-execution decline, counted.
    let args = [
        Value::Object(Some(a)),
        Value::Int(1),
        Value::Object(Some(b)),
        Value::Int(3),
    ];
    let mut f = cached_frame(&ADD_ELEMENT, &args);
    for _ in 0..ARRAY_OPERAND_MISS_BUDGET {
        assert_eq!(local(&shared, &zgc, &mut f, 0), None);
    }
    assert!(array_increment_declined(&f, 0));
    // Further misses (here by hand) neither wrap the counter nor clear
    // the bit.
    for _ in 0..300 {
        assert!(note_array_increment_operand_miss(&f, 0));
    }
    let counter = f
        .cached_method()
        .and_then(|cm| cm.array_inc_declined.get())
        .and_then(|memo| memo.operand_misses.get())
        .map(|m| m[0].load(Ordering::Relaxed));
    assert_eq!(counter, Some(ARRAY_OPERAND_MISS_BUDGET));
    assert!(array_increment_declined(&f, 0));
}

#[test]
fn the_budget_needs_the_memo_and_a_hit_never_counts() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let (Some(a), Some(b)) = (ints(&zgc, 4), ints(&zgc, 4)) else {
        return;
    };
    // Memo off: nothing is counted or retired.
    let shared = vm(false, true, false);
    let mut f = cached_frame(&ADD_STATIC, &[Value::Object(Some(a)), Value::Int(1)]);
    for _ in 0..u32::from(ARRAY_OPERAND_MISS_BUDGET) + 4 {
        assert_eq!(local(&shared, &zgc, &mut f, 0), None);
    }
    assert!(!array_increment_declined(&f, 0));
    assert!(!misses_allocated(&f));
    // Hits leave no counter behind.
    let shared = vm(true, true, false);
    let args = [
        Value::Object(Some(a)),
        Value::Int(1),
        Value::Object(Some(b)),
        Value::Int(3),
    ];
    let mut f = cached_frame(&ADD_ELEMENT, &args);
    set(&zgc, b, 3, 2);
    for _ in 0..u32::from(ARRAY_OPERAND_MISS_BUDGET) + 4 {
        assert_eq!(local(&shared, &zgc, &mut f, 0), Some(9));
    }
    assert!(!misses_allocated(&f));
    assert!(!array_increment_declined(&f, 0));
}

#[test]
fn wide_locals_and_index_expressions_decode_only_under_their_bit() {
    let f = frame(&[0xb1], &[]);
    let base = array_increment_mode(false, true) | ARRAY_INC_ARRAY_OPERAND;
    let wide = base | ARRAY_INC_ARRAY_OPERAND_WIDE;
    // (code, the operand it decodes to under `wide`, the pc past the
    // group)
    let cases: [(Vec<u8>, ArrayIncOperand, usize); 5] = [
        // acc[i] += x[i] with acc, i, x in 4, 5, 6: `aload 4; iload 5;
        // dup2; iaload; aload 6; iload 5; iaload; iadd; iastore`.
        (
            vec![
                0x19, 4, 0x15, 5, 0x5c, 0x2e, 0x19, 6, 0x15, 5, 0x2e, 0x60, 0x4f, 0xb1,
            ],
            ArrayIncOperand::Element {
                array_local: 6,
                index_local: 5,
                load: 0x2e,
            },
            13,
        ),
        // p[i] += p[i - 1]: `aload_0; iload_1; dup2; iaload; aload_0;
        // iload_1; iconst_1; isub; iaload; iadd; iastore`.
        (
            vec![
                0x2a, 0x1b, 0x5c, 0x2e, 0x2a, 0x1b, 0x04, 0x64, 0x2e, 0x60, 0x4f, 0xb1,
            ],
            ArrayIncOperand::ElementAt {
                array_local: 0,
                index_local: 1,
                delta: -1,
                load: 0x2e,
            },
            11,
        ),
        // h[k] += w[k + 200] (`sipush`), w in local 7.
        (
            vec![
                0x2a, 0x1b, 0x5c, 0x2e, 0x19, 7, 0x1b, 0x11, 0x00, 0xc8, 0x60, 0x2e, 0x60,
                0x4f, 0xb1,
            ],
            ArrayIncOperand::ElementAt {
                array_local: 7,
                index_local: 1,
                delta: 200,
                load: 0x2e,
            },
            14,
        ),
        // a[j] += b[k + 3] with k wide (`bipush`): `aload_2; iload 5;
        // bipush 3; iadd; iaload`.
        (
            vec![
                0x2a, 0x1b, 0x5c, 0x2e, 0x2c, 0x15, 5, 0x10, 3, 0x60, 0x2e, 0x60, 0x4f, 0xb1,
            ],
            ArrayIncOperand::ElementAt {
                array_local: 2,
                index_local: 5,
                delta: 3,
                load: 0x2e,
            },
            13,
        ),
        // A `byte[]` operand with a short array and a wide index.
        (
            vec![
                0x2a, 0x1b, 0x5c, 0x2e, 0x2c, 0x15, 6, 0x33, 0x60, 0x4f, 0xb1,
            ],
            ArrayIncOperand::Element {
                array_local: 2,
                index_local: 6,
                load: 0x33,
            },
            10,
        ),
    ];
    for (code, operand, next) in cases {
        let p = if code[0] == 0x19 { 4 } else { 2 };
        let code_len = code.len();
        assert_eq!(
            decode_array_increment_group(&f, &code, code_len, p, base),
            None,
            "{code:x?} without the bit"
        );
        assert_eq!(
            decode_array_increment_group(&f, &code, code_len, p, wide),
            Some((0x2e, operand, 0x60, next, FieldIncrementUse::Statement)),
            "{code:x?}"
        );
    }
    // Still refused under the bit: a local at or past `max_locals` (8),
    // an index expression of another arithmetic (`imul`) or with a local
    // addend, an operand load of another width.
    for code in [
        vec![
            0x2a, 0x1b, 0x5c, 0x2e, 0x19, 8, 0x1b, 0x2e, 0x60, 0x4f, 0xb1,
        ],
        vec![
            0x2a, 0x1b, 0x5c, 0x2e, 0x2c, 0x15, 9, 0x2e, 0x60, 0x4f, 0xb1,
        ],
        vec![
            0x2a, 0x1b, 0x5c, 0x2e, 0x2c, 0x1b, 0x05, 0x68, 0x2e, 0x60, 0x4f, 0xb1,
        ],
        vec![
            0x2a, 0x1b, 0x5c, 0x2e, 0x2c, 0x1b, 0x1c, 0x60, 0x2e, 0x60, 0x4f, 0xb1,
        ],
        vec![
            0x2a, 0x1b, 0x5c, 0x2e, 0x2c, 0x15, 5, 0x2f, 0x60, 0x4f, 0xb1,
        ],
    ] {
        let code_len = code.len();
        assert_eq!(
            decode_array_increment_group(&f, &code, code_len, 2, wide),
            None,
            "{code:x?}"
        );
    }
    // The wide bit alone (without the element operand's) opens nothing.
    let code = vec![
        0x2a, 0x1b, 0x5c, 0x2e, 0x19, 6, 0x15, 5, 0x2e, 0x60, 0x4f, 0xb1,
    ];
    let only_wide = array_increment_mode(false, true) | ARRAY_INC_ARRAY_OPERAND_WIDE;
    assert_eq!(
        decode_array_increment_group(&f, &code, code.len(), 2, only_wide),
        None
    );
}

#[test]
fn an_index_expression_operand_reads_and_declines_as_the_unfused_group() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let Some(p) = ints(&zgc, 4) else {
        return;
    };
    for (i, v) in [(0, 1), (1, 2), (2, 3), (3, 4)] {
        set(&zgc, p, i, v);
    }
    // p[i] += p[i - 1]
    let prefix = [
        0x2a, 0x1b, 0x5c, 0x2e, 0x2a, 0x1b, 0x04, 0x64, 0x2e, 0x60, 0x4f, 0xb1,
    ];
    let off = vm(false, false, false);
    let mut f = frame(&prefix, &[Value::Object(Some(p)), Value::Int(2)]);
    assert_eq!(local(&off, &zgc, &mut f, 0), None, "switch off");
    assert_eq!(get(&zgc, p, 2), Some(3));
    let on = vm(false, false, true);
    let mut f = frame(&prefix, &[Value::Object(Some(p)), Value::Int(2)]);
    assert_eq!(local(&on, &zgc, &mut f, 0), Some(11));
    assert_eq!(get(&zgc, p, 2), Some(5), "3 + p[1]");
    assert_eq!(f.stack.len(), 0);
    // i = 0: the unfused `p[-1]` throws; the fused group declines having
    // written nothing.
    let mut f = frame(&prefix, &[Value::Object(Some(p)), Value::Int(0)]);
    assert_eq!(local(&on, &zgc, &mut f, 0), None);
    assert_eq!(get(&zgc, p, 0), Some(1));
    // A wide operand array: `aload_0; iload_1; dup2; iaload; aload 5;
    // iload 6; iaload; iadd; iastore`.
    let Some(w) = ints(&zgc, 8) else {
        return;
    };
    set(&zgc, w, 7, 40);
    let wide = [
        0x2a, 0x1b, 0x5c, 0x2e, 0x19, 5, 0x15, 6, 0x2e, 0x60, 0x4f, 0xb1,
    ];
    let args = [
        Value::Object(Some(p)),
        Value::Int(3),
        Value::Int(0),
        Value::Int(0),
        Value::Int(0),
        Value::Object(Some(w)),
        Value::Int(7),
    ];
    let mut f = frame(&wide, &args);
    assert_eq!(local(&on, &zgc, &mut f, 0), Some(11));
    assert_eq!(get(&zgc, p, 3), Some(44));
}
