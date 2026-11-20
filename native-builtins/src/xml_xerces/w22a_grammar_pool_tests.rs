use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// Private VM identities: no other test uses them.
const W22A_VM_A: usize = 0x0A22_4A08;
const W22A_VM_B: usize = 0x0A22_4B08;

struct Forget;
impl Drop for Forget {
    fn drop(&mut self) {
        forget_vm_spring_xml_grammar_pool(W22A_VM_A);
        forget_vm_spring_xml_grammar_pool(W22A_VM_B);
    }
}

fn row(vm: usize) -> Option<usize> {
    spring_xml_grammar_pool_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&vm)
        .copied()
}

/// gc-common w22-a (`common-w21a-more-process-wide-global-root-handle-caches`
/// item 3): the shared grammar pool is one rooted handle per VM, never a
/// raw address. A handle that no longer resolves is a miss (rebuilt), not
/// an answer from a pre-collection fallback; VM B's pool is its own; VM
/// A's teardown drops A's row only.
#[test]
fn w22a_grammar_pool_is_rooted_per_vm_and_forgotten_with_the_vm() {
    let _forget = Forget;
    let mut a = crate::test_utils::mock_ctx();
    a.set_vm_identity(W22A_VM_A);
    let mut b = crate::test_utils::mock_ctx();
    b.set_vm_identity(W22A_VM_B);
    // B's global-ref slot 1 holds an unrelated object: the index A's pool
    // root gets in A's table.
    let unrelated_b = b.fresh_object_ref();
    let unrelated_handle = b.add_global_root(unrelated_b);

    let pool_a = spring_xml_shared_grammar_pool(&mut a).expect("A's pool");
    let handle_a = row(W22A_VM_A).expect("A's row is filed");
    assert_eq!(a.resolve_global_root(handle_a), Some(pool_a));
    assert_eq!(
        spring_xml_shared_grammar_pool(&mut a),
        Some(pool_a),
        "memoised"
    );

    let pool_b = spring_xml_shared_grammar_pool(&mut b).expect("B's pool");
    assert_ne!(
        pool_b, unrelated_b,
        "B must not resolve A's handle in B's table"
    );
    assert_eq!(b.resolve_global_root(unrelated_handle), Some(unrelated_b));

    // A's root is gone (as after a table reset): the row is a miss and is
    // replaced -- the old code answered the raw fallback address here.
    assert!(a.remove_global_root(handle_a));
    let rebuilt = spring_xml_shared_grammar_pool(&mut a).expect("rebuilt pool");
    assert_ne!(rebuilt, pool_a, "a dead row is rebuilt, not answered raw");
    let new_handle = row(W22A_VM_A).expect("A's row is re-filed");
    assert_eq!(a.resolve_global_root(new_handle), Some(rebuilt));

    forget_vm_spring_xml_grammar_pool(W22A_VM_A);
    assert_eq!(row(W22A_VM_A), None);
    assert!(row(W22A_VM_B).is_some(), "B's row survives A's teardown");
}
