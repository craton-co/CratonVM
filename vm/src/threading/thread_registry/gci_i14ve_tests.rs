use super::*;

/// A per-test VM identity for the process-wide `metadata_pin` rows.
const VM: usize = 0x14e0_0001;
/// The fake old generation, and addresses inside / outside it.
const OLD_BASE: usize = 0x4e10_0000;
const OLD_LEN: usize = 0x0100_0000;
const MIRROR: usize = OLD_BASE + 0x40;
const W1: usize = OLD_BASE + 0x1000;
const W2: usize = OLD_BASE + 0x2000;
const MOVED: usize = OLD_BASE + 0x8000;
const YOUNG: usize = 0x0120_0040;

fn fake(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; an address only.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn addrs(v: &[ObjectRef]) -> Vec<usize> {
    v.iter().map(|r| r.as_ptr() as usize).collect()
}

/// An unmounted, blocked, precisely deposited entry with an old mirror
/// and two old words.
fn parked(mirror: usize, words: &[usize]) -> (ThreadRegistry, ThreadId) {
    let mut registry = ThreadRegistry::new();
    registry.set_vt_parked_mirror_keyed_for_test(true);
    let t = ThreadId(1);
    registry.register(t, "vt-parked", Some(fake(mirror)));
    registry.set_mounted(t, false);
    registry.set_tlab_addr(t, 0x1234_5000);
    {
        let threads = registry.threads.read();
        let e = threads.get(&t).unwrap();
        e.root_snapshot
            .lock()
            .extend(words.iter().map(|&w| fake(w)));
        e.gc_block_state
            .in_blocked_region
            .store(true, Ordering::Release);
    }
    (registry, t)
}

fn arm(registry: &ThreadRegistry, candidates: &[u64]) {
    registry.arm_mirror_keyed_gather(VM, OLD_BASE, OLD_LEN, candidates.to_vec());
}

fn rows_of(mirror: usize) -> Vec<usize> {
    cratonvm_types::metadata_pin::roots_for_loader(mirror).unwrap_or_default()
}

/// The keyed entry is in no root section -- neither its words nor its
/// mirror -- and its words are rows owned by the mirror; the next,
/// unarmed gather is the historical one again.
#[test]
fn gci_i14ve_a_keyed_gather_leaves_out_the_parked_frames_and_the_mirror() {
    // Its own VM identity and mirror: the rows are process-wide, and the
    // other tests here file and forget rows under `VM` / `MIRROR`.
    const VM_A: usize = 0x14e0_00a1;
    const MIRROR_A: usize = OLD_BASE + 0x80;
    let (registry, _t) = parked(MIRROR_A, &[W1, W2]);
    registry.arm_mirror_keyed_gather(VM_A, OLD_BASE, OLD_LEN, vec![1]);
    let roots = registry.collect_all_root_snapshots();
    assert!(roots.is_empty(), "no root: {:?}", addrs(&roots));
    let mut rows = rows_of(MIRROR_A);
    rows.sort_unstable();
    assert_eq!(rows, vec![W1, W2], "the frames are the mirror's rows");
    let roots = registry.collect_all_root_snapshots();
    assert_eq!(addrs(&roots), vec![W1, W2, MIRROR_A], "consumed by one gather");
    cratonvm_types::metadata_pin::forget_vm_metadata_pins(VM_A);
    // Disarmed without a gather: nothing keyed.
    registry.arm_mirror_keyed_gather(VM_A, OLD_BASE, OLD_LEN, vec![1]);
    registry.disarm_mirror_keyed_gather();
    let roots = registry.collect_all_root_snapshots();
    assert_eq!(addrs(&roots), vec![W1, W2, MIRROR_A]);
    // The deferred pair of the first gather waits for its sweep.
    assert_eq!(registry.mirror_keyed.lock().deferred.len(), 1);
}

/// Everything that is not provably a parked, precise, all-old JDK
/// continuation stays rooted as before.
#[test]
fn gci_i14ve_only_a_precise_all_old_deposited_continuation_is_keyed() {
    let expect_rooted = |registry: &ThreadRegistry, what: &str| {
        let roots = registry.collect_all_root_snapshots();
        assert!(
            addrs(&roots).contains(&MIRROR),
            "{what}: the mirror stays a root"
        );
        assert!(registry.mirror_keyed.lock().deferred.is_empty(), "{what}");
    };
    // Not a deposited JDK continuation.
    let (registry, _) = parked(MIRROR, &[W1]);
    arm(&registry, &[2]);
    expect_rooted(&registry, "not a candidate");
    // Mounted.
    let (registry, t) = parked(MIRROR, &[W1]);
    registry.set_mounted(t, true);
    arm(&registry, &[1]);
    expect_rooted(&registry, "mounted");
    // Not in its blocked region.
    let (registry, t) = parked(MIRROR, &[W1]);
    {
        let threads = registry.threads.read();
        let block = &threads.get(&t).unwrap().gc_block_state;
        block.in_blocked_region.store(false, Ordering::Release);
    }
    arm(&registry, &[1]);
    expect_rooted(&registry, "unblocked");
    // A conservative deposit.
    let (registry, t) = parked(MIRROR, &[W1]);
    {
        let threads = registry.threads.read();
        let block = &threads.get(&t).unwrap().gc_block_state;
        block.deposit_conservative.store(true, Ordering::Release);
    }
    arm(&registry, &[1]);
    expect_rooted(&registry, "conservative");
    // A young word.
    let (registry, _) = parked(MIRROR, &[W1, YOUNG]);
    arm(&registry, &[1]);
    expect_rooted(&registry, "young word");
    // A young mirror.
    let (registry, _) = parked(YOUNG, &[W1]);
    arm(&registry, &[1]);
    let roots = registry.collect_all_root_snapshots();
    assert_eq!(addrs(&roots), vec![W1, YOUNG], "young mirror: rooted");
    // No old generation (G1 / ZGC answer (0, 0)).
    let (registry, _) = parked(MIRROR, &[W1]);
    registry.arm_mirror_keyed_gather(VM, 0, 0, vec![1]);
    expect_rooted(&registry, "no old generation");
}

/// The verdict: a mirror the pause moved or kept keeps its thread; a
/// mirror that did not survive retires the entry (dead, TLAB address
/// withdrawn, listed once for the door), and the list is consumed once.
#[test]
fn gci_i14ve_the_sweep_retires_only_an_entry_whose_mirror_died() {
    let empty = cratonvm_types::PointerMap::default();
    // Survived in place.
    let (registry, t) = parked(MIRROR, &[W1]);
    arm(&registry, &[1]);
    let _ = registry.collect_all_root_snapshots();
    assert_eq!(registry.sweep_mirror_keyed_parked(&empty, &|_| true), 0);
    assert!(registry.is_alive(t));
    assert!(registry.take_collected_parked().is_empty());
    // Moved: the map wins over the in-place verdict.
    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(MIRROR, MOVED);
    arm(&registry, &[1]);
    let _ = registry.collect_all_root_snapshots();
    assert_eq!(registry.sweep_mirror_keyed_parked(&moved, &|_| false), 0);
    assert!(registry.is_alive(t));
    // Died: retired inside the pause.
    arm(&registry, &[1]);
    let _ = registry.collect_all_root_snapshots();
    assert_eq!(registry.sweep_mirror_keyed_parked(&empty, &|a| a != MIRROR), 1);
    assert!(!registry.is_alive(t), "retired");
    {
        let threads = registry.threads.read();
        let e = threads.get(&t).unwrap();
        assert_eq!(e.tlab_addr.load(Ordering::Acquire), 0, "TLAB address withdrawn");
        assert_eq!(e.jvm_thread_addr.load(Ordering::Acquire), 0);
    }
    assert_eq!(registry.take_collected_parked(), vec![t]);
    assert!(registry.take_collected_parked().is_empty(), "taken once");
    assert_eq!(
        registry.sweep_mirror_keyed_parked(&empty, &|_| false),
        0,
        "the deferred list is consumed by one sweep"
    );
    // A dead entry is never gathered again.
    arm(&registry, &[1]);
    assert!(registry.collect_all_root_snapshots().is_empty());
    cratonvm_types::metadata_pin::forget_vm_metadata_pins(VM);
}

/// A mounted entry (impossible with a dead mirror: a mount needs a
/// carrier that holds it) is left alone rather than retired under a
/// running slice.
#[test]
fn gci_i14ve_a_remounted_entry_is_never_retired() {
    let empty = cratonvm_types::PointerMap::default();
    let (registry, t) = parked(MIRROR, &[W1]);
    arm(&registry, &[1]);
    let _ = registry.collect_all_root_snapshots();
    registry.set_mounted(t, true);
    assert_eq!(registry.sweep_mirror_keyed_parked(&empty, &|_| false), 0);
    assert!(registry.is_alive(t));
    assert!(registry.take_collected_parked().is_empty());
    cratonvm_types::metadata_pin::forget_vm_metadata_pins(VM);
}

/// The contended fallback: the door counts the pause and arms nothing,
/// so the gather roots everything; the count is cumulative per VM.
#[test]
fn gci_i14ve_a_contended_arm_keys_nothing_and_is_counted() {
    let (registry, _t) = parked(MIRROR, &[W1]);
    assert_eq!(registry.mirror_keyed_skipped_contended(), 0);
    registry.note_mirror_keyed_contended();
    let roots = registry.collect_all_root_snapshots();
    assert_eq!(addrs(&roots), vec![W1, MIRROR], "nothing armed: rooted");
    assert!(registry.mirror_keyed.lock().deferred.is_empty());
    registry.note_mirror_keyed_contended();
    assert_eq!(registry.mirror_keyed_skipped_contended(), 2);
    // The door's arm takes the non-blocking read and counts its failure.
    let src = read("src/runtime/interpreter/gc_and_alloc.rs");
    let arm = body(
        &src,
        "\npub(in crate::runtime::interpreter::gc_and_alloc) fn arm_parked_mirror_keyed_gather(",
    );
    let try_read = at(arm, ".try_deposited_jdk_continuation_ids()");
    let counted = at(arm, "registry.note_mirror_keyed_contended();");
    assert!(try_read < counted);
    assert!(!arm.contains(".deposited_jdk_continuation_ids()"), "no blocking read in the pause");
}

#[test]
fn gci_i14ve_mirror_keyed_dead_reads_the_map_first() {
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(MIRROR, MOVED);
    let deferred = [(ThreadId(1), MIRROR), (ThreadId(2), W1), (ThreadId(3), W2)];
    let dead = mirror_keyed_dead(&deferred, &map, &|a| a == W2);
    assert_eq!(dead, vec![(ThreadId(2), W1)]);
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

fn body<'a>(src: &'a str, start: &str) -> &'a str {
    let rest = &src[at(src, start)..];
    &rest[..at(rest, "\n}\n")]
}

/// The door arms before its scan, the verdict runs inside the pause
/// before the blocked fold reads the words, and the states are released
/// only after the world is.
#[test]
fn gci_i14ve_arm_verdict_and_release_sit_in_pause_order() {
    let src = read("src/runtime/interpreter/gc_and_alloc.rs");
    let door = body(
        &src,
        "\npub(in crate::runtime::interpreter::gc_and_alloc) fn run_collection_pause(",
    );
    let arm = at(
        door,
        "let _parked_keyed_arm = arm_parked_mirror_keyed_gather(shared);",
    );
    let scan = at(door, "collect_roots_registry_appended(");
    let gather = at(door, "thread_registry.collect_all_root_snapshots()");
    let collect = at(door, ".collect_garbage_with_finalizers(");
    // gci i22/dr: the door lends the pause's one relocation set.
    let roots = at(
        door,
        "update_all_roots(shared, thread, &result.pointer_map, &relocation_targets);",
    );
    let release = at(door, "complete_gc(result.pointer_map);");
    let retire = at(door, "retire_collected_continuations(shared, collected);");
    assert!(
        arm < scan && scan < gather && gather < collect && collect < roots
            && roots < release && release < retire,
        "{arm} {scan} {gather} {collect} {roots} {release} {retire}"
    );
    // The licence: Generational, no concurrent cycle, no debugger, the
    // loader-metadata licence.
    let lic = body(
        &src,
        "\npub(in crate::runtime::interpreter::gc_and_alloc) fn parked_mirror_keyed_licensed(",
    );
    for needle in [
        "is_generational()",
        "ConcurrentGcPhase::Idle",
        "debugger_observes_locals(shared)",
        "conditional_loader_metadata(shared)",
    ] {
        assert!(lic.contains(needle), "{needle}");
    }
    let gc = read("src/memory/gc.rs");
    let update = body(&gc, "\npub fn update_all_roots(");
    let sweep = at(update, ".sweep_mirror_keyed_parked(pointer_map, &|addr| in_place.survived(addr));");
    let window = at(update, "drop(in_place);");
    let early = at(update, "if pointer_map.is_empty() {");
    let fold = at(update, ".fold_pointer_map_into_blocked_audited(");
    let remap = at(update, ".update_thread_objs_after_gc(");
    assert!(
        sweep < window && window < early && early < fold && fold < remap,
        "{sweep} {window} {early} {fold} {remap}"
    );
}
