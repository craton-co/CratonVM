use super::{JitVerdictRegistry, SELF_LOCK_REFUSED_PSEUDO_ENTRY_PC as PSEUDO};

const CLASS: &str = "p/SyncBox";
const NAME: &str = "get";
const DESC: &str = "(Ljava/lang/Object;)Ljava/lang/Object;";

fn id() -> cratonvm_types::ClassId {
    cratonvm_types::ClassId::new(41)
}

fn memoized(vm: &JitVerdictRegistry, pc: usize) -> bool {
    vm.is_osr_entry_rejected(id(), CLASS, NAME, DESC, pc)
}

fn note(vm: &JitVerdictRegistry) {
    vm.record_osr_entry_reject(id(), CLASS, NAME, DESC, PSEUDO, true);
}

/// The memo round trip on one registry: recorded under the pseudo entry,
/// visible there only (no real OSR entry is rejected by it), per VM,
/// expired by a code-cache flush and forgotten with the class.
#[test]
fn a_self_lock_refusal_is_remembered_per_vm_and_per_class() {
    let vm_a = JitVerdictRegistry::default();
    let vm_b = JitVerdictRegistry::default();
    assert!(!memoized(&vm_a, PSEUDO));
    note(&vm_a);
    assert!(memoized(&vm_a, PSEUDO));
    assert!(!memoized(&vm_a, 0), "no real OSR entry is rejected");
    assert!(!memoized(&vm_b, PSEUDO), "per VM");
    vm_a.note_code_cache_flush();
    assert!(
        !memoized(&vm_a, PSEUDO),
        "a code-cache flush offers the self-locking body again"
    );
    note(&vm_a);
    assert!(memoized(&vm_a, PSEUDO));
    vm_a.forget_for_class(id(), CLASS);
    assert!(!memoized(&vm_a, PSEUDO), "forgotten with the class");
}
