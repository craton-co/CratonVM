use super::*;
use crate::vtype::VType;
use std::sync::Arc;

struct MockHierarchy;

impl ClassHierarchy for MockHierarchy {
    fn is_subclass(&self, child: &str, parent: &str) -> bool {
        if child == parent || parent == "java/lang/Object" {
            return true;
        }
        // String implements CharSequence (used by frame-merge tests).
        matches!(
            (child, parent),
            ("java/lang/String", "java/lang/CharSequence")
        )
    }

    fn common_superclass(&self, _a: &str, _b: &str) -> String {
        "java/lang/Object".to_string()
    }

    fn is_interface(&self, _name: &str) -> bool {
        false
    }
}

#[test]
fn locals_epoch_changes_exactly_when_the_locals_do() {
    let mut frame = VerificationFrame::initial_frame("C", "m", "(I)V", true, 3, 2);
    let e0 = frame.locals.epoch();
    // A clone is the same contents, so the same epoch.
    let copy = frame.clone();
    assert_eq!(copy.locals.epoch(), e0);
    // Reads never change it.
    let _ = frame.local_load(0).unwrap();
    let _ = frame.locals.len();
    assert_eq!(frame.locals.epoch(), e0);
    // Storing what the slot already holds changes nothing.
    frame.local_store(0, VType::Int).unwrap();
    assert_eq!(frame.locals.epoch(), e0);
    // A real store does, in the frame and not in the clone.
    frame.local_store(1, VType::Float).unwrap();
    assert_ne!(frame.locals.epoch(), e0);
    assert_eq!(copy.locals.epoch(), e0);
    // The stack is not part of the locals.
    let e1 = frame.locals.epoch();
    frame.push(VType::Int).unwrap();
    frame.pop().unwrap();
    assert_eq!(frame.locals.epoch(), e1);
    // Padding, direct mutation and adoption all draw a fresh one.
    frame.pad_locals_to(5);
    let e2 = frame.locals.epoch();
    assert_ne!(e2, e1);
    frame.locals[0] = VType::Top;
    let e3 = frame.locals.epoch();
    assert_ne!(e3, e2);
    frame.adopt_declared(&copy, 3);
    assert_ne!(frame.locals.epoch(), e3);
    assert_eq!(frame.locals, copy.locals);
}

#[test]
fn initial_frames_match_the_two_separate_constructors() {
    let this: std::sync::Arc<str> = std::sync::Arc::from("com/example/Foo");
    for (name, desc, is_static) in [
        ("bar", "(IJLjava/lang/String;)V", false),
        ("<init>", "(DD)V", false),
        ("s", "([I)I", true),
    ] {
        let (compact, initial) =
            VerificationFrame::initial_frames(&this, name, desc, is_static, 9, 4);
        let compact_ref = VerificationFrame::compact_initial_frame(
            "com/example/Foo",
            name,
            desc,
            is_static,
            4,
        );
        let initial_ref =
            VerificationFrame::initial_frame("com/example/Foo", name, desc, is_static, 9, 4);
        assert_eq!(compact.locals, compact_ref.locals);
        assert_eq!(compact.stack, compact_ref.stack);
        assert_eq!(compact.max_locals(), compact_ref.max_locals());
        assert_eq!(initial.locals, initial_ref.locals);
        assert_eq!(initial.stack, initial_ref.stack);
        assert_eq!(initial.max_locals(), initial_ref.max_locals());
    }
}

#[test]
fn initial_frame_static_void() {
    let frame = VerificationFrame::initial_frame(
        "com/example/Foo",
        "bar",
        "()V",
        true, // static
        1,
        2,
    );
    // Static method with no params: all locals are Top
    assert_eq!(frame.locals, vec![VType::Top]);
    assert!(frame.stack.is_empty());
}

#[test]
fn initial_frame_instance_method() {
    let frame = VerificationFrame::initial_frame(
        "com/example/Foo",
        "bar",
        "(I)V",
        false, // instance
        3,
        2,
    );
    // local[0] = Foo, local[1] = Int, local[2] = Top
    assert_eq!(
        frame.locals[0],
        VType::ObjectRef(Arc::from("com/example/Foo"))
    );
    assert_eq!(frame.locals[1], VType::Int);
    assert_eq!(frame.locals[2], VType::Top);
}

#[test]
fn initial_frame_constructor() {
    let frame =
        VerificationFrame::initial_frame("com/example/Foo", "<init>", "()V", false, 1, 1);
    assert_eq!(frame.locals[0], VType::UninitializedThis);
}

#[test]
fn initial_frame_long_param() {
    let frame = VerificationFrame::initial_frame("Foo", "m", "(J)V", true, 3, 1);
    // local[0] = Long, local[1] = Top (second slot), local[2] = Top
    assert_eq!(frame.locals[0], VType::Long);
    assert_eq!(frame.locals[1], VType::Top);
    assert_eq!(frame.locals[2], VType::Top);
}

#[test]
fn push_pop() {
    let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 4);
    frame.push(VType::Int).unwrap();
    frame.push(VType::Float).unwrap();
    assert_eq!(frame.stack_depth(), 2);

    let top = frame.pop().unwrap();
    assert_eq!(top, VType::Float);
    assert_eq!(frame.stack_depth(), 1);
}

#[test]
fn pop_empty_stack_errors() {
    let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 1);
    assert!(frame.pop().is_err());
}

#[test]
fn push_overflow_errors() {
    let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 1);
    frame.push(VType::Int).unwrap();
    assert!(frame.push(VType::Int).is_err());
}

#[test]
fn pop_expect_matching() {
    let h = MockHierarchy;
    let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 2);
    frame.push(VType::Int).unwrap();
    assert!(frame.pop_expect(&VType::Int, &h).is_ok());
}

#[test]
fn pop_expect_wrong_type() {
    let h = MockHierarchy;
    let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 2);
    frame.push(VType::Float).unwrap();
    assert!(frame.pop_expect(&VType::Int, &h).is_err());
}

#[test]
fn local_load_store() {
    let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 3, 1);
    frame.local_store(1, VType::Int).unwrap();
    assert_eq!(frame.local_load(1).unwrap(), &VType::Int);
}

#[test]
fn local_out_of_range_errors() {
    let frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 1);
    assert!(frame.local_load(5).is_err());
}

#[test]
fn frame_is_assignable_to_self() {
    let h = MockHierarchy;
    let frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 1);
    assert!(frame.is_assignable_to(&frame, &h));
}

#[test]
fn frame_merge_same_is_same() {
    let h = MockHierarchy;
    let frame = VerificationFrame::initial_frame("Foo", "m", "(I)V", true, 2, 2);
    let merged = frame.merge(&frame, &h).unwrap();
    assert_eq!(merged.locals, frame.locals);
}

#[test]
fn frame_merge_different_stack_depth_errors() {
    let h = MockHierarchy;
    let mut frame1 = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 2);
    let frame2 = frame1.clone();
    frame1.push(VType::Int).unwrap();
    assert!(frame1.merge(&frame2, &h).is_err());
}

#[test]
fn frame_assignable_string_to_charsequence_local() {
    // Regression: at a StackMapTable merge point, a local typed as
    // String in the current frame must be assignable to a declared
    // local of CharSequence (String implements CharSequence).
    let h = MockHierarchy;
    let mut current = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 1);
    current.locals[0] = VType::ObjectRef(Arc::from("java/lang/String"));
    let mut declared = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 1);
    declared.locals[0] = VType::ObjectRef(Arc::from("java/lang/CharSequence"));
    assert!(current.is_assignable_to(&declared, &h));
}

#[test]
fn frame_assignable_int_local_to_top_declared() {
    // Regression (B2): a StackMapTable full_frame may declare a local
    // slot as Top to indicate the value is unused past the merge point.
    // The current frame may still carry a concrete type (e.g. Int) from
    // a fall-through path; it must still be considered assignable since
    // Top is the top of the verification type lattice (JVMS 4.10.1.2).
    let h = MockHierarchy;
    let mut current = VerificationFrame::initial_frame("Foo", "m", "()V", true, 4, 1);
    current.locals[3] = VType::Int;
    let mut declared = VerificationFrame::initial_frame("Foo", "m", "()V", true, 4, 1);
    declared.locals[3] = VType::Top;
    assert!(current.is_assignable_to(&declared, &h));
}

// ----- max_locals bound (JVMS §4.9.1) -----------------------------------

#[test]
fn local_index_beyond_max_locals_rejected_even_when_described() {
    // `initial_frame` sizes `locals` from the descriptor and only pads up
    // to `max_locals`. Here the descriptor needs 3 slots (this + long) but
    // `max_locals` is 1, so slots 1 and 2 are DESCRIBED by the frame while
    // the runtime frame does not have them. They must be unaddressable.
    let frame = VerificationFrame::initial_frame("Foo", "m", "(J)V", false, 1, 1);
    assert!(
        frame.locals.len() > 1,
        "descriptor over-fills the locals vec"
    );
    assert!(frame.local_load(0).is_ok());
    assert!(
        frame.local_load(1).is_err(),
        "slot 1 is past max_locals and must not verify"
    );
    assert!(frame.local_load(2).is_err());
}

#[test]
fn local_store_beyond_max_locals_rejected() {
    let mut frame = VerificationFrame::initial_frame("Foo", "m", "(J)V", false, 1, 1);
    assert!(frame.local_store(2, VType::Int).is_err());
}

#[test]
fn pad_locals_to_installs_the_max_locals_bound() {
    // A compact (StackMapTable-derived) frame is unbounded until adopted;
    // adoption is `pad_locals_to`, which installs the real bound.
    let mut compact = VerificationFrame::compact_initial_frame("Foo", "m", "(I)V", true, 2);
    assert_eq!(compact.locals.len(), 1);
    compact.pad_locals_to(4);
    assert_eq!(compact.max_locals(), 4);
    assert_eq!(compact.locals.len(), 4);
    assert!(compact.local_load(3).is_ok());
    assert!(compact.local_load(4).is_err());
}

#[test]
fn pad_locals_to_never_widens_past_max_locals() {
    // A `full_frame` may declare MORE locals than `max_locals`; the
    // overhang must become unaddressable rather than raising the bound.
    let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 6, 1);
    frame.pad_locals_to(2);
    assert_eq!(frame.locals.len(), 6, "padding never shrinks the vec");
    assert!(frame.local_load(1).is_ok());
    assert!(
        frame.local_load(2).is_err(),
        "slot 2 is described but past max_locals"
    );
}

// ----- category-2 slot pairing (JVMS §4.10.1.6) -------------------------

#[test]
fn cat2_local_pair_round_trips() {
    let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 4, 1);
    frame.local_store_wide(1, VType::Long).unwrap();
    assert_eq!(frame.locals[1], VType::Long);
    assert_eq!(frame.locals[2], VType::Top);
    assert!(frame.local_load_wide(1, &VType::Long).is_ok());
}

#[test]
fn storing_over_the_upper_half_invalidates_the_cat2_base() {
    // `lstore_1; istore_2` overwrites the long's upper half. The base must
    // become `Top`, otherwise a later `lload_1` would verify clean and read
    // a torn value at run time.
    let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 4, 1);
    frame.local_store_wide(1, VType::Long).unwrap();
    frame.local_store(2, VType::Int).unwrap();
    assert_eq!(
        frame.locals[1],
        VType::Top,
        "cat-2 base must be invalidated"
    );
    assert!(
        frame.local_load_wide(1, &VType::Long).is_err(),
        "the split pair must not read back as a long"
    );
}

#[test]
fn cat2_load_rejects_a_split_pair() {
    let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 4, 1);
    // Hand-build a split pair: a Long base whose upper half is an Int.
    frame.locals[1] = VType::Long;
    frame.locals[2] = VType::Int;
    assert!(frame.local_load_wide(1, &VType::Long).is_err());
}

#[test]
fn cat2_store_at_last_slot_rejected() {
    // `max_locals = 2` leaves slots 0 and 1; a long at slot 1 needs slot 2.
    let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 2, 1);
    assert!(frame.local_store_wide(1, VType::Double).is_err());
    assert!(frame.local_store_wide(0, VType::Double).is_ok());
}

#[test]
fn cat2_store_at_u16_max_does_not_overflow() {
    // A `wide lstore 65535` used to compute `index + 1` unchecked: a debug
    // panic, and a wrap to slot 0 in release. Both are driven straight from
    // attacker-supplied bytecode, so this must be a clean rejection.
    let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 4, 1);
    assert!(frame.local_store_wide(u16::MAX, VType::Long).is_err());
    assert!(frame.local_load_wide(u16::MAX, &VType::Long).is_err());
}

// ----- uninitializedThis tracking ---------------------------------------

#[test]
fn constructor_initial_frame_reports_uninitialized_this() {
    let frame = VerificationFrame::initial_frame("Foo", "<init>", "()V", false, 1, 1);
    assert!(frame.has_uninitialized_this());
}

#[test]
fn ordinary_method_has_no_uninitialized_this() {
    let frame = VerificationFrame::initial_frame("Foo", "m", "()V", false, 1, 1);
    assert!(!frame.has_uninitialized_this());
}

#[test]
fn clear_stack() {
    let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 4);
    frame.push(VType::Int).unwrap();
    frame.push(VType::Float).unwrap();
    frame.clear_stack();
    assert!(frame.stack.is_empty());
}
