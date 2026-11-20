use super::*;

fn src(rel: &str) -> String {
    let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// The text of the top-level `fn` at `sig` up to its closing column-0
/// brace line.
fn fn_body(text: &str, sig: &str) -> String {
    let start = text.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &text[start..];
    let end = rest
        .find("\n\u{7d}\n")
        .unwrap_or_else(|| panic!("end of `{sig}` not found"));
    rest[..end].to_string()
}

#[test]
fn gci_i19vd_only_a_continuation_that_never_ran_compiled_code_is_precise() {
    let mut thread = crate::threading::JvmThread::new(ThreadId(19), "gci-i19vd");
    // A platform thread may hold a lost-tag local from a compiled callee.
    assert!(!continuation_is_interpreted_only(&thread));
    thread.kind = crate::threading::ThreadKind::Virtual;
    // A fresh continuation: the flag starts down.
    assert!(!thread
        .gc_block_state
        .continuation_entered_compiled
        .load(Ordering::Relaxed));
    assert!(continuation_is_interpreted_only(&thread));
    assert!(continuation_deposit_is_precise(
        true,
        continuation_is_interpreted_only(&thread),
        true
    ));
    // A door let it into compiled code: conservative from now on, on G1
    // with the switch on too.
    thread
        .gc_block_state
        .continuation_entered_compiled
        .store(true, Ordering::Relaxed);
    assert!(!continuation_is_interpreted_only(&thread));
    assert!(!continuation_deposit_is_precise(
        true,
        continuation_is_interpreted_only(&thread),
        true
    ));
}

/// The one check both interpreter->compiled entries pass records the
/// breach before its debug assertion, outside any `cfg`, so a release
/// build that a door let through still deposits conservatively.
#[test]
fn gci_i19vd_the_compiled_entry_check_records_the_breach_in_every_build() {
    let bridge = src("src/runtime/interpreter/jit_bridge/mint_ldc_slots_at_offer.rs");
    let fun = fn_body(
        &bridge,
        "\npub(in crate::runtime::interpreter) fn assert_no_compiled_continuation(",
    );
    let gate = fun
        .find("if thread_is_continuation(thread)")
        .expect("the record is gated on the thread kind");
    let store = fun
        .find(".continuation_entered_compiled")
        .expect("the check records the breach");
    let check = fun.find("debug_assert!(").expect("the debug assertion stays");
    assert!(gate < store && store < check);
    assert!(!fun.contains("cfg(debug_assertions)"));
    assert!(fun.contains(".store(true,"));
}
