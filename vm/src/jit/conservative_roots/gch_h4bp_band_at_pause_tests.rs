//! gch h4/bp (`CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE`): the deposit scope
//! that leaves a JNI in-native thread's compiled band to the pause.
use super::*;

fn unwind_through_the_scope() {
    panic!("gch h4/bp: an unwind through the band-at-pause scope");
}

/// The scope nests and restores (an unwind included), an unarmed scope is
/// `f` alone, and inside an armed one the coverage proof answers
/// "unproven" without walking anything.
#[test]
fn gch_h4bp_band_at_pause_scope_nests_restores_and_proves_nothing() {
    assert!(!band_at_pause_deposit_armed(), "off by default");
    with_band_at_pause_deposit(false, || {
        assert!(!band_at_pause_deposit_armed(), "unarmed: f alone");
    });
    let proven = with_band_at_pause_deposit(true, || {
        assert!(band_at_pause_deposit_armed());
        with_band_at_pause_deposit(true, || assert!(band_at_pause_deposit_armed()));
        assert!(
            band_at_pause_deposit_armed(),
            "an inner scope restores the outer one"
        );
        with_band_at_pause_deposit(false, || {
            assert!(
                band_at_pause_deposit_armed(),
                "an unarmed inner scope changes nothing"
            );
        });
        refresh_moving_young_coverage_for_current_thread()
    });
    assert!(!proven, "the deposit's own proof answers unproven");
    assert!(!band_at_pause_deposit_armed(), "restored on exit");
    let caught =
        std::panic::catch_unwind(|| with_band_at_pause_deposit(true, unwind_through_the_scope));
    assert!(caught.is_err());
    assert!(!band_at_pause_deposit_armed(), "restored on unwind");
}

/// gci i24/bp (`CRATONVM_JNI_IN_NATIVE_PROVEN_DEPTH_CREDIT`): the proof
/// scope re-arms the coverage proof only inside a band-at-pause scope; there
/// the proof answers exactly what it answers outside any scope (it is not
/// forced to "unproven"); the scope nests and restores, an unwind included.
#[test]
fn gci_i24bp_the_proof_scope_rearms_the_proof_inside_a_band_scope() {
    assert!(!band_at_pause_proof_armed(), "off by default");
    with_band_at_pause_proof(true, || {
        assert!(
            !band_at_pause_proof_armed(),
            "a proof scope without a band scope arms nothing"
        );
    });
    let baseline = refresh_moving_young_coverage_for_current_thread();
    let (armed, proven) = with_band_at_pause_deposit(true, || {
        assert!(!band_at_pause_proof_armed(), "a band scope alone");
        with_band_at_pause_proof(true, || {
            with_band_at_pause_proof(false, || {
                assert!(
                    band_at_pause_proof_armed(),
                    "an unarmed inner scope changes nothing"
                );
            });
            (
                band_at_pause_proof_armed(),
                refresh_moving_young_coverage_for_current_thread(),
            )
        })
    });
    assert!(armed, "armed inside both scopes");
    assert_eq!(proven, baseline, "the proof runs as it does outside the scopes");
    assert!(!band_at_pause_proof_armed(), "restored on exit");
    assert!(!band_at_pause_deposit_armed(), "restored on exit");
    let caught = std::panic::catch_unwind(|| {
        with_band_at_pause_deposit(true, || {
            with_band_at_pause_proof(true, unwind_through_the_scope)
        })
    });
    assert!(caught.is_err());
    assert!(!band_at_pause_proof_armed(), "restored on unwind");
    assert!(
        !JIT_CHAIN_HISTORY.with(|c| c.get().band_at_pause_prove),
        "the proof bit itself restored on unwind"
    );
}

/// The band's top is this thread's stack top, and only for a low end on
/// this thread's stack.
#[test]
fn gch_h4bp_the_band_top_is_this_threads_stack_top() {
    let sp = current_stack_pointer();
    match in_native_band_top(sp) {
        Some(top) => {
            assert!(top > sp, "the top lies above the SP");
            assert_eq!(top, own_stack_top());
        }
        // A platform that cannot say: the band is never recorded.
        None => assert_eq!(own_stack_top(), 0),
    }
    assert_eq!(
        in_native_band_top(usize::MAX - 7),
        None,
        "not on this stack"
    );
}

/// gci i28/nb (P5/nb): the pure rule of the narrowed band top.
#[test]
fn gci_i28nb_the_chain_band_top_rule() {
    assert_eq!(chain_band_hi(0x1000, 0x2000, 0x9000), Some(0x2000));
    assert_eq!(
        chain_band_hi(0x1000, 0x2001, 0x9000),
        Some(0x2008),
        "rounded up to a word"
    );
    assert_eq!(
        chain_band_hi(0x2000, 0x2000, 0x9000),
        None,
        "the chain is not above the band's low end"
    );
    assert_eq!(chain_band_hi(0x3000, 0x2000, 0x9000), None);
    assert_eq!(
        chain_band_hi(0x1000, 0x9000, 0x9000),
        None,
        "nothing to narrow"
    );
    assert_eq!(
        chain_band_hi(0x1000, 0x8ffc, 0x9000),
        None,
        "rounds onto the top"
    );
    assert_eq!(
        chain_band_hi(0x1000, usize::MAX - 3, usize::MAX),
        None,
        "no overflow"
    );
}

/// gci i28/nb: the band narrows to the outermost entry on a verdict the
/// frozen-band memo holds for the chain state, or on a fresh probe whose
/// verdict it then records; never with an empty chain or a low end at the
/// entry; a chain mutation voids the memo's answer.
#[cfg(any(target_os = "windows", target_os = "linux"))]
#[test]
fn gci_i28nb_the_band_narrows_to_the_outermost_entry_on_a_clean_verdict() {
    if current_thread_jit_depth() != 0 {
        return;
    }
    let top = own_stack_top();
    if top == 0 {
        return;
    }
    let lo = current_stack_pointer() & !7usize;
    assert_eq!(in_native_band_chain_top(lo, top), None, "an empty chain");
    let cover = (current_stack_pointer() + 512) & !7usize;
    assert!(cover < top, "the entry lies below the stack top");
    let _ = push_jit_entry_at(cover);
    // A compilation on another test thread voids a verdict (the key holds
    // the code-range generation): a memo answer is asserted only while
    // the generation is the one it started at.
    let gen0 = cratonvm_jit::jit_code_ranges_generation();
    let moved = || cratonvm_jit::jit_code_ranges_generation() != gen0;
    with_frozen_band_memo(None, || note_frozen_band_clean(cover, top, true));
    let got = in_native_band_chain_top(lo, top);
    assert!(
        moved() || got == Some((cover, ChainBandProof::Memo)),
        "the memo's verdict narrows: {got:?}"
    );
    assert_eq!(
        in_native_band_chain_top(cover, top),
        None,
        "a low end at the entry: nothing to narrow"
    );
    // The chain touched: the memo's verdict is void and a fresh probe
    // decides (a JIT-looking word above the entry keeps the whole band).
    let _ = push_jit_entry_at(cover - 64);
    let _ = pop_jit_entry();
    match in_native_band_chain_top(lo, top) {
        Some((hi, ChainBandProof::Probed)) => {
            assert_eq!(hi, cover);
            let again = in_native_band_chain_top(lo, top);
            assert!(
                moved() || again == Some((cover, ChainBandProof::Memo)),
                "the probe's verdict is recorded: {again:?}"
            );
        }
        Some((_, ChainBandProof::Memo)) => panic!("a push and a pop void the memo"),
        None => {}
    }
    let _ = pop_jit_entry();
    assert_eq!(
        in_native_band_chain_top(lo, top),
        None,
        "an empty chain again"
    );
}
