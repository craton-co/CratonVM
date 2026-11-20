use super::*;

fn src(rel: &str) -> String {
    std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel))
        .unwrap_or_else(|e| panic!("{rel}: {e}"))
}

fn body(src: &str, sig: &str, end: &str) -> String {
    let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &src[start..];
    let stop = rest
        .find(end)
        .unwrap_or_else(|| panic!("end of `{sig}` not found"));
    rest[..stop].to_string()
}

#[test]
fn the_wake_switch_never_acts_without_the_lazy_deposit() {
    assert!(!lazy_blocked_jit_wake_enabled() || lazy_blocked_jit_deposit_enabled());
}

/// The wake leaves the chain out only for an unfolded window whose entry
/// deferred it, and only the no-flag deposit ever skips.
#[test]
fn the_wake_skips_the_chain_only_for_an_unfolded_deferred_window() {
    let exec = src("src/vm/vm_exec/thread_interrupted_field_slot_of.rs");
    let wake = body(&exec, "fn check_post_block_gc_inner(", "\n    }\n");
    let at = wake
        .find("let lazy_wake = had_deferred_chain")
        .expect("the lazy wake is gated on the entry's deferred chain");
    let gate = &wake[at..at + wake[at..].find(';').expect("gate end")];
    for term in [
        "folds_k == 0",
        "fixup.is_empty()",
        "lazy_blocked_jit_wake_enabled()",
    ] {
        assert!(gate.contains(term), "the lazy wake gate lost `{term}`");
    }
    assert!(wake.contains("deposit_root_snapshot_no_flag_with(lazy_wake)"));
    let deposit = body(&exec, "fn deposit_root_snapshot_inner_with(", "\n    }\n");
    assert!(deposit.contains("if skip_chain && !raise_blocked_flag {"));
}

/// Every root-gathering exit of the take-over resolves the deferred
/// chains, the early one without the take-over (`CRATONVM_XT_JIT_ROOT_SCAN=0`)
/// included.
#[test]
fn every_root_gathering_pause_resolves_the_deferred_chains() {
    let alloc = src("src/runtime/interpreter/gc_and_alloc/mtroots_on.rs");
    let b = body(&alloc, "pub(in crate::runtime::interpreter) fn stw_take_over_and_wait(", "\n}\n");
    let early = b
        .find("return xt::TakenOver::default();")
        .expect("the no-take-over exit");
    let first = b
        .find("resolve_deferred_blocked_jit_chains(")
        .expect("a resolution");
    assert!(first < early, "the no-take-over exit skips the resolution");
    assert!(
        b[early..].contains("resolve_deferred_blocked_jit_chains("),
        "the take-over exit lost its resolution"
    );
}
