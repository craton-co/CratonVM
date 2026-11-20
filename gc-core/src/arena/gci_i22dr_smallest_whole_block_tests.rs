use super::*;

const K: usize = 1024;

/// An arena whose low free list holds exactly `holes` (sizes, in order),
/// each fenced by a live 64-byte object so no two coalesce. Returns the
/// arena and each hole's offset.
fn arena_with_holes(holes: &[usize]) -> (Arena, Vec<usize>) {
    let mut a = Arena::new(8 * K * K);
    let base = a.base_ptr() as usize;
    let mut offs = Vec::new();
    for &size in holes {
        let p = a.alloc(size, 8).expect("hole") as usize;
        let _fence = a.alloc(64, 8).expect("fence");
        offs.push(p - base);
    }
    for (&off, &size) in offs.iter().zip(holes) {
        a.add_free_block(off, size);
    }
    (a, offs)
}

#[test]
fn gci_i22dr_refill_takes_the_smallest_whole_block_first() {
    let (mut a, offs) = arena_with_holes(&[K * K, 16 * K, 64 * K]);
    let base = a.base_ptr() as usize;
    let (p, n) = a.take_smallest_block_at_least(16 * K, K * K).expect("16K");
    assert_eq!((p as usize - base, n), (offs[1], 16 * K), "the smallest hole, whole");
    let (p, n) = a.take_smallest_block_at_least(16 * K, K * K).expect("64K");
    assert_eq!((p as usize - base, n), (offs[2], 64 * K));
    assert_eq!(a.largest_free_block(), K * K, "the large block stayed whole");
    let (p, n) = a.take_smallest_block_at_least(16 * K, K * K).expect("1M");
    assert_eq!((p as usize - base, n), (offs[0], K * K), "a block of exactly max is whole");
    assert_eq!(a.free_list_bytes(), 0);
    assert!(a.take_smallest_block_at_least(16 * K, K * K).is_none(), "nothing left");
}

#[test]
fn gci_i22dr_a_block_larger_than_max_gives_a_max_head() {
    let (mut a, offs) = arena_with_holes(&[K * K]);
    let base = a.base_ptr() as usize;
    let (p, n) = a.take_smallest_block_at_least(16 * K, 256 * K).expect("head");
    assert_eq!((p as usize - base, n), (offs[0], 256 * K));
    assert_eq!(a.free_list_bytes(), 768 * K, "the tail went back");
    assert_eq!(a.largest_free_block(), 768 * K);
}

#[test]
fn gci_i22dr_refusals_leave_the_free_list_untouched() {
    let (mut a, _) = arena_with_holes(&[8 * K, 12 * K]);
    let before = a.free_list_bytes();
    assert!(a.take_smallest_block_at_least(16 * K, K * K).is_none(), "no hole reaches min");
    assert!(a.take_smallest_block_at_least(64 * K, 16 * K).is_none(), "min > max");
    a.set_prefer_bump(true);
    assert!(a.take_smallest_block_at_least(4 * K, K * K).is_none(), "prefer_bump");
    a.set_prefer_bump(false);
    assert_eq!(a.free_list_bytes(), before);
    let (_, n) = a.take_smallest_block_at_least(9 * K, K * K).expect("12K");
    assert_eq!(n, 12 * K, "the 8K hole is below min; the 12K one is whole");
    assert_eq!(a.free_list_bytes(), 8 * K);
}

#[test]
fn gci_i22dr_the_small_tier_answers_by_size_too() {
    let (mut a, offs) = arena_with_holes(&[2048, 1024, 3000 & !7]);
    let base = a.base_ptr() as usize;
    let (p, n) = a.take_smallest_block_at_least(1000, 4096).expect("1024");
    assert_eq!((p as usize - base, n), (offs[1], 1024));
    let (p, n) = a.take_smallest_block_at_least(1000, 4096).expect("2048");
    assert_eq!((p as usize - base, n), (offs[0], 2048));
}
