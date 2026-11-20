use super::*;

/// A per-test VM identity for the process-wide `metadata_pin` rows (not
/// the i14/ve tests' ones).
const VM: usize = 0x15f0_0001;
/// Two fake G1 `Old` regions, out of order (the arm sorts), and
/// addresses inside / outside them.
const OLD_A: (usize, usize) = (0x5e40_0000, 0x5e50_0000);
const OLD_B: (usize, usize) = (0x5e20_0000, 0x5e30_0000);
const MIRROR: usize = OLD_A.0 + 0x40;
const W1: usize = OLD_B.0 + 0x1000;
const W2: usize = OLD_A.0 + 0x2000;
/// Between the two regions: eden / survivor / humongous / free.
const NOT_OLD: usize = 0x5e30_0040;

fn fake(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; an address only.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn addrs(v: &[ObjectRef]) -> Vec<usize> {
    v.iter().map(|r| r.as_ptr() as usize).collect()
}

/// An unmounted, blocked, precisely deposited entry (registry id 1).
fn parked(mirror: usize, words: &[usize]) -> (ThreadRegistry, ThreadId) {
    let mut registry = ThreadRegistry::new();
    registry.set_vt_parked_mirror_keyed_for_test(true);
    let t = ThreadId(1);
    registry.register(t, "vt-parked-g1", Some(fake(mirror)));
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

fn arm(registry: &ThreadRegistry) {
    registry.arm_mirror_keyed_gather_ranges(VM, vec![OLD_A, OLD_B], vec![1]);
}

/// The remark judgement with a no-op marker (the row fixed point is
/// tested on its own below).
fn judge(registry: &ThreadRegistry, is_live: &dyn Fn(usize) -> bool) -> usize {
    registry.judge_mirror_keyed_parked_after_mark(is_live, &mut |_: &[usize]| {})
}

/// The row fixed point: a live owner's values are marked, an owner they
/// make live has its values marked next, a dead owner's never are; each
/// owner once; nothing live -> nothing marked.
#[test]
fn gci_i15vh_live_owner_rows_are_marked_to_a_fixed_point() {
    const A: usize = 0xa000;
    const B: usize = 0xb000;
    const C: usize = 0xc000;
    // addr-keyed: local to this test (a synthetic heap).
    let mut rows: rustc_hash::FxHashMap<usize, Vec<usize>> = rustc_hash::FxHashMap::default();
    rows.insert(A, vec![0xa010, B]);
    rows.insert(B, vec![0xb010]);
    rows.insert(C, vec![0xc010]);
    let marked = std::cell::RefCell::new(rustc_hash::FxHashSet::<usize>::default());
    marked.borrow_mut().insert(A);
    let calls = std::cell::Cell::new(0usize);
    let is_live = |a: usize| marked.borrow().contains(&a);
    let handed = mark_live_row_values_to_fixpoint(&rows, &is_live, &mut |values: &[usize]| {
        calls.set(calls.get() + 1);
        marked.borrow_mut().extend(values.iter().copied());
    });
    assert_eq!(handed, 3, "A's two values, then B's one");
    assert_eq!(calls.get(), 2, "two rounds: A, then B");
    let m = marked.borrow();
    assert!(m.contains(&0xa010) && m.contains(&B) && m.contains(&0xb010));
    assert!(!m.contains(&C) && !m.contains(&0xc010), "a dead owner's values stay unmarked");
    drop(m);
    let none = mark_live_row_values_to_fixpoint(&rows, &|_| false, &mut |_: &[usize]| {
        panic!("nothing is live")
    });
    assert_eq!(none, 0);
}

/// A keyed G1 gather makes the next judgement run the row fixed point
/// first (once), and a mirror only another live owner's rows reach turns
/// live inside it, before its own verdict.
#[test]
fn gci_i15vh_the_judgement_closes_the_rows_before_its_verdict() {
    const VM_R: usize = 0x15f0_00a2;
    const MIRROR_R: usize = OLD_A.0 + 0x400;
    const HOLDER: usize = OLD_B.0 + 0x400;
    let (registry, t) = parked(MIRROR_R, &[W1]);
    registry.arm_mirror_keyed_gather_ranges(VM_R, vec![OLD_A, OLD_B], vec![1]);
    let _ = registry.collect_all_root_snapshots();
    // A live owner (say, another parked thread's mirror) whose rows name
    // this thread's mirror.
    cratonvm_types::metadata_pin::add_metadata_pins(VM_R, &[(HOLDER, MIRROR_R)]);
    let marked = std::cell::RefCell::new(rustc_hash::FxHashSet::<usize>::default());
    marked.borrow_mut().insert(HOLDER);
    let is_live = |a: usize| (a != MIRROR_R && a != W1) || marked.borrow().contains(&a);
    let dead = registry.judge_mirror_keyed_parked_after_mark(&is_live, &mut |v: &[usize]| {
        marked.borrow_mut().extend(v.iter().copied());
    });
    assert_eq!(dead, 0, "reached through a live owner's rows: kept");
    assert!(marked.borrow().contains(&W1), "and its own frames are marked");
    assert_eq!(registry.settle_mirror_keyed_parked_after_mark(true), 0);
    assert!(registry.is_alive(t));
    // The flag was taken: an unkeyed judgement marks nothing.
    let n = registry.judge_mirror_keyed_parked_after_mark(&|_| true, &mut |_: &[usize]| {
        panic!("no keyed gather since the last judgement")
    });
    assert_eq!(n, 0);
    cratonvm_types::metadata_pin::forget_vm_metadata_pins(VM_R);
}

#[test]
fn gci_i15vh_sorted_ranges_answer_membership() {
    let ranges = [(0x100, 0x200), (0x300, 0x400)];
    for (addr, inside) in [
        (0x0ff, false),
        (0x100, true),
        (0x1ff, true),
        (0x200, false),
        (0x2ff, false),
        (0x300, true),
        (0x3ff, true),
        (0x400, false),
    ] {
        assert_eq!(addr_in_sorted_ranges(&ranges, addr), inside, "{addr:#x}");
    }
    assert!(!addr_in_sorted_ranges(&[], 0x100));
}

/// G1's arm keys an entry whose mirror and every word lie in `Old`
/// regions (any of them, whatever the caller's order); a word outside
/// every `Old` region keeps the entry rooted, mirror included.
#[test]
fn gci_i15vh_g1_ranges_key_only_an_all_old_entry() {
    // Its own VM identity and mirror: the rows are process-wide, and the
    // other tests here file and forget rows under `VM` / `MIRROR`.
    const VM_K: usize = 0x15f0_00a1;
    const MIRROR_K: usize = OLD_A.0 + 0x80;
    let (registry, _t) = parked(MIRROR_K, &[W1, W2]);
    registry.arm_mirror_keyed_gather_ranges(VM_K, vec![OLD_A, OLD_B], vec![1]);
    let roots = registry.collect_all_root_snapshots();
    assert!(roots.is_empty(), "keyed: no root at all: {:?}", addrs(&roots));
    assert_eq!(registry.mirror_keyed_deferred_mirrors(), vec![MIRROR_K]);
    let mut rows = cratonvm_types::metadata_pin::roots_for_loader(MIRROR_K).unwrap_or_default();
    rows.sort_unstable();
    assert_eq!(rows, vec![W1, W2], "the frames are the mirror's rows");
    cratonvm_types::metadata_pin::forget_vm_metadata_pins(VM_K);
    registry.forget_mirror_keyed_deferred();

    let (registry, _t) = parked(MIRROR, &[W1, NOT_OLD]);
    arm(&registry);
    let roots = registry.collect_all_root_snapshots();
    assert_eq!(addrs(&roots), vec![W1, NOT_OLD, MIRROR], "a non-Old word: rooted");
    assert!(registry.mirror_keyed_deferred_mirrors().is_empty());

    let (registry, _t) = parked(NOT_OLD, &[W1]);
    arm(&registry);
    let roots = registry.collect_all_root_snapshots();
    assert_eq!(addrs(&roots), vec![W1, NOT_OLD], "a non-Old mirror: rooted");

    // No Old region at all: nothing is old, nothing keyed.
    let (registry, _t) = parked(MIRROR, &[W1]);
    registry.arm_mirror_keyed_gather_ranges(VM, Vec::new(), vec![1]);
    let roots = registry.collect_all_root_snapshots();
    assert_eq!(addrs(&roots), vec![W1, MIRROR]);
}

/// The remark's verdict: judged on the completed mark, retired only
/// after a TRUSTED cleanup; an untrusted one keeps the thread, and a live
/// mirror is never retired. Each step consumes what it read.
#[test]
fn gci_i15vh_judge_then_settle_retires_only_on_a_trusted_mark() {
    // Unmarked mirror, untrusted cleanup: kept.
    let (registry, t) = parked(MIRROR, &[W1]);
    arm(&registry);
    let _ = registry.collect_all_root_snapshots();
    assert_eq!(judge(&registry, &|a| a != MIRROR), 1);
    assert!(registry.mirror_keyed_deferred_mirrors().is_empty(), "judged once");
    assert_eq!(registry.settle_mirror_keyed_parked_after_mark(false), 0);
    assert!(registry.is_alive(t), "an untrusted mark proves nothing");
    assert!(registry.take_collected_parked().is_empty());
    assert_eq!(registry.settle_mirror_keyed_parked_after_mark(true), 0, "settled once");
    assert!(registry.is_alive(t));

    // Marked mirror, trusted cleanup: kept.
    arm(&registry);
    let _ = registry.collect_all_root_snapshots();
    assert_eq!(judge(&registry, &|_| true), 0);
    assert_eq!(registry.settle_mirror_keyed_parked_after_mark(true), 0);
    assert!(registry.is_alive(t));

    // Unmarked mirror, trusted cleanup: retired inside the pause.
    arm(&registry);
    let _ = registry.collect_all_root_snapshots();
    assert_eq!(judge(&registry, &|a| a != MIRROR), 1);
    assert!(registry.is_alive(t), "the judgement alone retires nothing");
    assert_eq!(registry.settle_mirror_keyed_parked_after_mark(true), 1);
    assert!(!registry.is_alive(t), "retired");
    {
        let threads = registry.threads.read();
        let e = threads.get(&t).unwrap();
        assert_eq!(e.tlab_addr.load(Ordering::Acquire), 0, "TLAB address withdrawn");
    }
    assert_eq!(registry.take_collected_parked(), vec![t]);
    // A dead entry is never gathered again.
    arm(&registry);
    assert!(registry.collect_all_root_snapshots().is_empty());
    cratonvm_types::metadata_pin::forget_vm_metadata_pins(VM);
}

/// A pair with no verdict (the initial mark, a lost or unwinding remark)
/// is forgotten: the thread stays, nothing is collected. A remounted
/// entry is never retired even by a trusted verdict.
#[test]
fn gci_i15vh_unjudged_pairs_are_forgotten_and_a_mount_wins() {
    let (registry, t) = parked(MIRROR, &[W1]);
    arm(&registry);
    let _ = registry.collect_all_root_snapshots();
    registry.forget_mirror_keyed_deferred();
    assert_eq!(judge(&registry, &|_| false), 0);
    assert_eq!(registry.settle_mirror_keyed_parked_after_mark(true), 0);
    assert!(registry.is_alive(t));
    // Judged, then forgotten before the settle (the guard's drop).
    arm(&registry);
    let _ = registry.collect_all_root_snapshots();
    assert_eq!(judge(&registry, &|_| false), 1);
    registry.forget_mirror_keyed_deferred();
    assert_eq!(registry.settle_mirror_keyed_parked_after_mark(true), 0);
    assert!(registry.is_alive(t));
    // Remounted between the judgement and the settle (cannot happen
    // inside one pause; the settle still re-checks).
    arm(&registry);
    let _ = registry.collect_all_root_snapshots();
    assert_eq!(judge(&registry, &|_| false), 1);
    registry.set_mounted(t, true);
    assert_eq!(registry.settle_mirror_keyed_parked_after_mark(true), 0);
    assert!(registry.is_alive(t));
    assert!(registry.take_collected_parked().is_empty());
    // The Generational sweep never sees a G1 pair: nothing is deferred
    // once the remark settled.
    let empty = cratonvm_types::PointerMap::default();
    assert_eq!(registry.sweep_mirror_keyed_parked(&empty, &|_| false), 0);
    cratonvm_types::metadata_pin::forget_vm_metadata_pins(VM);
}

/// Step 10b's copy: one occurrence per keyed thread leaves the plain
/// scan's result; any further occurrence is a real root and stays; a
/// mirror that is not there removes nothing.
#[test]
fn gci_i15vh_strip_takes_exactly_one_copy_per_keyed_mirror() {
    const M: usize = 0x7000;
    const N: usize = 0x7100;
    const X: usize = 0x7200;
    let mut roots = vec![fake(M), fake(X), fake(M), fake(N)];
    assert_eq!(strip_keyed_mirror_registry_roots(&mut roots, &[M]), 1);
    assert_eq!(addrs(&roots), vec![X, M, N], "the second copy is a real root");
    assert_eq!(strip_keyed_mirror_registry_roots(&mut roots, &[0x7300]), 0);
    assert_eq!(addrs(&roots), vec![X, M, N]);
    assert_eq!(strip_keyed_mirror_registry_roots(&mut roots, &[M, N, N]), 2);
    assert_eq!(addrs(&roots), vec![X], "N owed twice, present once");
    assert_eq!(strip_keyed_mirror_registry_roots(&mut roots, &[]), 0);
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

/// Both G1 marking scans key between their PLAIN scan (whose step 10b
/// copy the strip removes) and their gather; the initial mark forgets its
/// pairs before seeding; the remark judges inside the callback after the
/// keep closure, settles right after the heap call (before the
/// address-keyed sweep) and releases the states after the world. No
/// evacuation door keys on G1, and the licence is G1-and-marking only.
#[test]
fn gci_i15vh_the_g1_marking_doors_key_judge_and_settle_in_pause_order() {
    let src = read("src/runtime/interpreter/gc_and_alloc.rs");
    let im = body(
        &src,
        "\npub(in crate::runtime::interpreter) fn g1_concurrent_mark_cycle(",
    );
    let scan = at(
        im,
        "with_class_unload_marking(|| collect_roots(shared, thread))",
    );
    let key = at(im, "let g1_keyed = g1_key_parked_for_mark(shared);");
    let gather = at(im, "thread_registry.collect_all_root_snapshots()");
    let strip = at(im, "g1_keyed.strip_registry_mirrors(&mut roots);");
    let forget = at(im, "g1_keyed.forget_without_verdict();");
    let seed = at(im, "g1_mark_roots(&stw, &all_roots)");
    assert!(
        scan < key && key < gather && gather < strip && strip < forget && forget < seed,
        "{scan} {key} {gather} {strip} {forget} {seed}"
    );
    assert!(
        !im.contains("judge_mirror_keyed"),
        "the initial mark never judges"
    );

    let rm = body(
        &src,
        "\npub(in crate::runtime::interpreter) fn g1_final_remark_cleanup(",
    );
    let scan = at(
        rm,
        "with_class_unload_marking(|| collect_roots(shared, thread))",
    );
    let key = at(rm, "let g1_keyed = g1_key_parked_for_mark(shared);");
    let gather = at(rm, "thread_registry.collect_all_root_snapshots()");
    let strip = at(rm, "g1_keyed.strip_registry_mirrors(&mut roots);");
    let keep_arm = at(rm, "g1_remark_mark_kept_closure(shared, &keep);");
    let judge = at(rm, ".judge_mirror_keyed_parked_after_mark(is_live, &mut |values: &[usize]| ");
    let marker = at(rm, "g1_remark_mark_kept_closure(shared, values)");
    let jni = at(rm, "clear_weak_global_refs_unmarked(shared, is_live, &keep)");
    assert!(judge < marker && marker < jni, "the rows close, then the verdicts read them");
    let cleanup_point = at(rm, "cleanup_at.set(Some(std::time::Instant::now()));");
    let heap = at(rm, ".g1_final_remark_and_cleanup(&stw, &all_roots, Some(&mut process));");
    let settle = at(rm, ".settle_mirror_keyed_parked_after_mark(keyed_trusted);");
    let sweep = at(rm, "sweep_address_keyed_tables(shared);");
    let release = at(rm, ".complete_gc(cratonvm_types::PointerMap::default());");
    let retire = at(rm, "retire_collected_continuations(shared, collected);");
    assert!(
        scan < key && key < gather && gather < strip && strip < heap,
        "{scan} {key} {gather} {strip} {heap}"
    );
    assert!(
        keep_arm < judge && judge < cleanup_point && cleanup_point < heap,
        "{keep_arm} {judge} {cleanup_point} {heap}"
    );
    assert!(
        heap < settle && settle < sweep && sweep < release && release < retire,
        "{heap} {settle} {sweep} {release} {retire}"
    );
    assert!(rm.contains("let keyed_trusted = completed && g1_parked_keyed_mark_trusted(shared);"));

    let lic = body(
        &src,
        "\npub(in crate::runtime::interpreter::gc_and_alloc) fn g1_parked_mirror_keyed_licensed(",
    );
    for needle in [
        "is_g1()",
        "g1_is_marking_active()",
        "jdk_continuations()",
        "!crate::runtime::jvmti::debugger_observes_locals(shared)",
    ] {
        assert!(lic.contains(needle), "{needle}");
    }
    let key_fn = body(
        &src,
        "\npub(in crate::runtime::interpreter::gc_and_alloc) fn g1_key_parked_for_mark(",
    );
    assert!(
        key_fn.contains(".try_deposited_jdk_continuation_ids()"),
        "never blocks"
    );
    assert!(
        !key_fn.contains(".deposited_jdk_continuation_ids()"),
        "no blocking read"
    );
    let forget = at(key_fn, "registry.forget_mirror_keyed_deferred();");
    let armed = at(key_fn, "registry.arm_mirror_keyed_gather_ranges(");
    assert!(
        forget < armed,
        "no earlier pair reaches this scan's strip or verdict"
    );
    // The Generational door's licence still refuses G1.
    let gen = body(
        &src,
        "\npub(in crate::runtime::interpreter::gc_and_alloc) fn parked_mirror_keyed_licensed(",
    );
    assert!(gen.contains("is_generational()"));
}
