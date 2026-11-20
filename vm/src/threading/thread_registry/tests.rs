use super::*;

#[test]
fn next_thread_id_increments() {
    let registry = ThreadRegistry::new();
    let id1 = registry.next_thread_id();
    let id2 = registry.next_thread_id();
    assert_eq!(id1, ThreadId(1));
    assert_eq!(id2, ThreadId(2));
}

/// zt t15/pl6: the JMM roster behind `java.threads.*` lists what
/// `ThreadMXBean`'s walk lists -- alive entries with a mirror that are not
/// virtual threads -- at every edge, and the four figures follow the
/// bean's formulas (an uncounted listed thread adds to `started` and
/// `peak`; the ended `main` is left out).
#[test]
fn zt15pl6_the_jmm_roster_lists_what_the_bean_walk_lists() {
    let registry = ThreadRegistry::new();
    let mut backing = [[0u64; 2]; 5];
    let mut mirror = |i: usize| {
        let ptr = backing[i].as_mut_ptr() as *mut u8;
        // SAFETY: a live, 8-byte-aligned, non-null allocation, never read.
        unsafe { ObjectRef::from_raw(ptr) }
    };
    let (m0, m1, m2, m3, m4) = (mirror(0), mirror(1), mirror(2), mirror(3), mirror(4));
    let counts = |r: &ThreadRegistry| {
        let c = r.jmm_perf_thread_counts();
        (c.live, c.daemon, c.peak, c.started)
    };
    assert_eq!(counts(&registry), (0, 0, 0, 0));
    // main: mirror set after the registration, counted.
    registry.register(ThreadId(0), "main", None);
    registry.note_jmm_thread_start(ThreadId(0));
    // Counted with no mirror yet: the bean's difference is -1, so its
    // `peak` / `started` read the live count until the mirror exists.
    assert_eq!(counts(&registry), (0, 0, 0, 0), "counted, not listed yet");
    registry.set_java_thread_obj(ThreadId(0), m0);
    assert_eq!(counts(&registry), (1, 0, 1, 1));
    // A started daemon platform thread.
    registry.register_starting_with_daemon(ThreadId(1), "d", Some(m1), true);
    registry.note_jmm_thread_start(ThreadId(1));
    assert_eq!(counts(&registry), (2, 1, 2, 2));
    // A virtual thread: never listed.
    registry.register_starting_virtual(ThreadId(2), "v", m2);
    assert_eq!(counts(&registry), (2, 1, 2, 2));
    // A VM-made thread with a mirror the JMM did not count: listed, and
    // added to `started` / `peak` as the bean adds it.
    registry.register_with_daemon(ThreadId(3), "vm-made", None, true);
    registry.set_java_thread_obj(ThreadId(3), m3);
    assert_eq!(counts(&registry), (3, 2, 3, 3));
    // A daemon flag flip of a listed entry.
    assert!(registry.set_daemon(ThreadId(3), false));
    assert_eq!(counts(&registry), (3, 1, 3, 3));
    // A withdrawn mirror unlists; re-set lists again (not twice).
    registry.clear_java_thread_obj_if(ThreadId(3), m3);
    assert_eq!(counts(&registry).0, 2);
    registry.set_java_thread_obj(ThreadId(3), m3);
    registry.set_java_thread_obj(ThreadId(3), m4);
    assert_eq!(counts(&registry).0, 3);
    // Deaths leave once; a second `mark_dead` changes nothing.
    registry.mark_dead(ThreadId(1));
    registry.mark_dead(ThreadId(1));
    registry.mark_dead(ThreadId(2));
    assert_eq!(counts(&registry), (2, 0, 3, 3), "peak and started stay");
    // The ended `main` is left out until it carries `DestroyJavaVM`.
    registry.mark_java_side_ended(ThreadId(0));
    assert_eq!(counts(&registry).0, 1);
    registry.mark_destroy_java_vm_listed();
    assert_eq!(counts(&registry).0, 2);
}

/// Fabricated, never-dereferenced heap addresses. Everything the owned-
/// synchronizer index does with an `ObjectRef` is take `as_ptr()` and use
/// it as a map key / identity comparison, so these never need to be real
/// objects — and making them real would mean booting a heap for a test
/// about bookkeeping.
fn fake_synchronizer(addr: usize) -> ObjectRef {
    // SAFETY: this ref is only ever compared and hashed by address; no
    // code under test reads through it.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// JIT round 19 wave 9, lane gc9 (`CRATONVM_JMX_HELD_NATIVE_FRAME_MONITORS`):
/// a pause's publish on behalf of a held thread lands at the trace
/// positions it was given, is read back for that trace length, and sets
/// the OWNER's published flag (so the owner's next empty publish
/// withdraws it); a thread with no published `JvmThread` gets nothing.
#[test]
fn a_held_threads_frame_monitors_are_published_for_its_stored_trace() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(42);
    registry.register(tid, "gc9-held", None);
    let thread = Box::new(crate::threading::jvm_thread::JvmThread::new(
        tid, "gc9-held",
    ));
    registry.set_jvm_thread_addr(
        tid,
        &*thread as *const crate::threading::jvm_thread::JvmThread as usize,
    );
    let lock_f = fake_synchronizer(0x60_000);
    let lock_g = fake_synchronizer(0x60_040);
    registry.complete_jmx_monitor_enter(tid, lock_f);
    registry.complete_jmx_monitor_enter(tid, lock_g);
    let frame = |method: &str| {
        crate::runtime::frame::Frame::new(
            crate::classloading::ClassId::new(3),
            "probe/T".to_string(),
            method.to_string(),
            "()V".to_string(),
            None,
            vec![0; 16],
            vec![],
            2,
            2,
            &[],
        )
    };
    let mut frames = vec![frame("run"), frame("tCold")];
    frames[1].held_monitors.push(lock_f);
    // Two frames at entries 1 and 2 (a served row below them), the
    // native on top: four entries.
    registry.publish_held_thread_frame_monitors(tid, &frames, &[1, 2], 4, &[]);
    assert!(thread.jmx_frame_monitors_published.load(Ordering::Relaxed));
    assert_eq!(registry.jmx_frame_monitors(tid, 4), Some(vec![(2, lock_f)]));
    // Another length: the publication does not describe it.
    assert_eq!(registry.jmx_frame_monitors(tid, 3), None);
    // No `JvmThread` address: nothing is published.
    let other = ThreadId(43);
    registry.register(other, "gc9-unknown", None);
    registry.complete_jmx_monitor_enter(other, lock_f);
    registry.publish_held_thread_frame_monitors(other, &frames, &[1, 2], 4, &[]);
    assert_eq!(registry.jmx_frame_monitors(other, 4), None);
}

/// JIT round 19 wave 11, lane gc11 (GC920-4,
/// `CRATONVM_JMX_HELD_NATIVE_COMPILED_HOLDS`): a held thread whose block
/// lock sits in a COMPILED activation (`tCold` at trace index 2, above the
/// interpreted `run` at 1, the native at 3) gets that hold published at
/// the activation's index; a candidate word that is no lock-stack object
/// (index 3) and one past the trace are dropped; the interpreted frame's
/// own hold still lands at its position.
#[test]
fn a_held_threads_compiled_holds_land_at_their_trace_positions() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(44);
    registry.register(tid, "gc11-held", None);
    let thread = Box::new(crate::threading::jvm_thread::JvmThread::new(
        tid,
        "gc11-held",
    ));
    registry.set_jvm_thread_addr(
        tid,
        &*thread as *const crate::threading::jvm_thread::JvmThread as usize,
    );
    let lock_e = fake_synchronizer(0x61_000);
    let lock_f = fake_synchronizer(0x61_040);
    let lock_g = fake_synchronizer(0x61_080);
    registry.complete_jmx_monitor_enter(tid, lock_e);
    registry.complete_jmx_monitor_enter(tid, lock_f);
    registry.complete_jmx_monitor_enter(tid, lock_g);
    let mut frames = vec![crate::runtime::frame::Frame::new(
        crate::classloading::ClassId::new(3),
        "probe/T".to_string(),
        "run".to_string(),
        "()V".to_string(),
        None,
        vec![0; 16],
        vec![],
        2,
        2,
        &[],
    )];
    frames[0].held_monitors.push(lock_e);
    let compiled = [
        (2u32, lock_f.as_ptr() as usize),
        (3, 0x61_0c0),
        (9, lock_g.as_ptr() as usize),
    ];
    registry.publish_held_thread_frame_monitors(tid, &frames, &[1], 4, &compiled);
    assert!(thread.jmx_frame_monitors_published.load(Ordering::Relaxed));
    assert_eq!(
        registry.jmx_frame_monitors(tid, 4),
        Some(vec![(2, lock_f), (1, lock_e)])
    );
    // Off (no candidates): the interpreted frame's hold alone, as in wave 9.
    registry.publish_held_thread_frame_monitors(tid, &frames, &[1], 4, &[]);
    assert_eq!(registry.jmx_frame_monitors(tid, 4), Some(vec![(1, lock_e)]));
}

/// JIT round 19 wave 12, lane gc12 (`CRATONVM_JMX_RUNNING_LOCKS_AT_TRACE`):
/// the pause's record keeps the lock stack of the trace's instant; the
/// attribution resolves against it even after the thread took another
/// monitor (where the current-stack read refuses), and a record is current
/// only within one pause end and no moving collection.
#[test]
fn a_trace_lock_record_pairs_the_attribution_with_its_own_instant() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(45);
    registry.register(tid, "gc12-running", None);
    let thread = Box::new(crate::threading::jvm_thread::JvmThread::new(
        tid,
        "gc12-running",
    ));
    registry.set_jvm_thread_addr(
        tid,
        &*thread as *const crate::threading::jvm_thread::JvmThread as usize,
    );
    let lock_a = fake_synchronizer(0x62_000);
    let callee = fake_synchronizer(0x62_040);
    registry.complete_jmx_monitor_enter(tid, lock_a);
    let mut frames = vec![crate::runtime::frame::Frame::new(
        crate::classloading::ClassId::new(3),
        "probe/T".to_string(),
        "drive".to_string(),
        "()V".to_string(),
        None,
        vec![0; 16],
        vec![],
        2,
        2,
        &[],
    )];
    frames[0].held_monitors.push(lock_a);
    // The pause: trace of 3 entries, `drive` at entry 1, lock stack [a].
    registry.publish_held_thread_frame_monitors(tid, &frames, &[1], 3, &[]);
    registry.record_trace_locks(tid, 3, 7, 2);
    let record = registry.trace_locks(tid).expect("recorded");
    assert_eq!(record.keys, vec![lock_a.as_ptr() as usize]);
    assert_eq!(record.trace_len, 3);
    // The thread runs on and takes the callee's monitor.
    registry.complete_jmx_monitor_enter(tid, callee);
    assert_eq!(
        registry.jmx_frame_monitors(tid, 3),
        None,
        "the current stack grew"
    );
    let keys = [lock_a];
    assert_eq!(
        registry.jmx_frame_monitors_against(tid, 3, &keys),
        Some(vec![(1, lock_a)])
    );
    assert_eq!(registry.jmx_frame_monitors_against(tid, 4, &keys), None);
    // Current at the publishing pause's own end only, and with no move.
    assert!(record.current_at(7, 2));
    assert!(record.current_at(8, 2));
    assert!(!record.current_at(9, 2));
    assert!(!record.current_at(8, 3));
}

/// gcg g6/jn: a conservatively deposited snapshot contributes every word
/// to the guard's exclusion; a precise one contributes none.
#[test]
fn gcg_g6jn_conservative_deposit_words_only_from_conservative_snapshots() {
    let snap: Vec<ObjectRef> = [0x50_000usize, 0x50_040, 0x50_080]
        .iter()
        .map(|&a| fake_synchronizer(a))
        .collect();
    let mut out = vec![0x10];
    conservative_deposit_words(false, &snap, &[], &mut out);
    assert_eq!(out, vec![0x10], "a precise deposit adds nothing");
    conservative_deposit_words(true, &snap, &[], &mut out);
    assert_eq!(out, vec![0x10, 0x50_000, 0x50_040, 0x50_080]);
    conservative_deposit_words(true, &[], &[], &mut out);
    assert_eq!(out.len(), 4, "an empty snapshot adds nothing");
}

/// gch h1/yp: a blocked thread's live locals and operand slots stay under
/// the unrecorded-young-root guard; its dead locals and every word no
/// slot recorded (the conservative probe's) are excluded.
#[test]
fn gch_h1yp_live_slot_words_of_a_conservative_deposit_stay_guarded() {
    use crate::threading::jvm_thread::SlotOrigin;
    let so = |idx: u32, is_stack: bool, live: bool, cur: usize| SlotOrigin {
        frame: 0,
        idx,
        is_stack,
        live,
        orig: cur,
        cur,
    };
    let origins = [
        so(0, false, true, 0x60_080),  // a live local: precise
        so(1, false, false, 0x60_000), // a dead local: conservative
        so(0, true, true, 0x60_040),   // an operand slot: precise
        so(2, false, true, 0x60_080),  // the same value twice
    ];
    let precise = precise_slot_origin_words(&origins);
    assert_eq!(
        precise,
        vec![0x60_040, 0x60_080],
        "sorted, deduplicated, live only"
    );
    let snap: Vec<ObjectRef> = [0x60_000usize, 0x60_040, 0x60_080, 0x60_0c0]
        .iter()
        .map(|&a| fake_synchronizer(a))
        .collect();
    let mut out = Vec::new();
    conservative_deposit_words(true, &snap, &precise, &mut out);
    assert_eq!(
        out,
        vec![0x60_000, 0x60_0c0],
        "the dead local and the unrecorded word are excluded; the live ones are not"
    );
    out.clear();
    conservative_deposit_words(false, &snap, &precise, &mut out);
    assert!(out.is_empty(), "a precise deposit still excludes nothing");
    assert!(precise_slot_origin_words(&[]).is_empty());
}

/// gch h3/yc: a PARKED thread's conservative safepoint deposit keeps the
/// precise words it recorded (`GcBlockState::precise_deposit_words`, in
/// deposit order, duplicates included) under the guard; only the
/// conservative probe's extras are excluded. An empty record is the h2
/// behaviour: the whole snapshot is excluded.
#[test]
fn gch_h3yc_a_parked_deposits_precise_words_stay_guarded() {
    let recorded = [0x70_080usize, 0x70_000, 0x70_080];
    let precise = sorted_precise_words(&recorded);
    assert_eq!(precise, vec![0x70_000, 0x70_080], "sorted and deduplicated");
    let snap: Vec<ObjectRef> = [0x70_000usize, 0x70_040, 0x70_080, 0x70_0c0]
        .iter()
        .map(|&a| fake_synchronizer(a))
        .collect();
    let mut out = Vec::new();
    conservative_deposit_words(true, &snap, &precise, &mut out);
    assert_eq!(
        out,
        vec![0x70_040, 0x70_0c0],
        "only the probe's extras are excluded"
    );
    out.clear();
    conservative_deposit_words(true, &snap, &sorted_precise_words(&[]), &mut out);
    assert_eq!(out.len(), 4, "no record: the whole snapshot, as before");
    // The record lives on the thread's block state and starts empty.
    let block = crate::threading::jvm_thread::GcBlockState::new();
    assert!(block.precise_deposit_words.lock().is_empty());
}

/// gcg g4/jn: the unblocked-thread `[blockgc]` line counts every snapshot
/// entry the map moves and lists the first `max` in snapshot order;
/// unmoved entries and an empty map count nothing.
#[test]
fn gcg_g4jn_moved_snapshot_entries_counts_and_bounds() {
    let snapshot: Vec<ObjectRef> = (0..12usize)
        .map(|i| fake_synchronizer(0x40_000 + i * 0x40))
        .collect();
    let mut pm = cratonvm_types::PointerMap::default();
    for i in (0..12usize).filter(|i| i % 3 != 0) {
        pm.insert(0x40_000 + i * 0x40, 0x80_000 + i * 0x40);
    }
    let (moved, first) = moved_snapshot_entries(&snapshot, &pm, 3);
    assert_eq!(moved, 8);
    assert_eq!(first, vec![0x40_040, 0x40_080, 0x40_100]);
    let (none, empty) =
        moved_snapshot_entries(&snapshot, &cratonvm_types::PointerMap::default(), 8);
    assert_eq!((none, empty.len()), (0, 0));
    let (all, capped) = moved_snapshot_entries(&snapshot, &pm, UNBLOCKED_MOVED_REPORT_ADDRS);
    assert_eq!((all, capped.len()), (8, UNBLOCKED_MOVED_REPORT_ADDRS));
}

fn owned_synchronizers(registry: &ThreadRegistry, tid: ThreadId) -> Vec<usize> {
    registry
        .jmx_lock_snapshot(tid)
        .map(|snapshot| snapshot.3.iter().map(|o| o.as_ptr() as usize).collect())
        .unwrap_or_default()
}

/// [`ThreadRegistry::set_jmx_owned_synchronizer_own`] must record exactly
/// what the general form records, for **every** (previous owner, new owner)
/// combination — not only the two AQS actually generates.
///
/// The fast form exists because the general one reads the registry-wide
/// `RwLock` to find a list the calling thread could have held a handle to.
/// That is only sound if the two are indistinguishable in their effect, and
/// the interesting combinations are the ones AQS does *not* produce: a
/// steal from a peer, and a release of a lock a peer owns. Both take the
/// fast form's fallback arm, and a divergence there would surface as a
/// `ThreadInfo.getLockedSynchronizers()` that names two owners for one lock
/// — silently, and only under JMX inspection.
///
/// Driven as a table so a future arm cannot be added without a case.
#[test]
fn the_own_handle_transition_records_what_the_general_one_records() {
    // (label, previous owner, new owner)
    let cases: [(&str, Option<ThreadId>, Option<ThreadId>); 6] = [
        ("first acquire by self", None, Some(ThreadId(1))),
        (
            "reentrant acquire by self",
            Some(ThreadId(1)),
            Some(ThreadId(1)),
        ),
        ("release by self", Some(ThreadId(1)), None),
        ("steal from a peer", Some(ThreadId(2)), Some(ThreadId(1))),
        ("release of a peer's lock", Some(ThreadId(2)), None),
        ("release of an unowned lock", None, None),
    ];
    let (me, peer) = (ThreadId(1), ThreadId(2));
    let lock = fake_synchronizer(0x2000);

    for (label, previous, owner) in cases {
        // Two registries, seeded identically, then driven one call apart.
        let general = ThreadRegistry::new();
        let own = ThreadRegistry::new();
        for registry in [&general, &own] {
            registry.register(me, "me", None);
            registry.register(peer, "peer", None);
            if let Some(previous) = previous {
                registry.set_jmx_owned_synchronizer(Some(previous), lock);
            }
        }

        general.set_jmx_owned_synchronizer(owner, lock);
        let own_list = own
            .jmx_locked_synchronizers_of(me)
            .expect("the handle must exist for a registered thread");
        own.set_jmx_owned_synchronizer_own(me, &own_list, owner, lock);

        for tid in [me, peer] {
            assert_eq!(
                owned_synchronizers(&general, tid),
                owned_synchronizers(&own, tid),
                "{label}: the two forms disagree about what {tid:?} owns"
            );
        }
    }
}

/// The handle must be the *same* list the JMX snapshot reads.
///
/// This is the one way the fast form can be wrong without any of its own
/// logic being wrong: hand it a list that is not the registry's, and every
/// write lands somewhere `jmx_lock_snapshot` will never look. The `Arc` is
/// what makes them one list, and nothing else in the type system says so.
#[test]
fn the_handle_and_the_snapshot_are_the_same_list() {
    let registry = ThreadRegistry::new();
    let me = ThreadId(1);
    registry.register(me, "me", None);
    let handle = registry
        .jmx_locked_synchronizers_of(me)
        .expect("registered");
    handle.lock().push(fake_synchronizer(0x3000));
    assert_eq!(
        owned_synchronizers(&registry, me),
        vec![0x3000],
        "a write through the handle was invisible to jmx_lock_snapshot — \
         the handle is not the registry's list"
    );
}

/// An unregistered thread has no handle, which is what sends its caller to
/// the general form rather than to a private list nobody reads.
#[test]
fn an_unregistered_thread_has_no_synchronizer_handle() {
    let registry = ThreadRegistry::new();
    assert!(registry.jmx_locked_synchronizers_of(ThreadId(77)).is_none());
}

fn addrs(objs: &[ObjectRef]) -> Vec<usize> {
    objs.iter().map(|o| o.as_ptr() as usize).collect()
}

/// The lock stack must report exactly what the `Mutex<Vec<_>>` it replaced
/// reported — same members, same order — through publish (dedupe), retract
/// (every copy), overflow past the slots, holes, and the drain back.
///
/// Driven against a model of the old list over a pseudo-random sequence
/// biased toward LIFO releases (what structured locking produces) but with
/// out-of-order ones too (JNI can release in any order). With twenty
/// addresses the held set hovers around seven, so the sequence crosses the
/// eight-slot boundary in both directions many times.
#[test]
fn the_lock_stack_reports_exactly_what_the_old_list_reported() {
    let stack = JmxLockStack::default();
    let mut model: Vec<usize> = Vec::new();
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for step in 0..20_000u32 {
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let a = 0x10_000usize + ((x >> 33) % 20) as usize * 0x40;
        match (x >> 60) & 3 {
            0 | 1 => {
                stack.publish(fake_synchronizer(a));
                if !model.contains(&a) {
                    model.push(a);
                }
            }
            2 => {
                if let Some(&last) = model.last() {
                    stack.retract(fake_synchronizer(last));
                    model.retain(|&m| m != last);
                }
            }
            _ => {
                stack.retract(fake_synchronizer(a));
                model.retain(|&m| m != a);
            }
        }
        assert_eq!(
            addrs(&stack.snapshot()),
            model,
            "step {step}: the snapshot diverged from the list it replaced"
        );
        let mut roots = Vec::new();
        stack.extend_roots_stw(&mut roots);
        assert_eq!(addrs(&roots), model, "step {step}: the GC roots diverged");
    }
}

/// A moving collection must forward entries in the slots AND the overflow,
/// and a later retract by the relocated address (which is what the owner's
/// remapped frames hold) must find them.
#[test]
fn the_lock_stack_remaps_slots_and_overflow() {
    let stack = JmxLockStack::default();
    let old: Vec<usize> = (0..JMX_LOCK_STACK_SLOTS + 3)
        .map(|i| 0x20_000 + i * 0x40)
        .collect();
    for &a in &old {
        stack.publish(fake_synchronizer(a));
    }
    let mut pm = cratonvm_types::PointerMap::default();
    let mut expected = Vec::new();
    for (i, &a) in old.iter().enumerate() {
        if i % 2 == 0 {
            let moved = 0x90_000 + i * 0x40;
            pm.insert(a, moved);
            expected.push(moved);
        } else {
            expected.push(a);
        }
    }
    stack.remap_stw(&pm);
    assert_eq!(addrs(&stack.snapshot()), expected);
    for &a in expected.iter().rev() {
        stack.retract(fake_synchronizer(a));
    }
    assert!(stack.snapshot().is_empty());
    let top = stack.top.load(Ordering::Relaxed);
    assert_eq!(top, 0, "LIFO release leaves no holes");
    assert!(
        !stack.spilled.load(Ordering::Relaxed),
        "a drained overflow must send the next publish back to the slots"
    );
}

/// A cross-thread `getLockedMonitors()` reads while the owner writes. It
/// must only ever see a set the owner actually held: here `{}`, `{A}` or
/// `{A, B}` — never `{B}`, which an unvalidated slot walk can assemble from
/// a stale `top` and slots rewritten under it.
#[test]
fn a_cross_thread_snapshot_never_sees_a_set_the_owner_did_not_hold() {
    let stack = Arc::new(JmxLockStack::default());
    let (a, b) = (0x30_000usize, 0x30_040usize);
    let done = Arc::new(AtomicBool::new(false));
    let owner = {
        let stack = Arc::clone(&stack);
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            for _ in 0..50_000 {
                stack.publish(fake_synchronizer(a));
                stack.publish(fake_synchronizer(b));
                stack.retract(fake_synchronizer(b));
                stack.retract(fake_synchronizer(a));
            }
            done.store(true, Ordering::Release);
        })
    };
    while !done.load(Ordering::Acquire) {
        let got = addrs(&stack.snapshot());
        assert!(
            got.is_empty() || got == [a] || got == [a, b],
            "torn snapshot {got:x?}"
        );
    }
    owner.join().expect("owner thread panicked");
}

/// The registry front door: a re-entrant publish is one entry, the entry is
/// a GC root while held, and the retract removes both.
#[test]
fn owned_monitors_publish_once_root_while_held_and_retract() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(1);
    registry.register(tid, "owner", None);
    let m = fake_synchronizer(0x40_000);
    registry.complete_jmx_monitor_enter(tid, m);
    registry.complete_jmx_monitor_enter(tid, m);
    let held = registry.jmx_lock_snapshot(tid).expect("registered").2;
    assert_eq!(addrs(&held), vec![0x40_000]);
    let roots = registry.collect_all_root_snapshots();
    assert_eq!(addrs(&roots), vec![0x40_000]);
    registry.remove_jmx_locked_monitor(tid, m);
    let held = registry.jmx_lock_snapshot(tid).expect("registered").2;
    assert!(held.is_empty());
    assert!(registry.collect_all_root_snapshots().is_empty());
}

/// An AQS ownership transition must leave the synchronizer recorded against
/// exactly one thread — the property the old Θ(threads) scan bought by
/// walking every thread and taking every thread's mutex, twice per
/// uncontended `ReentrantLock.lock()`/`unlock()` pair.
///
/// The reverse index replaces the scan; this pins that it still answers the
/// same. `probes/AqsOwnerScaleProbe.java` is the companion measurement —
/// with the scan in place, an uncontended lock got ~2x more expensive going
/// from 1 to 256 live threads while a `synchronized` control stayed flat.
#[test]
fn an_ownership_transition_records_the_synchronizer_against_exactly_one_thread() {
    let registry = ThreadRegistry::new();
    let (a, b, c) = (ThreadId(1), ThreadId(2), ThreadId(3));
    for (tid, name) in [(a, "a"), (b, "b"), (c, "c")] {
        registry.register(tid, name, None);
    }
    let lock = fake_synchronizer(0x1000);

    // acquire on A
    registry.set_jmx_owned_synchronizer(Some(a), lock);
    assert_eq!(owned_synchronizers(&registry, a), vec![0x1000]);
    assert!(owned_synchronizers(&registry, b).is_empty());

    // A reentrant acquire re-runs the setter; it must not double-record.
    registry.set_jmx_owned_synchronizer(Some(a), lock);
    assert_eq!(owned_synchronizers(&registry, a), vec![0x1000]);

    // Hand-off to B without an intervening release: A must lose it.
    registry.set_jmx_owned_synchronizer(Some(b), lock);
    assert!(
        owned_synchronizers(&registry, a).is_empty(),
        "the former owner must not keep a lock it no longer holds"
    );
    assert_eq!(owned_synchronizers(&registry, b), vec![0x1000]);

    // release
    registry.set_jmx_owned_synchronizer(None, lock);
    for tid in [a, b, c] {
        assert!(
            owned_synchronizers(&registry, tid).is_empty(),
            "a released synchronizer is owned by nobody"
        );
    }

    // A second, distinct lock is independent of the first.
    let other = fake_synchronizer(0x2000);
    registry.set_jmx_owned_synchronizer(Some(c), lock);
    registry.set_jmx_owned_synchronizer(Some(c), other);
    let mut held = owned_synchronizers(&registry, c);
    held.sort_unstable();
    assert_eq!(held, vec![0x1000, 0x2000]);
    registry.set_jmx_owned_synchronizer(None, lock);
    assert_eq!(owned_synchronizers(&registry, c), vec![0x2000]);
}

/// The index is keyed by a heap address, so a moving collection that
/// relocates a held synchronizer has to rekey it in the same pass that
/// remaps the per-thread lists. Without that, the next transition for the
/// lock misses its previous owner and records it against two threads.
#[test]
fn a_moving_collection_rekeys_the_owned_synchronizer_index() {
    let registry = ThreadRegistry::new();
    let (a, b) = (ThreadId(1), ThreadId(2));
    registry.register(a, "a", None);
    registry.register(b, "b", None);

    registry.set_jmx_owned_synchronizer(Some(a), fake_synchronizer(0x1000));
    let mut pointer_map = cratonvm_types::PointerMap::default();
    pointer_map.insert(0x1000usize, 0x9000usize);
    registry.update_thread_objs_after_gc(&pointer_map);

    assert_eq!(
        owned_synchronizers(&registry, a),
        vec![0x9000],
        "the list itself is remapped"
    );

    // Now hand the (relocated) lock to B. If the index still held the
    // vacated key, A would keep its stale entry and both threads would
    // report owning it.
    registry.set_jmx_owned_synchronizer(Some(b), fake_synchronizer(0x9000));
    assert!(
        owned_synchronizers(&registry, a).is_empty(),
        "the index followed the object, so the former owner was found"
    );
    assert_eq!(owned_synchronizers(&registry, b), vec![0x9000]);
}

/// gcg g2/k: a sliding compaction moves one object onto another's OLD
/// address. Rekeying pair by pair in that order overwrote the second row
/// and then carried the first one on; two phases keep both.
#[test]
fn gcg_k2_rekey_in_two_phases_survives_a_slide_onto_a_vacated_key() {
    // addr-keyed: a test-local map.
    let mut map: rustc_hash::FxHashMap<usize, &str> = rustc_hash::FxHashMap::default();
    map.insert(0x1000, "a");
    map.insert(0x2000, "b");
    map.insert(0x5000, "unmoved");
    // `a` slides onto `b`'s old address, `b` onto a fresh one -- in the
    // order that broke the one-pair-at-a-time rekey.
    rekey_in_two_phases(&mut map, &[(0x1000, 0x2000), (0x2000, 0x3000)]);
    assert_eq!(map.get(&0x2000), Some(&"a"));
    assert_eq!(map.get(&0x3000), Some(&"b"));
    assert_eq!(map.get(&0x5000), Some(&"unmoved"));
    assert_eq!(map.get(&0x1000), None);
    assert_eq!(map.len(), 3);
    // A pair whose old key has no row moves nothing.
    rekey_in_two_phases(&mut map, &[(0x7000, 0x8000)]);
    assert_eq!(map.len(), 3);
}

/// gcg g2/k: the same slide through the owned-synchronizer index: after
/// the collection each synchronizer is still recorded against its owner,
/// so a release of each finds it.
#[test]
fn gcg_k2_a_sliding_collection_keeps_every_owned_synchronizer_row() {
    let registry = ThreadRegistry::new();
    let (a, b) = (ThreadId(1), ThreadId(2));
    registry.register(a, "a", None);
    registry.register(b, "b", None);
    registry.set_jmx_owned_synchronizer(Some(a), fake_synchronizer(0x1000));
    registry.set_jmx_owned_synchronizer(Some(b), fake_synchronizer(0x2000));
    let mut pointer_map = cratonvm_types::PointerMap::default();
    pointer_map.insert(0x1000usize, 0x2000usize);
    pointer_map.insert(0x2000usize, 0x3000usize);
    registry.update_thread_objs_after_gc(&pointer_map);
    assert_eq!(owned_synchronizers(&registry, a), vec![0x2000]);
    assert_eq!(owned_synchronizers(&registry, b), vec![0x3000]);
    registry.set_jmx_owned_synchronizer(None, fake_synchronizer(0x2000));
    registry.set_jmx_owned_synchronizer(None, fake_synchronizer(0x3000));
    assert!(
        owned_synchronizers(&registry, a).is_empty(),
        "a's row followed its lock"
    );
    assert!(
        owned_synchronizers(&registry, b).is_empty(),
        "b's row was not overwritten"
    );
}

/// gcg g3/k (`common-a-proposal-per-thread-handshakes`, step 1): the
/// three classifications, the owner-side run, the requester's withdrawal
/// and the abandonment of a dropped owner's queue.
#[test]
fn gcg_k3_a_handshake_is_queued_run_withdrawn_or_handed_back() {
    use crate::threading::jvm_thread::{HandshakeState, JvmThread};
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    let registry = ThreadRegistry::new();
    // gci i23/hs: an op gets the VM its target polls for.
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let tid = ThreadId(31);
    let ran = Arc::new(AtomicUsize::new(0));
    let op = |ran: &Arc<AtomicUsize>| -> crate::threading::jvm_thread::HandshakeOp {
        let ran = Arc::clone(ran);
        Box::new(move |_: &crate::vm::SharedVm, t: &JvmThread| {
            assert_eq!(
                t.thread_id,
                ThreadId(31),
                "runs on the target's own thread state"
            );
            ran.fetch_add(1, Ordering::SeqCst);
        })
    };

    // Unknown thread: handed back.
    assert!(matches!(
        registry.request_handshake(tid, op(&ran)),
        HandshakeRequest::Gone(_)
    ));

    registry.register(tid, "hs", None);
    let mut thread = JvmThread::new(tid, "hs");
    registry.set_gc_block_state(tid, Arc::clone(&thread.gc_block_state));

    // Running target: queued, run by the owner, exactly once.
    let HandshakeRequest::Queued { state, ticket } = registry.request_handshake(tid, op(&ran))
    else {
        panic!("a running target queues the op");
    };
    assert!(Arc::ptr_eq(&state, &thread.gc_block_state));
    assert!(thread.gc_block_state.handshakes.is_pending());
    assert_eq!(ticket.state(), HandshakeState::Pending);
    thread.run_pending_handshakes(&shared);
    assert_eq!(ran.load(Ordering::SeqCst), 1);
    assert_eq!(
        ticket.wait_timeout(Duration::from_secs(5)),
        HandshakeState::Ran
    );
    assert!(!thread.gc_block_state.handshakes.is_pending());
    assert!(state.handshakes.withdraw(&ticket).is_none(), "already run");
    thread.run_pending_handshakes(&shared);
    assert_eq!(ran.load(Ordering::SeqCst), 1, "never twice");

    // Withdrawn before the owner polls: never runs.
    let HandshakeRequest::Queued { state, ticket } = registry.request_handshake(tid, op(&ran))
    else {
        panic!("queued");
    };
    assert!(state.handshakes.withdraw(&ticket).is_some());
    assert_eq!(ticket.state(), HandshakeState::Withdrawn);
    assert!(!thread.gc_block_state.handshakes.is_pending());
    thread.run_pending_handshakes(&shared);
    assert_eq!(ran.load(Ordering::SeqCst), 1);

    // Blocked target: handed back for the requester's blocked path.
    thread
        .gc_block_state
        .in_blocked_region
        .store(true, Ordering::Release);
    assert!(matches!(
        registry.request_handshake(tid, op(&ran)),
        HandshakeRequest::TargetBlocked(_)
    ));
    thread
        .gc_block_state
        .in_blocked_region
        .store(false, Ordering::Release);

    // Owner dropped with an op queued: abandoned, never run.
    let HandshakeRequest::Queued { ticket, .. } = registry.request_handshake(tid, op(&ran))
    else {
        panic!("queued");
    };
    drop(thread);
    assert_eq!(
        ticket.wait_timeout(Duration::from_secs(5)),
        HandshakeState::Abandoned
    );
    assert!(matches!(
        registry.request_handshake(tid, op(&ran)),
        HandshakeRequest::Gone(_)
    ));
    assert_eq!(ran.load(Ordering::SeqCst), 1);
}

/// gcg g3/k: a requester on another thread waits on the ticket while the
/// target runs the op at its poll.
#[test]
fn gcg_k3_a_queued_handshake_wakes_its_waiting_requester() {
    use crate::threading::jvm_thread::{HandshakeState, JvmThread};
    use std::time::Duration;
    let registry = Arc::new(ThreadRegistry::new());
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let tid = ThreadId(32);
    registry.register(tid, "hs2", None);
    let mut thread = JvmThread::new(tid, "hs2");
    registry.set_gc_block_state(tid, Arc::clone(&thread.gc_block_state));
    let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let ran_clone = Arc::clone(&ran);
    let HandshakeRequest::Queued { ticket, .. } = registry.request_handshake(
        tid,
        Box::new(move |_: &crate::vm::SharedVm, _t: &JvmThread| {
            ran_clone.fetch_add(1, Ordering::SeqCst);
        }),
    ) else {
        panic!("queued");
    };
    let waiter = {
        let ticket = Arc::clone(&ticket);
        std::thread::spawn(move || ticket.wait_timeout(Duration::from_secs(30)))
    };
    std::thread::sleep(Duration::from_millis(5));
    thread.run_pending_handshakes(&shared);
    assert_eq!(waiter.join().unwrap(), HandshakeState::Ran);
    assert_eq!(ran.load(Ordering::SeqCst), 1);
}

/// gci i23/hs: an op that counts its runs into `ran`.
fn i23hs_counting_op(
    ran: &Arc<std::sync::atomic::AtomicUsize>,
) -> crate::threading::jvm_thread::HandshakeOp {
    let ran = Arc::clone(ran);
    Box::new(
        move |_: &crate::vm::SharedVm, _: &crate::threading::jvm_thread::JvmThread| {
            ran.fetch_add(1, Ordering::SeqCst);
        },
    )
}

/// gci i23/hs (step 2): `handshake_and_wait` returns `Ran` once a polling
/// target runs the op, exactly once, with the requester on another thread.
#[test]
fn gci_i23hs_handshake_and_wait_runs_the_op_at_the_targets_poll() {
    use crate::threading::jvm_thread::JvmThread;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    let registry = Arc::new(ThreadRegistry::new());
    let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
    let tid = ThreadId(33);
    registry.register(tid, "hs3", None);
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    // The target owns its `JvmThread` on its own OS thread and polls.
    let target = {
        let registry = Arc::clone(&registry);
        let shared = Arc::clone(&shared);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut thread = JvmThread::new(tid, "hs3");
            registry.set_gc_block_state(tid, Arc::clone(&thread.gc_block_state));
            ready_tx.send(Arc::clone(&thread.gc_block_state)).unwrap();
            while !stop.load(Ordering::Acquire) {
                thread.run_pending_handshakes(&shared);
                std::thread::yield_now();
            }
        })
    };
    let state = ready_rx.recv().unwrap();
    let ran = Arc::new(AtomicUsize::new(0));
    let outcome = registry.handshake_and_wait(
        tid,
        i23hs_counting_op(&ran),
        Duration::from_secs(30),
        Duration::from_micros(100),
        || false,
    );
    stop.store(true, Ordering::Release);
    target.join().unwrap();
    assert_eq!(outcome, HandshakeOutcome::Ran);
    assert_eq!(ran.load(Ordering::SeqCst), 1);
    assert!(!state.handshakes.is_pending());
}

/// gcj j1/cm (P4/ph): the kicked wait kicks exactly once when it queued
/// the op (before the target serves it), and never when it queued
/// nothing (an unknown or a blocked target).
#[test]
fn gcj1cm_the_kicked_wait_kicks_once_and_only_when_it_queued() {
    use crate::threading::jvm_thread::JvmThread;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    let registry = ThreadRegistry::new();
    let ran = Arc::new(AtomicUsize::new(0));
    let kicks = AtomicUsize::new(0);
    let slice = Duration::from_micros(200);
    let kick = || {
        kicks.fetch_add(1, Ordering::SeqCst);
    };
    assert_eq!(
        registry.handshake_and_wait_kicked(
            ThreadId(39),
            i23hs_counting_op(&ran),
            Duration::from_millis(2),
            slice,
            || false,
            kick
        ),
        HandshakeOutcome::Gone
    );
    assert_eq!(kicks.load(Ordering::SeqCst), 0, "nothing queued, no kick");
    let tid = ThreadId(39);
    registry.register(tid, "cm1", None);
    let thread = JvmThread::new(tid, "cm1");
    registry.set_gc_block_state(tid, Arc::clone(&thread.gc_block_state));
    thread
        .gc_block_state
        .in_blocked_region
        .store(true, Ordering::Release);
    assert_eq!(
        registry.handshake_and_wait_kicked(
            tid,
            i23hs_counting_op(&ran),
            Duration::from_millis(2),
            slice,
            || false,
            kick
        ),
        HandshakeOutcome::TargetBlocked
    );
    assert_eq!(kicks.load(Ordering::SeqCst), 0, "blocked, no kick");
    thread
        .gc_block_state
        .in_blocked_region
        .store(false, Ordering::Release);
    let state = Arc::clone(&thread.gc_block_state);
    // The kick sees the op already queued (it runs after the push).
    let saw_pending = AtomicBool::new(false);
    assert_eq!(
        registry.handshake_and_wait_kicked(
            tid,
            i23hs_counting_op(&ran),
            Duration::from_millis(2),
            slice,
            || false,
            || {
                kicks.fetch_add(1, Ordering::SeqCst);
                saw_pending.store(state.handshakes.is_pending(), Ordering::SeqCst);
            }
        ),
        HandshakeOutcome::NotServed
    );
    assert_eq!(kicks.load(Ordering::SeqCst), 1, "queued: one kick");
    assert!(saw_pending.load(Ordering::SeqCst));
    assert_eq!(ran.load(Ordering::SeqCst), 0, "never polled, withdrawn");
}

/// gci i23/hs: a target that never polls is withdrawn at the budget
/// (`NotServed`) and then never runs the op; one that is blocked from the
/// start, or unknown, is answered without queueing anything.
#[test]
fn gci_i23hs_handshake_and_wait_withdraws_an_unserved_op() {
    use crate::threading::jvm_thread::JvmThread;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    let registry = ThreadRegistry::new();
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let ran = Arc::new(AtomicUsize::new(0));
    let slice = Duration::from_micros(200);
    assert_eq!(
        registry.handshake_and_wait(
            ThreadId(34),
            i23hs_counting_op(&ran),
            Duration::from_millis(5),
            slice,
            || false
        ),
        HandshakeOutcome::Gone
    );
    let tid = ThreadId(34);
    registry.register(tid, "hs4", None);
    let mut thread = JvmThread::new(tid, "hs4");
    registry.set_gc_block_state(tid, Arc::clone(&thread.gc_block_state));

    let t0 = Instant::now();
    assert_eq!(
        registry.handshake_and_wait(
            tid,
            i23hs_counting_op(&ran),
            Duration::from_millis(5),
            slice,
            || false
        ),
        HandshakeOutcome::NotServed
    );
    assert!(t0.elapsed() >= Duration::from_millis(5), "waited its budget");
    assert!(!thread.gc_block_state.handshakes.is_pending(), "withdrawn");
    thread.run_pending_handshakes(&shared);
    assert_eq!(ran.load(Ordering::SeqCst), 0, "a withdrawn op never runs");

    thread
        .gc_block_state
        .in_blocked_region
        .store(true, Ordering::Release);
    assert_eq!(
        registry.handshake_and_wait(
            tid,
            i23hs_counting_op(&ran),
            Duration::from_secs(30),
            slice,
            || false
        ),
        HandshakeOutcome::TargetBlocked
    );
    assert!(!thread.gc_block_state.handshakes.is_pending(), "never queued");
    thread
        .gc_block_state
        .in_blocked_region
        .store(false, Ordering::Release);
    thread.run_pending_handshakes(&shared);
    assert_eq!(ran.load(Ordering::SeqCst), 0);
}

/// t1 lane E wave 3 (`t1-V2-handshake-poll-bit-stale-carrier-arm-skips-the-kick-fallback`):
/// the second-chance kick runs exactly once, after the first unserved
/// slice, with the state the op was queued on -- and never for a target
/// that settled before the first slice ended.
#[test]
fn t1e_the_second_chance_kick_runs_once_after_an_unserved_first_slice() {
    use crate::threading::jvm_thread::JvmThread;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    let registry = ThreadRegistry::new();
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let ran = Arc::new(AtomicUsize::new(0));
    let slice = Duration::from_micros(200);
    let tid = ThreadId(31_901);
    registry.register(tid, "t1e-2nd", None);
    let mut thread = JvmThread::new(tid, "t1e-2nd");
    registry.set_gc_block_state(tid, Arc::clone(&thread.gc_block_state));
    let expected = Arc::as_ptr(&thread.gc_block_state) as usize;

    // Unserved: kick once, second chance once, then the budget.
    let kicks = AtomicUsize::new(0);
    let rekicks = AtomicUsize::new(0);
    let rekicked_on = AtomicUsize::new(0);
    assert_eq!(
        registry.handshake_and_wait_kicking_twice(
            tid,
            i23hs_counting_op(&ran),
            // 100 ms, not 5: under a loaded host one yield_now slice can outlast a
            // 5 ms budget, so the deadline passed before the second chance.
            Duration::from_millis(100),
            slice,
            || false,
            |_| {
                kicks.fetch_add(1, Ordering::SeqCst);
            },
            |state| {
                rekicks.fetch_add(1, Ordering::SeqCst);
                rekicked_on.store(state as *const GcBlockState as usize, Ordering::SeqCst);
            },
            || {},
        ),
        HandshakeOutcome::NotServed
    );
    assert_eq!(kicks.load(Ordering::SeqCst), 1);
    assert_eq!(rekicks.load(Ordering::SeqCst), 1, "exactly one second chance");
    assert_eq!(rekicked_on.load(Ordering::SeqCst), expected, "the queued-on state");
    assert!(!thread.gc_block_state.handshakes.is_pending(), "withdrawn");

    // Served inside the first kick (the target polled at once): no second chance.
    let rekicks = AtomicUsize::new(0);
    let target = &mut thread;
    assert_eq!(
        registry.handshake_and_wait_kicking_twice(
            tid,
            i23hs_counting_op(&ran),
            Duration::from_secs(30),
            slice,
            || false,
            |_| target.run_pending_handshakes(&shared),
            |_| {
                rekicks.fetch_add(1, Ordering::SeqCst);
            },
            || {},
        ),
        HandshakeOutcome::Ran
    );
    assert_eq!(ran.load(Ordering::SeqCst), 1);
    assert_eq!(rekicks.load(Ordering::SeqCst), 0, "a prompt target is never re-kicked");
}

/// t1 lane E wave 3: [`ThreadRegistry::mounted_os_tid`] answers only a live,
/// MOUNTED entry's published tid -- never an unmounted continuation's kept
/// last-carrier tid, a dead entry's, or an unpublished `0`.
#[test]
fn t1e_mounted_os_tid_answers_only_a_live_mounted_published_tid() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(31_902);
    assert_eq!(registry.mounted_os_tid(tid), None, "unknown");
    registry.register(tid, "t1e-tid", None);
    let entry_do = |f: &dyn Fn(&ThreadEntry)| {
        let threads = registry.threads.read();
        f(threads.get(&tid).unwrap());
    };
    entry_do(&|e| e.os_tid.store(0, Ordering::Release));
    assert_eq!(registry.mounted_os_tid(tid), None, "unpublished");
    entry_do(&|e| e.os_tid.store(4242, Ordering::Release));
    assert_eq!(registry.mounted_os_tid(tid), Some(4242));
    entry_do(&|e| e.mounted.store(false, Ordering::Release));
    assert_eq!(registry.mounted_os_tid(tid), None, "unmounted keeps its last carrier's tid");
    entry_do(&|e| e.mounted.store(true, Ordering::Release));
    entry_do(&|e| e.alive.store(false, Ordering::Release));
    assert_eq!(registry.mounted_os_tid(tid), None, "dead");
}

/// t1 lane E2 (`CRATONVM_GC_HANDSHAKE_WAIT_SEES_BLOCK`): a target that
/// enters a blocked region after the op was queued ends the spin slice at
/// once (`TargetBlocked`, the op withdrawn and never run); with the switch
/// off the same wait lasts out its slice. A drop ends it as `Gone`.
#[test]
fn t1e2_the_spin_slice_ends_when_the_target_blocks_after_the_queue() {
    use crate::threading::jvm_thread::JvmThread;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let ran = Arc::new(AtomicUsize::new(0));
    for sees_block in [true, false] {
        let mut registry = ThreadRegistry::new();
        registry.handshake_spin_wait = true;
        registry.handshake_wait_sees_block = sees_block;
        let tid = ThreadId(31_903);
        registry.register(tid, "t1e2-block", None);
        let mut thread = JvmThread::new(tid, "t1e2-block");
        registry.set_gc_block_state(tid, Arc::clone(&thread.gc_block_state));
        let state = Arc::clone(&thread.gc_block_state);
        // Long slice with the switch on (it must not be waited out);
        // short with it off (it is).
        let slice = if sees_block {
            Duration::from_secs(5)
        } else {
            Duration::from_millis(60)
        };
        let t0 = Instant::now();
        let outcome = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(2));
                state.in_blocked_region.store(true, Ordering::Release);
            });
            registry.handshake_and_wait_kicking_twice(
                tid,
                i23hs_counting_op(&ran),
                Duration::from_secs(10),
                slice,
                || false,
                |_| {},
                |_| {},
                || {},
            )
        });
        let took = t0.elapsed();
        assert_eq!(outcome, HandshakeOutcome::TargetBlocked, "sees_block={sees_block}");
        if sees_block {
            assert!(took < Duration::from_millis(2500), "ended at the block: {took:?}");
        } else {
            assert!(took >= slice, "lasted out its slice: {took:?}");
        }
        assert!(!thread.gc_block_state.handshakes.is_pending(), "withdrawn");
        thread
            .gc_block_state
            .in_blocked_region
            .store(false, Ordering::Release);
        thread.run_pending_handshakes(&shared);
        assert_eq!(ran.load(Ordering::SeqCst), 0, "a withdrawn op never runs");
    }
}

/// t1 wave 8 lane HANDSHK (`CRATONVM_GC_HANDSHAKE_UNMOUNTED_DEFERS_KICK`):
/// a target UNMOUNTED at the push is not kicked at once. (a) It deposits
/// (blocks) within the first slice: `TargetBlocked`, no kick at all. (b) It
/// stays unmounted and running: the kick runs once after the first slice,
/// the second chance (`rekick`) after the next. (c) A MOUNTED target, or the
/// switch off, is kicked at the push as before.
#[test]
fn t1w8_handshk_an_unmounted_target_is_kicked_only_after_its_first_slice() {
    use crate::threading::jvm_thread::JvmThread;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    let ran = Arc::new(AtomicUsize::new(0));
    let setup = |defers: bool, mounted: bool| {
        let mut registry = ThreadRegistry::new();
        registry.handshake_spin_wait = true;
        registry.handshake_wait_sees_block = false;
        registry.handshake_unmounted_defers_kick = defers;
        let tid = ThreadId(31_904);
        registry.register(tid, "t1w8-handshk", None);
        let thread = JvmThread::new(tid, "t1w8-handshk");
        registry.set_gc_block_state(tid, Arc::clone(&thread.gc_block_state));
        registry.set_mounted(tid, mounted);
        (registry, tid, thread)
    };
    // (a) blocks inside the first slice: never kicked.
    {
        let (registry, tid, thread) = setup(true, false);
        let state = Arc::clone(&thread.gc_block_state);
        let kicks = AtomicUsize::new(0);
        let rekicks = AtomicUsize::new(0);
        let outcome = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(2));
                state.in_blocked_region.store(true, Ordering::Release);
            });
            registry.handshake_and_wait_kicking_twice(
                tid,
                i23hs_counting_op(&ran),
                Duration::from_secs(10),
                Duration::from_millis(200),
                || false,
                |_| {
                    kicks.fetch_add(1, Ordering::SeqCst);
                },
                |_| {
                    rekicks.fetch_add(1, Ordering::SeqCst);
                },
                || {},
            )
        });
        assert_eq!(outcome, HandshakeOutcome::TargetBlocked);
        assert_eq!(kicks.load(Ordering::SeqCst), 0, "a deposit within the slice: no kick");
        assert_eq!(rekicks.load(Ordering::SeqCst), 0);
        assert!(!thread.gc_block_state.handshakes.is_pending(), "withdrawn");
    }
    // (b) stays unmounted and running: kick after slice 1, rekick after slice 2.
    {
        let (registry, tid, thread) = setup(true, false);
        let kicks = AtomicUsize::new(0);
        let rekicks = AtomicUsize::new(0);
        let slices = AtomicUsize::new(0);
        let outcome = registry.handshake_and_wait_kicking_twice(
            tid,
            i23hs_counting_op(&ran),
            Duration::from_millis(80),
            Duration::from_millis(20),
            || {
                slices.fetch_add(1, Ordering::SeqCst);
                false
            },
            |_| {
                assert!(
                    slices.load(Ordering::SeqCst) >= 1,
                    "the deferred kick runs after the first slice"
                );
                kicks.fetch_add(1, Ordering::SeqCst);
            },
            |_| {
                assert_eq!(kicks.load(Ordering::SeqCst), 1, "the second chance comes after");
                rekicks.fetch_add(1, Ordering::SeqCst);
            },
            || {},
        );
        assert_eq!(outcome, HandshakeOutcome::NotServed);
        assert_eq!(kicks.load(Ordering::SeqCst), 1);
        assert_eq!(rekicks.load(Ordering::SeqCst), 1);
        assert!(!thread.gc_block_state.handshakes.is_pending(), "withdrawn at the budget");
    }
    // (c) mounted, or the switch off: kicked at the push.
    for (defers, mounted) in [(true, true), (false, false)] {
        let (registry, tid, thread) = setup(defers, mounted);
        let state = Arc::clone(&thread.gc_block_state);
        let kicks = AtomicUsize::new(0);
        let outcome = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(2));
                state.in_blocked_region.store(true, Ordering::Release);
            });
            registry.handshake_and_wait_kicking_twice(
                tid,
                i23hs_counting_op(&ran),
                Duration::from_secs(10),
                Duration::from_millis(200),
                || false,
                |_| {
                    kicks.fetch_add(1, Ordering::SeqCst);
                },
                |_| {},
                || {},
            )
        });
        assert_eq!(
            outcome,
            HandshakeOutcome::TargetBlocked,
            "defers={defers} mounted={mounted}"
        );
        assert_eq!(kicks.load(Ordering::SeqCst), 1, "defers={defers} mounted={mounted}");
    }
    assert_eq!(ran.load(Ordering::SeqCst), 0, "no op ran: every one was withdrawn");
}

/// t1 wave 8 lane HANDSHK (`CRATONVM_GC_HANDSHAKE_REQUESTER_SAFE_WHILE_WAITING`, default ON):
/// when thread A samples thread B while thread B samples thread A (mutual/cyclic samplers),
/// both requesters drain their own pending handshake queues during their spin slices.
/// Both handshakes run promptly and settle with `HandshakeOutcome::Ran`, avoiding the
/// 1 ms budget exhaustion and fallback stop-the-world pause.
#[test]
fn t1_handshk_mutual_samplers_drain_their_own_handshakes_without_timeout() {
    use crate::threading::jvm_thread::JvmThread;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    let registry = Arc::new(ThreadRegistry::new());
    assert!(registry.handshake_requester_safe_while_waiting());
    let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));

    let tid_a = ThreadId(31_910);
    let tid_b = ThreadId(31_911);
    registry.register(tid_a, "sampler-a", None);
    registry.register(tid_b, "sampler-b", None);

    let ran_a = Arc::new(AtomicUsize::new(0));
    let ran_b = Arc::new(AtomicUsize::new(0));

    let op_for_b = i23hs_counting_op(&ran_b);
    let op_for_a = i23hs_counting_op(&ran_a);

    let barrier_sync = std::sync::Barrier::new(2);

    let (outcome_a, outcome_b) = std::thread::scope(|s| {
        let h_a = s.spawn(|| {
            let thread_a = JvmThread::new(tid_a, "sampler-a");
            registry.set_gc_block_state(tid_a, Arc::clone(&thread_a.gc_block_state));
            barrier_sync.wait();
            registry.handshake_and_wait_kicking_twice(
                tid_b,
                op_for_b,
                Duration::from_millis(100),
                Duration::from_micros(100),
                || false,
                |_| {},
                |_| {},
                || thread_a.run_pending_handshakes(&shared),
            )
        });
        let h_b = s.spawn(|| {
            let thread_b = JvmThread::new(tid_b, "sampler-b");
            registry.set_gc_block_state(tid_b, Arc::clone(&thread_b.gc_block_state));
            barrier_sync.wait();
            registry.handshake_and_wait_kicking_twice(
                tid_a,
                op_for_a,
                Duration::from_millis(100),
                Duration::from_micros(100),
                || false,
                |_| {},
                |_| {},
                || thread_b.run_pending_handshakes(&shared),
            )
        });
        (h_a.join().unwrap(), h_b.join().unwrap())        });

    assert_eq!(outcome_a, HandshakeOutcome::Ran, "thread A handshake served");
    assert_eq!(outcome_b, HandshakeOutcome::Ran, "thread B handshake served");
    assert_eq!(ran_a.load(Ordering::SeqCst), 1, "op on A ran");
    assert_eq!(ran_b.load(Ordering::SeqCst), 1, "op on B ran");
}

/// gci i23/hs: the wait stops early -- withdrawing the op -- when `abort`
/// says so (a pause was requested: `NotServed` within a slice or two of
/// it, not at the budget), when the target enters a blocked region
/// (`TargetBlocked`), and when the target is dropped (`Gone`).
#[test]
fn gci_i23hs_handshake_and_wait_stops_early_for_a_pause_a_block_or_a_drop() {
    use crate::threading::jvm_thread::JvmThread;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    let registry = ThreadRegistry::new();
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let ran = Arc::new(AtomicUsize::new(0));
    let slice = Duration::from_micros(200);
    let long = Duration::from_secs(30);
    let tid = ThreadId(35);
    registry.register(tid, "hs5", None);
    let mut thread = JvmThread::new(tid, "hs5");
    registry.set_gc_block_state(tid, Arc::clone(&thread.gc_block_state));

    // A pause is requested after a few slices.
    let t0 = Instant::now();
    let mut polls = 0u32;
    let outcome = registry.handshake_and_wait(tid, i23hs_counting_op(&ran), long, slice, || {
        polls += 1;
        polls >= 3
    });
    assert_eq!(outcome, HandshakeOutcome::NotServed);
    assert_eq!(polls, 3);
    assert!(t0.elapsed() < Duration::from_secs(10), "not the budget");
    thread.run_pending_handshakes(&shared);
    assert_eq!(ran.load(Ordering::SeqCst), 0);

    // The target blocks while the requester waits.
    let state = Arc::clone(&thread.gc_block_state);
    let blocker = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(5));
        state.in_blocked_region.store(true, Ordering::Release);
    });
    let outcome =
        registry.handshake_and_wait(tid, i23hs_counting_op(&ran), long, slice, || false);
    blocker.join().unwrap();
    assert_eq!(outcome, HandshakeOutcome::TargetBlocked);
    thread
        .gc_block_state
        .in_blocked_region
        .store(false, Ordering::Release);
    thread.run_pending_handshakes(&shared);
    assert_eq!(ran.load(Ordering::SeqCst), 0, "withdrawn, never run");

    // The target is dropped while the requester (on its own thread) waits.
    let registry = Arc::new(registry);
    let requester = {
        let registry = Arc::clone(&registry);
        let ran = Arc::clone(&ran);
        std::thread::spawn(move || {
            registry.handshake_and_wait(tid, i23hs_counting_op(&ran), long, slice, || false)
        })
    };
    std::thread::sleep(Duration::from_millis(5));
    drop(thread);
    assert_eq!(requester.join().unwrap(), HandshakeOutcome::Gone);
    assert_eq!(ran.load(Ordering::SeqCst), 0);
}

/// gci i23/hs: an op that unwinds settles its ticket (and any later
/// one) `Abandoned`, so its requester does not wait for it.
#[test]
fn gci_i23hs_an_unwinding_op_abandons_its_ticket() {
    use crate::threading::jvm_thread::{HandshakeState, JvmThread};
    use std::time::Duration;
    let registry = ThreadRegistry::new();
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let tid = ThreadId(36);
    registry.register(tid, "hs6", None);
    let mut thread = JvmThread::new(tid, "hs6");
    registry.set_gc_block_state(tid, Arc::clone(&thread.gc_block_state));
    let HandshakeRequest::Queued { ticket: first, .. } = registry.request_handshake(
        tid,
        Box::new(|_: &crate::vm::SharedVm, t: &JvmThread| {
            if t.thread_id == ThreadId(36) {
                panic!("i23hs op unwinds");
            }
        }),
    ) else {
        panic!("queued");
    };
    let second_ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let second_ran_clone = Arc::clone(&second_ran);
    let HandshakeRequest::Queued { ticket: second, .. } = registry.request_handshake(
        tid,
        Box::new(move |_: &crate::vm::SharedVm, _t: &JvmThread| {
            second_ran_clone.store(true, Ordering::SeqCst);
        }),
    ) else {
        panic!("queued");
    };
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        thread.run_pending_handshakes(&shared);
    }));
    assert!(unwound.is_err());
    assert_eq!(
        first.wait_timeout(Duration::from_secs(5)),
        HandshakeState::Abandoned
    );
    assert_eq!(
        second.wait_timeout(Duration::from_secs(5)),
        HandshakeState::Abandoned
    );
    assert!(!second_ran.load(Ordering::SeqCst), "the op after the unwinding one never ran");
}

/// gci i24/rv (`gci-i24rv-i22-i23-default-path-review-findings-FIXED-20261017`
/// item 7): an op that unwinds on a LIVE target is `NotServed` (the
/// caller takes its frame-trace pause), not `Gone` (which skipped the
/// pause and served the stale trace). A dropped target stays `Gone`
/// (`gci_i23hs_handshake_and_wait_stops_early_for_a_pause_a_block_or_a_drop`).
#[test]
fn gci_i24rv_an_op_that_unwinds_on_a_live_target_is_not_served() {
    use crate::threading::jvm_thread::JvmThread;
    use std::time::Duration;
    let registry = Arc::new(ThreadRegistry::new());
    let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
    let tid = ThreadId(37);
    registry.register(tid, "rv24", None);
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    // The target polls on its own OS thread and survives the unwind.
    let target = {
        let registry = Arc::clone(&registry);
        let shared = Arc::clone(&shared);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut thread = JvmThread::new(tid, "rv24");
            registry.set_gc_block_state(tid, Arc::clone(&thread.gc_block_state));
            ready_tx.send(()).unwrap();
            while !stop.load(Ordering::Acquire) {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    thread.run_pending_handshakes(&shared);                    }));
                std::thread::yield_now();
            }
        })
    };
    ready_rx.recv().unwrap();
    let outcome = registry.handshake_and_wait(
        tid,
        Box::new(|_: &crate::vm::SharedVm, t: &JvmThread| {
            if t.thread_id == ThreadId(37) {
                panic!("i24rv op unwinds");
            }
        }),
        Duration::from_secs(30),
        Duration::from_micros(100),
        || false,
    );
    stop.store(true, Ordering::Release);
    target.join().unwrap();
    assert_eq!(outcome, HandshakeOutcome::NotServed);
}

#[test]
fn register_and_is_alive() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(1);
    registry.register(tid, "worker-1", None);
    assert!(registry.is_alive(tid));
    assert_eq!(registry.count(), 1);
    assert_eq!(registry.alive_count(), 1);
    assert_eq!(registry.alive_count_and_os_tids().0, 1);
}

#[test]
fn mark_dead() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(1);
    registry.register(tid, "worker-1", None);
    assert!(registry.is_alive(tid));

    registry.mark_dead(tid);
    assert!(!registry.is_alive(tid));
    assert_eq!(registry.count(), 1);
    assert_eq!(registry.alive_count(), 0);
}

/// Dead-owner lock recovery needs POSITIVE evidence of death: a registered
/// thread marked dead. An id the registry never saw (the JNI `ThreadId(0)`
/// placeholder) is `!is_alive` too, and releasing its lock would hand a
/// live thread's monitor to someone else.
#[test]
fn is_registered_dead_needs_a_registered_thread_marked_dead() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(1);
    assert!(!registry.is_registered_dead(tid), "never registered");
    assert!(!registry.is_registered_dead(ThreadId(0)));
    registry.register(tid, "worker-1", None);
    assert!(!registry.is_registered_dead(tid), "alive");
    registry.mark_dead(tid);
    assert!(registry.is_registered_dead(tid));
}

/// T19.H1 summary honesty. The deposited frame chain is only a readout of
/// where a thread *is* while that thread is inside a blocked region;
/// otherwise it is history that nothing clears. An unmarked stale chain
/// has now produced three wrong root causes (see
/// `ThreadRegistry::deposit_freshness`), the most recent being
/// `bug-h2-teststringcache-thread-join-blocked-on-dead-threads-20260807`:
/// `main` was running `TestStringCache.runBenchmark()` while its untagged
/// deposit still read `java/lang/Thread.join@129`, and the whole doc was
/// written against that line. So the tags are asserted, not just the
/// columns.
#[test]
fn t19_summary_tags_a_running_threads_deposit_as_stale() {
    assert_eq!(ThreadRegistry::deposit_freshness(true, true), "live");
    assert_eq!(ThreadRegistry::deposit_freshness(true, false), "STALE");
    assert_eq!(
        ThreadRegistry::deposit_freshness(false, true),
        "post-mortem",
        "a dead thread's raised in_blocked_region is termination residue"
    );
    assert_eq!(
        ThreadRegistry::deposit_freshness(false, false),
        "post-mortem"
    );

    let registry = ThreadRegistry::new();
    let running = ThreadId(1);
    let parked = ThreadId(2);
    let dead = ThreadId(3);
    for (tid, name) in [(running, "running"), (parked, "parked"), (dead, "dead")] {
        registry.register(tid, name, None);
        registry.set_frame_trace(
            tid,
            Arc::new(Mutex::new(vec![cratonvm_native_api::StackTraceEntry {
                class_name: Arc::from("java/lang/Thread"),
                method_name: Arc::from("join"),
                method_descriptor: None,
                source_file: Some(Arc::from("Thread.java")),
                line_number: crate::runtime::stackwalker::LINE_NUMBER_UNKNOWN,
                byte_code_index: 129,
                class_id: None,
                method_index: None,
            }])),
        );
    }
    // `parked` is genuinely inside a blocked region. `dead` carries the
    // flag its exit sequence's final `deposit_root_snapshot()` raised and
    // that nothing lowers — the exact shape the H2 dump showed for its
    // three finished workers.
    for tid in [parked, dead] {
        registry
            .gc_block_state_of(tid)
            .expect("registered")
            .in_blocked_region
            .store(true, Ordering::Release);
    }
    registry.mark_dead(dead);

    let text = registry.render_thread_summary();
    let line_for = |name: &str| -> String {
        text.lines()
            .find(|l| l.contains(&format!("name={name:?}")))
            .unwrap_or_else(|| panic!("no summary row for {name}:\n{text}"))
            .to_string()
    };

    let running_line = line_for("running");
    assert!(
        running_line.contains("alive=true")
            && running_line.contains("blocked=false")
            && running_line.contains("deposit=STALE"),
        "a running thread's deposit must be tagged STALE: {running_line}"
    );
    assert!(
        line_for("parked").contains("deposit=live"),
        "a thread inside a blocked region IS at its deposited wait site"
    );
    let dead_line = line_for("dead");
    assert!(
        dead_line.contains("alive=false")
            && dead_line.contains("blocked=true")
            && dead_line.contains("deposit=post-mortem"),
        "a dead entry's raised blocked= must be labelled residue: {dead_line}"
    );

    assert!(
        text.contains("(1 at a live wait site, 1 STALE)"),
        "the chain section must split live wait sites from stale history:\n{text}"
    );
    let stale_header = text
        .lines()
        .find(|l| l.contains("name=\"running\"") && l.contains("frame(s), oldest first"))
        .unwrap_or_else(|| panic!("no chain header for the running thread:\n{text}"));
    assert!(
        stale_header.contains("deposit=STALE") && stale_header.contains("RUNNING"),
        "the stale chain's own header must say so: {stale_header}"
    );
    assert!(
        text.contains(ThreadRegistry::DEPOSIT_LEGEND),
        "the legend explaining deposit= must accompany the table:\n{text}"
    );
}

/// A RUNNING thread that produced no live dump must be labelled as such,
/// and must NOT be pointed at a `"T19.H1 stack dump: tid=N"` section that
/// was never emitted.
///
/// That dangling pointer is what
/// `known-issues/netty/brotli-integration-test-hangs-outside-the-interpreter`
/// was written around: the summary said "its real position is in the live
/// stack dump above", the live dump for that thread did not exist because
/// the thread was in JIT-compiled code, and the missing section read as
/// "blocked somewhere the watchdog cannot reach". The thread was neither
/// blocked nor in native code — it was spinning in a compiled
/// `ByteBuf.writeByte` loop, and `--nojit` dumped all 73 frames.
#[test]
fn t19_summary_names_a_running_thread_that_produced_no_live_dump() {
    let registry = ThreadRegistry::new();
    let dumped = ThreadId(1);
    let silent = ThreadId(2);
    for (tid, name) in [(dumped, "dumped"), (silent, "silent")] {
        registry.register(tid, name, None);
        registry.set_frame_trace(
            tid,
            Arc::new(Mutex::new(vec![cratonvm_native_api::StackTraceEntry {
                class_name: Arc::from("java/lang/Thread"),
                method_name: Arc::from("join"),
                method_descriptor: None,
                source_file: Some(Arc::from("Thread.java")),
                line_number: crate::runtime::stackwalker::LINE_NUMBER_UNKNOWN,
                byte_code_index: 129,
                class_id: None,
                method_index: None,
            }])),
        );
    }

    // Only tid=1 answered the watchdog.
    let text = registry.render_thread_summary_for(&[dumped.0]);
    let row_for = |name: &str| -> String {
        let lines: Vec<&str> = text.lines().collect();
        let idx = lines
            .iter()
            .position(|l| l.contains(&format!("name={name:?}")) && l.contains("alive="))
            .unwrap_or_else(|| panic!("no summary row for {name}:\n{text}"));
        lines[idx..(idx + 2).min(lines.len())].join("\n")
    };

    assert!(
        !row_for("dumped").contains("no-live-dump"),
        "a thread that DID dump must not be flagged: {}",
        row_for("dumped")
    );
    assert!(
        row_for("silent").contains("no-live-dump"),
        "a RUNNING thread that produced no dump must say so: {}",
        row_for("silent")
    );
    assert!(
        row_for("silent").contains("--nojit"),
        "and must name the one flag that separates compiled code from \
         native code: {}",
        row_for("silent")
    );

    // The chain header for the silent thread must not send the reader to a
    // section that does not exist.
    let silent_chain = text
        .lines()
        .find(|l| l.contains("name=\"silent\"") && l.contains("frame(s), oldest first"))
        .unwrap_or_else(|| panic!("no chain header for the silent thread:\n{text}"));
    assert!(
        !silent_chain.contains("stack dump: tid=2"),
        "must not point at a live section that was never emitted: {silent_chain}"
    );
    assert!(
        silent_chain.contains("no-live-dump"),
        "the silent thread's chain header must carry the tag: {silent_chain}"
    );
    let dumped_chain = text
        .lines()
        .find(|l| l.contains("name=\"dumped\"") && l.contains("frame(s), oldest first"))
        .unwrap_or_else(|| panic!("no chain header for the dumped thread:\n{text}"));
    assert!(
        dumped_chain.contains("stack dump: tid=1"),
        "a thread that DID dump keeps the pointer to its live section: {dumped_chain}"
    );

    // With no ack information at all, every running thread is reported the
    // safe way rather than the confident-and-wrong way.
    let blind = registry.render_thread_summary();
    assert_eq!(
        blind.matches("no-live-dump").count() >= 2,
        true,
        "an empty ack set must not license the 'read the dump above' \
         phrasing for anyone:\n{blind}"
    );
}

#[test]
fn terminated_entries_are_bounded() {
    let registry = ThreadRegistry::new();
    for id in 1..=(TERMINATED_THREAD_ENTRY_CAP as u64 + 3) {
        let tid = ThreadId(id);
        registry.register(tid, "short-lived", None);
        registry.mark_dead(tid);
    }

    assert_eq!(registry.alive_count(), 0);
    assert_eq!(registry.count(), TERMINATED_THREAD_ENTRY_CAP);
    assert!(!registry.is_alive(ThreadId(1)));
}

#[test]
fn starting_thread_is_not_stw_counted_until_ready() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(1);
    registry.register_starting_with_daemon(tid, "starting", None, false);

    assert!(registry.is_alive(tid));
    assert_eq!(registry.alive_count(), 1);
    assert_eq!(registry.alive_count_and_os_tids().0, 0);

    registry.mark_stw_ready(tid);
    assert_eq!(registry.alive_count_and_os_tids().0, 1);
}

#[test]
fn unknown_thread_not_alive() {
    let registry = ThreadRegistry::new();
    assert!(!registry.is_alive(ThreadId(99)));
}

// -----------------------------------------------------------------------
// ARCH-2026-07-26 `cross-owner-closeout` (CR-SW-2): thread dumps get line
// numbers. The depositor publishes a line-less snapshot (it must stay
// lock-free), so the reader resolves — but only fail-closed.
// -----------------------------------------------------------------------

use crate::runtime::stackwalker::test_support::{named_method, store_with};
use crate::runtime::stackwalker::{LINE_NUMBER_NATIVE, LINE_NUMBER_UNKNOWN};
use cratonvm_native_api::StackTraceEntry;
use cratonvm_reader::attribute::LineNumberEntry;

fn deposited_entry(
    class_id: crate::classloading::ClassId,
    method: &str,
    bci: i32,
) -> StackTraceEntry {
    // Exactly the shape `stackwalker::capture_frames_no_lines` deposits.
    StackTraceEntry {
        class_name: Arc::from("probe/Target"),
        method_name: Arc::from(method),
        method_descriptor: None,
        source_file: Some(Arc::from("Target.java")),
        line_number: LINE_NUMBER_UNKNOWN,
        byte_code_index: bci,
        class_id: Some(class_id),
        method_index: None,
    }
}

fn registry_with_trace(trace: Vec<StackTraceEntry>) -> (ThreadRegistry, ThreadId) {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(1);
    registry.register(tid, "worker-1", None);
    registry.set_frame_trace(tid, Arc::new(Mutex::new(trace)));
    (registry, tid)
}

#[test]
fn frame_trace_of_is_line_less_and_resolved_reader_fills_it_in() {
    let (store, cid) = store_with(vec![named_method(
        "compute",
        "()I",
        vec![
            LineNumberEntry {
                start_pc: 0,
                line_number: 40,
            },
            LineNumberEntry {
                start_pc: 4,
                line_number: 41,
            },
        ],
    )]);
    let (registry, tid) = registry_with_trace(vec![
        deposited_entry(cid, "compute", 0),
        deposited_entry(cid, "compute", 6),
    ]);

    // What every consumer sees today.
    let raw = registry.frame_trace_of(tid);
    assert_eq!(raw.len(), 2);
    assert!(raw.iter().all(|e| e.line_number == LINE_NUMBER_UNKNOWN));

    // What a reader holding a ClassStore can see instead.
    let resolved = registry.frame_trace_of_resolved(tid, &store);
    assert_eq!(resolved.len(), 2);
    assert_eq!(resolved[0].line_number, 40);
    assert_eq!(resolved[1].line_number, 41);

    // Resolution is non-destructive: the published snapshot is untouched,
    // so a second reader still starts from the deposited state.
    assert!(registry
        .frame_trace_of(tid)
        .iter()
        .all(|e| e.line_number == LINE_NUMBER_UNKNOWN));
}

#[test]
fn resolved_reader_leaves_overloaded_and_native_frames_alone() {
    let (store, cid) = store_with(vec![
        named_method(
            "run",
            "(I)V",
            vec![LineNumberEntry {
                start_pc: 0,
                line_number: 11,
            }],
        ),
        named_method(
            "run",
            "(J)V",
            vec![LineNumberEntry {
                start_pc: 0,
                line_number: 22,
            }],
        ),
    ]);
    let mut native = deposited_entry(cid, "run", -1);
    native.line_number = LINE_NUMBER_NATIVE;
    let (registry, tid) = registry_with_trace(vec![deposited_entry(cid, "run", 0), native]);

    let resolved = registry.frame_trace_of_resolved(tid, &store);
    assert_eq!(
        resolved[0].line_number, LINE_NUMBER_UNKNOWN,
        "a deposited frame has no method_index, so an overload set must \
         stay unknown rather than print a line from the wrong body"
    );
    assert_eq!(resolved[1].line_number, LINE_NUMBER_NATIVE);
    // Frame identity is never lost, which is the property a dump needs.
    assert_eq!(&*resolved[0].class_name, "probe/Target");
    assert_eq!(&*resolved[0].method_name, "run");
}

#[test]
fn resolved_reader_on_an_unknown_thread_is_empty_not_a_panic() {
    let (store, _cid) = store_with(vec![named_method("compute", "()I", Vec::new())]);
    let registry = ThreadRegistry::new();
    assert!(registry
        .frame_trace_of_resolved(ThreadId(99), &store)
        .is_empty());
}

#[test]
fn resolved_reader_after_class_unload_keeps_the_frame_and_drops_only_the_line() {
    let (mut store, cid) = store_with(vec![named_method(
        "compute",
        "()I",
        vec![LineNumberEntry {
            start_pc: 0,
            line_number: 40,
        }],
    )]);
    let (registry, tid) = registry_with_trace(vec![deposited_entry(cid, "compute", 0)]);
    assert_eq!(
        registry.frame_trace_of_resolved(tid, &store)[0].line_number,
        40
    );

    // ClassIds are monotonic and never reused, so a stale id can only miss.
    let _ = store.remove(cid);
    let after = registry.frame_trace_of_resolved(tid, &store);
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].line_number, LINE_NUMBER_UNKNOWN);
    assert_eq!(&*after[0].method_name, "compute");
}

#[test]
fn native_blocked_thread_is_reported_as_blocked_until_woken() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(1);
    registry.register(tid, "waiter", None);

    assert!(!registry.is_blocked(tid));
    registry.mark_native_thread_blocked(tid);
    assert!(registry.is_blocked(tid));
    registry.mark_native_thread_unblocked(tid);
    assert!(!registry.is_blocked(tid));

    registry.mark_dead(tid);
    assert!(!registry.is_blocked(tid));
}

// -----------------------------------------------------------------
// Test helpers
// -----------------------------------------------------------------

/// Build a non-null, 8-byte-aligned dummy heap address. The registry
/// only round-trips the address (scan as root / remap) and never
/// dereferences it, so a live `Box` allocation is a safe stand-in for
/// a real heap object pointer.
fn dummy_aligned_objref(backing: &mut [u64; 2]) -> ObjectRef {
    let ptr = backing.as_mut_ptr() as *mut u8;
    // SAFETY: `backing` is a live, 8-byte-aligned, non-null allocation.
    unsafe { ObjectRef::from_raw(ptr) }
}

/// An alive thread's `java.lang.Thread` mirror is reported as a GC root
/// by `collect_all_root_snapshots`, so a collection initiated by ANOTHER
/// thread (while this one is parked) cannot sweep the mirror out from
/// under the dangling `ThreadEntry.java_thread_obj` copy. Regression for
/// the Tomcat TestDigestAuthenticator "this.holder is null" NPE: a worker-
/// thread GC freed the parked JUnit main thread's mirror, so the cached
/// `currentThread()` pointer dereferenced an all-zero (freed) header.
#[test]
fn alive_thread_mirror_is_a_root() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(1);

    let mut backing = [0u64; 2];
    let mirror = dummy_aligned_objref(&mut backing);

    // Registered WITHOUT a mirror → not a root yet.
    registry.register(tid, "main", None);
    assert!(registry.collect_all_root_snapshots().is_empty());

    // Once the mirror is published it must be rooted.
    registry.set_java_thread_obj(tid, mirror);
    let roots = registry.collect_all_root_snapshots();
    assert_eq!(roots.len(), 1, "alive thread mirror must be a root");
    assert_eq!(roots[0].as_ptr(), mirror.as_ptr());

    // A dead thread's mirror is no longer rooted (it may be reclaimed).
    registry.mark_dead(tid);
    assert!(registry.collect_all_root_snapshots().is_empty());
}

#[test]
fn collect_reserved_tlab_tails_reports_only_alive_unretired_tlabs() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(1);
    registry.register(tid, "worker", None);

    let mut backing = vec![0u64; 128];
    let ptr = backing.as_mut_ptr() as *mut u8;
    let size = backing.len() * std::mem::size_of::<u64>();
    let mut tlab = unsafe { cratonvm_gc::Tlab::new(ptr, size) };
    assert!(tlab.alloc(64, 8).is_some());

    registry.set_tlab_addr(tid, &tlab as *const cratonvm_gc::Tlab as usize);

    let tails = registry.collect_reserved_tlab_tails();
    assert_eq!(tails, vec![(ptr as usize + 64, ptr as usize + size)]);

    tlab.retire();
    assert!(
        registry.collect_reserved_tlab_tails().is_empty(),
        "retired TLABs must not publish skip regions"
    );

    backing.fill(0);
    let mut tlab = unsafe { cratonvm_gc::Tlab::new(ptr, size) };
    assert!(tlab.alloc(64, 8).is_some());
    registry.set_tlab_addr(tid, &tlab as *const cratonvm_gc::Tlab as usize);
    registry.mark_dead(tid);
    assert!(
        registry.collect_reserved_tlab_tails().is_empty(),
        "dead threads must not publish stale TLAB pointers"
    );
}

/// gci i28/tl: the `TLAB totals` snapshot reads each live thread's cycle
/// counters through its published address, and skips a dead one.
#[test]
fn gci_i28tl_tlab_cycle_snapshots_read_live_threads_only() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(1);
    registry.register(tid, "worker", None);
    let mut backing = vec![0u64; 128];
    let ptr = backing.as_mut_ptr() as *mut u8;
    let size = backing.len() * std::mem::size_of::<u64>();
    // SAFETY: `backing` outlives `tlab` and is `size` bytes.
    let mut tlab = unsafe { cratonvm_gc::Tlab::new(ptr, size) };
    assert!(tlab.alloc(64, 8).is_some());
    tlab.retire_for_refill();
    let addr = &tlab as *const cratonvm_gc::Tlab as usize;
    registry.set_tlab_addr(tid, addr);
    let snaps = registry.tlab_cycle_snapshots();
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].0, 1, "keyed by the thread id");
    assert_eq!(snaps[0].1, tlab.cycle_mark());
    assert_eq!(
        snaps[0].1.since_start().refill_waste_bytes,
        (size - 64) as u64
    );
    assert_eq!(snaps[0].2, tlab.thread_allocated_bytes());
    registry.mark_dead(tid);
    assert!(registry.tlab_cycle_snapshots().is_empty());
}

/// INT-3 — the G1 takeover path pins the deposited snapshot roots of
/// FROZEN peers only, selected by OS tid; other threads' snapshots (and
/// dead threads) must not leak into the pin set.
#[test]
fn root_snapshots_for_os_tids_returns_only_matching_alive_threads() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(1);
    registry.register(tid, "worker", None);
    registry.set_os_tid_current(tid);

    let mut backing = [0u64; 2];
    let root = dummy_aligned_objref(&mut backing);
    registry.set_root_snapshot(tid, Arc::new(Mutex::new(vec![root])));

    let (_, _, tids, _) = registry.alive_count_blocked_and_os_tids();
    assert!(!tids.is_empty(), "os tid must be published");
    let roots = registry.root_snapshots_for_os_tids(&tids);
    assert_eq!(roots.len(), 1, "matching alive thread's snapshot returned");
    assert_eq!(roots[0].as_ptr(), root.as_ptr());

    assert!(
        registry.root_snapshots_for_os_tids(&[u32::MAX]).is_empty(),
        "non-matching tid must contribute nothing"
    );
    assert!(
        registry.root_snapshots_for_os_tids(&[]).is_empty(),
        "empty tid set short-circuits"
    );

    registry.mark_dead(tid);
    assert!(
        registry.root_snapshots_for_os_tids(&tids).is_empty(),
        "dead thread's snapshot must not be pinned"
    );
}

#[test]
#[cfg(any(windows, target_os = "linux"))]
fn set_os_tid_current_publishes_thread_stack_band() {
    let handle = std::thread::spawn(|| {
        let registry = ThreadRegistry::new();
        let tid = ThreadId(77);
        registry.register(tid, "daemon-waiter", None);
        let os_tid = current_os_tid();
        cratonvm_gc::shadow_stack::unpublish_thread_stack_band(os_tid);
        assert!(cratonvm_gc::shadow_stack::thread_stack_band(os_tid).is_none());
        registry.set_os_tid_current(tid);
        let band = cratonvm_gc::shadow_stack::thread_stack_band(os_tid)
            .expect("set_os_tid_current must publish the calling OS thread's stack band");
        let local = 0usize;
        let sp = std::hint::black_box(&local as *const usize as usize);
        assert!(sp >= band.lo && sp < band.hi, "sp={sp:#x} not in {band:?}");
        cratonvm_gc::shadow_stack::unpublish_thread_stack_band(os_tid);
    });
    handle.join().expect("spawned thread panicked");
}

#[test]
fn moved_thread_mirror_records_former_address_for_recovery() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(1);

    let mut old_backing = [0u64; 2];
    let mut new_backing = [0u64; 2];
    let old_ref = dummy_aligned_objref(&mut old_backing);
    let new_ref = dummy_aligned_objref(&mut new_backing);
    let old_addr = old_ref.as_ptr() as usize;
    let new_addr = new_ref.as_ptr() as usize;
    assert_ne!(old_addr, new_addr);

    registry.register(tid, "main", Some(old_ref));

    let mut pm = cratonvm_types::PointerMap::default();
    pm.insert(old_addr, new_addr);
    registry.update_thread_objs_after_gc(&pm);

    assert_eq!(
        registry.java_thread_obj(tid).unwrap().as_ptr() as usize,
        new_addr,
        "registry mirror must be remapped to the live address",
    );
    assert_eq!(
        registry.recover_stale_mirror(old_addr).unwrap().as_ptr() as usize,
        new_addr,
        "former mirror address must recover the live mirror",
    );
    assert!(
        registry.recover_stale_mirror(new_addr).is_none(),
        "the current live address is not itself a former address",
    );
}

#[test]
fn stale_mirror_recovery_survives_multiple_moves() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(1);

    let mut first_backing = [0u64; 2];
    let mut second_backing = [0u64; 2];
    let mut third_backing = [0u64; 2];
    let first = dummy_aligned_objref(&mut first_backing);
    let second = dummy_aligned_objref(&mut second_backing);
    let third = dummy_aligned_objref(&mut third_backing);
    let first_addr = first.as_ptr() as usize;
    let second_addr = second.as_ptr() as usize;
    let third_addr = third.as_ptr() as usize;
    assert_ne!(first_addr, second_addr);
    assert_ne!(second_addr, third_addr);

    registry.register(tid, "main", Some(first));

    let mut pm1 = cratonvm_types::PointerMap::default();
    pm1.insert(first_addr, second_addr);
    registry.update_thread_objs_after_gc(&pm1);

    let mut pm2 = cratonvm_types::PointerMap::default();
    pm2.insert(second_addr, third_addr);
    registry.update_thread_objs_after_gc(&pm2);

    for stale_addr in [first_addr, second_addr] {
        assert_eq!(
            registry.recover_stale_mirror(stale_addr).unwrap().as_ptr() as usize,
            third_addr,
            "any retained former mirror address should recover the current mirror",
        );
    }
}

#[test]
fn thread_name() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(1);
    registry.register(tid, "my-thread", None);
    assert_eq!(registry.thread_name(tid), Some("my-thread".to_string()));
    assert_eq!(registry.thread_name(ThreadId(99)), None);
}

#[test]
fn join_with_real_thread() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(1);
    registry.register(tid, "worker", None);

    let handle = std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(10));
    });
    registry.set_join_handle(tid, handle);

    let joined = registry.join(tid);
    assert!(joined);

    // Second join returns false (handle already taken)
    let joined2 = registry.join(tid);
    assert!(!joined2);
}

#[test]
fn join_no_handle_dead_thread() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(1);
    registry.register(tid, "worker", None);
    registry.mark_dead(tid);

    // No handle, thread is dead — returns false immediately
    let joined = registry.join(tid);
    assert!(!joined);
}

// -----------------------------------------------------------------
// T19.K1 — non-daemon thread waiting tests
// -----------------------------------------------------------------

/// Default `register` paints the thread as non-daemon.
#[test]
fn t19_k1_default_register_is_non_daemon() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(1);
    registry.register(tid, "user", None);
    assert!(!registry.is_daemon(tid));
    assert_eq!(registry.alive_non_daemon_thread_ids(), vec![tid]);
}

/// `register_with_daemon(true)` paints it as daemon and excludes it
/// from the non-daemon enumeration.
#[test]
fn t19_k1_register_with_daemon_excludes_from_wait() {
    let registry = ThreadRegistry::new();
    let user = ThreadId(1);
    let dmn = ThreadId(2);
    registry.register_with_daemon(user, "user", None, false);
    registry.register_with_daemon(dmn, "dmn", None, true);

    assert!(!registry.is_daemon(user));
    assert!(registry.is_daemon(dmn));
    assert_eq!(registry.alive_non_daemon_thread_ids(), vec![user]);
}

/// `set_daemon` flips the flag for a registered thread; setting
/// `daemon = true` removes it from the wait set.
#[test]
fn t19_k1_set_daemon_flips_flag() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(1);
    registry.register(tid, "worker", None);
    assert!(!registry.is_daemon(tid));
    assert!(registry.set_daemon(tid, true));
    assert!(registry.is_daemon(tid));
    assert!(registry.alive_non_daemon_thread_ids().is_empty());
}

/// `set_daemon` on an unknown id returns `false` and is a no-op.
#[test]
fn t19_k1_set_daemon_unknown_id_returns_false() {
    let registry = ThreadRegistry::new();
    assert!(!registry.set_daemon(ThreadId(99), true));
}

/// HelloWorld parity: an empty registry waits for zero threads
/// and returns immediately.
///
/// `n == 0` is the property. The elapsed bound only distinguishes
/// "returned" from "blocked forever", so it is a hang backstop, not a
/// 50 ms performance budget — a loaded runner can lose 50 ms to
/// scheduling before the test body even starts (observed failing).
#[test]
fn t19_k1_wait_with_empty_registry_returns_zero() {
    let registry = ThreadRegistry::new();
    let start = std::time::Instant::now();
    let n = registry.wait_for_non_daemon_threads(None);
    assert_eq!(n, 0);
    assert!(
        start.elapsed() < std::time::Duration::from_secs(20),
        "empty wait must not block: {:?}",
        start.elapsed(),
    );
}

/// HelloWorld parity: a registry that contains only daemon threads
/// waits for zero threads and returns immediately.
///
/// As above: `n == 0` plus "neither daemon was joined" (only `join()`
/// marks a thread dead) are the timing-independent properties; the
/// elapsed bound is a hang backstop.
#[test]
fn t19_k1_wait_with_only_daemons_returns_zero() {
    let registry = ThreadRegistry::new();
    registry.register_with_daemon(ThreadId(1), "gc", None, true);
    registry.register_with_daemon(ThreadId(2), "finalizer", None, true);
    let start = std::time::Instant::now();
    let n = registry.wait_for_non_daemon_threads(None);
    assert_eq!(n, 0);
    assert!(registry.is_alive(ThreadId(1)));
    assert!(registry.is_alive(ThreadId(2)));
    assert!(
        start.elapsed() < std::time::Duration::from_secs(20),
        "daemon-only wait must not block: {:?}",
        start.elapsed(),
    );
}

/// Happy path: a non-daemon thread blocks the wait until it
/// terminates.
///
/// "Blocked until it terminated" is stated directly by the worker's
/// completion flag: the flag is set as the worker's last action, so
/// observing it `true` *after* the wait returned proves the ordering.
/// The old `elapsed >= 80 ms` was a proxy for that and is not actually
/// safe under load in the way a lower bound usually is: the clock only
/// starts after `spawn` + `set_join_handle`, so on a loaded box the
/// worker's 100 ms sleep can be over before `start` is even sampled
/// (its sibling `t19_k1_wait_for_multiple_non_daemons` was observed
/// failing that way at 134 µs).
#[test]
fn t19_k1_wait_blocks_until_non_daemon_finishes() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(1);
    registry.register(tid, "user", None);
    let finished = Arc::new(AtomicBool::new(false));
    let f = finished.clone();
    let handle = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        f.store(true, Ordering::Release);
    });
    registry.set_join_handle(tid, handle);
    let joined = registry.wait_for_non_daemon_threads(None);
    assert_eq!(joined, 1);
    assert!(
        finished.load(Ordering::Acquire),
        "the wait returned before the non-daemon thread had finished",
    );
}

/// A daemon thread does not extend the wait — even if it runs for a
/// long time, the wait returns without considering it.
///
/// Same flake shape as `t19_k1_mixed_only_joins_non_daemons`: the
/// original `start.elapsed() < 50 ms` is a fixed wall-clock bound with
/// essentially no margin, and it was observed failing under load. What
/// must be true is that the daemon was *not waited for*, which the
/// counters below state directly and load cannot falsify.
#[test]
fn t19_k1_daemon_thread_does_not_keep_main_alive() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(1);
    registry.register_with_daemon(tid, "dmn", None, true);
    let daemon_finished = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let df = daemon_finished.clone();
    let rel = release.clone();
    let handle = std::thread::spawn(move || {
        // Long-running — the wait must not block on this. The 60 s cap
        // makes a regression fail rather than hang forever.
        let cap = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !rel.load(Ordering::Acquire) && std::time::Instant::now() < cap {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        df.store(true, Ordering::Release);
    });
    registry.set_join_handle(tid, handle);
    let start = std::time::Instant::now();
    let joined = registry.wait_for_non_daemon_threads(None);
    let elapsed = start.elapsed();
    assert_eq!(joined, 0, "a daemon must never be joined");
    assert!(
        !daemon_finished.load(Ordering::Acquire),
        "the wait returned only after the daemon finished — it was waited for",
    );
    // Only `join()` marks a thread dead, so this is a direct readout
    // that the daemon was skipped.
    assert!(registry.is_alive(tid));
    // Hang backstop only — well under the daemon's 60 s cap so
    // "waited for the daemon anyway" still fails here.
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "daemon thread must not delay shutdown: {:?}",
        elapsed,
    );
    release.store(true, Ordering::Release);
    assert!(registry.join(tid), "daemon handle should still be present");
}

/// Multiple non-daemon threads — the wait blocks until ALL of them
/// finish, and the count reflects all joined.
///
/// "Waited for the slowest" is stated as a completion count taken
/// after the wait returned, not as an elapsed-time lower bound. The
/// old `elapsed >= 80 ms` looked like a safe-direction assertion but
/// was not: the clock starts only after all three spawns, so on a
/// loaded box the sleeps can already be over by then — this test was
/// observed failing with `must wait for slowest: 134.294µs`.
#[test]
fn t19_k1_wait_for_multiple_non_daemons() {
    let registry = ThreadRegistry::new();
    let done = Arc::new(AtomicU64::new(0));
    for (i, sleep_ms) in [50u64, 100, 80].iter().enumerate() {
        let tid = ThreadId(i as u64 + 1);
        registry.register(tid, &format!("user-{i}"), None);
        let ms = *sleep_ms;
        let d = done.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(ms));
            d.fetch_add(1, Ordering::AcqRel);
        });
        registry.set_join_handle(tid, handle);
    }
    let joined = registry.wait_for_non_daemon_threads(None);
    assert_eq!(joined, 3);
    assert_eq!(
        done.load(Ordering::Acquire),
        3,
        "the wait returned before every non-daemon thread had finished",
    );
}

/// A panic in a non-daemon thread doesn't deadlock the joiner —
/// `JoinHandle::join` returns `Err` but our wrapper swallows it
/// and continues.
#[test]
fn t19_k1_panic_in_non_daemon_does_not_deadlock_joiner() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(1);
    registry.register(tid, "panicker", None);
    let handle = std::thread::Builder::new()
        .name("k1-panicker".into())
        .spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            panic!("intentional panic for K1 test");
        })
        .expect("spawn");
    registry.set_join_handle(tid, handle);
    let start = std::time::Instant::now();
    let joined = registry.wait_for_non_daemon_threads(None);
    let elapsed = start.elapsed();
    assert_eq!(joined, 1, "panicked thread should still count as joined");
    // `joined == 1` is the property (the `Err` from `JoinHandle::join`
    // was swallowed and counted). The elapsed check only separates
    // "returned" from "deadlocked", so it is a hang backstop — the
    // former 5 s budget was observed failing on a loaded runner.
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "join must complete in bounded time even when target panicked: {:?}",
        elapsed,
    );
}

/// Mixed daemon + non-daemon: the wait only joins the non-daemon
/// threads. The daemon thread is left alive (its handle still in
/// the registry — caller is expected to abandon it on exit).
///
/// The property under test is WHICH threads were joined, not how long
/// the call took. The earlier version asserted
/// `start.elapsed() < 1s`, which is a fixed wall-clock bound over three
/// OS-thread spawns plus a 30 ms sleep; on a loaded CI runner the
/// spawns alone can blow it, so it flaked. Every assertion below is
/// timing-independent except one deliberately generous hang backstop:
///
/// * `joined == 1` — exactly one join succeeded.
/// * `user_finished` — the non-daemon's body ran to completion before
///   the wait returned (we really did wait for it).
/// * `!daemon_finished` — the daemon was still mid-flight when the
///   wait returned (we did *not* wait for it). The daemon is gated on
///   a flag this test owns, so this cannot be falsified by slowness:
///   load can only delay the observation, never open the gate.
/// * `is_alive(dmn)` / `!is_alive(user)` — only `join()` calls
///   `mark_dead`, so the alive flags are a direct readout of who was
///   joined, independent of wall clock.
#[test]
fn t19_k1_mixed_only_joins_non_daemons() {
    let registry = ThreadRegistry::new();
    let user = ThreadId(1);
    let dmn = ThreadId(2);
    registry.register_with_daemon(user, "user", None, false);
    registry.register_with_daemon(dmn, "dmn", None, true);

    let user_finished = Arc::new(AtomicBool::new(false));
    let daemon_finished = Arc::new(AtomicBool::new(false));
    // Gate that keeps the daemon running until this test releases it.
    let release_daemon = Arc::new(AtomicBool::new(false));

    let uf = user_finished.clone();
    let user_h = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(30));
        uf.store(true, Ordering::Release);
    });
    let df = daemon_finished.clone();
    let rd = release_daemon.clone();
    let dmn_h = std::thread::spawn(move || {
        // Long-running daemon — must NOT be joined. The 60 s cap is a
        // safety net so a regression fails instead of hanging forever.
        let cap = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !rd.load(Ordering::Acquire) && std::time::Instant::now() < cap {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        df.store(true, Ordering::Release);
    });
    registry.set_join_handle(user, user_h);
    registry.set_join_handle(dmn, dmn_h);

    let start = std::time::Instant::now();
    let joined = registry.wait_for_non_daemon_threads(None);
    let elapsed = start.elapsed();

    assert_eq!(joined, 1, "only the non-daemon thread may be joined");
    assert!(
        user_finished.load(Ordering::Acquire),
        "the non-daemon thread must have completed before the wait returned",
    );
    assert!(
        !daemon_finished.load(Ordering::Acquire),
        "the wait returned only after the daemon finished — it was waited for",
    );
    // Daemon is still alive (we didn't join it).
    assert!(registry.is_alive(dmn));
    // User thread is joined → marked dead.
    assert!(!registry.is_alive(user));
    // Hang backstop only. Must stay comfortably below the daemon's 60 s
    // cap so "joined the daemon anyway" still fails here rather than
    // sailing through; it is *not* a performance assertion.
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "wait_for_non_daemon_threads appears to have hung: {:?}",
        elapsed,
    );

    // Release and reap the daemon so it doesn't linger for the rest of
    // the test binary's life.
    release_daemon.store(true, Ordering::Release);
    assert!(registry.join(dmn), "daemon handle should still be present");
}

/// Deadline-bounded wait: when a non-daemon thread's
/// `JoinHandle` is missing (registered + alive but no
/// `set_join_handle` call) the inner `join()` call returns
/// `false` after a brief spin-wait. The deadline is checked
/// between iterations; here we use a deadline shorter than
/// the spin-wait would take to complete a fresh second
/// iteration. The wait returns within a bounded window even
/// though the alive flag never flipped.
#[test]
fn t19_k1_wait_respects_deadline_when_join_returns_false() {
    let registry = ThreadRegistry::new();
    // Spawn a real OS thread that runs for 50 ms then signals
    // the registry it's dead. We set the handle so the join
    // call has something to take.
    let tid = ThreadId(1);
    registry.register(tid, "dies-eventually", None);
    let handle = std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(50));
    });
    registry.set_join_handle(tid, handle);
    // Deadline is generous: we expect the thread to finish in
    // ~50 ms, the join to complete, and the loop to exit
    // before the deadline. This proves the deadline path
    // doesn't pessimize the happy path.
    //
    // `joined == 1` *is* the assertion: had the wait not completed
    // within the deadline, the loop would have bailed out and returned
    // 0. So the deadline itself is the bound under test, and it is set
    // generously (a loaded runner needs the slack — the former 5 s
    // deadline / 2 s assertion pair was observed failing). The elapsed
    // check below is only a hang backstop.
    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_secs(60);
    let joined = registry.wait_for_non_daemon_threads(Some(deadline));
    assert_eq!(
        joined, 1,
        "the happy path must complete within a generous deadline"
    );
    assert!(
        !registry.is_alive(tid),
        "the joined thread must be marked dead"
    );
    assert!(
        start.elapsed() < std::time::Duration::from_secs(30),
        "happy-path deadline should not extend execution: {:?}",
        start.elapsed(),
    );
}

/// `alive_non_daemon_thread_ids` reflects the current state at
/// the moment of the snapshot — once a thread is marked dead
/// it drops out of the enumeration.
#[test]
fn t19_k1_snapshot_excludes_dead_threads() {
    let registry = ThreadRegistry::new();
    let a = ThreadId(1);
    let b = ThreadId(2);
    registry.register(a, "a", None);
    registry.register(b, "b", None);
    assert_eq!(registry.alive_non_daemon_thread_ids().len(), 2);
    registry.mark_dead(a);
    assert_eq!(registry.alive_non_daemon_thread_ids(), vec![b]);
}

/// Re-snapshot after a non-daemon thread spawns more non-daemon
/// threads: the wait loop must catch the second batch even
/// though the first snapshot was already empty when those
/// threads were added.
#[test]
fn t19_k1_wait_picks_up_late_spawned_threads() {
    let registry = std::sync::Arc::new(ThreadRegistry::new());
    let tid_a = ThreadId(1);
    registry.register(tid_a, "a", None);

    // Thread A sleeps a bit, then registers thread B.
    let b_finished = Arc::new(AtomicBool::new(false));
    let bf = b_finished.clone();
    let reg2 = registry.clone();
    let handle_a = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(40));
        // Register a second non-daemon thread that runs for 60 ms.
        let tid_b = ThreadId(2);
        reg2.register(tid_b, "b", None);
        let h_b = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(60));
            bf.store(true, Ordering::Release);
        });
        reg2.set_join_handle(tid_b, h_b);
    });
    registry.set_join_handle(tid_a, handle_a);

    let joined = registry.wait_for_non_daemon_threads(None);
    // Both A and B must be joined. `joined == 2` plus B's completion
    // flag say "the re-snapshot caught B" directly; the former
    // `elapsed >= 80 ms` said it only by proxy and, like its siblings,
    // could be defeated by a loaded box finishing the sleeps before
    // the clock was even sampled.
    assert_eq!(joined, 2, "must catch late-spawned thread B");
    assert!(
        b_finished.load(Ordering::Acquire),
        "the wait returned before late-spawned thread B had finished",
    );
    assert!(!registry.is_alive(ThreadId(2)), "B must be joined + dead");
}

/// `register_with_daemon` and the legacy `register` must coexist —
/// the legacy path defaults to non-daemon, the explicit path
/// honours its argument.
#[test]
fn t19_k1_legacy_register_is_non_daemon_explicit_register_honors_flag() {
    let registry = ThreadRegistry::new();
    registry.register(ThreadId(1), "legacy", None);
    registry.register_with_daemon(ThreadId(2), "explicit-daemon", None, true);
    registry.register_with_daemon(ThreadId(3), "explicit-user", None, false);

    let mut ids = registry.alive_non_daemon_thread_ids();
    ids.sort_by_key(|t| t.0);
    assert_eq!(ids, vec![ThreadId(1), ThreadId(3)]);
}

/// Deadline-bounded wait exits before all threads are joined: an
/// expired deadline stops the loop, leaving live non-daemon threads
/// unjoined; the same registry then joins all of them once the
/// deadline is lifted.
///
/// The deadline is checked *between* joins — an in-flight
/// `JoinHandle::join` is not interruptible — so the property under
/// test is "the loop stopped handing out joins", not "the call took
/// less than N milliseconds".
///
/// The earlier version spawned five 30 ms sleepers with a 60 ms
/// deadline and asserted only `start.elapsed() < 1s`. That measured
/// nothing: the five sleepers run concurrently, so on an idle box all
/// five finish at ~30 ms and the deadline never fires at all, while on
/// a loaded CI runner five OS-thread spawns plus scheduling latency can
/// exceed 1 s outright — a pure machine-speed assertion, and a flaky
/// one. Here the workers are gated on a flag this test owns, so they
/// are guaranteed still alive and unjoinable while the deadline is
/// tested, and the outcome is decided by counters rather than a clock:
///
/// * `joined == 0` with all five still `is_alive` after the expired
///   deadline — only `join()` calls `mark_dead`, so this is a direct
///   timing-independent readout that nothing was joined. Had the
///   deadline been ignored, the call would have blocked on the gate
///   (and, after the workers' own safety cap, reported 5).
/// * `joined == 5` on the follow-up unbounded wait — proves the zero
///   above was the deadline's doing and not an empty snapshot.
#[test]
fn t19_k1_deadline_exits_before_all_joined() {
    let registry = ThreadRegistry::new();
    // Gate: the workers stay alive — and therefore unjoinable — until
    // this test releases them.
    let release = Arc::new(AtomicBool::new(false));
    let mut ids = Vec::new();
    for i in 0..5 {
        let tid = ThreadId(i as u64 + 1);
        registry.register(tid, &format!("u-{i}"), None);
        let rel = release.clone();
        let handle = std::thread::spawn(move || {
            // The 30 s cap is a safety net: a regression that ignores
            // the deadline fails loudly instead of hanging forever.
            let cap = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !rel.load(Ordering::Acquire) && std::time::Instant::now() < cap {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        });
        registry.set_join_handle(tid, handle);
        ids.push(tid);
    }

    // A deadline sampled *now* is already reached by the time the wait
    // checks it (`Instant` is monotonic, and the check is `>=`), so the
    // loop must bail out before its first join. No sleeping, no
    // race: this cannot be perturbed by machine load.
    let start = std::time::Instant::now();
    let deadline = std::time::Instant::now();
    let joined = registry.wait_for_non_daemon_threads(Some(deadline));
    let elapsed = start.elapsed();

    assert_eq!(
        joined, 0,
        "an already-expired deadline must stop the wait before any join",
    );
    for tid in &ids {
        assert!(
            registry.is_alive(*tid),
            "{tid:?} was joined despite the expired deadline",
        );
    }
    // Hang backstop only — must stay well under the workers' 30 s cap
    // so a deadline regression still fails here. Not a perf assertion.
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "deadline must bound the wait: {:?}",
        elapsed,
    );

    // Lift the gate: the very same registry now joins all five, which
    // is what makes the `0` above attributable to the deadline.
    release.store(true, Ordering::Release);
    assert_eq!(
        registry.wait_for_non_daemon_threads(None),
        5,
        "the unbounded wait must join every non-daemon thread",
    );
    for tid in &ids {
        assert!(!registry.is_alive(*tid), "{tid:?} should be joined + dead");
    }
}

// -----------------------------------------------------------------
// ARCH-2026-07-26 — registry contention
// -----------------------------------------------------------------

/// Registry reads are concurrent: many threads must be able to be inside
/// `is_alive` / `get_park_state` / `is_blocked` at the same instant.
///
/// Proven by holding a *read* guard on the map for the whole window and
/// requiring readers on other threads to complete anyway. Under the old
/// `Mutex` this would deadlock; under an `RwLock` the readers share.
#[test]
fn registry_reads_do_not_exclude_each_other() {
    let registry = Arc::new(ThreadRegistry::new());
    for i in 0..8u64 {
        registry.register(ThreadId(i), &format!("t{i}"), None);
    }

    let held = registry.threads.read();

    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let mut workers = Vec::new();
    for i in 0..8u64 {
        let r = registry.clone();
        let tx = tx.clone();
        workers.push(std::thread::spawn(move || {
            assert!(r.is_alive(ThreadId(i)));
            assert!(!r.is_blocked(ThreadId(i)));
            assert_eq!(r.java_block_state(ThreadId(i)), 0);
            assert!(r.get_park_state(ThreadId(i)).is_some());
            assert_eq!(r.thread_name(ThreadId(i)), Some(format!("t{i}")));
            assert_eq!(r.alive_count(), 8);
            let _ = tx.send(());
        }));
    }
    drop(tx);

    let mut done = 0;
    while done < 8 {
        if rx.recv_timeout(std::time::Duration::from_secs(10)).is_err() {
            break;
        }
        done += 1;
    }
    // Release before asserting/joining so a regression fails the assert
    // instead of hanging the suite.
    drop(held);
    for w in workers {
        w.join().unwrap();
    }
    assert_eq!(
        done, 8,
        "registry readers blocked each other — `threads` must be an RwLock \
         so concurrent lookups do not serialize"
    );
}

/// Registration still excludes readers: a write guard must block a reader,
/// which is what keeps the map's invariants intact across insert/remove.
#[test]
fn registry_writes_still_exclude_readers() {
    let registry = Arc::new(ThreadRegistry::new());
    registry.register(ThreadId(1), "t", None);

    let held = registry.threads.write();
    let r = registry.clone();
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let worker = std::thread::spawn(move || {
        let alive = r.is_alive(ThreadId(1));
        let _ = tx.send(());
        alive
    });

    let blocked = rx
        .recv_timeout(std::time::Duration::from_millis(200))
        .is_err();
    drop(held);
    let alive = worker.join().unwrap();
    assert!(blocked, "a writer must exclude readers");
    assert!(
        alive,
        "the reader must still get the right answer afterwards"
    );
}

/// The census reports EVERY thread parked in `Object.wait()`, not the one
/// the watchdog happened to reach first.
///
/// This is the property
/// `internal/fixed-suite-bugs/netty/parameterizedsslhandlertest-residual-stalls-FIXED-20261013` was
/// blocked on: with one waiter reported, "nothing completed this promise"
/// and "the thread that would have completed it is itself parked" are
/// indistinguishable. A thread that has LEFT the wait must not appear, or
/// the census would manufacture the second reading.
#[test]
fn waiting_monitor_census_lists_every_waiter_and_only_waiters() {
    let registry = ThreadRegistry::new();
    let mut b1 = [0u64; 2];
    let mut b2 = [0u64; 2];
    let mut b3 = [0u64; 2];
    let (m1, m2, m3) = (
        dummy_aligned_objref(&mut b1),
        dummy_aligned_objref(&mut b2),
        dummy_aligned_objref(&mut b3),
    );
    for (i, name) in ["main", "reactor-1", "reactor-2"].iter().enumerate() {
        registry.register(ThreadId(i as u64), name, None);
    }
    assert!(
        registry.waiting_monitor_census().is_empty(),
        "nobody is waiting yet"
    );

    registry.set_jmx_waiting_monitor(ThreadId(0), m1);
    registry.set_jmx_waiting_monitor(ThreadId(2), m3);
    let rows = registry.waiting_monitor_census();
    assert_eq!(rows.len(), 2, "both waiters, and only them: {rows:?}");
    assert_eq!(rows[0].0, 0);
    assert_eq!(rows[0].1, "main");
    assert!(std::ptr::eq(rows[0].3.as_ptr(), m1.as_ptr()));
    assert_eq!(rows[1].0, 2);
    assert!(std::ptr::eq(rows[1].3.as_ptr(), m3.as_ptr()));

    // A thread that leaves the wait drops out; one that enters appears.
    registry.take_jmx_waiting_monitor(ThreadId(0));
    registry.set_jmx_waiting_monitor(ThreadId(1), m2);
    let rows = registry.waiting_monitor_census();
    assert_eq!(
        rows.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![1, 2],
        "the census must track the slot, not a high-water mark"
    );
}

/// gc-common 2026-09-23 (lane A) — a carrier whose OS thread finished
/// without `mark_dead` (its closure unwound) must drop out of every
/// stop-the-world census and must not have its TLAB read, while its
/// Java-visible liveness is left to the ordinary paths.
#[test]
fn a_finished_carrier_that_skipped_mark_dead_is_not_stw_counted() {
    let registry = ThreadRegistry::new();
    let main = ThreadId(0);
    let worker = ThreadId(1);
    registry.register(main, "main", None);
    registry.register(worker, "worker", None);
    // A TLAB address that must never be dereferenced: if the census still
    // treated this entry as live, `collect_reserved_tlab_tails` would read
    // through it.
    registry.set_tlab_addr(worker, 0x8);
    let handle = std::thread::spawn(|| {});
    while !handle.is_finished() {
        std::thread::yield_now();
    }
    registry.set_join_handle(worker, handle);

    let (alive, blocked, _os, blocked_tids) = registry.alive_count_blocked_and_os_tids();
    assert_eq!(alive, 1, "only main may be counted");
    assert_eq!(blocked, 0);
    assert!(blocked_tids.is_empty());
    assert_eq!(registry.alive_count_and_os_tids().0, 1);
    assert_eq!(registry.alive_thread_ids_excluding(&[]), vec![0]);
    assert!(
        registry.collect_reserved_tlab_tails().is_empty(),
        "a finished carrier's TLAB address must not be dereferenced"
    );
    // Java-visible liveness is not this census's business.
    assert!(registry.is_alive(worker));
    // And the ordinary join path still retires it.
    assert!(registry.join(worker));
    assert!(!registry.is_alive(worker));
}

fn fold_obj(addr: usize) -> ObjectRef {
    // SAFETY: compared and rewritten by address only; never dereferenced.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn fold_addrs(snapshot: &[ObjectRef]) -> Vec<usize> {
    snapshot.iter().map(|r| r.as_ptr() as usize).collect()
}

/// gc-common w4-a (`handoff-w3g-blocked-fold-costs` edit 1): the reordered
/// per-thread fold (snapshot pass first, lazily built chained set, seeds
/// inserted after the compose) gives exactly the pre-w4 compose-then-seed
/// result, including the overlap where a chained object moves ONTO the
/// address a first-moving object vacates in the same map.
#[test]
fn the_blocked_fold_matches_compose_then_seed() {
    // Frames hold 0x100; an earlier pause moved it to 0x200 (the snapshot
    // holds the CUR value).
    let mut fixup = cratonvm_types::PointerMap::default();
    fixup.insert(0x100, 0x200);
    let mut snapshot = vec![fold_obj(0x200), fold_obj(0x300), fold_obj(0x400)];
    // This pause: the chained object 0x200 -> 0x300, a first-moving object
    // 0x300 -> 0x500, 0x400 stays.
    let mut pm = cratonvm_types::PointerMap::default();
    pm.insert(0x200, 0x300);
    pm.insert(0x300, 0x500);
    let (composed, seeded) = fold_one_blocked_chain(&mut fixup, &mut snapshot, &pm);
    assert_eq!((composed, seeded), (1, 1));
    // Advanced through THIS map once -- not on through 0x300 -> 0x500.
    assert_eq!(fixup.get(&0x100), Some(&0x300));
    // The first move is seeded with its new address and not composed.
    assert_eq!(fixup.get(&0x300), Some(&0x500));
    assert_eq!(fixup.len(), 2);
    assert_eq!(fold_addrs(&snapshot), vec![0x300, 0x500, 0x400]);
}

/// The ABA guard survives the reorder: a recycled address that is already
/// a chain KEY for an older object keeps that object's (composed) value.
/// And a pause that moves none of the snapshot seeds nothing.
#[test]
fn the_blocked_fold_keeps_the_aba_guard_and_skips_unmoved_snapshots() {
    let mut fixup = cratonvm_types::PointerMap::default();
    fixup.insert(0x600, 0x700);
    let mut snapshot = vec![fold_obj(0x600)];
    let mut pm = cratonvm_types::PointerMap::default();
    pm.insert(0x600, 0x800);
    pm.insert(0x700, 0x900);
    let (composed, seeded) = fold_one_blocked_chain(&mut fixup, &mut snapshot, &pm);
    assert_eq!((composed, seeded), (1, 1));
    assert_eq!(fixup.get(&0x600), Some(&0x900), "the older chain wins");
    assert_eq!(fold_addrs(&snapshot), vec![0x800]);

    // Nothing in the snapshot moves: only the chain composes.
    let mut fixup = cratonvm_types::PointerMap::default();
    fixup.insert(0x100, 0x200);
    let mut snapshot = vec![fold_obj(0x1000), fold_obj(0x2000)];
    let mut pm = cratonvm_types::PointerMap::default();
    pm.insert(0x200, 0x250);
    assert_eq!(
        fold_one_blocked_chain(&mut fixup, &mut snapshot, &pm),
        (1, 0)
    );
    assert_eq!(fixup.get(&0x100), Some(&0x250));
    assert_eq!(fold_addrs(&snapshot), vec![0x1000, 0x2000]);
}

/// JIT round 17 wave 2 (lane smallfix): a dead entry no longer names the
/// OS tid its thread had (the OS recycles it), so `os_tid_of` answers
/// `None` and no claimant query can match it.
#[test]
fn mark_dead_withdraws_the_os_tid() {
    let registry = ThreadRegistry::new();
    let t = ThreadId(1);
    registry.register(t, "dying", None);
    registry.publish_os_tid_claim(t, 4243);
    assert_eq!(registry.os_tid_of(t), Some(4243));
    registry.mark_dead(t);
    {
        assert_eq!(registry.os_tid_of(t), None);
    }
    assert_eq!(registry.thread_id_for_os_tid_inner(4243), None);
}

/// gci i9/ds (`gci-i9ds-frozen-peer-deopt-stash-not-rooted-RETIRED-RULING-20261018`):
/// publishing the calling thread's OS tid also publishes its JIT deopt
/// stash cells into the entry's `GcBlockState`, whichever order the
/// owner installs the state and the tid in.
#[test]
fn gci_i9ds_the_os_tid_claim_publishes_the_deopt_stash_cells() {
    let own = current_os_tid();
    let cells = cratonvm_jit::deopt::stash_cell_addrs().expect("live TLS");
    // State first, tid second (the usual startup order).
    let registry = ThreadRegistry::new();
    let t = ThreadId(1);
    registry.register(t, "stash-owner", None);
    let state = Arc::new(GcBlockState::new());
    registry.set_gc_block_state(t, state.clone());
    assert_eq!(state.published_deopt_stash_cells(), None, "no tid yet");
    registry.set_os_tid_current(t);
    if own == 0 {
        // No OS-tid backend on this target: nothing is published.
        assert_eq!(state.published_deopt_stash_cells(), None);
        return;
    }
    assert_eq!(state.published_deopt_stash_cells(), Some((own, cells.0, cells.1)));
    // Tid first, state second: the owner's install republishes.
    let later = Arc::new(GcBlockState::new());
    registry.set_gc_block_state(t, later.clone());
    assert_eq!(later.published_deopt_stash_cells(), Some((own, cells.0, cells.1)));
    // Another OS thread's install does not publish ITS cells under `own`.
    let foreign = Arc::new(GcBlockState::new());
    std::thread::scope(|s| {
        s.spawn(|| registry.set_gc_block_state(t, foreign.clone()));
    });
    assert_eq!(foreign.published_deopt_stash_cells(), None);
    // A different triple replaces the old one whole.
    later.publish_deopt_stash_cells(own.wrapping_add(1).max(1), 0x1000, 0x2000);
    assert_eq!(
        later.published_deopt_stash_cells(),
        Some((own.wrapping_add(1).max(1), 0x1000, 0x2000))
    );
    // Zero words are refused (nothing half-published).
    let empty = GcBlockState::new();
    empty.publish_deopt_stash_cells(own, 0, 0x2000);
    empty.publish_deopt_stash_cells(0, 0x1000, 0x2000);
    assert_eq!(empty.published_deopt_stash_cells(), None);
}

/// gc-common 2026-09-23 (lane A) — the blocked-thread fold must never
/// re-acquire `threads` while it holds it.
///
/// `parking_lot::RwLock` is task-fair: a reader blocks while a writer is
/// queued. The fold used to call `self.os_tid_of` (a second
/// `threads.read()`) inside its walk, so a writer queued between the two
/// acquisitions deadlocked the GC initiator against that writer. This
/// drives exactly that interleaving — a fold loop over a BLOCKED entry on
/// one thread, registry writes on another — and fails on a hang instead
/// of hanging the suite. Probabilistic by nature, but the window is hit
/// within a few thousand iterations on any multi-core machine.
#[test]
fn blocked_fold_never_reacquires_the_registry_lock() {
    let registry = Arc::new(ThreadRegistry::new());
    let blocked = ThreadId(1);
    registry.register(blocked, "blocked", None);
    {
        let threads = registry.threads.read();
        let entry = threads.get(&blocked).unwrap();
        entry
            .gc_block_state
            .in_blocked_region
            .store(true, Ordering::Release);
        entry.os_tid.store(4242, Ordering::Release);
    }
    let mut pm = cratonvm_types::PointerMap::default();
    pm.insert(0x1000, 0x2000);

    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let registry = registry.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                // Any `threads.write()` will do; an unknown id keeps the
                // registry contents unchanged.
                registry.set_gc_block_state(ThreadId(u64::MAX), Arc::new(GcBlockState::new()));
            }
        })
    };
    let (tx, rx) = std::sync::mpsc::channel();
    {
        let registry = registry.clone();
        std::thread::spawn(move || {
            for _ in 0..20_000 {
                registry.fold_pointer_map_into_blocked(&pm);
            }
            let _ = tx.send(());
        });
    }
    let finished = rx.recv_timeout(std::time::Duration::from_secs(60)).is_ok();
    stop.store(true, Ordering::Relaxed);
    if finished {
        writer.join().unwrap();
    }
    assert!(
        finished,
        "fold_pointer_map_into_blocked deadlocked against a queued registry writer"
    );
}

/// gen r5w1/crash5: a blocked-peer native-stack capture is adopted only by
/// the NEWEST claimant of the OS tid it was read from. Two alive, blocked
/// entries claim one tid here -- a parked virtual thread's stale carrier
/// claim and the continuation mounted there since -- and only the mounted
/// one may later write the word back. Before the fix both adopted it, and
/// the stale one wrote it from whichever carrier remounted it.
#[test]
fn a_native_stack_capture_is_adopted_only_by_the_newest_claimant_of_its_tid() {
    let registry = ThreadRegistry::new();
    let (stale, mounted, elsewhere) = (ThreadId(1), ThreadId(2), ThreadId(3));
    registry.register(stale, "parked-continuation", None);
    registry.register(mounted, "mounted-continuation", None);
    registry.register(elsewhere, "other-thread", None);
    // `stale` mounted on OS thread 4343 first, `mounted` after it.
    registry.publish_os_tid_claim(stale, 4343);
    registry.publish_os_tid_claim(mounted, 4343);
    registry.publish_os_tid_claim(elsewhere, 4344);
    {
        let threads = registry.threads.read();
        for id in [stale, mounted, elsewhere] {
            threads
                .get(&id)
                .unwrap()
                .gc_block_state
                .in_blocked_region
                .store(true, Ordering::Release);
        }
    }
    // This test thread's unbound pause ledger stands in for a pause's.
    cratonvm_gc::gc_quiescence::clear_peer_stack_slots();
    cratonvm_gc::gc_quiescence::record_peer_stack_slot(4343, 0x7a00_1000, 0x1000);
    cratonvm_gc::gc_quiescence::record_peer_stack_slot(4344, 0x7b00_2000, 0x3000);
    let declined_before = NATIVE_SLOTS_DECLINED_STALE_CLAIM.load(Ordering::Relaxed);
    let mut pm = cratonvm_types::PointerMap::default();
    pm.insert(0x1000, 0x2000);
    pm.insert(0x3000, 0x4000);
    registry.fold_pointer_map_into_blocked(&pm);

    let slots_of = |id: ThreadId| -> Vec<(usize, usize, usize)> {
        registry
            .threads
            .read()
            .get(&id)
            .unwrap()
            .gc_block_state
            .native_slots
            .lock()
            .iter()
            .map(|n| (n.addr, n.orig, n.cur))
            .collect()
    };
    if !cratonvm_gc::gc_quiescence::blocked_peer_stack_remap_enabled() {
        // `CRATONVM_GC_NO_BLOCKED_PEER_STACK_REMAP` set for this process:
        // nothing is captured or adopted, so there is nothing to route.
        assert!(slots_of(mounted).is_empty());
        return;
    }
    assert!(
        slots_of(stale).is_empty(),
        "a stale claimant adopts nothing of the stack it no longer runs on"
    );
    assert_eq!(slots_of(mounted), vec![(0x7a00_1000, 0x1000, 0x2000)]);
    assert_eq!(
        slots_of(elsewhere),
        vec![(0x7b00_2000, 0x3000, 0x4000)],
        "a sole claimant adopts its own capture exactly as before"
    );
    assert!(NATIVE_SLOTS_DECLINED_STALE_CLAIM.load(Ordering::Relaxed) > declined_before);
}

/// t1 wave 5 lane E2 (hazard 2 of i94 stage 2): while a continuation slice
/// runs on its carrier's OS thread, a capture at or above the published
/// slice base is adopted by the carrier (whose tid claim is withdrawn for
/// the slice), below it by the continuation (the claimant) -- never both.
#[test]
fn t1e2_a_slice_stack_capture_is_routed_by_the_segment_base() {
    let registry = ThreadRegistry::new();
    let (carrier, cont) = (ThreadId(11_4646), ThreadId(12_4646));
    let os_tid = 46_4646u32;
    registry.register(carrier, "carrier", None);
    registry.register(cont, "continuation", None);
    registry.publish_os_tid_claim(carrier, os_tid);
    registry.clear_os_tid(carrier);
    registry.publish_os_tid_claim(cont, os_tid);
    {
        let threads = registry.threads.read();
        for id in [carrier, cont] {
            threads
                .get(&id)
                .unwrap()
                .gc_block_state
                .in_blocked_region
                .store(true, Ordering::Release);
        }
    }
    let prev = cratonvm_gc::shadow_stack::publish_stack_segment(os_tid, 0x7c00_2000, carrier.0);
    cratonvm_gc::gc_quiescence::clear_peer_stack_slots();
    cratonvm_gc::gc_quiescence::record_peer_stack_slot(os_tid, 0x7c00_1000, 0x1000);
    cratonvm_gc::gc_quiescence::record_peer_stack_slot(os_tid, 0x7c00_3000, 0x3000);
    let mut pm = cratonvm_types::PointerMap::default();
    pm.insert(0x1000, 0x2000);
    pm.insert(0x3000, 0x4000);
    registry.fold_pointer_map_into_blocked(&pm);
    cratonvm_gc::shadow_stack::restore_stack_segment(os_tid, prev);
    let slots_of = |id: ThreadId| -> Vec<(usize, usize, usize)> {
        registry
            .threads
            .read()
            .get(&id)
            .unwrap()
            .gc_block_state
            .native_slots
            .lock()
            .iter()
            .map(|n| (n.addr, n.orig, n.cur))
            .collect()
    };
    if !cratonvm_gc::gc_quiescence::blocked_peer_stack_remap_enabled() {
        assert!(slots_of(cont).is_empty() && slots_of(carrier).is_empty());
        return;
    }
    assert_eq!(slots_of(cont), vec![(0x7c00_1000, 0x1000, 0x2000)], "the slice's half");
    assert_eq!(
        slots_of(carrier),
        vec![(0x7c00_3000, 0x3000, 0x4000)],
        "the carrier's half, adopted by the carrier alone"
    );
}

/// gen r5w2/roots6 (`gengc-r5w1-crash5-blocked-native-slot-captures-grow-
/// every-cycle`, item 1): a long-parked thread's stack word re-captured
/// with the SAME value -- by another barrier round of the same pause, or by
/// the next pause -- is adopted once, and keeps following its object
/// through every later map. A word whose value changed is a new capture.
#[test]
fn a_recaptured_native_stack_word_is_adopted_once() {
    let registry = ThreadRegistry::new();
    let parked = ThreadId(1);
    registry.register(parked, "long-parked", None);
    registry.publish_os_tid_claim(parked, 4545);
    registry
        .threads
        .read()
        .get(&parked)
        .unwrap()
        .gc_block_state
        .in_blocked_region
        .store(true, Ordering::Release);
    let slots = || -> Vec<(usize, usize, usize)> {
        registry
            .threads
            .read()
            .get(&parked)
            .unwrap()
            .gc_block_state
            .native_slots
            .lock()
            .iter()
            .map(|n| (n.addr, n.orig, n.cur))
            .collect()
    };
    let dup_before = native_slot_duplicate_captures();
    // Pause 1, two barrier rounds reading the same word.
    cratonvm_gc::gc_quiescence::clear_peer_stack_slots();
    cratonvm_gc::gc_quiescence::record_peer_stack_slot(4545, 0x7c00_1000, 0x5000);
    cratonvm_gc::gc_quiescence::record_peer_stack_slot(4545, 0x7c00_1000, 0x5000);
    let mut first = cratonvm_types::PointerMap::default();
    first.insert(0x5000, 0x6000);
    registry.fold_pointer_map_into_blocked(&first);
    if !cratonvm_gc::gc_quiescence::blocked_peer_stack_remap_enabled() {
        assert!(slots().is_empty());
        return;
    }
    assert_eq!(slots(), vec![(0x7c00_1000, 0x5000, 0x6000)]);
    // Pause 2: still parked, so the word still reads 0x5000; beside it a
    // word whose value changed since.
    cratonvm_gc::gc_quiescence::record_peer_stack_slot(4545, 0x7c00_1000, 0x5000);
    cratonvm_gc::gc_quiescence::record_peer_stack_slot(4545, 0x7c00_1008, 0x7000);
    let mut second = cratonvm_types::PointerMap::default();
    second.insert(0x6000, 0x8000);
    second.insert(0x7000, 0x9000);
    registry.fold_pointer_map_into_blocked(&second);
    assert_eq!(
        slots(),
        vec![(0x7c00_1000, 0x5000, 0x8000), (0x7c00_1008, 0x7000, 0x9000)],
        "one entry per (address, value), each composed through both maps"
    );
    assert!(native_slot_duplicate_captures() >= dup_before + 2);
}

/// gen r5w2/roots6 (`gengc-r5w1-crash5-parked-virtual-threads-keep-their-
/// carriers-os-tid`): an UNMOUNTED virtual thread still carries its last
/// carrier's OS tid, but it is not blocked on that carrier (the
/// helper-window pass must not signal it for its sake) and not frozen on
/// it (a take-over must not walk its heap frames as the frozen stack's).
#[test]
fn an_unmounted_virtual_thread_is_neither_blocked_nor_frozen_on_its_last_carrier() {
    let registry = ThreadRegistry::new();
    let (parked, mounted) = (ThreadId(1), ThreadId(2));
    registry.register(parked, "vt-parked", None);
    registry.register(mounted, "vt-mounted", None);
    registry.publish_os_tid_claim(parked, 4646);
    registry.publish_os_tid_claim(mounted, 4646);
    {
        let threads = registry.threads.read();
        for id in [parked, mounted] {
            threads
                .get(&id)
                .unwrap()
                .gc_block_state
                .in_blocked_region
                .store(true, Ordering::Release);
        }
    }
    registry.set_jvm_thread_addr(parked, 0x1000);
    registry.set_jvm_thread_addr(mounted, 0x2000);
    assert!(registry.is_mounted(parked), "an entry starts mounted");
    registry.set_mounted(parked, false);
    assert!(!registry.is_mounted(parked));
    assert_eq!(
        registry.blocked_os_tids(),
        vec![4646],
        "only the mounted one"
    );
    assert_eq!(registry.frozen_peer_thread_addrs(&[4646]), vec![0x2000]);
    registry.set_mounted(mounted, false);
    assert!(registry.blocked_os_tids().is_empty());
    assert!(registry.frozen_peer_thread_addrs(&[4646]).is_empty());
    registry.set_mounted(parked, true);
    assert_eq!(
        registry.frozen_peer_thread_addrs(&[4646]),
        vec![0x1000],
        "remounted"
    );
}

/// JIT round 16 wave 9b (lane ffm,
/// `r16-ffm-takeover-signal-interrupts-blocking-syscalls-in-native`): a
/// thread blocked in foreign code is left out of the OS tids a pause
/// signals (the helper-window list, its monitor-proof split, the take-over
/// roster) but stays in the alive count; foreign code that is not (yet) in
/// its region, or a region that is not foreign, is signalled as before.
#[test]
fn a_thread_in_foreign_code_is_not_signalled_but_still_counted() {
    let registry = ThreadRegistry::new();
    let (ffm, sleeper) = (ThreadId(1), ThreadId(2));
    registry.register(ffm, "ffm", None);
    registry.register(sleeper, "sleeper", None);
    registry.publish_os_tid_claim(ffm, 5151);
    registry.publish_os_tid_claim(sleeper, 5252);
    registry.mark_stw_ready(ffm);
    registry.mark_stw_ready(sleeper);
    let state = |id: ThreadId| {
        registry
            .threads
            .read()
            .get(&id)
            .unwrap()
            .gc_block_state
            .clone()
    };
    for id in [ffm, sleeper] {
        state(id).in_blocked_region.store(true, Ordering::Release);
    }
    let sorted = |mut v: Vec<u32>| {
        v.sort_unstable();
        v
    };
    assert_eq!(sorted(registry.blocked_os_tids()), vec![5151, 5252]);
    state(ffm).foreign_native.store(true, Ordering::Release);
    assert_eq!(
        registry.blocked_os_tids(),
        vec![5252],
        "the foreign one is exempt"
    );
    let (rest, proven) = registry.blocked_os_tids_split_by_monitor_proof();
    assert_eq!((rest, proven), (vec![5252], Vec::new()));
    let (alive, roster) = registry.alive_count_and_os_tids();
    assert_eq!(alive, 2, "still counted alive");
    assert_eq!(roster, vec![5252]);
    // Raised before the deposit, region not up yet: a running mutator.
    state(ffm).in_blocked_region.store(false, Ordering::Release);
    assert_eq!(
        sorted(registry.alive_count_and_os_tids().1),
        vec![5151, 5252]
    );
    state(ffm).foreign_native.store(false, Ordering::Release);
    state(ffm).in_blocked_region.store(true, Ordering::Release);
    assert_eq!(sorted(registry.blocked_os_tids()), vec![5151, 5252]);
}

/// gci i2/jn (i1 review R5): `CRATONVM_FFM_IN_NATIVE_NO_SIGNAL` is read
/// once, into the registry, and all three roster reads use that value:
/// default on; with it off (`=0`, here the field) a thread in foreign code
/// is listed and signalled again by every one of them.
#[test]
fn gci_i2jn_ffm_no_signal_is_latched_per_registry() {
    assert!(ThreadRegistry::new().ffm_in_native_no_signal, "default on");
    let mut registry = ThreadRegistry::new();
    registry.ffm_in_native_no_signal = false;
    let (ffm, sleeper) = (ThreadId(1), ThreadId(2));
    registry.register(ffm, "ffm", None);
    registry.register(sleeper, "sleeper", None);
    registry.publish_os_tid_claim(ffm, 5151);
    registry.publish_os_tid_claim(sleeper, 5252);
    registry.mark_stw_ready(ffm);
    registry.mark_stw_ready(sleeper);
    for id in [ffm, sleeper] {
        let threads = registry.threads.read();
        let block = &threads.get(&id).unwrap().gc_block_state;
        block.in_blocked_region.store(true, Ordering::Release);
        if id == ffm {
            block.foreign_native.store(true, Ordering::Release);
        }
    }
    let sorted = |mut v: Vec<u32>| {
        v.sort_unstable();
        v
    };
    assert_eq!(sorted(registry.blocked_os_tids()), vec![5151, 5252]);
    assert_eq!(sorted(registry.blocked_os_tids_split_by_monitor_proof().0), vec![5151, 5252]);
    let (alive, roster) = registry.alive_count_and_os_tids();
    assert_eq!((alive, sorted(roster)), (2, vec![5151, 5252]));
}

/// gch h4/bp (`CRATONVM_JNI_IN_NATIVE_BAND_AT_PAUSE`): a JNI in-native
/// thread's band is read only while its region is up (whatever its
/// foreign-native mark says) and the band passes the bounds; each band
/// once; every word the probe accepts is pushed, in band order.
#[test]
fn gch_h4bp_in_native_bands_are_read_only_inside_the_bracket() {
    let registry = ThreadRegistry::new();
    let (a, b) = (ThreadId(1), ThreadId(2));
    registry.register(a, "jni", None);
    registry.register(b, "other", None);
    let state = |id: ThreadId| {
        registry
            .threads
            .read()
            .get(&id)
            .unwrap()
            .gc_block_state
            .clone()
    };
    let set_band = |s: &GcBlockState, lo: usize, hi: usize| {
        s.in_native_band_lo.store(lo, Ordering::Relaxed);
        s.in_native_band_hi.store(hi, Ordering::Relaxed);
    };
    // The "stack": a local array, words 0x5150 / 0x5158 standing for objects.
    let band: [usize; 8] = [0, 0x5150, 7, 0x5150, 0x5158, 0, 1, 0x5150];
    let lo = std::hint::black_box(&band) as *const [usize; 8] as usize;
    let hi = lo + std::mem::size_of::<[usize; 8]>();
    let probe = |w: usize| (w == 0x5150 || w == 0x5158).then(|| fake_synchronizer(w));
    let mut out = Vec::new();
    let none = InNativeBandScan::default();
    assert_eq!(
        registry.scan_in_native_bands(&probe, &mut out),
        none,
        "nobody in native"
    );
    let (sa, sb) = (state(a), state(b));
    set_band(&*sa, lo, hi);
    assert_eq!(
        registry.scan_in_native_bands(&probe, &mut out),
        none,
        "a band alone"
    );
    sa.foreign_native.store(true, Ordering::Release);
    assert_eq!(
        registry.scan_in_native_bands(&probe, &mut out),
        none,
        "in C, region not up yet"
    );
    sa.in_blocked_region.store(true, Ordering::Release);
    let scan = registry.scan_in_native_bands(&probe, &mut out);
    assert_eq!(
        scan,
        InNativeBandScan {
            threads: 1,
            words: 8,
            roots: 4
        }
    );
    assert_eq!(
        out.iter().map(|o| o.as_ptr() as usize).collect::<Vec<_>>(),
        vec![0x5150, 0x5150, 0x5158, 0x5150]
    );
    // gch h4/bp (vh4-bp B on h4a): the leave lowers `foreign_native` FIRST
    // and then waits out the pause with the region still up; the band must
    // still be read there.
    sa.foreign_native.store(false, Ordering::Release);
    out.clear();
    assert_eq!(
        registry.scan_in_native_bands(&probe, &mut out).threads,
        1,
        "waiting at the leave: the mark is down, the region is up"
    );
    assert_eq!(out.len(), 4);
    // A second entry on the same band (never expected) is not read twice.
    set_band(&*sb, lo, hi);
    sb.in_blocked_region.store(true, Ordering::Release);
    sb.foreign_native.store(true, Ordering::Release);
    out.clear();
    assert_eq!(registry.scan_in_native_bands(&probe, &mut out).threads, 1);
    assert_eq!(out.len(), 4);
    // A band that fails the bounds is never read; a withdrawn one neither.
    set_band(&*sb, lo + 4, hi);
    set_band(&*sa, 0, 0);
    out.clear();
    assert_eq!(registry.scan_in_native_bands(&probe, &mut out), none);
    assert!(out.is_empty());
    std::hint::black_box(&band);
}

/// gcj j2/jn2 (`CRATONVM_JNI_IN_NATIVE_KEEP_TLAB`, cut 2 of the JNI
/// transition ledger): the pause retires a kept TLAB only while its owner
/// is in native (region up) with the mark up, exactly once (the swap), and
/// leaves a mark whose region is down to the owner's leave; a retired TLAB
/// publishes no reserved tail.
#[test]
fn gcj2jn2_a_kept_in_native_tlab_is_retired_once_and_only_inside_the_bracket() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(1);
    registry.register(tid, "jni", None);
    let state = registry
        .threads
        .read()
        .get(&tid)
        .unwrap()
        .gc_block_state
        .clone();
    let mut backing = vec![0u64; 128];
    let ptr = backing.as_mut_ptr() as *mut u8;
    let size = backing.len() * std::mem::size_of::<u64>();
    let mut tlab = unsafe { cratonvm_gc::Tlab::new(ptr, size) };
    assert!(tlab.alloc(64, 8).is_some());
    let p = std::ptr::addr_of_mut!(tlab);
    registry.set_tlab_addr(tid, p as usize);
    // SAFETY (every read below): `p` is the live local `Tlab`, read only
    // between the registry's calls.
    let tail = || unsafe { (*p).reserved_tail() };
    assert_eq!(tail(), Some((ptr as usize + 64, ptr as usize + size)));
    // No mark: never retired, region up or not.
    assert_eq!(registry.retire_in_native_kept_tlabs(), 0);
    state.in_blocked_region.store(true, Ordering::Release);
    assert_eq!(registry.retire_in_native_kept_tlabs(), 0);
    assert!(tail().is_some());
    // The mark with the region down (the owner left native): the owner's.
    state.in_blocked_region.store(false, Ordering::Release);
    state.in_native_tlab_kept.store(true, Ordering::Release);
    assert_eq!(registry.retire_in_native_kept_tlabs(), 0);
    assert!(state.in_native_tlab_kept.load(Ordering::Acquire), "left to the leave");
    assert!(tail().is_some());
    // Both up: retired once, the mark taken.
    state.in_blocked_region.store(true, Ordering::Release);
    assert_eq!(registry.retire_in_native_kept_tlabs(), 1);
    assert!(!state.in_native_tlab_kept.load(Ordering::Acquire));
    assert_eq!(tail(), None, "retired");
    assert!(registry.collect_reserved_tlab_tails().is_empty());
    assert_eq!(registry.retire_in_native_kept_tlabs(), 0, "once");
    state.in_blocked_region.store(false, Ordering::Release);
    std::hint::black_box(&backing);
}

/// gci i24/bp (`CRATONVM_JNI_IN_NATIVE_PROVEN_DEPTH_CREDIT`): the pause
/// credits a proven in-native band once per pause stamp, only inside the
/// bracket, only with a proven depth equal to the published one, never a
/// second band on one address range; its young words reach the ledger raw
/// and in band order; the uncredited count follows every case.
#[test]
fn gci_i24bp_proven_in_native_bands_are_credited_once_per_pause() {
    let mut registry = ThreadRegistry::new();
    // The registry's exemption (`CRATONVM_FFM_IN_NATIVE_NO_SIGNAL`, default
    // ON) whatever this process's environment says.
    registry.ffm_in_native_no_signal = true;
    let (a, b) = (ThreadId(1), ThreadId(2));
    registry.register(a, "jni", None);
    registry.register(b, "jni2", None);
    let state = |id: ThreadId| {
        registry
            .threads
            .read()
            .get(&id)
            .unwrap()
            .gc_block_state
            .clone()
    };
    let set_os_tid = |id: ThreadId, t: u32| {
        registry
            .threads
            .read()
            .get(&id)
            .unwrap()
            .os_tid
            .store(t, Ordering::Release);
    };
    set_os_tid(a, 9101);
    set_os_tid(b, 9102);
    let set_band = |s: &GcBlockState, lo: usize, hi: usize| {
        s.in_native_band_lo.store(lo, Ordering::Relaxed);
        s.in_native_band_hi.store(hi, Ordering::Relaxed);
    };
    // The "stack": three words in the young range, three outside it.
    let band: [usize; 6] = [0x7000_0010, 3, 0x7000_0020, 0, 0x6000_0000, 0x7000_0018];
    let lo = std::hint::black_box(&band) as *const [usize; 6] as usize;
    let hi = lo + std::mem::size_of::<[usize; 6]>();
    let young = |w: usize| (0x7000_0000..0x7000_1000).contains(&w);
    let published = |t: u32| -> Option<usize> {
        match t {
            9101 => Some(3),
            9102 => Some(2),
            _ => None,
        }
    };
    let run = |stamp: u64| {
        let mut got: Vec<(Vec<usize>, usize)> = Vec::new();
        let done = registry.credit_in_native_proven_bands(
            stamp,
            &published,
            &young,
            &mut |w: &[usize], d: usize| got.push((w.to_vec(), d)),
        );
        (done, got)
    };
    let none = InNativeCredit::default();
    let (sa, sb) = (state(a), state(b));
    set_band(&*sa, lo, hi);
    assert_eq!(run(5), (none, Vec::new()), "a band with the region down");
    sa.in_blocked_region.store(true, Ordering::Release);
    sa.in_native_proven_jit_depth.store(3, Ordering::Release);
    assert_eq!(
        run(5).0,
        InNativeCredit {
            not_exempt: 1,
            ..none
        },
        "the mark down (a leave waiting out the pause): a helper window's, not ours"
    );
    assert_eq!(registry.uncredited_in_native_bands(5), 1);
    sa.foreign_native.store(true, Ordering::Release);
    sa.in_native_proven_jit_depth.store(0, Ordering::Release);
    assert_eq!(
        run(5).0,
        InNativeCredit {
            unproven: 1,
            ..none
        },
        "no proven depth"
    );
    assert_eq!(registry.uncredited_in_native_bands(5), 1);
    sa.in_native_proven_jit_depth.store(3, Ordering::Release);
    assert_eq!(run(0), (none, Vec::new()), "no pause stamp: nothing");
    let (done, got) = run(5);
    assert_eq!(
        done,
        InNativeCredit {
            credited: 1,
            depth: 3,
            words: 3,
            ..none
        }
    );
    assert_eq!(got, vec![(vec![0x7000_0010, 0x7000_0020, 0x7000_0018], 3)]);
    assert_eq!(registry.uncredited_in_native_bands(5), 0, "credited for 5");
    assert_eq!(registry.uncredited_in_native_bands(6), 1, "not for the next pause");
    assert_eq!(registry.uncredited_in_native_bands(0), 1, "stamp 0 counts every band");
    assert_eq!(
        run(5),
        (
            InNativeCredit {
                already: 1,
                ..none
            },
            Vec::new()
        ),
        "a second gathering in the same pause credits nothing again"
    );
    // A proof that does not match the published depth is never credited.
    sa.in_native_proven_jit_depth.store(4, Ordering::Release);
    assert_eq!(
        run(6).0,
        InNativeCredit {
            mismatched: 1,
            ..none
        }
    );
    assert_eq!(registry.uncredited_in_native_bands(6), 1);
    // Two entries on one band: the first is credited, the second is not.
    sa.in_native_proven_jit_depth.store(3, Ordering::Release);
    set_band(&*sb, lo, hi);
    sb.in_native_proven_jit_depth.store(2, Ordering::Release);
    sb.foreign_native.store(true, Ordering::Release);
    sb.in_blocked_region.store(true, Ordering::Release);
    let (done, got) = run(7);
    assert_eq!((done.credited, done.mismatched), (1, 1));
    assert_eq!(got.len(), 1);
    assert_eq!(registry.uncredited_in_native_bands(7), 1);
    std::hint::black_box(&band);
}

/// gci i24/bp: the credit rule (pure).
#[test]
fn gci_i24bp_in_native_credit_rule() {
    assert!(in_native_credit_admissible(3, 9101, Some(3), false));
    assert!(!in_native_credit_admissible(0, 9101, Some(0), false), "nothing proven");
    assert!(!in_native_credit_admissible(3, 0, Some(3), false), "unknown OS thread");
    assert!(!in_native_credit_admissible(3, 9101, None, false), "no published depth");
    assert!(!in_native_credit_admissible(3, 9101, Some(2), false), "an over-credit");
    assert!(!in_native_credit_admissible(2, 9101, Some(3), false), "a short proof");
    assert!(!in_native_credit_admissible(3, 9101, Some(3), true), "a duplicate");
}

/// JIT round 16 wave 14 (lane ffm3): only a thread in a lazy in-native
/// downcall is claimed, once, with its band; a claimed thread's leave
/// waits for the release; a tid already frozen by the take-over, an
/// unpublished `JvmThread` and an unmounted entry are left alone.
#[test]
fn only_threads_in_a_lazy_downcall_are_claimed() {
    use crate::threading::jvm_thread::{LAZY_NATIVE_CLAIMED, LAZY_NATIVE_IN};
    let registry = ThreadRegistry::new();
    let (a, b, c, d) = (ThreadId(1), ThreadId(2), ThreadId(3), ThreadId(4));
    for (id, tid) in [(a, 7101u32), (b, 7102), (c, 7103), (d, 7104)] {
        registry.register(id, "t", None);
        registry.publish_os_tid_claim(id, tid);
        registry.mark_stw_ready(id);
        registry.set_jvm_thread_addr(id, 0x1000 * tid as usize);
    }
    let state = |id: ThreadId| {
        registry
            .threads
            .read()
            .get(&id)
            .map(|e| e.gc_block_state.clone())
            .unwrap_or_default()
    };
    assert!(
        registry.claim_lazy_native_peers(&[]).is_empty(),
        "nobody in C"
    );
    state(a).enter_lazy_native(0x10_000, 0x20_000);
    state(b).enter_lazy_native(0x30_000, 0x40_000);
    state(c).enter_lazy_native(0x50_000, 0x60_000);
    registry.set_mounted(c, false);
    // `b` was already frozen by the take-over this pause.
    let claims = registry.claim_lazy_native_peers(&[7102]);
    assert_eq!(claims.len(), 1);
    assert_eq!(
        (claims[0].os_tid, claims[0].lo, claims[0].hi),
        (7101, 0x10_000, 0x20_000)
    );
    assert_eq!(
        state(a).lazy_native.load(Ordering::Acquire),
        LAZY_NATIVE_CLAIMED
    );
    assert_eq!(state(b).lazy_native.load(Ordering::Acquire), LAZY_NATIVE_IN);
    assert_eq!(state(c).lazy_native.load(Ordering::Acquire), LAZY_NATIVE_IN);
    assert!(
        registry
            .claim_lazy_native_peers(&[])
            .iter()
            .all(|c| c.os_tid != 7101),
        "a claimed thread is not claimed twice"
    );
    assert!(claims[0].state.release_lazy_native_claim());
    assert_eq!(state(a).lazy_native.load(Ordering::Acquire), LAZY_NATIVE_IN);
    assert!(!state(a).leave_lazy_native());
}

/// JIT round 19 wave 1 (lane eintr19, FFMEINTR18-1): a frame-trace
/// pause can claim peers with either interpreted or compiled activations;
/// trace publication reads the held owner's published state.
#[test]
fn r19_frame_trace_claim_admits_compiled_callers() {
    use crate::threading::jvm_thread::LAZY_NATIVE_CLAIMED;
    let registry = ThreadRegistry::new();
    let (a, b) = (ThreadId(1), ThreadId(2));
    for (id, tid) in [(a, 7201u32), (b, 7202)] {
        registry.register(id, "t", None);
        registry.publish_os_tid_claim(id, tid);
        registry.mark_stw_ready(id);
        registry.set_jvm_thread_addr(id, 0x1000 * tid as usize);
    }
    let state = |id: ThreadId| {
        registry
            .threads
            .read()
            .get(&id)
            .map(|e| e.gc_block_state.clone())
            .unwrap_or_default()
    };
    state(a).enter_lazy_native(0x10_000, 0x20_000);
    state(b).enter_lazy_native(0x30_000, 0x40_000);
    let claims = registry.claim_lazy_native_peers(&[]);
    assert_eq!(
        claims.iter().map(|c| c.os_tid).collect::<Vec<_>>(),
        vec![7201, 7202]
    );
    assert!(claims
        .iter()
        .all(|c| c.state.lazy_native.load(Ordering::Acquire) == LAZY_NATIVE_CLAIMED));
    for claim in &claims {
        assert!(claim.state.release_lazy_native_claim());
    }
}

/// JIT round 19 wave 1 (lane eintr19, FFMEINTR18-4): an OS tid counts as
/// blocked only when every live entry on it is blocked (a mounted virtual
/// thread shares its carrier's tid).
#[test]
fn r19_eintr19_a_tid_is_blocked_only_when_every_entry_on_it_is() {
    let registry = ThreadRegistry::new();
    let (carrier, mounted, other) = (ThreadId(1), ThreadId(2), ThreadId(3));
    for (id, tid) in [(carrier, 7301u32), (mounted, 7301), (other, 7302)] {
        registry.register(id, "t", None);
        registry.publish_os_tid_claim(id, tid);
        registry.mark_stw_ready(id);
    }
    let state = |id: ThreadId| {
        registry
            .threads
            .read()
            .get(&id)
            .map(|e| e.gc_block_state.clone())
            .unwrap_or_default()
    };
    assert!(registry.blocked_among_os_tids(&[]).is_empty());
    assert!(registry.blocked_among_os_tids(&[7301, 7302]).is_empty());
    state(carrier)
        .in_blocked_region
        .store(true, Ordering::Release);
    state(other)
        .in_blocked_region
        .store(true, Ordering::Release);
    assert_eq!(registry.blocked_among_os_tids(&[7301, 7302]), vec![7302]);
    state(mounted)
        .in_blocked_region
        .store(true, Ordering::Release);
    let mut both = registry.blocked_among_os_tids(&[7301, 7302]);
    both.sort_unstable();
    assert_eq!(both, vec![7301, 7302]);
    assert_eq!(registry.blocked_among_os_tids(&[7302]), vec![7302]);
}

/// gcj j2/cm2 (j1-cm registry residuals item 6): the sorted form answers
/// in ascending order, each tid once, whatever the order and repeats of
/// the asked tids; an unasked or unpublished tid is never answered.
#[test]
fn gcj2cm2_blocked_among_os_tids_is_sorted_and_deduplicated() {
    let registry = ThreadRegistry::new();
    let rows = [
        (ThreadId(1), 7403u32, true),
        (ThreadId(2), 7401, true),
        (ThreadId(3), 7402, false),
        (ThreadId(4), 7401, true),
        (ThreadId(5), 7404, true),
    ];
    for (id, tid, _) in rows {
        registry.register(id, "t", None);
        registry.publish_os_tid_claim(id, tid);
        registry.mark_stw_ready(id);
    }
    for (id, _, blocked) in rows {
        let state = registry
            .threads
            .read()
            .get(&id)
            .map(|e| e.gc_block_state.clone())
            .unwrap_or_default();
        state.in_blocked_region.store(blocked, Ordering::Release);
    }
    assert_eq!(
        registry.blocked_among_os_tids(&[7403, 7402, 7401, 7401, 9999]),
        vec![7401, 7403]
    );
    assert_eq!(registry.blocked_among_os_tids(&[7404]), vec![7404]);
    assert!(registry.blocked_among_os_tids(&[7402]).is_empty());
}


/// Interpreter round i2 wave 58, lane L6: a handshake pause excuses only
/// counted threads raised in a running native -- never the initiator (by
/// id or by OS thread), a blocked one or an unmounted one -- once, and a
/// frame-trace pause only a listable one. An excused thread's lowering
/// waits for the release.
#[test]
fn a_handshake_census_excuses_counted_threads_in_a_running_native() {
    use crate::threading::jvm_thread::{NativeExcuse, NX_EXCUSED};
    let registry = ThreadRegistry::new();
    let ids = [
        ThreadId(1),
        ThreadId(2),
        ThreadId(3),
        ThreadId(4),
        ThreadId(5),
    ];
    for (i, id) in ids.iter().enumerate() {
        registry.register(*id, "t", None);
        registry.publish_os_tid_claim(*id, 8101 + i as u32);
        registry.mark_stw_ready(*id);
    }
    let state = |id: ThreadId| {
        registry
            .threads
            .read()
            .get(&id)
            .map(|e| e.gc_block_state.clone())
            .unwrap_or_default()
    };
    let listable = NativeExcuse::raised(true, 4);
    let opaque = NativeExcuse::raised(false, 4);
    for id in ids {
        state(id).native_excuse.raise(listable);
    }
    state(ThreadId(2)).native_excuse.lower(listable);
    state(ThreadId(2)).native_excuse.raise(opaque);
    state(ThreadId(3))
        .in_blocked_region
        .store(true, Ordering::Release);
    registry.set_mounted(ThreadId(4), false);
    // Initiator 1; 5 runs on the calling OS thread.
    let trace = registry.excuse_running_natives_on(ThreadId(1), 8105, true);
    assert!(
        trace.is_empty(),
        "only the opaque 2 is left, and it is not listable"
    );
    let loop_exit = registry.excuse_running_natives_on(ThreadId(1), 8105, false);
    assert_eq!(loop_exit.len(), 1);
    assert_eq!((loop_exit[0].tid, loop_exit[0].os_tid), (ThreadId(2), 8102));
    assert_eq!(loop_exit[0].word, opaque);
    assert_eq!(state(ThreadId(2)).native_excuse.load(), opaque | NX_EXCUSED);
    assert!(
        registry
            .excuse_running_natives_on(ThreadId(1), 8105, false)
            .is_empty(),
        "excused once"
    );
    // The thread's lowering waits for the release.
    let s2 = state(ThreadId(2));
    let waiter = std::thread::spawn(move || s2.native_excuse.lower(opaque));
    let t0 = std::time::Instant::now();
    while t0.elapsed() < std::time::Duration::from_millis(30) {
        std::thread::yield_now();
    }
    assert_eq!(state(ThreadId(2)).native_excuse.load(), opaque | NX_EXCUSED);
    loop_exit[0].state.native_excuse.release();
    assert!(waiter.join().unwrap(), "it waited");
    assert_eq!(state(ThreadId(2)).native_excuse.load(), 0);
    // A listable thread, with no initiator in the registry.
    let trace = registry.excuse_running_natives_on(ThreadId(u64::MAX), 0, true);
    let mut tids: Vec<u64> = trace.iter().map(|x| x.tid.0).collect();
    tids.sort_unstable();
    assert_eq!(tids, vec![1, 5]);
}

/// gen r5w2/roots6 (`gengc-r5w1-crash5-an-unwinding-carrier-publishes-a-
/// dropped-jvmthread`): once the `JvmThread` whose `Tlab` an entry
/// publishes is DROPPED -- as an unwinding carrier's closure drops it,
/// with the entry still alive and the address still published -- no
/// collector reader dereferences it: `collect_reserved_tlab_tails` reports
/// no tail and `frozen_peer_thread_addrs` no address.
#[test]
fn a_dropped_jvm_thread_publishes_nothing_to_the_collector() {
    let registry = ThreadRegistry::new();
    let tid = ThreadId(7);
    registry.register(tid, "unwinding", None);
    registry.publish_os_tid_claim(tid, 4747);
    let mut backing = vec![0u64; 128];
    let ptr = backing.as_mut_ptr() as *mut u8;
    let size = backing.len() * std::mem::size_of::<u64>();
    // The published `Tlab` stands in for the dropped frame's: it stays
    // readable here, so a reader that ignored the flag would report it.
    // SAFETY: `backing` outlives `tlab` and is `size` bytes.
    let mut tlab = unsafe { cratonvm_gc::Tlab::new(ptr, size) };
    assert!(tlab.alloc(64, 8).is_some());
    {
        let thread = crate::threading::jvm_thread::JvmThread::new(tid, "unwinding");
        registry.set_gc_block_state(tid, thread.gc_block_state.clone());
        registry.set_tlab_addr(tid, &tlab as *const cratonvm_gc::Tlab as usize);
        registry.set_jvm_thread_addr(tid, &thread as *const _ as usize);
        assert_eq!(
            registry.collect_reserved_tlab_tails(),
            vec![(ptr as usize + 64, ptr as usize + size)],
            "live: the tail is published"
        );
        assert_eq!(registry.frozen_peer_thread_addrs(&[4747]).len(), 1);
        // `thread` drops here, as the unwind drops it.
    }
    assert!(
        registry.collect_reserved_tlab_tails().is_empty(),
        "dropped: nothing"
    );
    assert!(registry.frozen_peer_thread_addrs(&[4747]).is_empty());
    tlab.retire();
    drop(backing);
}

fn publish_os_tid(registry: &ThreadRegistry, tid: ThreadId, os_tid: u32) {
    registry
        .threads
        .read()
        .get(&tid)
        .expect("registered")
        .os_tid
        .store(os_tid, Ordering::Release);
}

/// gc-common w3-a — the census says whether the initiator owns a counted
/// slot, by its own id or by OS thread, and says "unknown" whenever a
/// counted entry could be the caller.
#[test]
fn the_census_reports_whether_the_initiator_is_counted() {
    let registry = ThreadRegistry::new();
    registry.register(ThreadId(0), "main", None);
    registry.register(ThreadId(1), "worker", None);
    publish_os_tid(&registry, ThreadId(0), 100);
    publish_os_tid(&registry, ThreadId(1), 101);

    // An unregistered caller on a third OS thread: provably not counted.
    let (alive, blocked, os, blocked_tids, verdict) =
        registry.alive_count_blocked_and_os_tids_for_inner(ThreadId(7), 555);
    assert_eq!((alive, blocked), (2, 0));
    assert_eq!(os.len(), 2);
    assert!(blocked_tids.is_empty());
    assert_eq!(
        verdict,
        Some(false),
        "two counted entries on other OS threads"
    );

    // Its own entry is counted.
    let (.., verdict) = registry.alive_count_blocked_and_os_tids_for_inner(ThreadId(1), 555);
    assert_eq!(verdict, Some(true));

    // Another id published on the caller's OS thread (a virtual thread on
    // this carrier): counted, never "not counted" — that would hang.
    let (.., verdict) = registry.alive_count_blocked_and_os_tids_for_inner(ThreadId(7), 101);
    assert_eq!(verdict, Some(true));

    // No OS-tid backend: undecidable.
    let (.., verdict) = registry.alive_count_blocked_and_os_tids_for_inner(ThreadId(7), 0);
    assert_eq!(verdict, None);

    // A counted entry that has not published its OS tid could be the caller.
    registry.register(ThreadId(2), "fresh", None);
    let (alive, .., verdict) =
        registry.alive_count_blocked_and_os_tids_for_inner(ThreadId(7), 555);
    assert_eq!(alive, 3);
    assert_eq!(verdict, None);

    // The census half is identical to the plain census.
    let plain = registry.alive_count_blocked_and_os_tids();
    let with = registry.alive_count_blocked_and_os_tids_for_inner(ThreadId(7), 555);
    assert_eq!((plain.0, plain.1), (with.0, with.1));
}

/// A dead or not-yet-ready entry is outside the census, so an initiator
/// whose only entry is one of those owns no slot.
#[test]
fn an_initiator_outside_the_stw_census_is_not_counted() {
    let registry = ThreadRegistry::new();
    registry.register(ThreadId(0), "main", None);
    publish_os_tid(&registry, ThreadId(0), 100);
    registry.register_starting_with_daemon(ThreadId(1), "starting", None, false);
    publish_os_tid(&registry, ThreadId(1), 101);
    // The starting thread requests from its own OS thread before
    // `mark_stw_ready`: its entry is not counted, main's is on another OS
    // thread.
    let (alive, .., verdict) =
        registry.alive_count_blocked_and_os_tids_for_inner(ThreadId(1), 101);
    assert_eq!(alive, 1);
    assert_eq!(verdict, Some(false));
    registry.mark_stw_ready(ThreadId(1));
    let (alive, .., verdict) =
        registry.alive_count_blocked_and_os_tids_for_inner(ThreadId(1), 101);
    assert_eq!(alive, 2);
    assert_eq!(verdict, Some(true));
    registry.mark_dead(ThreadId(1));
    let (alive, .., verdict) =
        registry.alive_count_blocked_and_os_tids_for_inner(ThreadId(1), 101);
    assert_eq!(alive, 1);
    assert_eq!(verdict, Some(false));
}

/// gc-common w6-g (`common-w3a-jni-unbound-counted-monitor-enter`,
/// residual 2): the claimant choice itself.
#[test]
fn newest_os_tid_claimant_table() {
    let (a, b, c) = (ThreadId(1), ThreadId(2), ThreadId(3));
    assert_eq!(newest_os_tid_claimant(&[]), None);
    assert_eq!(
        newest_os_tid_claimant(&[(a, 0)]),
        Some(a),
        "one claim, even unstamped"
    );
    assert_eq!(newest_os_tid_claimant(&[(a, 4), (b, 9), (c, 7)]), Some(b));
    assert_eq!(
        newest_os_tid_claimant(&[(a, 0), (b, 0)]),
        None,
        "a tie is ambiguous"
    );
    assert_eq!(newest_os_tid_claimant(&[(a, 0), (b, 0), (c, 1)]), Some(c));
}

/// gc-common w6-g: several virtual threads that mounted on one carrier all
/// keep its OS tid; the one that published last there is the mounted one
/// and is the answer. Moving a claimant to another carrier, and its death,
/// take it out of the old carrier's answer.
#[test]
fn the_newest_publish_on_an_os_thread_names_the_mounted_thread() {
    let registry = ThreadRegistry::new();
    let (v1, v2, v3) = (ThreadId(1), ThreadId(2), ThreadId(3));
    registry.register(v1, "vt-1", None);
    registry.register(v2, "vt-2", None);
    registry.register(v3, "vt-3", None);
    const CARRIER_A: u32 = 7001;
    const CARRIER_B: u32 = 7002;
    assert_eq!(registry.thread_id_for_os_tid_inner(CARRIER_A), None);
    registry.publish_os_tid_claim(v1, CARRIER_A); // v1 mounts on A
    assert_eq!(registry.thread_id_for_os_tid_inner(CARRIER_A), Some(v1));
    registry.publish_os_tid_claim(v2, CARRIER_A); // v1 parks, v2 mounts on A
    assert_eq!(
        registry.thread_id_for_os_tid_inner(CARRIER_A),
        Some(v2),
        "the parked v1 still claims A, but v2 published there last",
    );
    registry.publish_os_tid_claim(v1, CARRIER_B); // v1 resumes on B
    assert_eq!(registry.thread_id_for_os_tid_inner(CARRIER_A), Some(v2));
    assert_eq!(registry.thread_id_for_os_tid_inner(CARRIER_B), Some(v1));
    registry.publish_os_tid_claim(v3, CARRIER_A); // v2 parks, v3 mounts on A
    assert_eq!(registry.thread_id_for_os_tid_inner(CARRIER_A), Some(v3));
    registry.mark_dead(v3); // v3 ends; v2 remounts on A
    registry.publish_os_tid_claim(v2, CARRIER_A);
    assert_eq!(registry.thread_id_for_os_tid_inner(CARRIER_A), Some(v2));
    assert_eq!(registry.thread_id_for_os_tid_inner(0), None, "no backend");

    // Entries whose tid was stored without a publish ordinal stay
    // ambiguous when more than one claims the OS thread.
    let registry = ThreadRegistry::new();
    registry.register(v1, "a", None);
    registry.register(v2, "b", None);
    publish_os_tid(&registry, v1, 900);
    publish_os_tid(&registry, v2, 900);
    assert_eq!(registry.thread_id_for_os_tid_inner(900), None);
}



/// gc-common w6-g: `own_jvm_thread_addr` answers only a published address
/// of an alive entry.
#[test]
fn own_jvm_thread_addr_answers_only_a_published_alive_entry() {
    let registry = ThreadRegistry::new();
    let t = ThreadId(4);
    assert_eq!(registry.own_jvm_thread_addr(t), None, "unknown id");
    registry.register(t, "t", None);
    assert_eq!(registry.own_jvm_thread_addr(t), None, "not published");
    registry.set_jvm_thread_addr(t, 0x1000);
    assert_eq!(registry.own_jvm_thread_addr(t), Some(0x1000));
    registry.mark_dead(t);
    assert_eq!(registry.own_jvm_thread_addr(t), None, "dead");
}

#[test]
fn initiator_counted_verdict_table() {
    assert_eq!(
        initiator_counted_verdict(true, false, false, true),
        Some(true)
    );
    assert_eq!(
        initiator_counted_verdict(false, true, true, true),
        Some(true)
    );
    assert_eq!(initiator_counted_verdict(false, false, false, false), None);
    assert_eq!(initiator_counted_verdict(false, false, true, true), None);
    assert_eq!(
        initiator_counted_verdict(false, false, true, false),
        Some(false)
    );
}
