use super::{java_int_to_string_radix, java_long_to_string_radix, java_radix_or_ten};

/// The JDK IGNORES an out-of-range radix and uses 10. It does NOT throw,
/// and it does NOT clamp into 2..=36 (a `clamp` would answer "11111111"
/// for `toString(255, 0)` where the JDK says "255").
#[test]
fn out_of_range_radix_substitutes_ten_and_never_clamps() {
    for bad in [0, 1, -1, 37, 40, i32::MIN, i32::MAX] {
        assert_eq!(java_radix_or_ten(bad), 10, "radix {bad}");
        assert_eq!(java_int_to_string_radix(5, bad), "5", "radix {bad}");
        assert_eq!(java_int_to_string_radix(-5, bad), "-5", "radix {bad}");
        assert_eq!(java_int_to_string_radix(255, bad), "255", "radix {bad}");
        assert_eq!(
            java_int_to_string_radix(i32::MIN, bad),
            "-2147483648",
            "radix {bad}"
        );
        assert_eq!(
            java_int_to_string_radix(i32::MAX, bad),
            "2147483647",
            "radix {bad}"
        );
        assert_eq!(java_long_to_string_radix(-5, bad), "-5", "radix {bad}");
        assert_eq!(
            java_long_to_string_radix(i64::MIN, bad),
            "-9223372036854775808",
            "radix {bad}"
        );
        assert_eq!(
            java_long_to_string_radix(i64::MAX, bad),
            "9223372036854775807",
            "radix {bad}"
        );
    }
    // Both boundary radices stay themselves.
    assert_eq!(java_radix_or_ten(2), 2);
    assert_eq!(java_radix_or_ten(36), 36);
    assert_eq!(java_int_to_string_radix(255, 2), "11111111");
    assert_eq!(java_int_to_string_radix(255, 36), "73");
}

/// Radix 0 divides by zero and radix 1 never terminates in an unguarded
/// digit loop, so this drives them on a worker thread against a deadline:
/// a regression FAILS the suite (hang -> timeout, panic -> disconnect)
/// instead of wedging it.
#[test]
fn radix_zero_and_one_terminate_within_a_deadline() {
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let out = vec![
            java_int_to_string_radix(5, 0),
            java_int_to_string_radix(5, 1),
            java_int_to_string_radix(-5, 1),
            java_int_to_string_radix(i32::MIN, 0),
            java_long_to_string_radix(5, 0),
            java_long_to_string_radix(5, 1),
            java_long_to_string_radix(i64::MIN, 1),
        ];
        let _ = tx.send(out);
    });
    let out = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("radix 0/1 must terminate and must not panic");
    assert_eq!(
        out,
        vec![
            "5",
            "5",
            "-5",
            "-2147483648",
            "5",
            "5",
            "-9223372036854775808",
        ]
    );
    worker.join().expect("worker thread panicked");
}

/// `Integer.MIN_VALUE` has no positive counterpart — negating it overflows.
#[test]
fn integer_min_value_at_every_legal_radix() {
    assert_eq!(
        java_int_to_string_radix(i32::MIN, 2),
        "-10000000000000000000000000000000"
    );
    assert_eq!(java_int_to_string_radix(i32::MIN, 8), "-20000000000");
    assert_eq!(java_int_to_string_radix(i32::MIN, 10), "-2147483648");
    assert_eq!(java_int_to_string_radix(i32::MIN, 16), "-80000000");
    assert_eq!(java_int_to_string_radix(i32::MIN, 36), "-zik0zk");
    assert_eq!(java_int_to_string_radix(i32::MAX, 36), "zik0zj");
    assert_eq!(java_int_to_string_radix(i32::MAX, 16), "7fffffff");
    // Every legal radix must at least round-trip through the magnitude.
    for r in 2..=36i32 {
        let s = java_int_to_string_radix(i32::MIN, r);
        let mag = s.strip_prefix('-').expect("MIN_VALUE renders negative");
        assert_eq!(
            u32::from_str_radix(mag, r as u32),
            Ok(2147483648u32),
            "radix {r}"
        );
    }
}

/// `Long.MIN_VALUE`, same trap one width up.
#[test]
fn long_min_value_at_every_legal_radix() {
    assert_eq!(java_long_to_string_radix(i64::MIN, 16), "-8000000000000000");
    assert_eq!(java_long_to_string_radix(i64::MIN, 36), "-1y2p0ij32e8e8");
    assert_eq!(
        java_long_to_string_radix(i64::MIN, 8),
        "-1000000000000000000000"
    );
    assert_eq!(
        java_long_to_string_radix(i64::MIN, 2),
        "-1000000000000000000000000000000000000000000000000000000000000000"
    );
    assert_eq!(java_long_to_string_radix(i64::MAX, 36), "1y2p0ij32e8e7");
    for r in 2..=36i32 {
        let s = java_long_to_string_radix(i64::MIN, r);
        let mag = s.strip_prefix('-').expect("MIN_VALUE renders negative");
        assert_eq!(
            u64::from_str_radix(mag, r as u32),
            Ok(9223372036854775808u64),
            "radix {r}"
        );
    }
}

/// Negatives are sign-magnitude, never a two's-complement bit pattern —
/// the defect in the (now deleted) duplicate `Integer.toString(II)` body,
/// which used `format!("{:x}", val)` on a signed `i32` and answered
/// "ffffffff" for `Integer.toString(-1, 16)`.
#[test]
fn negatives_are_sign_magnitude() {
    assert_eq!(java_int_to_string_radix(-1, 16), "-1");
    assert_eq!(java_int_to_string_radix(-1, 2), "-1");
    assert_eq!(java_int_to_string_radix(-1, 8), "-1");
    assert_eq!(java_int_to_string_radix(-255, 16), "-ff");
    assert_eq!(java_int_to_string_radix(-5, 2), "-101");
    assert_eq!(java_long_to_string_radix(-1, 16), "-1");
    assert_eq!(java_long_to_string_radix(-255, 16), "-ff");
    assert_ne!(java_int_to_string_radix(-1, 16), "ffffffff");
    assert_ne!(java_long_to_string_radix(-1, 16), "ffffffffffffffff");
}

/// Zero and the small positives, at both boundaries and base 10.
#[test]
fn zero_and_small_positives() {
    for r in 2..=36i32 {
        assert_eq!(java_int_to_string_radix(0, r), "0", "radix {r}");
        assert_eq!(java_long_to_string_radix(0, r), "0", "radix {r}");
    }
    assert_eq!(java_int_to_string_radix(5, 2), "101");
    assert_eq!(java_int_to_string_radix(255, 16), "ff");
    assert_eq!(java_int_to_string_radix(255, 8), "377");
    assert_eq!(java_long_to_string_radix(255, 36), "73");
}
