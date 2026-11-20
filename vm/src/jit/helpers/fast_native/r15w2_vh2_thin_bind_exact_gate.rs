use super::*;
use crate::config::VmConfig;

/// The first exact mint unwires the three `VarHandle` tables of every
/// later compile's `direct_helper_table_for`, and nothing else.
#[test]
pub fn an_exact_mint_unwires_every_varhandle_thin_bind() {
    let shared = SharedVm::new(VmConfig::default());
    let open = direct_helper_table_for(&shared);
    // Every read slot VH2-3 leaves bindable is wired.
    assert!(open
        .varhandle_read
        .iter()
        .enumerate()
        .all(|(slot, &a)| (a != 0) == varhandle_read_slot_bindable(slot)));
    assert!(open
        .varhandle_write
        .iter()
        .chain(&open.varhandle_cas)
        .all(|&a| a != 0));
    shared
        .natives
        .var_handle_exact_slot
        .store(1, std::sync::atomic::Ordering::Relaxed);
    let closed = direct_helper_table_for(&shared);
    if cratonvm_jit::varhandle_judged_binds_enabled() {
        // Round 15 wave 6 (lane vh8): the ladder binds judged rows, which
        // need no withdrawal, so the table stays wired across the mint.
        assert_eq!(closed.varhandle_read, open.varhandle_read);
        assert_eq!(closed.varhandle_write, open.varhandle_write);
        assert_eq!(closed.varhandle_cas, open.varhandle_cas);
    } else {
        assert!(closed
            .varhandle_read
            .iter()
            .chain(&closed.varhandle_write)
            .chain(&closed.varhandle_cas)
            .all(|&a| a == 0));
    }
    assert_eq!(closed.integer_value_of, open.integer_value_of);
    assert_eq!(closed.static_base_resolver, open.static_base_resolver);
}

/// VH2-3: exactly the `REF_STRICT` read slots (a site declaring a class
/// other than `Object`) are left unbound while reference returns are cast.
#[test]
pub fn only_ref_strict_read_slots_are_withheld_for_the_cast() {
    if !cratonvm_native_builtins::lang_invoke::vh_reference_return_checkcast_enabled() {
        return; // an explicit `0` in the test environment wins
    }
    for slot in 0..cratonvm_jit::VARHANDLE_READ_SLOTS {
        assert_eq!(
            varhandle_read_slot_bindable(slot),
            cratonvm_jit::varhandle_read_slot_kind(slot)
                != cratonvm_jit::VARHANDLE_READ_KIND_REF_STRICT,
            "slot {slot}"
        );
    }
    // `None` only under the reference half's own kill switch.
    let strict = "(Ljava/lang/Object;)Ljava/lang/String;";
    if let Some(slot) = cratonvm_jit::varhandle_read_helper_slot("get", strict) {
        assert!(!varhandle_read_slot_bindable(slot));
    }
    let object = "(Ljava/lang/Object;)Ljava/lang/Object;";
    if let Some(slot) = cratonvm_jit::varhandle_read_helper_slot("get", object) {
        assert!(varhandle_read_slot_bindable(slot));
    }
}
