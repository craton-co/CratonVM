//! gc-common round 2026-09-23, wave 7, lane G: the GC doors after wave 6
//! -- `System.gc()` coalescing (per VM) and the post-GC debug verifiers on
//! every pause (`common-e-small-findings` items 7 and 1).

use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use std::sync::atomic::Ordering;

fn gen_vm() -> SharedVm {
    SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    })
}

fn source() -> String {
    super::GC_AND_ALLOC_SRC.replace("\r\n", "\n")
}

/// The item starting at `sig` at the start of a line, up to the next
/// column-0 closing brace (`\u{7d}`, see `w5a_pause_driver_tests::body`).
fn body(src: &str, sig: &str) -> String {
    let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &src[start..];
    let stop = rest
        .find("\n\u{7d}\n")
        .unwrap_or_else(|| panic!("end of `{sig}` not found"));
    rest[..stop].to_string()
}

/// Item 7: the rule is "at least one `System.gc()` collection COMPLETED
/// since the call began" -- not "the counter moved by two", which would
/// never coalesce the common case (N callers, one winner).
#[test]
fn a_system_gc_coalesces_only_on_a_later_completion() {
    assert!(!system_gc_satisfied(5, 5));
    assert!(system_gc_satisfied(5, 6));
    assert!(system_gc_satisfied(5, 9));
    let shared = gen_vm();
    assert!(!system_gc_completed_since(&shared, None));
    let base = system_gc_collections(&shared).load(Ordering::Relaxed);
    assert!(!system_gc_completed_since(&shared, Some(base)));
    system_gc_collections(&shared).fetch_add(1, Ordering::Relaxed);
    assert!(system_gc_completed_since(&shared, Some(base)));
}


/// Item 7: a caller whose own blocked-region flag is raised can be
/// excluded from a pause's census, so it must never coalesce.
#[test]
fn a_blocked_caller_never_coalesces() {
    let shared = gen_vm();
    let thread = JvmThread::new(ThreadId(0), "w7g-blocked");
    assert!(system_gc_coalescing_baseline(&shared, &thread).is_some());
    thread
        .gc_block_state
        .in_blocked_region
        .store(true, Ordering::Release);
    assert_eq!(system_gc_coalescing_baseline(&shared, &thread), None);
    thread
        .gc_block_state
        .in_blocked_region
        .store(false, Ordering::Release);
}

/// Item 7: every WON `System.gc()` counts once, on its own VM only (the
/// counter is per VM, never a process static), and the allocation doors
/// never count.
#[test]
fn each_won_system_gc_counts_once_on_its_own_vm() {
    let a = gen_vm();
    let b = gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "w7g-sysgc");
    let a0 = system_gc_collections(&a).load(Ordering::Relaxed);
    let b0 = system_gc_collections(&b).load(Ordering::Relaxed);
    let cycles0 = a.mem.gc_cycle_count.load(Ordering::Relaxed);
    force_gc_from_native(&a, &mut thread);
    force_gc_from_native(&a, &mut thread);
    assert_eq!(system_gc_collections(&a).load(Ordering::Relaxed) - a0, 2);
    assert_eq!(system_gc_collections(&b).load(Ordering::Relaxed), b0);
    assert!(a.mem.gc_cycle_count.load(Ordering::Relaxed) >= cycles0 + 2);
    maybe_gc_forced_at(&a, &mut thread, "w7g-test");
    assert_eq!(system_gc_collections(&a).load(Ordering::Relaxed) - a0, 2);
    // The metaspace refusal's collection is the same door.
    force_gc_for_class_metadata(&a, &mut thread);
    assert_eq!(system_gc_collections(&a).load(Ordering::Relaxed) - a0, 3);
}

/// The metaspace refusal's collection runs none of `System.gc()`'s drains
/// (it runs inside `defineClass`, holding the loader's lock); `System.gc()`
/// keeps all three.
#[test]
fn the_class_metadata_collection_runs_no_drains() {
    let src = source();
    let meta = body(&src, "pub fn force_gc_for_class_metadata(");
    let full = body(&src, "fn full_gc_door(");
    for drain in [
        "run_finalizers_forced(",
        "run_cleaner_actions_forced(",
        "run_gc_notifications(",
    ] {
        assert!(!meta.contains(drain), "metadata collection runs `{drain}`");
        assert!(full.contains(drain), "System.gc() lost `{drain}`");
    }
    assert!(meta.contains("force_full_collection(shared, thread, GcDoor::MetadataThreshold)"));
    assert!(full.contains("force_full_collection(shared, thread, GcDoor::SystemGc)"));
}

/// gci i47w/nt47: the metadata door hands its notifications to the
/// delivery thread after EITHER collection (the G1 concurrent start or
/// the full one), and never waits for them.
#[test]
fn gci_i47w_nt47_the_metadata_door_hands_off_after_either_collection() {
    let src = source();
    let meta = body(&src, "pub fn force_gc_for_class_metadata(");
    let cs = meta
        .find("g1_metadata_concurrent_start(shared, thread)")
        .expect("the concurrent start");
    let full = meta
        .find("force_full_collection(shared, thread, GcDoor::MetadataThreshold)")
        .expect("the full collection");
    let hand = meta
        .find("metadata_door_hands_off_gc_notifications(shared, thread)")
        .expect("the hand-off");
    assert!(cs < full && full < hand);
    assert!(!meta.contains("return;"), "no early return skips the hand-off");
    assert!(!meta.contains("wait_for_delivery("));
    let door = body(&src, "fn metadata_door_hands_off_gc_notifications(");
    assert!(!door.contains("wait_for_delivery("));
    assert!(!door.contains("run_gc_notifications("));
}

/// Item 7, ordering: the bump is inside the pause (before the frozen peers
/// resume and the world is released), only for the `System.gc()` door; and
/// `force_gc_from_native` reads its baseline before its first attempt and
/// checks it only after a LOST attempt.
#[test]
fn the_system_gc_count_is_bumped_before_release_and_read_before_the_first_attempt() {
    let src = source();
    let b = body(&src, "fn run_collection_pause(");
    let seal = b
        .find("gc_event_seal(shared, &mut gc_event)")
        .expect("sealed");
    let bump = b
        .find("system_gc_collections(shared).fetch_add(1")
        .expect("bumps");
    let resume = b
        .find("retire_skip_spans_and_resume(shared, taken)")
        .expect("resumes");
    let release = b
        .find(".complete_gc(result.pointer_map)")
        .expect("releases");
    assert!(seal < bump && bump < resume && resume < release);
    assert!(b[..bump].contains("GcDoor::SystemGc | GcDoor::MetadataThreshold"));

    let f = body(&src, "fn force_full_collection(");
    let baseline = f
        .find("system_gc_coalescing_baseline(shared, thread)")
        .expect("baseline");
    let first = f
        .find("for _attempt in 0..FORCE_GC_ATTEMPTS")
        .expect("attempt loop");
    let pause = f
        .find("run_collection_pause(shared, thread, door)")
        .expect("pause");
    let check = f
        .rfind("system_gc_completed_since(shared, coalesce_baseline)")
        .expect("coalescing check");
    assert!(baseline < first && first < pause && pause < check);
    let after_wait = f.find("system_gc_completed_since(shared, coalesce_baseline)").unwrap();
    assert!(baseline < after_wait && after_wait < first);
}

/// Item 1: the verifier set runs on every pause (no census-count gate
/// around the call), and both halves of the heap-stale pair share one
/// walkability gate.
#[test]
fn the_post_gc_verifiers_run_on_every_pause_under_one_walk_gate() {
    let src = source();
    let b = body(&src, "fn run_collection_pause(");
    assert!(
        !b.contains("if solo {\n        post_gc_debug_verifiers"),
        "the verifier set is back behind the census-count gate"
    );
    let pre = b
        .find("crate::memory::gc::verify_heap_object_fields_pre_gc(shared)")
        .expect("pre half");
    assert!(b[..pre].contains("if debug_heap_walks_permitted(shared, solo)"));
    let post = b.find("post_gc_debug_verifiers(").expect("post set");
    assert!(b[post..].contains("debug_heap_walks_permitted(shared, solo),"));
    let v = body(&src, "fn post_gc_debug_verifiers(");
    let gate = v.find("if heap_walks").expect("walkers gated");
    assert!(gate < v.find("validate_object_sizes(shared)").expect("validator"));
    assert!(
        gate < v
            .find("verify_heap_object_fields(shared, pointer_map)")
            .expect("stale verifier")
    );
}

/// Item 1: with no walker armed the gate is shut on every pause without a
/// registry walk; with one armed a solo pause walks.
#[test]
fn the_walk_gate_is_shut_unless_a_walker_is_armed() {
    let shared = gen_vm();
    if debug_heap_walkers_armed() {
        assert!(debug_heap_walks_permitted(&shared, true));
    } else {
        assert!(!debug_heap_walks_permitted(&shared, true));
        assert!(!debug_heap_walks_permitted(&shared, false));
    }
}

/// gci i2/oo (`gci-i1pp-jcmd-gc-run-is-a-deferred-young-collection-...`):
/// `jcmd GC.run` waits for a FULL collection on the `Diagnostic Command`
/// door, which the next `maybe_gc` runs, and is answered only after it.
#[test]
fn gci_i2oo_jcmd_gc_run_waits_for_its_full_collection() {
    let shared = gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "gci-i2oo-dcmd");
    let base = system_gc_collections(&shared).load(Ordering::Relaxed);
    let sh = &shared;
    std::thread::scope(|s| {
        let requester = s.spawn(move || {
            request_diagnostic_command_gc(sh, std::time::Duration::from_secs(60))
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !requester.is_finished() && std::time::Instant::now() < deadline {
            maybe_gc(sh, &mut thread);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(requester.join().expect("requester"), "answered after it ran");
    });
    assert_eq!(
        system_gc_collections(&shared).load(Ordering::Relaxed) - base,
        1,
        "one full collection on the dcmd door"
    );
    assert!(!shared
        .mem
        .alloc_ladder
        .dcmd_gc_requested
        .load(Ordering::Relaxed));
    assert!(!shared.mem.gc_requested.load(Ordering::Relaxed));
}

/// The latch alone (the native String path's young-collection lever) is
/// still an ordinary collection, never the full dcmd one; and a timed-out
/// request is reported as such and stays pending for the next `maybe_gc`.
#[test]
fn gci_i2oo_the_plain_latch_stays_young_and_a_late_request_still_runs() {
    let shared = gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "gci-i2oo-latch");
    let base = system_gc_collections(&shared).load(Ordering::Relaxed);
    let cycles = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
    shared.mem.gc_requested.store(true, Ordering::Relaxed);
    maybe_gc(&shared, &mut thread);
    assert!(shared.mem.gc_cycle_count.load(Ordering::Relaxed) > cycles);
    assert_eq!(system_gc_collections(&shared).load(Ordering::Relaxed), base);

    assert!(!request_diagnostic_command_gc(
        &shared,
        std::time::Duration::ZERO
    ));
    maybe_gc(&shared, &mut thread);
    assert_eq!(system_gc_collections(&shared).load(Ordering::Relaxed) - base, 1);
    assert!(!take_diagnostic_command_gc_request(&shared), "taken once");
}

/// A compiled allocating loop takes the request at its next JIT-guarded
/// TLAB refill (it never reaches `maybe_gc`); the interpreter's and the
/// native allocator's refills do not (they reach `maybe_gc`, or must not
/// collect there).
#[test]
fn gci_i2oo_jit_guarded_refills_take_the_request() {
    let src = source();
    let b = body(&src, "fn tlab_alloc_shaped_inner(");
    let take = b
        .find("if refill_needs_young_room && take_diagnostic_command_gc_request(shared)")
        .expect("the refill takes the request");
    let run = b
        .find("run_diagnostic_command_gc(shared, thread)")
        .expect("and runs it");
    assert!(take < run);
    assert!(b[take..run].contains("thread.tlab.retire()"));
    let full = body(&src, "fn run_diagnostic_command_gc(");
    assert!(full.contains("force_full_collection(shared, thread, GcDoor::DiagnosticCommand)"));
}

/// gci i9/dc (`gci-i8rv-dcmd-live-walk-on-an-idle-vm-FIXED-20261017`): each
/// attach command's full collection carries HotSpot's own cause.
#[test]
fn gci_i9dc_each_attach_collection_has_its_hotspot_door() {
    assert_eq!(DcmdFullGc::GcRun.door(), GcDoor::DiagnosticCommand);
    assert_eq!(DcmdFullGc::HeapInspection.door(), GcDoor::HeapInspection);
    assert_eq!(DcmdFullGc::HeapDump.door(), GcDoor::HeapDump);
    assert_eq!(DcmdFullGc::HeapInspection.door().cause(), "Heap Inspection Initiated GC");
    assert_eq!(DcmdFullGc::HeapDump.door().cause(), "Heap Dump Initiated GC");
}

/// A VM the process registry does not know (every unit VM) has no `Arc`
/// to attach a thread to: the attached path declines (`None`) without
/// collecting and without raising either latch, so the caller's latch
/// path is unchanged (`diagnostic_state_trigger_gc` pins that one).
#[test]
fn gci_i9dc_an_unregistered_vm_declines_the_attached_collection() {
    let shared = gen_vm();
    let base = system_gc_collections(&shared).load(Ordering::Relaxed);
    for kind in [DcmdFullGc::GcRun, DcmdFullGc::HeapInspection, DcmdFullGc::HeapDump] {
        assert_eq!(run_dcmd_full_gc_on_attach_thread(&shared, kind), None);
    }
    assert_eq!(system_gc_collections(&shared).load(Ordering::Relaxed), base);
    assert!(!shared
        .mem
        .alloc_ladder
        .dcmd_gc_requested
        .load(Ordering::Relaxed));
    assert!(!shared.mem.gc_requested.load(Ordering::Relaxed));
}

/// zt t7/tg (zt1tp item 3): the director tick's collection is a
/// RULE-driven cycle on an attached thread -- the allocation-threshold
/// door with the rule's cause charged first (so it reports `Timer`), not
/// `System.gc()`'s full collection (no explicit-GC semantics); a lost
/// pause withdraws an answered request; the tick thread keeps only a
/// `Weak` while it sleeps; and nothing starts unless the heap wants a
/// tick (`-XX:ZCollectionInterval` / the opt-in idle tick).
#[test]
fn zt7tg_the_director_tick_runs_a_rule_driven_cycle_on_an_attached_thread() {
    let src = source();
    let start = body(&src, "fn start_zgc_director_tick(");
    assert!(start.contains("if !heap.zgc_director_tick_wanted()"));
    assert!(start.contains("std::sync::Arc::downgrade(shared)"));
    let tick = body(&src, "fn zgc_director_tick_loop(");
    let wait = tick.find("ZTick::Wait(wait) => {").expect("a wait arm");
    let dropped = tick[wait..].find("drop(shared);").expect("drops the VM");
    let slept = tick[wait..]
        .find("std::thread::sleep(wait);")
        .expect("then sleeps");
    assert!(dropped < slept, "no strong VM reference while sleeping");
    let run = body(&src, "fn run_zgc_tick_collection(");
    assert!(run.contains("crate::native::jni::run_attached_service_as("));
    assert!(run.contains("\"ZDirector\","));
    // zt t11/tg4: the attachment's JMM counting follows the switch.
    assert!(run.contains("let jmm_counted = !zgc_tick_thread_uncounted_enabled();"));
    let job = body(&src, "fn zgc_tick_collection(");
    let request = job
        .find("heap.request_tick_cycle(cause);")
        .expect("charges the rule");
    let pause = job
        .find("run_collection_pause(shared, thread, GcDoor::AllocationThreshold)")
        .expect("through the rule-driven door");
    assert!(
        request < pause,
        "the cause is on the latch before the pause reads it"
    );
    assert!(!job.contains("force_full_collection"), "not System.gc()'s door");
    assert!(!job.contains("request_major_gc"), "no explicit-GC request");
    assert!(job.contains("heap.withdraw_tick_cycle_if_answered(cause"));
}

/// zt t10/tg3 (`t10-x7-report.md` R1): the tick collection reads the
/// director stop before each attempt's request and again between the
/// request and the pause, where it takes its own cause bit back; the
/// collector side (the switch, the own-bit rule) is
/// `zt10tg3_a_stopped_tick_takes_back_only_its_own_bit` in `gc`.
#[test]
fn zt10tg3_the_tick_collection_stands_down_at_the_director_stop() {
    let src = source();
    let job = body(&src, "fn zgc_tick_collection(");
    let first_stop = job
        .find("if heap.zgc_director_tick_stopped() {")
        .expect("a stop check");
    // zt t11/tg4 (`t11-x8-report.md` C12): the top-of-attempt check also
    // takes back a bit a previous attempt's lost pause left.
    assert!(job[first_stop..].starts_with(
        "if heap.zgc_director_tick_stopped() {\n            heap.withdraw_tick_cycle_on_stop(cause);" // literal-continuation-ok: a source witness or a HotSpot log table, spaces are the text
    ));
    let request = job
        .find("heap.request_tick_cycle(cause);")
        .expect("charges the rule");
    assert!(first_stop < request, "no new attempt after the stop");
    let second_stop = request
        + job[request..]
            .find("if heap.zgc_director_tick_stopped() {")
            .expect("a stop check after the request");
    let withdraw = second_stop
        + job[second_stop..]
            .find("heap.withdraw_tick_cycle_on_stop(cause);")
            .expect("takes the request's bit back");
    let pause = job
        .find("run_collection_pause(shared, thread, GcDoor::AllocationThreshold)")
        .expect("the pause");
    assert!(withdraw < pause, "the bit goes back before any pause");
    // zt t11/tg4 (T1): the pause is in flight before the last check and
    // until it ends, on every path.
    // zt t12/x9: entered through the RAII guard (left on unwind too),
    // dropped explicitly on the two ordinary paths.
    let enter = job
        .find("let in_flight = heap.tick_pause_guard();")
        .expect("marked in flight");
    assert!(enter < second_stop, "before the last stop check");
    assert!(!job.contains("heap.tick_pause_enter();"), "no unguarded enter");
    let leaves: Vec<usize> = job
        .match_indices("drop(in_flight);")
        .map(|m| m.0)
        .collect();
    assert_eq!(leaves.len(), 4, "the stand-down, two concurrent exits, and the pause end");
    assert!(leaves[0] > second_stop && leaves[0] < withdraw);
    assert!(leaves[1] > withdraw && leaves[2] > leaves[1] && leaves[2] < pause);
    assert!(leaves[3] > pause);
}

/// zt t12/x9: a mispredicted arming (the collection ran whole-heap) drops
/// the opt-in soft-free hold before the round runs, whatever the clock
/// switch says, so the major stores its own free figure.
#[test]
fn zt12x9_a_mispredicted_major_drops_the_soft_free_hold() {
    let src = source();
    let round = crate::runtime::interpreter::gc_and_alloc::process_references_text(&src);
    let due = round
        .find("if shared.mem.heap.zgc_take_armed_major_soft_clock_due() {")
        .expect("the misprediction note, taken unconditionally");
    let drop_hold = round
        .find("ref_proc.set_young_round_holds_soft_free(false);")
        .expect("the hold is dropped");
    let clock = round
        .find("ref_proc.note_round_clock_now();")
        .expect("the clock is advanced");
    let process = round
        .find("ref_proc.process_references(")
        .expect("the round");
    assert!(due < drop_hold && drop_hold < clock && clock < process);
}

/// zt t20/gn7: `CRATONVM_ZGC_GEN_YOUNG_HOLDS_SOFT_FREE` is default ON (a
/// `=0` kill), read only on a young cycle that keeps its referents, and
/// handed to the processor under its lock (`gn5_SF4_1h` SAME as HotSpot
/// 25's `kept=true`).
#[test]
fn zt20gn7_the_young_soft_free_hold_is_default_on() {
    let interp = concat!(include_str!("../../interpreter/mod.rs"), "\n", include_str!("../../interpreter/cached_invoke_target.rs"), "\n", include_str!("../../interpreter/execute.rs"), "\n", include_str!("../../interpreter/resume_continuation.rs"), "\n", include_str!("../../interpreter/execute_frame_from_index_loop.rs"), "\n", include_str!("../../interpreter/resolve_array_class_loader_aware.rs"), "\n", include_str!("../../interpreter/park_for_debugger_under.rs"), "\n", include_str!("../../interpreter/double_arith_apply.rs")).replace("\r\n", "\n");
    let pre = body(&interp, "pub fn weakref_null_referents_pre_gc(");
    let at = |needle: &str| {
        pre.find(needle)
            .unwrap_or_else(|| panic!("`{needle}` not found"))
    };
    let arm = at("let keep_referents = zgc_young_cycle_keeps_references(shared);");
    let read = at("let hold_soft_free = keep_referents;");
    let hand = at("rp.set_young_round_holds_soft_free(hold_soft_free);");
    assert!(arm < read && read < hand);
    assert!(!pre.contains(concat!(
        "runtime_flag_on(\"CRATON",
        "VM_ZGC_GEN_YOUNG_HOLDS_SOFT_FREE\")"
    )));
}

/// zt t11/tg4 (`zgc-zt11tg4-*` item 1): the tick collection hands its GC
/// notifications off as the metadata door does (a `--compatible` VM
/// included), under `CRATONVM_ZGC_TICK_NOTIFICATIONS_OFF_THREAD`, and runs
/// the listeners inline only when nothing took them; `=0` is the rule of
/// every other door. The metadata door's own rule (pending notifications,
/// the inline arm hands nothing off) is pinned by its gci i47w tests.
#[test]
fn zt11tg4_the_tick_collection_hands_its_notifications_off() {
    let src = source();
    let job = body(&src, "fn zgc_tick_collection(");
    let handed = job
        .find("if !zgc_tick_hands_off_gc_notifications(shared, thread) {")
        .expect("the tick's hand-off");
    let inline = job
        .find("run_gc_notifications(shared, thread, false);")
        .expect("inline only when nothing took them");
    assert!(handed < inline);
    assert!(!job.contains("gc_notifications_go_to_delivery_thread"));
    let hand = body(&src, "fn zgc_tick_hands_off_gc_notifications(");
    let on = hand
        .find("metadata_door_hands_off_gc_notifications(shared, thread)")
        .expect("the drainless door's hand-off");
    let off = hand
        .find("gc_notifications_go_to_delivery_thread(shared, thread)")
        .expect("=0: every other door's rule");
    assert!(hand.contains("if zgc_tick_notifications_off_thread_enabled() {"));
    assert!(on < off, "the switch picks the hand-off first");
    assert!(
        drainless_door_hands_off_gc_notifications(GcNotificationDelivery::WhenLockHeld),
        "--compatible hands off whether or not a lock is held"
    );
    assert!(!drainless_door_hands_off_gc_notifications(GcNotificationDelivery::Inline));
}

/// The attached path's shape: it never raises the latch (nothing is left
/// pending for a later mutator), refuses a caller that is itself a VM
/// thread (the pause would wait for it), collects through the
/// `System.gc()` door's full collection under the command's door on an
/// attached service thread, and the full-collection count the requester
/// and a coalescing `System.gc()` read moves for the two new doors too.
/// The three attach commands pass their own kind (`vm_init.rs`).
#[test]
fn gci_i9dc_the_attached_collection_runs_now_and_leaves_nothing_pending() {
    let src = source();
    let b = body(&src, "fn run_dcmd_full_gc_on_attach_thread(");
    assert!(b.contains("system_gc_satisfied(baseline, now)"));
    assert!(b.contains("crate::native::jni::is_foreign_attached()"));
    assert!(b.contains("current_os_tid_claimed() != Some(false)"));
    assert!(b.contains("crate::native::jni::vm_for_identity(shared.vm_identity)?"));
    assert!(b.contains("force_full_collection(shared, thread, door)"));
    assert!(b.contains("crate::native::jni::run_attached_service(vm, \"Attach Listener\""));
    assert!(!b.contains("dcmd_gc_requested"), "no latch on the attached path");
    assert!(!b.contains("gc_requested.store"), "no latch on the attached path");
    let pause = body(&src, "fn run_collection_pause(");
    let bump = pause
        .find("system_gc_collections(shared).fetch_add(1")
        .expect("bumps");
    assert!(pause[..bump].contains("| GcDoor::HeapInspection"));
    assert!(pause[..bump].contains("| GcDoor::HeapDump"));

    let init = include_str!("../../../vm/vm_init/class_store_cut_with.rs").replace("\r\n", "\n");
    assert_eq!(init.matches("DcmdFullGc::HeapInspection,").count(), 1, "the histogram");
    // gci i47w/dh47: the third is `HotSpotDiagnosticMXBean.dumpHeap`'s live collection.
    assert_eq!(init.matches("DcmdFullGc::HeapDump,").count(), 3, "both dumps and the MXBean dump");
    assert_eq!(init.matches("DcmdFullGc::GcRun,").count(), 1, "GC.run");
    let walk = init
        .find("fn dcmd_live_heap_walk_collect(")
        .expect("the walk's collection");
    let walk = &init[walk..];
    let attached = walk
        .find("run_dcmd_full_gc_on_attach_thread(vm, kind)")
        .expect("tries the attached path");
    let latch = walk
        .find("request_diagnostic_command_gc(")
        .expect("then the latch");
    assert!(attached < latch);
}
