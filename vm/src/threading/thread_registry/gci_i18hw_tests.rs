use super::*;

fn src(rel: &str) -> String {
    let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// The text of `fn <sig>` up to its closing 4-space-indented brace line.
fn method_body(text: &str, sig: &str) -> String {
    let start = text.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &text[start..];
    let end = rest
        .find("\n    \u{7d}\n")
        .unwrap_or_else(|| panic!("end of `{sig}` not found"));
    rest[..end].to_string()
}

#[test]
fn gci_i18hw_only_a_continuation_on_g1_with_the_switch_deposits_precisely() {
    assert!(continuation_deposit_is_precise(true, true, true));
    // Generational / ZGC keep the probes (non-moving JIT-active sweep on Gen).
    assert!(!continuation_deposit_is_precise(false, true, true));
    // A platform thread may hold a lost-tag local from a compiled callee.
    assert!(!continuation_deposit_is_precise(true, false, true));
    // `CRATONVM_G1_VT_PRECISE_DEPOSIT=0`: the historical deposit.
    assert!(!continuation_deposit_is_precise(true, true, false));
    assert!(!continuation_deposit_is_precise(false, false, false));
}

#[test]
fn gci_i18hw_the_switch_is_read_once_and_defaults_on() {
    // The field is the flag's default-on reading (ON unless set to an
    // off word, which nothing in the test environment does).
    assert_eq!(
        ThreadRegistry::new().g1_vt_precise_deposit(),
        true
    );
}

/// The blocking deposit (the yield's `deposit_root_snapshot`) consults the
/// rule before it records `deposit_conservative` and before any probe, so
/// the flag the mirror-keyed gather and the guards read agrees with the
/// words deposited.
#[test]
fn gci_i18hw_the_blocking_deposit_applies_the_rule_before_its_probes() {
    let exec = src("src/vm/vm_exec/thread_interrupted_field_slot_of.rs");
    let deposit = method_body(&exec, "fn deposit_root_snapshot_inner_with(");
    let rule = deposit
        .find("continuation_deposit_is_precise(")
        .expect("the deposit asks continuation_deposit_is_precise");
    let flag = deposit
        .find(".deposit_conservative")
        .expect("the deposit records deposit_conservative");
    let all_live = deposit
        .find("let all_live_locals")
        .expect("the dead-locals rule");
    let probe = deposit
        .find("scan_locals_conservative(")
        .expect("the conservative locals probe");
    assert!(rule < flag && rule < all_live && rule < probe);
    // gci i19/vd: the continuation input is now
    // `continuation_is_interpreted_only` (kind Virtual AND never entered
    // compiled code), not the bare `ThreadKind::Virtual` match.
    for term in [
        "conservative_locals_enabled()",
        "is_g1()",
        "continuation_is_interpreted_only(self.thread)",
        "g1_vt_precise_deposit()",
    ] {
        assert!(
            deposit[..flag].contains(term),
            "the rule lost its `{term}` input"
        );
    }
}
