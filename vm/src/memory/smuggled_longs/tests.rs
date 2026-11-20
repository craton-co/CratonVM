use super::*;
use crate::memory::vm_heap::GcBackend;

fn heap() -> VmHeap {
    VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024)
}

#[test]
fn empty_registry_rejects() {
    // NOTE: `NONEMPTY` is process-global; this test only asserts the
    // contract for a value never minted anywhere in this heap.
    let h = heap();
    assert!(!is_minted(&h, 0xDEAD_BEE8));
}

/// i1 wave 9, lane L4: a heap's `forget_heap` on one thread must never
/// hide a mint another thread records meanwhile. Before `NONEMPTY` was
/// written under the lock, `forget_heap` took its "no heap has a mint"
/// verdict under the lock and stored `false` after releasing it, so a mint
/// landing in between was overwritten and `is_minted` answered `false`.
/// A race, so this is a stress run, not a proof; with the fix the
/// invariant holds on every interleaving.
#[test]
fn a_concurrent_forget_never_hides_a_fresh_mint() {
    const ROUNDS: usize = 20_000;
    let hidden = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        s.spawn(|| {
            let h = heap();
            let bits = 0x0000_7000_0000_6230u64;
            for _ in 0..ROUNDS {
                record_minted_long(&h, bits);
                if !is_minted(&h, bits) {
                    hidden.fetch_add(1, Ordering::Relaxed);
                }
                forget_heap(&h);
            }
        });
        s.spawn(|| {
            let h = heap();
            for _ in 0..ROUNDS {
                record_minted_long(&h, 0x0000_7000_0000_7230);
                forget_heap(&h);
            }
        });
    });
    assert_eq!(
        hidden.load(Ordering::Relaxed),
        0,
        "a mint recorded while another heap was being forgotten read back as unminted"
    );
}

#[test]
fn record_then_hit() {
    let h = heap();
    record_minted_long(&h, 0x0000_7000_0000_1230);
    assert!(is_minted(&h, 0x0000_7000_0000_1230));
    assert!(!is_minted(&h, 0x0000_7000_0000_1238));
}

/// A mint belongs to the heap it was minted against. Before the tables
/// were keyed, `is_minted` answered from one shared set, so any heap in
/// the process vouched for any other heap's handle.
#[test]
fn mint_is_not_visible_to_another_heap() {
    let a = heap();
    let b = heap();
    let bits = 0x0000_7000_0000_2230u64;
    record_minted_long(&a, bits);
    assert!(is_minted(&a, bits));
    assert!(
        !is_minted(&b, bits),
        "a handle minted against heap A must not be vouched for by heap B"
    );
}

/// The regression this fix exists for: a collection in heap B must not
/// unregister heap A's mints.
///
/// Under the old single set, B's `remap_and_sweep` asked
/// `B.is_object_address(a_handle)` — always `None` for a foreign
/// address — and dropped the entry. A's later moving collection then saw
/// `is_minted == false` and refused to rewrite the Java-visible `long`,
/// leaving it pointing into vacated from-space.
#[test]
fn foreign_collection_does_not_sweep_away_our_mint() {
    use cratonvm_types::ClassId;

    let a = heap();
    let b = heap();
    // A real, live object in A — so the entry is legitimately retained
    // by A's own sweep and only a foreign sweep could remove it.
    let obj = a.alloc_object(ClassId::new(0), 0);
    let bits = obj.as_ptr() as u64;
    record_minted_long(&a, bits);

    // B collects. Nothing of A's may be consulted or discarded.
    remap_and_sweep(&cratonvm_types::PointerMap::default(), &b);

    assert!(
        is_minted(&a, bits),
        "heap B's collection must not unregister heap A's mint"
    );
}

/// A value minted against heap A must not be rewritten by heap B's
/// relocation map.
#[test]
fn foreign_pointer_map_does_not_rewrite_our_mint() {
    use cratonvm_types::ClassId;

    let a = heap();
    let b = heap();
    let obj = a.alloc_object(ClassId::new(0), 0);
    let bits = obj.as_ptr() as u64;
    let elsewhere = 0x0000_7000_0000_3330u64;
    record_minted_long(&a, bits);

    // B moves an object that happens to sit at the same address.
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(bits as usize, elsewhere as usize);
    remap_and_sweep(&map, &b);

    assert!(
        is_minted(&a, bits),
        "A's entry must keep its own address after B's collection"
    );
    assert!(
        !is_minted(&a, elsewhere),
        "B's relocation must not have rewritten A's entry"
    );
}

/// gc-common w5-g: the concurrent sweep's freed-span drop removes exactly
/// the mints inside a freed span of THIS heap -- another heap's identical
/// value is untouched -- and never consults the heap (the values here are
/// not objects at all).
#[test]
fn drop_in_spans_drops_only_this_heaps_mints_inside_a_span() {
    let h = heap();
    let other = heap();
    let inside = 0x0000_7000_0000_4010u64;
    let edge = 0x0000_7000_0000_4000u64 + 0x100;
    let outside = 0x0000_7000_0000_5000u64;
    record_minted_long(&h, inside);
    record_minted_long(&h, edge);
    record_minted_long(&h, outside);
    record_minted_long(&other, inside);

    drop_in_spans(&h, &[(0x0000_7000_0000_4000usize, 0x100)]);

    assert!(!is_minted(&h, inside), "inside the freed span: dropped");
    assert!(is_minted(&h, edge), "one past the span: kept");
    assert!(is_minted(&h, outside));
    assert!(
        is_minted(&other, inside),
        "another heap's table is not touched"
    );
}

/// The owning heap's own sweep still works: a moved handle is relocated,
/// and a handle whose referent no longer parses as an object is dropped.
#[test]
fn own_sweep_relocates_and_drops() {
    use cratonvm_types::ClassId;

    let h = heap();
    let live = h.alloc_object(ClassId::new(0), 0);
    let moved = h.alloc_object(ClassId::new(0), 0);
    let live_bits = live.as_ptr() as u64;
    let moved_bits = moved.as_ptr() as u64;
    let dead_bits = 0x1_0000u64; // not an object address in this heap
    assert!(h.is_object_address(dead_bits as usize).is_none());

    record_minted_long(&h, live_bits);
    record_minted_long(&h, dead_bits);

    let mut map = cratonvm_types::PointerMap::default();
    map.insert(live_bits as usize, moved_bits as usize);
    remap_and_sweep(&map, &h);

    assert!(
        is_minted(&h, moved_bits),
        "a relocated handle must be registered at its new address"
    );
    assert!(
        !is_minted(&h, live_bits),
        "the pre-move address must no longer be registered"
    );
    assert!(
        !is_minted(&h, dead_bits),
        "an entry whose referent is not a live object must be dropped"
    );
}
