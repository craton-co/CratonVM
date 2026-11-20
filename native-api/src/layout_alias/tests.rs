use super::*;

/// The rule, stated against the case that motivated widening the detector.
///
/// `java/nio/channels/AsynchronousSocketChannel` declares exactly one
/// instance field (`provider`, transitively — its superclass is
/// `java/lang/Object`, checked with `javap -p` against JDK 25.0.3.9), and
/// `native-io/src/async_socket.rs` allocates it with `N_FIELDS = 4`. That is
/// the widest live over-allocation W7-49-slot-index-recensus.md found, and
/// the pre-2026-08-12 detector could not see it because that site is a
/// direct `alloc_object` rather than the fabrication funnel.
///
/// This asserts the *classification*, which is the only half a unit test can
/// reach. That the site now arrives here at all is what
/// `native-api/tests/layout_alias_coverage.rs` proves, and the two together
/// are the claim — this one alone would be a probe that cannot fail.
/// (The path was `vm/tests/layout_alias_detector_coverage.rs` here until
/// 2026-08-12; no such file was ever committed. A pointer to a gate that
/// does not exist reads exactly like a gate that does — corrected while
/// reading this module for W7-68-live-under-allocations.md.)
#[test]
fn async_socket_channel_shape_classifies_as_over() {
    assert_eq!(classify(4, 1), Some(Direction::Over));
}

#[test]
fn under_and_exact_and_unmeasured() {
    assert_eq!(
        classify(1, 3),
        Some(Direction::Under),
        "Kafka HashSet shape"
    );
    assert_eq!(classify(3, 3), None, "agreement is not a finding");
    assert_eq!(classify(0, 4), None, "caller asserts no layout");
}

/// The blind spot W7-73-short-object-blind-spot.md closed.
///
/// This assertion was `assert_eq!(classify(4, 0), None, "class not loaded,
/// or an interface")` until 2026-08-12, with a comment calling it
/// "unmeasured, NOT clean". The comment was right and the return value was
/// still read as clean by every consumer, because a suppressed row and an
/// absent defect are the same bytes on the wire.
///
/// `declared == 0` is the ONLY case the base allocator's
/// `slots = num_fields.max(real_fields)` clamp leaves alone, so it is the
/// only case in which the object can be narrower than the class it is handed
/// out as. The three shapes that produce it are in the module header; this
/// test asserts the classification, and
/// `native-api/tests/layout_alias_coverage.rs` asserts that the sites
/// producing it actually reach here.
#[test]
fn a_class_declaring_nothing_is_reported_not_swallowed() {
    assert_eq!(
        classify(6, 0),
        Some(Direction::Undeclared),
        "java/util/zip/ZipEntry's ClassId::new(0) fallback arm: 6 slots, no \
         declared layout to clamp against, real class declares 14"
    );
    assert_eq!(
        classify(5, 0),
        Some(Direction::Undeclared),
        "the java/lang/Thread mirror in vertx_eventloop.rs / xnio_io_thread.rs: \
         5 slots against a class declaring 19, and NOT on a fallback arm"
    );
    // Still nothing to say: the caller asserted no layout at all.
    assert_eq!(classify(0, 0), None, "no request, no claim");
}

/// The dedup key carries the site, so two natives making the same mistake on
/// the same class are two rows. Collapsing them would be the detector
/// choosing to be quieter, which this lane is not allowed to do.
#[test]
fn site_is_part_of_the_dedup_key() {
    let a = AllocSite::Java("Foo.bar").as_key();
    let b = AllocSite::Java("Baz.qux").as_key();
    assert_ne!(a, b);
}
