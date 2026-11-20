//! Round 13 wave 10, lane mega8: a way withdrawn for an UNLOADED receiver
//! is free for another class at once (`CRATONVM_JIT_IC_DEAD_WAYS_FREE_AT_ONCE`),
//! while a way withdrawn for any other reason still waits for its grace.
//! Every test holds a JIT execution token across the retirement and the
//! republication, the state of a thread spinning in a compiled loop: no
//! drain of another test can grace the retirement in between.
use super::*;
use crate::{ExecutableBuffer, JitCache};

/// Raw, even entry words (bit 0 is the context tag): published, never called.
fn word(class_id: u32) -> u64 {
    0x3000 + (u64::from(class_id) << 4)
}

fn ret_body() -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    CompiledMethod::new(buf)
}

fn published(cache: &JitCache, name: &str) -> Arc<CompiledMethod> {
    let cid = cratonvm_types::ClassId::new(0x130A);
    let class: Arc<str> = Arc::from("R13w10Mega8");
    let method: Arc<str> = Arc::from(name);
    let desc: Arc<str> = Arc::from("()V");
    cache.put(class.clone(), method.clone(), desc.clone(), cid, ret_body());
    cache.get(&class, &method, &desc, cid).expect("published")
}

/// Four inline ways (a site whose code probes them), classes 2 and 3
/// unloaded: with the switch on a new class takes a freed inline way at
/// once; with it off it overflows to the hashed set, as before.
#[test]
fn an_unloaded_receivers_inline_way_is_free_at_once() {
    let execution = crate::jit_execution_enter();
    for free in [true, false] {
        let pic = JitPICSlot::new();
        pic.note_inline_ways_probed();
        for c in 1..=4 {
            pic.install(c, "C", word(c), false, false);
        }
        let mut deferred: Vec<Arc<CompiledMethod>> = Vec::new();
        let mut free_at_once = Some(free);
        let retired = pic.retire_receiver_classes_deferring(
            &|c| c == 2 || c == 3,
            &mut free_at_once,
            &mut deferred,
        );
        assert_eq!(retired, 2, "free {free}");
        assert!(deferred.is_empty(), "raw words retain no owner");
        pic.install(9, "C", word(9), false, false);
        let inline = if free { Some((word(9), false)) } else { None };
        assert_eq!(pic.lookup(9), inline, "free {free}: class 9's inline way");
        assert_eq!(
            pic.lookup_megamorphic(9).is_some(),
            !free,
            "free {free}: class 9 overflowed to the hashed set"
        );
        assert!(
            !pic.is_megamorphic(),
            "free {free}: a retired way is not live"
        );
        // A way withdrawn for a recompiled callee (not an unload) still
        // waits for the grace, switch or not.
        assert!(pic.retire_entry(word(1) as usize));
        pic.install(11, "C", word(11), false, false);
        let took_way_zero = pic.way(0).is_some_and(|w| w.0 == 11);
        assert!(
            !took_way_zero,
            "free {free}: class 11 took a way before its grace"
        );
        pic.clear_entries();
    }
    crate::jit_execution_leave(execution);
}

/// A full hashed set of an unmarked site: a way freed by an unload takes
/// the set's next class at once with the switch on.
#[test]
fn an_unloaded_receivers_hashed_way_is_free_at_once() {
    let target = JitPICSlot::mega_base_index(1);
    let same_set: Vec<u32> = (1u32..100_000)
        .filter(|&c| JitPICSlot::mega_base_index(c) == target)
        .take(JIT_MEGA_WAYS + 1)
        .collect();
    assert_eq!(same_set.len(), JIT_MEGA_WAYS + 1);
    let (a, b, c) = (same_set[0], same_set[1], same_set[2]);
    let execution = crate::jit_execution_enter();
    for free in [true, false] {
        let pic = JitPICSlot::new();
        pic.install(a, "A", word(a), false, false);
        pic.install(b, "B", word(b), false, false);
        let mut deferred: Vec<Arc<CompiledMethod>> = Vec::new();
        let mut free_at_once = Some(free);
        pic.retire_receiver_classes_deferring(&|x| x == a, &mut free_at_once, &mut deferred);
        assert_eq!(pic.lookup_megamorphic(a), None);
        pic.install(c, "C", word(c), false, false);
        let expect = if free { Some((word(c), false)) } else { None };
        assert_eq!(pic.lookup_megamorphic(c), expect, "free {free}");
        assert_eq!(
            pic.lookup_megamorphic(b),
            Some((word(b), false)),
            "b survives"
        );
        pic.clear_entries();
    }
    crate::jit_execution_leave(execution);
}

/// The shared table: a full set whose head is unloaded takes a fifth key
/// at once with the switch on; a supersede's retirement still waits.
#[test]
fn an_unloaded_receivers_shared_way_is_free_at_once() {
    let cache = JitCache::new();
    let first = published(&cache, "dw-1");
    let second = published(&cache, "dw-2");
    let (e1, e2) = (first.entry_ptr() as u64, second.entry_ptr() as u64);
    let selector = 3;
    let set = MegaDispatchTable::set_base(1, selector);
    let same_set: Vec<u32> = (1u32..200_000)
        .filter(|&c| MegaDispatchTable::set_base(c, selector) == set)
        .take(MEGA_DISPATCH_WAYS + 2)
        .collect();
    assert_eq!(same_set.len(), MEGA_DISPATCH_WAYS + 2);
    let (head, last, fifth, sixth) = (
        same_set[0],
        same_set[MEGA_DISPATCH_WAYS - 1],
        same_set[MEGA_DISPATCH_WAYS],
        same_set[MEGA_DISPATCH_WAYS + 1],
    );
    let execution = crate::jit_execution_enter();
    for free in [true, false] {
        let mut table = MegaDispatchTable::new();
        table.free_dead_receiver_ways = free;
        for &c in &same_set[..MEGA_DISPATCH_WAYS - 1] {
            table.install(c, selector, e2, false, false);
        }
        table.install(last, selector, e1, false, false);
        assert_eq!(table.entries_used(), MEGA_DISPATCH_WAYS);
        table.retire_receiver_classes(&|id: u32| id == head);
        assert!(table.lookup(head, selector).is_none());
        table.install(fifth, selector, e2, false, false);
        assert_eq!(table.lookup(fifth, selector).is_some(), free, "free {free}");
        // A supersede-style retirement of a LIVE receiver's way is not
        // freed early.
        table.retire_entry(e1 as usize);
        assert!(table.lookup(last, selector).is_none());
        table.install(sixth, selector, e2, false, false);
        assert!(
            table.lookup(sixth, selector).is_none(),
            "free {free}: sixth before grace"
        );
        table.clear_entries();
    }
    crate::jit_execution_leave(execution);
    for body in [first, second] {
        crate::defer_jit_owner(Some(body));
    }
}
