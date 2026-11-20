//! Round 12 wave 7, lane mega6: a way or cell retired from key `K` takes
//! `K` again before its retirement is graced, and only `K`
//! (`CRATONVM_JIT_IC_SAME_KEY_REFILL`). Every test holds a JIT execution
//! token across the retirement and the republication: that is the state
//! of a mutator inside an OSR loop, and it keeps any other test's drain
//! from gracing the retirement in between (the process-wide proof needs
//! no execution in flight; the per-thread one is bounded by this thread's
//! last return to depth 0, which predates the stamp). The machine-code
//! side is executed in `runtime_lowering::r12w7_mega6_refill_probe_tests`.
use super::*;
use crate::{ExecutableBuffer, JitCache};

/// Raw, even entry words (bit 0 is the context tag): published, never called.
fn word(class_id: u32) -> u64 {
    0x1000 + (u64::from(class_id) << 4)
}

fn ret_body() -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    CompiledMethod::new(buf)
}

fn published(cache: &JitCache, name: &str) -> Arc<CompiledMethod> {
    let cid = cratonvm_types::ClassId::new(0x1207);
    let class: Arc<str> = Arc::from("R12w7Mega6");
    let method: Arc<str> = Arc::from(name);
    let desc: Arc<str> = Arc::from("()V");
    cache.put(class.clone(), method.clone(), desc.clone(), cid, ret_body());
    cache.get(&class, &method, &desc, cid).expect("published")
}

/// The four inline ways of a site whose code probes them, with class 2's
/// way retired by the supersede walk while a thread is in compiled code.
fn full_site_with_class_two_retired(refill: bool) -> JitPICSlot {
    let mut pic = JitPICSlot::new();
    pic.same_class_refill = refill;
    pic.note_inline_ways_probed();
    for c in 1..=4 {
        pic.install(c, "C", word(c), false, false);
    }
    assert_eq!(pic.way(1).map(|w| w.0), Some(2));
    assert!(pic.retire_entry(word(2) as usize), "the supersede walk");
    assert_eq!(pic.lookup(2), None);
    pic
}

#[test]
fn a_retired_inline_way_takes_its_own_class_back_before_the_grace() {
    let execution = crate::jit_execution_enter();
    let pic = full_site_with_class_two_retired(true);
    // Another class must not take it: a reader of class 2 may still sit
    // between its compare and its word load. It overflows to the hashed
    // set, and a retired way is not a live fifth class.
    pic.install(9, "C", word(9), false, false);
    assert_eq!(
        pic.lookup(9),
        None,
        "class 9 took the way class 2 was retired from"
    );
    assert_eq!(pic.lookup_megamorphic(9), Some((word(9), false)));
    assert!(!pic.is_megamorphic());
    // Class 2's recompiled body takes its own way back at once.
    let recompiled = word(2) + 0x100_0000;
    pic.install(2, "C", recompiled, false, false);
    assert_eq!(pic.lookup(2), Some((recompiled, false)));
    assert_eq!(pic.way(1).map(|w| (w.0, w.1)), Some((2, recompiled)));
    assert_eq!(pic.lookup_megamorphic(2), None, "inline only, no duplicate");

    // `CRATONVM_JIT_IC_SAME_KEY_REFILL=0`: the way stays parked until the
    // grace, so the recompiled body goes to the hashed set (the behaviour
    // that stranded every callee tiering up inside an OSR loop).
    let off = full_site_with_class_two_retired(false);
    off.install(2, "C", recompiled, false, false);
    assert_eq!(
        off.lookup(2),
        None,
        "the parked way was refilled with the switch off"
    );
    assert_eq!(off.lookup_megamorphic(2), Some((recompiled, false)));
    crate::jit_execution_leave(execution);
    pic.clear_entries();
    off.clear_entries();
}

#[test]
fn a_retired_hashed_way_takes_its_own_class_back_before_the_grace() {
    let target = JitPICSlot::mega_base_index(1);
    let same_set: Vec<u32> = (1u32..100_000)
        .filter(|&c| JitPICSlot::mega_base_index(c) == target)
        .take(JIT_MEGA_WAYS + 1)
        .collect();
    assert_eq!(same_set.len(), JIT_MEGA_WAYS + 1);
    let (a, b, c) = (same_set[0], same_set[1], same_set[2]);
    let execution = crate::jit_execution_enter();
    for refill in [true, false] {
        let mut pic = JitPICSlot::new();
        pic.same_class_refill = refill;
        // An unmarked site: the hashed set first.
        pic.install(a, "A", word(a), false, false);
        pic.install(b, "B", word(b), false, false);
        assert_eq!(pic.lookup_megamorphic(a), Some((word(a), false)));
        assert!(pic.retire_entry(word(a) as usize));
        pic.install(c, "C", word(c), false, false);
        assert_eq!(
            pic.lookup_megamorphic(c),
            None,
            "another class took a retired way"
        );
        let recompiled = word(a) + 0x100_0000;
        pic.install(a, "A", recompiled, false, false);
        let expect = if refill {
            Some((recompiled, false))
        } else {
            None
        };
        assert_eq!(pic.lookup_megamorphic(a), expect, "refill {refill}");
        pic.clear_entries();
    }
    crate::jit_execution_leave(execution);
}

/// The class cell (one per class and column: nowhere else to go) and a
/// shared way, retired by a supersede, take their own key back at once;
/// another key waits for the grace.
#[test]
fn a_retired_cell_and_shared_way_take_their_own_key_back_before_the_grace() {
    let cache = JitCache::new();
    let first = published(&cache, "rf-c1");
    let second = published(&cache, "rf-c2");
    let (e1, e2) = (first.entry_ptr() as u64, second.entry_ptr() as u64);
    let execution = crate::jit_execution_enter();
    for refill in [true, false] {
        let mut table = MegaDispatchTable::new();
        table.same_key_refill = refill;
        table.install_with_class_slot(5, 1, Some((3, 8)), e1, false, false);
        assert_eq!(table.lookup_class_cell(5, 1, 3), Some((e1, false)));
        table.retire_entry(e1 as usize);
        assert_eq!(table.lookup_class_cell(5, 1, 3), None);
        // Another selector at the same column of the same class.
        table.install_with_class_slot(5, 2, Some((3, 8)), e2, false, false);
        assert_eq!(
            table.lookup_class_cell(5, 2, 3),
            None,
            "another key took the cell"
        );
        // The same key, with the recompiled body.
        table.install_with_class_slot(5, 1, Some((3, 8)), e2, true, false);
        let expect = if refill { Some((e2, true)) } else { None };
        assert_eq!(table.lookup_class_cell(5, 1, 3), expect, "refill {refill}");
        assert!(
            table.lookup(5, 1).is_some(),
            "a free hashed way took it either way"
        );

        // A full shared set: a retired way takes its own key back, and
        // not a fifth key.
        let selector = 7;
        let set = MegaDispatchTable::set_base(1, selector);
        let same_set: Vec<u32> = (1u32..200_000)
            .filter(|&c| MegaDispatchTable::set_base(c, selector) == set)
            .take(MEGA_DISPATCH_WAYS + 1)
            .collect();
        assert_eq!(same_set.len(), MEGA_DISPATCH_WAYS + 1);
        let (head, fifth) = (same_set[0], same_set[MEGA_DISPATCH_WAYS]);
        table.install(head, selector, e1, false, false);
        for &c in &same_set[1..MEGA_DISPATCH_WAYS] {
            table.install(c, selector, e2, false, false);
        }
        table.retire_entry(e1 as usize);
        table.install(fifth, selector, e2, false, false);
        assert!(
            table.lookup(fifth, selector).is_none(),
            "a fifth key took a retired way"
        );
        table.install(head, selector, e2, false, false);
        assert_eq!(
            table.lookup(head, selector).is_some(),
            refill,
            "refill {refill}: the head's own way"
        );
        table.clear_entries();
    }
    crate::jit_execution_leave(execution);
    for b in [first, second] {
        crate::defer_jit_owner(Some(b));
    }
}

/// A publication of a body a supersede displaced while the publisher was
/// resolving it is withdrawn under the writer lock, from the cell and the
/// hashed way alike; the current body is published as ever.
#[test]
fn a_superseded_body_is_rolled_back_out_of_the_table() {
    let cache = JitCache::new();
    let old = published(&cache, "rf-s1");
    let new = published(&cache, "rf-s1");
    assert!(old.superseded.load(std::sync::atomic::Ordering::Acquire));
    assert!(!Arc::ptr_eq(&old, &new));
    let (e_old, e_new) = (old.entry_ptr() as u64, new.entry_ptr() as u64);
    for rollback in [true, false] {
        let mut table = MegaDispatchTable::new();
        table.superseded_rollback = rollback;
        table.install_with_class_slot(5, 1, Some((1, 2)), e_old, false, false);
        let kept = if rollback { None } else { Some((e_old, false)) };
        assert_eq!(
            table.lookup_class_cell(5, 1, 1),
            kept,
            "rollback {rollback}"
        );
        assert_eq!(
            table.lookup(5, 1).is_some(),
            !rollback,
            "rollback {rollback}"
        );
        table.install_with_class_slot(6, 1, Some((1, 2)), e_new, false, false);
        assert_eq!(table.lookup_class_cell(6, 1, 1), Some((e_new, false)));
        table.clear_entries();
    }
    for b in [old, new] {
        crate::defer_jit_owner(Some(b));
    }
}

/// An unloaded class's cell array is buried, not freed, until the stamp
/// taken after its row was zeroed is graced; then its columns go back to
/// the budget (`r12w4-mega3-shared-table-never-reclaims-FIXED-20260928.md`,
/// item 1). With the switch off it stays, as before.
#[test]
fn an_unloaded_class_array_is_freed_once_graced() {
    let cache = JitCache::new();
    let body = published(&cache, "rf-u1");
    let entry = body.entry_ptr() as u64;
    let execution = crate::jit_execution_enter();
    for reclaim in [true, false] {
        let mut table = MegaDispatchTable::new();
        table.reclaim_dead_arrays = reclaim;
        for class_id in 10..20u32 {
            table.install_with_class_slot(class_id, 1, Some((2, 4)), entry, false, false);
        }
        assert_eq!(table.writer.lock().columns, 40);
        table.retire_receiver_classes(&|id: u32| id < 15);
        {
            let state = table.writer.lock();
            let buried = if reclaim { 5 } else { 0 };
            assert_eq!(state.classes.len(), 10 - buried, "reclaim {reclaim}");
            assert_eq!(state.graveyard.len(), buried, "buried, not freed");
            assert_eq!(state.columns, 40, "nothing freed before the grace");
        }
        assert_eq!(table.lookup_class_cell(12, 1, 2), None);
        assert_eq!(table.lookup_class_cell(17, 1, 2), Some((entry, false)));
        {
            let mut state = table.writer.lock();
            MegaDispatchTable::sweep_graveyard_locked(&mut state, |_| false);
            assert_eq!(state.graveyard.len(), if reclaim { 5 } else { 0 });
            MegaDispatchTable::sweep_graveyard_locked(&mut state, |_| true);
            assert!(state.graveyard.is_empty());
            assert_eq!(state.columns, if reclaim { 20 } else { 40 });
        }
        table.clear_entries();
    }
    crate::jit_execution_leave(execution);
    crate::defer_jit_owner(Some(body));
}
