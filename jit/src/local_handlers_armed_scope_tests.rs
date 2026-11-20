use super::*;

#[test]
fn the_armed_flag_is_clear_inside_a_new_scope_and_after_it_ends() {
    note_local_handlers_armed(true);
    {
        let _scope = LocalHandlersArmedScope::enter();
        assert!(
            !local_handlers_were_armed(),
            "a stale flag from an earlier compile"
        );
        note_local_handlers_armed(true);
        assert!(local_handlers_were_armed());
    }
    assert!(
        !local_handlers_were_armed(),
        "a compile that bailed left the flag set"
    );
}

#[test]
fn an_unwinding_compile_clears_the_armed_flag() {
    let result = std::panic::catch_unwind(|| {
        let _scope = LocalHandlersArmedScope::enter();
        note_local_handlers_armed(true);
        panic!("compile unwound");
    });
    assert!(result.is_err());
    assert!(!local_handlers_were_armed());
}
