use super::*;

/// Walk every workspace source tree and require each address-keyed
/// `ObjectRef` declaration to be accounted for in [`census::AUDITED`].
///
/// Scans DIRECTORIES rather than a list of file names on purpose: a module
/// split renames files, and a gate keyed on file names goes quietly
/// fail-open at exactly the moment the code it guards was reorganised. For
/// the same reason it fails loudly — rather than passing vacuously — when
/// it cannot find the workspace root, when the walk turns up implausibly
/// few files, or when the pattern matches nothing at all.
#[test]
fn the_address_keyed_table_census_is_complete() {
    fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                collect_rs(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the vm crate directory must have a parent")
        .to_path_buf();
    assert!(
        workspace.join("Cargo.toml").is_file(),
        "workspace root not found at {} — failing rather than scanning nothing",
        workspace.display()
    );

    let mut files = Vec::new();
    for entry in std::fs::read_dir(&workspace).expect("workspace root is readable") {
        let src = entry.expect("readable directory entry").path().join("src");
        if src.is_dir() {
            collect_rs(&src, &mut files);
        }
    }
    assert!(
        files.len() > 100,
        "only {} source files found under {} — the directory walk is broken",
        files.len(),
        workspace.display()
    );

    // Declarations, not mentions: the pattern inside a doc comment or a
    // prose note is not a table. Everything from the first `//` is prose.
    let declarations_in = |text: &str| {
        text.lines()
            .filter(|line| {
                let code = line.split("//").next().unwrap_or("");
                code.contains("HashMap<ObjectRef") || code.contains("HashSet<ObjectRef")
            })
            .count()
    };

    let mut findings: Vec<String> = Vec::new();
    let mut total_declarations = 0usize;
    let mut seen: Vec<&str> = Vec::new();
    for file in &files {
        let rel = file
            .strip_prefix(&workspace)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");
        // This module is the fixup machinery and `value.rs` is where the
        // contract itself is argued; neither owns a table.
        if [
            "vm/src/memory/addr_keyed.rs",
            "vm/src/memory/addr_keyed/census.rs",
            "vm/src/memory/addr_keyed/tests.rs",
            "vm/src/memory/gc/tests.rs",
            "types/src/value.rs",
            "types/src/value/tests.rs",
        ]
        .iter()
        .any(|path| rel.ends_with(path))
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        let count = declarations_in(&text);
        if count == 0 {
            continue;
        }
        total_declarations += count;
        match super::census::AUDITED
            .iter()
            .find(|a| rel.ends_with(a.path))
        {
            None => findings.push(format!(
                "{rel}: {count} address-keyed declaration(s), NOT in the census"
            )),
            Some(audited) => {
                seen.push(audited.path);
                if audited.declarations != count {
                    findings.push(format!(
                        "{rel}: census says {} declaration(s), found {count} — a \
                         table was added or removed here, so re-audit it and \
                         update the entry",
                        audited.declarations
                    ));
                }
            }
        }
    }
    for audited in super::census::AUDITED {
        if !seen.contains(&audited.path) {
            findings.push(format!(
                "{}: in the census but no longer declares an address-keyed \
                 table — drop the entry (or fix the path if the file moved)",
                audited.path
            ));
        }
    }

    assert!(
        total_declarations > 0,
        "the census matched no declarations at all — the pattern stopped \
         matching, so this test was about to pass vacuously"
    );
    assert!(
        findings.is_empty(),
        "address-keyed `ObjectRef` table census is out of date.\n\
         `ObjectRef`'s Hash/Eq are addresses: a moving collection strands \
         live entries, and a DEAD entry collides with whatever object the \
         allocator next places on that address — a silent wrong answer, not \
         a miss (docs/threading/objectref-concurrency-contract.md §7.3).\n\
         Give the table a scan+remap pair (see \
         `net_phase_e::gc_scan_inet_addr_roots`), route it through \
         `addr_keyed::remap_and_sweep`, or state why it is safe — then \
         record it in `census::AUDITED`.\n  {}",
        findings.join("\n  ")
    );
}

/// gcg g2/k (`common-w33b-proposal-census-raw-address-keyed-tables-DONE-20261015`): the
/// census above sees tables keyed by `ObjectRef`, not tables keyed by an
/// address held as a plain `usize` -- the same hazard (a dead object's row
/// answers for whatever the allocator puts at its address next), invisible
/// to the pattern. This sibling is a POPULATION ratchet over
/// `HashMap<usize` / `HashMap<(usize` / `HashSet<usize` / `HashSet<(usize`
/// declarations (FxHashMap / FxHashSet included) in the trees where object
/// addresses are routinely keys. Most such keys are not addresses at all
/// (class ids, VM identities, pcs), which is why it counts instead of
/// failing on each one.
///
/// A declaration is CLASSIFIED when its line, or the line right above it,
/// carries `addr-keyed: <disposition>` in a comment (for example
/// `// addr-keyed: REMAPPED by ..., PRUNED by ...`, or `addr-keyed: not an
/// address`). Unclassified declarations may not grow past each tree's
/// baseline; `vm/src/threading` starts fully classified, so any new one
/// there must carry the marker. Lower a baseline when you classify or
/// remove a declaration.
///
/// The baselines were counted on the gcg g2 tree (2026-10-01) with the
/// rule above (code is the text before the first `//`).
#[test]
fn gcg_k2_raw_address_keyed_declarations_are_classified_or_counted() {
    const PATTERNS: [&str; 4] = [
        "HashMap<usize",  // addr-keyed: a pattern literal, not a table
        "HashMap<(usize", // addr-keyed: a pattern literal, not a table
        "HashSet<usize",  // addr-keyed: a pattern literal, not a table
        "HashSet<(usize", // addr-keyed: a pattern literal, not a table
    ];
    const MARKER: &str = "addr-keyed:";
    const BASELINES: [(&str, usize); 8] = [
        // 325 -> 336 at the gcg round's 2026-10-02 merge of `dev`: dev brought 11
        // unclassified declarations (classloader.rs 3, classloader_value_sidetable.rs 2,
        // jboss_jdkspecific.rs 2, lang_class.rs 2, reflect_annotations.rs 1 -- plus 3 in
        // test_utils.rs). They need a disposition; see the gcg handoff, item "dev's
        // address-keyed declarations". 336 -> 338 at the second merge (dev `67f4beb552`):
        // lang_class.rs `ANNOTATION_JDK_HANDLERS` (keyed by the annotation proxy's address)
        // and the `dropped` proxy-address set that prunes it.
        // 338 -> 224 (gch h1/ak): every dev declaration classified, plus the classloader /
        // lang_class / logging / lang_math / panama families
        // (`docs/feature-designs/real-jdk-mode.md`).
        // 224 -> 220 (interpreter round i4 wave 96, lane L5): `cargo fmt` (dev
        // `dfedfc98ca`, i84 `b49c8bc6e6`) moved seven declarations off the line under
        // their marker (227 unclassified); the markers now sit on the declaration
        // lines (classloader.rs 2, jboss_jdkspecific.rs, lang_class.rs, lang_math.rs,
        // panama.rs 2). The gci round's 2026-10-15 merge of `dev` keeps 220.
        ("native-builtins/src", 220),
        ("native-io/src", 23),
        // 47 -> 49 at the same merge: dev's `deopt_resume.rs` `by_point` and
        // `invoke.rs` `SEEN` (see the gcg handoff). dev then brought `lambda.rs`
        // `LAMBDA_DOOR_SITES` (50, red on dev 2eee67be76). gch h1/ak classified `by_point`,
        // `SEEN`, `LAMBDA_DOOR_SITES` and `jit_bridge.rs`'s `osr_backend` parameter: 46.
        // 46 -> 45 (gci round, 2026-10-13): the gci waves added two, both now classified
        // (i5/hd `hprof.rs` `attributed`, i11/vt `interpreter.rs` unmounted-frames
        // `current`); the census counts 45, so the one row of slack is taken back.
        ("vm/src/runtime", 45),
        ("vm/src/threading", 0),
        // gcj j1/st (2026-10-19): two more trees where object addresses are keys --
        // the GC's own side tables and the VM core. Counted on the gcj base with the
        // rule above after classifying this lane's files (`addr_keyed.rs`,
        // `string_table.rs`): `vm/src/memory` 13 (`gc.rs` 10, `roots.rs` 3: pause-local
        // root / destination sets), `vm/src/vm` 9 (`vm_exec.rs` 7, `vm_init.rs` 1,
        // `vm_util.rs` 1 -- join-handle pointers, VM identities, pause-local sets; none
        // keyed by a heap address that outlives a pause).
        ("vm/src/memory", 13),
        ("vm/src/vm", 9),
        // gcj j2/jn2 (j1-st X1): `vm/src/native` starts fully classified -- the seven
        // `jni.rs` declarations are C-heap `Box` / buffer addresses (global-ref boxes,
        // element / string / critical copies), the G1 critical pins (a heap address held
        // in place by its region pin and strong global until its release removes the row)
        // and one pause-local remark set.
        ("vm/src/native", 0),
        // gcj j2/jt2 (j1-st X2): every declaration in vm/src/jit is classified.
        ("vm/src/jit", 0),
    ];

    fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the vm crate directory must have a parent")
        .to_path_buf();
    let mut findings: Vec<String> = Vec::new();
    let mut classified_total = 0usize;
    for (tree, baseline) in BASELINES {
        let root = workspace.join(tree);
        assert!(
            root.is_dir(),
            "{} is not a directory — the census would scan nothing",
            root.display()
        );
        let mut files = Vec::new();
        collect_rs(&root, &mut files);
        assert!(!files.is_empty(), "no source files under {tree}");
        let mut unclassified: Vec<String> = Vec::new();
        for file in &files {
            let Ok(text) = std::fs::read_to_string(file) else {
                continue;
            };
            let mut previous = "";
            for (n, line) in text.lines().enumerate() {
                let code = line.split("//").next().unwrap_or("");
                if PATTERNS.iter().any(|p| code.contains(p)) {
                    if line.contains(MARKER) || previous.contains(MARKER) {
                        classified_total += 1;
                    } else {
                        let rel = file.strip_prefix(&workspace).unwrap_or(file);
                        unclassified.push(format!(
                            "{}:{}",
                            rel.to_string_lossy().replace('\\', "/"),
                            n + 1
                        ));
                    }
                }
                previous = line;
            }
        }
        if unclassified.len() > baseline {
            findings.push(format!(
                "{tree}: {} unclassified raw-address-keyed declaration(s), \
                 baseline {baseline}. \
                 Classify the new one with an `{MARKER} <disposition>` comment on or \
                 above its line (or raise nothing: give it a GC disposition first). \
                 Unclassified lines:\n    {}",
                unclassified.len(),
                unclassified.join("\n    ")
            ));
        }
    }
    assert!(
        classified_total > 0,
        "no classified declaration found — the marker or the pattern stopped matching, \
         so this ratchet was about to pass vacuously"
    );
    assert!(findings.is_empty(), "{}", findings.join("\n"));
}

const A: usize = 0x1_0000;
const B: usize = 0x2_0000;
const C: usize = 0x3_0000;

fn key(addr: usize) -> ObjectRef {
    object_ref_at(addr)
}

fn table(entries: &[(usize, u32)]) -> FxHashMap<ObjectRef, u32> {
    entries.iter().map(|&(a, v)| (key(a), v)).collect()
}

fn nothing_moved() -> cratonvm_types::PointerMap {
    cratonvm_types::PointerMap::default()
}

#[test]
fn moved_entry_is_rekeyed_and_keeps_its_value() {
    let mut t = table(&[(A, 7)]);
    let map = cratonvm_types::PointerMap::from_iter([(A, B)]);

    let stats = remap_and_sweep(&mut t, &map, &|_| true);

    assert_eq!(stats.moved, 1);
    assert_eq!(t.get(&key(B)), Some(&7), "value must follow the object");
    assert!(!t.contains_key(&key(A)), "old address must not linger");
}

#[test]
fn live_unmoved_entry_survives_a_nonmoving_sweep() {
    let mut t = table(&[(A, 7)]);

    let stats = remap_and_sweep(&mut t, &nothing_moved(), &|addr| addr == A);

    assert_eq!(
        stats,
        SweepStats {
            moved: 0,
            retained: 1,
            dropped: 0
        }
    );
    assert_eq!(t.get(&key(A)), Some(&7));
}

/// The correctness-critical case: a non-moving collection publishes an
/// empty pointer map, so a remap-only fixup would be a no-op and the
/// dead entry would stay to collide with whatever is allocated onto
/// its reclaimed address next.
#[test]
fn dead_entry_is_dropped_even_when_nothing_moved() {
    let mut t = table(&[(A, 7)]);

    let stats = remap_and_sweep(&mut t, &nothing_moved(), &|_| false);

    assert_eq!(stats.dropped, 1);
    assert!(t.is_empty(), "reclaimed address must not stay keyed");
}

#[test]
fn mixed_cycle_sorts_each_entry_into_the_right_bucket() {
    let mut t = table(&[(A, 1), (B, 2), (C, 3)]);
    // A relocated to 0x40000; B survived in place; C died.
    let map = cratonvm_types::PointerMap::from_iter([(A, 0x4_0000)]);

    let stats = remap_and_sweep(&mut t, &map, &|addr| addr == B);

    assert_eq!(
        stats,
        SweepStats {
            moved: 1,
            retained: 1,
            dropped: 1
        }
    );
    assert_eq!(t.get(&key(0x4_0000)), Some(&1));
    assert_eq!(t.get(&key(B)), Some(&2));
    assert_eq!(t.len(), 2);
}

/// A relocated survivor evacuated onto a dead entry's old address must
/// win, whatever order the two entries come out of the table in.
#[test]
fn relocated_survivor_wins_a_destination_collision() {
    let mut t = table(&[(A, 1), (B, 2)]);
    // A moves onto B's address; B is dead, but `is_live(B)` now reports
    // true because A occupies that address.
    let map = cratonvm_types::PointerMap::from_iter([(A, B)]);

    let stats = remap_and_sweep(&mut t, &map, &|_| true);

    assert_eq!(t.len(), 1);
    assert_eq!(t.get(&key(B)), Some(&1), "A's value, not B's stale one");
    assert_eq!(
        stats,
        SweepStats {
            moved: 1,
            retained: 0,
            dropped: 1
        }
    );
}

/// gc-common w16-x: the batched verdict is the per-address one.
#[test]
fn in_place_verdict_agrees_with_survived_in_place() {
    use crate::memory::vm_heap::{GcBackend, VmHeap};
    use cratonvm_types::ClassId;

    for backend in [GcBackend::Generational, GcBackend::G1] {
        let heap = VmHeap::new(backend, 16 * 1024 * 1024);
        let live = heap.alloc_object(ClassId::new(7), 1).as_ptr() as usize;
        let mut addrs = vec![live, live + 8, 0x1_0000, 8];
        if let Some((lo, hi)) = heap.young_inactive_semispace_range() {
            addrs.extend([lo, hi - 16]);
        }
        let verdict = InPlaceVerdict::capture(&heap);
        for a in addrs {
            assert_eq!(verdict.survived(a), survived_in_place(&heap, a), "{a:#x}");
        }
    }
}

/// gc-common w18-a: the batched verdict is the per-address one for every
/// row a REAL Generational collection leaves behind -- three dead rows in
/// four, the shape that made the per-address form quadratic -- while the
/// reclaimed-hole probe behind it is taken at most once for the whole
/// sweep (`OnceCell`, by construction).
#[test]
fn in_place_verdict_agrees_across_a_collection_with_many_dead_rows() {
    use crate::memory::vm_heap::{GcBackend, VmHeap};
    use cratonvm_types::ClassId;

    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    let monitors = crate::threading::monitor::MonitorTable::new();
    let objs: Vec<_> = (0..256)
        .map(|i| heap.alloc_object(ClassId::new(0), (i % 4) + 1))
        .collect();
    let mut roots: Vec<_> = objs.iter().step_by(4).copied().collect();
    // SAFETY: single-threaded test; this heap has no other mutator.
    let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
    let result = heap.collect_garbage(&stw, &mut roots, &monitors);

    let verdict = InPlaceVerdict::capture(&heap);
    let mut dead = 0usize;
    for obj in &objs {
        let a = obj.as_ptr() as usize;
        // A moved row is answered through the pointer map by every sweep,
        // before either verdict is asked.
        if result.pointer_map.contains_key(&a) {
            continue;
        }
        let want = survived_in_place(&heap, a);
        dead += usize::from(!want);
        assert_eq!(verdict.survived(a), want, "{a:#x}");
    }
    assert!(dead > 0, "the fixture must exercise dead rows");
}

/// gc-common w18-a: the probe is lazy. A window whose rows never reach the
/// reclaimed-hole arm (here: addresses that are not objects at all) never
/// copies the Generational young free list.
#[test]
fn in_place_verdict_takes_no_probe_until_an_address_needs_it() {
    use crate::memory::vm_heap::{GcBackend, VmHeap};

    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    let verdict = InPlaceVerdict::capture(&heap);
    assert!(!verdict.survived(0x1_0000), "not a heap address");
    assert!(!verdict.survived(8), "the null page");
    assert!(!verdict.probe_taken(), "no address needed the free list");
}

/// `survived_in_place` never drops a row for an address the collector
/// says is reclaimed memory is NOT (a live, parseable object), and always
/// drops one it says IS -- here the inactive young semispace, which no live
/// object ever occupies.
#[test]
fn survived_in_place_keeps_a_live_object_and_drops_reclaimed_space() {
    use crate::memory::vm_heap::{GcBackend, VmHeap};
    use cratonvm_types::ClassId;

    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    let live = heap.alloc_object(ClassId::new(7), 1);
    let live_addr = live.as_ptr() as usize;
    if heap.is_object_address(live_addr).is_some() {
        assert!(
            survived_in_place(&heap, live_addr),
            "a live object must never be judged dead by the reclaimed-space test"
        );
    }
    let (inactive_lo, _) = heap
        .young_inactive_semispace_range()
        .expect("the Generational heap has a semispace pair");
    assert!(
        !survived_in_place(&heap, inactive_lo),
        "the inactive semispace holds no live object; a row keyed there is dead"
    );
    assert!(
        !survived_in_place(&heap, 0x1_0000),
        "not a heap address at all"
    );
}

/// The sweep-only entry point for reclamations outside a pause runs on a
/// fresh VM without touching anything it should not (both tables empty).
#[test]
fn sweep_address_keyed_tables_is_callable_on_an_idle_vm() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    sweep_address_keyed_tables(&shared);
}

/// gc-common w3-g: the out-of-pause sweep (the entry point the Generational
/// concurrent old-gen sweep and G1's remark cleanup now call) drops a
/// Throwable-trace row keyed at space the collector reports RECLAIMED --
/// here the inactive young semispace, which no live object occupies -- and
/// keeps the row of a live object. This is "free at A, sweep before A is
/// reused": without the sweep the dead row would stay until the next STW
/// collection, by which time an object placed at A would inherit it.
#[test]
fn the_out_of_pause_sweep_drops_a_row_keyed_at_reclaimed_space() {
    use crate::config::{GcAlgorithm, VmConfig};
    use cratonvm_types::ClassId;

    let shared = crate::vm::SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    let heap = &shared.mem.heap;
    let live = heap.alloc_object(ClassId::new(7), 1);
    let (dead_addr, _) = heap
        .young_inactive_semispace_range()
        .expect("the Generational heap has a semispace pair");
    let dead = object_ref_at(dead_addr);
    let no_frames: std::sync::Arc<[crate::native::registry::StackTraceEntry]> =
        std::sync::Arc::from(Vec::new());
    shared.store_throwable_stack_trace(live, no_frames.clone());
    shared.store_throwable_stack_trace(dead, no_frames);
    assert!(shared.throwable_stack_trace_for(dead).is_some());

    sweep_address_keyed_tables(&shared);

    assert!(
        shared.throwable_stack_trace_for(dead).is_none(),
        "a row keyed at reclaimed space must be dropped by the out-of-pause sweep"
    );
    if heap.is_object_address(live.as_ptr() as usize).is_some() {
        assert!(
            shared.throwable_stack_trace_for(live).is_some(),
            "a live object's row must survive the sweep"
        );
    }
}

/// gc-common w5-g: the span test the concurrent sweep's freed-span drop
/// uses. `start` and `start + len - 1` are in, `start + len` is out, a gap
/// between spans is out, and no spans means nothing is in.
#[test]
fn in_spans_boundaries() {
    let spans = [(0x1000usize, 0x40usize), (0x2000, 0x10)];
    assert!(in_spans(0x1000, &spans));
    assert!(in_spans(0x103F, &spans));
    assert!(!in_spans(0x1040, &spans));
    assert!(!in_spans(0x0FFF, &spans));
    assert!(!in_spans(0x1800, &spans), "the gap between spans");
    assert!(in_spans(0x2000, &spans));
    assert!(in_spans(0x200F, &spans));
    assert!(!in_spans(0x2010, &spans));
    assert!(!in_spans(0x1000, &[]));
}

/// gc-common w5-g (`common-w3g-concurrent-sweep-direct-old-alloc-window`):
/// the freed-span drop removes a Throwable-trace row keyed inside a freed
/// span WITHOUT asking the heap -- so a humongous array that has since
/// landed on the span cannot make it look live -- and keeps a row keyed
/// outside every span.
#[test]
fn the_freed_span_drop_is_a_pure_range_test() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let dead = object_ref_at(A + 0x40);
    let outside = object_ref_at(B);
    let no_frames: std::sync::Arc<[crate::native::registry::StackTraceEntry]> =
        std::sync::Arc::from(Vec::new());
    shared.store_throwable_stack_trace(dead, no_frames.clone());
    shared.store_throwable_stack_trace(outside, no_frames);

    drop_address_keyed_rows_in(&shared, &[]);
    assert!(
        shared.throwable_stack_trace_for(dead).is_some(),
        "no spans: no-op"
    );

    drop_address_keyed_rows_in(&shared, &[(A, 0x100)]);
    assert!(
        shared.throwable_stack_trace_for(dead).is_none(),
        "a row keyed inside a freed span is dropped"
    );
    assert!(
        shared.throwable_stack_trace_for(outside).is_some(),
        "a row keyed outside every span is kept"
    );
}

/// gcd d10/r: an unmoved row keyed at the address ANOTHER object was
/// relocated onto is dead, even when that object has no row of its own --
/// the slide collision `relocated_survivor_wins_a_destination_collision`
/// covers only when both have rows.
#[test]
fn gcd_d10r_an_unmoved_row_at_a_relocation_destination_is_dropped() {
    // B died; A (no row here) slid onto B's base. `is_live(B)` is true
    // after the pause -- A lives there now.
    let mut t = table(&[(B, 2), (C, 3)]);
    let map = cratonvm_types::PointerMap::from_iter([(A, B)]);

    let stats = remap_and_sweep(&mut t, &map, &|_| true);

    assert_eq!(
        stats,
        SweepStats {
            moved: 0,
            retained: 1,
            dropped: 1
        }
    );
    assert!(!t.contains_key(&key(B)), "B's row must not pass to A");
    assert_eq!(t.get(&key(C)), Some(&3), "an uninvolved survivor stays");
}

/// gcd d10/r: the destination set answers "another survivor now lives
/// here" for a moved pair's target, never for its source, never for an
/// identity pair (a stationary survivor), and is built lazily.
#[test]
fn gcd_d10r_relocation_targets_claim_only_other_objects_destinations() {
    let map = cratonvm_types::PointerMap::from_iter([(A, B), (C, C)]);
    let targets = RelocationTargets::consulting(Some(&map));
    assert!(!targets.built(), "nothing asked yet");
    assert!(targets.claims(B), "A was relocated onto B");
    assert!(targets.built());
    assert!(!targets.claims(A), "a source is not a destination");
    assert!(
        !targets.claims(C),
        "an identity pair is a stationary survivor"
    );

    let none = RelocationTargets::none();
    assert!(!none.claims(B));
    assert!(!none.built(), "an unarmed screen never builds a set");
}

/// gcd d10/r: the screen is armed only where a relocation can land a
/// survivor on the base of an object that died in the same pause: never on
/// G1, on Generational only when the cycle reclaimed old storage, and never
/// for an empty map.
#[test]
fn gcd_d10r_the_relocation_screen_is_armed_only_where_a_base_can_be_reused() {
    use crate::memory::vm_heap::{GcBackend, VmHeap};

    /// `old_gen_reclaimed_last_cycle` is this thread's; put it back.
    struct RestoreReclaimed(bool);
    impl Drop for RestoreReclaimed {
        fn drop(&mut self) {
            cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(self.0);
        }
    }
    let _restore = RestoreReclaimed(cratonvm_gc::gc_quiescence::old_gen_reclaimed_last_cycle());

    let map = cratonvm_types::PointerMap::from_iter([(A, B)]);
    let empty = cratonvm_types::PointerMap::default();

    let g1 = VmHeap::new(GcBackend::G1, 16 * 1024 * 1024);
    cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(true);
    assert!(
        !RelocationTargets::for_pause(&g1, &map).claims(B),
        "G1 evacuates into free regions"
    );

    let gen = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(false);
    assert!(
        !RelocationTargets::for_pause(&gen, &map).claims(B),
        "a young-only cycle copies into space that held no object"
    );
    cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(true);
    assert!(
        RelocationTargets::for_pause(&gen, &map).claims(B),
        "an old-gen compaction slides survivors onto dead bases"
    );
    assert!(!RelocationTargets::for_pause(&gen, &empty).claims(B));
}

/// gcd d10/r: a verdict taken with the pause's relocations refuses a live,
/// parseable address that a DIFFERENT survivor was moved onto, and keeps
/// it for an identity pair and for a verdict without relocations.
#[test]
fn gcd_d10r_an_address_a_survivor_was_moved_onto_did_not_survive_in_place() {
    use crate::memory::vm_heap::{GcBackend, VmHeap};
    use cratonvm_types::ClassId;

    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    let live = heap.alloc_object(ClassId::new(7), 1).as_ptr() as usize;
    if heap.is_object_address(live).is_none() {
        // As `survived_in_place_keeps_a_live_object_and_drops_reclaimed_space`:
        // nothing to judge if the heap does not parse its own allocation.
        return;
    }
    assert!(InPlaceVerdict::capture(&heap).survived(live));

    let slid_onto = cratonvm_types::PointerMap::from_iter([(live + 0x100, live)]);
    let screened = InPlaceVerdict {
        heap: &heap,
        probe: std::cell::OnceCell::new(),
        targets: VerdictTargets::Own(RelocationTargets::consulting(Some(&slid_onto))),
    };
    assert!(screened.claimed_by_relocation(live));
    assert!(
        !screened.survived(live),
        "the object that was here died; the one here now arrived by relocation"
    );

    let stationary = cratonvm_types::PointerMap::from_iter([(live, live)]);
    let kept = InPlaceVerdict {
        heap: &heap,
        probe: std::cell::OnceCell::new(),
        targets: VerdictTargets::Own(RelocationTargets::consulting(Some(&stationary))),
    };
    assert!(
        kept.survived(live),
        "an identity pair is a stationary survivor"
    );
}

/// `old_gen_reclaimed_last_cycle` is this thread's; the gci i22/dr tests
/// set it and put it back.
struct I22drRestoreReclaimed(bool);
impl Drop for I22drRestoreReclaimed {
    fn drop(&mut self) {
        cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(self.0);
    }
}

/// gci i22/dr (P2/dr): the pause's ONE destination set, lent to three
/// verdicts with `capture_with`, answers every address exactly as three
/// private sets (`capture_after`, the pre-i22 shape) answer it, and is
/// built once: the first verdict that asks builds it, the others reuse it.
#[test]
fn gci_i22dr_one_shared_relocation_set_answers_as_three_private_ones() {
    use crate::memory::vm_heap::{GcBackend, VmHeap};

    let _restore =
        I22drRestoreReclaimed(cratonvm_gc::gc_quiescence::old_gen_reclaimed_last_cycle());
    cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(true);
    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    // Moved pairs (one onto another's source), an identity pair, a pair
    // to 0 (never a destination).
    let map = cratonvm_types::PointerMap::from_iter([
        (A, B),
        (B, C),
        (C + 0x100, C + 0x200),
        (C + 0x300, C + 0x300),
        (C + 0x400, 0),
    ]);
    let shared = RelocationTargets::for_pause(&heap, &map);
    assert!(shared.is_armed(), "armed on Generational when old-gen was reclaimed");
    assert!(!shared.built(), "lazy: nothing asked yet");
    let lent = [
        InPlaceVerdict::capture_with(&heap, &shared),
        InPlaceVerdict::capture_with(&heap, &shared),
        InPlaceVerdict::capture_with(&heap, &shared),
    ];
    let private = [
        InPlaceVerdict::capture_after(&heap, &map),
        InPlaceVerdict::capture_after(&heap, &map),
        InPlaceVerdict::capture_after(&heap, &map),
    ];
    let probes = [0usize, A, B, C, C + 0x100, C + 0x200, C + 0x300, C + 0x400, C + 0x500];
    for (i, (l, p)) in lent.iter().zip(private.iter()).enumerate() {
        for &addr in &probes {
            assert_eq!(
                l.claimed_by_relocation(addr),
                p.claimed_by_relocation(addr),
                "verdict {i}, address {addr:#x}"
            );
        }
        assert!(shared.built(), "built by the first verdict that asked");
    }
    assert!(lent[0].claimed_by_relocation(B), "A was moved onto B");
    assert!(lent[1].claimed_by_relocation(C + 0x200));
    assert!(!lent[2].claimed_by_relocation(C + 0x300), "an identity pair");
    assert!(!lent[2].claimed_by_relocation(0), "a pair to 0 is no destination");
    assert!(!lent[2].claimed_by_relocation(A), "a source is no destination");
}

/// gci i22/dr: the shared set keeps the d10/r arming rule -- lent from a
/// G1 heap, a young-only Generational cycle or an empty map, it claims
/// nothing and is never built.
#[test]
fn gci_i22dr_a_shared_unarmed_set_claims_nothing_and_builds_nothing() {
    use crate::memory::vm_heap::{GcBackend, VmHeap};

    let _restore =
        I22drRestoreReclaimed(cratonvm_gc::gc_quiescence::old_gen_reclaimed_last_cycle());
    let map = cratonvm_types::PointerMap::from_iter([(A, B)]);
    let empty = cratonvm_types::PointerMap::default();

    let g1 = VmHeap::new(GcBackend::G1, 16 * 1024 * 1024);
    cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(true);
    let t = RelocationTargets::for_pause(&g1, &map);
    assert!(!t.is_armed(), "G1 evacuation does not arm the set");
    assert!(!InPlaceVerdict::capture_with(&g1, &t).claimed_by_relocation(B));
    assert!(!t.built(), "G1 never builds the set");

    let gen = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(false);
    let t = RelocationTargets::for_pause(&gen, &map);
    assert!(!t.is_armed(), "young-only cycle does not arm the set");
    assert!(!InPlaceVerdict::capture_with(&gen, &t).claimed_by_relocation(B));
    assert!(!t.built(), "a young-only cycle never builds the set");

    cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(true);
    let t = RelocationTargets::for_pause(&gen, &empty);
    assert!(!t.is_armed(), "empty map does not arm the set");
    assert!(!InPlaceVerdict::capture_with(&gen, &t).claimed_by_relocation(B));
    assert!(!t.built(), "an empty map never builds the set");
}

/// gci i22/dr: `survived` through a lent set is `survived` through a
/// private one -- a live, parseable address another survivor slid onto
/// is refused by both, an uninvolved live address kept by both.
#[test]
fn gci_i22dr_survived_through_the_shared_set_matches_capture_after() {
    use crate::memory::vm_heap::{GcBackend, VmHeap};
    use cratonvm_types::ClassId;

    let _restore =
        I22drRestoreReclaimed(cratonvm_gc::gc_quiescence::old_gen_reclaimed_last_cycle());
    cratonvm_gc::gc_quiescence::set_old_gen_reclaimed(true);
    let heap = VmHeap::new(GcBackend::Generational, 16 * 1024 * 1024);
    let live = heap.alloc_object(ClassId::new(7), 1).as_ptr() as usize;
    let other = heap.alloc_object(ClassId::new(7), 1).as_ptr() as usize;
    if heap.is_object_address(live).is_none() || heap.is_object_address(other).is_none() {
        // As `gcd_d10r_an_address_a_survivor_was_moved_onto_did_not_survive_in_place`.
        return;
    }
    let map = cratonvm_types::PointerMap::from_iter([(live + 0x1000, live)]);
    let shared = RelocationTargets::for_pause(&heap, &map);
    let lent = InPlaceVerdict::capture_with(&heap, &shared);
    let private = InPlaceVerdict::capture_after(&heap, &map);
    for addr in [live, other] {
        assert_eq!(lent.survived(addr), private.survived(addr), "{addr:#x}");
    }
    assert!(!lent.survived(live), "slid onto: the object that was here died");
    assert!(lent.survived(other), "an uninvolved survivor stays");
}

/// gci i28/wt (`gci-i27gr-g1-kept-young-region-weak-tables-read-region-liveness-FIXED-20261016`):
/// the pause's G1 unheld set rides on its one `RelocationTargets` and every
/// verdict lent that value refuses exactly those addresses; an address
/// outside the set keeps the old answer, a set-less value changes nothing,
/// and the set never arms the relocation screen.
#[test]
fn gci_i28_wt_the_pause_s_g1_unheld_set_is_refused_by_every_lent_verdict() {
    use crate::memory::vm_heap::{GcBackend, VmHeap};
    use cratonvm_types::ClassId;

    let g1 = VmHeap::new(GcBackend::G1, 16 * 1024 * 1024);
    let dead = g1.alloc_object(ClassId::new(7), 1).as_ptr() as usize;
    let live = g1.alloc_object(ClassId::new(7), 1).as_ptr() as usize;
    let map = cratonvm_types::PointerMap::default();

    let plain = RelocationTargets::for_pause(&g1, &map);
    assert!(!plain.g1_unheld(dead), "no verdict handed in");
    assert!(plain.g1_unheld_set().is_empty());
    let before = InPlaceVerdict::capture_with(&g1, &plain);
    let (dead_before, live_before) = (before.survived(dead), before.survived(live));

    // addr-keyed: test-local PRE-collection addresses.
    let unheld: FxHashSet<usize> = [dead].into_iter().collect();
    let targets = RelocationTargets::for_pause(&g1, &map).with_g1_unheld(unheld);
    assert!(targets.g1_unheld(dead));
    assert!(!targets.g1_unheld(live));
    assert_eq!(targets.g1_unheld_set().len(), 1);
    let lent = [
        InPlaceVerdict::capture_with(&g1, &targets),
        InPlaceVerdict::capture_with(&g1, &targets),
    ];
    for v in &lent {
        assert!(!v.survived(dead), "an unheld address never survives in place");
        assert_eq!(v.survived(live), live_before, "outside the set: the old answer");
        assert!(!v.claimed_by_relocation(dead), "the set is not a relocation claim");
    }
    assert!(!targets.built(), "G1 still never builds the destination set");

    // A verdict not over the pause's value never sees the set.
    assert_eq!(InPlaceVerdict::capture(&g1).survived(dead), dead_before);
}

#[test]
fn empty_table_is_a_no_op() {
    let mut t: FxHashMap<ObjectRef, u32> = FxHashMap::default();

    let stats = remap_and_sweep(
        &mut t,
        &cratonvm_types::PointerMap::from_iter([(A, B)]),
        &|_| true,
    );

    assert_eq!(stats, SweepStats::default());
    assert!(t.is_empty());
}
