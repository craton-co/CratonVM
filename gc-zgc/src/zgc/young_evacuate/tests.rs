use super::*;

const PAGE: usize = 2 * 1024 * 1024;
const BASE: usize = 0x1000_0000;

#[test]
fn the_low_bound_rounds_the_floor_up_to_the_page_grid() {
    assert_eq!(
        scope_low_bound(BASE, BASE, BASE + 3 * PAGE, PAGE),
        Some(BASE)
    );
    assert_eq!(
        scope_low_bound(BASE + 1, BASE, BASE + 3 * PAGE, PAGE),
        Some(BASE + PAGE)
    );
    assert_eq!(
        scope_low_bound(BASE + PAGE, BASE, BASE + 3 * PAGE, PAGE),
        Some(BASE + PAGE)
    );
    // No boundary below the cursor: nothing to scope.
    assert_eq!(
        scope_low_bound(BASE + PAGE + 8, BASE, BASE + 2 * PAGE, PAGE),
        None
    );
    // A floor below the arena is not a floor.
    assert_eq!(scope_low_bound(BASE - 8, BASE, BASE + 3 * PAGE, PAGE), None);
}

#[test]
fn a_page_is_admitted_only_at_or_above_lo_and_unblocked() {
    let s = ZYoungEvacScope {
        lo: BASE + 2 * PAGE,
        lo_page: 2,
        low_end: BASE + 6 * PAGE,
        blocked_pages: vec![3, 5],
        ..Default::default()
    };
    let admitted: Vec<u64> = (0..7).filter(|p| s.admits_page(*p)).collect();
    assert_eq!(admitted, vec![2, 4, 6]);
}

#[test]
fn a_straddler_blocks_the_pages_it_reaches_above_lo() {
    let lo = BASE + 2 * PAGE;
    let mut out = Vec::new();
    // Ends exactly at lo: no page.
    push_straddler_pages(lo - 64, lo, lo, BASE, PAGE, &mut out);
    assert!(out.is_empty());
    // Starts at lo: not a straddler (it is in the live list).
    push_straddler_pages(lo, lo + 64, lo, BASE, PAGE, &mut out);
    assert!(out.is_empty());
    push_straddler_pages(lo - 64, lo + 64, lo, BASE, PAGE, &mut out);
    assert_eq!(out, vec![2]);
    out.clear();
    push_straddler_pages(lo - 64, lo + PAGE + 8, lo, BASE, PAGE, &mut out);
    assert_eq!(out, vec![2, 3]);
}

/// Two bitmaps over one grid: word `w` covers `BASE + 512w`, bit `k` the
/// base `BASE + 512w + 8k`.
fn grid(starts: &[usize], olds: &[usize], words: usize) -> (Vec<u64>, Vec<u64>) {
    let mut s = vec![0u64; words];
    let mut o = vec![0u64; words];
    for &a in starts {
        let off = (a - BASE) >> 3;
        s[off / 64] |= 1u64 << (off % 64);
    }
    for &a in olds {
        let off = (a - BASE) >> 3;
        o[off / 64] |= 1u64 << (off % 64);
    }
    (s, o)
}

#[test]
fn the_word_walk_splits_young_bases_from_old_pages() {
    // Small pages so a few words span several of them.
    let page = 1024;
    let starts = [
        BASE,
        BASE + 8,
        BASE + 600,
        BASE + 1100,
        BASE + 2048,
        BASE + 3000,
    ];
    let olds = [BASE + 8, BASE + 1100, BASE + 3000];
    let (s, o) = grid(&starts, &olds, 8);
    let wb = |w: usize| BASE + (w << 9);
    let st = |w: usize| s[w];
    let ol = |w: usize| o[w];

    let mut pages = Vec::new();
    old_start_pages(
        0,
        8,
        &wb,
        &st,
        &ol,
        BASE,
        BASE + 4096,
        BASE,
        page,
        &mut pages,
    );
    finish_blocked(&mut pages);
    // Old bases at offsets 8 (page 0), 1100 (page 1), 3000 (page 2).
    assert_eq!(pages, vec![0, 1, 2]);
}

/// t1 wave 10 lane ZT3GN: the scoped pin lookup answers what
/// `resolve_interior_for_pin` answers for a base in the scope (exact hit,
/// interior, one past the end), `None` for a word in a dead run or below
/// the first base, and over-pins an unsizable base.
#[test]
fn the_scoped_pin_lookup_finds_the_containing_scope_base() {
    let live = [1000usize, 1100, 1300, 5000];
    // 1000: 64 bytes, 1100: 200 bytes, 1300: 16 bytes, 5000: unsizable.
    let size_of = |b: usize| match b {
        1000 => Some(64),
        1100 => Some(200),
        1300 => Some(16),
        _ => None,
    };
    assert_eq!(scoped_pin_base(999, &live, size_of), None);
    assert_eq!(scoped_pin_base(1000, &live, size_of), Some(1000));
    assert_eq!(scoped_pin_base(1063, &live, size_of), Some(1000));
    // One past the end names the object below.
    assert_eq!(scoped_pin_base(1064, &live, size_of), Some(1000));
    // A dead run between two survivors.
    assert_eq!(scoped_pin_base(1065, &live, size_of), None);
    assert_eq!(scoped_pin_base(1299, &live, size_of), Some(1100));
    assert_eq!(scoped_pin_base(1301, &live, size_of), Some(1300));
    assert_eq!(scoped_pin_base(1400, &live, size_of), None);
    // Unsizable: over-pin.
    assert_eq!(scoped_pin_base(90_000, &live, size_of), Some(5000));
    assert_eq!(scoped_pin_base(10, &[], size_of), None);
}

#[test]
fn the_extra_set_is_unique_sorted_and_disjoint_from_the_scope() {
    let lo = 1000;
    let hi = 2000;
    let young = vec![900, 10, 2500, 1500];
    let carded = vec![10, 300, 1999, 2000, 2500];
    let extra = merge_extra(young, carded, lo, hi);
    assert_eq!(extra, vec![10, 300, 900, 2000, 2500]);
}

#[test]
fn the_counters_book_scopes_refusals_and_moves() {
    let c = ZYoungEvacCounters::new(true);
    assert!(c.on.load(Ordering::Relaxed));
    let s = ZYoungEvacScope {
        live: vec![1, 2, 3],
        blocked_pages: vec![7],
        ..Default::default()
    };
    c.note_scope(&s);
    c.note_refusal(YE_REFUSE_FLOOR);
    c.note_extra_rewritten(4);
    c.note_moved(0);
    c.note_moved(5);
    let r = c.report();
    assert_eq!(
        r,
        ZYoungEvacReport {
            scopes: 1,
            refusals: 1,
            last_refusal: YE_REFUSE_FLOOR,
            live_scoped: 3,
            extra_rewritten: 4,
            blocked_pages: 1,
            moved: 5,
            cycles_moved: 1,
        }
    );
}
