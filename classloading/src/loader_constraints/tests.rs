use super::*;

/// JIT round 23 wave 8f: the parent-text memo answers only the same
/// address under the same epoch, and keeps one entry.
#[test]
fn the_parent_text_memo_answers_one_address_per_epoch() {
    let mut c = LoaderConstraints::new();
    assert_eq!(c.parent_text(0x1000, 3), None);
    c.remember_parent_text(0x1000, 3, "'app'");
    assert_eq!(c.parent_text(0x1000, 3), Some("'app'"));
    assert_eq!(c.parent_text(0x1000, 4), None, "a moved epoch misses");
    assert_eq!(c.parent_text(0x2000, 3), None, "another address misses");
    c.remember_parent_text(0x2000, 4, "'p' @1");
    assert_eq!(c.parent_text(0x1000, 3), None, "one entry");
    assert_eq!(c.parent_text(0x2000, 4), Some("'p' @1"));
    assert!(c.is_empty(), "the memo records no constraint");
}

/// `forget` drops exactly a dead loader's rows and the pins naming dead
/// classes; survivors keep their groups and answers (gc-common w9-e).
#[test]
fn forget_drops_a_dead_loaders_rows_and_dead_pins_only() {
    let mut c = LoaderConstraints::new();
    assert!(c.pin("Foo", 1, 100).is_none());
    assert!(c.impose("Foo", 1, 2).is_none());
    assert!(c.impose("Foo", 2, 3).is_none());
    assert!(c.pin("Bar", 3, 300).is_none());
    let pairs = c.tracked_pairs();
    assert_eq!(pairs, 4, "(Foo,1) (Foo,2) (Foo,3) (Bar,3)");
    // Loader 1 dies; nothing it pinned is unloaded yet.
    assert_eq!(c.forget(&|l| l == 1, &|_| false), 1);
    assert_eq!(c.tracked_pairs(), pairs - 1);
    assert_eq!(
        c.required_class("Foo", 2),
        Some(100),
        "the group keeps its pin"
    );
    assert_eq!(c.required_class("Foo", 3), Some(100));
    assert_eq!(c.required_class("Bar", 3), Some(300));
    // Class 100 is unloaded: the surviving group loses only that pin, and
    // a new class id pinned there is not a violation.
    assert_eq!(c.forget(&|_| false, &|k| k == 100), 0);
    assert_eq!(c.required_class("Foo", 2), None);
    let before = c.violation_count();
    assert!(c.pin("Foo", 2, 101).is_none());
    assert_eq!(c.violation_count(), before);
    assert_eq!(
        c.required_class("Foo", 3),
        Some(101),
        "2 and 3 are still one group"
    );
}

/// The define path's two calls (interpreter round i1 wave 37, lane L5):
/// nothing for an unconstrained `(name, loader)`, and no row grown for it;
/// the pin a constraint already carries, and a violation that leaves it.
#[test]
fn the_define_path_sees_only_constrained_rows() {
    let mut c = LoaderConstraints::new();
    assert!(c.is_empty());
    assert_eq!(c.pinned_for("S", 1), None);
    assert!(c.pin_if_constrained("S", 1, 10).is_none());
    assert!(c.is_empty(), "an unconstrained load adds no row");
    assert!(c.impose("S", 1, 2).is_none());
    assert!(c.pin_if_constrained("S", 1, 10).is_none());
    assert_eq!(c.pinned_for("S", 2), Some(10));
    assert_eq!(c.pinned_for("S", 3), None);
    let v = c.pin_if_constrained("S", 2, 20).expect("violation");
    assert_eq!((v.existing, v.conflicting), (10, 20));
    assert_eq!(c.pinned_for("S", 1), Some(10), "the pin stays");
    assert!(c.pin_if_constrained("T", 2, 20).is_none());
    assert_eq!(c.tracked_pairs(), 2);
    c.set_label(
        2,
        LoaderLabel {
            name_and_id: "'b' @1".into(),
            parent: Some("'app'".into()),
        },
    );
    assert_eq!(c.label(2).map(|l| l.name_and_id.as_str()), Some("'b' @1"));
    c.forget(&|l| l == 2, &|_| false);
    assert!(
        c.label(2).is_none(),
        "a dead loader's label goes with its rows"
    );
    assert_eq!(c.tracked_pairs(), 1);
}

#[test]
fn agreeing_loaders_are_not_a_violation() {
    let mut c = LoaderConstraints::new();
    assert!(c.pin("Foo", 1, 100).is_none());
    assert!(c.pin("Foo", 2, 100).is_none());
    assert!(c.impose("Foo", 1, 2).is_none());
    assert_eq!(c.violation_count(), 0);
}

#[test]
fn two_loaders_with_their_own_foo_violate_an_imposed_constraint() {
    let mut c = LoaderConstraints::new();
    // Each loader legitimately defines its own Foo. That alone is legal —
    // distinct namespaces — and must NOT be a violation on its own.
    assert!(c.pin("Foo", 1, 100).is_none());
    assert!(c.pin("Foo", 2, 200).is_none());
    assert_eq!(c.violation_count(), 0, "two namespaces alone are legal");

    // It becomes a violation only once something requires them to agree.
    let v = c.impose("Foo", 1, 2).expect("must violate");
    assert_eq!(v.name, "Foo");
    assert_eq!((v.existing, v.conflicting), (100, 200));
    assert_eq!(c.violation_count(), 1);
    assert!(v.message().contains("5.3.4"));
}

#[test]
fn a_constraint_imposed_before_either_side_loads_is_caught_on_pin() {
    // Resolution order is not fixed, so the check has to work from both
    // directions. This is the direction `impose` alone would miss.
    let mut c = LoaderConstraints::new();
    assert!(c.impose("Bar", 1, 2).is_none(), "nothing pinned yet");
    assert!(c.pin("Bar", 1, 100).is_none());
    let v = c
        .pin("Bar", 2, 200)
        .expect("must violate on the second pin");
    assert_eq!((v.existing, v.conflicting), (100, 200));
}

#[test]
fn constraints_are_transitive() {
    let mut c = LoaderConstraints::new();
    c.impose("Baz", 1, 2);
    c.impose("Baz", 2, 3);
    assert!(c.pin("Baz", 1, 100).is_none());
    // Loader 3 was never named alongside 1 directly.
    let v = c
        .pin("Baz", 3, 300)
        .expect("transitive constraint must bind");
    assert_eq!((v.existing, v.conflicting), (100, 300));
}

#[test]
fn names_do_not_bleed_into_each_other() {
    let mut c = LoaderConstraints::new();
    c.impose("A", 1, 2);
    assert!(c.pin("A", 1, 100).is_none());
    // A constraint on A says nothing about B.
    assert!(c.pin("B", 2, 999).is_none());
    assert_eq!(c.violation_count(), 0);
}

#[test]
fn required_class_answers_only_when_determined() {
    let mut c = LoaderConstraints::new();
    assert_eq!(c.required_class("Q", 1), None, "unconstrained");
    c.impose("Q", 1, 2);
    c.pin("Q", 2, 42);
    assert_eq!(
        c.required_class("Q", 1),
        Some(42),
        "loader 1 is bound by loader 2's resolution"
    );
}

#[test]
fn the_retained_violation_list_is_bounded_but_the_count_is_not() {
    let mut c = LoaderConstraints::new();
    for i in 0..(MAX_RECORDED as u32 + 10) {
        let name = format!("N{i}");
        c.pin(&name, 1, 1);
        c.pin(&name, 2, 2);
        c.impose(&name, 1, 2);
    }
    assert_eq!(c.violations().len(), MAX_RECORDED);
    assert_eq!(c.violation_count(), MAX_RECORDED as u64 + 10);
}
