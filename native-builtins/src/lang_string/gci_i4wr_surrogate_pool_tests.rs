use super::{
    surrogate_intern_claim, surrogate_intern_forget_vm, surrogate_intern_probe,
    surrogate_intern_replace_dead,
};

/// A VM identity no real VM or other test uses (identities are addresses).
const VM: usize = 0x6c14_0057;

#[test]
fn gci_i4wr_a_dead_row_is_replaced_and_a_live_one_kept() {
    let units: Vec<u16> = vec![0xD800, 0x0077, 0x0072];
    assert_eq!(surrogate_intern_claim(VM, units.clone(), 11), 11, "first claim wins");
    assert_eq!(surrogate_intern_claim(VM, units.clone(), 13), 11, "insert-if-absent");
    // 11 died: the replace swaps it for 17.
    assert_eq!(surrogate_intern_replace_dead(VM, units.clone(), 11, 17), 17);
    assert_eq!(surrogate_intern_probe(VM, &units), Some(17));
    // A racer that also saw 11 dead loses to the row now there.
    assert_eq!(surrogate_intern_replace_dead(VM, units.clone(), 11, 19), 17);
    assert_eq!(surrogate_intern_probe(VM, &units), Some(17));
    // No row at all (forgotten meanwhile): the replace claims.
    surrogate_intern_forget_vm(VM);
    assert_eq!(surrogate_intern_replace_dead(VM, units.clone(), 17, 23), 23);
    assert_eq!(surrogate_intern_probe(VM, &units), Some(23));
    surrogate_intern_forget_vm(VM);
}

/// `intern_unrepresentable` roots weakly under a weak pool and replaces a
/// row whose handle no longer resolves (source pin: the mock context has
/// no global-root table to drive it).
#[test]
fn gci_i4wr_intern_unrepresentable_follows_the_pool_disposition() {
    let src = concat!(include_str!("../lang_string/mod.rs"), "\n", include_str!("../lang_string/decode_string_chars.rs"), "\n", include_str!("../lang_string/native_sb_get_chars.rs"), "\n", include_str!("../lang_string/exact_regex_replace_units.rs"), "\n", include_str!("../lang_string/fmt_temporal_fault_char.rs"), "\n", include_str!("../lang_string/bounds_off_count_violation.rs")).replace("\r\n", "\n");
    let start = src
        .find("\npub fn intern_unrepresentable(")
        .expect("intern_unrepresentable");
    let body = &src[start..];
    let body = &body[..body.find("\n}\n").expect("its end")];
    // gci i20/mr finding 4: weak on every collector (dev's gcD rule), not
    // only under a weak main pool.
    assert!(!body.contains("string_pool_is_weak"));
    assert!(body.contains("let mut handle = ctx.add_weak_global_root(this);"));
    // Both answers taken from a weak row get the keep-alive.
    assert_eq!(body.matches("ctx.gc_reference_keep_alive(").count(), 2);
    assert!(body.contains("surrogate_intern_replace_dead(vm, units, dead, handle)"));
    assert!(body.contains("ctx.remove_global_root(dead);"));
}
