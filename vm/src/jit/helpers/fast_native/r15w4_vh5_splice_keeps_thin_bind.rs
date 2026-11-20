use super::*;
use crate::config::VmConfig;

/// VH3-5: a `compareAndSet` site is protected like a read, and the first
/// exact mint withdraws the protection with the binds.
#[test]
pub fn the_refusal_follows_the_bind() {
    if !cratonvm_native_builtins::lang_invoke::vh_splice_refusal_asks_bind_enabled()
        || !cratonvm_jit::direct_jit_callee_calls_enabled()
        || !cratonvm_jit::varhandle_cas_direct_helpers_enabled()
        || !cratonvm_jit::varhandle_read_direct_helpers_enabled()
    {
        return; // an explicit `0` in the test environment wins
    }
    let shared = SharedVm::new(VmConfig::default());
    let cas = "(Ljava/lang/Object;II)Z";
    let read = "(Ljava/lang/Object;)I";
    assert!(cratonvm_jit::varhandle_cas_helper_slot("compareAndSet", cas).is_some());
    assert!(varhandle_splice_keeps_thin_bind(
        &shared,
        "compareAndSet",
        cas,
        false
    ));
    assert!(varhandle_splice_keeps_thin_bind(
        &shared, "get", read, false
    ));
    // JIT round 20 wave 11 (lane pIndy, W14L4-2): an optimizing-tier
    // splice whose call the IR bind loop binds keeps the bind.
    let ir_bound = |shared: &SharedVm, name: &str, desc: &str| {
        cratonvm_jit::varhandle_ir_bound_family(name, desc).is_some_and(|erased| {
                varhandle_method_entry_binds_admitted(shared)
                    && direct_native_bind_admitted(
                        shared,
                        "java/lang/invoke/VarHandle",
                        name,
                        erased,
                    )
            })
    };
    assert_eq!(
        varhandle_splice_keeps_thin_bind(&shared, "get", read, true),
        !cratonvm_jit::varhandle_ir_read_shape("get", read) && !ir_bound(&shared, "get", read)
    );
    assert_eq!(
        varhandle_splice_keeps_thin_bind(&shared, "compareAndSet", cas, true),
        !ir_bound(&shared, "compareAndSet", cas)
    );
    // Not a thin-bindable shape (an array element read): never refused.
    assert!(!varhandle_splice_keeps_thin_bind(
        &shared, "get", "([II)I", false
    ));
    shared
        .natives
        .var_handle_exact_slot
        .store(1, std::sync::atomic::Ordering::Relaxed);
    if varhandle_thin_binds_admitted(&shared) {
        return; // `CRATONVM_JIT_VH_THIN_BIND_EXACT_GATE=0`
    }
    // Round 15 wave 6 (lane vh8): a judged ladder keeps binding after the
    // mint, so the protection stays with it.
    let judged = cratonvm_jit::varhandle_judged_binds_enabled();
    assert_eq!(
        varhandle_splice_keeps_thin_bind(&shared, "compareAndSet", cas, false),
        judged
    );
    assert_eq!(
        varhandle_splice_keeps_thin_bind(&shared, "get", read, false),
        judged
    );
    assert_eq!(
        varhandle_splice_keeps_thin_bind(&shared, "get", read, true),
        judged
            && !cratonvm_jit::varhandle_ir_read_shape("get", read)
            && !ir_bound(&shared, "get", read)
    );
}
