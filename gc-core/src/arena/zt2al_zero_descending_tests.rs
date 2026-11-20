//! zt t2/al (`CRATONVM_ZGC_CHUNK_ZERO_DESCENDING`): `ZeroPlan::zero_descending`
//! writes exactly the bytes `ZeroPlan::zero` writes, whatever the plan and
//! the piece size -- only the store order differs.
use super::*;

const LEN: usize = 64 * 1024;
const MARK: u8 = 0x5C;

/// Run `zero` and `zero_descending(piece)` on two copies of the same dirty
/// buffer and return both results.
fn both(plan_of: impl Fn(usize) -> Option<ZeroPlan>, piece: usize) -> (Vec<u8>, Vec<u8>) {
    let mut a = vec![MARK; LEN];
    let mut b = vec![MARK; LEN];
    let pa = plan_of(a.as_ptr() as usize);
    let pb = plan_of(b.as_ptr() as usize);
    // SAFETY: each buffer is `LEN` writable bytes owned here, and each
    // plan's ranges are clamped inside its own buffer.
    unsafe {
        ZeroPlan::zero(pa.as_ref(), a.as_mut_ptr(), LEN);
        ZeroPlan::zero_descending(pb.as_ref(), b.as_mut_ptr(), LEN, piece);
    }
    (a, b)
}

#[test]
fn descending_zeroing_writes_exactly_what_zero_writes() {
    let plans: [&dyn Fn(usize) -> Option<ZeroPlan>; 4] = [
        &|_| None,
        // Known-zero at both ends and in the middle, one dirty stamp in it.
        &|base| {
            Some(ZeroPlan {
                known_zero: vec![
                    (base, base + 512),
                    (base + 20_000, base + 30_008),
                    (base + LEN - 64, base + LEN),
                ],
                pristine_bytes: 0,
                slide_bytes: 0,
                dirty_stamps: vec![base + 20_016],
            })
        },
        // The whole hand-out is known zero.
        &|base| {
            Some(ZeroPlan {
                known_zero: vec![(base, base + LEN)],
                pristine_bytes: LEN,
                slide_bytes: 0,
                dirty_stamps: Vec::new(),
            })
        },
        // Adjacent ranges and an unaligned gap.
        &|base| {
            Some(ZeroPlan {
                known_zero: vec![(base + 8, base + 4096), (base + 4096, base + 4100)],
                pristine_bytes: 0,
                slide_bytes: 4092,
                dirty_stamps: vec![base + 16, base + LEN - 8],
            })
        },
    ];
    for (i, plan_of) in plans.iter().enumerate() {
        for piece in [0, 8, 100, 4096, 16 * 1024, 1 << 20] {
            let (a, b) = both(plan_of, piece);
            assert!(a == b, "plan {i}, piece {piece}: the two zeroings differ");
        }
    }
    // The positive control: with no plan both zero everything.
    let (a, _) = both(&|_| None, 4096);
    assert!(a.iter().all(|&x| x == 0));
    // ...and a known-zero range is left untouched by both (it read MARK
    // here, which a real plan never allows, so the skip is visible).
    let (a, b) = both(plans[2], 4096);
    assert!(a.iter().all(|&x| x == MARK) && b.iter().all(|&x| x == MARK));
}
