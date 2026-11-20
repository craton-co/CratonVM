//! Interpreter round i3 wave 75, lane L7: the fused array increment's
//! per-method decline memo (`CRATONVM_INTERP_FUSED_ARRAY_DECLINE_MEMO`,
//! `array_increment_declined` / `note_array_increment_decline`). A group
//! the decoder refuses on its bytes sets its entry pc's bit, under the
//! switch only; a per-execution decline (null array, index out of range,
//! zero divisor) sets nothing; the bit is per pc, so a hit elsewhere in
//! the method still fuses; a frame without a cached method entry keeps no
//! memo. Each test sets the switches it needs explicitly.
use super::*;
use crate::classloading::resolution::CachedBytecodeMethod;
use std::sync::Arc;

fn vm(memo: bool) -> Arc<SharedVm> {
    let mut config = crate::config::VmConfig::default();
    config.interp_fused_array_increment = true;
    config.interp_fused_array_increment_value = true;
    config.interp_fused_array_increment_top = true;
    config.interp_fused_array_increment_dup2 = true;
    config.interp_fused_array_accumulate = false;
    config.interp_fused_array_increment_shift_div = true;
    config.interp_fused_array_update_no_dup = true;
    config.interp_fused_array_decline_memo = memo;
    config.interp_fused_array_increment_array_operand = false;
    config.interp_fused_array_increment_static_operand = false;
    config.site_census = false;
    Arc::new(SharedVm::new(config))
}

/// A static frame pushed from a fresh `CachedBytecodeMethod` over `code`
/// (padded as the cached-invoke path pads it), `args` in its first
/// locals, `max_locals` 4.
fn cached_frame(code: &[u8], args: &[Value]) -> Frame {
    let cm = Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: ClassId::new(0x00ab_0751),
            class_name: Arc::from("cratonvm/test/i75l7/DeclineMemo"),
            method_name: Arc::from("m"),
            method_descriptor: Arc::from("([III)V"),
            source_file: None,
            code: crate::runtime::frame::padded_bytecode(code),
            exception_table: Arc::from(vec![].as_slice()),
            max_stack: 6,
            max_locals: 4,
            num_params: 3,
            is_synchronized: false,
            is_static: true,
        },
    ));
    let (mut locals_pool, mut stacks_pool) = (Vec::new(), Vec::new());
    Frame::new_pooled_cached(cm, args, &mut locals_pool, &mut stacks_pool)
}

/// The same frame built the owned way (no cached method entry).
fn owned_frame(code: &[u8], args: &[Value]) -> Frame {
    Frame::new(
        ClassId::new(0x00ab_0752),
        "cratonvm/test/i75l7/DeclineMemoOwned".to_string(),
        "m".to_string(),
        "([III)V".to_string(),
        None,
        code.to_vec(),
        vec![],
        6,
        4,
        args,
    )
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

/// `a[j] += SF` (`aload_0; iload_1; dup2; iaload; getstatic #1; iadd;
/// iastore`), pc 0..=8: a group the decoder refuses on its bytes.
const STATIC_OPERAND: [u8; 9] = [0x2a, 0x1b, 0x5c, 0x2e, 0xb2, 0x00, 0x01, 0x60, 0x4f];
/// `a[j]++` (`aload_0; iload_1; dup2; iaload; iconst_1; iadd; iastore`).
const INCREMENT: [u8; 7] = [0x2a, 0x1b, 0x5c, 0x2e, 0x04, 0x60, 0x4f];

/// `bytes` then `return`.
fn method(bytes: &[u8]) -> Vec<u8> {
    let mut code = bytes.to_vec();
    code.push(0xb1);
    code
}

fn args(arr: Option<ObjectRef>, index: i32) -> [Value; 3] {
    [Value::Object(arr), Value::Int(index), Value::Int(3)]
}

#[test]
fn a_shape_decline_is_remembered_only_under_the_switch() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let Some(arr) = ints(&zgc, 4) else {
        return;
    };
    let code = method(&STATIC_OPERAND);
    for memo in [false, true] {
        let shared = vm(memo);
        let mut f = cached_frame(&code, &args(Some(arr), 1));
        let code_len = f.code.len() - 2;
        assert!(!array_increment_declined(&f, 0));
        assert_eq!(
            fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
            None
        );
        assert_eq!(f.stack.len(), 0, "a decline pushes nothing");
        assert_eq!(array_increment_declined(&f, 0), memo, "memo {memo}");
        // Only the entry's own pc.
        for pc in 1..code.len() {
            assert!(!array_increment_declined(&f, pc), "pc {pc}");
        }
        assert_eq!(get(&zgc, arr, 1), Some(0), "nothing written");
    }
}

#[test]
fn each_entry_remembers_its_own_pc() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let Some(arr) = ints(&zgc, 4) else {
        return;
    };
    let shared = vm(true);
    // The stack-top entry at its `iload_1`.
    let mut f = cached_frame(&method(&STATIC_OPERAND[1..]), &args(Some(arr), 1));
    f.stack.push(Value::Object(Some(arr))).unwrap();
    let code_len = f.code.len() - 2;
    assert_eq!(
        fused_array_increment_top(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
        None
    );
    assert_eq!(f.stack.len(), 1, "the array slot is left");
    assert!(array_increment_declined(&f, 0));
    // The `dup2` entry at its `dup2`.
    let mut f = cached_frame(&method(&STATIC_OPERAND[2..]), &args(Some(arr), 1));
    f.stack.push(Value::Object(Some(arr))).unwrap();
    f.stack.push_int(1).unwrap();
    let code_len = f.code.len() - 2;
    assert_eq!(
        fused_array_increment_dup2(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
        None
    );
    assert_eq!(f.stack.len(), 2, "both slots are left");
    assert!(array_increment_declined(&f, 0));
    // The no-dup prefix that is not an update of the same element
    // (`a[j] = a[k] + 1`): refused on its bytes, remembered.
    let code = method(&[0x2a, 0x1b, 0x2a, 0x1c, 0x2e, 0x04, 0x60, 0x4f]);
    let mut f = cached_frame(&code, &args(Some(arr), 1));
    let code_len = f.code.len() - 2;
    assert_eq!(
        fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
        None
    );
    assert!(array_increment_declined(&f, 0));
    assert_eq!(get(&zgc, arr, 1), Some(0));
}

#[test]
fn a_per_execution_decline_is_not_remembered() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let Some(arr) = ints(&zgc, 4) else {
        return;
    };
    let shared = vm(true);
    let code = method(&INCREMENT);
    // A null array, an index out of range either side.
    for (a, index) in [(None, 1), (Some(arr), 4), (Some(arr), -1)] {
        let mut f = cached_frame(&code, &args(a, index));
        let code_len = f.code.len() - 2;
        assert_eq!(
            fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
            None
        );
        assert!(!array_increment_declined(&f, 0), "{index}");
    }
    // A zero divisor (`a[j] /= 0`): the unfused `idiv` throws, every time.
    let div = method(&[0x2a, 0x1b, 0x5c, 0x2e, 0x03, 0x6c, 0x4f]);
    let mut f = cached_frame(&div, &args(Some(arr), 1));
    let code_len = f.code.len() - 2;
    assert_eq!(
        fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
        None
    );
    assert!(!array_increment_declined(&f, 0));
    // The increment hits once its array and index are good.
    let mut f = cached_frame(&code, &args(Some(arr), 1));
    let code_len = f.code.len() - 2;
    assert_eq!(
        fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
        Some(7)
    );
    assert!(!array_increment_declined(&f, 0));
    assert_eq!(get(&zgc, arr, 1), Some(1));
}

#[test]
fn a_remembered_site_leaves_the_other_sites_of_its_method_fused() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let Some(arr) = ints(&zgc, 4) else {
        return;
    };
    let shared = vm(true);
    // `a[j] += SF; a[j]++; return`: the second group starts at pc 9.
    let mut bytes = STATIC_OPERAND.to_vec();
    bytes.extend_from_slice(&INCREMENT);
    let mut f = cached_frame(&method(&bytes), &args(Some(arr), 2));
    let code_len = f.code.len() - 2;
    assert_eq!(
        fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
        None
    );
    assert!(array_increment_declined(&f, 0));
    assert!(!array_increment_declined(&f, 9));
    assert_eq!(
        fused_array_increment_local(&shared, &zgc, &mut f, code_len, 9, &mut no_sites()),
        Some(16)
    );
    assert!(!array_increment_declined(&f, 9));
    assert_eq!(get(&zgc, arr, 2), Some(1));
    // A pc past the bitmap answers "not declined".
    assert!(!array_increment_declined(&f, 1 << 20));
}

#[test]
fn a_frame_without_a_cached_method_keeps_no_memo() {
    let zgc = VmHeap::from_zgc_capacity(1 << 20);
    let Some(arr) = ints(&zgc, 4) else {
        return;
    };
    let shared = vm(true);
    let mut f = owned_frame(&method(&STATIC_OPERAND), &args(Some(arr), 1));
    assert!(f.cached_method().is_none());
    let code_len = f.code.len() - 2;
    assert_eq!(
        fused_array_increment_local(&shared, &zgc, &mut f, code_len, 0, &mut no_sites()),
        None
    );
    assert!(!array_increment_declined(&f, 0));
}
