use super::*;

fn ret_body() -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(64).expect("an executable buffer");
    buf.emit(&[0xC3]); // ret
    CompiledMethod::new(buf)
}

/// Unknown is "no": a body nobody stamped keeps the caller's copy, and
/// every fact the rule names withdraws ownership on its own.
#[test]
fn only_a_handler_free_trap_free_plain_body_owns_its_arguments() {
    let mut cm = ret_body();
    assert!(!callee_body_owns_its_arguments(&cm), "unstamped");
    cm.method_declares_no_handlers = true;
    assert!(callee_body_owns_its_arguments(&cm), "stamped, nothing else");
    cm.has_frameless_trap_stub = true;
    assert!(
        !callee_body_owns_its_arguments(&cm),
        "a frameless trap stub"
    );
    cm.has_frameless_trap_stub = false;
    cm.has_indy_trap = true;
    assert!(!callee_body_owns_its_arguments(&cm), "an indy trap");
    cm.has_indy_trap = false;
    cm.requires_wrapped_entry = true;
    assert!(!callee_body_owns_its_arguments(&cm), "a wrapped entry");
    cm.requires_wrapped_entry = false;
    cm.self_locks_monitor = true;
    assert!(!callee_body_owns_its_arguments(&cm), "a self-locking body");
}

/// The entry question goes through the owner registry: an entry nothing
/// registers owns nothing, whatever the body at that address says.
#[test]
fn an_entry_owns_its_arguments_only_through_its_registered_body() {
    let mut cm = ret_body();
    cm.method_declares_no_handlers = true;
    let arc = Arc::new(cm);
    let entry = arc.entry_ptr() as usize;
    assert!(!direct_callee_owns_its_arguments(entry), "not registered");
    register_jit_entry_owner_for_adapter(entry, &arc);
    assert!(direct_callee_owns_its_arguments(entry), "registered");
    // Unregistered while the body is still alive, so no other test's
    // body can be handed this address in between.
    jit_entry_owners().lock().remove(&entry);
    assert!(
        !direct_callee_owns_its_arguments(entry),
        "unregistered again"
    );
    drop(arc);
}
