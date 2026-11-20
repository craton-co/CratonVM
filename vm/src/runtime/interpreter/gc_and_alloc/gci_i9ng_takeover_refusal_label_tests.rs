use super::*;

#[test]
fn gci_i9ng_takeover_refusal_names_the_takeover_or_the_helper_windows() {
    use cratonvm_gc::gc_quiescence::incomplete_reason as r;
    assert_eq!(takeover_refusal_label(1), r::XT_TAKEOVER);
    assert_eq!(takeover_refusal_label(0), r::XT_HELPER_WINDOW);
    assert_ne!(r::label(takeover_refusal_label(0)), "none");
    assert_ne!(r::label(takeover_refusal_label(3)), "none");
}
