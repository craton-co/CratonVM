use super::{
    serial_eden_bytes, serial_eden_bytes_at, serial_eden_young_trigger, serial_survivor_reserve,
    serial_survivor_reserve_at, survivor_reserve_split, survivor_reserve_split_at,
    GenerationalHeap,
};
use cratonvm_gc_core::gen_young_sizing::YoungGenSizing;

const M: usize = 1 << 20;
const K: usize = 1 << 10;

/// The default ratio is the old arithmetic byte for byte; a typed ratio
/// is HotSpot Serial's `compute_space_boundaries`; a 0 reads as 1.
#[test]
fn gci_i45_sc_the_pure_sizes_follow_the_survivor_ratio() {
    for y in [0, 100 * K, 10 * M, 16 * M, 256 * M / 3, usize::MAX] {
        assert_eq!(serial_eden_bytes_at(y, 8), serial_eden_bytes(y), "young {y}");
    }
    for s in [0, 16 * K, 100 * K, 9 * M, 64 * M] {
        assert_eq!(serial_survivor_reserve_at(s, 8), serial_survivor_reserve(s), "semi {s}");
    }
    assert_eq!(
        survivor_reserve_split_at(9 * M, 9 * M, 600 * K, 100 * K, 8),
        survivor_reserve_split(9 * M, 9 * M, 600 * K, 100 * K)
    );
    // HotSpot Serial, -Xmx256m -XX:SurvivorRatio=4: Y 89456640,
    // S align_down(Y / 6, 64K) = 14876672, eden Y - 2S.
    assert_eq!(serial_eden_bytes_at(256 * M / 3, 4), 59_703_296);
    // -Xmx64m: Y 22347776, S 3670016.
    assert_eq!(serial_eden_bytes_at(64 * M / 3, 4), 15_007_744);
    assert_eq!(serial_eden_bytes_at(10 * M, 0), serial_eden_bytes_at(10 * M, 1));
    // The reserve: `semi / (SurvivorRatio + 1)`, aligned.
    assert_eq!(serial_survivor_reserve_at(10 * M, 4), 2 * M);
    assert_eq!(serial_survivor_reserve_at(10 * M, 0), 5 * M);
    // The split keeps its sums at any ratio.
    let s = survivor_reserve_split_at(10 * M, 10 * M, 600 * K, 100 * K, 4);
    assert_eq!(s.survivor_max, 2 * M);
    assert_eq!(s.eden_max + s.survivor_max, 10 * M);
    assert_eq!(s.eden_committed + s.survivor_committed, 10 * M);
}

// A typed ratio re-arms the Serial eden every construction-time arm
// derived (trigger, age clock, tenuring eden); `None`, 8 and the kill
// switch leave the heap exactly as built.
