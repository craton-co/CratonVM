use super::*;

/// `Object(None)` must NOT be boxed. A native clearing a real reference
/// field writes exactly that, and boxing it would replace a null field
/// with a non-null wrapper — the one regression this scheme can cause
/// (`vm/src/vm/vm_init.rs`'s `System.out` slot-0 guard is the site that
/// already had to work around a non-null wrapper defeating a null check).
#[test]
fn a_null_reference_is_not_boxed() {
    assert!(!needs_reference_box(Value::Object(None)));
    // SAFETY: never dereferenced — `needs_reference_box` only reads the tag.
    let some = unsafe { ObjectRef::from_raw(8usize as *mut u8) };
    assert!(!needs_reference_box(Value::Object(Some(some))));
}

/// The other side, so the predicate is not vacuously false: every
/// non-reference tag IS boxed. `Uninitialized` is included deliberately —
/// it is what a freshly-allocated legacy slot decodes as, and letting it
/// through unboxed would write a raw 0 into the slot under exactly the
/// encoder this change exists to stop trusting.
#[test]
fn every_non_reference_tag_is_boxed() {
    for v in [
        Value::Int(16),
        Value::Long(-1),
        Value::Float(0.75),
        Value::Double(-17.5),
        Value::Uninitialized,
    ] {
        assert!(needs_reference_box(v), "{v:?} must be boxed");
    }
}

/// The latch gates the read half, and the read half is otherwise a
/// faithful un-wrap. Both directions, so neither "always unbox" nor
/// "never unbox" passes.
#[test]
fn the_latch_gates_the_read_half_in_both_directions() {
    // SAFETY: never dereferenced — the closures below are the only things
    // that would, and they answer from the test's own tables.
    let r = unsafe { ObjectRef::from_raw(16usize as *mut u8) };
    let boxed = Value::Object(Some(r));
    let owner = ClassId::new(9);

    // Latch armed (this test's own store; the flag is monotone and other
    // tests in this binary may have armed it already, which is fine). An
    // array wrapper arms the UNEXPECTED latch, so every slot is screened
    // open from here on.
    note_wrapper_created();
    assert!(field_read_may_hold_wrapper(owner, 3));
    assert_eq!(
        unbox_reference_field(
            boxed,
            owner,
            3,
            |_| Some(AUTOBOX_CLASS_ID),
            |_| { Value::Int(42) }
        ),
        Value::Int(42),
        "an armed latch plus a wrapper class id must unbox",
    );
    assert_eq!(
        unbox_reference_field(
            boxed,
            owner,
            3,
            |_| Some(ClassId::new(3)),
            |_| { Value::Int(42) }
        ),
        boxed,
        "an ordinary object must come back as itself, not as its slot 0",
    );
    assert_eq!(
        unbox_reference_field(boxed, owner, 3, |_| None, |_| Value::Int(42)),
        boxed,
        "an address the heap cannot validate must come back untouched",
    );
    assert_eq!(
        unbox_reference_field(
            Value::Object(None),
            owner,
            3,
            |_| Some(AUTOBOX_CLASS_ID),
            |_| { Value::Int(42) }
        ),
        Value::Object(None),
        "null is not a wrapper",
    );
}

/// `box_for_reference_slot` arms the latch, and returns the wrapper the
/// allocator handed it rather than the primitive.
#[test]
fn boxing_arms_the_latch_and_returns_the_wrapper() {
    // SAFETY: never dereferenced.
    let wrapper = unsafe { ObjectRef::from_raw(24usize as *mut u8) };
    let mut boxed_payload = None;
    let out = box_for_reference_slot(Value::Int(0x5EED), ClassId::new(9), 0, |v| {
        boxed_payload = Some(v);
        wrapper
    });
    assert_eq!(out, Value::Object(Some(wrapper)));
    assert_eq!(boxed_payload, Some(Value::Int(0x5EED)));
    assert!(wrapper_exists());
}

/// gc-common w3-e: the split latch. A declared overlay store opens the
/// field screen for ITS slot only and leaves the unexpected latch off; an
/// undeclared field store, an array wrapper, or a second declared slot
/// arms the unexpected latch, after which every slot is screened open.
/// Private instances, because the process-wide one is monotone and
/// shared with every other test in this binary.
#[test]
fn the_split_latch_keeps_the_mirror_overlay_to_its_own_slot() {
    let mirror = ClassId::new(12);
    let other = ClassId::new(40);

    let l = WrapperLatches::new();
    assert!(!l.unexpected());
    assert!(!l.field_may_hold(mirror, 0));

    // The mirror populator, ~33 times at boot.
    for _ in 0..33 {
        l.note_field(mirror, 0, true);
    }
    assert!(!l.unexpected(), "declared overlays must not arm the latch");
    assert!(
        l.field_may_hold(mirror, 0),
        "the overlay slot must still unbox"
    );
    assert!(
        !l.field_may_hold(mirror, 1),
        "a neighbouring field is not the overlay"
    );
    assert!(
        !l.field_may_hold(other, 0),
        "another class's slot 0 is not the overlay"
    );

    // A second DISTINCT declared slot: conservative.
    let l2 = WrapperLatches::new();
    l2.note_field(mirror, 0, true);
    l2.note_field(other, 3, true);
    assert!(l2.unexpected());
    assert!(l2.field_may_hold(other, 7));

    // An undeclared field store.
    let l3 = WrapperLatches::new();
    l3.note_field(other, 2, false);
    assert!(l3.unexpected());
    assert!(l3.field_may_hold(mirror, 5));

    // An array wrapper.
    let l4 = WrapperLatches::new();
    l4.note_undeclared();
    assert!(l4.field_may_hold(other, 0));

    // An index that cannot be packed is never the overlay.
    let l5 = WrapperLatches::new();
    l5.note_field(mirror, u32::MAX as usize, true);
    assert!(l5.unexpected());
    assert_eq!(
        overlay_slot_key(ClassId::new(u32::MAX), u32::MAX as usize),
        None
    );
    assert_ne!(
        overlay_slot_key(ClassId::new(u32::MAX), (u32::MAX - 1) as usize),
        Some(NO_OVERLAY_SLOT),
    );
}

/// `unbox_reference_field` screens on the SLOT: with a wrapper-looking
/// referent, the overlay slot unboxes, any other slot comes back untouched
/// (while the process-wide unexpected latch is off -- which another test in
/// this binary may already have armed, in which case both unbox and the
/// second half is skipped).
#[test]
fn unbox_reference_field_follows_the_field_screen() {
    // SAFETY: never dereferenced -- the closures answer from the test.
    let r = unsafe { ObjectRef::from_raw(40usize as *mut u8) };
    let boxed = Value::Object(Some(r));
    let mirror = ClassId::new(12);
    {
        let _scope = expect_primitive_into_reference();
        let _ = box_for_reference_slot(Value::Int(12), mirror, 0, |_| r);
    }
    assert_eq!(
        unbox_reference_field(
            boxed,
            mirror,
            0,
            |_| Some(AUTOBOX_CLASS_ID),
            |_| Value::Int(12)
        ),
        Value::Int(12),
        "the declared overlay slot must unbox",
    );
    if !unexpected_wrapper_exists() {
        assert_eq!(
            unbox_reference_field(
                boxed,
                ClassId::new(41),
                0,
                |_| panic!("validated a referent behind a closed screen"),
                |_| Value::Int(0)
            ),
            boxed,
        );
    }
}

/// gen r5w1/oom5: inside an armed fallible scope the boxing store uses
/// the scope's allocator, never the backend's panicking closure; a decline
/// stores null and is visible on the scope; the hook is one-shot and is
/// gone once the scope drops.
#[test]
fn a_fallible_wrapper_scope_declines_instead_of_calling_the_panicking_allocator() {
    fn decline(_heap: *const (), _v: Value) -> Option<ObjectRef> {
        None
    }
    fn accept(heap: *const (), _v: Value) -> Option<ObjectRef> {
        // SAFETY: never dereferenced; the "heap" pointer is the wrapper.
        Some(unsafe { ObjectRef::from_raw(heap as *mut u8) })
    }
    {
        // SAFETY: `decline` reads nothing through the pointer.
        let scope = unsafe { arm_fallible_wrapper(decline, std::ptr::null()) };
        assert!(!scope.declined());
        let out = box_for_reference_slot(Value::Int(7), ClassId::new(9), 0, |_| {
            panic!("the panicking allocator ran inside a fallible scope")
        });
        assert_eq!(out, Value::Object(None));
        assert!(scope.declined());
    }
    {
        // SAFETY: `accept` never dereferences the pointer.
        let scope = unsafe { arm_fallible_wrapper(accept, 48usize as *const ()) };
        let out = box_for_reference_slot(Value::Int(7), ClassId::new(9), 0, |_| {
            panic!("the panicking allocator ran inside a fallible scope")
        });
        // SAFETY: never dereferenced.
        let expect = unsafe { ObjectRef::from_raw(48usize as *mut u8) };
        assert_eq!(out, Value::Object(Some(expect)));
        assert!(!scope.declined());
        // One-shot: a second boxing store in the same scope takes the
        // ordinary path.
        let mut ran = false;
        let _ = box_for_reference_slot(Value::Int(8), ClassId::new(9), 0, |_| {
            ran = true;
            expect
        });
        assert!(ran);
    }
    // Dropped: the ordinary path, and no stale decline.
    let mut ran = false;
    let _ = box_for_reference_slot(Value::Int(9), ClassId::new(9), 0, |_| {
        ran = true;
        // SAFETY: never dereferenced.
        unsafe { ObjectRef::from_raw(56usize as *mut u8) }
    });
    assert!(ran);
    assert!(!FALLIBLE_WRAPPER_DECLINED.with(|d| d.get()));
}

/// A reference store must not reach the allocator at all — the hot path
/// pays no allocation, and this is what pins that.
#[test]
fn a_reference_store_never_allocates() {
    // SAFETY: never dereferenced.
    let r = unsafe { ObjectRef::from_raw(32usize as *mut u8) };
    for v in [Value::Object(None), Value::Object(Some(r))] {
        let out = box_for_reference_slot(v, ClassId::new(9), 0, |_| {
            panic!("allocated a wrapper for a reference store: {v:?}")
        });
        assert_eq!(out, v);
    }
}
