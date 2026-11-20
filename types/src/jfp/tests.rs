use super::*;

/// The three NaNs a real program actually produces, and the rules that
/// separate this module from the two Rust primitives it is easy to reach
/// for instead.
#[test]
fn every_nan_is_one_value_for_equality_hashing_and_ordering() {
    let quiet = f64::from_bits(0x7ff8_0000_0000_0000);
    let signed = f64::from_bits(0xfff8_0000_0000_0000); // what `sqrt(-1.0)` gives on x86
    let payload = f64::from_bits(0x7ff8_0000_0000_0001);
    let all_ones = f64::from_bits(0x7fff_ffff_ffff_ffff);

    for &a in &[quiet, signed, payload, all_ones] {
        for &b in &[quiet, signed, payload, all_ones] {
            assert!(double_equals(a, b), "{a:?} vs {b:?}");
            assert_eq!(double_compare(a, b), 0);
            assert_eq!(double_hash_code(a), double_hash_code(b));
        }
        // Greater than every number, including +inf — this is the clause
        // `f64::total_cmp` gets wrong for `signed`, where it answers "less
        // than -inf".
        assert_eq!(double_compare(a, f64::INFINITY), 1);
        assert_eq!(double_compare(f64::NEG_INFINITY, a), -1);
    }
    assert_eq!(
        signed.total_cmp(&f64::NEG_INFINITY),
        std::cmp::Ordering::Less
    );
}

#[test]
fn signed_zero_is_two_values() {
    assert!(!double_equals(0.0, -0.0));
    assert_eq!(double_compare(-0.0, 0.0), -1);
    assert_eq!(double_compare(0.0, -0.0), 1);
    assert_eq!(double_compare(0.0, 0.0), 0);
    assert_ne!(double_hash_code(0.0), double_hash_code(-0.0));
    // ...but `==` says they are the same, which is why a map keyed on them
    // with plain `==` silently loses one.
    assert!(0.0 == -0.0);
}

#[test]
fn ordinary_values_are_unaffected() {
    assert_eq!(double_compare(1.0, 2.0), -1);
    assert_eq!(double_compare(2.0, 1.0), 1);
    assert_eq!(double_compare(-5.5, -5.5), 0);
    assert_eq!(double_to_long_bits(1.0), 0x3ff0_0000_0000_0000);
    assert_eq!(double_hash_code(1.0), 0x3ff0_0000);
}

#[test]
fn float_mirrors_double() {
    let quiet = f32::from_bits(0x7fc0_0000);
    let signed = f32::from_bits(0xffc0_0000);
    let payload = f32::from_bits(0x7fc0_0001);
    for &a in &[quiet, signed, payload] {
        for &b in &[quiet, signed, payload] {
            assert!(float_equals(a, b));
            assert_eq!(float_compare(a, b), 0);
            assert_eq!(float_hash_code(a), float_hash_code(b));
        }
        assert_eq!(float_compare(a, f32::INFINITY), 1);
    }
    assert!(!float_equals(0.0, -0.0));
    assert_eq!(float_compare(-0.0, 0.0), -1);
}

/// The three ways this rule gets written wrong, side by side.
///
/// `f64::total_cmp` and `partial_cmp(..).unwrap_or(Equal)` are the two
/// substitutions that have actually shipped in this tree, and each is wrong
/// in its own direction. Naming both here means a reader who reaches for
/// one finds the counter-example before the census does.
#[test]
fn neither_total_cmp_nor_partial_cmp_is_double_compare() {
    let neg_nan = f64::from_bits(0xfff8_0000_0000_0000);

    // total_cmp: orders a negatively-signed NaN BELOW -infinity.
    assert_eq!(double_compare(neg_nan, f64::NEG_INFINITY), 1);
    assert_eq!(
        neg_nan.total_cmp(&f64::NEG_INFINITY),
        std::cmp::Ordering::Less
    );

    // partial_cmp: `None` for any NaN, so the usual `unwrap_or(Equal)`
    // makes NaN equal to everything — not merely the wrong order, but a
    // NON-TRANSITIVE comparator: NaN == 1.0 and NaN == 2.0 while 1.0 != 2.0.
    assert_eq!(neg_nan.partial_cmp(&1.0), None);
    assert_eq!(double_compare(neg_nan, 1.0), 1);
    assert_eq!(double_compare(1.0, neg_nan), -1);

    // ...and it is wrong with no NaN in sight, which is the reachable half:
    // `partial_cmp` calls the two zeros equal because `-0.0 == 0.0`.
    assert_eq!((-0.0f64).partial_cmp(&0.0), Some(std::cmp::Ordering::Equal));
    assert_eq!(double_compare(-0.0, 0.0), -1);
}

/// A sort is the shape that actually reached users, so assert on one.
#[test]
fn sorting_puts_every_nan_at_the_top_whatever_its_payload() {
    let mut v = vec![
        f64::from_bits(0xfff8_0000_0000_0000),
        f64::NEG_INFINITY,
        -0.0,
        0.0,
        1.0,
        f64::INFINITY,
        f64::from_bits(0x7ff8_0000_0000_0001),
    ];
    v.sort_by(|a, b| double_ordering(*a, *b));
    let bits: Vec<u64> = v.iter().map(|x| x.to_bits()).collect();
    assert_eq!(
        bits,
        vec![
            0xfff0_0000_0000_0000, // -inf
            0x8000_0000_0000_0000, // -0.0
            0x0000_0000_0000_0000, // +0.0
            0x3ff0_0000_0000_0000, // 1.0
            0x7ff0_0000_0000_0000, // +inf
            // The two NaNs keep their payloads — only the ORDER is
            // canonical — and land above +inf. Under `total_cmp` the
            // negative one would have sorted first, below -inf.
            0xfff8_0000_0000_0000,
            0x7ff8_0000_0000_0001,
        ]
    );
}

/// `double_min`/`double_max` are not `f64::min`/`f64::max`: NaN
/// propagates, and signed zero is pinned rather than left unspecified.
#[test]
fn double_min_max_propagate_nan_and_pin_signed_zero() {
    assert!(double_max(1.0, f64::NAN).is_nan());
    assert!(double_min(1.0, f64::NAN).is_nan());
    assert!(double_max(f64::NAN, 1.0).is_nan());
    assert!(double_min(f64::NAN, 1.0).is_nan());

    assert_eq!(double_max(0.0, -0.0).to_bits(), 0.0f64.to_bits());
    assert_eq!(double_min(0.0, -0.0).to_bits(), (-0.0f64).to_bits());
    assert_eq!(double_max(-0.0, 0.0).to_bits(), 0.0f64.to_bits());
    assert_eq!(double_min(-0.0, 0.0).to_bits(), (-0.0f64).to_bits());

    assert_eq!(double_max(1.0, 3.0), 3.0);
    assert_eq!(double_min(1.0, 3.0), 1.0);
}
