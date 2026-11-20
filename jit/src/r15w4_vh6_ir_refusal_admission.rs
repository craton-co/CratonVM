use super::*;

fn wired() -> DirectHelperTable {
    DirectHelperTable {
        varhandle_read: [1; VARHANDLE_READ_SLOTS],
        varhandle_write: [1; VARHANDLE_WRITE_SLOTS],
        varhandle_cas: [1; VARHANDLE_CAS_SLOTS],
        ..DirectHelperTable::EMPTY
    }
}

/// F1: the refusal protects exactly the binds the single-pass ladder makes.
#[test]
fn the_ir_refusal_follows_the_single_pass_admission() {
    if !direct_jit_callee_calls_enabled()
        || !varhandle_write_direct_helpers_enabled()
    {
        return; // an explicit `0` in the test environment wins
    }
    let (name, desc) = ("set", "(Ljava/lang/Object;I)V");
    if varhandle_write_helper_slot(name, desc).is_none() {
        return;
    }
    let table = wired();
    let admits_fn = |_: &str, _: &str, _: &str| true;
    let refuses_fn = |_: &str, _: &str, _: &str| false;
    let admits: &dyn Fn(&str, &str, &str) -> bool = &admits_fn;
    let refuses: &dyn Fn(&str, &str, &str) -> bool = &refuses_fn;
    // `--compatible`: the wired slot binds, whatever the admission says.
    assert!(varhandle_ir_refusal_protects_bind(
        name, desc, &table, false, None
    ));
    // `--real-jdk`: only when the registry admits the bind.
    assert!(varhandle_ir_refusal_protects_bind(
        name,
        desc,
        &table,
        true,
        Some(admits)
    ));
    assert!(!varhandle_ir_refusal_protects_bind(
        name,
        desc,
        &table,
        true,
        Some(refuses)
    ));
    assert!(!varhandle_ir_refusal_protects_bind(
        name, desc, &table, true, None
    ));
    // An unwired table (after the first exact mint) protects nothing.
    let empty = DirectHelperTable::EMPTY;
    assert!(!varhandle_ir_refusal_protects_bind(
        name, desc, &empty, false, None
    ));
    // Not a thin-bindable shape.
    assert!(!varhandle_ir_refusal_protects_bind(
        "get", "([II)I", &table, false, None
    ));
}
