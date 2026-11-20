use super::*;

/// A concrete class is never a row; an interface or an abstract class is
/// recorded once, by name, however often it is minted (nio21 wave 11,
/// lane L1: the census is what `Nio21ConcreteReceivers`' grep reads).
#[test]
fn concrete_is_never_recorded_and_each_violation_once() {
    assert!(!observe_uninstantiable_receiver(
        "nio21w11/Concrete",
        0x0001
    ));
    assert!(observe_uninstantiable_receiver(
        "nio21w11/Iface",
        ACC_INTERFACE | ACC_ABSTRACT
    ));
    assert!(!observe_uninstantiable_receiver(
        "nio21w11/Iface",
        ACC_INTERFACE | ACC_ABSTRACT
    ));
    assert!(observe_uninstantiable_receiver(
        "nio21w11/Abs",
        ACC_ABSTRACT
    ));
    let rows = SEEN.get().expect("recorded").lock().clone();
    assert!(!rows.iter().any(|(n, _, _)| n == "nio21w11/Concrete"));
    let kinds: Vec<&str> = rows
        .iter()
        .filter(|(n, _, _)| n.starts_with("nio21w11/"))
        .map(|(_, k, _)| *k)
        .collect();
    assert_eq!(kinds, ["interface", "abstract"]);
    // The requester is this file: `#[track_caller]` names the asker.
    assert!(rows
        .iter()
        .filter(|(n, _, _)| n.starts_with("nio21w11/"))
        .all(|(_, _, r)| r.contains("observe_tests.rs")));
}
