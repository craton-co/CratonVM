use super::*;

/// A dirtied and freed block reused by a zeroed hand-out reads zero, as
/// does a fresh one; nothing is counted as skipped.
#[test]
fn gcg_g4og_a_zeroed_hand_out_reads_zero_after_reuse_and_skips_nothing() {
    let mut og = OldGen::new(1 << 20);
    let a = og.alloc(4096, 8).expect("room");
    // SAFETY: `a` is a 4096-byte block this generation handed out.
    unsafe { std::ptr::write_bytes(a, 0xA5, 4096) };
    // SAFETY: exactly the block `alloc(4096, 8)` returned.
    unsafe { og.free(a, 4096) };
    let b = og.alloc(4096, 8).expect("room");
    let c = og.alloc(8192, 8).expect("room");
    for (p, n) in [(b, 4096usize), (c, 8192)] {
        // SAFETY: `n` bytes at `p` are a live hand-out of this generation.
        let bytes = unsafe { std::slice::from_raw_parts(p, n) };
        assert!(
            bytes.iter().all(|&x| x == 0),
            "a zeroed hand-out reads zero"
        );
    }
    // gcg g6/og: the `zero_skipped_bytes` counter is gone with the skip.
}
