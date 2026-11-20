/// Software model of the emitted `emit_ldiv_magic64` sequence:
/// t = mulhi_signed(magic, n) (+ n when magic < 0);
/// q = (t >> shift) + (n logical>> 63).
fn model_q(n: i64, magic: i64, shift: u32) -> i64 {
    let mut t = ((n as i128 * magic as i128) >> 64) as i64;
    if magic < 0 {
        t = t.wrapping_add(n);
    }
    (t >> shift).wrapping_add(((n as u64) >> 63) as i64)
}

#[test]
fn magic_signed_div64_matches_exact_division() {
    let divisors: [i64; 14] = [2, 3, 5, 6, 7, 9, 10, 11, 12, 25, 100, 1000, 7919, 1_000_003];
    let mut dividends: Vec<i64> = vec![
        0,
        1,
        -1,
        2,
        -2,
        i64::MAX,
        i64::MIN,
        i64::MAX - 1,
        i64::MIN + 1,
    ];
    // Pseudo-random spread (deterministic LCG) incl. sign flips.
    let mut x = 0x9E3779B97F4A7C15u64;
    for _ in 0..2000 {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        dividends.push(x as i64);
    }
    for &d in &divisors {
        let (magic, shift) = super::Compiler::magic_signed_div64(d);
        let mut extra = vec![d, -d, d - 1, 1 - d, d + 1, -d - 1, d * 3, -d * 3];
        extra.extend_from_slice(&dividends);
        for &n in &extra {
            let expect = n / d; // Rust trunc-toward-zero == JVM ldiv
            let got = model_q(n, magic, shift);
            assert_eq!(
                got, expect,
                "n={n} d={d} magic={magic:#x} shift={shift}: got {got}, want {expect}"
            );
        }
    }
}
