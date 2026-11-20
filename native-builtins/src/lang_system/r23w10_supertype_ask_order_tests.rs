use super::supertype_ask_order;

#[test]
fn interfaces_are_asked_before_the_superclass() {
    let key = concat!("CRATON", "VM_DEFINE_INTERFACES_BEFORE_SUPER");
    for value in [None, Some("0"), Some("1")] {
        cratonvm_types::flags::with_thread_overrides(&[(key, value)], || {
            assert_eq!(supertype_ask_order(3, true), vec![1, 2, 0]);
            assert_eq!(supertype_ask_order(2, true), vec![1, 0]);
            assert_eq!(supertype_ask_order(1, true), vec![0], "only the superclass");
            assert_eq!(supertype_ask_order(2, false), vec![0, 1], "interfaces only");
            assert!(supertype_ask_order(0, false).is_empty());
        });
    }
}
