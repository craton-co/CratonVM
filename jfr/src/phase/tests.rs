use super::*;
use std::time::Duration;

/// The level flag, the registry and the compilation breakdown are all
/// process-wide. Every test here takes this first.
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn spin_ns(ns: u64) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_nanos(ns) {
        std::hint::spin_loop();
    }
}

/// Extract an integer field from a flat JSON object body. Enough to prove
/// the document parses back to the numbers that went in without pulling a
/// serde dependency into this crate for a test.
fn json_u64(doc: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\":");
    let at = doc.find(&needle)? + needle.len();
    let tail = &doc[at..];
    let end = tail
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(tail.len());
    tail[..end].parse().ok()
}

fn json_str<'a>(doc: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\":\"");
    let at = doc.find(&needle)? + needle.len();
    let tail = &doc[at..];
    let end = tail.find('"')?;
    Some(&tail[..end])
}

// --- Category / level vocabulary -------------------------------------

#[test]
fn category_names_and_indices_agree_with_all() {
    for (i, c) in Category::ALL.iter().enumerate() {
        assert_eq!(c.index(), i, "{} has the wrong index", c.name());
        assert_eq!(
            Category::from_name(c.name()),
            Some(*c),
            "{} does not round-trip through from_name",
            c.name()
        );
    }
    let mut names: Vec<&str> = Category::ALL.iter().map(|c| c.name()).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), CATEGORY_COUNT, "duplicate category name");
}

#[test]
fn the_executor_split_is_mutually_exclusive_across_levels() {
    assert!(Category::JavaExecution.active_at(Level::Coarse));
    assert!(!Category::Interpretation.active_at(Level::Coarse));
    assert!(!Category::JitExecution.active_at(Level::Coarse));

    assert!(!Category::JavaExecution.active_at(Level::Fine));
    assert!(Category::Interpretation.active_at(Level::Fine));
    assert!(Category::JitExecution.active_at(Level::Fine));

    for c in Category::ALL {
        assert!(!c.active_at(Level::Off), "{} records when off", c.name());
    }
}

// --- Disabled path ---------------------------------------------------

#[test]
fn disabled_records_nothing() {
    let _g = TEST_LOCK.lock();
    set_level_for_test(Level::Off);
    reset_for_test();

    {
        let span = enter(Category::ClassLoad);
        assert!(!span.is_measuring(), "a disabled span must not measure");
        spin_ns(50_000);
    }
    let report = report();
    assert!(
        report.threads.is_empty(),
        "the disabled path registered a thread account"
    );
    assert_eq!(report.attributed_ns, 0);
    assert_eq!(report.level, Level::Off);
    set_level_for_test(Level::Off);
    reset_for_test();
}

#[test]
fn a_category_inactive_at_this_level_records_nothing() {
    let _g = TEST_LOCK.lock();
    set_level_for_test(Level::Coarse);
    reset_for_test();

    {
        let span = enter(Category::Interpretation);
        assert!(!span.is_measuring());
        spin_ns(50_000);
    }
    let report = report();
    assert_eq!(report.total_ns(Category::Interpretation), 0);

    set_level_for_test(Level::Off);
    reset_for_test();
}

// --- Disjointness ----------------------------------------------------

#[test]
fn nested_phases_charge_disjoint_time() {
    let _g = TEST_LOCK.lock();
    set_level_for_test(Level::Coarse);
    reset_for_test();

    // Register the thread account first: its one-time cost (registry lock,
    // Arc allocation, thread-name copy) would otherwise land between
    // `outer_start` and the first frame's own clock read and read as lost
    // time.
    mark_process_start();
    let outer_start = Instant::now();
    {
        let _outer = enter(Category::JavaExecution);
        spin_ns(200_000);
        {
            let _middle = enter(Category::ClassLoad);
            spin_ns(200_000);
            {
                let _inner = enter(Category::Verification);
                spin_ns(200_000);
            }
            spin_ns(200_000);
        }
        spin_ns(200_000);
    }
    let outer_total = outer_start.elapsed().as_nanos() as u64;

    let report = report();
    let java = report.total_ns(Category::JavaExecution);
    let load = report.total_ns(Category::ClassLoad);
    let verify = report.total_ns(Category::Verification);

    // Each of the three saw its own spins only.
    for (name, ns) in [
        ("java_execution", java),
        ("class_load", load),
        ("verification", verify),
    ] {
        assert!(ns > 0, "{name} recorded nothing");
    }
    // The decisive assertion: the parts sum to at most the whole. Under
    // full double-counting this sum would be ~3x the outer span.
    let sum = java + load + verify;
    assert!(
        sum <= outer_total,
        "nested phases double-counted: parts {sum} ns exceed the outer span's {outer_total} ns",
    );
    // ...and they account for essentially all of it (the only slack is
    // the instrumentation's own clock reads).
    assert!(
        sum * 100 >= outer_total * 90,
        "nested phases lost time: parts {sum} ns cover less than 90% of {outer_total} ns",
    );

    set_level_for_test(Level::Off);
    reset_for_test();
}

#[test]
fn out_of_order_close_is_counted_and_still_charges_once() {
    let _g = TEST_LOCK.lock();
    set_level_for_test(Level::Coarse);
    reset_for_test();

    let outer = enter(Category::JavaExecution);
    spin_ns(100_000);
    let inner = enter(Category::ClassLoad);
    spin_ns(100_000);
    // Deliberately wrong order: close the outer span first.
    drop(outer);
    spin_ns(100_000);
    drop(inner);

    let report = report();
    assert_eq!(
        report.anomalies.out_of_order_closes, 1,
        "an out-of-order close must be reported, not silently accepted"
    );
    // The inner span was unwound by the outer's close, so its later drop
    // charged nothing extra.
    assert_eq!(
        report.entries[Category::ClassLoad.index()].1,
        1,
        "the inner span was charged more than once"
    );
    let thread = report.primary.expect("a thread registered");
    assert_eq!(
        thread.over_attributed_ns, 0,
        "an out-of-order close must not push attribution past the wall clock"
    );

    set_level_for_test(Level::Off);
    reset_for_test();
}

// --- Reconciliation --------------------------------------------------

#[test]
fn categories_plus_unattributed_sum_to_the_measured_wall_time() {
    let _g = TEST_LOCK.lock();
    set_level_for_test(Level::Coarse);
    reset_for_test();

    mark_process_start();
    {
        let _s = enter(Category::VmStartup);
        spin_ns(300_000);
    }
    {
        let _s = enter(Category::JavaExecution);
        spin_ns(300_000);
    }

    let report = report();
    let thread = report.primary.clone().expect("primary thread registered");
    assert_eq!(
        thread.attributed_ns + thread.open_ns + thread.unattributed_ns,
        thread.wall_ns,
        "the reconciliation identity does not hold on the basis thread",
    );
    assert_eq!(
        thread.over_attributed_ns, 0,
        "nothing over-attributed on a well-formed run"
    );
    let summed: u64 = thread.categories.iter().map(|(_, ns)| *ns).sum();
    assert_eq!(summed, thread.attributed_ns);

    set_level_for_test(Level::Off);
    reset_for_test();
}

#[test]
fn a_deliberate_gap_lands_in_unattributed_not_in_a_category() {
    let _g = TEST_LOCK.lock();
    set_level_for_test(Level::Coarse);
    reset_for_test();

    mark_process_start();
    {
        let _s = enter(Category::JavaExecution);
        spin_ns(200_000);
    }
    // Instrumented by nothing on purpose.
    spin_ns(2_000_000);
    {
        let _s = enter(Category::VmShutdown);
        spin_ns(200_000);
    }

    let report = report();
    let thread = report.primary.clone().expect("primary thread registered");
    assert!(
        thread.unattributed_ns >= 1_500_000,
        "the 2 ms gap was absorbed into a category: unattributed is only {} ns",
        thread.unattributed_ns
    );
    assert!(
        !report.reconciles(),
        "a run that is majority-unattributed must not report as reconciled \
         (residual {:.4})",
        report.residual_fraction()
    );
    assert!(report.residual_fraction() > 0.02);

    set_level_for_test(Level::Off);
    reset_for_test();
}

#[test]
fn an_open_span_is_reported_as_open_not_as_unattributed() {
    let _g = TEST_LOCK.lock();
    set_level_for_test(Level::Coarse);
    reset_for_test();

    mark_process_start();
    let held = enter(Category::VmShutdown);
    spin_ns(1_000_000);
    let report = report();
    let thread = report.primary.clone().expect("primary thread registered");
    assert_eq!(thread.open_spans, 1);
    assert!(
        thread.open_ns >= 500_000,
        "an in-flight span's elapsed time must show as open_ns, got {}",
        thread.open_ns
    );
    assert_eq!(
        thread.attributed_ns + thread.open_ns + thread.unattributed_ns,
        thread.wall_ns,
    );
    drop(held);

    set_level_for_test(Level::Off);
    reset_for_test();
}

// --- Concurrency -----------------------------------------------------

#[test]
fn concurrent_threads_neither_lose_nor_double_count() {
    let _g = TEST_LOCK.lock();
    set_level_for_test(Level::Coarse);
    reset_for_test();

    const THREADS: usize = 8;
    const SPANS: usize = 25;

    fn worker() {
        for _ in 0..SPANS {
            let _outer = enter(Category::JavaExecution);
            spin_ns(20_000);
            let _inner = enter(Category::GcPause);
            spin_ns(20_000);
        }
    }

    let mut handles = Vec::new();
    for i in 0..THREADS {
        let name = format!("phase-test-{i}");
        let builder = std::thread::Builder::new().name(name);
        handles.push(builder.spawn(worker).expect("spawn"));
    }
    for h in handles {
        h.join().expect("join");
    }

    let report = report();
    let expected = (THREADS * SPANS) as u64;
    assert_eq!(
        report.entries[Category::JavaExecution.index()].1,
        expected,
        "outer spans lost or duplicated across threads"
    );
    assert_eq!(
        report.entries[Category::GcPause.index()].1,
        expected,
        "inner spans lost or duplicated across threads"
    );
    assert_eq!(
        report.anomalies.out_of_order_closes, 0,
        "correctly-nested concurrent spans reported an ordering anomaly"
    );
    // Every worker thread is its own account, and each one's parts sum to
    // its own wall clock.
    let mut workers = 0usize;
    for t in &report.threads {
        if !t.name.starts_with("phase-test-") {
            continue;
        }
        workers += 1;
        let parts = t.attributed_ns + t.open_ns + t.unattributed_ns;
        assert_eq!(parts, t.wall_ns, "thread {} does not reconcile", t.name);
        assert_eq!(t.over_attributed_ns, 0);
        assert!(
            t.attributed_ns <= t.wall_ns,
            "thread {} attributed {} ns of a {} ns life",
            t.name,
            t.attributed_ns,
            t.wall_ns
        );
    }
    assert_eq!(workers, THREADS);

    set_level_for_test(Level::Off);
    reset_for_test();
}

// --- Reconciliation basis --------------------------------------------

/// The `vm-cli` shape: the thread that can first read the flags is not the
/// thread that runs the VM, and the first one spends the run blocked in
/// `join`. Without [`claim_reconciliation_basis`] the basis stays on the
/// waiter and `reconciles()` is a verdict about a thread that did nothing.
#[test]
fn the_basis_moves_to_the_thread_that_claims_it_last() {
    let _g = TEST_LOCK.lock();
    set_level_for_test(Level::Coarse);
    reset_for_test();

    // The launcher: seeds the epoch, claims the basis, then only waits.
    mark_process_start();
    let launcher_id = with_or_init_state(|s| s.account.thread_id).expect("launcher account");
    assert_eq!(
        registry().primary_thread_id.load(Ordering::Relaxed),
        launcher_id,
        "mark_process_start did not claim the calling thread"
    );

    let builder = std::thread::Builder::new().name("phase-basis-vm".into());
    let worker = builder
        .spawn(|| {
            claim_reconciliation_basis();
            let _p = enter(Category::VmStartup);
            spin_ns(200_000);
        })
        .expect("spawn");
    worker.join().expect("join");

    let report = report();
    let primary = report.primary.as_ref().expect("a basis thread");
    assert_eq!(
        primary.name, "phase-basis-vm",
        "the basis stayed on the launcher thread"
    );
    assert!(
        primary.categories[Category::VmStartup.index()].1 > 0,
        "the basis thread recorded no vm_startup time"
    );
    // The launcher is still tracked — moving the basis must not drop its
    // account, only its verdict-carrying role.
    assert!(
        report.threads.iter().any(|t| t.thread_id == launcher_id),
        "the launcher account disappeared when the basis moved"
    );
    // And the epoch is still the launcher's: the process wall covers at
    // least the worker's whole life.
    assert!(
        report.process_wall_ns >= primary.wall_ns,
        "claim_reconciliation_basis re-seeded the epoch"
    );

    set_level_for_test(Level::Off);
    reset_for_test();
}

/// Same guarantee as [`mark_process_start`]: when collection is off,
/// claiming the basis must not register an account or read the clock.
#[test]
fn claiming_the_basis_while_disabled_registers_nothing() {
    let _g = TEST_LOCK.lock();
    set_level_for_test(Level::Off);
    reset_for_test();

    mark_process_start();
    claim_reconciliation_basis();

    let report = report();
    assert!(
        report.threads.is_empty(),
        "the disabled path registered a thread account"
    );
    assert_eq!(registry().primary_thread_id.load(Ordering::Relaxed), 0);

    reset_for_test();
}

/// The `vm-cli` span nesting, end to end: `vm_startup` then
/// `java_execution` then `vm_shutdown`, on one thread, charging disjoint
/// time that sums to that thread's wall clock.
#[test]
fn the_launcher_span_sequence_charges_disjoint_time() {
    let _g = TEST_LOCK.lock();
    set_level_for_test(Level::Coarse);
    reset_for_test();

    let builder = std::thread::Builder::new().name("phase-launcher-seq".into());
    let worker = builder
        .spawn(|| {
            claim_reconciliation_basis();
            let startup = enter(Category::VmStartup);
            spin_ns(300_000);
            // A class load nests inside startup and subtracts from it.
            {
                let _load = enter(Category::ClassLoad);
                spin_ns(300_000);
            }
            startup.end();

            let exec = enter(Category::JavaExecution);
            spin_ns(300_000);
            exec.end();

            let _shutdown = enter(Category::VmShutdown);
            spin_ns(300_000);
        })
        .expect("spawn");
    worker.join().expect("join");

    let report = report();
    let primary = report.primary.as_ref().expect("a basis thread");
    let cat = |c: Category| primary.categories[c.index()].1;

    let charged = [
        Category::VmStartup,
        Category::ClassLoad,
        Category::JavaExecution,
        Category::VmShutdown,
    ];
    for c in charged {
        assert!(
            cat(c) > 0,
            "{} charged nothing in the launcher sequence",
            c.name()
        );
    }
    // Disjoint, not overlapping: these four are the only spans opened, so
    // they account for all of the thread's attributed time exactly. A
    // nested `class_load` that was *added* to `vm_startup` rather than
    // carved out of it would push this sum past `attributed_ns`.
    let sum: u64 = charged.iter().map(|c| cat(*c)).sum();
    assert_eq!(
        sum, primary.attributed_ns,
        "the four launcher categories do not partition the attributed time"
    );
    assert_eq!(primary.over_attributed_ns, 0, "over-attribution");
    assert_eq!(primary.out_of_order_closes, 0, "an out-of-order close");
    assert_eq!(
        primary.attributed_ns + primary.open_ns + primary.unattributed_ns,
        primary.wall_ns,
        "the launcher sequence does not reconcile"
    );

    set_level_for_test(Level::Off);
    reset_for_test();
}

// --- Serialization ---------------------------------------------------

#[test]
fn json_round_trips_the_numbers_and_the_shape() {
    let _g = TEST_LOCK.lock();
    set_level_for_test(Level::Fine);
    reset_for_test();

    mark_process_start();
    {
        let _s = enter(Category::Interpretation);
        spin_ns(200_000);
    }
    set_compilation_breakdown(&[("scan", 11), ("build", 22), ("lower", 33)]);

    let report = report();
    let doc = report.to_json();

    let want_schema = PHASE_ACCOUNTING_SCHEMA_VERSION as u64;
    assert_eq!(json_u64(&doc, "schema_version"), Some(want_schema));
    assert_eq!(json_str(&doc, "level"), Some("fine"));
    let want_wall = report.process_wall_ns;
    assert_eq!(json_u64(&doc, "process_wall_ns"), Some(want_wall));
    assert_eq!(json_u64(&doc, "attributed_ns"), Some(report.attributed_ns));
    let want_unattributed = report.unattributed_ns;
    assert_eq!(json_u64(&doc, "unattributed_ns"), Some(want_unattributed));
    let want_thread_wall = report.thread_wall_ns;
    assert_eq!(json_u64(&doc, "thread_wall_ns"), Some(want_thread_wall));
    // Every category key is present even at zero, so two runs diff cleanly.
    for c in Category::ALL {
        assert!(
            doc.contains(&format!("\"{}\":", c.name())),
            "category {} missing from the JSON",
            c.name()
        );
    }
    // The delegated breakdown survives verbatim.
    assert!(doc.contains("\"scan\":11"));
    assert!(doc.contains("\"build\":22"));
    assert!(doc.contains("\"lower\":33"));
    let compiled_ns = report.total_ns(Category::Compilation) as i128;
    assert_eq!(report.compilation_delta_ns, 66 - compiled_ns);
    // Balanced braces: a hand-rolled encoder's most likely failure.
    assert_eq!(
        doc.chars().filter(|c| *c == '{').count(),
        doc.chars().filter(|c| *c == '}').count(),
        "unbalanced braces in the JSON report"
    );

    set_level_for_test(Level::Off);
    reset_for_test();
}

#[test]
fn the_summary_line_is_integer_only_so_the_gate_can_scrape_it() {
    let _g = TEST_LOCK.lock();
    set_level_for_test(Level::Coarse);
    reset_for_test();

    mark_process_start();
    {
        let _s = enter(Category::GcPause);
        spin_ns(100_000);
    }
    let line = report().summary_line();
    assert!(line.starts_with("[PHASE-ACCOUNTING] "));
    for token in line.split_whitespace().skip(1) {
        let Some((key, value)) = token.split_once('=') else {
            panic!("summary token {token} is not key=value");
        };
        assert!(!key.is_empty());
        if key == "level" {
            continue;
        }
        assert!(
            value.chars().all(|c| c.is_ascii_digit()),
            "summary value for {key} is not an integer: {value}"
        );
    }
    for c in Category::ALL {
        assert!(
            line.contains(&format!(" {}_ns=", c.name())),
            "summary line omits {}",
            c.name()
        );
    }

    set_level_for_test(Level::Off);
    reset_for_test();
}

#[test]
fn thread_names_with_quotes_do_not_break_the_json() {
    let _g = TEST_LOCK.lock();
    set_level_for_test(Level::Coarse);
    reset_for_test();

    let name = "we\"ird\n\tname".to_string();
    let builder = std::thread::Builder::new().name(name);
    let handle = builder.spawn(|| {
        let _s = enter(Category::NativeCall);
        spin_ns(10_000);
    });
    handle.expect("spawn").join().expect("join");

    let doc = report().to_json();
    assert!(
        doc.contains("we\\\"ird\\n\\tname"),
        "thread name not escaped"
    );
    assert!(!doc.contains("we\"ird"), "raw quote leaked into the JSON");

    set_level_for_test(Level::Off);
    reset_for_test();
}

// --- JFR sink --------------------------------------------------------

#[test]
fn the_jfr_sink_writes_a_readable_chunk() {
    let _g = TEST_LOCK.lock();
    set_level_for_test(Level::Coarse);
    reset_for_test();

    mark_process_start();
    {
        let _s = enter(Category::Compilation);
        spin_ns(150_000);
    }
    {
        let _s = enter(Category::GcPause);
        spin_ns(150_000);
    }

    let report = report();
    let name = format!("cratonvm-phase-accounting-{}.jfr", std::process::id());
    let path = std::env::temp_dir().join(name);
    let bytes = write_jfr_report(&path, &report).expect("jfr write");
    assert!(bytes > 0);

    let chunk = crate::jdk_chunk::read_chunk(&path).expect("chunk");
    let mut registry = EventTypeRegistry::new();
    register_phase_events(&mut registry);
    let events = &chunk.events;
    assert!(
        !events.is_empty(),
        "the phase report produced no readable events"
    );
    for e in events {
        assert!(
            registry.find_by_name(&e.type_name).is_some(),
            "event of unregistered type {} survived the round trip",
            e.type_name
        );
    }
    assert_eq!(
        events
            .iter()
            .filter(|e| e.type_name == PHASE_SUMMARY_EVENT)
            .count(),
        1,
        "expected exactly one summary event"
    );

    let _ = std::fs::remove_file(&path);
    set_level_for_test(Level::Off);
    reset_for_test();
}

#[test]
fn the_json_sink_writes_a_whole_document() {
    let _g = TEST_LOCK.lock();
    set_level_for_test(Level::Coarse);
    reset_for_test();

    mark_process_start();
    {
        let _s = enter(Category::ClassLoad);
        spin_ns(100_000);
    }
    let report = report();
    let name = format!("cratonvm-phase-accounting-{}.json", std::process::id());
    let path = std::env::temp_dir().join(name);
    write_json_report(&path, &report).expect("json write");
    let body = std::fs::read_to_string(&path).expect("read back");
    assert!(body.trim_end().starts_with('{'));
    assert!(body.trim_end().ends_with('}'));
    assert_eq!(json_u64(&body, "attributed_ns"), Some(report.attributed_ns));

    let _ = std::fs::remove_file(&path);
    set_level_for_test(Level::Off);
    reset_for_test();
}

// --- Delegation ------------------------------------------------------

#[test]
fn the_compilation_breakdown_is_replaced_not_accumulated() {
    let _g = TEST_LOCK.lock();
    set_level_for_test(Level::Coarse);
    reset_for_test();

    set_compilation_breakdown(&[("scan", 5)]);
    set_compilation_breakdown(&[("scan", 5), ("build", 7)]);
    let breakdown = compilation_breakdown();
    assert_eq!(breakdown.len(), 2);
    assert_eq!(breakdown[0], ("scan".to_string(), 5));
    assert_eq!(breakdown[1], ("build".to_string(), 7));

    set_level_for_test(Level::Off);
    reset_for_test();
}
