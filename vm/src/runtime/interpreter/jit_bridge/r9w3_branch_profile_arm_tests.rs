use super::arm_method_branch_profile;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

fn entry(code_len: usize) -> cratonvm_jit_api::CachedBytecodeMethod {
    cratonvm_jit_api::CachedBytecodeMethod::from_parts(cratonvm_jit_api::CachedMethodParts {
        declaring_class_id: cratonvm_types::ClassId::new(77),
        class_name: Arc::from("r9w3/Armed"),
        method_name: Arc::from("hot"),
        method_descriptor: Arc::from("(I)I"),
        source_file: None,
        code: Arc::from(vec![0u8; code_len].as_slice()),
        exception_table: Arc::from(vec![].as_slice()),
        max_stack: 2,
        max_locals: 1,
        num_params: 1,
        is_synchronized: false,
        is_static: true,
    })
}

/// The nomination door's arm sets the entry's flag AND arms the method's
/// own counters, sized with the length the interpreter's recorder uses,
/// and a second call is a no-op.
#[test]
fn arming_sets_the_entry_flag_and_the_methods_counters_once() {
    let store = crate::jit::profile::ProfileStore::new();
    let cached = entry(16);
    assert!(!cached.branch_profile_armed.load(Relaxed));
    assert!(arm_method_branch_profile(&store, &cached));
    assert!(cached.branch_profile_armed.load(Relaxed));
    // The same handle the interpreter's recorder gets for this frame.
    let counters = store.branch_counters_borrowed(
        cached.declaring_class_id.as_u32(),
        &cached.method_name,
        &cached.method_descriptor,
        super::cached_method_code_len(&cached),
    );
    assert!(counters.is_armed());
    // The entry's code is padded like `frame::padded_bytecode`'s: the
    // counters are sized to the 14 real bytes, the length a replay checks.
    assert_eq!(counters.code_len(), 14);
    // Sticky and idempotent: the second call touches nothing.
    assert!(!arm_method_branch_profile(&store, &cached));
    assert!(cached.branch_profile_armed.load(Relaxed));
}

/// A clone of an armed entry is armed: it names the same method.
#[test]
fn a_clone_of_an_armed_entry_stays_armed() {
    let store = crate::jit::profile::ProfileStore::new();
    let cached = entry(8);
    assert!(arm_method_branch_profile(&store, &cached));
    let copy = cached.clone();
    assert!(copy.branch_profile_armed.load(Relaxed));
    assert!(!entry(8).branch_profile_armed.load(Relaxed));
}
