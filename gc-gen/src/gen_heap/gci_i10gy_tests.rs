use super::*;
use cratonvm_gc_core::gen_young_sizing::YoungGenSizing;

/// Every `-Xmx` constructor pins the young growth ceiling to the
/// semi-space it starts with. `VmHeap` builds a Generational heap only
/// through `with_capacity_and_young` (which is `with_capacity` when no
/// young flag is given), so Phase 6's expansion arm grows nothing on any
/// cycle, Cheney or pinned. If this fails, a constructor now leaves young
/// room to grow, and pinned5 L3 is live again: a pinned cycle would then
/// need `Arena::grow_in_place` (the i1/pn design on the page).
#[test]
fn no_xmx_constructor_leaves_young_room_to_grow() {
    const M: usize = 1024 * 1024;
    let sizings = [
        YoungGenSizing::default(),
        YoungGenSizing {
            xmn: Some(16 * M),
            ..YoungGenSizing::default()
        },
        YoungGenSizing {
            new_size: Some(8 * M),
            max_new_size: Some(64 * M),
            ..YoungGenSizing::default()
        },
        YoungGenSizing {
            new_ratio: Some(2),
            ..YoungGenSizing::default()
        },
    ];
    for total in [64 * M, 128 * M, 512 * M] {
        for young in &sizings {
            let h = GenerationalHeap::with_capacity_and_young(total, young);
            let from = h.young_semi_capacity();
            let to = h.young_to.lock().capacity();
            let old = h.old_gen_capacity();
            assert_eq!(from, to, "total={total} {young:?}: the semis start equal");
            assert!(
                h.max_young_semi_size <= from,
                "total={total} {young:?}: growth ceiling {} above the starting semi {from}",
                h.max_young_semi_size,
            );
            assert_eq!(
                h.young_semi_max(from, to, old),
                Some(from),
                "total={total} {young:?}: the published young ceiling is the starting semi",
            );
        }
    }
}
