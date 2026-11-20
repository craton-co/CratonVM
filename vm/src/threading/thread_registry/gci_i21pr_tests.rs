//! gci i21/pr (`CRATONVM_GC_DEDUPE_INITIATOR_SNAPSHOT`): the collection
//! door's gather leaves out the initiator's own snapshot, which
//! `collect_roots` step 11 already pushed, and nothing else.
use super::*;

const INIT_WORD: usize = 0x0570_0040;
const INIT_WORD_2: usize = 0x0570_0080;
const PEER_WORD: usize = 0x0571_0040;
const INIT_MIRROR: usize = 0x0125_0080;
const PEER_MIRROR: usize = 0x0125_00c0;

fn fake(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; an address only.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn sorted(v: &[ObjectRef]) -> Vec<usize> {
    let mut a: Vec<usize> = v.iter().map(|r| r.as_ptr() as usize).collect();
    a.sort_unstable();
    a
}

/// An initiator (its snapshot `Arc` shared with its entry, as
/// `set_root_snapshot` shares the `JvmThread`'s) and one peer.
fn two_threads() -> (ThreadRegistry, Arc<Mutex<Vec<ObjectRef>>>) {
    let registry = ThreadRegistry::new();
    let init = ThreadId(1);
    let peer = ThreadId(2);
    registry.register(init, "initiator", Some(fake(INIT_MIRROR)));
    registry.register(peer, "peer", Some(fake(PEER_MIRROR)));
    let snapshot = Arc::new(Mutex::new(vec![fake(INIT_WORD), fake(INIT_WORD_2)]));
    registry.set_root_snapshot(init, Arc::clone(&snapshot));
    {
        let threads = registry.threads.read();
        threads
            .get(&peer)
            .unwrap()
            .root_snapshot
            .lock()
            .push(fake(PEER_WORD));
    }
    (registry, snapshot)
}

/// The armed gather leaves out exactly the initiator's snapshot words;
/// its mirror and the peer's words and mirror stay. Consumed once.
#[test]
fn gci_i21pr_an_armed_gather_skips_only_the_initiator_snapshot() {
    let (registry, snapshot) = two_threads();
    registry.arm_initiator_snapshot_skip(&snapshot);
    let roots = registry.collect_all_root_snapshots();
    let mut want = vec![INIT_MIRROR, PEER_MIRROR, PEER_WORD];
    want.sort_unstable();
    assert_eq!(sorted(&roots), want);
    assert_eq!(registry.disarm_initiator_snapshot_skip(), Some(2));
    assert_eq!(registry.disarm_initiator_snapshot_skip(), None, "taken once");
    // The next gather is the historical one.
    let roots = registry.collect_all_root_snapshots();
    let mut want = vec![INIT_MIRROR, PEER_MIRROR, PEER_WORD, INIT_WORD, INIT_WORD_2];
    want.sort_unstable();
    assert_eq!(sorted(&roots), want);
}

/// Equality: the armed gather plus the initiator's snapshot (what step 11
/// pushes) is the unarmed gather, as a multiset -- the root set the
/// collector sees is unchanged, only the duplicate is gone.
#[test]
fn gci_i21pr_step_11_plus_the_armed_gather_is_the_unarmed_gather() {
    let (registry, snapshot) = two_threads();
    let unarmed = registry.collect_all_root_snapshots();
    registry.arm_initiator_snapshot_skip(&snapshot);
    let mut armed = registry.collect_all_root_snapshots();
    let _ = registry.disarm_initiator_snapshot_skip();
    armed.extend(snapshot.lock().iter().copied());
    assert_eq!(sorted(&armed), sorted(&unarmed));
}

/// Only the entry SHARING the armed `Arc` is skipped: a different `Arc`
/// with the same words (another thread that happens to hold the same
/// objects) skips nothing.
#[test]
fn gci_i21pr_an_equal_but_unshared_snapshot_is_not_skipped() {
    let (registry, snapshot) = two_threads();
    let unarmed = registry.collect_all_root_snapshots();
    let lookalike = Arc::new(Mutex::new(snapshot.lock().clone()));
    registry.arm_initiator_snapshot_skip(&lookalike);
    let roots = registry.collect_all_root_snapshots();
    assert_eq!(sorted(&roots), sorted(&unarmed));
    assert_eq!(registry.disarm_initiator_snapshot_skip(), Some(0));
}

/// An arm disarmed before any gather (the door's guard on an unwind)
/// leaves the next gather historical and reports `None`.
#[test]
fn gci_i21pr_a_disarmed_arm_leaves_the_next_gather_whole() {
    let (registry, snapshot) = two_threads();
    let unarmed = registry.collect_all_root_snapshots();
    registry.arm_initiator_snapshot_skip(&snapshot);
    assert_eq!(registry.disarm_initiator_snapshot_skip(), None);
    let roots = registry.collect_all_root_snapshots();
    assert_eq!(sorted(&roots), sorted(&unarmed));
    assert_eq!(registry.disarm_initiator_snapshot_skip(), None);
}

/// The split gather (`collect_all_root_snapshots_split`) neither reads
/// nor consumes the arm: only the door's plain gather does.
#[test]
fn gci_i21pr_the_split_gather_does_not_consume_the_arm() {
    let (registry, snapshot) = two_threads();
    registry.arm_initiator_snapshot_skip(&snapshot);
    let (roots, old_only) = registry.collect_all_root_snapshots_split(&[]);
    assert!(old_only.is_empty());
    assert!(sorted(&roots).contains(&INIT_WORD), "the split gather is whole");
    let roots = registry.collect_all_root_snapshots();
    assert!(!sorted(&roots).contains(&INIT_WORD), "still armed for the plain gather");
    assert_eq!(registry.disarm_initiator_snapshot_skip(), Some(2));
}

/// gcj j3/x2 (fail-first: before the arm recorded its thread, the
/// foreign gather consumed it and left the initiator's words out of a
/// root set where they are not duplicated): a plain gather on another
/// thread is whole, leaves the arm, and is counted; the arming thread's
/// own gather still skips.
#[test]
fn gcj3x2_a_foreign_threads_gather_neither_consumes_nor_applies_the_arm() {
    let (registry, snapshot) = two_threads();
    let unarmed = registry.collect_all_root_snapshots();
    registry.arm_initiator_snapshot_skip(&snapshot);
    let foreign = std::thread::scope(|s| {
        s.spawn(|| registry.collect_all_root_snapshots())
            .join()
            .unwrap()
    });
    assert_eq!(sorted(&foreign), sorted(&unarmed), "the foreign gather is whole");
    assert_eq!(registry.initiator_skip_foreign_gathers(), 1);
    let roots = registry.collect_all_root_snapshots();
    assert!(!sorted(&roots).contains(&INIT_WORD), "the arming thread still skips");
    assert_eq!(registry.disarm_initiator_snapshot_skip(), Some(2));
    // Unarmed gathers on another thread are not counted.
    std::thread::scope(|s| {
        s.spawn(|| registry.collect_all_root_snapshots());
    });
    assert_eq!(registry.initiator_skip_foreign_gathers(), 1);
}

/// gcj j3/x2 R10 (fail-first: before the split arm recorded its thread,
/// the foreign gather took it): a plain gather on another thread leaves
/// an armed parked split for the arming thread, and is counted.
#[test]
fn gcj3x2_r10_a_foreign_threads_gather_leaves_the_parked_split_armed() {
    let (registry, _snapshot) = two_threads();
    registry.arm_parked_split_gather(&[(0x1000, 0x2000)]);
    std::thread::scope(|s| {
        s.spawn(|| registry.collect_all_root_snapshots());
    });
    assert_eq!(registry.parked_split_foreign_gathers(), 1);
    assert!(registry.parked_split.lock().armed.is_some(), "still armed");
    let _ = registry.collect_all_root_snapshots();
    assert!(
        registry.parked_split.lock().armed.is_none(),
        "the arming thread took it"
    );
    let _ = registry.take_parked_old_only();
    // Unarmed gathers on another thread are not counted.
    std::thread::scope(|s| {
        s.spawn(|| registry.collect_all_root_snapshots());
    });
    assert_eq!(registry.parked_split_foreign_gathers(), 1);
}

fn read(rel: &str) -> String {
    if rel == "src/runtime/interpreter/gc_and_alloc.rs" {
        return crate::runtime::interpreter::gc_and_alloc::read_gc_and_alloc_src_from_disk()
            .replace("\r\n", "\n");
    }
    let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{path}: {e}"))
        .replace("\r\n", "\n")
}

fn at(text: &str, needle: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("`{needle}` not found"))
}

/// Source pin: the collection door arms before the
/// scan whose step 11 pushes the initiator's snapshot, and before its one
/// gather; it is the only arm site, and the concurrent cycles' doors
/// (which have no step 11 in front of their gather) never arm.
#[test]
fn gci_i21pr_only_the_collection_door_arms_before_its_scan() {
    let src = read("src/runtime/interpreter/gc_and_alloc.rs");
    let start = at(
        &src,
        "\npub(in crate::runtime::interpreter::gc_and_alloc) fn run_collection_pause(",
    );
    let rest = &src[start..];
    let body = &rest[..at(rest, "\n\u{7d}\n")];
    let arm = at(
        body,
        "let _initiator_skip_arm = arm_initiator_snapshot_skip(shared, thread);",
    );
    let scan = at(body, "collect_roots_registry_appended(shared, thread)");
    let gather = at(body, "thread_registry.collect_all_root_snapshots()");
    assert!(arm < scan && scan < gather, "{arm} {scan} {gather}");
    assert_eq!(
        src.matches("registry.arm_initiator_snapshot_skip(").count(),
        1
    );
    let helper = &src[at(
        &src,
        "\npub(in crate::runtime::interpreter::gc_and_alloc) fn arm_initiator_snapshot_skip<",
    )..];
    let helper = &helper[..at(helper, "\n\u{7d}\n")];
    assert!(helper.contains("if !dedupe_initiator_snapshot_enabled(shared)"));
    assert!(helper.contains("&thread.root_snapshot"));
    let gate = &src[at(
        &src,
        "\npub(in crate::runtime::interpreter::gc_and_alloc) fn dedupe_initiator_snapshot_enabled(",
    )..];
    let gate = &gate[..at(gate, "\n\u{7d}\n")];
    assert!(
        gate.contains("true"),
        "dedupe_initiator_snapshot_enabled must enable deduplication"
    );
    for door in [
        "\npub(in crate::runtime::interpreter) fn maybe_concurrent_gc_at(",
        "\npub(in crate::runtime::interpreter) fn g1_concurrent_mark_cycle(",
    ] {
        let s = at(&src, door);
        let rest = &src[s..];
        let b = &rest[..at(rest, "\n\u{7d}\n")];
        assert!(!b.contains("arm_initiator_snapshot_skip"), "{door}: must not skip");
    }
}
