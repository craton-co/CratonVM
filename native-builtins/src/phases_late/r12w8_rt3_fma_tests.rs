use super::{fma_f32_soft, fma_f64_soft, java_fma_f32, java_fma_f64};

fn same64(x: f64, y: f64) -> bool {
    (x.is_nan() && y.is_nan()) || x.to_bits() == y.to_bits()
}

fn same32(x: f32, y: f32) -> bool {
    (x.is_nan() && y.is_nan()) || x.to_bits() == y.to_bits()
}

/// The two double-rounding traps of `R12HuntFpEdges.fmas`, plus the cases
/// whose answer an unfused `a * b + c` gets wrong. Fails on a CRT whose
/// `fma`/`fmaf` double-rounds (the page's trap values), and the soft forms
/// are checked whatever the host CPU has.
#[test]
fn r12w8_fma_rounds_once_in_every_form() {
    let d = |x: u64| f64::from_bits(x);
    let f = |x: u32| f32::from_bits(x);
    // (1 + 2^-26)(1 + 2^-27) + 2^-200: the product is half-way, 2^-200 tips it.
    let a = d(0x3FF0_0000_0400_0000);
    let b = d(0x3FF0_0000_0200_0000);
    let c = d(0x3370_0000_0000_0000);
    // 0.1 * 10 - 1 is exactly 2^-54 (unfused: 0.0).
    // 1e308 * 10 + -inf is -inf (unfused: NaN).
    let cases64: [(f64, f64, f64, u64); 6] = [
        (a, b, c, 0x3FF0_0000_0600_0001),
        (0.1, 10.0, -1.0, 0x3C90_0000_0000_0000),
        (2.0, 3.0, 1.0, 7.0f64.to_bits()),
        (1e308, 10.0, f64::NEG_INFINITY, f64::NEG_INFINITY.to_bits()),
        (-1.0, 0.0, -0.0, 0x8000_0000_0000_0000),
        (1.0, -0.0, 0.0, 0),
    ];
    for (a, b, c, want) in cases64 {
        let want = f64::from_bits(want);
        assert!(
            same64(fma_f64_soft(a, b, c), want),
            "soft fma({a:e}, {b:e}, {c:e})"
        );
        assert!(
            same64(java_fma_f64(a, b, c), want),
            "java fma({a:e}, {b:e}, {c:e})"
        );
    }
    // (1 + 2^-12)^2 + 2^-80: half-way in f32, tipped by 2^-80.
    // 0.1f * 10 - 1 is exactly 2^-26.
    let cases32: [(f32, f32, f32, u32); 4] = [
        (f(0x3F80_0800), f(0x3F80_0800), f(0x1780_0000), 0x3F80_1001),
        (0.1, 10.0, -1.0, 0x3280_0000),
        (2.0, 3.0, 1.0, 7.0f32.to_bits()),
        (f32::INFINITY, 0.0, 1.0, f32::NAN.to_bits()),
    ];
    for (a, b, c, want) in cases32 {
        let want = f32::from_bits(want);
        assert!(
            same32(fma_f32_soft(a, b, c), want),
            "soft fmaf({a:e}, {b:e}, {c:e})"
        );
        assert!(
            same32(java_fma_f32(a, b, c), want),
            "java fmaf({a:e}, {b:e}, {c:e})"
        );
    }
}

fn edges64() -> Vec<f64> {
    vec![
        0.0,
        -0.0,
        1.0,
        -1.0,
        0.1,
        -3.0,
        1.0 + f64::EPSILON,
        f64::MIN_POSITIVE,
        -f64::MIN_POSITIVE,
        f64::from_bits(1),
        f64::from_bits(0x8000_0000_0000_0003),
        f64::from_bits(0x000F_FFFF_FFFF_FFFF),
        f64::MAX,
        -f64::MAX,
        2f64.powi(512),  // ~sqrt(MAX)
        2f64.powi(-511), // sqrt(MIN_POSITIVE)
        2f64.powi(-537),
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
    ]
}

struct XorShift(u64);
impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    /// A finite double with a random significand and an exponent in
    /// `[-span, span]` around 1.
    fn near_one(&mut self, span: u64) -> f64 {
        let r = self.next();
        let exp = 1023 - span + (r >> 52) % (2 * span + 1);
        f64::from_bits((r & (1u64 << 63)) | (exp << 52) | (self.next() & ((1u64 << 52) - 1)))
    }
}

/// The soft forms against the FMA3 instruction on every edge triple and on
/// random operands chosen to cancel, to underflow and to overflow. Runs
/// only where the host has FMA3 (the reference); elsewhere the fixed
/// cases above are the check.
#[cfg(target_arch = "x86_64")]
#[test]
fn r12w8_soft_fma_agrees_with_the_fma3_instruction() {
    if !is_x86_feature_detected!("fma") {
        return;
    }
    let hw64 = |a: f64, b: f64, c: f64| unsafe { super::fma_f64_fma3(a, b, c) };
    let hw32 = |a: f32, b: f32, c: f32| unsafe { super::fma_f32_fma3(a, b, c) };
    let edges = edges64();
    for &a in &edges {
        for &b in &edges {
            for &c in &edges {
                assert!(
                    same64(fma_f64_soft(a, b, c), hw64(a, b, c)),
                    "fma({a:e}, {b:e}, {c:e}): soft {:e}, fma3 {:e}",
                    fma_f64_soft(a, b, c),
                    hw64(a, b, c)
                );
                let (a32, b32, c32) = (a as f32, b as f32, c as f32);
                assert!(
                    same32(fma_f32_soft(a32, b32, c32), hw32(a32, b32, c32)),
                    "fmaf({a32:e}, {b32:e}, {c32:e})"
                );
            }
        }
    }
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    for i in 0..400_000u32 {
        let (a, b, c) = match i % 5 {
            0 => (
                f64::from_bits(rng.next()),
                f64::from_bits(rng.next()),
                f64::from_bits(rng.next()),
            ),
            1 => (rng.near_one(40), rng.near_one(40), rng.near_one(80)),
            2 => {
                // Cancellation: c close to -(a*b).
                let (a, b) = (rng.near_one(30), rng.near_one(30));
                let p = -(a * b);
                let c =
                    f64::from_bits(p.to_bits().wrapping_add(rng.next() % 5).wrapping_sub(2));
                (a, b, c)
            }
            3 => (rng.near_one(1000), rng.near_one(1000), rng.near_one(1022)),
            _ => {
                // Results near the subnormal range.
                let a = rng.near_one(530);
                let b = f64::from_bits(rng.next() & 0x800F_FFFF_FFFF_FFFF);
                (a, b, f64::from_bits(rng.next() & 0x801F_FFFF_FFFF_FFFF))
            }
        };
        assert!(
            same64(fma_f64_soft(a, b, c), hw64(a, b, c)),
            "fma({a:e}, {b:e}, {c:e}) [{:#x} {:#x} {:#x}]: soft {:#x}, fma3 {:#x}",
            a.to_bits(),
            b.to_bits(),
            c.to_bits(),
            fma_f64_soft(a, b, c).to_bits(),
            hw64(a, b, c).to_bits()
        );
        let (a32, b32) = (
            f32::from_bits(rng.next() as u32),
            f32::from_bits(rng.next() as u32),
        );
        let c32 = if i % 2 == 0 {
            f32::from_bits(rng.next() as u32)
        } else {
            // Near-cancelling and near-half-way addends.
            -(a32 * b32) * (1.0 + (rng.next() % 7) as f32 * f32::EPSILON)
        };
        assert!(
            same32(fma_f32_soft(a32, b32, c32), hw32(a32, b32, c32)),
            "fmaf({a32:e}, {b32:e}, {c32:e}) [{:#x} {:#x} {:#x}]",
            a32.to_bits(),
            b32.to_bits(),
            c32.to_bits()
        );
    }
}
