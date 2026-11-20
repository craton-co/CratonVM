use super::{analyze_with_receiver, receiver_in_local_zero};

/// `this` is non-null, and the loop header is where that stops being free.
///
/// The body below is `for (i = 0; i < 10; i++) { x = this.f; }`. The
/// `getfield` at bci 3 proves its own receiver non-null on fall-through,
/// so the BACKEDGE into the loop header carries the fact. The entry path
/// does not, and the meet is an intersection — so without a seed the fact
/// dies at the header on every iteration and the receiver is re-tested
/// forever, for a value the JVM guarantees at entry.
///
/// Both arms are asserted in one test on purpose: the `false` arm IS the
/// pre-seed behaviour, so this test would have failed before the seed
/// existed and states exactly what changed.
#[test]
fn the_receiver_seed_is_what_survives_the_loop_header_meet() {
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0x03,               // 0:  iconst_0
        0x3C,               // 1:  istore_1
        0x2A,               // 2:  aload_0          <- loop header
        0xB4, 0x00, 0x01,   // 3:  getfield #1
        0x3D,               // 6:  istore_2
        0x1B,               // 7:  iload_1
        0x04,               // 8:  iconst_1
        0x60,               // 9:  iadd
        0x3C,               // 10: istore_1
        0x1B,               // 11: iload_1
        0x10, 0x0A,         // 12: bipush 10
        0xA1, 0xFF, 0xF4,   // 14: if_icmplt 2      (14 - 12)
        0x1B,               // 17: iload_1
        0xAC,               // 18: ireturn
    ];
    let n = code.len();

    let unseeded = analyze_with_receiver(&code, n, false);
    assert!(
        !unseeded.is_nonnull(3, 0),
        "without the seed the entry path carries nothing, so the meet at \
         the loop header must be empty -- if this passes, the header is no \
         longer a join and the test has stopped measuring the thing it names"
    );

    let seeded = analyze_with_receiver(&code, n, true);
    assert!(
        seeded.is_nonnull(3, 0),
        "with `this` seeded, both edges into the header carry local 0 and \
         the getfield receiver check is dead"
    );
}

/// A `getfield` on a local that is NOT the receiver keeps its check.
///
/// Same shape, but the field is read out of local 1 -- a parameter, which
/// the seed says nothing about. The guard here is against a seed that
/// leaks across locals, which would elide a real null check on an argument
/// and turn an NPE into a SIGSEGV.
#[test]
fn the_seed_does_not_leak_to_a_parameter() {
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0x2B,               // 0: aload_1
        0xB4, 0x00, 0x01,   // 1: getfield #1
        0x57,               // 4: pop
        0xB1,               // 5: return
    ];
    let seeded = analyze_with_receiver(&code, code.len(), true);
    assert!(
        !seeded.is_nonnull(1, 1),
        "the seed proves local 0 and nothing else"
    );
}

/// The derivation, including the width convention that would silently
/// misclassify every method with a `long` or `double` parameter.
#[test]
fn the_receiver_derivation_reads_an_argument_count_not_a_slot_count() {
    // Instance: descriptor declares 2, `num_params` counts `this` too.
    assert_eq!(receiver_in_local_zero("Foo.bar:(II)V", 3), Some(true));
    // Static: the two agree exactly.
    assert_eq!(receiver_in_local_zero("Foo.bar:(II)V", 2), Some(false));
    // `J` and `D` are ONE argument each here. Counting them as two JVM
    // slots (which `compute_param_jvm_slots` does, and this must not)
    // would make declared = 4 and answer `None` for a plain instance
    // method -- or, worse, `Some(false)` for the static one.
    assert_eq!(receiver_in_local_zero("Foo.bar:(JD)V", 3), Some(true));
    assert_eq!(receiver_in_local_zero("Foo.bar:(JD)V", 2), Some(false));
    // No-arg forms are still distinguishable.
    assert_eq!(receiver_in_local_zero("Foo.bar:()V", 1), Some(true));
    assert_eq!(receiver_in_local_zero("Foo.bar:()V", 0), Some(false));
    // Disagreement is unanswered, never guessed.
    assert_eq!(receiver_in_local_zero("Foo.bar:(II)V", 7), None);
    // The legacy `compile()` wrapper's empty key has no descriptor.
    assert_eq!(receiver_in_local_zero("", 1), None);
    assert_eq!(receiver_in_local_zero("Foo.bar:(II", 3), None);
}
