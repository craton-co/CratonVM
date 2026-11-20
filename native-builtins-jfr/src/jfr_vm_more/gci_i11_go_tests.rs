//! gci i11/go: the `jdk.ExecuteVMOperation` name of a collection.
use super::*;

fn cycle(old: bool, vm_operation: Option<&'static str>) -> GcCycle<'static> {
    GcCycle {
        gc_id: 1,
        name: "G1New",
        cause: "G1 Evacuation Pause",
        start_nanos: 0,
        duration_nanos: 0,
        heap_before: 0,
        committed_before: 0,
        heap_after: 0,
        committed_after: 0,
        max: 0,
        old,
        old_event: old,
        tenuring_threshold: None,
        phases: &[],
        metaspace_used: 0,
        metaspace_spaces: None,
        g1_type: Some("Normal"),
        vm_operation,
        longest_pause_nanos: None,
        zgc: None,
        pauses: &[],
        gc_thread: None,
        reference_counts: None,
        caller: None,
        metaspace_chunk_free_lists: false,
    }
}

/// gci i20/ob (`gci-i19ob-jfr-gc-row-field-residuals-FIXED-20261017` item 1):
/// `jdk.ExecuteVMOperation.caller` is the collection's requesting Java
/// thread when the site names one (recorded under its tid, as
/// `jdk.SystemGC`'s `eventThread`), else 0 as before.
#[test]
fn gci_i20_ob_execute_vm_operation_names_the_requesting_thread() {
    let src = include_str!("../jfr_vm_more.rs").replace("\r\n", "\n");
    let start = src.find("\npub fn gc_cycle(").expect("gc_cycle");
    let rest = &src[start + 1..];
    let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
    assert!(body.contains("(\"caller\", Val::Ref(caller))"));
    assert!(!body.contains("(\"caller\", Val::Ref(0))"));
    assert!(body.contains(".and_then(|who| caller_thread_ref(b, who))"));
    // The literal the VM builds carries the field (default None here).
    let mut c = cycle(false, Some("G1CollectForAllocation"));
    assert!(c.caller.is_none());
    c.caller = Some(JfrCallerThread {
        java_tid: 3,
        name: "main",
        os_tid: 7,
    });
    let mut b = Backend::default();
    let who = c.caller.as_ref().expect("set");
    assert_eq!(caller_thread_ref(&mut b, who), Some(3));
}

/// gci i23/pk (`gci-i20ob-g1-remark-and-cleanup-write-no-vm-operation-rows-FIXED-20261016`):
/// a non-collection VM-operation pause writes `gc_cycle`'s stop rows in
/// `gc_cycle`'s order, under the next shared safepoint id, with the
/// caller's operation name -- and no collection row.
#[test]
fn gci_i23pk_vm_operation_pause_writes_the_stop_rows_only() {
    let src = include_str!("../jfr_vm_more.rs").replace("\r\n", "\n");
    let start = src
        .find("\npub fn vm_operation_pause(")
        .expect("vm_operation_pause");
    let rest = &src[start + 1..];
    let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
    let begin = body
        .find("admit(b, \"jdk.SafepointBegin\", 0)")
        .expect("begin");
    let op = body
        .find("admit(b, \"jdk.ExecuteVMOperation\", duration)")
        .expect("operation");
    let end = body.find("admit(b, \"jdk.SafepointEnd\", 0)").expect("end");
    assert!(begin < op && op < end);
    assert!(body.contains("b.safepoint_id += 1;"));
    assert!(body.contains("enum_key_auto(b, \"jdk.types.VMOperationType\", operation)"));
    for absent in [
        "jdk.GarbageCollection",
        "jdk.GCPhasePause",
        "jdk.GCHeapSummary",
    ] {
        assert!(!body.contains(absent), "{absent}");
    }
}

/// gci i23/pk (`gci-i19ob-jfr-gc-row-field-residuals-FIXED-20261017` item 2):
/// `gcThreshold` is HotSpot's default `capacity_until_GC` -- 21 MiB for
/// a small program, the 40 %-free floor on the 64 KiB granule above it --
/// and never 0 again.
#[test]
fn gci_i23pk_the_metaspace_gc_threshold_is_hotspots_default_policy() {
    let mib = 1024 * 1024;
    assert_eq!(metaspace_gc_threshold(0), 21 * mib);
    assert_eq!(metaspace_gc_threshold(-5), 21 * mib);
    assert_eq!(metaspace_gc_threshold(8 * mib), 21 * mib);
    // 12.6 MiB is the last size whose 40 %-free floor fits in 21 MiB.
    assert_eq!(metaspace_gc_threshold(21 * mib * 60 / 100), 21 * mib);
    // 30 MiB used: 50 MiB floor, already granule-aligned.
    assert_eq!(metaspace_gc_threshold(30 * mib), 50 * mib);
    // One byte more rounds up to the next granule.
    assert_eq!(
        metaspace_gc_threshold(30 * mib + 1),
        50 * mib + 64 * 1024
    );
    assert!(metaspace_gc_threshold(i64::MAX) > 0, "saturates, no panic");
    // gci i31/hp: the rows read `metaspace_threshold_step`, which applies
    // this policy after a full collection only.
    let src = include_str!("../jfr_vm_more.rs").replace("\r\n", "\n");
    assert!(src.contains(concat!("(\"gcThreshold\", Val::Long(", "threshold)),")));
    assert!(!src.contains(concat!(
        "Val::Long(metaspace_gc_threshold(",
        "c.metaspace_used))"
    )));
}

/// gci i31/hp (vi20 Batch 3 pk `S-threshold`): `capacity_until_GC` is
/// state -- `MetaspaceSize` until the metadata passes it, raised by
/// `compute_new_size` after a full collection only, kept by the next
/// collection's Before-GC row.
#[test]
fn gci_i31hp_the_metaspace_threshold_moves_like_capacity_until_gc() {
    let mib = 1024 * 1024;
    let ms = 21 * mib;
    // pk23 S: ~14.2 MiB before the first (full) System.gc: Before GC is
    // `MetaspaceSize` (HotSpot 22020096), not `used / 0.6`.
    let used = 23658496 * 60 / 100;
    let (before, after) = metaspace_threshold_step(0, used, true);
    assert_eq!(before, ms);
    assert_eq!(after, 23658496);
    // The next collection's Before GC keeps what the full one left.
    assert_eq!(metaspace_threshold_step(after, used, false), (after, after));
    // A young collection never applies the 40 %-free policy.
    assert_eq!(metaspace_threshold_step(0, used, false), (ms, ms));
    // Small metadata: `MetaspaceSize` throughout.
    assert_eq!(metaspace_threshold_step(0, 8 * mib, true), (ms, ms));
    // Metadata past the threshold expands it at allocation time (granule).
    let (b, a) = metaspace_threshold_step(ms, 30 * mib + 1, false);
    assert_eq!(b, 30 * mib + 64 * 1024);
    assert_eq!(a, b);
    assert_eq!(metaspace_threshold_step(ms, 30 * mib, true).1, 50 * mib);
    // Never below what the previous collection left (no shrink modelled).
    assert_eq!(metaspace_threshold_step(50 * mib, mib, true), (50 * mib, 50 * mib));
    assert_eq!(metaspace_threshold_step(-1, -1, true), (ms, ms));
}

/// gci i23/pk (`gci-i19ob-jfr-gc-row-field-residuals-FIXED-20261017` item 3):
/// the `jdk.SystemGC` stack starts at the `Runtime.gc()` native frame
/// (added when the capture, which lists the frames below the running
/// native, lacks it; kept once when present) and lists innermost first;
/// `system_gc_by` writes the id it is handed.
#[test]
fn gci_i23pk_the_system_gc_stack_starts_at_runtime_gc() {
    use cratonvm_native_api::StackTraceEntry;
    use std::sync::Arc;
    let at = |class: &str, method: &str| StackTraceEntry {
        class_name: Arc::from(class),
        method_name: Arc::from(method),
        method_descriptor: None,
        source_file: None,
        line_number: 7,
        byte_code_index: 0,
        class_id: None,
        method_index: None,
    };
    // Outermost first, as `capture_stack_trace` lists it.
    let frames =
        system_gc_stack_frames(vec![at("Main", "main"), at("java.lang.System", "gc")]);
    let names: Vec<(&str, &str)> = frames
        .iter()
        .map(|f| (f.class_name.as_ref(), f.method_name.as_ref()))
        .collect();
    assert_eq!(
        names,
        [
            ("java.lang.Runtime", "gc"),
            ("java.lang.System", "gc"),
            ("Main", "main")
        ]
    );
    assert_eq!(frames[0].line_number, -2, "a native frame");
    assert_eq!(frames[0].method_descriptor.as_deref(), Some("()V"));
    let present = system_gc_stack_frames(vec![
        at("Main", "main"),
        at("java/lang/Runtime", "gc"),
    ]);
    assert_eq!(present.len(), 2, "not doubled");
    assert_eq!(present[0].class_name.as_ref(), "java/lang/Runtime");
    assert_eq!(system_gc_stack_frames(Vec::new()).len(), 1);
    let src = include_str!("../jfr_vm_more.rs").replace("\r\n", "\n");
    let start = src.find("\npub fn system_gc_by(").expect("system_gc_by");
    let rest = &src[start + 1..];
    let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
    assert!(body.contains("(\"stackTrace\", Val::Ref(stack_trace)),"));
}

/// gci i12/ge (findings page item 2): `longestPause` is the caller's
/// longest pause (capped at the duration), else the duration.
#[test]
fn gci_i12_ge_the_longest_pause_is_the_callers() {
    let mut c = cycle(true, Some("G1CollectFull"));
    c.duration_nanos = 9_000;
    assert_eq!(gc_longest_pause(&c), 9_000);
    c.longest_pause_nanos = Some(4_000);
    assert_eq!(gc_longest_pause(&c), 4_000);
    c.longest_pause_nanos = Some(12_000);
    assert_eq!(gc_longest_pause(&c), 9_000, "never above the sum");
}

/// gci i12/ge (findings page item 1): `jdk.SystemGC` is written once per
/// request by `system_gc`, no longer by `gc_cycle` per pause.
#[test]
fn gci_i12_ge_system_gc_rows_are_per_request() {
    let src = include_str!("../jfr_vm_more.rs").replace("\r\n", "\n");
    let start = src.find("\npub fn gc_cycle(").expect("gc_cycle");
    let rest = &src[start + 1..];
    let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
    assert!(!body.contains("admit(b, \"jdk.SystemGC\""));
    // gci i19/ob: the row is written by `system_gc_by` (`system_gc`
    // delegates with no caller).
    let start = src.find("\npub fn system_gc_by(").expect("system_gc_by");
    let rest = &src[start + 1..];
    let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
    assert!(body.contains("(\"invokedConcurrent\", Val::Bool(invoked_concurrent))"));
    let start = src.find("\npub fn system_gc(").expect("system_gc");
    let rest = &src[start + 1..];
    let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
    assert!(body.contains("system_gc_by(vm, None,"));
}

/// gci i19/ob (`gci-i12ge-...-review-findings-20261013` residual r1): the
/// caller is recorded under its Java tid (the key `register_thread` uses),
/// an existing record of that tid is kept, and a tid below 1 is refused so
/// the row falls back to the VM thread.
#[test]
fn gci_i19_ob_system_gc_names_the_calling_java_thread() {
    let mut b = Backend::default();
    let main = JfrCallerThread {
        java_tid: 3,
        name: "main",
        os_tid: 4242,
    };
    assert_eq!(caller_thread_ref(&mut b, &main), Some(3));
    let rec = b.tables.threads.get(&3).expect("recorded");
    assert_eq!(rec.java_name, "main");
    assert_eq!(rec.os_name, "main");
    assert_eq!(rec.java_tid, 3);
    assert_eq!(rec.os_tid, 4242);
    assert!(b.needs.threads.contains(&3));
    // A record made earlier for the same tid wins (same key, same thread).
    let renamed = JfrCallerThread {
        java_tid: 3,
        name: "other",
        os_tid: 1,
    };
    assert_eq!(caller_thread_ref(&mut b, &renamed), Some(3));
    assert_eq!(b.tables.threads.get(&3).map(|r| r.java_name.as_str()), Some("main"));
    // Not a Java thread id: no record, the caller uses the VM thread.
    let none = JfrCallerThread {
        java_tid: 0,
        name: "x",
        os_tid: 0,
    };
    assert_eq!(caller_thread_ref(&mut b, &none), None);
    assert!(!b.tables.threads.contains_key(&0));
}

/// gci i19/ob (residual r3): the `jdk.ConcurrentModeFailure` writer exists,
/// carries the cycle's `gcId`, and is admitted by name like every row.
#[test]
fn gci_i19_ob_concurrent_mode_failure_row_carries_the_gc_id() {
    let src = include_str!("../jfr_vm_more.rs").replace("\r\n", "\n");
    let start = src
        .find("\npub fn concurrent_mode_failure(")
        .expect("concurrent_mode_failure");
    let rest = &src[start + 1..];
    let body = &rest[..rest.find("\n\u{7d}\n").expect("its end")];
    assert!(body.contains("admit(b, \"jdk.ConcurrentModeFailure\", 0)"));
    assert!(body.contains("(\"gcId\", Val::Int(gc_id))"));
}

/// gci i12/ge (`gci-i11go-...-review-findings-20261013` item 3): the
/// event kind is the caller's `old_event`, never a substring of the cause.
#[test]
fn gci_i12_ge_the_event_kind_is_the_callers() {
    let mut c = cycle(false, None);
    c.cause = "System.gc()";
    assert_eq!(gc_kind_event(&c), "jdk.YoungGarbageCollection");
    c.old_event = true;
    c.cause = "Diagnostic Command";
    assert_eq!(gc_kind_event(&c), "jdk.OldGarbageCollection");
    // `old` (the operation fallback's input) does not decide the kind.
    c.old = true;
    c.old_event = false;
    assert_eq!(gc_kind_event(&c), "jdk.YoungGarbageCollection");
}

/// zt t1/jf: HotSpot 25 ZGC's VM operation per pause name -- the young
/// generation's for a minor, the old one's (after the both-generation
/// mark start) for a major.
#[test]
fn zt1jf_zgc_pause_vm_operations_are_hotspots() {
    let major = [
        ("Pause Mark Start (Major)", "ZMarkStartYoungAndOld"),
        ("Pause Mark End", "ZMarkEndOld"),
        ("Pause Relocate Start", "ZRelocateStartOld"),
    ];
    for (pause, op) in major {
        assert_eq!(zgc_pause_vm_operation(pause, true), op);
    }
    let minor = [
        ("Pause Mark Start", "ZMarkStartYoung"),
        ("Pause Mark End", "ZMarkEndYoung"),
        ("Pause Relocate Start", "ZRelocateStartYoung"),
    ];
    for (pause, op) in minor {
        assert_eq!(zgc_pause_vm_operation(pause, false), op);
    }
}

/// zt t1/jf: one per-pause safepoint takes exactly one fresh id (a
/// backend that is not recording writes nothing but still numbers it,
/// as `gc_cycle` does).
#[test]
fn zt1jf_a_per_pause_safepoint_takes_one_fresh_id() {
    let mut b = Backend {
        safepoint_id: 41,
        ..Default::default()
    };
    safepoint_operation_rows(&mut b, 1, 0, "ZMarkEndOld", 0, 5);
    assert_eq!(b.safepoint_id, 42);
}

/// `jfr-gc-safepoint-rows-have-no-behavioural-unit-test` (X1-13): run
/// `gc_cycle` against a mock recording and count what it appended. A ZGC
/// collection with its three named pauses and `pause_vm_operations` on
/// writes one `SafepointBegin` / `ExecuteVMOperation` / `SafepointEnd`
/// triple per pause (HotSpot 25 ZGC's VM operations, each its own
/// safepoint id); `zgc: None` (G1 / Serial / Parallel / Generational)
/// writes one triple for the collection; the ZGC arm with the switch off
/// (`CRATONVM_ZGC_JFR_PAUSE_VM_OPERATIONS=0`) is the kill control: one.
#[test]
fn zt1jf_per_pause_safepoints_are_zgc_only() {
    use crate::jfr_backend::test_recording;
    const SAFEPOINT: &[(&str, &str)] = &[("safepointId", "long")];
    const EVENTS: &[(&str, &[(&str, &str)])] = &[
        ("jdk.SafepointBegin", SAFEPOINT),
        (
            "jdk.ExecuteVMOperation",
            &[
                ("operation", "jdk.types.VMOperationType"),
                ("safepoint", "boolean"),
                ("safepointId", "long"),
            ],
        ),
        ("jdk.SafepointEnd", SAFEPOINT),
    ];
    let pauses = [
        ("Pause Mark Start (Major)", 1_000u64, 100u64),
        ("Pause Mark End", 1_100, 600),
        ("Pause Relocate Start", 1_700, 300),
    ];
    let z = |on: bool| ZgcCycleJfr {
        major: true,
        relocation_set: None,
        relocation_groups: &[],
        uncommit: None,
        young_tenuring_threshold: None,
        reference_counts: None,
        cycle_span: None,
        cycle_heap_before: None,
        pause_vm_operations: on,
    };
    let (on, off) = (z(true), z(false));
    // (VM id, the collection's ZGC half, its named pause rows)
    let arms: [(usize, Option<&ZgcCycleJfr<'_>>, &[(&str, u64, u64)], usize); 4] = [
        (0x5afe_9001, Some(&on), &pauses, 3),
        (0x5afe_9002, None, &pauses, 1),
        (0x5afe_9003, Some(&off), &pauses, 1),
        // No named rows: the one collection-wide safepoint even when on.
        (0x5afe_9004, Some(&on), &[], 1),
    ];
    for (vm, zgc, rows, want) in arms {
        let rec = test_recording::open(vm, EVENTS);
        let mut c = cycle(true, None);
        c.name = if zgc.is_some() { "ZGC Major" } else { "G1Old" };
        c.start_nanos = 1_000;
        c.duration_nanos = 1_000;
        c.zgc = zgc;
        c.pauses = rows;
        gc_cycle(vm, &c);
        let begins = rec.rows_of("jdk.SafepointBegin");
        let ops = rec.rows_of("jdk.ExecuteVMOperation");
        let ends = rec.rows_of("jdk.SafepointEnd");
        let label = format!("arm {vm:#x}");
        assert_eq!(begins.len(), want, "{label}: SafepointBegin rows");
        assert_eq!(ops.len(), want, "{label}: ExecuteVMOperation rows");
        assert_eq!(ends.len(), want, "{label}: SafepointEnd rows");
        // Each triple shares one id, and the ids are fresh per safepoint.
        let ids: Vec<i64> = begins.iter().map(|r| r.get("safepointId").unwrap()).collect();
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(ops[i].get("safepointId"), Some(*id), "{label}");
            assert_eq!(ends[i].get("safepointId"), Some(*id), "{label}");
            assert_eq!(ops[i].get("safepoint"), Some(1), "{label}");
        }
        let mut distinct = ids.clone();
        distinct.dedup();
        assert_eq!(distinct.len(), ids.len(), "{label}: ids {ids:?}");
        let names: Vec<String> = ops
            .iter()
            .map(|r| {
                let key = r.get("operation").unwrap();
                rec.enum_text("jdk.types.VMOperationType", key).unwrap()
            })
            .collect();
        if want == 3 {
            assert_eq!(names, ["ZMarkStartYoungAndOld", "ZMarkEndOld", "ZRelocateStartOld"]);
        } else {
            assert_eq!(names.len(), 1, "{label}");
        }
    }
}

#[test]
fn gci_i11_go_the_callers_operation_name_wins() {
    assert_eq!(
        gc_vm_operation(&cycle(false, Some("G1CollectForAllocation"))),
        "G1CollectForAllocation"
    );
    assert_eq!(gc_vm_operation(&cycle(true, Some("G1CollectFull"))), "G1CollectFull");
    // No name (ZGC): the historical spelling.
    assert_eq!(gc_vm_operation(&cycle(false, None)), "Collect");
    assert_eq!(gc_vm_operation(&cycle(true, None)), "GC_HeapInspection");
}

/// zt t2/jf: the ZGC code-heap rows name exactly the JMX `CodeHeap`
/// pools (the MXBean and the JFR row describe the same heaps), the JIT's
/// code in the non-profiled one; and the generic row's inline-string
/// `codeBlobType` (a constant-pool field: it read `null`) is not what the
/// ZGC arm writes.
#[test]
fn zt2jf_code_heap_rows_name_the_jmx_code_heap_pools() {
    let pools = cratonvm_native_api::registry::JMX_NON_HEAP_POOL_NAMES;
    for name in CODE_HEAP_NAMES {
        assert!(pools.contains(&name), "{name}");
    }
    assert_eq!(CODE_HEAP_NAMES[2], CODE_HEAP_NON_PROFILED);
    let src = include_str!("../jfr_vm_more.rs").replace("\r\n", "\n");
    let arm = src
        .find("\"jdk.CodeCacheStatistics\" if zgc_jfr_code_heap_rows(ctx) => {")
        .expect("the ZGC arm");
    let body: String = src[arm..].chars().take(2_000).collect();
    assert!(body.contains("(\"codeBlobType\", Val::Ref(key)),"));
    assert!(body.contains("enum_key_auto(b, \"jdk.types.CodeBlobType\", name)"));
}


/// zt t5/jf: a `jdk.SystemGC` row ends with the ZGC collection recorded
/// inside it (taken once); a collection outside the request, or none,
/// leaves the writer's duration.
#[test]
fn zt5jf_a_system_gc_row_ends_with_its_collection() {
    let d = |b: &mut Backend| zgc_system_gc_duration(b, 1_000, 30_000);
    let mut b = Backend::default();
    assert_eq!(d(&mut b), 30_000, "none on record");
    b.last_zgc_collection_end = Some(5_000);
    assert_eq!(d(&mut b), 4_000);
    assert_eq!(b.last_zgc_collection_end, None, "taken");
    assert_eq!(d(&mut b), 30_000, "taken once");
    b.last_zgc_collection_end = Some(500);
    assert_eq!(d(&mut b), 30_000, "before the request");
    b.last_zgc_collection_end = Some(40_000);
    assert_eq!(d(&mut b), 30_000, "after it");
}

/// zt t3/jf (zt2x1 item 4): a major's young event ends at its mark-end
/// pause, inside the cycle; a minor, or a major without named rows,
/// spans the cycle.
#[test]
fn zt3jf_a_majors_young_event_ends_at_its_mark_end() {
    let rows = [
        ("Pause Mark Start (Major)", 1_000u64, 100u64),
        ("Pause Mark End", 1_100, 600),
        ("Pause Relocate Start", 1_700, 300),
    ];
    assert_eq!(zgc_young_span(&rows, 0, (1_000, 1_000), true), (1_000, 100));
    assert_eq!(zgc_young_span(&rows, 100, (900, 1_000), true), (900, 100));
    assert_eq!(zgc_young_span(&rows, 0, (1_000, 1_000), false), (1_000, 1_000));
    assert_eq!(zgc_young_span(&[], 0, (1_000, 1_000), true), (1_000, 1_000));
    // A row before the span start clamps to 0, never negative.
    assert_eq!(zgc_young_span(&rows, 0, (5_000, 1_000), true), (5_000, 0));
}

/// zt t2/jf (R6): a ZGC collection written as one safepoint names the
/// operation that opens its cycle; `CRATONVM_ZGC_JFR_PAUSE_VM_OPERATIONS=0`
/// (`pause_vm_operations: false`) keeps `Collect`; a caller's name wins.
#[test]
fn zt2jf_a_one_row_zgc_collection_names_its_opening_operation() {
    let z = |major: bool, on: bool| ZgcCycleJfr {
        major,
        relocation_set: None,
        relocation_groups: &[],
        uncommit: None,
        young_tenuring_threshold: None,
        reference_counts: None,
        cycle_span: None,
        cycle_heap_before: None,
        pause_vm_operations: on,
    };
    let (major, minor, off) = (z(true, true), z(false, true), z(true, false));
    let mut c = cycle(false, None);
    c.zgc = Some(&major);
    assert_eq!(gc_vm_operation(&c), "ZMarkStartYoungAndOld");
    c.zgc = Some(&minor);
    assert_eq!(gc_vm_operation(&c), "ZMarkStartYoung");
    c.zgc = Some(&off);
    assert_eq!(gc_vm_operation(&c), "Collect");
    c.zgc = Some(&major);
    c.vm_operation = Some("HeapDumper");
    assert_eq!(gc_vm_operation(&c), "HeapDumper");
}
