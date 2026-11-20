use super::*;

/// The reader is opt-in.
#[test]
fn zt31zc11_the_switch_is_opt_in() {
    let run = |v: Option<&str>| {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_REMAP_IN_MARK", v)],
            zgc_remap_in_mark_enabled,
        )
    };
    assert!(!run(None));
    assert!(run(Some("1")));
    assert!(!run(Some("0")));
}

/// With nothing kept, nothing forwards; the census counts.
#[test]
fn zt31zc11_no_kept_cycle_forwards_nothing() {
    let st = ZRemapInMarkState::new(true);
    assert!(!st.cycle_published());
    assert_eq!(st.forwarded(0x1000), None);
    st.note_healed(true);
    st.note_healed(false);
    st.note_skipped(3);
    st.note_refused(ZRemapInMarkRefusal::NoFlip);
    let r = st.report();
    assert_eq!((r.healed, r.forwarded, r.skipped, r.refused), (2, 1, 3, 1));
    assert_eq!(r.last_refusal, ZRemapInMarkRefusal::NoFlip as u8);
}
