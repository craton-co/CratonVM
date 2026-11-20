use super::*;

/// **Bump-first must skip a hole the default policy would have taken.**
///
/// This is the whole behavioural claim, and it is the one that makes a
/// generational nursery possible: an object placed in a hole BELOW the young
/// floor is in the old region and no young cycle reclaims it. Asserted by
/// address, against the same arena in both policies, because "it allocated
/// somewhere" is satisfied by either.
#[test]
fn bump_first_skips_a_hole_the_default_policy_takes() {
    // Two identical arenas, one hole each, one request each.
    let mut with_holes = |prefer_bump: bool| -> (usize, usize) {
        let mut a = Arena::new(64 * 1024);
        let base = a.base_ptr() as usize;
        let first = a.alloc(256, 8).expect("first") as usize;
        let _second = a.alloc(256, 8).expect("second") as usize;
        let cursor_before = a.used();
        // Free the FIRST block, making a hole below the cursor.
        a.add_free_block(first - base, 256);
        a.set_prefer_bump(prefer_bump);
        let next = a.alloc(256, 8).expect("third") as usize;
        (next - base, cursor_before)
    };

    let (default_off, hole_off) = with_holes(false);
    assert_eq!(
        default_off, 0,
        "the DEFAULT policy must reuse the hole at offset 0 -- if it does not, \
         this test is not comparing the two policies"
    );

    let (bump_off, cursor_before) = with_holes(true);
    assert!(
        bump_off >= cursor_before,
        "bump-first must allocate at or above the cursor ({cursor_before}), not \
         in the hole at 0 -- got {bump_off}. An object in that hole is below a \
         generational nursery floor and waits for a major"
    );
    let _ = hole_off;
}

/// **Bump-first must NOT be able to cause an OutOfMemoryError.**
///
/// It skips only the free-list FAST path; `alloc`'s post-bump retry searches
/// both tiers in full and then coalesces and searches again. So once the bump
/// tail is exhausted, every hole is still reachable. If that were not true,
/// turning the nursery on would turn a servable allocation into an OOM, which
/// is the one outcome a layout preference must never have -- on a
/// non-compacting heap the layout policy IS the OOM policy.
#[test]
fn bump_first_still_serves_from_the_free_list_once_the_tail_is_gone() {
    let mut a = Arena::new(16 * 1024);
    let base = a.base_ptr() as usize;
    a.set_prefer_bump(true);

    // Fill the arena by bumping until it refuses.
    let mut blocks: Vec<usize> = Vec::new();
    while let Some(p) = a.alloc(512, 8) {
        blocks.push(p as usize - base);
    }
    assert!(
        blocks.len() > 8,
        "the fixture must fill the arena: {}",
        blocks.len()
    );
    assert!(
        a.alloc(512, 8).is_none(),
        "the arena is full, so this must refuse"
    );
    assert_eq!(
        a.free_list_after_bump(),
        0,
        "nothing has fallen through yet"
    );

    // Free two blocks in the middle and ask again. The bump tail is gone, so
    // only the free list can serve it.
    a.add_free_block(blocks[3], 512);
    a.add_free_block(blocks[4], 512);
    let got = a
        .alloc(512, 8)
        .expect("bump-first must fall through to the free list, not OOM");
    let off = got as usize - base;
    assert!(
        off == blocks[3] || off == blocks[4],
        "it must come from one of the freed holes, got {off}"
    );
    assert_eq!(
        a.free_list_after_bump(),
        1,
        "and the fall-through must be COUNTED -- otherwise 'the nursery is \
         leaking into the old region' is an inference rather than a number"
    );
}

/// **A pre-merged run coalesces to the same free list as its members do.**
///
/// This is the claim the ZGC young sweep's run merge rests on
/// (`zgc_gen_dead_runs`): handing one span per run of adjacent dead objects
/// must leave the arena in the state that handing one span per object leaves
/// it in, because `coalesce_free_list` merges exactly those spans into
/// exactly that run a few statements later either way.
///
/// It is not self-evident, and the reason is `add_free_block`'s **region
/// routing**: a span goes to the small or the large tier by its own size, so
/// a merged run routes differently on the way IN than its members did. What
/// makes the result identical is that the coalescer rebuilds the list from
/// `low_blocks_sorted` rather than merging in place — so the routing on the
/// way in cannot survive it. Asserted rather than assumed, because "the
/// coalescer normalises it" is the entire safety argument for the merge and
/// it is one refactor away from stopping being true.
#[test]
fn an_arena_coalesces_pre_merged_runs_to_the_same_shape() {
    const N: usize = 16;
    const SZ: usize = 256;

    // Same arena, same bumped region, same total span -- the only difference
    // is how many calls it arrives in.
    let shape = |per_object: bool| -> (Vec<(usize, usize)>, usize, usize) {
        let mut a = Arena::new(64 * 1024);
        let base = a.base_ptr() as usize;
        let p = a.alloc(N * SZ, 8).expect("one bumped region") as usize;
        let off = p - base;
        if per_object {
            for i in 0..N {
                a.add_free_block(off + i * SZ, SZ);
            }
        } else {
            a.add_free_block(off, N * SZ);
        }
        a.coalesce_free_list();
        (
            a.free_blocks_sorted(),
            a.free_list_bytes(),
            a.largest_free_block(),
        )
    };

    let (blocks_each, bytes_each, largest_each) = shape(true);
    let (blocks_run, bytes_run, largest_run) = shape(false);

    assert_eq!(
        blocks_each, blocks_run,
        "the free list must have the same SPANS either way"
    );
    assert_eq!(bytes_each, bytes_run, "and the same total");
    assert_eq!(
        largest_each, largest_run,
        "and the same largest block -- which is the figure an allocation \
         decision is actually taken on"
    );
    assert_eq!(
        bytes_run,
        N * SZ,
        "and the fixture must have freed the whole region, or this compares \
         two empty lists and passes for nothing"
    );
}

/// **Off by default**, so no other collector's layout changes.
#[test]
fn bump_first_is_off_unless_asked_for() {
    let a = Arena::new(4 * 1024);
    assert!(!a.prefer_bump(), "the default policy must be unchanged");
}
