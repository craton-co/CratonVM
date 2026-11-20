use super::*;

fn attached() -> (ReferenceProcessor, Arc<AllocationNoteStripes>) {
    let mut p = ReferenceProcessor::new_with_policy(1000);
    let stripes = Arc::new(AllocationNoteStripes::new());
    p.attach_allocation_note_stripes(Arc::clone(&stripes));
    (p, stripes)
}

/// A striped note becomes the row a locked push makes: same filter, a
/// sequence number taken at the note, absorbed by the take.
#[test]
fn a_striped_note_is_taken_as_a_locked_push() {
    let (mut p, stripes) = attached();
    p.note_allocated_reference(ReferenceType::Weak, 0x1000, 7, false);
    stripes.note(ReferenceType::Soft, 0x1040, 8, false);
    stripes.note(ReferenceType::Phantom, 0x1080, 9, true);
    stripes.note(ReferenceType::Finalizer, 0x10c0, 10, false);
    stripes.note(ReferenceType::Weak, 0, 7, false);
    assert_eq!(stripes.pending(), 2);
    assert_eq!(p.allocated_unbound_len(), 3);
    assert_eq!(p.registration_seq(), 3);
    let taken = p.take_allocated_unbound();
    assert_eq!(stripes.pending(), 0);
    let rows: Vec<(usize, u64, bool)> = taken
        .iter()
        .map(|a| (a.row.reference_obj, a.row.registration_seq, a.row.runs_cleaner))
        .collect();
    assert_eq!(rows, vec![(0x1000, 0, false), (0x1040, 1, false), (0x1080, 2, true)]);
    assert_eq!(taken[1].class_id, 8);
    assert_eq!(p.allocated_unbound_len(), 0);
}

/// Watched, relocated and pruned before absorption, as a pushed row is.
#[test]
fn a_striped_note_is_watched_relocated_and_pruned() {
    let (mut p, stripes) = attached();
    stripes.note(ReferenceType::Weak, 0x2000, 7, false);
    stripes.note(ReferenceType::Weak, 0x2040, 7, false);
    assert!(p.all_tracked_addrs().contains(&0x2000));
    assert!(p.all_tracked_addrs().contains(&0x2040));
    let mut m = cratonvm_types::PointerMap::default();
    m.insert(0x2000, 0x3000);
    p.update_after_gc(&m);
    assert_eq!(stripes.pending(), 0, "the relocation absorbed the notes");
    stripes.note(ReferenceType::Weak, 0x2080, 7, false);
    p.remove_collected(&|addr| addr != 0x2040 && addr != 0x2080);
    let left: Vec<usize> = p
        .take_allocated_unbound()
        .iter()
        .map(|a| a.row.reference_obj)
        .collect();
    assert_eq!(left, vec![0x3000]);
}

/// A soft note waiting in a stripe answers the soft queries as an
/// unbound soft row does.
#[test]
fn a_striped_soft_note_counts_for_the_soft_queries() {
    let (p, stripes) = attached();
    assert!(!p.has_active_soft_refs());
    assert!(p.active_soft_referents().is_some());
    stripes.note(ReferenceType::Soft, 0x4000, 8, false);
    assert!(p.has_active_soft_refs());
    assert!(p.active_soft_referents().is_none());
}

/// The stripes continue the processor's sequence (a row registered before
/// the attach keeps the smaller number), and a second attach is ignored.
#[test]
fn the_stripes_continue_the_sequence() {
    let mut p = ReferenceProcessor::new_with_policy(1000);
    p.discover_reference(ReferenceType::Weak, 0x5000, 0x5100, None);
    let stripes = Arc::new(AllocationNoteStripes::new());
    p.attach_allocation_note_stripes(Arc::clone(&stripes));
    p.attach_allocation_note_stripes(Arc::new(AllocationNoteStripes::new()));
    assert_eq!(p.registration_seq(), 1);
    stripes.note(ReferenceType::Weak, 0x5040, 7, false);
    assert_eq!(p.registration_seq(), 2);
    assert_eq!(p.take_allocated_unbound()[0].row.registration_seq, 1);
}

/// Notes from many threads all arrive, each once.
#[test]
fn notes_from_many_threads_all_arrive() {
    let (mut p, stripes) = attached();
    std::thread::scope(|s| {
        for t in 0..8usize {
            let stripes = &stripes;
            s.spawn(move || {
                for i in 0..500usize {
                    stripes.note(ReferenceType::Weak, 0x10_0000 + (t * 500 + i) * 0x40, 7, false);
                }
            });
        }
    });
    let mut addrs: Vec<usize> = p
        .take_allocated_unbound()
        .iter()
        .map(|a| a.row.reference_obj)
        .collect();
    assert_eq!(addrs.len(), 4000);
    addrs.sort_unstable();
    addrs.dedup();
    assert_eq!(addrs.len(), 4000);
    assert_eq!(stripes.census().0, 4000);
}

// ---- t1 wave 6, lane G6: the application retire without the mutex ----

/// A retire cancels its reference's note while it waits among the newest
/// notes of the stripe it is looked up in; a second retire, or one in
/// another stripe, finds nothing. The survivors keep their numbers.
#[test]
fn g6_a_retire_cancels_a_recent_note_in_its_stripe() {
    let (mut p, stripes) = attached();
    stripes.note_in(3, ReferenceType::Weak, 0x6000, 7, false);
    stripes.note_in(3, ReferenceType::Phantom, 0x6040, 9, true);
    stripes.note_in(5, ReferenceType::Weak, 0x6080, 7, false);
    assert!(stripes.cancel_recent_in(3, 0x6040));
    assert!(!stripes.cancel_recent_in(3, 0x6040), "cancelled once");
    assert!(!stripes.cancel_recent_in(3, 0x6080), "another stripe's note");
    assert!(!stripes.cancel_recent_in(3, 0));
    assert_eq!(stripes.pending(), 2);
    assert_eq!(stripes.cancelled(), 1);
    assert_eq!(stripes.census().0, 3, "taken = absorbed + pending + cancelled");
    assert!(!p.all_tracked_addrs().contains(&0x6040), "a cancelled note is not watched");
    let rows: Vec<(usize, u64)> = p
        .take_allocated_unbound()
        .iter()
        .map(|a| (a.row.reference_obj, a.row.registration_seq))
        .collect();
    assert_eq!(rows, vec![(0x6000, 0), (0x6080, 2)]);
    assert_eq!(stripes.census().0, 3);
}

/// The cancel looks at the newest `CANCEL_WINDOW` notes only, so its cost
/// does not grow with the notes waiting; an older note takes the locked
/// path (and is dropped by the bind, its referent being null).
#[test]
fn g6_the_cancel_window_is_bounded() {
    let (mut p, stripes) = attached();
    stripes.note_in(0, ReferenceType::Weak, 0x7000, 7, false);
    for i in 0..CANCEL_WINDOW {
        stripes.note_in(0, ReferenceType::Weak, 0x8000 + i * 0x40, 7, false);
    }
    assert!(!stripes.cancel_recent_in(0, 0x7000), "outside the window");
    assert!(stripes.cancel_recent_in(0, 0x8000), "the oldest note inside it");
    assert_eq!(p.take_allocated_unbound().len(), CANCEL_WINDOW);
}

/// Once drained, a note cannot be cancelled; the locked retire then finds
/// no bound row either (`false`), which is the verdict the cancel copies:
/// the unbound row is left for the bind to drop.
#[test]
fn g6_a_drained_note_is_left_to_the_locked_retire() {
    let (mut p, stripes) = attached();
    stripes.note_in(1, ReferenceType::Phantom, 0x9000, 9, true);
    assert_eq!(p.allocated_unbound_len(), 1);
    let taken = p.take_allocated_unbound();
    assert!(!stripes.cancel_recent_in(1, 0x9000));
    assert!(!p.mark_manually_enqueued(0x9000), "an unbound row is not retired");
    for a in taken {
        p.requeue_allocated_unbound(a);
    }
    assert_eq!(p.allocated_unbound_len(), 1);
}

/// No note is lost or doubled while notes, cancels and drains race: every
/// note made is either cancelled (its cancel returned `true`) or arrives
/// in exactly one drain, never both. Six makers share three stripes (two
/// per stripe, so cancels contend with each other's pushes) while a
/// drainer keeps absorbing under the processor's lock, as a collection's
/// readers do.
#[test]
fn g6_no_note_is_lost_or_doubled_under_concurrent_drain_and_cancel() {
    const MAKERS: usize = 6;
    const PER_MAKER: usize = 3000;
    let (p, stripes) = attached();
    let p = Mutex::new(p);
    let done = AtomicUsize::new(0);
    let mut arrived: Vec<usize> = Vec::new();
    let cancelled: Vec<Vec<usize>> = std::thread::scope(|s| {
        let drainer = s.spawn(|| {
            let mut got = Vec::new();
            loop {
                let finished = done.load(Ordering::Acquire) == MAKERS;
                got.extend(
                    p.lock()
                        .take_allocated_unbound()
                        .iter()
                        .map(|a| a.row.reference_obj),
                );
                if finished {
                    break;
                }
                std::thread::yield_now();
            }
            got
        });
        let makers: Vec<_> = (0..MAKERS)
            .map(|t| {
                let stripes = &stripes;
                let done = &done;
                s.spawn(move || {
                    let mut mine = Vec::new();
                    for i in 0..PER_MAKER {
                        let addr = 0x100_0000 + (t * PER_MAKER + i) * 0x40;
                        stripes.note_in(t % 3, ReferenceType::Weak, addr, 7, false);
                        // Retire every other reference, sometimes a few
                        // notes later (still inside the window).
                        if i % 2 == 1 {
                            let target = if i % 6 == 1 { addr } else { addr - 0x40 };
                            if stripes.cancel_recent_in(t % 3, target) {
                                mine.push(target);
                            }
                        }
                    }
                    done.fetch_add(1, Ordering::AcqRel);
                    mine
                })
            })
            .collect();
        let cancelled = makers.into_iter().map(|h| h.join().unwrap()).collect();
        arrived = drainer.join().unwrap();
        cancelled
    });
    // The drainer's last take ran after every maker finished.
    arrived.extend(p.lock().take_allocated_unbound().iter().map(|a| a.row.reference_obj));
    let cancelled: Vec<usize> = cancelled.into_iter().flatten().collect();
    let mut all: Vec<usize> = arrived.iter().chain(cancelled.iter()).copied().collect();
    assert_eq!(all.len(), MAKERS * PER_MAKER, "each note arrives or is cancelled, once");
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), MAKERS * PER_MAKER, "never both, never twice");
    assert_eq!(stripes.cancelled(), cancelled.len() as u64);
    assert_eq!(stripes.pending(), 0);
    assert_eq!(stripes.census().0, (MAKERS * PER_MAKER) as u64);
}

/// Each stripe sits on its own cache line.
#[test]
fn g6_stripes_do_not_share_a_cache_line() {
    assert_eq!(std::mem::align_of::<NoteStripe>(), 64);
    assert!(std::mem::size_of::<NoteStripe>() % 64 == 0);
}

/// t1 wave 8, lane I77: a reference whose constructor bridge registered a
/// row while its allocation note still waits (bridged constructors plus
/// discovery at allocation) owns BOTH. The cancel removes the note but
/// answers `false`, so the caller takes the locked retire, which settles
/// the row: before, the cancel answered `true` and the row stayed active,
/// to be delivered when its referent died (the PGJDBC phantom ghost).
#[test]
fn i77_a_bridge_row_sends_the_retire_to_the_locked_path() {
    let (mut p, stripes) = attached();
    stripes.note_in(2, ReferenceType::Phantom, 0xa000, 9, false);
    p.discover_reference(ReferenceType::Phantom, 0xa000, 0xb000, Some(0xc000));
    assert!(!stripes.cancel_recent_in(2, 0xa000), "the row needs the lock");
    assert_eq!(stripes.pending(), 0, "the note is removed all the same");
    assert_eq!(stripes.cancelled(), 1);
    assert!(p.mark_manually_enqueued(0xa000), "the locked retire settles the row");
    assert!(!p.has_active_reference_row(0xa000));
}

/// t1 wave 8, lane I77: the pre-collection bind's own registration and a
/// finalizer's do not set the mark, so the default `--real-jdk` shape
/// (every row made by the bind) keeps the lock-free cancel.
#[test]
fn i77_the_bind_keeps_the_lock_free_cancel() {
    let (mut p, stripes) = attached();
    stripes.note_in(4, ReferenceType::Weak, 0xd000, 7, false);
    stripes.note_in(4, ReferenceType::Phantom, 0xd040, 9, true);
    for a in p.take_allocated_unbound() {
        let referent = a.row.reference_obj + 0x10_000;
        assert_eq!(p.bind_allocated_reference(a, referent, None), BindOutcome::Bound);
    }
    p.discover_reference(ReferenceType::Finalizer, 0xd080, 0xd080, None);
    stripes.note_in(4, ReferenceType::Weak, 0xd0c0, 7, false);
    assert!(stripes.cancel_recent_in(4, 0xd0c0), "no row outside the bind yet");
    stripes.note_in(4, ReferenceType::Weak, 0xd100, 7, false);
    p.discover_phantom_cleaner(0xd140, 0xe000, None);
    assert!(!stripes.cancel_recent_in(4, 0xd100), "a bridged cleaner sets the mark");
}
