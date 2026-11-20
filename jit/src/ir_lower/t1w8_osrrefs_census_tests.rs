use super::*;

/// Only an entry with a reference the reference half excuses, and only
/// when every other value is excused too.
#[test]
fn t1w8_osrrefs_would_admit_key() {
    let k = Lowerer::osr_entry_would_admit_refs_key_by;
    assert_eq!(k(&[]), None, "nothing unnamed is no refusal of this kind");
    assert_eq!(k(&[(true, false)]), Some("would-admit:dataflow-live-in-refs"));
    assert_eq!(
        k(&[(true, false), (false, true)]),
        Some("would-admit:dataflow-live-in-refs+dataflow-live-in")
    );
    assert_eq!(k(&[(false, true)]), None, "the primitive half's own key");
    assert_eq!(k(&[(true, false), (false, false)]), None, "a value neither excuses");
}
