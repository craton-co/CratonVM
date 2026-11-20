use super::{
    callee_template_declines_redefined_target, callee_templates_latched_off, jit_site_key,
    ldc_slot_memo_latched_off, try_jit_static_bytecode_callee, typecheck_memos_latched_off,
    ClassId, JitInvokeInfo, Value, STATIC_BYTECODE_CALLEE_CACHE,
};
use crate::classloading::resolution::{CachedBytecodeMethod, RedefineGate};
use crate::config::VmConfig;
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use std::sync::atomic::AtomicU32;
use std::sync::Arc;

static SITE: JitInvokeInfo = JitInvokeInfo {
    class_name: "cratonvm/test/I18L4Callee",
    method_name: "twice",
    descriptor: "(I)I",
    num_jit_args: 1,
    return_type: b'I',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// None of the memos is latched: whether a class was ever redefined in
/// this process does not decide whether they serve.
#[test]
fn no_helper_memo_is_latched_off_by_a_redefinition() {
    assert!(!ldc_slot_memo_latched_off());
    assert!(!typecheck_memos_latched_off());
    assert!(!callee_templates_latched_off());
    assert!(!callee_template_declines_redefined_target(0));
    // Wave 40 (lane L2): a template of a class redefined before its fill
    // is served, unless the kill switch restores the wave-18 decline.
    assert_eq!(
        callee_template_declines_redefined_target(1),
        !super::CALLEE_TEMPLATES_SERVE_REDEFINED_TARGETS
    );
}

/// Under `CALLEE_TEMPLATES_SERVE_REDEFINED_TARGETS`'s kill switch, a
/// template whose declaring class was redefined is current (its gate is
/// not stale: filled after the redefinition) but is still not served:
/// that callee keeps the by-name path. It is kept, not evicted, so the
/// site does not re-resolve on every call. (With the switch on, the
/// default since wave 40, it is served and the call runs, which this
/// stripped thread cannot do.)
#[test]
fn a_template_of_a_redefined_callee_is_kept_but_not_served() {
    if !crate::runtime::env_cache::jit_static_bytecode_callee()
        || !callee_template_declines_redefined_target(1)
    {
        return;
    }
    let vm = SharedVm::new(VmConfig::default());
    let key = jit_site_key(vm.vm_identity, &SITE as *const JitInvokeInfo as usize);
    let cached = Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: ClassId::new(0x00AB_1811),
            class_name: Arc::from("cratonvm/test/I18L4Callee"),
            method_name: Arc::from("twice"),
            method_descriptor: Arc::from("(I)I"),
            source_file: None,
            // iload_0; iload_0; iadd; ireturn
            code: crate::runtime::frame::padded_bytecode(&[0x1a, 0x1a, 0x60, 0xac]),
            exception_table: Arc::from(Vec::new()),
            max_stack: 2,
            max_locals: 1,
            num_params: 1,
            is_synchronized: false,
            is_static: true,
        },
    ));
    // Redefined once, and the entry was filled after that: not stale.
    let gate = RedefineGate::snapshot(Arc::new(AtomicU32::new(1)));
    assert!(!gate.is_stale());
    STATIC_BYTECODE_CALLEE_CACHE.with(|c| {
        c.borrow_mut().insert(key, Some((cached, gate)));
    });
    let mut thread = JvmThread::new(ThreadId(18_418), "i18-l4-template");
    // SAFETY: `SITE` is a static `JitInvokeInfo`; the call declines before
    // it reads the thread or the arguments.
    let args = [Value::Int(3)];
    let served = unsafe { try_jit_static_bytecode_callee(&vm, &mut thread, &SITE, key, &args) };
    assert!(
        served.is_none(),
        "a redefined callee keeps the by-name path"
    );
    assert!(
        STATIC_BYTECODE_CALLEE_CACHE.with(|c| c.borrow().contains_key(&key)),
        "a current template is kept"
    );
    STATIC_BYTECODE_CALLEE_CACHE.with(|c| {
        c.borrow_mut().remove(&key);
    });
}
