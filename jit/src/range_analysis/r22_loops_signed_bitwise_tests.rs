use super::*;

const W: IntWidth = IntWidth::W32;

fn r(lo: i64, hi: i64) -> Range {
    Range::exact(W, lo, hi)
}

/// Every `[lo, hi]` with endpoints in `-9..=9`, plus the width corners.
fn ranges() -> Vec<Range> {
    let mut out = Vec::new();
    for lo in -9i64..=9 {
        for hi in lo..=9 {
            out.push(r(lo, hi));
        }
    }
    let min = i64::from(i32::MIN);
    let max = i64::from(i32::MAX);
    out.extend([
        r(min, min),
        r(min, -1),
        r(min, min + 3),
        r(max - 3, max),
        r(-1, max),
    ]);
    out
}

/// Every value of a short range; the endpoints and a few inner values of
/// a long one.
fn values(x: Range) -> Vec<i64> {
    let (lo, hi) = (x.lo().expect("non-empty"), x.hi().expect("non-empty"));
    if hi - lo <= 40 {
        return (lo..=hi).collect();
    }
    [lo, lo + 1, hi - 1, hi, 0, -1, 1]
        .into_iter()
        .filter(|v| (lo..=hi).contains(v))
        .collect()
}

fn holds(got: Range, v: i64) -> bool {
    got.contains_all(Range::constant(W, v))
}

#[test]
fn or_signed_contains_every_result_and_never_widens_the_plain_form() {
    for a in ranges() {
        for b in ranges() {
            let got = a.or_signed(b);
            assert!(a.or(b).contains_all(got), "{a:?} | {b:?}: {got:?}");
            for x in values(a) {
                for y in values(b) {
                    // Cast: both are i32 values by construction.
                    let v = i64::from((x as i32) | (y as i32));
                    assert!(holds(got, v), "{x} | {y} = {v} not in {got:?}");
                }
            }
        }
    }
}

#[test]
fn xor_signed_contains_every_result_and_never_widens_the_plain_form() {
    for a in ranges() {
        for b in ranges() {
            let got = a.xor_signed(b);
            assert!(a.xor(b).contains_all(got), "{a:?} ^ {b:?}: {got:?}");
            for x in values(a) {
                for y in values(b) {
                    // Cast: both are i32 values by construction.
                    let v = i64::from((x as i32) ^ (y as i32));
                    assert!(holds(got, v), "{x} ^ {y} = {v} not in {got:?}");
                }
            }
        }
    }
}

#[test]
fn ushr_signed_contains_every_result_and_never_widens_the_plain_form() {
    for a in ranges() {
        for s in [0i64, 1, 2, 5, 31, 32, 33, -1] {
            let c = Range::constant(W, s);
            let got = a.ushr_signed(c);
            assert!(a.ushr(c).contains_all(got), "{a:?} >>> {s}: {got:?}");
            for x in values(a) {
                // Cast: JVMS masks an `int` shift count to five bits.
                let v = i64::from(((x as i32) as u32 >> ((s as u32) & 31)) as i32);
                assert!(holds(got, v), "{x} >>> {s} = {v} not in {got:?}");
            }
        }
    }
}

#[test]
fn the_signed_forms_are_sharper_where_the_plain_ones_give_up() {
    // `x | Integer.MIN_VALUE` for a small non-negative `x`.
    let min = i64::from(i32::MIN);
    let min_c = Range::constant(W, min);
    assert_eq!(r(0, 7).or_signed(min_c), r(min, -1));
    assert!(r(0, 7).or(min_c).is_top());
    // Two negative values xor to a non-negative one.
    assert_eq!(r(-8, -1).xor_signed(r(-4, -1)), r(0, 7));
    // `-1 >>> 1` is exactly `Integer.MAX_VALUE`.
    let one = Range::constant(W, 1);
    let max_c = Range::constant(W, i64::from(i32::MAX));
    assert_eq!(Range::constant(W, -1).ushr_signed(one), max_c);
    // The 64-bit width too: `-1L >>> 60` is 15.
    let l = IntWidth::W64;
    let sixty = Range::constant(W, 60);
    assert_eq!(Range::constant(l, -1).ushr_signed(sixty), Range::constant(l, 15));
}
