use super::{zero_on_reclaim_carried, zero_on_reclaim_claim};

#[test]
fn the_claim_is_the_free_bytes_the_pause_zeroed_and_nothing_else() {
    // free: [0,64) [128,256) [512,1024); zeroed dead: [32,160) [600,700).
    let free = [(0, 64), (128, 128), (512, 512)];
    let zeroed = [(32, 128), (600, 100)];
    assert_eq!(
        zero_on_reclaim_claim(&free, &zeroed, &[]),
        vec![(32, 32), (128, 32), (600, 100)]
    );
    // A free block the pause did not zero (an initial free block that held
    // a copy) is never claimed.
    assert!(zero_on_reclaim_claim(&[(0, 4096)], &[], &[]).is_empty());
    assert!(zero_on_reclaim_claim(&[], &[(0, 4096)], &[]).is_empty());
    // Touching zeroed pieces inside one free block merge into one span, so a
    // hand-out across the seam is still wholly inside one claimed span.
    assert_eq!(
        zero_on_reclaim_claim(&[(0, 256)], &[(0, 128), (128, 128)], &[]),
        vec![(0, 256)]
    );
}

/// t1 wave 11, lane GCB: the old set's bytes in an initial free block join
/// the claim minus the destination's written extent, and merge with a
/// zeroed dead span they touch.
#[test]
fn carried_bytes_are_the_old_claim_minus_every_written_destination_extent() {
    let base = 0x10_0000;
    // Old claim inside the initial free blocks: [1024,4096) [8192,9216).
    let prior = [(1024, 3072), (8192, 1024)];
    // One destination over [1024,4096) whose copies reached 2048; one over
    // [8192,9216) that took nothing (cursor == start).
    let dest = [
        (base + 1024, base + 2048, base + 4096),
        (base + 8192, base + 8192, base + 9216),
    ];
    let carried = zero_on_reclaim_carried(&prior, &dest, base);
    assert_eq!(carried, vec![(2048, 2048), (8192, 1024)]);
    // No old claim: nothing carried, whatever the destinations.
    assert!(zero_on_reclaim_carried(&[], &dest, base).is_empty());
    // The rebuild's free list: [0,512) dead and zeroed, [2048,4096) the
    // untouched remainder merged with the zeroed dead [4096,5120), and
    // [8192,9216). The copies [1024,2048) are kept, not free.
    let free = [(0, 512), (2048, 3072), (8192, 1024)];
    let zeroed = [(0, 512), (4096, 1024)];
    assert_eq!(
        zero_on_reclaim_claim(&free, &zeroed, &carried),
        vec![(0, 512), (2048, 3072), (8192, 1024)]
    );
    // Without the carry only the zeroed bytes are claimed.
    assert_eq!(
        zero_on_reclaim_claim(&free, &zeroed, &[]),
        vec![(0, 512), (4096, 1024)]
    );
    // A destination written to its end carries nothing of its block.
    let full = [(base + 1024, base + 4096, base + 4096)];
    assert!(zero_on_reclaim_carried(&[(1024, 3072)], &full, base).is_empty());
}
