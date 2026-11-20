use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: a key only; the registry and the mock hash the address and
    // never dereference it (8-byte aligned, lesson o).
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn row(text: &str) -> std::sync::Arc<MatcherRealCache> {
    let (byte_to_utf16, utf16_to_byte) = matcher_realjdk_build_offset_tables(text);
    std::sync::Arc::new(MatcherRealCache {
        ascii_run: None,
        text_identity: 1,
        pattern_identity: 2,
        capture_count: 1,
        utf8: std::sync::Arc::from(text.to_string().into_boxed_str()),
        byte_to_utf16: std::sync::Arc::from(byte_to_utf16.into_boxed_slice()),
        utf16_to_byte: std::sync::Arc::from(utf16_to_byte.into_boxed_slice()),
        re: compile_java_regex("a", 0).expect("trivial pattern"),
    })
}

fn cached_text(key: usize) -> Option<String> {
    matcher_realjdk_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .map(|(_, entry)| entry.utf8.to_string())
}

/// Any stamp: these tests look rows up by key only.
fn table_row(text: &str) -> MatcherRealRow {
    let stamp = MatcherRealRowStamp {
        text_addr: 0x2900_D800,
        pattern_addr: 0x2900_D900,
        epoch: 0,
        mod_count: 0,
    };
    (stamp, row(text))
}

/// Two live Matchers with ONE identity hash get two rows, a dead one's row
/// goes with its lock key, and VM teardown drops the rest.
#[test]
fn rows_are_per_matcher_and_go_with_their_lock_keys() {
    const VM: usize = 0x2900_D001;
    let ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(VM);
    let (a_addr, b_addr) = (0x1_2900_D100usize, 0x2_2900_D100usize);
    let (a, b) = (obj(a_addr), obj(b_addr));
    assert_eq!(
        ctx.identity_hash_code(a),
        ctx.identity_hash_code(b),
        "premise: the two Matchers share an identity hash"
    );
    let ka = crate::gc_stable_weak_lock_key(&ctx, a).unwrap();
    let kb = crate::gc_stable_weak_lock_key(&ctx, b).unwrap();
    assert_ne!(ka, kb, "each Matcher has its own key");
    {
        let mut guard = matcher_realjdk_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.insert(ka, table_row("text of a"));
        guard.insert(kb, table_row("text of b"));
    }
    assert_eq!(cached_text(ka).as_deref(), Some("text of a"));
    assert_eq!(cached_text(kb).as_deref(), Some("text of b"));
    assert_eq!(crate::existing_weak_lock_key(&ctx, a), Some(ka));

    // `a` dies: the lock-key sweep frees its key and the hook drops its row.
    crate::gc_sweep_lock_keys(VM, &|addr| addr == b_addr);
    assert_eq!(
        cached_text(ka),
        None,
        "a dead Matcher's decoded text is freed"
    );
    assert_eq!(cached_text(kb).as_deref(), Some("text of b"));

    crate::forget_vm_lock_keys(VM);
    assert_eq!(cached_text(kb), None, "VM teardown drops the rest");
}

/// The same address and hash in two VMs are two keys: VM B never finds a
/// row VM A filed.
#[test]
fn two_vms_never_share_a_row() {
    const VM_A: usize = 0x2900_D0A1;
    const VM_B: usize = 0x2900_D0B1;
    let a = crate::test_utils::mock_ctx();
    a.set_vm_identity(VM_A);
    let b = crate::test_utils::mock_ctx();
    b.set_vm_identity(VM_B);
    let m = obj(0x3_2900_D200);
    let ka = crate::gc_stable_weak_lock_key(&a, m).unwrap();
    assert_eq!(
        crate::existing_weak_lock_key(&b, m),
        None,
        "VM B has no key (so no row) of VM A's"
    );
    let kb = crate::gc_stable_weak_lock_key(&b, m).unwrap();
    assert_ne!(ka, kb);
    crate::forget_vm_lock_keys(VM_A);
    crate::forget_vm_lock_keys(VM_B);
}

/// The address-validated thread-local memos are dropped when the stamp
/// (VM identity, collection count) changes, and kept while it holds.
#[test]
fn memos_are_dropped_when_another_vm_runs_on_the_thread() {
    let first = crate::test_utils::mock_ctx();
    first.set_vm_identity(0x2900_D0C1);
    let second = crate::test_utils::mock_ctx();
    second.set_vm_identity(0x2900_D0C2);
    let state = MatcherRealState {
        matcher_raw: 0x2900_D300,
        groups_raw: 0x2900_D400,
        mod_count: 3,
        first: 0,
        last: 1,
        from: 0,
        to: 4,
    };

    matcher_real_memo_sync(&first);
    MATCHER_REAL_LAST_STATE.with(|s| s.set(Some(state)));
    MATCHER_REAL_LAST_CACHE.with(|c| {
        c.borrow_mut()
            .replace((0x2900_D300, 0x2900_D500, 0x2900_D600, row("memo")));
    });
    matcher_real_memo_sync(&first);
    assert!(
        MATCHER_REAL_LAST_STATE.with(|s| s.get()).is_some(),
        "the same stamp keeps the memo"
    );
    assert!(MATCHER_REAL_LAST_CACHE.with(|c| c.borrow().is_some()));

    matcher_real_memo_sync(&second);
    assert!(
        MATCHER_REAL_LAST_STATE.with(|s| s.get()).is_none(),
        "another VM on this thread drops it"
    );
    assert!(MATCHER_REAL_LAST_CACHE.with(|c| c.borrow().is_none()));
}
