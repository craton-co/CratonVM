use super::{mh_real_bound_member_kind as kind, MhRealBoundMember};

/// `Acc.plus(I)I` bound on its receiver: `(I)I` at the site.
#[test]
fn j27_virtual_bind_to_keeps_the_round_21_answer() {
    let got = kind(5, "p/Acc", "(I)I", "(Lp/Acc;I)I", "(I)I", "(I)I", false);
    assert_eq!(got, Some(MhRealBoundMember::Virtual));
    // Another `invokeExact` type is the door's WrongMethodTypeException.
    assert_eq!(kind(5, "p/Acc", "(I)I", "(Lp/Acc;I)I", "(I)I", "(I)J", true), None);
    // A stamped (asType) direct handle is not at its own type.
    assert_eq!(kind(5, "p/Acc", "(I)I", "(Ljava/lang/Object;I)I", "(I)I", "(I)I", true), None);
}

/// `static long add(Long, int)` bound on its `Long`: `(I)J` at the site.
#[test]
fn j27_static_bind_to_drops_the_leading_reference_parameter() {
    let desc = "(Ljava/lang/Long;I)J";
    let got = kind(6, "p/S", desc, desc, "(I)J", "(I)J", true);
    assert_eq!(
        got,
        Some(MhRealBoundMember::Static {
            leading: "java/lang/Long".to_string()
        })
    );
    let one = kind(6, "p/S", "(Lp/Box;)V", "(Lp/Box;)V", "()V", "()V", true);
    assert_eq!(
        one,
        Some(MhRealBoundMember::Static {
            leading: "p/Box".to_string()
        })
    );
}

#[test]
fn j27_static_bind_to_refuses_what_it_cannot_prove() {
    let desc = "(Ljava/lang/Long;I)J";
    // The switch off: the round-21 answer (no static bind).
    assert_eq!(kind(6, "p/S", desc, desc, "(I)J", "(I)J", false), None);
    // Site is not the bound type (WrongMethodTypeException is the door's).
    assert_eq!(kind(6, "p/S", desc, desc, "(I)J", "(Ljava/lang/Long;I)J", true), None);
    // The bound type disagrees with the member minus its leading parameter.
    assert_eq!(kind(6, "p/S", desc, desc, "(J)J", "(J)J", true), None);
    // The direct handle was stamped by asType.
    assert_eq!(kind(6, "p/S", desc, "(Ljava/lang/Object;I)J", "(I)J", "(I)J", true), None);
    // A primitive or array leading parameter.
    assert_eq!(kind(6, "p/S", "(II)J", "(II)J", "(I)J", "(I)J", true), None);
    assert_eq!(kind(6, "p/S", "([II)J", "([II)J", "(I)J", "(I)J", true), None);
    // No parameter at all, and an interface / special kind.
    assert_eq!(kind(6, "p/S", "()J", "()J", "()J", "()J", true), None);
    assert_eq!(kind(7, "p/S", desc, desc, "(I)J", "(I)J", true), None);
    assert_eq!(kind(9, "p/S", "(I)I", "(Lp/S;I)I", "(I)I", "(I)I", true), None);
}
