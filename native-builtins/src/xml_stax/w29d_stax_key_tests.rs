use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess, NativeInvokeAccess};

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: a key only, 8-byte aligned, never dereferenced.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn first_text(ctx: &dyn NativeContext, reader: ObjectRef) -> Option<String> {
    with_state(ctx, reader, |s| {
        s.events
            .iter()
            .find(|e| e.kind == CHARACTERS)
            .map(|e| e.text.clone())
    })
    .flatten()
}

/// Two live readers with one hash in ONE VM, and the same address in two
/// VMs, each replay their own document; a dead reader's state and a dead
/// `Location`'s row go with their lock keys.
#[test]
fn readers_never_replay_each_others_documents() {
    const VM: usize = 0x29D5_7A01;
    const OTHER_VM: usize = 0x29D5_7A02;
    let ctx = crate::test_utils::mock_ctx();
    ctx.set_vm_identity(VM);
    let other = crate::test_utils::mock_ctx();
    other.set_vm_identity(OTHER_VM);
    let (a_addr, b_addr) = (0x1_29D5_7A00usize, 0x2_29D5_7A00usize);
    let (a, b) = (obj(a_addr), obj(b_addr));
    assert_eq!(
        ctx.identity_hash_code(a),
        ctx.identity_hash_code(b),
        "premise: the two readers share an identity hash"
    );

    store_state(&ctx, a, make_reader_state(b"<r>alpha</r>"));
    store_state(&ctx, b, make_reader_state(b"<r>beta</r>"));
    assert_eq!(first_text(&ctx, a).as_deref(), Some("alpha"));
    assert_eq!(first_text(&ctx, b).as_deref(), Some("beta"));
    assert!(
        require_state(&other, a).is_err(),
        "another VM's reader at the same address has no state"
    );

    let ka = existing_obj_key(&ctx, a).expect("a was keyed");
    crate::gc_sweep_lock_keys(VM, &|addr| addr == b_addr);
    assert!(
        !reader_table().lock().contains_key(&ka),
        "a reader dropped unclosed takes its parsed document with it"
    );
    assert_eq!(first_text(&ctx, b).as_deref(), Some("beta"));

    crate::forget_vm_lock_keys(VM);
    crate::forget_vm_lock_keys(OTHER_VM);
    assert!(
        require_state(&ctx, b).is_err(),
        "VM teardown drops the rest"
    );
}
