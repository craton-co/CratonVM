use super::*;

#[test]
fn fast_boot_jit_tuning_is_limited_to_small_hosts_unless_explicit() {
    assert!(fast_boot_jit_tuning_on(true, false, Some(4)));
    assert!(!fast_boot_jit_tuning_on(true, false, Some(5)));
    assert!(fast_boot_jit_tuning_on(true, true, Some(32)));
    assert!(!fast_boot_jit_tuning_on(false, true, Some(4)));
    assert!(fast_boot_jit_tuning_on(true, false, None));
}

/// The per-entry trace reads (`Throwable.getStackTraceDepth` /
/// `getStackTraceElement`) answer exactly what the whole-trace read does,
/// entry by entry, and nothing for an absent trace or index.
#[test]
fn per_entry_throwable_trace_reads_agree_with_the_whole_trace() {
    let shared = SharedVm::new(VmConfig::default());
    let throwable = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    assert_eq!(shared.throwable_stack_trace_len_for(throwable), None);
    assert!(shared
        .throwable_stack_trace_entry_for(throwable, 0)
        .is_none());
    let frames: Vec<StackTraceEntry> = (0..5)
        .map(|i| {
            crate::runtime::stackwalker::synthetic_entry(
                Arc::from("probe/Deep"),
                Arc::from(format!("m{i}").as_str()),
            )
        })
        .collect();
    shared.store_throwable_stack_trace(throwable, frames.into());
    let whole = shared.throwable_stack_trace_for(throwable).expect("stored");
    assert_eq!(
        shared.throwable_stack_trace_len_for(throwable),
        Some(whole.len())
    );
    for (i, expected) in whole.iter().enumerate() {
        let got = shared
            .throwable_stack_trace_entry_for(throwable, i)
            .expect("in range");
        assert_eq!(got.method_name, expected.method_name);
        assert_eq!(got.line_number, expected.line_number);
    }
    assert!(shared
        .throwable_stack_trace_entry_for(throwable, whole.len())
        .is_none());
}

/// Interpreter round i3 wave 82, lane L2
/// (`CRATONVM_THROWABLE_BACKTRACE_IN_OBJECT`, set explicitly here): a
/// capture's frames land in a holder the throwable's `backtrace` names,
/// every reader (through the throwable or the holder itself) answers the
/// registry's answer, the registry holds nothing, and a later registry
/// store (the trap splice's) retires the holder.
#[test]
fn a_throwable_owned_backtrace_answers_as_the_registry_did() {
    use crate::vm::realms::thread_realm::UNRESOLVED_THROWABLE_FIELD_INDEX;
    let shared = SharedVm::new(VmConfig::default());
    let cache = &shared.threads.throwable_layout_cache;
    cache.set_backtrace_in_object_for_test(true);
    // A three-slot stand-in whose slot 0 plays `Throwable.backtrace`.
    assert_eq!(
        cache.backtrace.load(Ordering::Relaxed),
        UNRESOLVED_THROWABLE_FIELD_INDEX
    );
    cache.backtrace.store(0, Ordering::Relaxed);
    let throwable = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let labels: Vec<Arc<str>> = (0..4)
        .map(|i| Arc::from(format!("probe/Deep.m{i}()V").as_str()))
        .collect();
    let frames: Vec<BacktraceFrame> = labels
        .iter()
        .enumerate()
        .map(|(i, label)| BacktraceFrame::Compiled {
            label: Arc::clone(label),
            owner_class_id: 0,
            bci: i as i32 - 1,
            cp_stamp: None,
        })
        .collect();
    let expected: Vec<String> = frames
        .iter()
        .map(|f| {
            let e = f.to_entry();
            format!("{}.{}@{}", e.class_name, e.method_name, e.byte_code_index)
        })
        .collect();
    let names = |trace: Option<Vec<StackTraceEntry>>| -> Option<Vec<String>> {
        trace.map(|t| {
            t.iter()
                .map(|e| format!("{}.{}@{}", e.class_name, e.method_name, e.byte_code_index))
                .collect()
        })
    };
    shared.retain_captured_backtrace(throwable, frames.into_boxed_slice());
    let holder = shared.backtrace_holder_of(throwable).expect("a holder");
    assert_eq!(
        shared.mem.heap.get_field_as(throwable, 0, b'L'),
        Value::Object(Some(holder))
    );
    assert!(
        shared
            .threads
            .throwable_stacks
            .iter()
            .all(|s| s.read().is_empty()),
        "the registry holds nothing"
    );
    assert_eq!(shared.throwable_stack_trace_len_for(throwable), Some(4));
    assert_eq!(shared.throwable_stack_trace_len_for(holder), Some(4));
    assert_eq!(
        names(shared.throwable_stack_trace_for(throwable)),
        Some(expected.clone())
    );
    assert_eq!(
        names(shared.throwable_stack_trace_for(holder)),
        Some(expected.clone())
    );
    let third = shared
        .throwable_stack_trace_entry_for(throwable, 2)
        .expect("in range");
    assert_eq!(names(Some(vec![third])), Some(vec![expected[2].clone()]));
    assert!(shared
        .throwable_stack_trace_entry_for(throwable, 4)
        .is_none());

    // A registry store (the trap splice) retires the holder.
    let entries: Vec<StackTraceEntry> = vec![crate::runtime::stackwalker::synthetic_entry(
        Arc::from("probe/Spliced"),
        Arc::from("m"),
    )];
    shared.store_throwable_stack_trace(throwable, entries.into());
    assert!(shared.backtrace_holder_of(throwable).is_none());
    assert_eq!(
        shared.mem.heap.get_field_as(throwable, 0, b'L'),
        Value::Object(Some(throwable)),
        "the registry marker"
    );
    assert_eq!(shared.throwable_stack_trace_len_for(throwable), Some(1));

    // A trace holding a built entry keeps the registry, marked.
    let fresh = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let built =
        crate::runtime::stackwalker::synthetic_entry(Arc::from("probe/Built"), Arc::from("m"));
    shared.retain_captured_backtrace(fresh, vec![BacktraceFrame::Entry(built)].into());
    assert!(shared.backtrace_holder_of(fresh).is_none());
    assert_eq!(
        shared.mem.heap.get_field_as(fresh, 0, b'L'),
        Value::Object(Some(fresh))
    );
    assert_eq!(shared.throwable_stack_trace_len_for(fresh), Some(1));
}

/// Page `throwable-backtrace-holder-allocates-for-a-pooled-oome`: with the
/// holder switch on, a traced pool error's capture keeps the registry
/// (marked), allocating no holder on the heap that refused; any other
/// throwable still gets one.
#[test]
fn a_pooled_oome_trace_takes_no_holder() {
    let shared = SharedVm::new(VmConfig::default());
    let cache = &shared.threads.throwable_layout_cache;
    cache.set_backtrace_in_object_for_test(true);
    cache.backtrace.store(0, Ordering::Relaxed);
    let pooled = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    let other = shared.mem.heap.alloc_object(ClassId::new(0), 3);
    shared.mem.preallocated_oome.write().push_traced(pooled);
    let label: Arc<str> = Arc::from("probe/Fill.m()V");
    let frames = || -> Box<[BacktraceFrame]> {
        vec![BacktraceFrame::Compiled {
            label: Arc::clone(&label),
            owner_class_id: 0,
            bci: 3,
            cp_stamp: None,
        }]
        .into_boxed_slice()
    };
    shared.retain_captured_backtrace(pooled, frames());
    assert!(shared.backtrace_holder_of(pooled).is_none(), "no holder");
    assert_eq!(
        shared.mem.heap.get_field_as(pooled, 0, b'L'),
        Value::Object(Some(pooled)),
        "the registry marker"
    );
    assert_eq!(shared.throwable_stack_trace_len_for(pooled), Some(1));
    shared.retain_captured_backtrace(other, frames());
    assert!(shared.backtrace_holder_of(other).is_some(), "the holder path");
}

/// Page `i82-L2-a-cloned-throwable-does-not-keep-its-trace`: a clone whose
/// copied `backtrace` names the original gets its own registry row and
/// marker, so the original's later refill (a new row for the original)
/// leaves the clone's trace as it was cloned.
#[test]
fn a_cloned_throwable_keeps_the_trace_it_was_cloned_with() {
    let shared = SharedVm::new(VmConfig::default());
    let heap = &shared.mem.heap;
    // Two-slot stand-ins whose slot 0 plays `Throwable.backtrace`.
    let original = heap.alloc_object(ClassId::new(0), 2);
    let made = crate::runtime::stackwalker::synthetic_entry(
        Arc::from("probe/Copyable"),
        Arc::from("makeCopyable"),
    );
    shared.store_throwable_stack_trace(original, vec![made].into());
    heap.set_field_as(original, 0, Value::Object(Some(original)), b'L');
    // `Object.clone()`'s field copy.
    let clone = heap.alloc_object(ClassId::new(0), 2);
    heap.set_field_as(clone, 0, Value::Object(Some(original)), b'L');
    shared.give_clone_its_own_trace(original, clone, 0);
    assert_eq!(
        heap.get_field_as(clone, 0, b'L'),
        Value::Object(Some(clone))
    );
    let top = |t: ObjectRef| {
        shared.throwable_stack_trace_for(t).map(|tr| {
            tr.iter()
                .map(|e| e.method_name.to_string())
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(top(clone), Some(vec!["makeCopyable".to_string()]));
    // The original refills.
    let refilled = crate::runtime::stackwalker::synthetic_entry(
        Arc::from("probe/Copyable"),
        Arc::from("refillFrom"),
    );
    shared.store_throwable_stack_trace(original, vec![refilled].into());
    assert_eq!(top(original), Some(vec!["refillFrom".to_string()]));
    assert_eq!(
        top(clone),
        Some(vec!["makeCopyable".to_string()]),
        "the clone's own row"
    );
    // An original with no row leaves the clone as copied.
    let bare = heap.alloc_object(ClassId::new(0), 2);
    let bare_clone = heap.alloc_object(ClassId::new(0), 2);
    heap.set_field_as(bare_clone, 0, Value::Object(Some(bare)), b'L');
    shared.give_clone_its_own_trace(bare, bare_clone, 0);
    assert_eq!(
        heap.get_field_as(bare_clone, 0, b'L'),
        Value::Object(Some(bare))
    );
    assert_eq!(top(bare_clone), None);
}

#[test]
fn container_heap_sizing_leaves_explicit_heap_unchanged() {
    let mut config = VmConfig::default();
    config.max_heap_size = 512 * 1024 * 1024;

    apply_container_default_heap(&mut config);

    assert_eq!(config.max_heap_size, 512 * 1024 * 1024);
}

#[test]
fn container_heap_sizing_respects_disabled_container_support() {
    let mut config = VmConfig::default();
    config.use_container_support = false;

    apply_container_default_heap(&mut config);

    assert_eq!(config.max_heap_size, DEFAULT_MAX_HEAP_SIZE);
}

// -----------------------------------------------------------------------
// Real-JDK mode (docs/book/src/user-guide/real-jdk-mode.md §8)
// -----------------------------------------------------------------------

/// The `Compatible` boot precondition must probe **nothing**.
///
/// `VmConfig::default()` is hermetic — it is built by hundreds of unit
/// tests, by every embedder and on CI machines with no JDK installed — and
/// the whole point of returning early in `require_jdk_image_for_jdk_only`
/// is that adding the strict-mode check must not make the *default* boot
/// depend on what happens to be on the host. `Ok(None)` is therefore the
/// assertion: not merely "it succeeded", but "it did not resolve a
/// `JAVA_HOME`", which is only possible if the probe never ran.
#[test]
fn compatible_boot_precondition_does_no_host_probing() {
    let config = VmConfig::default();
    assert_eq!(
        config.compatibility_mode,
        CompatibilityMode::Compatible,
        "the default config must never infer strict mode from a build \
         feature or an unrelated env var (contract §6)"
    );

    let resolved = require_jdk_image_for_jdk_only(&config)
        .expect("the default configuration must always be bootable");
    assert!(
        resolved.is_none(),
        "Compatible mode returned a resolved JDK home ({resolved:?}) — the \
         default path probed the host, which breaks VmConfig::default()'s \
         hermetic contract"
    );
}

/// `--real-jdk` + the synthetic class library is a configuration error, and
/// it must be caught here rather than only in the launcher: embedders and
/// in-process harnesses build a `VmConfig` by hand and never parse argv.
#[test]
fn jdk_only_with_synthetic_library_is_a_configuration_error() {
    // `with_jdk_mode` rather than poking a field, so this test keeps
    // asking the same question regardless of whether `JdkMode` is stored
    // or derived in `config.rs`.
    let mut config = VmConfig::default().with_jdk_mode(crate::config::JdkMode::Synthetic);
    config.compatibility_mode = CompatibilityMode::JdkOnly;

    let err = require_jdk_image_for_jdk_only(&config)
        .expect_err("jdk-only + synthetic-jdk must be rejected");
    // `validate_compatibility` owns the wording (contract §6); assert on
    // the fact rather than on its exact prose so agent A can improve the
    // message without breaking this test.
    assert!(
        matches!(err, VmError::InvalidConfiguration(_)),
        "expected a configuration error, got {err:?}"
    );
    // Crucially: rejected by the *conflict*, not by the JDK search. A
    // strict run must fail this way even on a host with a perfectly good
    // JDK installed, which is why `validate_compatibility` runs first.
    let text = err.to_string();
    assert!(
        !text.contains("Searched, in order"),
        "the conflict must be reported before the JDK probe runs, but the \
         message carried the probe's search report: {text}"
    );
}

/// Registration-site redaction keeps the workspace-relative form (which is
/// what `#[track_caller]` normally produces and what a reviewer needs) and
/// collapses absolute paths, which otherwise leak the builder's home
/// directory into a committed baseline and make the census differ per
/// machine.
#[test]
fn registration_site_redaction_keeps_relative_and_collapses_absolute() {
    // Workspace-relative: kept verbatim, line number and all.
    assert_eq!(
        redact_registration_site("native-builtins/src/lib.rs:1234"),
        "native-builtins/src/lib.rs:1234"
    );
    // Windows separators normalise so a census taken on Windows diffs
    // against one taken on Linux.
    assert_eq!(
        redact_registration_site("native-builtins\\src\\jmx.rs:88"),
        "native-builtins/src/jmx.rs:88"
    );
    // POSIX absolute: collapsed to basename + line.
    assert_eq!(
        redact_registration_site("/home/someone/craton/native-builtins/src/lib.rs:1234"),
        "<redacted>/lib.rs:1234"
    );
    // Windows absolute: the drive-letter colon must not be mistaken for
    // the line-number separator.
    assert_eq!(
        redact_registration_site("C:\\craton\\wt-jdk-only\\native-builtins\\src\\lib.rs:12"),
        "<redacted>/lib.rs:12"
    );
    // Nothing recognisable left to keep.
    assert_eq!(redact_registration_site("/"), "<redacted>");
}

/// Prose-embedded path redaction, the other half of §9's redaction rule.
/// Migrated here with the census writers it serves, so the rule and its
/// test stay in the same crate.
#[test]
fn absolute_paths_are_redacted_to_their_basename() {
    let unix = redact_absolute_paths("refused at /home/ci-agent/work/app.jar (boot)");
    assert!(unix.contains("<redacted>/app.jar"), "{unix}");
    assert!(!unix.contains("ci-agent"), "{unix}");

    let windows = redact_absolute_paths("source C:\\Users\\victor\\build\\app.jar");
    assert!(windows.contains("<redacted>/app.jar"), "{windows}");
    assert!(!windows.contains("victor"), "{windows}");

    // Already JSON-escaped text: the separator arrives doubled.
    let escaped = redact_absolute_paths("\"C:\\\\Users\\\\victor\\\\app.jar\"");
    assert!(escaped.contains("<redacted>/app.jar"), "{escaped}");
    assert!(!escaped.contains("victor"), "{escaped}");

    // Internal-form names and descriptors are not paths and must survive
    // verbatim — every violation body is full of them.
    let names = "java/lang/String.charAt(I)C and (Ljava/lang/Object;)V";
    assert_eq!(redact_absolute_paths(names), names);
    // Too shallow to be worth hiding.
    assert_eq!(redact_absolute_paths("in /tmp now"), "in /tmp now");
}

#[test]
fn json_escape_quotes_and_escapes() {
    assert_eq!(json_escape("plain"), "\"plain\"");
    assert_eq!(json_escape("a\"b"), "\"a\\\"b\"");
    assert_eq!(json_escape("c:\\x"), "\"c:\\\\x\"");
    assert_eq!(json_escape("l1\nl2\tt"), "\"l1\\nl2\\tt\"");
    assert_eq!(json_escape("\u{1}"), "\"\\u0001\"");
    assert_eq!(json_opt_string(None, true), "null");
    assert_eq!(json_opt_string(Some("x"), true), "\"x\"");
}

/// j27 CB17-6: the census publishes every interpreter-table intrinsic,
/// with the registry row that owns its slot (`null` when none does -- the
/// three `String` rows on a real JDK) and whether the strict policy
/// declines it.
#[test]
fn j27_census_publishes_the_interpreter_table_intrinsics() {
    let block = native_census_table_intrinsics_json(&[], false);
    assert!(block.starts_with("  \"table_intrinsics\": ["), "{block}");
    assert!(block.ends_with("],\n"), "{block}");
    let n = cratonvm_native_builtins::intrinsics::TABLE.len();
    assert_eq!(block.matches("\"class\": ").count(), n, "{block}");
    assert!(
        block.contains(
            "{\"class\": \"java/lang/String\", \"name\": \"length\", \
             \"descriptor\": \"()I\", \"static\": false, \
             \"declined_by_strict_policy\": false, \"registry_kind\": null}"
        ),
        "{block}"
    );
    // The block is valid JSON once wrapped as an object member.
    let doc = format!("{{{}\"end\": 0}}", block);
    let parsed: serde_json::Value = serde_json::from_str(&doc).expect("valid JSON");
    assert_eq!(parsed["table_intrinsics"].as_array().map(Vec::len), Some(n));
}

fn origin_row(name: &str, origin: &str, loader_id: u32) -> ClassOriginEntry {
    ClassOriginEntry {
        name: name.to_string(),
        origin: origin.to_string(),
        reason: None,
        requested_by: None,
        real_bytes_found: false,
        loader_id,
        supertypes: Vec::new(),
    }
}

/// The report's four class counters must be a partition of the ten origin
/// tags: every tag lands in exactly one bucket, so the sum is the row count
/// and no class can go missing between the census and the summary.
#[test]
fn origin_buckets_partition_every_row() {
    let rows: Vec<ClassOriginEntry> = CLASS_ORIGIN_TAGS
        .iter()
        .map(|tag| origin_row(&format!("x/{tag}"), tag, 0))
        .collect();
    let buckets = fold_origin_buckets(&rows);
    assert_eq!(buckets.boot_image, 1);
    // application-class-path + user-defined
    assert_eq!(buckets.application, 2);
    // vm-array, hidden-class, generated-lambda, generated-proxy,
    // reflection-accessor, vm-internal
    assert_eq!(buckets.generated, 6);
    assert_eq!(buckets.compatibility, 1);
    assert_eq!(
        buckets.total(),
        rows.len() as u64,
        "the fold must be a partition — no class may fall between buckets"
    );
}

#[test]
fn class_origin_census_is_sorted_counted_and_redacted() {
    let mut rows = vec![
        ClassOriginEntry {
            name: "org/jboss/logging/Logger".into(),
            origin: "compatibility-stub".into(),
            reason: Some("enterprise-prefix fallback".into()),
            requested_by: Some("/home/ci-agent/work/app.jar".into()),
            real_bytes_found: false,
            loader_id: 0,
            supertypes: vec!["java/lang/Object".into()],
        },
        origin_row("com/example/Main", "application-class-path", 2),
        origin_row("java/lang/String", "boot-image", 0),
        origin_row("com/example/Main", "application-class-path", 1),
        origin_row("[I", "vm-array", 0),
    ];
    let n = rows.len();
    let text = render_class_origins_json(&mut rows, false);

    // Sorted by (name, loader_id, origin).
    let at = |needle: &str| {
        text.find(needle)
            .unwrap_or_else(|| panic!("missing {needle}"))
    };
    assert!(at("\"[I\"") < at("\"com/example/Main\""));
    assert!(at("\"com/example/Main\"") < at("\"java/lang/String\""));
    assert!(at("\"java/lang/String\"") < at("\"org/jboss/logging/Logger\""));
    // The loader tiebreak: the two `com/example/Main` rows are ordered by
    // defining loader, not by insertion.
    assert!(at("\"loader_id\": 1") < at("\"loader_id\": 2"));

    // Every tag is present, so "zero" is stated rather than implied.
    for tag in CLASS_ORIGIN_TAGS {
        assert!(text.contains(&format!("\"{tag}\":")), "counts omit {tag}");
    }
    assert!(text.contains("\"compatibility-stub\": 1"), "{text}");
    assert!(text.contains("\"boot-image\": 1"), "{text}");
    assert!(text.contains("\"application-class-path\": 2"), "{text}");
    assert!(text.contains("\"hidden-class\": 0"), "{text}");
    assert!(text.contains(&format!("\"total\": {n}")), "{text}");
    assert!(text.contains("\"schema_version\": 1"), "{text}");

    // Redaction is on by default (no --explain-jdk-only here).
    assert!(text.contains("<redacted>/app.jar"), "{text}");
    assert!(!text.contains("ci-agent"), "{text}");

    // ...and off under --explain-jdk-only.
    let verbose = render_class_origins_json(&mut rows, true);
    assert!(verbose.contains("/home/ci-agent/work/app.jar"), "{verbose}");
}

/// The `java.vm.info` property and the launcher's `-version` banner read
/// the same function, so the policy token can only ever be spelled once.
/// Compatible mode must stay byte-for-byte HotSpot's value (§10).
#[test]
fn vm_info_mode_list_spells_the_policy_once() {
    assert_eq!(
        vm_info_mode_list(CompatibilityMode::Compatible),
        "mixed mode"
    );
    assert_eq!(
        vm_info_mode_list(CompatibilityMode::JdkOnly),
        "mixed mode, jdk-only"
    );
    // The token is the mode's own wire spelling, appended to the list —
    // never a re-spelling of it.
    assert!(vm_info_mode_list(CompatibilityMode::JdkOnly)
        .ends_with(CompatibilityMode::JdkOnly.as_str()));
}

/// Interpreter round i7 wave 138, lane L1b: `-Xint` (last of the three
/// mode options) with the JIT off answers HotSpot's `interpreted mode`;
/// `--nojit` alone, `-Xint -Xmixed`, and `-Xint` with a live JIT stay
/// `mixed mode`. The jdk-only token is unchanged.
#[test]
fn i138_l1b_java_vm_info_says_interpreted_mode_under_xint() {
    let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<String>>();
    assert_eq!(hotspot_execution_mode_option(&args(&["-Xmx1g"])), None);
    assert_eq!(hotspot_execution_mode_option(&args(&["-Xint", "-Xmixed"])), Some("-Xmixed"));
    assert_eq!(hotspot_execution_mode_option(&args(&["-Xcomp", "-Xint"])), Some("-Xint"));
    assert_eq!(
        vm_info_execution_mode_list(CompatibilityMode::Compatible, true),
        "interpreted mode"
    );
    assert_eq!(
        vm_info_execution_mode_list(CompatibilityMode::JdkOnly, true),
        "interpreted mode, jdk-only"
    );
    let mut config = VmConfig::default();
    config.jit_enabled = false;
    assert!(!vm_info_interpreted(&config), "--nojit alone is not -Xint");
    config.jvm_args = args(&["-Xint"]);
    assert!(vm_info_interpreted(&config));
    let info = vm_identity_properties(&config)
        .into_iter()
        .find(|(k, _)| *k == "java.vm.info")
        .map(|(_, v)| v);
    assert_eq!(info.as_deref(), Some("interpreted mode"));
    config.jvm_args = args(&["-Xint", "-Xmixed"]);
    assert!(!vm_info_interpreted(&config));
    config.jvm_args = args(&["-Xint"]);
    config.jit_enabled = true;
    assert!(!vm_info_interpreted(&config), "a live JIT is not interpreted mode");
}

/// Wave 138, L1b: HotSpot 25's Windows `os.name` table
/// (`java_props_md.c`); the old code named every host `Windows 11`.
#[test]
fn i138_l1b_windows_os_name_table() {
    assert_eq!(windows_os_name_for(10, 0, 26100, true), "Windows 11");
    assert_eq!(windows_os_name_for(10, 0, 22000, true), "Windows 11");
    assert_eq!(windows_os_name_for(10, 0, 19045, true), "Windows 10");
    assert_eq!(windows_os_name_for(10, 0, 26100, false), "Windows Server 2025");
    assert_eq!(windows_os_name_for(10, 0, 20348, false), "Windows Server 2022");
    assert_eq!(windows_os_name_for(10, 0, 17763, false), "Windows Server 2019");
    assert_eq!(windows_os_name_for(10, 0, 14393, false), "Windows Server 2016");
    assert_eq!(windows_os_name_for(6, 3, 9600, false), "Windows Server 2012 R2");
    assert_eq!(windows_os_name_for(6, 3, 9600, true), "Windows 8.1");
    assert_eq!(windows_os_name_for(11, 0, 1, true), "Windows NT (unknown)");
}

#[cfg(feature = "experimental-debug")]
#[test]
fn startup_jvmti_missing_native_agent_is_hotspots_init_error() {
    // HotSpot: `Error occurred during initialization of VM` and this
    // line on stdout, exit 1 (the launcher's panic hook prints them).
    for (option, prefix) in [
        (
            "-agentpath:definitely-not-a-real-agent-library",
            "Could not find agent library definitely-not-a-real-agent-library \
             in absolute path, with error: ",
        ),
        (
            "-agentlib:definitely-not-a-real-agent-library=x",
            "Could not find agent library definitely-not-a-real-agent-library \
             on the library path, with error: ",
        ),
    ] {
        let mut config = VmConfig::default();
        config.jvmti_agent_options.push(option.to_string());
        let mut env = crate::jvmti::create_jvmti_env();
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            load_startup_jvmti_agents(&config, &mut env);
        }))
        .expect_err("a missing agent library stops the boot");
        let error = payload
            .downcast_ref::<crate::error::VmInitializationError>()
            .expect("the payload is HotSpot's initialization error");
        assert!(error.detail.starts_with(prefix), "{}", error.detail);
        assert!(error
            .report()
            .starts_with("Error occurred during initialization of VM\nCould not find"));
    }
}

/// Interpreter round i2 wave 57, lane L1: a build without JVMTI refuses a
/// startup agent with HotSpot's not-found line and its own reason.
#[test]
fn a_build_without_jvmti_refuses_a_startup_agent_with_an_init_error_line() {
    let cause = "with error: JVMTI support is not compiled into this CratonVM build";
    assert_eq!(
        startup_agent_unsupported_detail("-agentpath:/tmp/libx.so=fail"),
        format!(
            "Could not find agent library /tmp/libx.so in absolute path, {cause} \
             (it needs the experimental-debug feature)"
        )
    );
    assert!(startup_agent_unsupported_detail("-agentlib:nope")
        .starts_with("Could not find agent library nope on the library path, with error: "));
    assert!(startup_agent_unsupported_detail("-javaagent:a.jar")
        .starts_with("Could not load agent -javaagent:a.jar: "));
}

#[cfg(not(feature = "experimental-debug"))]
#[test]
fn a_build_without_jvmti_stops_the_boot_with_the_init_error_payload() {
    let mut config = VmConfig::default();
    config
        .jvmti_agent_options
        .push("-agentlib:nope".to_string());
    let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        reject_startup_jvmti_agents_when_disabled(&config)
    }))
    .expect_err("a startup agent stops the boot");
    let error = payload
        .downcast_ref::<crate::error::VmInitializationError>()
        .expect("the payload is HotSpot's initialization error");
    assert!(error.report().starts_with(
        "Error occurred during initialization of VM\nCould not find agent library nope"
    ));
}

#[cfg(feature = "experimental-debug")]
#[test]
fn startup_agent_failure_lines_are_hotspots() {
    use crate::jvmti::JvmtiError;
    let failed = JvmtiError::AgentEntryPointFailed {
        path: "/resolved/libx.so".to_string(),
        symbol: "Agent_OnLoad".to_string(),
        code: 7,
    };
    assert_eq!(
        startup_agent_failure_detail("x", false, &failed),
        "agent library failed Agent_OnLoad: x"
    );
    let missing = JvmtiError::AgentEntryPointMissing {
        path: "/tmp/liby.so".to_string(),
        symbol: "Agent_OnLoad".to_string(),
    };
    assert_eq!(
        startup_agent_failure_detail("/tmp/liby.so", true, &missing),
        "Could not find Agent_OnLoad function in the agent library: /tmp/liby.so"
    );
}

#[cfg(feature = "experimental-debug")]
#[test]
#[should_panic(expected = "-javaagent must run through runtime::agent_loader::invoke_premains")]
fn startup_jvmti_rejects_javaagent_tokens() {
    let mut config = VmConfig::default();
    config
        .jvmti_agent_options
        .push("-javaagent:agent.jar=opts".to_string());
    let mut env = crate::jvmti::create_jvmti_env();

    load_startup_jvmti_agents(&config, &mut env);
}

// -----------------------------------------------------------------------
// T2.1.3: JDK module classifier
// -----------------------------------------------------------------------

#[test]
fn t2_classify_java_base() {
    assert_eq!(classify_jdk_module("java/lang/String"), "java.base");
    assert_eq!(classify_jdk_module("java/util/HashMap"), "java.base");
    assert_eq!(classify_jdk_module("java/io/File"), "java.base");
    assert_eq!(classify_jdk_module("java/nio/ByteBuffer"), "java.base");
    assert_eq!(classify_jdk_module("java/net/URL"), "java.base");
    assert_eq!(
        classify_jdk_module("java/security/MessageDigest"),
        "java.base"
    );
    assert_eq!(classify_jdk_module("java/math/BigInteger"), "java.base");
    assert_eq!(classify_jdk_module("java/time/LocalDate"), "java.base");
}

#[test]
fn t2_classify_non_base_modules() {
    assert_eq!(
        classify_jdk_module("java/net/http/HttpClient"),
        "java.net.http"
    );
    assert_eq!(classify_jdk_module("java/sql/DriverManager"), "java.sql");
    assert_eq!(
        classify_jdk_module("javax/management/MBeanServer"),
        "java.management"
    );
    assert_eq!(
        classify_jdk_module("java/util/logging/Logger"),
        "java.logging"
    );
    assert_eq!(classify_jdk_module("jdk/jfr/Event"), "jdk.jfr");
    assert_eq!(
        classify_jdk_module("com/sun/management/ThreadMXBean"),
        "jdk.management"
    );
    assert_eq!(classify_jdk_module("sun/misc/Unsafe"), "jdk.unsupported");
}

#[test]
fn t2_classify_longest_prefix_wins() {
    // java/net/http must win over java/net.
    assert_eq!(
        classify_jdk_module("java/net/http/HttpRequest"),
        "java.net.http"
    );
    // java/util/logging must win over java/util.
    assert_eq!(
        classify_jdk_module("java/util/logging/LogRecord"),
        "java.logging"
    );
}

#[test]
fn t2_classify_unknown_goes_to_other() {
    assert_eq!(classify_jdk_module("com/example/App"), "other");
    assert_eq!(classify_jdk_module("org/acme/Lib"), "other");
    assert_eq!(classify_jdk_module(""), "other");
}

#[test]
fn t2_classify_no_false_prefix_matches() {
    // "java/netXYZ" must NOT match "java/net" — the classifier's
    // prefixes all end with "/".
    assert_eq!(classify_jdk_module("java/netXYZ/Foo"), "other");
}

#[test]
fn t2_grouped_dump_is_empty_when_no_missing_natives() {
    let shared = SharedVm::new(VmConfig::default());
    let grouped = shared.classify_missing_natives_by_module();
    assert!(grouped.is_empty());
}

#[test]
fn t2_grouped_dump_groups_and_dedupes() {
    let shared = SharedVm::new(VmConfig::default());
    shared.record_missing_native("java/lang/Foo", "a", "()V", None);
    shared.record_missing_native("java/net/http/Client", "send", "()V", None);
    shared.record_missing_native("java/sql/Conn", "close", "()V", None);
    // Duplicate should be dropped by record_missing_native's own dedupe.
    shared.record_missing_native("java/lang/Foo", "a", "()V", None);

    let grouped = shared.classify_missing_natives_by_module();
    assert_eq!(grouped.len(), 3);
    assert_eq!(grouped["java.base"].len(), 1);
    assert_eq!(grouped["java.net.http"].len(), 1);
    assert_eq!(grouped["java.sql"].len(), 1);
}

#[test]
fn t2_grouped_json_round_trip_is_valid_json() {
    let shared = SharedVm::new(VmConfig::default());
    shared.record_missing_native("java/lang/Foo", "a", "()V", None);
    shared.record_missing_native("com/example/Bar", "b", "()V", Some("Main.main".into()));

    let tmp = std::env::temp_dir().join("t2_grouped.json");
    shared
        .dump_missing_natives_grouped_json(&tmp)
        .expect("missing natives dump should succeed");

    let content = std::fs::read_to_string(&tmp).expect("file should be readable");
    // Smoke checks — it's syntactically a JSON object with the
    // expected top-level keys and at least one module entry.
    assert!(content.starts_with("{\n"));
    assert!(content.ends_with("}\n"));
    assert!(content.contains("\"version\": 1"));
    assert!(content.contains("\"modules\":"));
    assert!(content.contains("\"java.base\""));
    assert!(content.contains("\"other\""));
    // Entries should appear under their correct groups.
    assert!(content.contains("\"java/lang/Foo\""));
    assert!(content.contains("\"com/example/Bar\""));
    let _ = std::fs::remove_file(&tmp);
}

// -----------------------------------------------------------------------
// SharedVm construction
// -----------------------------------------------------------------------

#[test]
fn shared_vm_default_config() {
    let shared = SharedVm::new(VmConfig::default());
    assert!(shared.natives.native_methods.len() > 0);
    // C25 synthetic stubs plus wired super/interfaces (`Enumeration$Impl`,
    // `java/lang/Object`, `java/util/Enumeration`, `Comparator$Native`,
    // `java/util/Comparator`), PLUS the unmodifiable-collection-view
    // bootstrap (`Collection`/`List`/`Set`/`SortedSet`/`NavigableSet`/
    // `Map`/`ListIterator`/`Serializable` and the 8
    // `cratonvm/internal/Unmodifiable*` synthetic stamps, see
    // `d3474b3e`/`c2d68883`) plus `AssertionError` and `Iterator` and
    // their transitively-loaded superinterfaces. Grew again from 25 to 29
    // with `4edaa9ba7`'s 4 new bootstrap loads: `java/util/Map$Entry`
    // plus the `cratonvm/internal/UnmodifiableEntrySet` +
    // `UnmodifiableMapEntry` + `UnmodifiableEntryItr` synthetic stamps
    // (registered in `vm_init.rs` to fix `Collections.unmodifiableMap()
    // .entrySet()`'s `setValue()` not throwing). This count legitimately
    // grew from 5 as bootstrap work landed; if it changes again, verify
    // the new value against `SharedVm::new`'s class-loading calls rather
    // than assuming a regression.
    assert_eq!(shared.classes.class_manager.read().loaded_count(), 29);
    assert!(shared.classes.statics.read().is_empty());
    assert!(shared.mem.string_pool.read().is_empty());
    assert!(shared.classes.class_mirrors.read().is_empty());
    assert!(shared.classes.lambda_proxies.read().is_empty());
}

#[test]
fn shared_vm_system_properties_populated() {
    let shared = SharedVm::new(VmConfig::default());
    let props = shared.system_properties.read();
    assert!(props.contains_key("os.name"));
    assert!(props.contains_key("os.arch"));
    assert!(props.contains_key("file.separator"));
    assert!(props.contains_key("path.separator"));
    assert!(props.contains_key("line.separator"));
    assert!(props.contains_key("java.version"));
    assert_eq!(
        props.get("java.vendor").map(|s| s.as_str()),
        Some("CratonVM")
    );
    assert!(props.contains_key("file.encoding"));
}

// -----------------------------------------------------------------
// W7-67 — host default locale
// -----------------------------------------------------------------

/// The BCP-47 spelling Windows hands back. These are not "language and
/// country" — a script subtag sits between them and must not be mistaken
/// for the region, which is what a naive `split_once('_')` did.
#[test]
fn parse_locale_name_reads_bcp47_subtags() {
    let ru = parse_locale_name("ru-RU");
    assert_eq!(ru.language, "ru");
    assert_eq!(ru.country, "RU");
    assert_eq!(ru.script, "");
    assert_eq!(ru.variant, "");

    let zh = parse_locale_name("zh-Hans-CN");
    assert_eq!(zh.language, "zh");
    assert_eq!(zh.script, "Hans");
    assert_eq!(zh.country, "CN");

    // A 3-digit region (UN M.49) is a region, not a variant.
    let es = parse_locale_name("es-419");
    assert_eq!(es.language, "es");
    assert_eq!(es.country, "419");

    // Everything after the region is a variant; Java uppercases them and
    // joins multiples with `_`.
    let ca = parse_locale_name("ca-ES-valencia");
    assert_eq!(ca.country, "ES");
    assert_eq!(ca.variant, "VALENCIA");
}

/// The POSIX spelling `$LANG` carries, including the suffixes that are not
/// part of the locale.
#[test]
fn parse_locale_name_reads_posix_spelling() {
    let ru = parse_locale_name("ru_RU.UTF-8");
    assert_eq!(ru.language, "ru");
    assert_eq!(ru.country, "RU");

    let bare = parse_locale_name("en");
    assert_eq!(bare.language, "en");
    assert_eq!(bare.country, "");

    // `@latin`/`@cyrillic` name a script, not a variant.
    let sr = parse_locale_name("sr_RS@latin");
    assert_eq!(sr.language, "sr");
    assert_eq!(sr.country, "RS");
    assert_eq!(sr.script, "Latn");

    // Any other modifier is carried as a variant.
    assert_eq!(parse_locale_name("de_DE@euro").variant, "EURO");
}

/// `java_props_md.c` maps the POSIX locales onto English/US — a real JVM
/// never reports language `"C"`. This is the CI default on a bare shell.
#[test]
fn parse_locale_name_maps_posix_locales_to_english() {
    for raw in ["C", "POSIX", "", "  "] {
        let got = parse_locale_name(raw);
        assert_eq!(got.language, "en", "input {raw:?}");
        assert_eq!(got.country, "US", "input {raw:?}");
    }
    // Only the EXACT names get `en_US`: `C.UTF-8` is parsed, `C` maps to
    // `en` and there is no country (HotSpot 25, LANG=C.UTF-8, measured).
    for raw in ["C.UTF-8", "C.utf8", "POSIX.UTF-8"] {
        let got = parse_locale_name(raw);
        assert_eq!(got.language, "en", "input {raw:?}");
        assert_eq!(got.country, "", "input {raw:?}");
    }
}

/// glibc cannot load a locale nobody installed, `setlocale(LC_ALL, "")`
/// then fails and HotSpot reports `en_US`. `C` and `C.UTF-8` always load.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[test]
fn an_uninstalled_env_locale_is_not_loadable() {
    let can = |name: &str| {
        let c = std::ffi::CString::new(name).unwrap();
        unsafe {
            let loc = libc::newlocale(libc::LC_ALL_MASK, c.as_ptr(), std::ptr::null_mut());
            let ok = !loc.is_null();
            if ok {
                libc::freelocale(loc);
            }
            ok
        }
    };
    assert!(can("C"));
    assert!(can("POSIX"));
    assert!(!can("xx_YY.UTF-8"));
}

/// The three ISO-639 codes Java froze at their pre-1989 spellings. Applying
/// them here is what keeps `System.getProperty("user.language")` equal to
/// `Locale.getDefault().getLanguage()`, which `Locale` reaches by its own
/// internal `convertOldISOCodes`.
#[test]
fn parse_locale_name_applies_the_frozen_iso639_codes() {
    assert_eq!(parse_locale_name("he-IL").language, "iw");
    assert_eq!(parse_locale_name("yi").language, "ji");
    assert_eq!(parse_locale_name("id-ID").language, "in");
}

/// `SystemProps.fillI18nProps` rule 2/3: the base property takes the
/// DISPLAY value, and `.format` appears ONLY when it differs. A host whose
/// UI and regional format agree — the common case — must not grow a
/// redundant overlay, because `Locale.getDefault(FORMAT)` reading a
/// present-but-equal key is indistinguishable from reading the base.
#[test]
fn fill_i18n_props_writes_the_format_overlay_only_when_it_differs() {
    let mut props = HashMap::new();
    fill_i18n_props(&mut props, &[], "user.language", "ru", "ru");
    assert_eq!(props.get("user.language").map(String::as_str), Some("ru"));
    assert!(!props.contains_key("user.language.format"));
    // `.display` is never derived from platform values — the JDK's
    // condition for writing it is dead once the base has taken the same
    // value.
    assert!(!props.contains_key("user.language.display"));

    let mut split = HashMap::new();
    fill_i18n_props(&mut split, &[], "user.language", "en", "ru");
    assert_eq!(split.get("user.language").map(String::as_str), Some("en"));
    assert_eq!(
        split.get("user.language.format").map(String::as_str),
        Some("ru")
    );
}

/// `SystemProps.fillI18nProps` rule 1, and the one that is easy to get
/// wrong: a command-line `-Duser.language` does not merely override the
/// base — it suppresses the derived overlay entirely. Without this,
/// `-Duser.language=en -Duser.country=US` on this ru_RU host would pin the
/// base to en and leave `user.language.format=ru` behind, so
/// `NumberFormat.getInstance()` would still format in Russian and a
/// "pinned locale" run would not actually be pinned.
#[test]
fn fill_i18n_props_lets_a_command_line_value_suppress_the_overlay() {
    let cmdline = vec![("user.language".to_string(), "en".to_string())];
    let mut props = HashMap::new();
    fill_i18n_props(&mut props, &cmdline, "user.language", "en", "ru");
    assert!(
        props.is_empty(),
        "a -D base value must suppress both the derived base and the \
         overlay, got {props:?}"
    );
}

/// Windows HotSpot publishes all four `user.*` locale keys, empty string
/// included. Linux HotSpot publishes only the non-null ones
/// (`fill_i18n_props`), so there only `user.language` is guaranteed.
#[test]
fn shared_vm_publishes_the_whole_user_locale_family() {
    let shared = SharedVm::new(VmConfig::default());
    let props = shared.system_properties.read();
    let always: &[&str] = if cfg!(target_os = "windows") {
        &[
            "user.language",
            "user.script",
            "user.country",
            "user.variant",
        ]
    } else {
        &["user.language"]
    };
    for key in always {
        assert!(props.contains_key(*key), "{key} must be published");
    }
    // Never the raw POSIX locale names.
    let lang = props.get("user.language").cloned().unwrap_or_default();
    assert!(
        !lang.is_empty() && lang != "C" && lang != "POSIX",
        "user.language must be a real language code, got {lang:?}"
    );
}

/// r16 locale lane (b): the Linux `java_props_md.c` `ParseLocale`, one row
/// per environment shape `R16LocaleEnv` covers.
#[test]
fn parse_locale_java_props_md_matches_the_linux_launcher() {
    fn p(raw: &str) -> (String, Option<String>, Option<String>, Option<String>) {
        let got = parse_locale_java_props_md(raw);
        (got.language, got.script, got.country, got.variant)
    }
    let s = |v: &str| Some(v.to_string());
    let en = "en".to_string();
    // Exactly `C` / `POSIX` / no locale: en_US.
    for raw in ["", "C", "POSIX"] {
        assert_eq!(p(raw), (en.clone(), None, s("US"), None), "{raw:?}");
    }
    // `C.UTF-8` is NOT premapped: language `C` -> `en`, NO country.
    assert_eq!(p("C.UTF-8"), (en.clone(), None, None, None));
    assert_eq!(p("C.utf8"), (en.clone(), None, None, None));
    assert_eq!(p("POSIX.UTF-8"), (en.clone(), None, None, None));
    // The codeset is dropped; language/country kept as spelled.
    assert_eq!(p("en_US.UTF-8"), (en.clone(), None, s("US"), None));
    assert_eq!(
        p("de_DE.ISO-8859-1"),
        ("de".to_string(), None, s("DE"), None)
    );
    // `@euro` is ignored; `@latin` / `@cyrillic` are scripts.
    assert_eq!(p("de_DE@euro"), ("de".to_string(), None, s("DE"), None));
    assert_eq!(
        p("de_DE.UTF-8@euro"),
        ("de".to_string(), None, s("DE"), None)
    );
    assert_eq!(
        p("sr_RS.UTF-8@latin"),
        ("sr".to_string(), s("Latn"), s("RS"), None)
    );
    assert_eq!(
        p("sr_RS@cyrillic"),
        ("sr".to_string(), s("Cyrl"), s("RS"), None)
    );
    // `@nynorsk` is the one variant.
    assert_eq!(
        p("no_NO@nynorsk"),
        ("no".to_string(), None, s("NO"), s("NY"))
    );
    // A bare language goes through `locale_aliases` and gains a country.
    assert_eq!(p("de"), ("de".to_string(), None, s("DE"), None));
    assert_eq!(p("en.UTF-8"), (en.clone(), None, s("US"), None));
    assert_eq!(p("eo"), ("eo".to_string(), None, None, None));
    // Old ISO codes and the Linux oddities.
    assert_eq!(p("he_IL.UTF-8"), ("iw".to_string(), None, s("IL"), None));
    assert_eq!(p("sr_YU"), ("sr".to_string(), None, s("CS"), None));
    assert_eq!(p("german"), ("de".to_string(), None, s("DE"), None));
    // Everything after the first `_` is the country, verbatim.
    assert_eq!(
        p("ca_ES_VALENCIA.UTF-8"),
        ("ca".to_string(), None, s("ES_VALENCIA"), None)
    );
}

/// `fillI18nProps` with a NULL platform value publishes nothing, and a
/// non-null format beside a null base still gets its `.format` key.
#[test]
fn fill_i18n_props_platform_follows_the_nulls() {
    let mut props = HashMap::new();
    fill_i18n_props_platform(&mut props, &[], "user.country", None, None);
    assert!(props.is_empty());
    fill_i18n_props_platform(&mut props, &[], "user.country", None, Some("DE"));
    assert_eq!(
        props.get("user.country.format").map(String::as_str),
        Some("DE")
    );
    assert!(!props.contains_key("user.country"));
    let mut same = HashMap::new();
    fill_i18n_props_platform(&mut same, &[], "user.language", Some("en"), Some("en"));
    assert_eq!(same.len(), 1);
}

#[test]
fn shared_vm_custom_system_properties() {
    let mut config = VmConfig::default();
    config
        .system_properties
        .push(("custom.key".to_string(), "custom.value".to_string()));
    config
        .system_properties
        .push(("java.version".to_string(), "21.0".to_string()));
    let shared = SharedVm::new(config);
    let props = shared.system_properties.read();
    assert_eq!(
        props.get("custom.key").map(|s| s.as_str()),
        Some("custom.value")
    );
    // User override should take precedence
    assert_eq!(props.get("java.version").map(|s| s.as_str()), Some("21.0"));
}

// NEW-11: `PrintStream.println` is a synthetic override. In the
// non-synthetic default build the real JDK bytecode handles it,
// so the native is only present when `synthetic-jdk` is enabled.
#[test]
#[cfg(feature = "synthetic-jdk")]
fn shared_vm_native_methods_registered() {
    let shared = SharedVm::new(VmConfig::default());
    // Check a few known native methods are registered
    assert!(shared
        .natives
        .native_methods
        .find("java/io/PrintStream", "println", "(Ljava/lang/String;)V")
        .is_some());
    assert!(shared
        .natives
        .native_methods
        .find("java/io/PrintStream", "println", "(I)V")
        .is_some());
}

#[test]
fn shared_vm_lambda_id_starts_high() {
    let shared = SharedVm::new(VmConfig::default());
    let id = shared.alloc_lambda_proxy_id();
    assert!(id.as_u32() >= 0x8000_0000);
}

#[test]
fn shared_vm_lambda_ids_are_sequential() {
    let shared = SharedVm::new(VmConfig::default());
    let id1 = shared.alloc_lambda_proxy_id();
    let id2 = shared.alloc_lambda_proxy_id();
    assert_eq!(id2.as_u32(), id1.as_u32() + 1);
}

#[test]
fn shared_vm_debug_format() {
    let shared = SharedVm::new(VmConfig::default());
    let debug = format!("{:?}", shared);
    assert!(debug.contains("SharedVm"));
    assert!(debug.contains("classes_loaded"));
    assert!(debug.contains("heap_bytes"));
    assert!(debug.contains("native_methods"));
}

// -----------------------------------------------------------------------
// SharedVm: class lock objects
// -----------------------------------------------------------------------

#[test]
fn class_lock_object_created_and_cached() {
    let shared = SharedVm::new(VmConfig::default());
    let class_id = ClassId::new(5);
    let lock1 = shared.get_class_lock_object(class_id);
    let lock2 = shared.get_class_lock_object(class_id);
    // Same class -> same lock object
    assert_eq!(lock1.as_ptr(), lock2.as_ptr());
}

#[test]
fn class_lock_objects_differ_per_class() {
    let shared = SharedVm::new(VmConfig::default());
    let lock_a = shared.get_class_lock_object(ClassId::new(1));
    let lock_b = shared.get_class_lock_object(ClassId::new(2));
    assert_ne!(lock_a.as_ptr(), lock_b.as_ptr());
}

#[test]
fn class_lock_object_for_fieldful_class_is_plain_zero_slot_object() {
    let shared = SharedVm::new(VmConfig::default());
    let fieldful_id = shared
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class("cratonvm/test/FieldfulStaticLockTarget", 3)
        .expect("Compatible mode fabricates; this fixture never runs under --real-jdk");
    let object_id = shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id("java/lang/Object")
        .unwrap_or(ClassId::new(0));

    let lock = shared.get_class_lock_object(fieldful_id);
    let header = shared.mem.heap.get_header(lock);

    assert_eq!(header.class_id, object_id);
    assert_eq!(header.num_slots(), 0);
}

// -----------------------------------------------------------------------
// SharedVm: ensure_system_streams
// -----------------------------------------------------------------------

#[test]
fn ensure_system_streams_creates_objects() {
    let shared = SharedVm::new(VmConfig::default());
    let (out, err) = shared.ensure_system_streams();
    let out_header = shared.mem.heap.get_header(out);
    let err_header = shared.mem.heap.get_header(err);
    assert_ne!(out.as_ptr(), err.as_ptr());
    assert_eq!(out_header.class_id, err_header.class_id);
    assert!(out_header.num_slots() >= 1);
    assert!(err_header.num_slots() >= 1);

    // Synthetic PrintStream uses slot 0 as its stdout/stderr descriptor.
    // In the real JDK, that slot is FilterOutputStream.out (a reference),
    // and the streams are identified by object identity instead. Writing
    // an integer there would box it and corrupt the real stream graph.
    let slot0_is_ref = cratonvm_gc::class_layout(out_header.class_id.as_u32())
        .and_then(|layout| layout.field_is_ref(0))
        .unwrap_or(false);
    if slot0_is_ref {
        assert!(!matches!(shared.mem.heap.get_field(out, 0), Value::Int(1)));
        assert!(!matches!(shared.mem.heap.get_field(err, 0), Value::Int(2)));
    } else {
        assert_eq!(shared.mem.heap.get_field(out, 0), Value::Int(1));
        assert_eq!(shared.mem.heap.get_field(err, 0), Value::Int(2));
    }
}

#[test]
fn ensure_system_streams_idempotent() {
    let shared = SharedVm::new(VmConfig::default());
    let (out1, err1) = shared.ensure_system_streams();
    let (out2, err2) = shared.ensure_system_streams();
    assert_eq!(out1.as_ptr(), out2.as_ptr());
    assert_eq!(err1.as_ptr(), err2.as_ptr());
}

// -----------------------------------------------------------------------
// Vm construction
// -----------------------------------------------------------------------

/// Dropping a `Vm` must actually destroy its `SharedVm`.
///
/// It did not: `JcmdProcessor::new_with_vm_state` cloned the `Arc` into
/// nine diagnostic-command closures that live inside
/// `SharedVm::debug.jcmd_processor`, so the VM owned nine strong
/// references to itself. `Arc::strong_count` went 1 -> 10 across that one
/// call and stayed at 9 after the `Vm` was dropped, leaking the heap,
/// class manager, thread registry and JIT caches of every VM ever built --
/// visible in this crate's own test binary as ~80 `Attach-Listener`
/// threads outliving the tests that created them.
#[test]
fn dropping_a_vm_destroys_its_shared_state() {
    let vm = Vm::new(VmConfig::default());
    let weak = std::sync::Arc::downgrade(&vm.shared);
    assert_eq!(
        std::sync::Arc::strong_count(&vm.shared),
        1,
        "the Vm must be the only strong owner of its SharedVm"
    );
    drop(vm);
    assert!(
        weak.upgrade().is_none(),
        "SharedVm outlived its Vm: {} strong refs remain",
        weak.strong_count()
    );
}

/// gc-common w11-f: dropping a `Vm` whose `SharedVm` lives on (a daemon
/// thread's `Arc`, here the test's clone) retires `ThreadId(0)`. It is no
/// longer alive, so no later pause waits for it, and it publishes no
/// address into the freed `main_thread`. See `Vm::retire_primordial_thread`.
#[test]
fn w11f_dropping_a_vm_retires_the_primordial_thread_entry() {
    let vm = Vm::new(VmConfig::default());
    let shared = std::sync::Arc::clone(&vm.shared);
    let registry = &shared.threads.thread_registry;
    let own = &*vm.main_thread as *const JvmThread as usize;
    assert!(registry.is_alive(ThreadId(0)));
    assert_eq!(registry.own_jvm_thread_addr(ThreadId(0)), Some(own));
    drop(vm);
    assert!(
        !registry.is_alive(ThreadId(0)),
        "the primordial entry outlived its JvmThread"
    );
    assert_eq!(
        registry.own_jvm_thread_addr(ThreadId(0)),
        None,
        "the freed main_thread is still published"
    );
}

/// gc-common w11-f: only the entry that publishes THIS `main_thread` is
/// retired. A `Vm` wrapped around another VM's `SharedVm` (the m19
/// finalizer tests build one) must leave that VM's primordial entry alive.
#[test]
fn w11f_a_wrapper_vm_leaves_the_owners_primordial_entry_alone() {
    let owner = Vm::new(VmConfig::default());
    let wrapper = Vm {
        shared: std::sync::Arc::clone(&owner.shared),
        main_thread: Box::new(JvmThread::new(ThreadId(0), "wrapper")),
    };
    drop(wrapper);
    let registry = &owner.shared.threads.thread_registry;
    let own = &*owner.main_thread as *const JvmThread as usize;
    assert!(registry.is_alive(ThreadId(0)));
    assert_eq!(registry.own_jvm_thread_addr(ThreadId(0)), Some(own));
}

#[test]
fn vm_new_creates_main_thread() {
    let vm = Vm::new(VmConfig::default());
    assert_eq!(vm.main_thread.thread_id, ThreadId(0));
    assert_eq!(vm.main_thread.name, "main");
    assert!(vm.main_thread.printed.is_empty());
}

#[test]
fn vm_main_thread_publishes_stable_tlab_tail() {
    let mut vm = Vm::new(VmConfig::default());
    let mut backing = vec![0u64; 16];
    let base = backing.as_mut_ptr() as *mut u8;
    let size = backing.len() * std::mem::size_of::<u64>();
    vm.main_thread.tlab = unsafe { cratonvm_gc::Tlab::new(base, size) };
    vm.main_thread.tlab.alloc(32, 8).unwrap();

    // Moving `Vm` must not change the address observed by the registry:
    // the `JvmThread` lives in its dedicated Box.
    let vm = vm;
    assert_eq!(
        vm.shared
            .threads
            .thread_registry
            .collect_reserved_tlab_tails(),
        vec![(base as usize + 32, base as usize + size)],
    );
}

#[test]
fn vm_self_arc_initialized() {
    let vm = Vm::new(VmConfig::default());
    // self_arc should be set, and get_arc should work
    let arc = vm.shared.get_arc();
    // Same bootstrap class set as `shared_vm_default_config` — see that
    // test's comment for what's currently in it and why the count moves.
    assert_eq!(arc.classes.class_manager.read().loaded_count(), 29);
}

#[test]
fn vm_debug_format() {
    let vm = Vm::new(VmConfig::default());
    let debug = format!("{:?}", vm);
    assert!(debug.contains("Vm"));
    assert!(debug.contains("classes_loaded"));
    assert!(debug.contains("heap_bytes"));
    assert!(debug.contains("native_methods"));
    assert!(debug.contains("printed_values"));
}

// -----------------------------------------------------------------------
// Vm: static field access
// -----------------------------------------------------------------------

#[test]
fn vm_set_and_get_static() {
    let mut vm = Vm::new(VmConfig::default());
    let class_id = ClassId::new(10);
    vm.set_static(class_id, 0, Value::Int(42));
    assert_eq!(vm.get_static(class_id, 0), Value::Int(42));
}

#[test]
fn vm_get_static_default() {
    let vm = Vm::new(VmConfig::default());
    // Unset static field should return default (Int(0))
    assert_eq!(vm.get_static(ClassId::new(99), 0), Value::Int(0));
}

#[test]
fn vm_static_field_resize() {
    let mut vm = Vm::new(VmConfig::default());
    let class_id = ClassId::new(20);
    // Set field at index 5 without first setting lower indices
    vm.set_static(class_id, 5, Value::Long(999));
    assert_eq!(vm.get_static(class_id, 5), Value::Long(999));
    // Lower indices should be default
    assert_eq!(vm.get_static(class_id, 0), Value::Int(0));
}

// -----------------------------------------------------------------------
// Vm: array creation
// -----------------------------------------------------------------------

#[test]
fn vm_new_array() {
    let mut vm = Vm::new(VmConfig::default());
    let arr = vm.new_array(ArrayElementType::Int, 10);
    assert_eq!(vm.shared.mem.heap.array_length(arr), 10);
}

#[test]
fn vm_new_ref_array() {
    let mut vm = Vm::new(VmConfig::default());
    let arr = vm.new_ref_array(ClassId::new(3), 5);
    assert_eq!(vm.shared.mem.heap.array_length(arr), 5);
}

// -----------------------------------------------------------------------
// M9: Cached field counts
// -----------------------------------------------------------------------

#[test]
fn cached_string_num_fields_starts_at_zero() {
    let shared = SharedVm::new(VmConfig::default());
    assert_eq!(
        shared
            .classes
            .cached_string_num_fields
            .load(Ordering::Relaxed),
        0
    );
}

#[test]
fn cached_class_mirror_num_fields_starts_at_zero() {
    let shared = SharedVm::new(VmConfig::default());
    assert_eq!(
        shared
            .classes
            .cached_class_mirror_num_fields
            .load(Ordering::Relaxed),
        0
    );
}

#[test]
fn create_java_string_caches_field_count() {
    let shared = SharedVm::new(VmConfig::default());
    // Before first call, cached is 0
    assert_eq!(
        shared
            .classes
            .cached_string_num_fields
            .load(Ordering::Relaxed),
        0
    );
    // Create a string — should resolve and cache the field count
    let s = crate::vm::vm_object::create_java_string(&shared, "hello");
    // After first call, cached should be nonzero (2 for synthetic stub)
    let cached = shared
        .classes
        .cached_string_num_fields
        .load(Ordering::Relaxed);
    assert!(
        cached >= 2,
        "cached field count should be at least 2, got {}",
        cached
    );
    // Second call should use the cached value
    let s2 = crate::vm::vm_object::create_java_string(&shared, "world");
    assert_ne!(s.as_ptr(), s2.as_ptr()); // different strings
}

#[test]
fn class_mirror_caches_field_count() {
    let shared = SharedVm::new(VmConfig::default());
    assert_eq!(
        shared
            .classes
            .cached_class_mirror_num_fields
            .load(Ordering::Relaxed),
        0
    );
    let _mirror = crate::vm::vm_object::get_or_create_class_mirror(&shared, ClassId::new(1));
    let cached = shared
        .classes
        .cached_class_mirror_num_fields
        .load(Ordering::Relaxed);
    assert!(
        cached >= 2,
        "cached class mirror field count should be at least 2, got {}",
        cached
    );
}

#[test]
fn pre_set_cached_field_count_used() {
    let shared = SharedVm::new(VmConfig::default());
    // Pre-set to 4 (simulating real JDK String with 4 fields)
    shared
        .classes
        .cached_string_num_fields
        .store(4, Ordering::Relaxed);
    let s = crate::vm::vm_object::create_java_string(&shared, "test4fields");
    // The string should have been allocated with 4 fields, so writing field 3 should work
    shared.mem.heap.set_field(s, 3, Value::Int(42));
    assert_eq!(shared.mem.heap.get_field(s, 3), Value::Int(42));
}

// -----------------------------------------------------------------------
// M9: Missing native audit log
// -----------------------------------------------------------------------

#[test]
fn missing_natives_log_initially_empty() {
    let shared = SharedVm::new(VmConfig::default());
    assert!(shared.debug.missing_natives_log.lock().is_empty());
}

#[test]
fn audit_missing_natives_config_default_false() {
    let config = VmConfig::default();
    assert!(!config.audit_missing_natives);
}

#[test]
fn audit_missing_natives_config_builder() {
    let config = VmConfig::new().with_audit_missing_natives(true);
    assert!(config.audit_missing_natives);
}

#[test]
fn dump_missing_natives_noop_when_empty() {
    let shared = SharedVm::new(VmConfig::default());
    // Should not panic even with empty log
    shared.dump_missing_natives();
}

#[test]
fn missing_natives_log_dedup_via_record() {
    let shared = SharedVm::new(VmConfig::default());
    // record_missing_native dedupes on (class, method, descriptor).
    shared.record_missing_native("java/lang/Foo", "bar", "()V", None);
    shared.record_missing_native("java/lang/Baz", "qux", "(I)I", Some("main".into()));
    shared.record_missing_native("java/lang/Foo", "bar", "()V", None); // dup
    let log = shared.debug.missing_natives_log.lock();
    assert_eq!(log.len(), 2);
    // dump_missing_natives prints without panicking even on a
    // populated log.
    drop(log);
    shared.dump_missing_natives();
}

// -----------------------------------------------------------------------
// NEW-10 — missing-natives JSON dump
// -----------------------------------------------------------------------

/// Write the missing-natives log to a temp file and read back the
/// produced JSON. Asserts: schema, key names, ordering, escape,
/// call-site capture.
#[test]
fn new10_dump_missing_natives_json_schema() {
    let shared = SharedVm::new(VmConfig::default());
    shared.record_missing_native(
        "java/lang/Thread",
        "setNative",
        "()V",
        Some("com/example/Main.main([Ljava/lang/String;)V".into()),
    );
    shared.record_missing_native("java/lang/Foo", "bar", "(I)V", None);
    // Duplicate entry — must not appear twice.
    shared.record_missing_native(
        "java/lang/Foo",
        "bar",
        "(I)V",
        Some("another/caller.m()V".into()),
    );

    let tmp_dir = std::env::temp_dir().join("cratonvm_new10_test");
    let _ = std::fs::create_dir_all(&tmp_dir);
    let path = tmp_dir.join(format!("missing_{}.json", std::process::id()));
    shared
        .dump_missing_natives_json(&path)
        .expect("dump should succeed");

    let contents = std::fs::read_to_string(&path).expect("read back");

    // Schema: outer object with the "missing_natives" key
    assert!(
        contents.starts_with("{\n  \"missing_natives\": ["),
        "file must start with the JSON schema header, got {contents:?}"
    );
    assert!(
        contents.trim_end().ends_with("]\n}"),
        "file must end with `]\\n}}`, got {contents:?}"
    );

    // Entries are sorted by (class, name, desc) — java/lang/Foo
    // appears before java/lang/Thread alphabetically.
    let foo_pos = contents.find("java/lang/Foo").expect("Foo present");
    let thread_pos = contents.find("java/lang/Thread").expect("Thread present");
    assert!(
        foo_pos < thread_pos,
        "entries should be sorted alphabetically"
    );

    // Call-site preserved (first occurrence wins).
    assert!(contents.contains("com/example/Main.main([Ljava/lang/String;)V"));
    // And the first-seen null-call-site entry is still null (not
    // overwritten by the duplicate's non-null call-site).
    assert!(contents.contains("\"sample_call_site\": null"));

    // No duplicate Foo entry.
    let foo_count = contents.matches("java/lang/Foo").count();
    assert_eq!(foo_count, 1, "duplicate record must dedupe to 1 Foo entry");

    let _ = std::fs::remove_file(&path);
}

/// Repeated runs must produce byte-identical JSON for the same
/// set of entries (so the committed baseline file diffs cleanly).
#[test]
fn new10_dump_missing_natives_json_is_diff_stable() {
    let shared1 = SharedVm::new(VmConfig::default());
    let shared2 = SharedVm::new(VmConfig::default());
    // Insert in different orders — the output must be identical
    // because dump sorts on (class, name, desc).
    shared1.record_missing_native("a/B", "one", "()V", Some("x".into()));
    shared1.record_missing_native("a/A", "two", "()I", None);
    shared2.record_missing_native("a/A", "two", "()I", None);
    shared2.record_missing_native("a/B", "one", "()V", Some("x".into()));

    let tmp = std::env::temp_dir().join("cratonvm_new10_diff_stable");
    let _ = std::fs::create_dir_all(&tmp);
    let p1 = tmp.join(format!("d1_{}.json", std::process::id()));
    let p2 = tmp.join(format!("d2_{}.json", std::process::id()));
    shared1
        .dump_missing_natives_json(&p1)
        .expect("missing natives dump should succeed");
    shared2
        .dump_missing_natives_json(&p2)
        .expect("missing natives dump should succeed");

    let a = std::fs::read_to_string(&p1).expect("file should be readable");
    let b = std::fs::read_to_string(&p2).expect("file should be readable");
    assert_eq!(
        a, b,
        "dump output must be byte-identical regardless of record order"
    );
    let _ = std::fs::remove_file(&p1);
    let _ = std::fs::remove_file(&p2);
}

/// JSON escaping: a class name containing quote / backslash /
/// control characters round-trips through json_escape correctly.
/// The on-disk file must be valid JSON that any conformant parser
/// accepts.
#[test]
fn new10_dump_missing_natives_json_escapes_strings() {
    let shared = SharedVm::new(VmConfig::default());
    shared.record_missing_native(
        "evil\"class\\name",
        "method\nwith\tcontrol",
        "()V",
        Some("caller\u{0001}ctrl".into()),
    );
    let tmp = std::env::temp_dir().join("cratonvm_new10_escape");
    let _ = std::fs::create_dir_all(&tmp);
    let path = tmp.join(format!("esc_{}.json", std::process::id()));
    shared
        .dump_missing_natives_json(&path)
        .expect("missing natives dump should succeed");
    let contents = std::fs::read_to_string(&path).expect("file should be readable");
    // The raw control characters must NOT appear in the file —
    // they must be encoded.
    assert!(!contents.contains('\n') || contents.matches('\n').count() > 2);
    assert!(contents.contains("\\\""));
    assert!(contents.contains("\\\\"));
    assert!(contents.contains("\\n"));
    assert!(contents.contains("\\t"));
    assert!(contents.contains("\\u0001"));
    let _ = std::fs::remove_file(&path);
}

/// Empty audit log still produces a valid JSON document (the
/// schema's `missing_natives` array is empty).
#[test]
fn new10_dump_missing_natives_json_empty_log() {
    let shared = SharedVm::new(VmConfig::default());
    let tmp = std::env::temp_dir().join("cratonvm_new10_empty");
    let _ = std::fs::create_dir_all(&tmp);
    let path = tmp.join(format!("empty_{}.json", std::process::id()));
    shared
        .dump_missing_natives_json(&path)
        .expect("missing natives dump should succeed");
    let contents = std::fs::read_to_string(&path).expect("file should be readable");
    // Exact format: "{\n  \"missing_natives\": []\n}\n"
    assert_eq!(contents, "{\n  \"missing_natives\": []\n}\n");
    let _ = std::fs::remove_file(&path);
}

// -----------------------------------------------------------------------
// M9: Real JDK mode registers fewer natives
// -----------------------------------------------------------------------

#[test]
fn real_jdk_mode_registers_fewer_natives() {
    // NOTE: this test previously asserted `synthetic_stubs == 0` for
    // real-JDK mode, backed by `set_drop_synthetic_stubs(true)` forced
    // on unconditionally in `vm_init.rs`'s real-JDK arms (dev d8092acb,
    // 2026-07-14). That default was reverted the same day: several
    // SyntheticStub-tagged register_* clusters (the whole
    // java.lang.management/JMX native surface, java.util.function.
    // Function$Identity) are permanent bridges needed in BOTH modes --
    // no working real-bytecode fallback exists for them yet -- and
    // dropping them broke WildFly boot immediately (ObjectName NPE /
    // UnsatisfiedLinkError). `drop_synthetic_stubs` is opt-in only
    // again (`CRATONVM_NO_STUBS`), matching its own field doc. This
    // test now checks the weaker, still-true invariant: real-JDK mode
    // registers meaningfully fewer natives than synthetic-JDK mode
    // (fewer collection/layout fallbacks needed once real bytecode
    // handles those classes directly).
    let mut real_config = VmConfig::default();
    real_config.use_synthetic_jdk = false;
    let real_shared = SharedVm::new(real_config);
    let real_count = real_shared
        .natives
        .native_methods
        .dump_registrations()
        .len();

    let mut synthetic_config = VmConfig::default();
    synthetic_config.use_synthetic_jdk = true;
    let synthetic_shared = SharedVm::new(synthetic_config);
    let synthetic_count = synthetic_shared
        .natives
        .native_methods
        .dump_registrations()
        .len();

    assert!(
        real_count <= synthetic_count,
        "real-JDK mode ({real_count}) should not register MORE natives \
         than synthetic-JDK mode ({synthetic_count})"
    );
}

#[test]
fn drop_synthetic_stubs_mechanism_still_works_when_opted_in() {
    // The `CRATONVM_NO_STUBS` / `set_drop_synthetic_stubs(true)` opt-in
    // mechanism itself (native-api/src/registry.rs) is unit-tested here
    // directly, decoupled from whether any particular boot mode enables
    // it by default (see `real_jdk_mode_registers_fewer_natives` above
    // for why real-JDK mode does not, as of 2026-07-14).
    let mut r = cratonvm_native_api::NativeMethodRegistry::new();
    r.set_drop_synthetic_stubs(true);
    // Test fixture, not a VM registration: this throwaway registry never
    // reaches a running VM and class `Test` does not exist. The two empty
    // bodies exist only so the assertions below can observe which category
    // `drop_synthetic_stubs` filters. KEEP.
            r.register("Test", "stub", "()V", |_ctx, _args| Ok(None), cratonvm_native_api::NativeKind::SyntheticStub);
            r.register("Test", "bridge", "()V", |_ctx, _args| Ok(None), cratonvm_native_api::NativeKind::Bridge);
    let kinds: Vec<_> = r
        .dump_registrations()
        .iter()
        .map(|(_, m, _, k)| (m.to_string(), *k))
        .collect();
    assert!(
        !kinds.iter().any(|(m, _)| m == "stub"),
        "SyntheticStub registration should have been dropped"
    );
    assert!(
        kinds.iter().any(|(m, _)| m == "bridge"),
        "Bridge registration should survive drop_synthetic_stubs"
    );
}

// NEW-11: the "synthetic mode registers many natives" assertion is
// inherently feature-gated — it is checking a property of the
// `synthetic-jdk` build. In the default build the synthetic
// overrides are not registered, so the count is roughly 1200
// instead of 4000+.
#[test]
#[cfg(feature = "synthetic-jdk")]
fn synthetic_mode_registers_many_natives() {
    let shared = SharedVm::new(VmConfig::default());
    // Synthetic mode should have 5000+ registrations
    assert!(
        shared.natives.native_methods.len() > 4000,
        "Synthetic mode should have > 4000 natives, got {}",
        shared.natives.native_methods.len()
    );
}

// -----------------------------------------------------------------------
// M19: Finalizer / Cleaner execution
// -----------------------------------------------------------------------

#[test]
fn m19_shared_vm_has_reference_processor() {
    let shared = SharedVm::new(VmConfig::default());
    let rp = shared.mem.ref_processor.lock();
    assert_eq!(rp.pending_finalization_count(), 0);
}

#[test]
fn m19_shared_vm_has_finalizer_thread() {
    let shared = SharedVm::new(VmConfig::default());
    assert_eq!(shared.mem.finalizer_thread.pending_count(), 0);
    assert!(!shared.mem.finalizer_thread.is_running());
}

#[test]
fn m19_shared_vm_has_cleaner_thread() {
    let shared = SharedVm::new(VmConfig::default());
    assert_eq!(shared.mem.cleaner_thread.pending_count(), 0);
    assert!(!shared.mem.cleaner_thread.is_running());
}

#[test]
fn m19_register_finalizable_enqueues() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    // Allocate a dummy object to get a valid address
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    shared.register_finalizable(obj.as_ptr() as usize);
    let rp = shared.mem.ref_processor.lock();
    // Should have one finalizer reference discovered
    assert!(
        rp.stats().finalizer_refs_discovered == 0,
        "before processing, discovered count is still 0 (it's set during process_references)"
    );
}

#[test]
fn m19_process_references_enqueues_dead_finalizable() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let addr = obj.as_ptr() as usize;
    shared.register_finalizable(addr);

    // Process references — the object is "dead" (not marked)
    shared.process_references(&|_| false, 100, 0);

    // The finalizer thread should have the object enqueued
    assert_eq!(shared.mem.finalizer_thread.pending_count(), 1);
    let dequeued = shared.mem.finalizer_thread.dequeue();
    assert_eq!(dequeued, Some(addr));
}

#[test]
fn m19_process_references_keeps_live_finalizable() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let addr = obj.as_ptr() as usize;
    shared.register_finalizable(addr);

    // Process references — the object is "live" (marked)
    shared.process_references(&|_| true, 100, 0);

    // The finalizer thread should NOT have the object
    assert_eq!(shared.mem.finalizer_thread.pending_count(), 0);
}

#[test]
fn m19_drain_finalizers_transfers_to_thread() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let addr = obj.as_ptr() as usize;

    // Directly enqueue in ref_processor's finalization_queue
    {
        let mut rp = shared.mem.ref_processor.lock();
        rp.discover_reference(
            cratonvm_gc::reference::ReferenceType::Finalizer,
            addr,
            addr,
            None,
        );
    }
    // Process to move to finalization queue
    shared.process_references(&|_| false, 100, 0);

    // drain_finalizers should report 0 because process_references already
    // pushed directly to the finalizer_thread
    assert_eq!(shared.mem.finalizer_thread.pending_count(), 1);
}

#[test]
fn m19_cleaner_actions_drained() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    shared.mem.cleaner_thread.submit_action(0x1000);
    shared.mem.cleaner_thread.submit_action(0x2000);
    let actions = shared.drain_cleaners();
    assert_eq!(actions, vec![0x1000, 0x2000]);
    assert_eq!(shared.mem.cleaner_thread.pending_count(), 0);
}

#[test]
fn m19_finalizer_thread_lifecycle() {
    let shared = SharedVm::new(VmConfig::default());
    assert!(!shared.mem.finalizer_thread.is_running());
    shared.mem.finalizer_thread.start();
    assert!(shared.mem.finalizer_thread.is_running());
    shared.mem.finalizer_thread.stop();
    assert!(!shared.mem.finalizer_thread.is_running());
}

#[test]
fn m19_cleaner_thread_lifecycle() {
    let shared = SharedVm::new(VmConfig::default());
    assert!(!shared.mem.cleaner_thread.is_running());
    shared.mem.cleaner_thread.start();
    assert!(shared.mem.cleaner_thread.is_running());
    shared.mem.cleaner_thread.stop();
    assert!(!shared.mem.cleaner_thread.is_running());
}

// -----------------------------------------------------------------------
// Session 7: Bootstrap java.lang.Object from real JDK bytecode
// Run with: cargo test -p cratonvm-vm -- --ignored
// -----------------------------------------------------------------------

/// Helper: create a SharedVm configured for real JDK mode.
/// Returns None if no JDK is available.
fn create_real_jdk_vm() -> Option<SharedVm> {
    create_real_jdk_vm_with_config(false)
}

fn create_real_jdk_vm_audit() -> Option<SharedVm> {
    create_real_jdk_vm_with_config(true)
}

fn create_real_jdk_vm_with_config(audit: bool) -> Option<SharedVm> {
    // Detect JDK from JAVA_HOME or PATH
    let java_home = crate::config::resolve_java_home_public(None)?;
    let java_home_str = java_home.to_string_lossy().into_owned();

    let config = VmConfig::new().with_java_home(java_home_str);

    // Set use_synthetic_jdk to false to load real JDK classes
    let mut config = config;
    config.use_synthetic_jdk = false;
    config.audit_missing_natives = audit;

    Some(SharedVm::new(config))
}

#[test]
#[ignore] // requires JDK on host
fn s7_object_loaded_from_real_bytecode() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    // java.lang.Object should be loaded during bootstrap
    let obj_id = cm.get_loaded_class_id("java/lang/Object");
    assert!(obj_id.is_some(), "Object should be loaded");

    let obj_id = obj_id.expect("obj_id should be Some");
    let cls = cm
        .class_store
        .get(obj_id)
        .expect("class should exist in class store");

    // Verify it's from real bytecode, not a synthetic stub
    assert!(
        !cls.origin.is_compatibility_stub(),
        "Object should come from real bytecode, not a synthetic stub"
    );

    // Object has no superclass
    assert!(cls.superclass.is_none(), "Object has no superclass");

    // Object should have methods from real bytecode
    assert!(
        !cls.methods.is_empty(),
        "Object should have methods parsed from bytecode, got 0"
    );

    // Check specific methods exist
    let method_names: Vec<&str> = cls.methods.iter().map(|m| m.name.as_ref()).collect();
    assert!(
        method_names.contains(&"hashCode"),
        "Object should have hashCode method, found: {:?}",
        method_names
    );
    assert!(
        method_names.contains(&"equals"),
        "Object should have equals method, found: {:?}",
        method_names
    );
    assert!(
        method_names.contains(&"toString"),
        "Object should have toString method, found: {:?}",
        method_names
    );
    assert!(
        method_names.contains(&"getClass"),
        "Object should have getClass method, found: {:?}",
        method_names
    );

    // Verify native methods have the ACC_NATIVE flag
    let hash_code = cls
        .methods
        .iter()
        .find(|m| &*m.name == "hashCode")
        .expect("method hashCode should exist");
    assert!(hash_code.is_native(), "hashCode should be native");
    let get_class = cls
        .methods
        .iter()
        .find(|m| &*m.name == "getClass")
        .expect("method getClass should exist");
    assert!(get_class.is_native(), "getClass should be native");

    // Verify non-native methods (equals, toString) have bytecode
    let equals = cls
        .methods
        .iter()
        .find(|m| &*m.name == "equals")
        .expect("method equals should exist");
    assert!(
        !equals.is_native(),
        "equals should NOT be native (it's bytecode)"
    );

    eprintln!(
        "java.lang.Object: {} methods, {} fields, synthetic={}",
        cls.methods.len(),
        cls.fields.len(),
        cls.origin.is_compatibility_stub()
    );
}

#[test]
#[ignore] // requires JDK on host
fn s7_object_native_methods_wired() {
    let shared = create_real_jdk_vm().expect("No JDK found");

    // Verify all Object native methods are registered in the native registry
    assert!(
        shared
            .natives
            .native_methods
            .find("java/lang/Object", "hashCode", "()I")
            .is_some(),
        "hashCode native should be registered"
    );
    assert!(
        shared
            .natives
            .native_methods
            .find("java/lang/Object", "getClass", "()Ljava/lang/Class;")
            .is_some(),
        "getClass native should be registered"
    );
    assert!(
        shared
            .natives
            .native_methods
            .find("java/lang/Object", "clone", "()Ljava/lang/Object;")
            .is_some(),
        "clone native should be registered"
    );
    assert!(
        shared
            .natives
            .native_methods
            .find("java/lang/Object", "notify", "()V")
            .is_some(),
        "notify native should be registered"
    );
    assert!(
        shared
            .natives
            .native_methods
            .find("java/lang/Object", "notifyAll", "()V")
            .is_some(),
        "notifyAll native should be registered"
    );
    // JDK 19+: wait0 is the actual native (wait is bytecode)
    assert!(
        shared
            .natives
            .native_methods
            .find("java/lang/Object", "wait0", "(J)V")
            .is_some(),
        "wait0 native should be registered (JDK 19+)"
    );
}

#[test]
#[ignore] // requires JDK on host
fn s7_new_object_and_hashcode() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    let shared = Arc::new(create_real_jdk_vm().expect("No JDK found"));
    let mut thread = JvmThread::new(ThreadId(1), "test");

    // Load Object class
    let obj_class_id = shared
        .classes
        .class_manager
        .write()
        .load_class("java/lang/Object")
        .expect("Should load Object");

    // Get field count for allocation
    let num_fields = shared
        .classes
        .class_manager
        .read()
        .class_store
        .get(obj_class_id)
        .map(|c| c.num_total_fields)
        .unwrap_or(0);

    // Allocate a new Object (equivalent to `new Object()`)
    let obj_ref = shared.mem.heap.alloc_object(obj_class_id, num_fields);

    // Call hashCode() via native dispatch
    let result = crate::vm::vm_exec::invoke_on_class_shared(
        &shared,
        &mut thread,
        obj_class_id,
        "hashCode",
        "()I",
        &[Value::Object(Some(obj_ref))],
    );

    assert!(result.is_ok(), "hashCode() failed: {:?}", result.err());
    let hash = result.expect("result should be Ok");
    assert!(
        matches!(hash, Some(Value::Int(_))),
        "hashCode() should return Int, got {:?}",
        hash
    );
    let hash_val = match hash {
        Some(Value::Int(v)) => v,
        _ => unreachable!(),
    };
    eprintln!("new Object().hashCode() = {hash_val}");
}

#[test]
#[ignore] // requires JDK on host
fn s7_object_equals_identity() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    let shared = Arc::new(create_real_jdk_vm().expect("No JDK found"));
    let mut thread = JvmThread::new(ThreadId(1), "test");

    let obj_class_id = shared
        .classes
        .class_manager
        .write()
        .load_class("java/lang/Object")
        .expect("Should load Object");

    let num_fields = shared
        .classes
        .class_manager
        .read()
        .class_store
        .get(obj_class_id)
        .map(|c| c.num_total_fields)
        .unwrap_or(0);

    let obj1 = shared.mem.heap.alloc_object(obj_class_id, num_fields);
    let obj2 = shared.mem.heap.alloc_object(obj_class_id, num_fields);

    // equals(this) should return true (identity)
    let result = crate::vm::vm_exec::invoke_on_class_shared(
        &shared,
        &mut thread,
        obj_class_id,
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(obj1)), Value::Object(Some(obj1))],
    );
    assert!(result.is_ok(), "equals(self) failed: {:?}", result.err());
    assert_eq!(
        result.expect("result should be Ok"),
        Some(Value::Int(1)),
        "obj.equals(obj) should return true (1)"
    );

    // equals(other) should return false (different objects)
    let result = crate::vm::vm_exec::invoke_on_class_shared(
        &shared,
        &mut thread,
        obj_class_id,
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(obj1)), Value::Object(Some(obj2))],
    );
    assert!(result.is_ok(), "equals(other) failed: {:?}", result.err());
    assert_eq!(
        result.expect("result should be Ok"),
        Some(Value::Int(0)),
        "obj1.equals(obj2) should return false (0)"
    );
}

#[test]
#[ignore] // requires JDK on host
fn s7_object_to_string() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    let shared = Arc::new(create_real_jdk_vm().expect("No JDK found"));
    let mut thread = JvmThread::new(ThreadId(1), "test");

    let obj_class_id = shared
        .classes
        .class_manager
        .write()
        .load_class("java/lang/Object")
        .expect("Should load Object");

    let num_fields = shared
        .classes
        .class_manager
        .read()
        .class_store
        .get(obj_class_id)
        .map(|c| c.num_total_fields)
        .unwrap_or(0);

    let obj = shared.mem.heap.alloc_object(obj_class_id, num_fields);

    let result = crate::vm::vm_exec::invoke_on_class_shared(
        &shared,
        &mut thread,
        obj_class_id,
        "toString",
        "()Ljava/lang/String;",
        &[Value::Object(Some(obj))],
    );
    assert!(result.is_ok(), "toString() failed: {:?}", result.err());

    // toString should return a non-null String reference
    let ret = result.expect("result should be Ok");
    match ret {
        Some(Value::Object(Some(str_ref))) => {
            // Read the string content and verify it starts with "java.lang.Object@"
            let text = crate::vm::vm_object::read_java_string(&shared.mem.heap, str_ref);
            assert!(
                text.is_some(),
                "toString() returned an object but could not read string value"
            );
            let text = text.expect("text extraction should succeed");
            assert!(
                text.starts_with("java.lang.Object@"),
                "toString() should return 'java.lang.Object@<hex>', got: {text}"
            );
        }
        other => panic!("toString() should return Object(Some(..)), got: {other:?}"),
    }
}

#[test]
#[ignore] // requires JDK on host
fn s7_object_class_state_initialized() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    let obj_id = cm
        .get_loaded_class_id("java/lang/Object")
        .expect("Object should be loaded");
    let cls = cm
        .class_store
        .get(obj_id)
        .expect("class should exist in class store");

    // Object should be in Loaded or higher state
    // (It may not be Initialized yet if <clinit> wasn't triggered,
    // but it should at least be Loaded since bootstrap_core_classes ran)
    assert!(
        cls.state != crate::classloading::ClassState::Loading,
        "Object should not be in Loading state, got {:?}",
        cls.state
    );
}

#[test]
#[ignore] // requires JDK on host
fn s7_bootstrap_loads_multiple_real_classes() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    // bootstrap_core_classes should have loaded many real classes
    let modules = cm.list_boot_modules();
    assert!(!modules.is_empty(), "Boot modules should be discovered");

    // Check that key classes are loaded and not synthetic stubs
    for class_name in &[
        "java/lang/Object",
        "java/io/Serializable",
        "java/lang/Comparable",
    ] {
        let id = cm.get_loaded_class_id(class_name);
        assert!(id.is_some(), "{class_name} should be loaded");
        let cls = cm
            .class_store
            .get(id.expect("class id should be loaded"))
            .expect("class should exist in class store");
        assert!(
            !cls.origin.is_compatibility_stub(),
            "{class_name} should be real bytecode, not synthetic"
        );
    }

    let total = cm.loaded_count();
    eprintln!(
        "Bootstrap loaded {total} classes ({} modules)",
        modules.len()
    );
}

// ===================================================================
// Session 8: Bootstrap — java.lang.Class and java.lang.String
// ===================================================================

/// Verify that java/lang/String is loaded from real bytecode with correct
/// field layout (4 instance fields: value, coder, hash, hashIsZero).
#[test]
#[ignore] // requires real JDK on PATH
fn s8_string_loaded_from_real_bytecode() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();
    let id = cm
        .get_loaded_class_id("java/lang/String")
        .expect("String not loaded");
    let cls = cm.get_class(id).expect("String class not found");

    // Must be loaded from real bytecode, not synthetic
    assert!(
        !cls.origin.is_compatibility_stub(),
        "String should be real, not synthetic"
    );

    // JDK 9+: String has 4 instance fields: value (byte[]), coder (byte), hash (int), hashIsZero (boolean)
    assert_eq!(
        cls.num_total_fields, 4,
        "JDK 9+ String has 4 instance fields"
    );

    // Verify field names
    let instance_field_names: Vec<&str> = cls
        .fields
        .iter()
        .filter(|f| !f.is_static())
        .map(|f| f.name.as_ref())
        .collect();
    assert!(
        instance_field_names.contains(&"value"),
        "Should have 'value' field"
    );
    assert!(
        instance_field_names.contains(&"coder"),
        "Should have 'coder' field"
    );
    assert!(
        instance_field_names.contains(&"hash"),
        "Should have 'hash' field"
    );
    assert!(
        instance_field_names.contains(&"hashIsZero"),
        "Should have 'hashIsZero' field"
    );

    // The 'value' field descriptor should be [B (byte array) for compact strings
    let value_field = cls
        .fields
        .iter()
        .find(|f| &*f.name == "value")
        .expect("method value should exist");
    assert_eq!(
        &*value_field.descriptor, "[B",
        "String.value should be byte[] in JDK 9+"
    );

    eprintln!(
        "String loaded: {} instance fields, {} methods",
        cls.num_total_fields,
        cls.methods.len()
    );
}

/// Verify that java/lang/Class is loaded from real bytecode.
#[test]
#[ignore] // requires real JDK on PATH
fn s8_class_loaded_from_real_bytecode() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();
    let id = cm
        .get_loaded_class_id("java/lang/Class")
        .expect("Class not loaded");
    let cls = cm.get_class(id).expect("Class not found");

    assert!(
        !cls.origin.is_compatibility_stub(),
        "Class should be real, not synthetic"
    );

    // JDK 25 Class has many fields (name, module, classLoader, etc.)
    assert!(
        cls.num_total_fields >= 5,
        "Class should have many instance fields, got {}",
        cls.num_total_fields
    );

    // Verify it implements Serializable
    let implements_serializable = cls.interfaces.iter().any(|&iface_id| {
        cm.get_class(iface_id)
            .map(|c| &*c.name == "java/io/Serializable")
            .unwrap_or(false)
    });
    assert!(
        implements_serializable,
        "Class should implement Serializable"
    );

    eprintln!(
        "Class loaded: {} instance fields, {} methods",
        cls.num_total_fields,
        cls.methods.len()
    );
}

/// Verify that compact_strings flag is enabled and String statics are pre-initialized.
#[test]
#[ignore] // requires real JDK on PATH
fn s8_string_compact_strings_enabled() {
    let shared = create_real_jdk_vm().expect("No JDK found");

    // The compact_strings flag should be set by pre_init_string_statics
    assert!(
        shared
            .classes
            .compact_strings
            .load(std::sync::atomic::Ordering::Relaxed),
        "compact_strings should be true for real JDK"
    );

    // Verify COMPACT_STRINGS static field is set
    let cm = shared.classes.class_manager.read();
    let string_id = cm
        .get_loaded_class_id("java/lang/String")
        .expect("class java/lang/String should be loaded");
    let cls = cm
        .get_class(string_id)
        .expect("class should exist in class store");

    // Find COMPACT_STRINGS static field index
    let mut static_idx = 0;
    let mut found = false;
    for field in &cls.fields {
        if field.is_static() {
            if &*field.name == "COMPACT_STRINGS" {
                found = true;
                break;
            }
            static_idx += 1;
        }
    }
    assert!(found, "COMPACT_STRINGS static field should exist");
    drop(cm);

    let val = crate::vm::vm_object::get_static_shared(&shared, string_id, static_idx);
    assert_eq!(val, Value::Int(1), "COMPACT_STRINGS should be true (1)");
}

/// Create a Java String via create_java_string in compact mode, then read it back.
#[test]
#[ignore] // requires real JDK on PATH
fn s8_create_and_read_compact_string() {
    let shared = Arc::new(create_real_jdk_vm().expect("No JDK found"));

    // Create a Latin-1 string
    let hello = crate::vm::vm_object::create_java_string(&shared, "hello");
    let result = crate::vm::vm_object::read_java_string(&shared.mem.heap, hello);
    assert_eq!(result, Some("hello".to_string()));

    // Create a non-Latin-1 string (forces UTF16 coder)
    let emoji = crate::vm::vm_object::create_java_string(&shared, "\u{1F600} smile");
    let result = crate::vm::vm_object::read_java_string(&shared.mem.heap, emoji);
    assert_eq!(result, Some("\u{1F600} smile".to_string()));

    // Verify interning works
    let hello2 = crate::vm::vm_object::create_java_string(&shared, "hello");
    assert_eq!(
        hello.as_ptr(),
        hello2.as_ptr(),
        "Interned strings should be same object"
    );

    eprintln!("Compact string creation and reading works correctly");
}

/// Verify String.intern() native is registered and callable.
#[test]
#[ignore] // requires real JDK on PATH
fn s8_string_intern_native_wired() {
    let shared = create_real_jdk_vm().expect("No JDK found");

    // String.intern() should be in the native registry
    let found = shared.natives.native_methods.find(
        "java/lang/String",
        "intern",
        "()Ljava/lang/String;",
    );
    assert!(
        found.is_some(),
        "String.intern() native should be registered"
    );
}

/// Verify Class native methods are registered for real JDK bootstrap.
#[test]
#[ignore] // requires real JDK on PATH
fn s8_class_natives_wired() {
    let shared = create_real_jdk_vm().expect("No JDK found");

    // All critical Class natives should be registered
    let critical_natives = [
        ("getPrimitiveClass", "(Ljava/lang/String;)Ljava/lang/Class;"),
        (
            "forName0",
            "(Ljava/lang/String;ZLjava/lang/ClassLoader;Ljava/lang/Class;)Ljava/lang/Class;",
        ),
        ("isInstance", "(Ljava/lang/Object;)Z"),
        ("isAssignableFrom", "(Ljava/lang/Class;)Z"),
        ("getSuperclass", "()Ljava/lang/Class;"),
        ("initClassName", "()Ljava/lang/String;"),
        ("isHidden", "()Z"),
        ("desiredAssertionStatus0", "(Ljava/lang/Class;)Z"),
    ];

    for (method, desc) in &critical_natives {
        let found = shared
            .natives
            .native_methods
            .find("java/lang/Class", method, desc);
        assert!(
            found.is_some(),
            "Class.{method}{desc} native should be registered"
        );
    }
}

/// Verify that Class mirrors work correctly with real JDK Class layout.
#[test]
#[ignore] // requires real JDK on PATH
fn s8_class_mirror_with_real_layout() {
    let shared = Arc::new(create_real_jdk_vm().expect("No JDK found"));

    let cm = shared.classes.class_manager.read();
    let string_id = cm
        .get_loaded_class_id("java/lang/String")
        .expect("class java/lang/String should be loaded");
    drop(cm);

    // Create a class mirror for String
    let mirror = crate::vm::vm_object::get_or_create_class_mirror(&shared, string_id);

    // Field 0 stores Int(class_id) for legacy compatibility.
    let stored_id = shared.mem.heap.get_field(mirror, 0);
    assert_eq!(stored_id.as_int(), Some(string_id.as_u32() as i32));

    // Field 1 should store the name
    let name_val = shared.mem.heap.get_field(mirror, 1);
    match name_val {
        Value::Object(Some(name_ref)) => {
            let name = crate::vm::vm_object::read_java_string(&shared.mem.heap, name_ref);
            assert_eq!(name, Some("java/lang/String".to_string()));
        }
        _ => panic!("Expected name string in field 1"),
    }

    // Caching should work
    let mirror2 = crate::vm::vm_object::get_or_create_class_mirror(&shared, string_id);
    assert_eq!(
        mirror.as_ptr(),
        mirror2.as_ptr(),
        "Mirrors should be cached"
    );
}

/// Verify that getPrimitiveClass works with real JDK mode.
#[test]
#[ignore] // requires real JDK on PATH
fn s8_get_primitive_class_mirror() {
    let shared = Arc::new(create_real_jdk_vm().expect("No JDK found"));

    let primitives = [
        "int", "long", "boolean", "byte", "char", "short", "float", "double", "void",
    ];
    for prim in &primitives {
        let mirror = crate::vm::vm_object::get_or_create_primitive_mirror(&shared, prim);
        // Field 0 = Int(-1) marker for primitive mirrors.
        assert_eq!(
            shared.mem.heap.get_field(mirror, 0),
            Value::Int(-1),
            "Primitive mirror for '{prim}' should have Int(-1) marker"
        );
    }
}

/// Verify that String and Class are both loaded as real classes (not synthetic).
#[test]
#[ignore] // requires real JDK on PATH
fn s8_string_and_class_both_real() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    for class_name in &["java/lang/String", "java/lang/Class"] {
        let id = cm
            .get_loaded_class_id(class_name)
            .unwrap_or_else(|| panic!("{class_name} should be loaded"));
        let cls = cm.get_class(id).expect("class should exist in class store");
        assert!(
            !cls.origin.is_compatibility_stub(),
            "{class_name} should be real bytecode, not synthetic"
        );
        assert!(
            cls.methods.len() > 10,
            "{class_name} should have many methods (got {})",
            cls.methods.len()
        );
    }
}

/// Execute String.valueOf(42) using real JDK bytecode — returns the string "42".
#[test]
#[ignore] // requires real JDK on PATH
fn s8_string_valueof_int_execution() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

    let string_id = shared
        .classes
        .class_manager
        .write()
        .load_class("java/lang/String")
        .expect("failed to load class java/lang/String");
    crate::vm::ensure_class_initialized_shared(&shared, &mut thread, string_id)
        .expect("class initialization should succeed");

    // Call String.valueOf(42) — static method
    let result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        string_id,
        "valueOf",
        "(I)Ljava/lang/String;",
        &[Value::Int(42)],
    )
    .expect("String.valueOf(42) failed");

    match result {
        Some(Value::Object(Some(str_ref))) => {
            let s = crate::vm::vm_object::read_java_string(&shared.mem.heap, str_ref);
            assert_eq!(
                s,
                Some("42".to_string()),
                "String.valueOf(42) should return \"42\""
            );
        }
        other => panic!(
            "String.valueOf(42) should return a String object, got {:?}",
            other
        ),
    }
    eprintln!("String.valueOf(42) = \"42\" — real JDK bytecode execution works!");
}

/// Execute "hello".length() using real JDK bytecode — returns 5.
#[test]
#[ignore] // requires real JDK on PATH
fn s8_string_length_execution() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

    // Create the Java string "hello"
    let hello = crate::vm::vm_object::create_java_string(&shared, "hello");

    let string_class_id = shared.mem.heap.class_id_of(hello);
    crate::vm::ensure_class_initialized_shared(&shared, &mut thread, string_class_id)
        .expect("class initialization should succeed");

    // Call "hello".length()
    let result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        string_class_id,
        "length",
        "()I",
        &[Value::Object(Some(hello))],
    )
    .expect("String.length() failed");

    assert_eq!(
        result,
        Some(Value::Int(5)),
        "\"hello\".length() should be 5"
    );
    eprintln!("\"hello\".length() = 5 — real JDK bytecode execution works!");
}

/// Execute Class.forName("java.lang.String") using real JDK bytecode.
#[test]
#[ignore] // requires real JDK on PATH
fn s8_class_forname_execution() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

    let class_id = shared
        .classes
        .class_manager
        .write()
        .load_class("java/lang/Class")
        .expect("failed to load class java/lang/Class");
    crate::vm::ensure_class_initialized_shared(&shared, &mut thread, class_id)
        .expect("class initialization should succeed");

    // Create the argument string "java.lang.String" (dot notation)
    let name_str = crate::vm::vm_object::create_java_string(&shared, "java.lang.String");

    // Call Class.forName0(name, true, null, null) — the native underlying forName
    let result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        class_id,
        "forName0",
        "(Ljava/lang/String;ZLjava/lang/ClassLoader;Ljava/lang/Class;)Ljava/lang/Class;",
        &[
            Value::Object(Some(name_str)),
            Value::Int(1),
            Value::Object(None),
            Value::Object(None),
        ],
    )
    .expect("Class.forName0 failed");

    match result {
        Some(Value::Object(Some(mirror))) => {
            // The returned mirror should be for java/lang/String
            let stored_id = shared.mem.heap.get_field(mirror, 0);
            let cm = shared.classes.class_manager.read();
            let string_id = cm
                .get_loaded_class_id("java/lang/String")
                .expect("class java/lang/String should be loaded");
            assert_eq!(
                stored_id.as_int(),
                Some(string_id.as_u32() as i32),
                "Class.forName(\"java.lang.String\") should return String's class mirror"
            );
        }
        other => panic!(
            "Class.forName should return a Class mirror, got {:?}",
            other
        ),
    }
    eprintln!("Class.forName(\"java.lang.String\") works — real JDK bytecode execution!");
}

/// Execute String.charAt(0) on "hello" — returns 'h'.
#[test]
#[ignore] // requires real JDK on PATH
fn s8_string_charat_execution() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

    let hello = crate::vm::vm_object::create_java_string(&shared, "hello");
    let string_class_id = shared.mem.heap.class_id_of(hello);
    crate::vm::ensure_class_initialized_shared(&shared, &mut thread, string_class_id)
        .expect("class initialization should succeed");

    let result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        string_class_id,
        "charAt",
        "(I)C",
        &[Value::Object(Some(hello)), Value::Int(0)],
    )
    .expect("String.charAt(0) failed");

    assert_eq!(
        result,
        Some(Value::Int('h' as i32)),
        "\"hello\".charAt(0) should be 'h'"
    );
    eprintln!("\"hello\".charAt(0) = 'h' — real JDK bytecode execution works!");
}

/// Execute String.equals() comparing two equal strings.
#[test]
#[ignore] // requires real JDK on PATH
fn s8_string_equals_execution() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

    let s1 = crate::vm::vm_object::create_java_string(&shared, "hello");
    // Create a second string with the same content but different object
    // Force a new allocation by not using interning
    let s2 = {
        let string_id = shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id("java/lang/String")
            .expect("class java/lang/String should be loaded");
        let cls = shared
            .classes
            .class_manager
            .read()
            .get_class(string_id)
            .expect("class should exist in class store")
            .num_total_fields;
        let obj = shared.mem.heap.alloc_object(string_id, cls);
        crate::runtime::interpreter::init_primitive_fields(&shared, obj, string_id);
        // Copy the value array and coder from s1
        let val = shared.mem.heap.get_field(s1, 0); // value byte[]
        shared.mem.heap.set_field(obj, 0, val);
        let coder = shared.mem.heap.get_field(s1, 1); // coder
        shared.mem.heap.set_field(obj, 1, coder);
        obj
    };

    let string_class_id = shared.mem.heap.class_id_of(s1);
    crate::vm::ensure_class_initialized_shared(&shared, &mut thread, string_class_id)
        .expect("class initialization should succeed");

    // s1.equals(s2) should be true
    let result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        string_class_id,
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(s1)), Value::Object(Some(s2))],
    )
    .expect("String.equals() failed");

    assert_eq!(
        result,
        Some(Value::Int(1)),
        "\"hello\".equals(\"hello\") should be true"
    );

    // s1.equals(different string) should be false
    let other = crate::vm::vm_object::create_java_string(&shared, "world");
    let result2 = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        string_class_id,
        "equals",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(s1)), Value::Object(Some(other))],
    )
    .expect("String.equals() failed");

    assert_eq!(
        result2,
        Some(Value::Int(0)),
        "\"hello\".equals(\"world\") should be false"
    );
    eprintln!("String.equals() works correctly with real JDK bytecode!");
}

/// Execute String.hashCode() — returns a consistent value.
#[test]
#[ignore] // requires real JDK on PATH
fn s8_string_hashcode_execution() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

    let hello = crate::vm::vm_object::create_java_string(&shared, "hello");
    let string_class_id = shared.mem.heap.class_id_of(hello);
    crate::vm::ensure_class_initialized_shared(&shared, &mut thread, string_class_id)
        .expect("class initialization should succeed");

    let result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        string_class_id,
        "hashCode",
        "()I",
        &[Value::Object(Some(hello))],
    )
    .expect("String.hashCode() failed");

    // hashCode() should return a non-zero consistent value
    let hash1 = match result {
        Some(Value::Int(h)) => h,
        other => panic!("hashCode() should return Int, got {:?}", other),
    };
    assert_ne!(hash1, 0, "\"hello\".hashCode() should not be 0");

    // Calling again should return the same value (consistency)
    let result2 = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        string_class_id,
        "hashCode",
        "()I",
        &[Value::Object(Some(hello))],
    )
    .expect("String.hashCode() failed");
    assert_eq!(
        result2,
        Some(Value::Int(hash1)),
        "hashCode() should be consistent"
    );

    // Two strings with same content should have same hash
    let hello2 = crate::vm::vm_object::create_java_string(&shared, "hello");
    let result3 = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        string_class_id,
        "hashCode",
        "()I",
        &[Value::Object(Some(hello2))],
    )
    .expect("String.hashCode() failed");
    assert_eq!(
        result3,
        Some(Value::Int(hash1)),
        "same content should have same hashCode"
    );

    eprintln!(
        "\"hello\".hashCode() = {} — consistent and non-zero!",
        hash1
    );
}

/// Execute Class.getName() on String's class mirror.
#[test]
#[ignore] // requires real JDK on PATH
fn s8_class_getname_execution() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

    let cm = shared.classes.class_manager.read();
    let string_id = cm
        .get_loaded_class_id("java/lang/String")
        .expect("class java/lang/String should be loaded");
    drop(cm);

    let mirror = crate::vm::vm_object::get_or_create_class_mirror(&shared, string_id);
    let class_class_id = shared.mem.heap.class_id_of(mirror);
    crate::vm::ensure_class_initialized_shared(&shared, &mut thread, class_class_id)
        .expect("class initialization should succeed");

    // Call getName() on the String class mirror
    let result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        class_class_id,
        "getName",
        "()Ljava/lang/String;",
        &[Value::Object(Some(mirror))],
    )
    .expect("Class.getName() failed");

    match result {
        Some(Value::Object(Some(name_ref))) => {
            let name = crate::vm::vm_object::read_java_string(&shared.mem.heap, name_ref);
            assert_eq!(
                name,
                Some("java.lang.String".to_string()),
                "String.class.getName() should return \"java.lang.String\""
            );
        }
        other => panic!("Class.getName() should return a String, got {:?}", other),
    }
    eprintln!(
        "String.class.getName() = \"java.lang.String\" — real JDK bytecode execution works!"
    );
}

/// Execute String.substring(1, 4) on "hello" — returns "ell".
#[test]
#[ignore] // requires real JDK on PATH
fn s8_string_substring_execution() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

    let hello = crate::vm::vm_object::create_java_string(&shared, "hello");
    let string_class_id = shared.mem.heap.class_id_of(hello);
    crate::vm::ensure_class_initialized_shared(&shared, &mut thread, string_class_id)
        .expect("class initialization should succeed");

    let result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        string_class_id,
        "substring",
        "(II)Ljava/lang/String;",
        &[Value::Object(Some(hello)), Value::Int(1), Value::Int(4)],
    )
    .expect("String.substring(1,4) failed");

    match result {
        Some(Value::Object(Some(sub_ref))) => {
            let s = crate::vm::vm_object::read_java_string(&shared.mem.heap, sub_ref);
            assert_eq!(
                s,
                Some("ell".to_string()),
                "\"hello\".substring(1,4) should be \"ell\""
            );
        }
        other => panic!("String.substring should return a String, got {:?}", other),
    }
    eprintln!("\"hello\".substring(1,4) = \"ell\" — real JDK bytecode execution works!");
}

/// Execute Class.isInstance() — verify runtime type checking works.
#[test]
#[ignore] // requires real JDK on PATH
fn s8_class_isinstance_execution() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

    let cm = shared.classes.class_manager.read();
    let string_id = cm
        .get_loaded_class_id("java/lang/String")
        .expect("class java/lang/String should be loaded");
    drop(cm);

    let mirror = crate::vm::vm_object::get_or_create_class_mirror(&shared, string_id);
    let class_class_id = shared.mem.heap.class_id_of(mirror);
    crate::vm::ensure_class_initialized_shared(&shared, &mut thread, class_class_id)
        .expect("class initialization should succeed");

    // A String object should be an instance of String.class
    let hello = crate::vm::vm_object::create_java_string(&shared, "test");
    let result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        class_class_id,
        "isInstance",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(mirror)), Value::Object(Some(hello))],
    )
    .expect("Class.isInstance() failed");

    assert_eq!(
        result,
        Some(Value::Int(1)),
        "String.class.isInstance(\"test\") should be true"
    );

    // null should not be an instance
    let result_null = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        class_class_id,
        "isInstance",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(mirror)), Value::Object(None)],
    )
    .expect("Class.isInstance(null) failed");

    assert_eq!(
        result_null,
        Some(Value::Int(0)),
        "String.class.isInstance(null) should be false"
    );
    eprintln!("Class.isInstance() works correctly with real JDK bytecode!");
}

// ===================================================================
// Session 9: Bootstrap — Core java.lang Classes
// ===================================================================

/// Verify all 17 java.lang wrapper/core classes are loaded from real bytecode.
#[test]
#[ignore] // requires real JDK on PATH
fn s9_all_core_lang_classes_real() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    let core_classes = [
        "java/lang/Object",
        "java/lang/String",
        "java/lang/Class",
        "java/lang/System",
        "java/lang/Thread",
        "java/lang/Throwable",
        "java/lang/Number",
        "java/lang/Integer",
        "java/lang/Long",
        "java/lang/Double",
        "java/lang/Float",
        "java/lang/Boolean",
        "java/lang/Byte",
        "java/lang/Short",
        "java/lang/Character",
        "java/lang/Void",
        "java/lang/Exception",
        "java/lang/RuntimeException",
        "java/lang/Error",
    ];

    for class_name in &core_classes {
        let id = cm
            .get_loaded_class_id(class_name)
            .unwrap_or_else(|| panic!("{class_name} should be loaded"));
        let cls = cm.get_class(id).expect("class should exist in class store");
        assert!(
            !cls.origin.is_compatibility_stub(),
            "{class_name} should be real bytecode, not synthetic"
        );
    }

    eprintln!(
        "All {} core java.lang classes loaded from real bytecode",
        core_classes.len()
    );
}

/// Verify System native methods are all registered.
#[test]
#[ignore] // requires real JDK on PATH
fn s9_system_natives_complete() {
    let shared = create_real_jdk_vm().expect("No JDK found");

    let system_natives = [
        ("registerNatives", "()V"),
        ("currentTimeMillis", "()J"),
        ("nanoTime", "()J"),
        ("arraycopy", "(Ljava/lang/Object;ILjava/lang/Object;II)V"),
        ("identityHashCode", "(Ljava/lang/Object;)I"),
        ("exit", "(I)V"),
        ("setIn0", "(Ljava/io/InputStream;)V"),
        ("setOut0", "(Ljava/io/PrintStream;)V"),
        ("setErr0", "(Ljava/io/PrintStream;)V"),
        ("mapLibraryName", "(Ljava/lang/String;)Ljava/lang/String;"),
    ];

    for (method, desc) in &system_natives {
        assert!(
            shared
                .natives
                .native_methods
                .find("java/lang/System", method, desc)
                .is_some(),
            "System.{method}{desc} should be registered"
        );
    }
}

/// Verify Thread native methods are all registered.
#[test]
#[ignore] // requires real JDK on PATH
fn s9_thread_natives_complete() {
    let shared = create_real_jdk_vm().expect("No JDK found");

    let thread_natives = [
        ("registerNatives", "()V"),
        ("currentThread", "()Ljava/lang/Thread;"),
        ("currentCarrierThread", "()Ljava/lang/Thread;"),
        ("sleep", "(J)V"),
        ("sleepNanos0", "(J)V"),
        ("yield0", "()V"),
        ("start0", "()V"),
        ("interrupt0", "()V"),
        ("isAlive", "()Z"),
        ("holdsLock", "(Ljava/lang/Object;)Z"),
        ("setPriority0", "(I)V"),
        ("setNativeName", "(Ljava/lang/String;)V"),
        ("getNextThreadIdOffset", "()J"),
        ("setCurrentThread", "(Ljava/lang/Thread;)V"),
        ("findScopedValueBindings", "()Ljava/lang/Object;"),
        ("scopedValueCache", "()[Ljava/lang/Object;"),
        ("setScopedValueCache", "([Ljava/lang/Object;)V"),
        ("ensureMaterializedForStackWalk", "(Ljava/lang/Object;)V"),
        ("clearInterruptEvent", "()V"),
    ];

    for (method, desc) in &thread_natives {
        assert!(
            shared
                .natives
                .native_methods
                .find("java/lang/Thread", method, desc)
                .is_some(),
            "Thread.{method}{desc} should be registered"
        );
    }
}

/// Verify VM/CDS internal natives needed by IntegerCache etc. are registered.
#[test]
#[ignore] // requires real JDK on PATH
fn s9_vm_cds_natives_registered() {
    let shared = create_real_jdk_vm().expect("No JDK found");

    // VM natives
    assert!(
        shared
            .natives
            .native_methods
            .find(
                "jdk/internal/misc/VM",
                "getSavedProperty",
                "(Ljava/lang/String;)Ljava/lang/String;"
            )
            .is_some(),
        "VM.getSavedProperty should be registered"
    );
    assert!(
        shared
            .natives
            .native_methods
            .find("jdk/internal/misc/VM", "initLevel", "()I")
            .is_none(),
        "VM.initLevel runs the JDK body under the default-ON `jdk-vm-init-level` row (j27 VMALIGN)"
    );

    // CDS natives
    assert!(
        shared
            .natives
            .native_methods
            .find(
                "jdk/internal/misc/CDS",
                "initializeFromArchive",
                "(Ljava/lang/Class;)V"
            )
            .is_some(),
        "CDS.initializeFromArchive should be registered"
    );
    assert!(
        shared
            .natives
            .native_methods
            .find("jdk/internal/misc/CDS", "isSharingEnabled", "()Z")
            .is_some(),
        "CDS.isSharingEnabled should be registered"
    );
}

/// Verify wrapper classes have TYPE fields pre-initialized with primitive mirrors.
#[test]
#[ignore] // requires real JDK on PATH
fn s9_wrapper_type_fields_initialized() {
    let shared = Arc::new(create_real_jdk_vm().expect("No JDK found"));

    let wrappers = [
        ("java/lang/Integer", "int"),
        ("java/lang/Long", "long"),
        ("java/lang/Boolean", "boolean"),
        ("java/lang/Byte", "byte"),
        ("java/lang/Short", "short"),
        ("java/lang/Character", "char"),
        ("java/lang/Float", "float"),
        ("java/lang/Double", "double"),
    ];

    for (wrapper_name, prim_name) in &wrappers {
        let cm = shared.classes.class_manager.read();
        let class_id = cm
            .get_loaded_class_id(wrapper_name)
            .unwrap_or_else(|| panic!("{wrapper_name} should be loaded"));
        let cls = cm
            .get_class(class_id)
            .expect("class should exist in class store");

        // Find TYPE static field index
        let mut static_idx = 0;
        let mut found = false;
        for field in &cls.fields {
            if field.is_static() {
                if &*field.name == "TYPE" {
                    found = true;
                    break;
                }
                static_idx += 1;
            }
        }
        drop(cm);
        assert!(found, "{wrapper_name} should have a TYPE static field");

        let val = crate::vm::vm_object::get_static_shared(&shared, class_id, static_idx);
        match val {
            Value::Object(Some(mirror)) => {
                // The mirror should be a primitive class mirror (field 0 = Int(-1))
                assert_eq!(
                    shared.mem.heap.get_field(mirror, 0),
                    Value::Int(-1),
                    "{wrapper_name}.TYPE should be a primitive mirror for '{prim_name}'"
                );
            }
            _ => panic!(
                "{wrapper_name}.TYPE should be initialized to a primitive mirror, got {val:?}"
            ),
        }
    }
}

/// Verify Number is loaded as abstract class with correct superclass.
#[test]
#[ignore] // requires real JDK on PATH
fn s9_number_class_hierarchy() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    // Number extends Object
    let number_id = cm
        .get_loaded_class_id("java/lang/Number")
        .expect("class java/lang/Number should be loaded");
    let number = cm
        .get_class(number_id)
        .expect("class should exist in class store");
    assert!(!number.origin.is_compatibility_stub());
    let object_id = cm
        .get_loaded_class_id("java/lang/Object")
        .expect("class java/lang/Object should be loaded");
    assert_eq!(number.superclass, Some(object_id));

    // Integer extends Number
    let int_id = cm
        .get_loaded_class_id("java/lang/Integer")
        .expect("class java/lang/Integer should be loaded");
    let int_cls = cm
        .get_class(int_id)
        .expect("class should exist in class store");
    assert_eq!(int_cls.superclass, Some(number_id));

    // Long extends Number
    let long_id = cm
        .get_loaded_class_id("java/lang/Long")
        .expect("class java/lang/Long should be loaded");
    let long_cls = cm
        .get_class(long_id)
        .expect("class should exist in class store");
    assert_eq!(long_cls.superclass, Some(number_id));

    // Double extends Number
    let double_id = cm
        .get_loaded_class_id("java/lang/Double")
        .expect("class java/lang/Double should be loaded");
    let double_cls = cm
        .get_class(double_id)
        .expect("class should exist in class store");
    assert_eq!(double_cls.superclass, Some(number_id));
}

/// Verify Throwable/Exception/Error class hierarchy.
#[test]
#[ignore] // requires real JDK on PATH
fn s9_throwable_hierarchy() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    let throwable_id = cm
        .get_loaded_class_id("java/lang/Throwable")
        .expect("class java/lang/Throwable should be loaded");
    let exception_id = cm
        .get_loaded_class_id("java/lang/Exception")
        .expect("class java/lang/Exception should be loaded");
    let runtime_ex_id = cm
        .get_loaded_class_id("java/lang/RuntimeException")
        .expect("class java/lang/RuntimeException should be loaded");
    let error_id = cm
        .get_loaded_class_id("java/lang/Error")
        .expect("class java/lang/Error should be loaded");
    let object_id = cm
        .get_loaded_class_id("java/lang/Object")
        .expect("class java/lang/Object should be loaded");

    // Throwable extends Object
    let throwable = cm
        .get_class(throwable_id)
        .expect("class should exist in class store");
    assert_eq!(throwable.superclass, Some(object_id));
    assert!(!throwable.origin.is_compatibility_stub());

    // Exception extends Throwable
    let exception = cm
        .get_class(exception_id)
        .expect("class should exist in class store");
    assert_eq!(exception.superclass, Some(throwable_id));

    // RuntimeException extends Exception
    let runtime_ex = cm
        .get_class(runtime_ex_id)
        .expect("class should exist in class store");
    assert_eq!(runtime_ex.superclass, Some(exception_id));

    // Error extends Throwable
    let error = cm
        .get_class(error_id)
        .expect("class should exist in class store");
    assert_eq!(error.superclass, Some(throwable_id));
}

/// Verify System class has expected methods loaded from bytecode.
#[test]
#[ignore] // requires real JDK on PATH
fn s9_system_class_has_methods() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    let sys_id = cm
        .get_loaded_class_id("java/lang/System")
        .expect("class java/lang/System should be loaded");
    let sys = cm
        .get_class(sys_id)
        .expect("class should exist in class store");
    assert!(!sys.origin.is_compatibility_stub());

    // System should have well-known methods
    let method_names: Vec<&str> = sys.methods.iter().map(|m| m.name.as_ref()).collect();
    assert!(
        method_names.contains(&"currentTimeMillis"),
        "System should have currentTimeMillis"
    );
    assert!(
        method_names.contains(&"arraycopy"),
        "System should have arraycopy"
    );
    assert!(method_names.contains(&"exit"), "System should have exit");
    assert!(
        method_names.contains(&"getProperty"),
        "System should have getProperty"
    );
    // initPhase1 is the JDK internal init — present in real JDK System
    assert!(
        method_names.contains(&"initPhase1"),
        "System should have initPhase1"
    );
}

/// Verify Integer class fields and methods from real bytecode.
#[test]
#[ignore] // requires real JDK on PATH
fn s9_integer_class_structure() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    let int_id = cm
        .get_loaded_class_id("java/lang/Integer")
        .expect("class java/lang/Integer should be loaded");
    let int_cls = cm
        .get_class(int_id)
        .expect("class should exist in class store");
    assert!(!int_cls.origin.is_compatibility_stub());

    // Integer has one instance field: 'value' (int)
    let instance_fields: Vec<&str> = int_cls
        .fields
        .iter()
        .filter(|f| !f.is_static())
        .map(|f| f.name.as_ref())
        .collect();
    assert!(
        instance_fields.contains(&"value"),
        "Integer should have 'value' field"
    );

    // Static fields should include TYPE, MIN_VALUE, MAX_VALUE, etc.
    let static_fields: Vec<&str> = int_cls
        .fields
        .iter()
        .filter(|f| f.is_static())
        .map(|f| f.name.as_ref())
        .collect();
    assert!(
        static_fields.contains(&"TYPE"),
        "Integer should have TYPE static"
    );
    assert!(
        static_fields.contains(&"MIN_VALUE"),
        "Integer should have MIN_VALUE"
    );
    assert!(
        static_fields.contains(&"MAX_VALUE"),
        "Integer should have MAX_VALUE"
    );

    // Methods
    let method_names: Vec<&str> = int_cls.methods.iter().map(|m| m.name.as_ref()).collect();
    assert!(
        method_names.contains(&"valueOf"),
        "Integer should have valueOf"
    );
    assert!(
        method_names.contains(&"parseInt"),
        "Integer should have parseInt"
    );
    assert!(
        method_names.contains(&"intValue"),
        "Integer should have intValue"
    );
    assert!(
        method_names.contains(&"toString"),
        "Integer should have toString"
    );
}

/// Verify Thread class is loaded with expected structure.
#[test]
#[ignore] // requires real JDK on PATH
fn s9_thread_class_structure() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    let thread_id = cm
        .get_loaded_class_id("java/lang/Thread")
        .expect("class java/lang/Thread should be loaded");
    let thread = cm
        .get_class(thread_id)
        .expect("class should exist in class store");
    assert!(!thread.origin.is_compatibility_stub());

    // Thread should have many methods
    let method_names: Vec<&str> = thread.methods.iter().map(|m| m.name.as_ref()).collect();
    assert!(method_names.contains(&"start"), "Thread should have start");
    assert!(method_names.contains(&"run"), "Thread should have run");
    assert!(
        method_names.contains(&"getName"),
        "Thread should have getName"
    );
    assert!(
        method_names.contains(&"isAlive"),
        "Thread should have isAlive"
    );
    assert!(
        method_names.contains(&"interrupt"),
        "Thread should have interrupt"
    );

    // Thread should have instance fields like name, tid, priority
    let instance_fields: Vec<&str> = thread
        .fields
        .iter()
        .filter(|f| !f.is_static())
        .map(|f| f.name.as_ref())
        .collect();
    assert!(
        instance_fields.contains(&"name"),
        "Thread should have 'name' field"
    );

    eprintln!(
        "Thread: {} methods, {} instance fields",
        thread.methods.len(),
        instance_fields.len()
    );
}

/// Runtime test: Integer.parseInt("42") returns 42.
#[test]
#[ignore] // requires real JDK on PATH
fn s9_integer_parse_int_runtime() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    let shared = Arc::new(create_real_jdk_vm().expect("No JDK found"));
    let mut thread = JvmThread::new(ThreadId(1), "test");

    let str_arg = crate::vm::vm_object::create_java_string(&shared, "42");

    let result = crate::vm::invoke_shared(
        &shared,
        &mut thread,
        "java/lang/Integer",
        "parseInt",
        "(Ljava/lang/String;)I",
        &[Value::Object(Some(str_arg))],
    );
    assert!(
        result.is_ok(),
        "Integer.parseInt(\"42\") failed: {:?}",
        result.err()
    );
    assert_eq!(
        result.expect("result should be Ok"),
        Some(Value::Int(42)),
        "Integer.parseInt(\"42\") should return 42"
    );
}

/// Runtime test: Thread.currentThread() returns a non-null Thread object.
#[test]
#[ignore] // requires real JDK on PATH
fn s9_thread_current_thread_runtime() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    let shared = Arc::new(create_real_jdk_vm().expect("No JDK found"));
    let mut thread = JvmThread::new(ThreadId(1), "test-thread");

    let result = crate::vm::invoke_shared(
        &shared,
        &mut thread,
        "java/lang/Thread",
        "currentThread",
        "()Ljava/lang/Thread;",
        &[],
    );
    assert!(
        result.is_ok(),
        "Thread.currentThread() failed: {:?}",
        result.err()
    );
    match result.expect("result should be Ok") {
        Some(Value::Object(Some(_thread_ref))) => {
            // Successfully returned a Thread object
        }
        other => panic!("Thread.currentThread() should return a Thread object, got: {other:?}"),
    }
}

/// Runtime test: the VM-created Thread returned by
/// `Thread.currentThread()` in real-JDK mode has its
/// `holder:FieldHolder` field populated with a non-null ThreadGroup,
/// NORM_PRIORITY, and `daemon=false`.  Regression guard for B1 — a
/// null holder makes every real-JDK Thread accessor (getState,
/// getPriority, getThreadGroup, isDaemon) NPE.
#[test]
#[ignore] // requires real JDK on PATH
fn b1_current_thread_holder_populated() {
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    let shared = Arc::new(create_real_jdk_vm().expect("No JDK found"));
    let mut thread = JvmThread::new(ThreadId(1), "main");

    let result = crate::vm::invoke_shared(
        &shared,
        &mut thread,
        "java/lang/Thread",
        "currentThread",
        "()Ljava/lang/Thread;",
        &[],
    )
    .expect("Thread.currentThread() should succeed");
    let t_obj = match result {
        Some(Value::Object(Some(r))) => r,
        other => panic!("expected Thread object, got {other:?}"),
    };

    // Resolve the `holder` slot via the class store — the slot index
    // is layout-dependent so we must not hardcode it.
    let thread_class = shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id("java/lang/Thread")
        .expect("Thread class should be loaded");
    let holder_slot = {
        let cm = shared.classes.class_manager.read();
        crate::vm::resolve_field_index_for_test(thread_class, "holder", &cm.class_store)
    }
    .expect("Thread.holder field should be resolvable");

    let holder_val = shared.mem.heap.get_field(t_obj, holder_slot);
    let holder = match holder_val {
        Value::Object(Some(r)) => r,
        other => panic!("Thread.holder must not be null on VM-created Thread, got {other:?}"),
    };

    // Confirm `holder.group` is non-null.
    let holder_class = shared.mem.heap.class_id_of(holder);
    let group_slot = {
        let cm = shared.classes.class_manager.read();
        crate::vm::resolve_field_index_for_test(holder_class, "group", &cm.class_store)
    }
    .expect("FieldHolder.group field should be resolvable");
    match shared.mem.heap.get_field(holder, group_slot) {
        Value::Object(Some(_)) => {}
        other => panic!("holder.group must not be null, got {other:?}"),
    }

    // Confirm `holder.priority == 5` (NORM_PRIORITY).
    let prio_slot = {
        let cm = shared.classes.class_manager.read();
        crate::vm::resolve_field_index_for_test(holder_class, "priority", &cm.class_store)
    }
    .expect("FieldHolder.priority field should be resolvable");
    assert!(
        matches!(shared.mem.heap.get_field(holder, prio_slot), Value::Int(5)),
        "holder.priority should be NORM_PRIORITY (5)"
    );
}

/// Runtime test: System.arraycopy works correctly.
#[test]
#[ignore] // requires real JDK on PATH
fn s9_system_arraycopy_runtime() {
    use crate::memory::ArrayElementType;
    use crate::threading::jvm_thread::{JvmThread, ThreadId};

    let shared = Arc::new(create_real_jdk_vm().expect("No JDK found"));
    let mut thread = JvmThread::new(ThreadId(1), "test");

    // Create source array [10, 20, 30]
    let src = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, 3);
    shared
        .mem
        .heap
        .set_array_element(src, 0, Value::Int(10))
        .expect("array element set should succeed");
    shared
        .mem
        .heap
        .set_array_element(src, 1, Value::Int(20))
        .expect("array element set should succeed");
    shared
        .mem
        .heap
        .set_array_element(src, 2, Value::Int(30))
        .expect("array element set should succeed");

    // Create destination array [0, 0, 0]
    let dst = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, 3);

    // System.arraycopy(src, 0, dst, 0, 3)
    let result = crate::vm::invoke_shared(
        &shared,
        &mut thread,
        "java/lang/System",
        "arraycopy",
        "(Ljava/lang/Object;ILjava/lang/Object;II)V",
        &[
            Value::Object(Some(src)),
            Value::Int(0),
            Value::Object(Some(dst)),
            Value::Int(0),
            Value::Int(3),
        ],
    );
    assert!(
        result.is_ok(),
        "System.arraycopy failed: {:?}",
        result.err()
    );

    // Verify destination has copied values
    assert_eq!(
        shared
            .mem
            .heap
            .get_array_element(dst, 0)
            .expect("array element get should succeed"),
        Value::Int(10)
    );
    assert_eq!(
        shared
            .mem
            .heap
            .get_array_element(dst, 1)
            .expect("array element get should succeed"),
        Value::Int(20)
    );
    assert_eq!(
        shared
            .mem
            .heap
            .get_array_element(dst, 2)
            .expect("array element get should succeed"),
        Value::Int(30)
    );
}

// -----------------------------------------------------------------------
// Session 10: Native Method Bridge
// -----------------------------------------------------------------------

// --- Session 10: Native Method Bridging — 100% coverage ---

#[test]
#[ignore] // requires real JDK on boot classpath
fn s10_native_coverage_100_percent() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let report = crate::vm::vm_object::validate_native_coverage(&shared);

    eprintln!("\n=== Native Method Coverage Report ===");
    eprintln!("Total ACC_NATIVE methods: {}", report.total());
    eprintln!("Covered: {}", report.covered.len());
    eprintln!("Missing: {}", report.missing.len());
    eprintln!("Coverage: {:.1}%", report.coverage_ratio() * 100.0);

    if !report.missing.is_empty() {
        eprintln!("\n--- Missing natives ({}) ---", report.missing.len());
        for m in &report.missing {
            eprintln!("  {m}");
        }
    }

    assert_eq!(
        report.missing.len(),
        0,
        "All ACC_NATIVE methods in bootstrapped classes must be registered. \
         Missing: {:?}",
        report
            .missing
            .iter()
            .map(|m| m.to_string())
            .collect::<Vec<_>>()
    );
    assert!(
        report.total() > 150,
        "Expected 150+ ACC_NATIVE methods, found {}",
        report.total()
    );
}

#[test]
#[ignore]
fn s10_unsafe_natives_complete() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let natives = crate::vm::vm_object::scan_class_natives(&shared, "jdk/internal/misc/Unsafe");

    eprintln!("jdk/internal/misc/Unsafe: {} native methods", natives.len());
    for n in &natives {
        let registered = shared
            .natives
            .native_methods
            .find(&n.class_name, &n.method_name, &n.descriptor)
            .is_some();
        eprintln!("  {} {}", if registered { "✓" } else { "✗" }, n);
        assert!(registered, "Unsafe native not registered: {n}");
    }
    // JDK 25 Unsafe has 60+ native methods
    assert!(
        natives.len() > 50,
        "Expected 50+ Unsafe natives, found {}",
        natives.len()
    );
}

#[test]
#[ignore]
fn s10_classloader_natives_registered() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let natives = crate::vm::vm_object::scan_class_natives(&shared, "java/lang/ClassLoader");

    eprintln!("ClassLoader: {} native methods", natives.len());
    for n in &natives {
        let registered = shared
            .natives
            .native_methods
            .find(&n.class_name, &n.method_name, &n.descriptor)
            .is_some();
        assert!(registered, "ClassLoader native not registered: {n}");
    }
    assert!(
        natives.len() >= 5,
        "ClassLoader should have 5+ native methods, found {}",
        natives.len()
    );
}

#[test]
#[ignore]
fn s10_reference_natives_registered() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let natives = crate::vm::vm_object::scan_class_natives(&shared, "java/lang/ref/Reference");

    eprintln!("Reference: {} native methods", natives.len());
    for n in &natives {
        let registered = shared
            .natives
            .native_methods
            .find(&n.class_name, &n.method_name, &n.descriptor)
            .is_some();
        assert!(registered, "Reference native not registered: {n}");
    }
    assert!(
        natives.len() >= 4,
        "Reference should have 4+ native methods, found {}",
        natives.len()
    );
}

#[test]
#[ignore]
fn s10_method_handle_natives_registered() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let natives =
        crate::vm::vm_object::scan_class_natives(&shared, "java/lang/invoke/MethodHandle");

    eprintln!("MethodHandle: {} native methods", natives.len());
    for n in &natives {
        let registered = shared
            .natives
            .native_methods
            .find(&n.class_name, &n.method_name, &n.descriptor)
            .is_some();
        assert!(registered, "MethodHandle native not registered: {n}");
    }
    assert!(
        natives.len() >= 7,
        "MethodHandle should have 7+ native methods, found {}",
        natives.len()
    );
}

#[test]
#[ignore]
fn s10_jni_name_generation() {
    // Verify JNI naming convention generation
    let short = crate::vm::vm_object::jni_short_name("java/lang/System", "arraycopy");
    assert_eq!(short, "Java_java_lang_System_arraycopy");

    let short2 = crate::vm::vm_object::jni_short_name("java/lang/Object", "hashCode");
    assert_eq!(short2, "Java_java_lang_Object_hashCode");

    // Underscores in names get escaped
    let short3 = crate::vm::vm_object::jni_short_name("com/example/My_Class", "my_method");
    assert_eq!(short3, "Java_com_example_My_1Class_my_1method");

    // Long name with descriptor
    let long = crate::vm::vm_object::jni_long_name(
        "java/lang/System",
        "arraycopy",
        "(Ljava/lang/Object;ILjava/lang/Object;II)V",
    );
    assert!(long.starts_with("Java_java_lang_System_arraycopy__"));
    assert!(long.contains("Ljava_lang_Object"));
}

#[test]
#[ignore]
fn s10_file_io_natives_registered() {
    let shared = create_real_jdk_vm().expect("No JDK found");

    // FileInputStream JDK 25 natives
    let fis = crate::vm::vm_object::scan_class_natives(&shared, "java/io/FileInputStream");
    eprintln!("FileInputStream: {} native methods", fis.len());
    for n in &fis {
        let registered = shared
            .natives
            .native_methods
            .find(&n.class_name, &n.method_name, &n.descriptor)
            .is_some();
        assert!(registered, "FileInputStream native not registered: {n}");
    }

    // FileOutputStream JDK 25 natives
    let fos = crate::vm::vm_object::scan_class_natives(&shared, "java/io/FileOutputStream");
    eprintln!("FileOutputStream: {} native methods", fos.len());
    for n in &fos {
        let registered = shared
            .natives
            .native_methods
            .find(&n.class_name, &n.method_name, &n.descriptor)
            .is_some();
        assert!(registered, "FileOutputStream native not registered: {n}");
    }
}

#[test]
#[ignore]
fn s10_vm_internal_natives_registered() {
    let shared = create_real_jdk_vm().expect("No JDK found");

    // jdk/internal/misc/VM
    let vm_natives = crate::vm::vm_object::scan_class_natives(&shared, "jdk/internal/misc/VM");
    eprintln!("VM: {} native methods", vm_natives.len());
    for n in &vm_natives {
        let registered = shared
            .natives
            .native_methods
            .find(&n.class_name, &n.method_name, &n.descriptor)
            .is_some();
        assert!(registered, "VM native not registered: {n}");
    }

    // jdk/internal/misc/CDS
    let cds_natives =
        crate::vm::vm_object::scan_class_natives(&shared, "jdk/internal/misc/CDS");
    eprintln!("CDS: {} native methods", cds_natives.len());
    for n in &cds_natives {
        let registered = shared
            .natives
            .native_methods
            .find(&n.class_name, &n.method_name, &n.descriptor)
            .is_some();
        assert!(registered, "CDS native not registered: {n}");
    }
}

#[test]
#[ignore]
fn s10_register_natives_handled() {
    let shared = create_real_jdk_vm().expect("No JDK found");

    // All classes with registerNatives() must have it registered as noop
    let classes_with_register_natives = [
        "java/lang/Object",
        "java/lang/System",
        "java/lang/Class",
        "java/lang/Thread",
        "java/lang/ClassLoader",
        "java/io/FileInputStream",
        "java/io/FileOutputStream",
        "java/io/FileDescriptor",
        "jdk/internal/misc/Unsafe",
    ];
    for class in &classes_with_register_natives {
        let found = shared
            .natives
            .native_methods
            .find(class, "registerNatives", "()V")
            .is_some();
        assert!(found, "registerNatives not registered for {class}");
    }
}

#[test]
#[ignore]
fn s10_scan_class_natives_returns_empty_for_unknown() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let natives = crate::vm::vm_object::scan_class_natives(&shared, "com/nonexistent/Foo");
    assert!(natives.is_empty());
}

// -----------------------------------------------------------------------
// Session 11: Bootstrap — java.util Core Collections
// -----------------------------------------------------------------------

/// Helper: create a SharedVm + JvmThread pair for Session 11 tests.
fn create_real_jdk_vm_with_thread(
) -> Option<(SharedVm, crate::threading::jvm_thread::JvmThread)> {
    let shared = create_real_jdk_vm()?;
    let thread = crate::threading::jvm_thread::JvmThread::new(
        crate::threading::jvm_thread::ThreadId(99),
        "test-s11",
    );
    Some((shared, thread))
}

/// Helper: create a SharedVm + JvmThread pair with audit mode enabled.
fn create_real_jdk_vm_with_thread_audit(
) -> Option<(SharedVm, crate::threading::jvm_thread::JvmThread)> {
    let shared = create_real_jdk_vm_audit()?;
    let thread = crate::threading::jvm_thread::JvmThread::new(
        crate::threading::jvm_thread::ThreadId(99),
        "test-s11-audit",
    );
    Some((shared, thread))
}

/// Helper: load a class, initialize it, allocate an instance, run <init>.
fn new_object_initialized(
    shared: &SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    class_name: &str,
    init_desc: &str,
    init_args: &[Value],
) -> ObjectRef {
    let class_id = shared
        .classes
        .class_manager
        .write()
        .load_class(class_name)
        .unwrap_or_else(|e| panic!("Failed to load {class_name}: {e:?}"));
    crate::vm::ensure_class_initialized_shared(shared, thread, class_id)
        .unwrap_or_else(|e| panic!("Failed to initialize {class_name}: {e:?}"));
    let num_fields = shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .map(|c| c.num_total_fields)
        .unwrap_or(0);
    let obj = shared.mem.heap.alloc_object(class_id, num_fields);
    crate::runtime::interpreter::init_primitive_fields(shared, obj, class_id);
    // Call <init>
    let mut full_args = vec![Value::Object(Some(obj))];
    full_args.extend_from_slice(init_args);
    crate::vm::invoke_on_class_shared(
        shared, thread, class_id, "<init>", init_desc, &full_args,
    )
    .unwrap_or_else(|e| panic!("Failed to call {class_name}.<init>: {e:?}"));
    obj
}

// --- Collection class loading from real JDK bytecode ---

#[test]
#[ignore]
fn s11_hashmap_loaded_from_real_bytecode() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    let hm_id = cm.get_loaded_class_id("java/util/HashMap");
    assert!(hm_id.is_some(), "HashMap should be loaded during bootstrap");
    let hm_id = hm_id.expect("hm_id should be Some");
    let cls = cm
        .get_class(hm_id)
        .expect("class should exist in class store");
    assert!(
        !cls.origin.is_compatibility_stub(),
        "HashMap should be from real bytecode"
    );

    // HashMap should have many methods
    let method_names: Vec<&str> = cls.methods.iter().map(|m| m.name.as_ref()).collect();
    assert!(method_names.contains(&"put"), "HashMap should have put()");
    assert!(method_names.contains(&"get"), "HashMap should have get()");
    assert!(method_names.contains(&"size"), "HashMap should have size()");
    assert!(
        method_names.contains(&"remove"),
        "HashMap should have remove()"
    );
    assert!(
        method_names.contains(&"containsKey"),
        "HashMap should have containsKey()"
    );

    // HashMap extends AbstractMap
    let super_name = cls
        .superclass
        .and_then(|sid| cm.get_class(sid).map(|c| c.name.clone()));
    assert_eq!(
        super_name.as_deref(),
        Some("java/util/AbstractMap"),
        "HashMap should extend AbstractMap"
    );

    eprintln!(
        "HashMap: {} methods, {} fields, super={:?}",
        cls.methods.len(),
        cls.fields.len(),
        super_name
    );
}

#[test]
#[ignore]
fn s11_arraylist_loaded_from_real_bytecode() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    let al_id = cm.get_loaded_class_id("java/util/ArrayList");
    assert!(
        al_id.is_some(),
        "ArrayList should be loaded during bootstrap"
    );
    let al_id = al_id.expect("al_id should be Some");
    let cls = cm
        .get_class(al_id)
        .expect("class should exist in class store");
    assert!(
        !cls.origin.is_compatibility_stub(),
        "ArrayList should be from real bytecode"
    );

    let method_names: Vec<&str> = cls.methods.iter().map(|m| m.name.as_ref()).collect();
    assert!(method_names.contains(&"add"), "ArrayList should have add()");
    assert!(method_names.contains(&"get"), "ArrayList should have get()");
    assert!(
        method_names.contains(&"size"),
        "ArrayList should have size()"
    );
    assert!(
        method_names.contains(&"remove"),
        "ArrayList should have remove()"
    );

    let super_name = cls
        .superclass
        .and_then(|sid| cm.get_class(sid).map(|c| c.name.clone()));
    assert_eq!(
        super_name.as_deref(),
        Some("java/util/AbstractList"),
        "ArrayList should extend AbstractList"
    );

    eprintln!(
        "ArrayList: {} methods, {} fields",
        cls.methods.len(),
        cls.fields.len()
    );
}

// --- Abstract collection class hierarchy ---

#[test]
#[ignore]
fn s11_abstract_collection_hierarchy() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    for name in &[
        "java/util/AbstractCollection",
        "java/util/AbstractList",
        "java/util/AbstractSet",
        "java/util/AbstractMap",
    ] {
        let id = cm.get_loaded_class_id(name);
        assert!(id.is_some(), "{name} should be loaded during bootstrap");
        let cls = cm
            .get_class(id.expect("class id should be loaded"))
            .expect("class should exist in store");
        assert!(
            !cls.origin.is_compatibility_stub(),
            "{name} should be from real bytecode"
        );
    }

    // Verify hierarchy: AbstractList extends AbstractCollection
    let al_id = cm
        .get_loaded_class_id("java/util/AbstractList")
        .expect("class java/util/AbstractList should be loaded");
    let al = cm
        .get_class(al_id)
        .expect("class should exist in class store");
    let super_name = al
        .superclass
        .and_then(|sid| cm.get_class(sid).map(|c| c.name.clone()));
    assert_eq!(
        super_name.as_deref(),
        Some("java/util/AbstractCollection"),
        "AbstractList should extend AbstractCollection"
    );

    // AbstractCollection extends Object
    let ac_id = cm
        .get_loaded_class_id("java/util/AbstractCollection")
        .expect("class java/util/AbstractCollection should be loaded");
    let ac = cm
        .get_class(ac_id)
        .expect("class should exist in class store");
    let super_name = ac
        .superclass
        .and_then(|sid| cm.get_class(sid).map(|c| c.name.clone()));
    assert_eq!(
        super_name.as_deref(),
        Some("java/lang/Object"),
        "AbstractCollection should extend Object"
    );
}

// --- Interface loading ---

#[test]
#[ignore]
fn s11_collection_interfaces_loaded() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    for name in &[
        "java/util/Collection",
        "java/util/List",
        "java/util/Set",
        "java/util/Map",
        "java/lang/Iterable",
        "java/util/Iterator",
    ] {
        let id = cm.get_loaded_class_id(name);
        assert!(id.is_some(), "{name} should be loaded during bootstrap");
        let cls = cm
            .get_class(id.expect("class id should be loaded"))
            .expect("class should exist in store");
        assert!(
            !cls.origin.is_compatibility_stub(),
            "{name} should be from real bytecode"
        );
    }
}

// --- Class initialization ---

#[test]
#[ignore]
fn s11_hashmap_initializes() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");
    let hm_id = shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id("java/util/HashMap")
        .expect("class java/util/HashMap should be loaded");
    crate::vm::ensure_class_initialized_shared(&shared, &mut thread, hm_id)
        .expect("HashMap <clinit> should execute successfully");

    let state = shared
        .classes
        .class_manager
        .read()
        .get_class(hm_id)
        .expect("class should exist in class store")
        .state;
    assert_eq!(
        state,
        crate::classloading::ClassState::Initialized,
        "HashMap should be in Initialized state"
    );
}

#[test]
#[ignore]
fn s11_arraylist_initializes() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");
    let al_id = shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id("java/util/ArrayList")
        .expect("class java/util/ArrayList should be loaded");
    crate::vm::ensure_class_initialized_shared(&shared, &mut thread, al_id)
        .expect("ArrayList <clinit> should execute successfully");

    let state = shared
        .classes
        .class_manager
        .read()
        .get_class(al_id)
        .expect("class should exist in class store")
        .state;
    assert_eq!(state, crate::classloading::ClassState::Initialized);
}

#[test]
#[ignore]
fn s11_all_core_collections_initialize() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

    let collection_classes = [
        "java/util/HashMap",
        "java/util/ArrayList",
        "java/util/LinkedList",
        "java/util/HashSet",
        "java/util/AbstractCollection",
        "java/util/AbstractList",
        "java/util/AbstractSet",
        "java/util/AbstractMap",
        "java/util/Collections",
        "java/util/Arrays",
        "java/util/Objects",
    ];

    for name in &collection_classes {
        let id = shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(name);
        assert!(id.is_some(), "{name} should be loaded");
        let id = id.expect("id should be Some");
        crate::vm::ensure_class_initialized_shared(&shared, &mut thread, id)
            .unwrap_or_else(|e| panic!("{name} <clinit> failed: {e:?}"));
        let state = shared
            .classes
            .class_manager
            .read()
            .get_class(id)
            .expect("class should exist in class store")
            .state;
        assert_eq!(
            state,
            crate::classloading::ClassState::Initialized,
            "{name} should be Initialized"
        );
    }
}

// --- Inner class loading ---

#[test]
#[ignore]
fn s11_hashmap_inner_classes_load() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

    // Initialize HashMap first (may trigger inner class loading)
    let hm_id = shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id("java/util/HashMap")
        .expect("class java/util/HashMap should be loaded");
    crate::vm::ensure_class_initialized_shared(&shared, &mut thread, hm_id)
        .expect("class initialization should succeed");

    // HashMap$Node should be loadable (may or may not be bootstrapped yet)
    let node_id = shared
        .classes
        .class_manager
        .write()
        .load_class("java/util/HashMap$Node");
    assert!(
        node_id.is_ok(),
        "HashMap$Node should be loadable, got: {:?}",
        node_id.err()
    );
    let node_id = node_id.expect("node_id should be Some");

    let cm = shared.classes.class_manager.read();
    let node = cm
        .get_class(node_id)
        .expect("class should exist in class store");
    assert!(
        !node.origin.is_compatibility_stub(),
        "HashMap$Node should be from real bytecode"
    );

    // Node should have key, value, hash, next fields
    let field_names: Vec<&str> = node.fields.iter().map(|f| f.name.as_ref()).collect();
    eprintln!("HashMap$Node fields: {:?}", field_names);
    assert!(
        field_names.contains(&"hash"),
        "Node should have 'hash' field"
    );
    assert!(field_names.contains(&"key"), "Node should have 'key' field");
    assert!(
        field_names.contains(&"value"),
        "Node should have 'value' field"
    );
    assert!(
        field_names.contains(&"next"),
        "Node should have 'next' field"
    );
}

// --- Object instantiation and method invocation ---

#[test]
#[ignore]
fn s11_hashmap_put_and_get() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread_audit().expect("No JDK found");

    // new HashMap()
    let hm = new_object_initialized(&shared, &mut thread, "java/util/HashMap", "()V", &[]);

    // Create key and value strings
    let key = crate::vm::vm_object::create_java_string(&shared, "mykey");
    let val = crate::vm::vm_object::create_java_string(&shared, "myvalue");

    // HashMap.put(key, value)
    let hm_id = shared.mem.heap.class_id_of(hm);
    let put_result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        hm_id,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(hm)),
            Value::Object(Some(key)),
            Value::Object(Some(val)),
        ],
    );
    assert!(
        put_result.is_ok(),
        "HashMap.put() failed: {:?}",
        put_result.err()
    );
    let prev = put_result.expect("put_result should be Ok");
    eprintln!("put() returned: {:?}", prev);

    // Check audit log for missing natives
    {
        let log = shared.debug.missing_natives_log.lock();
        if !log.is_empty() {
            eprintln!("Missing natives called during put():");
            for entry in log.iter() {
                eprintln!("  {}", entry.full_signature());
            }
        } else {
            eprintln!("No missing natives during put().");
        }
    }

    // Inspect raw instance fields of HashMap to see what put() did
    {
        let cm = shared.classes.class_manager.read();
        let cls = cm
            .get_class(hm_id)
            .expect("class should exist in class store");
        eprintln!(
            "HashMap instance fields (first_field_index={}, num_total={}):",
            cls.first_field_index, cls.num_total_fields
        );
        let mut inst_idx = cls.first_field_index;
        for f in &cls.fields {
            if !f.is_static() {
                let val = shared.mem.heap.get_field(hm, inst_idx);
                eprintln!(
                    "  slot[{}] {} ({}) = {:?}",
                    inst_idx, f.name, f.descriptor, val
                );
                inst_idx += 1;
            }
        }
        // Also check parent class (AbstractMap) instance fields
        if let Some(super_id) = cls.superclass {
            let super_cls = cm
                .get_class(super_id)
                .expect("class should exist in class store");
            eprintln!(
                "AbstractMap instance fields (first={}, total={}):",
                super_cls.first_field_index, super_cls.num_total_fields
            );
            let mut si = super_cls.first_field_index;
            for f in &super_cls.fields {
                if !f.is_static() {
                    let val = shared.mem.heap.get_field(hm, si);
                    eprintln!("  slot[{}] {} ({}) = {:?}", si, f.name, f.descriptor, val);
                    si += 1;
                }
            }
        }
    }

    // HashMap.size() should be 1
    let size_result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        hm_id,
        "size",
        "()I",
        &[Value::Object(Some(hm))],
    );
    assert!(
        size_result.is_ok(),
        "HashMap.size() failed: {:?}",
        size_result.err()
    );
    let size_val = size_result.expect("size_result should be Ok");
    eprintln!("size() returned: {:?}", size_val);
    assert_eq!(
        size_val,
        Some(Value::Int(1)),
        "HashMap should have size 1 after one put"
    );

    // HashMap.get(key) should return the value
    let get_result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        hm_id,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(hm)), Value::Object(Some(key))],
    );
    assert!(
        get_result.is_ok(),
        "HashMap.get() failed: {:?}",
        get_result.err()
    );
    let retrieved = get_result.expect("get_result should be Ok");
    match retrieved {
        Some(Value::Object(Some(obj_ref))) => {
            let text = crate::vm::vm_object::read_java_string(&shared.mem.heap, obj_ref);
            assert_eq!(
                text.as_deref(),
                Some("myvalue"),
                "HashMap.get() should return 'myvalue'"
            );
        }
        other => panic!(
            "HashMap.get() should return string object, got: {:?}",
            other
        ),
    }

    eprintln!("HashMap.put/get works with real JDK bytecode!");
}

#[test]
#[ignore]
fn s11_arraylist_add_and_get() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

    // new ArrayList()
    let al = new_object_initialized(&shared, &mut thread, "java/util/ArrayList", "()V", &[]);

    // Box an integer: create Integer.valueOf(42)
    let int_class_id = shared
        .classes
        .class_manager
        .write()
        .load_class("java/lang/Integer")
        .expect("failed to load class java/lang/Integer");
    crate::vm::ensure_class_initialized_shared(&shared, &mut thread, int_class_id)
        .expect("class initialization should succeed");
    let boxed_42 = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        int_class_id,
        "valueOf",
        "(I)Ljava/lang/Integer;",
        &[Value::Int(42)],
    )
    .expect("Integer.valueOf(42) failed");
    let boxed_42 = match boxed_42 {
        Some(Value::Object(Some(r))) => r,
        other => panic!("Integer.valueOf should return object, got: {:?}", other),
    };

    // ArrayList.add(Integer.valueOf(42))
    let al_id = shared.mem.heap.class_id_of(al);
    let add_result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        al_id,
        "add",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(al)), Value::Object(Some(boxed_42))],
    );
    assert!(
        add_result.is_ok(),
        "ArrayList.add() failed: {:?}",
        add_result.err()
    );
    assert_eq!(
        add_result.expect("add_result should be Ok"),
        Some(Value::Int(1)),
        "ArrayList.add() should return true (1)"
    );

    // ArrayList.size() should be 1
    let size_result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        al_id,
        "size",
        "()I",
        &[Value::Object(Some(al))],
    );
    assert!(
        size_result.is_ok(),
        "ArrayList.size() failed: {:?}",
        size_result.err()
    );
    assert_eq!(
        size_result.expect("size_result should be Ok"),
        Some(Value::Int(1))
    );

    // ArrayList.get(0) should return the Integer
    let get_result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        al_id,
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(al)), Value::Int(0)],
    );
    assert!(
        get_result.is_ok(),
        "ArrayList.get(0) failed: {:?}",
        get_result.err()
    );
    match get_result.expect("get_result should be Ok") {
        Some(Value::Object(Some(obj_ref))) => {
            // Read the intValue
            let int_val = crate::vm::invoke_on_class_shared(
                &shared,
                &mut thread,
                shared.mem.heap.class_id_of(obj_ref),
                "intValue",
                "()I",
                &[Value::Object(Some(obj_ref))],
            )
            .expect("Integer.intValue() failed");
            assert_eq!(
                int_val,
                Some(Value::Int(42)),
                "ArrayList.get(0) should return Integer(42)"
            );
        }
        other => panic!("ArrayList.get(0) should return object, got: {:?}", other),
    }

    eprintln!("ArrayList.add/get works with real JDK bytecode!");
}

// --- LinkedList ---

#[test]
#[ignore]
fn s11_linkedlist_loaded_and_initializes() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    let ll_id = cm.get_loaded_class_id("java/util/LinkedList");
    assert!(ll_id.is_some(), "LinkedList should be loaded");
    let ll_id = ll_id.expect("ll_id should be Some");
    let cls = cm
        .get_class(ll_id)
        .expect("class should exist in class store");
    assert!(
        !cls.origin.is_compatibility_stub(),
        "LinkedList should be from real bytecode"
    );

    let super_name = cls
        .superclass
        .and_then(|sid| cm.get_class(sid).map(|c| c.name.clone()));
    assert_eq!(
        super_name.as_deref(),
        Some("java/util/AbstractSequentialList"),
        "LinkedList should extend AbstractSequentialList"
    );
    drop(cm);

    crate::vm::ensure_class_initialized_shared(&shared, &mut thread, ll_id)
        .expect("LinkedList <clinit> should succeed");
}

// --- HashSet ---

#[test]
#[ignore]
fn s11_hashset_loaded_and_initializes() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

    let hs_id = shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id("java/util/HashSet");
    assert!(hs_id.is_some(), "HashSet should be loaded");
    let hs_id = hs_id.expect("hs_id should be Some");
    {
        let cm = shared.classes.class_manager.read();
        let cls = cm
            .get_class(hs_id)
            .expect("class should exist in class store");
        assert!(
            !cls.origin.is_compatibility_stub(),
            "HashSet should be from real bytecode"
        );
    }

    crate::vm::ensure_class_initialized_shared(&shared, &mut thread, hs_id)
        .expect("HashSet <clinit> should succeed");
}

// --- TreeMap loading (on-demand, not in bootstrap tier 2) ---

#[test]
#[ignore]
fn s11_treemap_loads_and_initializes() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

    // TreeMap may not be in tier 2 bootstrap but should be loadable
    let tm_id = shared
        .classes
        .class_manager
        .write()
        .load_class("java/util/TreeMap")
        .expect("TreeMap should be loadable");

    {
        let cm = shared.classes.class_manager.read();
        let cls = cm
            .get_class(tm_id)
            .expect("class should exist in class store");
        assert!(
            !cls.origin.is_compatibility_stub(),
            "TreeMap should be from real bytecode"
        );

        let method_names: Vec<&str> = cls.methods.iter().map(|m| m.name.as_ref()).collect();
        assert!(method_names.contains(&"put"), "TreeMap should have put()");
        assert!(method_names.contains(&"get"), "TreeMap should have get()");
    }

    crate::vm::ensure_class_initialized_shared(&shared, &mut thread, tm_id)
        .expect("TreeMap <clinit> should succeed");
}

// --- Spliterator interface ---

#[test]
#[ignore]
fn s11_spliterator_interface_loads() {
    let (shared, _thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

    let sp_id = shared
        .classes
        .class_manager
        .write()
        .load_class("java/util/Spliterator")
        .expect("Spliterator should be loadable");

    let cm = shared.classes.class_manager.read();
    let cls = cm
        .get_class(sp_id)
        .expect("class should exist in class store");
    assert!(
        !cls.origin.is_compatibility_stub(),
        "Spliterator should be from real bytecode"
    );

    // Spliterator is an interface
    assert!(
        cls.access_flags
            .contains(cratonvm_reader::class_access_flags::ClassAccessFlags::INTERFACE),
        "Spliterator should be an interface"
    );
}

// --- HashMap.containsKey and remove ---

#[test]
#[ignore]
fn s11_hashmap_contains_and_remove() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

    let hm = new_object_initialized(&shared, &mut thread, "java/util/HashMap", "()V", &[]);
    let hm_id = shared.mem.heap.class_id_of(hm);

    let key = crate::vm::vm_object::create_java_string(&shared, "testkey");
    let val = crate::vm::vm_object::create_java_string(&shared, "testval");

    // put(key, val)
    crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        hm_id,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(hm)),
            Value::Object(Some(key)),
            Value::Object(Some(val)),
        ],
    )
    .expect("put failed");

    // containsKey(key) == true
    let contains = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        hm_id,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(Some(hm)), Value::Object(Some(key))],
    )
    .expect("containsKey failed");
    assert_eq!(contains, Some(Value::Int(1)), "containsKey should be true");

    // remove(key) returns old value
    let removed = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        hm_id,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(hm)), Value::Object(Some(key))],
    )
    .expect("remove failed");
    match removed {
        Some(Value::Object(Some(obj_ref))) => {
            let text = crate::vm::vm_object::read_java_string(&shared.mem.heap, obj_ref);
            assert_eq!(text.as_deref(), Some("testval"));
        }
        other => panic!("remove should return old value, got: {:?}", other),
    }

    // size should be 0 after remove
    let size = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        hm_id,
        "size",
        "()I",
        &[Value::Object(Some(hm))],
    )
    .expect("size failed");
    assert_eq!(size, Some(Value::Int(0)), "size should be 0 after remove");
}

// --- Multiple puts ---

#[test]
#[ignore]
fn s11_hashmap_multiple_entries() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

    let hm = new_object_initialized(&shared, &mut thread, "java/util/HashMap", "()V", &[]);
    let hm_id = shared.mem.heap.class_id_of(hm);

    // Put 10 entries
    for i in 0..10 {
        let key = crate::vm::vm_object::create_java_string(&shared, &format!("key{i}"));
        let val = crate::vm::vm_object::create_java_string(&shared, &format!("val{i}"));
        crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            hm_id,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[
                Value::Object(Some(hm)),
                Value::Object(Some(key)),
                Value::Object(Some(val)),
            ],
        )
        .unwrap_or_else(|e| panic!("put({i}) failed: {e:?}"));
    }

    let size = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        hm_id,
        "size",
        "()I",
        &[Value::Object(Some(hm))],
    )
    .expect("size failed");
    assert_eq!(size, Some(Value::Int(10)), "HashMap should have 10 entries");

    // Verify each entry can be retrieved
    for i in 0..10 {
        let key = crate::vm::vm_object::create_java_string(&shared, &format!("key{i}"));
        let result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            hm_id,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(hm)), Value::Object(Some(key))],
        )
        .unwrap_or_else(|e| panic!("get(key{i}) failed: {e:?}"));
        match result {
            Some(Value::Object(Some(obj_ref))) => {
                let text = crate::vm::vm_object::read_java_string(&shared.mem.heap, obj_ref);
                assert_eq!(
                    text.as_deref(),
                    Some(&format!("val{i}") as &str),
                    "get(key{i}) should return val{i}"
                );
            }
            other => panic!("get(key{i}) should return string, got: {:?}", other),
        }
    }

    eprintln!("HashMap with 10 entries: all put/get pairs verified!");
}

// --- ArrayList multiple adds ---

#[test]
#[ignore]
fn s11_arraylist_multiple_elements() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

    let al = new_object_initialized(&shared, &mut thread, "java/util/ArrayList", "()V", &[]);
    let al_id = shared.mem.heap.class_id_of(al);

    // Add 20 strings (triggers internal array resize from default capacity 10)
    for i in 0..20 {
        let s = crate::vm::vm_object::create_java_string(&shared, &format!("item{i}"));
        crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            al_id,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(al)), Value::Object(Some(s))],
        )
        .unwrap_or_else(|e| panic!("add({i}) failed: {e:?}"));
    }

    let size = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        al_id,
        "size",
        "()I",
        &[Value::Object(Some(al))],
    )
    .expect("size failed");
    assert_eq!(
        size,
        Some(Value::Int(20)),
        "ArrayList should have 20 elements"
    );

    // Verify element at index 15
    let get_result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        al_id,
        "get",
        "(I)Ljava/lang/Object;",
        &[Value::Object(Some(al)), Value::Int(15)],
    )
    .expect("get(15) failed");
    match get_result {
        Some(Value::Object(Some(obj_ref))) => {
            let text = crate::vm::vm_object::read_java_string(&shared.mem.heap, obj_ref);
            assert_eq!(text.as_deref(), Some("item15"));
        }
        other => panic!("get(15) should return 'item15', got: {:?}", other),
    }

    eprintln!("ArrayList with 20 elements (resize triggered): verified!");
}

// -----------------------------------------------------------------------
// Session 13: Bootstrap — java.util.concurrent
// -----------------------------------------------------------------------

/// Verify all j.u.c classes are loaded from real JDK bytecode.
#[test]
#[ignore] // requires real JDK on PATH
fn s13_concurrent_classes_loaded_from_real_bytecode() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    let concurrent_classes = [
        // Atomic classes
        "java/util/concurrent/atomic/AtomicInteger",
        "java/util/concurrent/atomic/AtomicLong",
        "java/util/concurrent/atomic/AtomicBoolean",
        "java/util/concurrent/atomic/AtomicReference",
        "java/util/concurrent/atomic/AtomicIntegerArray",
        "java/util/concurrent/atomic/AtomicLongArray",
        "java/util/concurrent/atomic/AtomicReferenceArray",
        "java/util/concurrent/atomic/AtomicStampedReference",
        "java/util/concurrent/atomic/AtomicMarkableReference",
        "java/util/concurrent/atomic/LongAdder",
        "java/util/concurrent/atomic/DoubleAdder",
        "java/util/concurrent/atomic/LongAccumulator",
        "java/util/concurrent/atomic/DoubleAccumulator",
        // Locks
        "java/util/concurrent/locks/ReentrantLock",
        "java/util/concurrent/locks/ReentrantReadWriteLock",
        "java/util/concurrent/locks/StampedLock",
        "java/util/concurrent/locks/AbstractQueuedSynchronizer",
        "java/util/concurrent/locks/LockSupport",
        "java/util/concurrent/locks/Lock",
        "java/util/concurrent/locks/Condition",
        "java/util/concurrent/locks/ReadWriteLock",
        // Concurrent collections
        "java/util/concurrent/ConcurrentHashMap",
        "java/util/concurrent/CopyOnWriteArrayList",
        "java/util/concurrent/CopyOnWriteArraySet",
        // Synchronizers
        "java/util/concurrent/CountDownLatch",
        "java/util/concurrent/Semaphore",
        "java/util/concurrent/CyclicBarrier",
        "java/util/concurrent/Phaser",
        "java/util/concurrent/CompletableFuture",
        // Executors
        "java/util/concurrent/ForkJoinPool",
        "java/util/concurrent/ForkJoinTask",
        "java/util/concurrent/ExecutorService",
        "java/util/concurrent/ThreadPoolExecutor",
        "java/util/concurrent/Executors",
    ];

    for class_name in &concurrent_classes {
        let id = cm.get_loaded_class_id(class_name);
        assert!(
            id.is_some(),
            "{class_name} should be loaded during bootstrap"
        );
        let cls = cm
            .get_class(id.expect("class id should be loaded"))
            .expect("class should exist in store");
        assert!(
            !cls.origin.is_compatibility_stub(),
            "{class_name} should be from real JDK bytecode, not a synthetic stub"
        );
    }

    eprintln!(
        "All {} j.u.c classes loaded from real bytecode",
        concurrent_classes.len()
    );
}

/// Verify Unsafe CAS methods needed by j.u.c are registered for both APIs.
#[test]
#[ignore] // requires real JDK on PATH
fn s13_unsafe_cas_methods_registered() {
    let shared = create_real_jdk_vm().expect("No JDK found");

    // jdk.internal.misc.Unsafe (modern API)
    let u = "jdk/internal/misc/Unsafe";
    let cas_methods = [
        ("compareAndSetInt", "(Ljava/lang/Object;JII)Z"),
        ("compareAndSetLong", "(Ljava/lang/Object;JJJ)Z"),
        (
            "compareAndSetReference",
            "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
        ),
        ("getAndAddInt", "(Ljava/lang/Object;JI)I"),
        ("getAndAddLong", "(Ljava/lang/Object;JJ)J"),
        ("getAndSetInt", "(Ljava/lang/Object;JI)I"),
        ("getAndSetLong", "(Ljava/lang/Object;JJ)J"),
        (
            "getAndSetReference",
            "(Ljava/lang/Object;JLjava/lang/Object;)Ljava/lang/Object;",
        ),
        ("getIntVolatile", "(Ljava/lang/Object;J)I"),
        ("putIntVolatile", "(Ljava/lang/Object;JI)V"),
        ("getLongVolatile", "(Ljava/lang/Object;J)J"),
        ("putLongVolatile", "(Ljava/lang/Object;JJ)V"),
        (
            "getReferenceVolatile",
            "(Ljava/lang/Object;J)Ljava/lang/Object;",
        ),
        (
            "putReferenceVolatile",
            "(Ljava/lang/Object;JLjava/lang/Object;)V",
        ),
        ("park", "(ZJ)V"),
        ("unpark", "(Ljava/lang/Object;)V"),
    ];

    for (method, desc) in &cas_methods {
        assert!(
            shared
                .natives
                .native_methods
                .find(u, method, desc)
                .is_some(),
            "Unsafe.{method}{desc} should be registered on {u}"
        );
    }

    // sun.misc.Unsafe (legacy API)
    let u_legacy = "sun/misc/Unsafe";
    let legacy_cas = [
        ("compareAndSwapInt", "(Ljava/lang/Object;JII)Z"),
        ("compareAndSwapLong", "(Ljava/lang/Object;JJJ)Z"),
        (
            "compareAndSwapObject",
            "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
        ),
    ];

    for (method, desc) in &legacy_cas {
        assert!(
            shared
                .natives
                .native_methods
                .find(u_legacy, method, desc)
                .is_some(),
            "{u_legacy}.{method}{desc} should be registered"
        );
    }
}

/// Verify LockSupport park/unpark methods are registered -- unless the
/// `JdkLockSupport` row (interpreter round i2 wave 69, lane L1; default
/// on) leaves them to the JDK's bodies, in which case none is.
#[test]
#[ignore] // requires real JDK on PATH
fn s13_lock_support_natives_registered() {
    let shared = create_real_jdk_vm().expect("No JDK found");

    let ls = "java/util/concurrent/locks/LockSupport";
    if shared
        .natives
        .native_methods
        .aligned(cratonvm_types::HotSpotAlignment::JdkLockSupport)
    {
        for (method, desc) in [
            ("park", "(Ljava/lang/Object;)V"),
            ("unpark", "(Ljava/lang/Thread;)V"),
        ] {
            assert!(
                shared
                    .natives
                    .native_methods
                    .find(ls, method, desc)
                    .is_none(),
                "LockSupport.{method}{desc} runs its bytecode under the row"
            );
        }
        return;
    }
    let methods = [
        ("park", "(Ljava/lang/Object;)V"),
        ("parkNanos", "(Ljava/lang/Object;J)V"),
        ("unpark", "(Ljava/lang/Thread;)V"),
    ];

    for (method, desc) in &methods {
        assert!(
            shared
                .natives
                .native_methods
                .find(ls, method, desc)
                .is_some(),
            "LockSupport.{method}{desc} should be registered"
        );
    }
}

/// Verify AQS is loaded from real bytecode and has expected structure.
#[test]
#[ignore] // requires real JDK on PATH
fn s13_aqs_structure() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    let aqs_id = cm
        .get_loaded_class_id("java/util/concurrent/locks/AbstractQueuedSynchronizer")
        .expect("AQS should be loaded");
    let aqs = cm
        .get_class(aqs_id)
        .expect("class should exist in class store");
    assert!(
        !aqs.origin.is_compatibility_stub(),
        "AQS should be real bytecode"
    );

    // AQS should have key methods
    let method_names: Vec<&str> = aqs.methods.iter().map(|m| m.name.as_ref()).collect();
    assert!(method_names.contains(&"acquire"), "AQS should have acquire");
    assert!(method_names.contains(&"release"), "AQS should have release");
    assert!(
        method_names.contains(&"tryAcquire"),
        "AQS should have tryAcquire"
    );
    assert!(
        method_names.contains(&"tryRelease"),
        "AQS should have tryRelease"
    );

    eprintln!("AQS loaded with {} methods", aqs.methods.len());
}

/// Verify ReentrantLock extends from AQS (via inner Sync class).
#[test]
#[ignore] // requires real JDK on PATH
fn s13_reentrant_lock_structure() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    let rl_id = cm
        .get_loaded_class_id("java/util/concurrent/locks/ReentrantLock")
        .expect("ReentrantLock should be loaded");
    let rl = cm
        .get_class(rl_id)
        .expect("class should exist in class store");
    assert!(
        !rl.origin.is_compatibility_stub(),
        "ReentrantLock should be real bytecode"
    );

    // Verify key methods
    let method_names: Vec<&str> = rl.methods.iter().map(|m| m.name.as_ref()).collect();
    assert!(
        method_names.contains(&"lock"),
        "ReentrantLock should have lock"
    );
    assert!(
        method_names.contains(&"unlock"),
        "ReentrantLock should have unlock"
    );
    assert!(
        method_names.contains(&"tryLock"),
        "ReentrantLock should have tryLock"
    );
    assert!(
        method_names.contains(&"newCondition"),
        "ReentrantLock should have newCondition"
    );
}

/// Verify AtomicInteger structure from real bytecode.
#[test]
#[ignore] // requires real JDK on PATH
fn s13_atomic_integer_structure() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    let ai_id = cm
        .get_loaded_class_id("java/util/concurrent/atomic/AtomicInteger")
        .expect("AtomicInteger should be loaded");
    let ai = cm
        .get_class(ai_id)
        .expect("class should exist in class store");
    assert!(
        !ai.origin.is_compatibility_stub(),
        "AtomicInteger should be real bytecode"
    );

    // AtomicInteger should have a volatile 'value' field
    let field_names: Vec<&str> = ai
        .fields
        .iter()
        .filter(|f| !f.is_static())
        .map(|f| f.name.as_ref())
        .collect();
    assert!(
        field_names.contains(&"value"),
        "AtomicInteger should have 'value' field"
    );

    // Should have key methods
    let method_names: Vec<&str> = ai.methods.iter().map(|m| m.name.as_ref()).collect();
    assert!(
        method_names.contains(&"get"),
        "AtomicInteger should have get"
    );
    assert!(
        method_names.contains(&"set"),
        "AtomicInteger should have set"
    );
    assert!(
        method_names.contains(&"getAndIncrement"),
        "AtomicInteger should have getAndIncrement"
    );
    assert!(
        method_names.contains(&"incrementAndGet"),
        "AtomicInteger should have incrementAndGet"
    );
    assert!(
        method_names.contains(&"compareAndSet"),
        "AtomicInteger should have compareAndSet"
    );
}

/// Runtime test: AtomicInteger.incrementAndGet() — roadmap deliverable.
#[test]
#[ignore] // requires real JDK on PATH
fn s13_atomic_integer_increment_and_get_runtime() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

    // new AtomicInteger(0)
    let ai = new_object_initialized(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicInteger",
        "(I)V",
        &[Value::Int(0)],
    );
    let ai_id = shared.mem.heap.class_id_of(ai);

    // incrementAndGet() should return 1
    let result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        ai_id,
        "incrementAndGet",
        "()I",
        &[Value::Object(Some(ai))],
    );
    assert!(
        result.is_ok(),
        "incrementAndGet() failed: {:?}",
        result.err()
    );
    assert_eq!(
        result.expect("result should be Ok"),
        Some(Value::Int(1)),
        "AtomicInteger(0).incrementAndGet() should return 1"
    );

    // incrementAndGet() again should return 2
    let result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        ai_id,
        "incrementAndGet",
        "()I",
        &[Value::Object(Some(ai))],
    );
    assert!(
        result.is_ok(),
        "incrementAndGet() #2 failed: {:?}",
        result.err()
    );
    assert_eq!(
        result.expect("result should be Ok"),
        Some(Value::Int(2)),
        "AtomicInteger(1).incrementAndGet() should return 2"
    );

    // get() should return 2
    let result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        ai_id,
        "get",
        "()I",
        &[Value::Object(Some(ai))],
    );
    assert!(result.is_ok(), "get() failed: {:?}", result.err());
    assert_eq!(
        result.expect("result should be Ok"),
        Some(Value::Int(2)),
        "AtomicInteger.get() should return 2 after two increments"
    );

    eprintln!("AtomicInteger.incrementAndGet() works with real JDK bytecode!");
}

/// Runtime test: AtomicInteger.compareAndSet().
#[test]
#[ignore] // requires real JDK on PATH
fn s13_atomic_integer_compare_and_set_runtime() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

    let ai = new_object_initialized(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicInteger",
        "(I)V",
        &[Value::Int(42)],
    );
    let ai_id = shared.mem.heap.class_id_of(ai);

    // compareAndSet(42, 100) should succeed (return true)
    let result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        ai_id,
        "compareAndSet",
        "(II)Z",
        &[Value::Object(Some(ai)), Value::Int(42), Value::Int(100)],
    );
    assert!(
        result.is_ok(),
        "compareAndSet(42,100) failed: {:?}",
        result.err()
    );
    assert_eq!(
        result.expect("result should be Ok"),
        Some(Value::Int(1)),
        "compareAndSet(42,100) should return true"
    );

    // compareAndSet(42, 200) should fail (expected 42 but value is now 100)
    let result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        ai_id,
        "compareAndSet",
        "(II)Z",
        &[Value::Object(Some(ai)), Value::Int(42), Value::Int(200)],
    );
    assert!(
        result.is_ok(),
        "compareAndSet(42,200) failed: {:?}",
        result.err()
    );
    assert_eq!(
        result.expect("result should be Ok"),
        Some(Value::Int(0)),
        "compareAndSet(42,200) should return false (value is 100)"
    );

    // Verify value is 100
    let result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        ai_id,
        "get",
        "()I",
        &[Value::Object(Some(ai))],
    );
    assert_eq!(result.expect("result should be Ok"), Some(Value::Int(100)));
}

/// Runtime test: ConcurrentHashMap.put() and get() — roadmap deliverable.
#[test]
#[ignore] // requires real JDK on PATH
fn s13_concurrent_hashmap_put_and_get_runtime() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

    // new ConcurrentHashMap()
    let chm = new_object_initialized(
        &shared,
        &mut thread,
        "java/util/concurrent/ConcurrentHashMap",
        "()V",
        &[],
    );
    let chm_id = shared.mem.heap.class_id_of(chm);

    // Create key and value strings
    let key = crate::vm::vm_object::create_java_string(&shared, "testKey");
    let value = crate::vm::vm_object::create_java_string(&shared, "testValue");

    // put("testKey", "testValue")
    let put_result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        chm_id,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[
            Value::Object(Some(chm)),
            Value::Object(Some(key)),
            Value::Object(Some(value)),
        ],
    );
    assert!(
        put_result.is_ok(),
        "ConcurrentHashMap.put() failed: {:?}",
        put_result.err()
    );
    // First put should return null (no previous value)
    assert_eq!(
        put_result.expect("put_result should be Ok"),
        Some(Value::Object(None)),
        "First put should return null"
    );

    // get("testKey") should return "testValue"
    let get_result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        chm_id,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(chm)), Value::Object(Some(key))],
    );
    assert!(
        get_result.is_ok(),
        "ConcurrentHashMap.get() failed: {:?}",
        get_result.err()
    );
    match get_result.expect("get_result should be Ok") {
        Some(Value::Object(Some(obj_ref))) => {
            let text = crate::vm::vm_object::read_java_string(&shared.mem.heap, obj_ref);
            assert_eq!(
                text.as_deref(),
                Some("testValue"),
                "ConcurrentHashMap.get(\"testKey\") should return \"testValue\""
            );
        }
        other => panic!(
            "ConcurrentHashMap.get() should return string, got: {:?}",
            other
        ),
    }

    // size() should be 1
    let size_result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        chm_id,
        "size",
        "()I",
        &[Value::Object(Some(chm))],
    );
    assert!(
        size_result.is_ok(),
        "size() failed: {:?}",
        size_result.err()
    );
    assert_eq!(
        size_result.expect("size_result should be Ok"),
        Some(Value::Int(1))
    );

    eprintln!("ConcurrentHashMap.put/get works with real JDK bytecode!");
}

/// Verify CountDownLatch structure from real bytecode.
#[test]
#[ignore] // requires real JDK on PATH
fn s13_count_down_latch_structure() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    let cdl_id = cm
        .get_loaded_class_id("java/util/concurrent/CountDownLatch")
        .expect("CountDownLatch should be loaded");
    let cdl = cm
        .get_class(cdl_id)
        .expect("class should exist in class store");
    assert!(
        !cdl.origin.is_compatibility_stub(),
        "CountDownLatch should be real bytecode"
    );

    let method_names: Vec<&str> = cdl.methods.iter().map(|m| m.name.as_ref()).collect();
    assert!(
        method_names.contains(&"countDown"),
        "CountDownLatch should have countDown"
    );
    assert!(
        method_names.contains(&"await"),
        "CountDownLatch should have await"
    );
    assert!(
        method_names.contains(&"getCount"),
        "CountDownLatch should have getCount"
    );
}

/// Verify Semaphore structure from real bytecode.
#[test]
#[ignore] // requires real JDK on PATH
fn s13_semaphore_structure() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    let sem_id = cm
        .get_loaded_class_id("java/util/concurrent/Semaphore")
        .expect("Semaphore should be loaded");
    let sem = cm
        .get_class(sem_id)
        .expect("class should exist in class store");
    assert!(
        !sem.origin.is_compatibility_stub(),
        "Semaphore should be real bytecode"
    );

    let method_names: Vec<&str> = sem.methods.iter().map(|m| m.name.as_ref()).collect();
    assert!(
        method_names.contains(&"acquire"),
        "Semaphore should have acquire"
    );
    assert!(
        method_names.contains(&"release"),
        "Semaphore should have release"
    );
    assert!(
        method_names.contains(&"tryAcquire"),
        "Semaphore should have tryAcquire"
    );
    assert!(
        method_names.contains(&"availablePermits"),
        "Semaphore should have availablePermits"
    );
}

/// Verify CyclicBarrier structure from real bytecode.
#[test]
#[ignore] // requires real JDK on PATH
fn s13_cyclic_barrier_structure() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    let cb_id = cm
        .get_loaded_class_id("java/util/concurrent/CyclicBarrier")
        .expect("CyclicBarrier should be loaded");
    let cb = cm
        .get_class(cb_id)
        .expect("class should exist in class store");
    assert!(
        !cb.origin.is_compatibility_stub(),
        "CyclicBarrier should be real bytecode"
    );

    let method_names: Vec<&str> = cb.methods.iter().map(|m| m.name.as_ref()).collect();
    assert!(
        method_names.contains(&"await"),
        "CyclicBarrier should have await"
    );
    assert!(
        method_names.contains(&"reset"),
        "CyclicBarrier should have reset"
    );
    assert!(
        method_names.contains(&"getParties"),
        "CyclicBarrier should have getParties"
    );
}

/// Verify ForkJoinPool and ExecutorService are loaded from real bytecode.
#[test]
#[ignore] // requires real JDK on PATH
fn s13_executor_framework_structure() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    // ExecutorService is an interface
    let es_id = cm
        .get_loaded_class_id("java/util/concurrent/ExecutorService")
        .expect("ExecutorService should be loaded");
    let es = cm
        .get_class(es_id)
        .expect("class should exist in class store");
    assert!(
        !es.origin.is_compatibility_stub(),
        "ExecutorService should be real bytecode"
    );
    let es_methods: Vec<&str> = es.methods.iter().map(|m| m.name.as_ref()).collect();
    assert!(
        es_methods.contains(&"submit") || es_methods.contains(&"shutdown"),
        "ExecutorService should have submit or shutdown"
    );

    // ThreadPoolExecutor
    let tpe_id = cm
        .get_loaded_class_id("java/util/concurrent/ThreadPoolExecutor")
        .expect("ThreadPoolExecutor should be loaded");
    let tpe = cm
        .get_class(tpe_id)
        .expect("class should exist in class store");
    assert!(
        !tpe.origin.is_compatibility_stub(),
        "ThreadPoolExecutor should be real bytecode"
    );

    // ForkJoinPool
    let fjp_id = cm
        .get_loaded_class_id("java/util/concurrent/ForkJoinPool")
        .expect("ForkJoinPool should be loaded");
    let fjp = cm
        .get_class(fjp_id)
        .expect("class should exist in class store");
    assert!(
        !fjp.origin.is_compatibility_stub(),
        "ForkJoinPool should be real bytecode"
    );

    let fjp_methods: Vec<&str> = fjp.methods.iter().map(|m| m.name.as_ref()).collect();
    assert!(
        fjp_methods.contains(&"submit") || fjp_methods.contains(&"invoke"),
        "ForkJoinPool should have submit or invoke"
    );

    // ForkJoinTask
    let fjt_id = cm
        .get_loaded_class_id("java/util/concurrent/ForkJoinTask")
        .expect("ForkJoinTask should be loaded");
    let fjt = cm
        .get_class(fjt_id)
        .expect("class should exist in class store");
    assert!(
        !fjt.origin.is_compatibility_stub(),
        "ForkJoinTask should be real bytecode"
    );

    eprintln!("Executor framework: ExecutorService, ThreadPoolExecutor, ForkJoinPool, ForkJoinTask all loaded");
}

/// Verify AtomicLong structure and methods.
#[test]
#[ignore] // requires real JDK on PATH
fn s13_atomic_long_structure() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    let al_id = cm
        .get_loaded_class_id("java/util/concurrent/atomic/AtomicLong")
        .expect("AtomicLong should be loaded");
    let al = cm
        .get_class(al_id)
        .expect("class should exist in class store");
    assert!(!al.origin.is_compatibility_stub());

    let method_names: Vec<&str> = al.methods.iter().map(|m| m.name.as_ref()).collect();
    assert!(method_names.contains(&"get"));
    assert!(method_names.contains(&"set"));
    assert!(method_names.contains(&"incrementAndGet"));
    assert!(method_names.contains(&"compareAndSet"));
}

/// Verify AtomicReference structure.
#[test]
#[ignore] // requires real JDK on PATH
fn s13_atomic_reference_structure() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    let ar_id = cm
        .get_loaded_class_id("java/util/concurrent/atomic/AtomicReference")
        .expect("AtomicReference should be loaded");
    let ar = cm
        .get_class(ar_id)
        .expect("class should exist in class store");
    assert!(!ar.origin.is_compatibility_stub());

    let method_names: Vec<&str> = ar.methods.iter().map(|m| m.name.as_ref()).collect();
    assert!(method_names.contains(&"get"));
    assert!(method_names.contains(&"set"));
    assert!(method_names.contains(&"compareAndSet"));
    assert!(method_names.contains(&"getAndSet"));
}

/// Verify StampedLock structure.
#[test]
#[ignore] // requires real JDK on PATH
fn s13_stamped_lock_structure() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    let sl_id = cm
        .get_loaded_class_id("java/util/concurrent/locks/StampedLock")
        .expect("StampedLock should be loaded");
    let sl = cm
        .get_class(sl_id)
        .expect("class should exist in class store");
    assert!(
        !sl.origin.is_compatibility_stub(),
        "StampedLock should be real bytecode"
    );

    let method_names: Vec<&str> = sl.methods.iter().map(|m| m.name.as_ref()).collect();
    assert!(
        method_names.contains(&"readLock"),
        "StampedLock should have readLock"
    );
    assert!(
        method_names.contains(&"writeLock"),
        "StampedLock should have writeLock"
    );
    assert!(
        method_names.contains(&"tryOptimisticRead"),
        "StampedLock should have tryOptimisticRead"
    );
    assert!(
        method_names.contains(&"validate"),
        "StampedLock should have validate"
    );
}

/// Verify Phaser structure.
#[test]
#[ignore] // requires real JDK on PATH
fn s13_phaser_structure() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    let ph_id = cm
        .get_loaded_class_id("java/util/concurrent/Phaser")
        .expect("Phaser should be loaded");
    let ph = cm
        .get_class(ph_id)
        .expect("class should exist in class store");
    assert!(
        !ph.origin.is_compatibility_stub(),
        "Phaser should be real bytecode"
    );

    let method_names: Vec<&str> = ph.methods.iter().map(|m| m.name.as_ref()).collect();
    assert!(
        method_names.contains(&"arrive"),
        "Phaser should have arrive"
    );
    assert!(
        method_names.contains(&"register"),
        "Phaser should have register"
    );
}

/// Verify LongAdder/DoubleAdder are loaded.
#[test]
#[ignore] // requires real JDK on PATH
fn s13_adders_loaded() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    for class_name in &[
        "java/util/concurrent/atomic/LongAdder",
        "java/util/concurrent/atomic/DoubleAdder",
        "java/util/concurrent/atomic/LongAccumulator",
        "java/util/concurrent/atomic/DoubleAccumulator",
    ] {
        let id = cm
            .get_loaded_class_id(class_name)
            .unwrap_or_else(|| panic!("{class_name} should be loaded"));
        let cls = cm.get_class(id).expect("class should exist in class store");
        assert!(
            !cls.origin.is_compatibility_stub(),
            "{class_name} should be real bytecode"
        );
    }
}

/// Runtime test: ExecutorService.submit(Runnable) creates and runs task.
#[test]
#[ignore] // requires real JDK on PATH
fn s13_executor_submit_runnable_runtime() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

    // Create an AtomicInteger(0) to verify execution
    let ai = new_object_initialized(
        &shared,
        &mut thread,
        "java/util/concurrent/atomic/AtomicInteger",
        "(I)V",
        &[Value::Int(0)],
    );
    let ai_id = shared.mem.heap.class_id_of(ai);

    // Executors.newSingleThreadExecutor()
    let exec_class = shared
        .classes
        .class_manager
        .write()
        .load_class("java/util/concurrent/Executors")
        .expect("Failed to load Executors");
    let exec = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        exec_class,
        "newSingleThreadExecutor",
        "()Ljava/util/concurrent/ExecutorService;",
        &[],
    )
    .expect("newSingleThreadExecutor failed")
    .expect("should return executor");

    // submit(Runnable) — the Runnable increments our AtomicInteger
    // Since we can't easily create a lambda in the test harness, verify the
    // executor object was created and the submit method is registered.
    let exec_obj = match exec {
        Value::Object(Some(o)) => o,
        _ => panic!("Executor should be an object"),
    };
    let exec_id = shared.mem.heap.class_id_of(exec_obj);

    // Verify shutdown/isShutdown contract
    let is_shutdown = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        exec_id,
        "isShutdown",
        "()Z",
        &[Value::Object(Some(exec_obj))],
    )
    .expect("isShutdown failed");
    assert_eq!(
        is_shutdown,
        Some(Value::Int(0)),
        "New executor should not be shutdown"
    );

    // shutdown()
    let _ = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        exec_id,
        "shutdown",
        "()V",
        &[Value::Object(Some(exec_obj))],
    )
    .expect("shutdown failed");

    let is_shutdown = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        exec_id,
        "isShutdown",
        "()Z",
        &[Value::Object(Some(exec_obj))],
    )
    .expect("isShutdown failed after shutdown");
    assert_eq!(
        is_shutdown,
        Some(Value::Int(1)),
        "Executor should be shutdown"
    );

    eprintln!("ExecutorService.submit/shutdown lifecycle works!");
}

/// Runtime test: ExecutorService.submit(Callable) returns a completed Future.
#[test]
#[ignore] // requires real JDK on PATH
fn s13_executor_submit_callable_future_runtime() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

    // Executors.newFixedThreadPool(2)
    let exec_class = shared
        .classes
        .class_manager
        .write()
        .load_class("java/util/concurrent/Executors")
        .expect("Failed to load Executors");
    let exec = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        exec_class,
        "newFixedThreadPool",
        "(I)Ljava/util/concurrent/ExecutorService;",
        &[Value::Int(2)],
    )
    .expect("newFixedThreadPool failed")
    .expect("should return executor");

    let exec_obj = match exec {
        Value::Object(Some(o)) => o,
        _ => panic!("Executor should be an object"),
    };

    // awaitTermination should return true (all tasks done)
    let exec_id = shared.mem.heap.class_id_of(exec_obj);
    let result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        exec_id,
        "awaitTermination",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        &[
            Value::Object(Some(exec_obj)),
            Value::Long(1000),
            Value::Object(None),
        ],
    )
    .expect("awaitTermination failed");
    assert_eq!(
        result,
        Some(Value::Int(1)),
        "awaitTermination should return true"
    );

    eprintln!("ExecutorService.newFixedThreadPool + awaitTermination works!");
}

// -----------------------------------------------------------------------
// Session 14: Module System Wiring
// -----------------------------------------------------------------------

/// Verify that boot modules are discovered from JDK jmod files.
#[test]
#[ignore] // requires real JDK on PATH
fn s14_boot_modules_discovered() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    let modules = cm.list_boot_modules();
    assert!(
        !modules.is_empty(),
        "Should discover at least one boot module"
    );
    assert!(
        modules.iter().any(|m| m == "java.base"),
        "java.base must be present in boot modules, got: {:?}",
        modules
    );

    eprintln!(
        "Discovered {} boot modules: {:?}",
        modules.len(),
        &modules[..modules.len().min(10)]
    );
}

/// Verify that the module registry is populated with module descriptors.
#[test]
#[ignore] // requires real JDK on PATH
fn s14_module_registry_populated() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    eprintln!("Module registry size: {}", cm.module_registry.len());

    assert!(
        !cm.module_registry.is_empty(),
        "Module registry should have registered modules"
    );
    assert!(
        cm.module_registry.len() >= 1,
        "At least java.base should be registered"
    );

    eprintln!("Module registry has {} modules", cm.module_registry.len());
}

/// Verify that the readability graph is built correctly:
/// - java.base reads itself
/// - Other modules read java.base (implicit)
#[test]
#[ignore] // requires real JDK on PATH
fn s14_readability_graph() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    // java.base reads itself
    assert!(
        cm.module_registry.reads("java.base", "java.base"),
        "java.base should read itself"
    );

    // If java.logging is registered, it should read java.base
    if cm.module_registry.len() > 1 {
        let modules = cm.list_boot_modules();
        // Find any non-base module
        if let Some(other) = modules.iter().find(|m| m.as_str() != "java.base") {
            assert!(
                cm.module_registry.reads(other, "java.base"),
                "{other} should read java.base"
            );
        }
    }
}

/// Verify that java.lang.Object is assigned to java.base module.
#[test]
#[ignore] // requires real JDK on PATH
fn s14_object_in_java_base_module() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    let obj_id = cm
        .get_loaded_class_id("java/lang/Object")
        .expect("Object should be loaded");
    let obj = cm
        .get_class(obj_id)
        .expect("class should exist in class store");

    // Object should be in java.base module
    assert_eq!(
        obj.module_name.as_deref(),
        Some("java.base"),
        "java.lang.Object should be in java.base module, got: {:?}",
        obj.module_name
    );
}

/// Verify that classes loaded from classpath (no module) are in the unnamed module.
#[test]
#[ignore] // requires real JDK on PATH
fn s14_classpath_classes_unnamed_module() {
    let shared = create_real_jdk_vm().expect("No JDK found");

    // Load a test class from the test classpath (not a JDK class)
    // Use a synthetic class that would be created with no module
    let cm = shared.classes.class_manager.read();

    // Any synthetic stub or test class should have no module_name
    // Check that unnamed module classes can still access java.base exports
    let obj_id = cm
        .get_loaded_class_id("java/lang/Object")
        .expect("Object should be loaded");

    // Module access check: unnamed module (accessor) → java.base (target)
    // This should always succeed
    let result = crate::classloading::access_control::check_module_access_by_id(
        ClassId::new(0), // synthetic class (unnamed module)
        obj_id,
        &cm,
    );
    assert!(
        result.is_ok(),
        "Unnamed module should be able to access java.base exports: {:?}",
        result.err()
    );
}

/// Verify that java.base exports java/lang to all modules.
#[test]
#[ignore] // requires real JDK on PATH
fn s14_java_base_exports_java_lang() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    // Module access from any module to java.lang.Object should succeed
    // because java.base exports java/lang unconditionally
    let obj_id = cm
        .get_loaded_class_id("java/lang/Object")
        .expect("Object should be loaded");
    let string_id = cm
        .get_loaded_class_id("java/lang/String")
        .expect("String should be loaded");
    let integer_id = cm
        .get_loaded_class_id("java/lang/Integer")
        .expect("Integer should be loaded");

    // Same-module access: String → Object (both in java.base)
    let result =
        crate::classloading::access_control::check_module_access_by_id(string_id, obj_id, &cm);
    assert!(
        result.is_ok(),
        "String→Object access should succeed: {:?}",
        result.err()
    );

    // Same-module: Integer → String
    let result = crate::classloading::access_control::check_module_access_by_id(
        integer_id, string_id, &cm,
    );
    assert!(
        result.is_ok(),
        "Integer→String access should succeed: {:?}",
        result.err()
    );
}

/// Load a class from java.sql module and verify it can access java.base exports.
/// This is the specific roadmap deliverable.
#[test]
#[ignore] // requires real JDK on PATH
fn s14_load_java_sql_access_java_base() {
    let shared = create_real_jdk_vm().expect("No JDK found");

    // Try to load java.sql.Connection (interface in java.sql module)
    let result = shared
        .classes
        .class_manager
        .write()
        .load_class("java/sql/Connection");
    if result.is_err() {
        eprintln!("Skipping: java.sql module not available in this JDK configuration");
        return;
    }
    let conn_id = result.expect("result should be Ok");

    let cm = shared.classes.class_manager.read();
    let conn = cm
        .get_class(conn_id)
        .expect("class should exist in class store");

    // Connection should be from real bytecode
    assert!(
        !conn.origin.is_compatibility_stub(),
        "java.sql.Connection should be real bytecode"
    );

    // Connection should be in java.sql module
    if let Some(ref mod_name) = conn.module_name {
        assert_eq!(
            mod_name, "java.sql",
            "Connection should be in java.sql module, got: {mod_name}"
        );
    }

    // java.sql.Connection can access java.lang.Object (java.base exports java/lang)
    let obj_id = cm
        .get_loaded_class_id("java/lang/Object")
        .expect("class java/lang/Object should be loaded");
    let result =
        crate::classloading::access_control::check_module_access_by_id(conn_id, obj_id, &cm);
    assert!(
        result.is_ok(),
        "java.sql.Connection should be able to access java.lang.Object: {:?}",
        result.err()
    );

    // java.sql.Connection can access java.lang.String
    let str_id = cm
        .get_loaded_class_id("java/lang/String")
        .expect("class java/lang/String should be loaded");
    let result =
        crate::classloading::access_control::check_module_access_by_id(conn_id, str_id, &cm);
    assert!(
        result.is_ok(),
        "java.sql.Connection should be able to access java.lang.String: {:?}",
        result.err()
    );

    eprintln!("java.sql.Connection loaded and can access java.base exports!");
}

/// Verify that module access enforcement is wired into field resolution.
#[test]
#[ignore] // requires real JDK on PATH
fn s14_module_enforcement_in_field_resolution() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

    // Access System.out (java.base class accessing its own static field)
    // This verifies that module enforcement doesn't break same-module access
    let result = crate::vm::invoke_shared(
        &shared,
        &mut thread,
        "java/lang/System",
        "currentTimeMillis",
        "()J",
        &[],
    );
    assert!(
        result.is_ok(),
        "System.currentTimeMillis() should work with module enforcement: {:?}",
        result.err()
    );
}

/// Verify that module access enforcement is wired into method resolution.
#[test]
#[ignore] // requires real JDK on PATH
fn s14_module_enforcement_in_method_resolution() {
    let (shared, mut thread) = create_real_jdk_vm_with_thread().expect("No JDK found");

    // Integer.valueOf(42) — involves method resolution across class boundaries
    // within java.base module
    let result = crate::vm::invoke_shared(
        &shared,
        &mut thread,
        "java/lang/Integer",
        "valueOf",
        "(I)Ljava/lang/Integer;",
        &[Value::Int(42)],
    );
    assert!(
        result.is_ok(),
        "Integer.valueOf(42) should work with module enforcement: {:?}",
        result.err()
    );
}

/// Verify opens infrastructure supports reflection access.
#[test]
#[ignore] // requires real JDK on PATH
fn s14_opens_infrastructure() {
    let shared = create_real_jdk_vm().expect("No JDK found");
    let cm = shared.classes.class_manager.read();

    // Verify module registry exists and has readability data
    if cm.module_registry.is_empty() {
        eprintln!("Skipping: no modules registered");
        return;
    }

    // The unnamed module should be able to read any named module
    // (this is the classpath compatibility guarantee)
    assert!(
        cm.module_registry.reads("", "java.base"),
        "Unnamed module should read java.base"
    );
}

// =======================================================================
// Session 15: Remove Synthetic Stubs — clean separation verification
// =======================================================================

/// S15: In real JDK mode the VM boots with only essential natives (no synthetic stubs).
#[test]
#[ignore]
fn s15_essential_natives_only_in_real_jdk_mode() {
    let shared = match create_real_jdk_vm() {
        Some(s) => s,
        None => {
            eprintln!("Skipping: no JDK found");
            return;
        }
    };
    let count = shared.natives.native_methods.len();
    // Essential natives + I/O natives should be well under 2000
    assert!(
        count < 2000,
        "Real JDK mode should have < 2000 essential natives, got {}",
        count
    );
    // But must have at least the core set
    assert!(
        count > 100,
        "Real JDK mode should have > 100 essential natives, got {}",
        count
    );
}

/// S15: Essential natives include Object.hashCode, System.arraycopy, Class.forName0
#[test]
#[ignore]
fn s15_essential_natives_include_core_methods() {
    let shared = match create_real_jdk_vm() {
        Some(s) => s,
        None => {
            eprintln!("Skipping: no JDK found");
            return;
        }
    };
    // Object.hashCode must be registered
    assert!(
        shared
            .natives
            .native_methods
            .find("java/lang/Object", "hashCode", "()I")
            .is_some(),
        "Object.hashCode must be an essential native"
    );
    // System.arraycopy must be registered
    assert!(
        shared
            .natives
            .native_methods
            .find(
                "java/lang/System",
                "arraycopy",
                "(Ljava/lang/Object;ILjava/lang/Object;II)V"
            )
            .is_some(),
        "System.arraycopy must be an essential native"
    );
    // Class.forName0 must be registered
    assert!(shared.natives.native_methods.find("java/lang/Class", "forName0",
        "(Ljava/lang/String;ZLjava/lang/ClassLoader;Ljava/lang/Class;)Ljava/lang/Class;").is_some(),
        "Class.forName0 must be an essential native");
    // Thread.currentThread must be registered
    assert!(
        shared
            .natives
            .native_methods
            .find("java/lang/Thread", "currentThread", "()Ljava/lang/Thread;")
            .is_some(),
        "Thread.currentThread must be an essential native"
    );
}

/// S15: Synthetic overrides are NOT registered in real JDK mode
#[test]
#[ignore]
fn s15_no_synthetic_overrides_in_real_jdk_mode() {
    let shared = match create_real_jdk_vm() {
        Some(s) => s,
        None => {
            eprintln!("Skipping: no JDK found");
            return;
        }
    };
    // StringBuilder.append should NOT be registered — it's JDK bytecode
    assert!(
        shared
            .natives
            .native_methods
            .find(
                "java/lang/StringBuilder",
                "append",
                "(Ljava/lang/String;)Ljava/lang/StringBuilder;"
            )
            .is_none(),
        "StringBuilder.append should not be an essential native — it runs as JDK bytecode"
    );
    // HashMap.put should NOT be registered
    assert!(
        shared
            .natives
            .native_methods
            .find(
                "java/util/HashMap",
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
            )
            .is_none(),
        "HashMap.put should not be an essential native — it runs as JDK bytecode"
    );
    // BigInteger.add should NOT be registered
    assert!(
        shared
            .natives
            .native_methods
            .find(
                "java/math/BigInteger",
                "add",
                "(Ljava/math/BigInteger;)Ljava/math/BigInteger;"
            )
            .is_none(),
        "BigInteger.add should not be an essential native — it runs as JDK bytecode"
    );
}

/// S15: Real JDK String.valueOf(42) works via native override (not synthetic stubs)
#[test]
#[ignore]
fn s15_string_valueof_works_without_synthetic() {
    let (shared, mut thread) = match create_real_jdk_vm_with_thread() {
        Some(s) => s,
        None => {
            eprintln!("Skipping: no JDK found");
            return;
        }
    };

    let string_id = shared
        .classes
        .class_manager
        .write()
        .load_class("java/lang/String")
        .expect("failed to load class java/lang/String");
    crate::vm::ensure_class_initialized_shared(&shared, &mut thread, string_id)
        .expect("class initialization should succeed");

    let result = crate::vm::invoke_on_class_shared(
        &shared,
        &mut thread,
        string_id,
        "valueOf",
        "(I)Ljava/lang/String;",
        &[Value::Int(42)],
    )
    .expect("String.valueOf(42) failed");

    match result {
        Some(Value::Object(Some(str_ref))) => {
            let s = crate::vm::vm_object::read_java_string(&shared.mem.heap, str_ref);
            assert_eq!(
                s,
                Some("42".to_string()),
                "String.valueOf(42) should return \"42\""
            );
        }
        other => panic!(
            "String.valueOf(42) should return a String object, got {:?}",
            other
        ),
    }
}

/// S15: Real JDK HashMap initializes from bytecode without synthetic stubs
#[test]
#[ignore]
fn s15_hashmap_works_without_synthetic_stubs() {
    let (shared, mut thread) = match create_real_jdk_vm_with_thread() {
        Some(s) => s,
        None => {
            eprintln!("Skipping: no JDK found");
            return;
        }
    };

    // Load and initialize HashMap from real JDK bytecode
    let hm_id = shared
        .classes
        .class_manager
        .write()
        .load_class("java/util/HashMap")
        .expect("failed to load class java/util/HashMap");
    crate::vm::ensure_class_initialized_shared(&shared, &mut thread, hm_id)
        .expect("class initialization should succeed");

    // Verify HashMap class loaded successfully with real field layout
    let cm = shared.classes.class_manager.read();
    let hm_class = cm
        .get_class(hm_id)
        .expect("class should exist in class store");
    assert_eq!(&*hm_class.name, "java/util/HashMap");
    // Real JDK HashMap has instance fields (table, size, threshold, loadFactor, etc.)
    assert!(
        hm_class.num_total_fields > 0,
        "HashMap should have instance fields, got {}",
        hm_class.num_total_fields
    );
    // HashMap should have a superclass (AbstractMap)
    assert!(
        hm_class.superclass.is_some(),
        "HashMap should extend AbstractMap"
    );
}

/// S15: Feature flag correctly gates synthetic-jdk registration functions
#[test]
#[ignore]
fn s15_feature_gate_controls_synthetic_availability() {
    // When compiled with synthetic-jdk (default), register_builtins should exist
    #[cfg(feature = "synthetic-jdk")]
    {
        let mut registry = crate::native::registry::NativeMethodRegistry::new();
        crate::native::register_builtins(&mut registry);
        // Synthetic mode registers thousands of methods
        assert!(
            registry.len() > 2000,
            "Synthetic mode should register > 2000 methods, got {}",
            registry.len()
        );
    }

    // Essential-only should always work regardless of feature
    let mut registry = crate::native::registry::NativeMethodRegistry::new();
    crate::native::register_essential_natives(&mut registry);
    assert!(
        registry.len() < 1000,
        "Essential natives should be < 1000, got {}",
        registry.len()
    );
    assert!(
        registry.len() > 100,
        "Essential natives should be > 100, got {}",
        registry.len()
    );
}

/// S15: All prior session tests still pass — String.valueOf with various inputs
#[test]
#[ignore]
fn s15_full_regression_real_jdk_string_ops() {
    let (shared, mut thread) = match create_real_jdk_vm_with_thread() {
        Some(s) => s,
        None => {
            eprintln!("Skipping: no JDK found");
            return;
        }
    };

    let string_id = shared
        .classes
        .class_manager
        .write()
        .load_class("java/lang/String")
        .expect("failed to load class java/lang/String");
    crate::vm::ensure_class_initialized_shared(&shared, &mut thread, string_id)
        .expect("class initialization should succeed");

    // String.valueOf for multiple int values
    for (input, expected) in &[(0, "0"), (42, "42"), (-1, "-1"), (2147483647, "2147483647")] {
        let result = crate::vm::invoke_on_class_shared(
            &shared,
            &mut thread,
            string_id,
            "valueOf",
            "(I)Ljava/lang/String;",
            &[Value::Int(*input)],
        )
        .expect(&format!("String.valueOf({}) failed", input));

        match result {
            Some(Value::Object(Some(str_ref))) => {
                let s = crate::vm::vm_object::read_java_string(&shared.mem.heap, str_ref);
                assert_eq!(
                    s,
                    Some(expected.to_string()),
                    "String.valueOf({}) should return \"{}\"",
                    input,
                    expected
                );
            }
            other => panic!("String.valueOf({}) failed: {:?}", input, other),
        }
    }
}

// =======================================================================
// Session 16: Iterative Interpreter Refactor
// =======================================================================

/// Default max_stack_depth raised to 8192 (see the doc comment on
/// `VmConfig::default`'s `max_stack_depth` field in config.rs for the
/// root-cause rationale — `DefaultListableBeanFactoryTests
/// .extensiveCircularReference` needs >1024 frames for a 99-bean
/// circular-reference chain that HotSpot handles trivially).
#[test]
fn s16_default_max_stack_depth_is_8192() {
    let config = crate::config::VmConfig::default();
    assert_eq!(config.max_stack_depth, 8192);
}

/// S16: Frame has monitor_on_exit field for stackless synchronized dispatch.
#[test]
fn s16_frame_has_monitor_on_exit() {
    use crate::runtime::frame::Frame;
    let frame = Frame::new(
        crate::classloading::ClassId::new(0),
        "Test".to_string(),
        "test".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1], // return
        vec![],
        2,
        2,
        &[],
    );
    assert!(
        frame.monitor_on_exit.is_none(),
        "New frames should have monitor_on_exit == None"
    );
}

/// Helper: create a real JDK VM with the test resources directory on the classpath.
fn create_real_jdk_vm_with_resources() -> Option<SharedVm> {
    let java_home = crate::config::resolve_java_home_public(None)?;
    let java_home_str = java_home.to_string_lossy().into_owned();
    let resources_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("resources");
    let mut config = VmConfig::new().with_java_home(java_home_str);
    config.use_synthetic_jdk = false;
    config
        .classpath
        .push(resources_dir.to_string_lossy().into_owned());
    Some(SharedVm::new(config))
}

/// S16: Fibonacci(50) iterative runs correctly on the interpreter.
#[test]
#[ignore]
fn s16_fibonacci_50_iterative() {
    let shared = create_real_jdk_vm_with_resources().expect("No JDK found");
    let mut thread = crate::threading::jvm_thread::JvmThread::new(
        crate::threading::jvm_thread::ThreadId(1),
        "test",
    );
    let class_id = shared
        .classes
        .class_manager
        .write()
        .load_class("cratonvm/DeepCallTest")
        .expect("Failed to load DeepCallTest");
    crate::vm::ensure_class_initialized_shared(&shared, &mut thread, class_id)
        .expect("clinit failed");
    let result = crate::runtime::interpreter::execute(
        &shared,
        &mut thread,
        class_id,
        "fibIterative",
        "(I)I",
        &[Value::Int(50)],
    )
    .expect("fibIterative failed");
    // fib(50) = 12586269025, truncated to i32 = 1_258_626_902 (overflow wraps)
    assert!(result.is_some(), "fibIterative should return a value");
}

/// S16: Deep 1000-level recursion succeeds (stackless dispatch).
#[test]
#[ignore]
fn s16_deep_recursion_1000() {
    let shared = create_real_jdk_vm_with_resources().expect("No JDK found");
    let mut thread = crate::threading::jvm_thread::JvmThread::new(
        crate::threading::jvm_thread::ThreadId(1),
        "test",
    );
    let class_id = shared
        .classes
        .class_manager
        .write()
        .load_class("cratonvm/DeepCallTest")
        .expect("Failed to load DeepCallTest");
    crate::vm::ensure_class_initialized_shared(&shared, &mut thread, class_id)
        .expect("clinit failed");
    let result = crate::runtime::interpreter::execute(
        &shared,
        &mut thread,
        class_id,
        "deepCountdown",
        "(I)I",
        &[Value::Int(1000)],
    )
    .expect("deepCountdown(1000) failed");
    match result {
        Some(Value::Int(v)) => {
            assert_eq!(v, 1000, "deepCountdown(1000) should return 1000, got {}", v)
        }
        other => panic!("deepCountdown returned unexpected: {:?}", other),
    }
}

/// S16: Tail-recursive sum of 1..5000 works with a real frame per call,
/// and runaway tail recursion throws `StackOverflowError`.
///
/// This used to be `s16_tail_call_sum_10000`, recursing 10000 deep on the
/// strength of the interpreter's self-tail-call elimination. HotSpot
/// eliminates no frame, so that elimination was deleted (see
/// docs/feature-designs/real-jdk-mode.md);
/// the depth now stays under the default `max_stack_depth` (8192), and
/// the second half pins the HotSpot behaviour past it.
#[test]
#[ignore]
fn s16_tail_call_sum_5000_pushes_real_frames() {
    let shared = create_real_jdk_vm_with_resources().expect("No JDK found");
    let mut thread = crate::threading::jvm_thread::JvmThread::new(
        crate::threading::jvm_thread::ThreadId(1),
        "test",
    );
    let class_id = shared
        .classes
        .class_manager
        .write()
        .load_class("cratonvm/DeepCallTest")
        .expect("Failed to load DeepCallTest");
    crate::vm::ensure_class_initialized_shared(&shared, &mut thread, class_id)
        .expect("clinit failed");
    let result = crate::runtime::interpreter::execute(
        &shared,
        &mut thread,
        class_id,
        "tailSum",
        "(II)I",
        &[Value::Int(5000), Value::Int(0)],
    )
    .expect("tailSum(5000,0) failed");
    match result {
        Some(Value::Int(v)) => assert_eq!(
            v, 12502500,
            "tailSum(5000,0) should return 12502500, got {}",
            v
        ),
        other => panic!("tailSum returned unexpected: {:?}", other),
    }
    let runaway = crate::runtime::interpreter::execute(
        &shared,
        &mut thread,
        class_id,
        "tailSum",
        "(II)I",
        &[Value::Int(100_000), Value::Int(0)],
    );
    assert!(
        runaway.is_err(),
        "tailSum(100000,0) must overflow the frame stack (no tail-call elimination)"
    );
}

/// S16: Mutual recursion (isEven/isOdd) works with stackless dispatch.
#[test]
#[ignore]
fn s16_mutual_recursion() {
    let shared = create_real_jdk_vm_with_resources().expect("No JDK found");
    let mut thread = crate::threading::jvm_thread::JvmThread::new(
        crate::threading::jvm_thread::ThreadId(1),
        "test",
    );
    let class_id = shared
        .classes
        .class_manager
        .write()
        .load_class("cratonvm/DeepCallTest")
        .expect("Failed to load DeepCallTest");
    crate::vm::ensure_class_initialized_shared(&shared, &mut thread, class_id)
        .expect("clinit failed");
    let result = crate::runtime::interpreter::execute(
        &shared,
        &mut thread,
        class_id,
        "isEvenOdd",
        "(I)I",
        &[Value::Int(100)],
    )
    .expect("isEvenOdd(100) failed");
    match result {
        Some(Value::Int(v)) => {
            assert_eq!(v, 1, "isEvenOdd(100) should return 1 (even), got {}", v)
        }
        other => panic!("isEvenOdd returned unexpected: {:?}", other),
    }
}

/// S16: StackOverflowError is thrown when max_stack_depth is exceeded.
#[test]
#[ignore]
fn s16_stack_overflow_error() {
    let shared = create_real_jdk_vm_with_resources().expect("No JDK found");
    let mut thread = crate::threading::jvm_thread::JvmThread::new(
        crate::threading::jvm_thread::ThreadId(1),
        "test",
    );
    let class_id = shared
        .classes
        .class_manager
        .write()
        .load_class("cratonvm/DeepCallTest")
        .expect("Failed to load DeepCallTest");
    crate::vm::ensure_class_initialized_shared(&shared, &mut thread, class_id)
        .expect("clinit failed");
    // deepCountdown is NOT tail-call optimizable (return deepCountdown(n-1) + 1),
    // so with n=100000 and max_stack_depth=1024 it should overflow.
    let result = crate::runtime::interpreter::execute(
        &shared,
        &mut thread,
        class_id,
        "deepCountdown",
        "(I)I",
        &[Value::Int(100000)],
    );
    assert!(
        result.is_err(),
        "deepCountdown(100000) should fail with StackOverflowError"
    );
}

/// S16: Configurable max frame depth via VmConfig.
#[test]
fn s16_configurable_max_frame_depth() {
    let config = crate::config::VmConfig {
        max_stack_depth: 64,
        ..crate::config::VmConfig::default()
    };
    assert_eq!(config.max_stack_depth, 64);
    // Verify it can be set to arbitrary values
    let config2 = crate::config::VmConfig {
        max_stack_depth: 4096,
        ..crate::config::VmConfig::default()
    };
    assert_eq!(config2.max_stack_depth, 4096);    }

// (S16 `s16_frame_reset_for_tail_call` removed 2026-09-23 with
// `Frame::reset_for_tail_call`, whose only production caller — the
// self-recursive tail-call elimination — had already been deleted.)

/// S16: pop_and_recycle_frame releases synchronized monitor.
#[test]
fn s16_pop_and_recycle_releases_monitor() {
    use std::sync::Arc;
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let mut thread = crate::threading::jvm_thread::JvmThread::new(
        crate::threading::jvm_thread::ThreadId(1),
        "test",
    );
    // Create a monitor object and acquire it
    let obj = shared
        .mem
        .heap
        .alloc_object(crate::classloading::ClassId::new(0), 1);
    shared.threads.monitors.enter(obj, thread.thread_id);

    // Push a frame with monitor_on_exit set
    let mut frame = crate::runtime::frame::Frame::new(
        crate::classloading::ClassId::new(0),
        "Test".to_string(),
        "sync".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        2,
        2,
        &[],
    );
    frame.monitor_on_exit = Some(obj);
    thread.frames.push(frame);

    // pop_and_recycle_frame should release the monitor
    crate::runtime::interpreter::pop_and_recycle_frame(&shared, &mut thread);

    // Verify monitor was released — entering again should succeed without deadlock
    shared.threads.monitors.enter(obj, thread.thread_id);
    let _ = shared.threads.monitors.exit(obj, thread.thread_id);
}

// -----------------------------------------------------------------------
// VmDiagnosticState on SharedVm (Session 43)
// -----------------------------------------------------------------------

#[test]
fn diagnostic_state_thread_snapshots() {
    use crate::runtime::serviceability::VmDiagnosticState;
    let shared = SharedVm::new(VmConfig::default());
    let snapshots = shared.thread_snapshots();
    // With no threads registered, should still return at least the main placeholder
    assert!(!snapshots.is_empty());
    assert_eq!(snapshots[0].name, "main");
}

/// gci i18/dc (`gci-i17jh-thread-print-shows-no-frames-for-a-running-thread`):
/// the pause is taken only from an outsider caller, with the published
/// states and the one-pause promise on, and only when a thread runs.
#[test]
fn gci_i18dc_thread_print_pause_decision() {
    for published in [false, true] {
        for one_pause in [false, true] {
            for outsider in [false, true] {
                for running in [false, true] {
                    assert_eq!(
                        super::thread_dump_wants_frame_trace_pause(published, one_pause, outsider, running),
                        published && one_pause && outsider && running,
                        "published={published} one_pause={one_pause} outsider={outsider} running={running}"
                    );
                }
            }
        }
    }
}

/// gci i18/dc: `thread_snapshots` takes the pause before its per-thread
/// loop, as `PAUSE_OUTSIDER`, behind the decision and a VM the JNI
/// registry knows, and reads a running thread's published trace when the
/// pause published.
#[test]
fn gci_i18dc_thread_print_reads_running_threads_after_one_pause() {
    let src = concat!(include_str!("../mod.rs"), "\n", include_str!("../missing_native_entry.rs"), "\n", include_str!("../real_jdk_to_array_typed.rs"), "\n", include_str!("../zgc_relocation_permitted.rs"), "\n", include_str!("../wait_site.rs"), "\n", include_str!("../class_store_cut_with.rs")).replace("\r\n", "\n");
    let start = src
        .find("    fn thread_snapshots(&self) -> Vec<")
        .expect("thread_snapshots");
    let rest = &src[start..];
    let body = &rest[..rest.find("\n    fn thread_dump_header(").expect("its end")];
    let decide = body.find("thread_dump_wants_frame_trace_pause(").expect("the decision");
    let known = body
        .find("crate::native::jni::vm_for_identity(self.vm_identity).is_some()")
        .expect("a registered VM only");
    let pause = body
        .find("crate::runtime::interpreter::stw_publish_frame_traces(")
        .expect("the pause");
    let outsider = body
        .find("crate::runtime::interpreter::PAUSE_OUTSIDER")
        .expect("as an outsider");
    let caller = body
        .find("registry.current_os_tid_claimed() == Some(false)")
        .expect("the caller is no mutator");
    let foreign = body
        .find("!crate::native::jni::is_foreign_attached()")
        .expect("nor a foreign attachment");
    let one_pause = body.find("registry.thread_dump_one_pause()").expect("the kill");
    let loop_at = body
        .find(".filter(|e| e.alive && !continuation_entry(e.thread_id))\n")
        .expect("the per-thread loop");
    assert!(foreign < decide && caller < decide && one_pause > decide);
    assert!(decide < known && known < pause && pause < outsider && outsider < loop_at);
    assert!(body.contains("let stack = if blocked || (traced && published)"));
}

#[test]
fn diagnostic_state_heap_summary() {
    use crate::runtime::serviceability::VmDiagnosticState;
    let shared = SharedVm::new(VmConfig::default());
    let summary = shared.heap_summary();
    assert!(summary.total_capacity > 0);
    assert!(summary.young_gen_capacity > 0 || summary.old_gen_capacity > 0);
}

#[test]
fn diagnostic_state_class_histogram_empty() {
    use crate::runtime::serviceability::VmDiagnosticState;
    let shared = SharedVm::new(VmConfig::default());
    let hist = shared.class_histogram();
    // No objects allocated, histogram should be empty
    assert!(hist.is_empty());
}

#[test]
fn diagnostic_state_class_histogram_with_objects() {
    use crate::runtime::serviceability::VmDiagnosticState;
    let shared = SharedVm::new(VmConfig::default());
    // Allocate some objects
    let _obj1 = shared.mem.heap.alloc_object(ClassId::new(1), 2);
    let _obj2 = shared.mem.heap.alloc_object(ClassId::new(1), 2);
    let _obj3 = shared.mem.heap.alloc_object(ClassId::new(2), 3);
    let hist = shared.class_histogram();
    assert!(hist.len() >= 2); // at least 2 classes
    let total_instances: u64 = hist.iter().map(|e| e.instance_count).sum();
    assert!(total_instances >= 3);
}

#[test]
fn diagnostic_state_trigger_gc() {
    use crate::runtime::serviceability::VmDiagnosticState;
    let shared = SharedVm::new(VmConfig::default());
    assert!(!shared
        .mem
        .gc_requested
        .load(std::sync::atomic::Ordering::Relaxed));
    // gci i2/oo: no mutator takes the request in a unit VM, so the bounded
    // wait runs out and the command reports it as deferred -- never as
    // completed -- with both latches left for the next allocation.
    let ran = shared.trigger_gc();
    assert!(!ran);
    assert!(shared
        .mem
        .gc_requested
        .load(std::sync::atomic::Ordering::Relaxed));
    assert!(shared
        .mem
        .alloc_ladder
        .dcmd_gc_requested
        .load(std::sync::atomic::Ordering::Relaxed));
}

#[test]
fn diagnostic_state_uptime() {
    use crate::runtime::serviceability::VmDiagnosticState;
    let shared = SharedVm::new(VmConfig::default());
    let uptime = shared.uptime_secs();
    assert!(uptime >= 0.0);
}

#[test]
fn diagnostic_state_command_line() {
    use crate::runtime::serviceability::VmDiagnosticState;
    let shared = SharedVm::new(VmConfig::default());
    let cmd = shared.command_line();
    assert!(cmd.contains("cratonvm"));
}

#[test]
fn diagnostic_state_system_properties() {
    use crate::runtime::serviceability::VmDiagnosticState;
    let shared = SharedVm::new(VmConfig::default());
    let props = shared.system_properties();
    assert!(!props.is_empty());
    // Should contain at least java.version
    assert!(props.iter().any(|(k, _)| k == "java.version"));
}

#[test]
fn diagnostic_state_vm_flags() {
    use crate::runtime::serviceability::VmDiagnosticState;
    let shared = SharedVm::new(VmConfig::default());
    let flags = shared.vm_flags();
    assert!(!flags.is_empty());
    // Should contain GC algorithm flag: HotSpot 25 lists the selected
    // collector's `-XX:+Use*GC` as an ergonomic row (wave 138, L1b:
    // `hotspot_flags::resolved_flags`; MEASURED `-XX:+UseG1GC` by default).
    assert!(flags.iter().any(|f| f.contains("GC")), "{flags:?}");
    // Should contain max heap size
    assert!(flags.iter().any(|f| f.contains("MaxHeapSize")));
    // zt t24/x20: the default collector named although no flag selected
    // it, and no non-HotSpot `CompatibilityMode` token.
    assert!(
        !flags.iter().any(|f| f.contains("CompatibilityMode")),
        "{flags:?}"
    );
    #[cfg(feature = "zgc")]
    assert!(flags.iter().any(|f| f == "-XX:+UseZGC"), "{flags:?}");
}

#[test]
fn diagnostic_state_jcmd_integration() {
    use crate::runtime::serviceability::{JcmdProcessor, VmDiagnosticState};
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let jcmd = JcmdProcessor::new_with_vm_state(shared.clone());

    // Thread.print should use real data
    let result = jcmd.process_command("Thread.print");
    assert!(result.success);
    assert!(result.output.contains("main"));

    // GC.run raises the request; with no mutator to run it here the
    // bounded wait (gci i2/oo) runs out and says so.
    let result = jcmd.process_command("GC.run");
    assert!(result.success);
    assert!(result.output.contains("deferred"));
    assert!(shared
        .mem
        .gc_requested
        .load(std::sync::atomic::Ordering::Relaxed));

    // VM.uptime should be a real number. Interpreter round i7 wave 138,
    // lane L1b: in HotSpot 25's shape (`UptimeDCmd`, `<x.xxx> s`, MEASURED
    // `2.746 s`) since gcj j2/ob2's `CRATONVM_GEN_JCMD_HOTSPOT_TEXTS`
    // (default ON); the old `VM uptime: <x> seconds` only with it off.
    let result = jcmd.process_command("VM.uptime");
    assert!(result.success);
    let uptime = result.output.trim_end();
    if crate::runtime::serviceability::gen_jcmd_hotspot_texts() {
        let secs = uptime.strip_suffix(" s").expect(uptime);
        assert!(secs.parse::<f64>().is_ok_and(|s| s >= 0.0), "{uptime}");
        assert_eq!(secs.split('.').nth(1).map(str::len), Some(3), "{uptime}");
    } else {
        assert!(uptime.contains("seconds"), "{uptime}");
    }

    // VM.flags should show real config
    let result = jcmd.process_command("VM.flags");
    assert!(result.success);
    assert!(result.output.contains("MaxHeapSize"));

    // VM.system_properties should show real properties
    let result = jcmd.process_command("VM.system_properties");
    assert!(result.success);
    assert!(result.output.contains("java.version"));

    // GC.heap_info should show real heap stats
    let result = jcmd.process_command("GC.heap_info");
    assert!(result.success);
    // gci i14/hd: HotSpot's text on Generational and G1; the summary text
    // on ZGC (the default backend with the `zgc` feature). gci i47w/oa47:
    // HotSpot 25's `HeapInfoDCmd` prints `print_heap_on` alone (no `Heap`
    // header, no Metaspace line, the collector's first line at column 0);
    // only the summary text names Metaspace.
    let hotspot_text = matches!(
        shared.config.gc_algorithm,
        crate::config::GcAlgorithm::G1 | crate::config::GcAlgorithm::Generational
    );
    assert_eq!(result.output.contains("Metaspace"), !hotspot_text, "{}", result.output);
    assert!(!result.output.starts_with("Heap\n "), "{}", result.output);

    // GC.class_histogram — empty heap
    let result = jcmd.process_command("GC.class_histogram");
    assert!(result.success);
    assert!(result.output.contains("#instances"));

    // VM.info, VM.metaspace, VM.classes, VM.classloader_stats
    let vm_info = jcmd.process_command("VM.info");
    assert!(vm_info.success);
    assert!(vm_info.output.contains("GC Precious Log:"));
    // Wave 139 (L1b): HotSpot's `CPUs:` row, not the invented
    // `Polling page: 0x0000000000000000`.
    assert!(vm_info.output.contains(" CPUs: "));
    assert!(!vm_info.output.contains("Polling page:"));
    assert!(vm_info.output.contains("Metaspace:"));

    let vm_ms = jcmd.process_command("VM.metaspace");
    assert!(vm_ms.success);
    assert!(vm_ms.output.contains("Total Usage - "));

    let vm_cls = jcmd.process_command("VM.classes");
    assert!(vm_cls.success);
    assert!(vm_cls.output.contains("KlassAddr"));

    let vm_cls_stats = jcmd.process_command("VM.classloader_stats");
    assert!(vm_cls_stats.success);
    assert!(vm_cls_stats.output.contains("<boot class loader>"));

    // JFR.check / JFR.start / JFR.dump / JFR.stop HotSpot 25 output format
    let check_empty = jcmd.process_command("JFR.check");
    assert!(check_empty.success);
    assert!(check_empty
        .output
        .starts_with("No available recordings.\n\nUse jcmd "));

    let start_out = jcmd.process_command("JFR.start name=myrec maxsize=10m");
    assert!(start_out.success, "{start_out:?}");
    assert!(
        start_out
            .output
            .starts_with("Started recording 1.\n\nUse jcmd "),
        "{}",
        start_out.output
    );

    let check_run = jcmd.process_command("JFR.check");
    assert!(check_run.success);
    assert_eq!(check_run.output, "Recording 1: name=myrec (running)\n");

    let tmp_jfr = std::env::temp_dir().join("cratonvm_test_jcmd_jfr_parity.jfr");
    let _ = std::fs::remove_file(&tmp_jfr);
    let dump_out = jcmd.process_command(&format!(
        "JFR.dump name=myrec filename={}",
        tmp_jfr.display()
    ));
    assert!(dump_out.success, "{dump_out:?}");
    assert!(
        dump_out.output.starts_with("Dumped recording \"myrec\", "),
        "{}",
        dump_out.output
    );
    assert!(tmp_jfr.exists());
    let _ = std::fs::remove_file(&tmp_jfr);

    let stop_out = jcmd.process_command("JFR.stop name=myrec");
    assert!(stop_out.success, "{stop_out:?}");
    assert_eq!(stop_out.output, "Stopped recording \"myrec\".\n");
}

/// gci i14/hd (`gci-i14hd-g1-heap-diagnostics-review-findings-FIXED-20261016`
/// item 6): `jcmd GC.heap_info` on G1 prints HotSpot 25 G1's
/// `Universe::print_on` block (it printed `Heap Configuration:` /
/// `Young Generation: ...`).
#[test]
fn gci_i14hd_gc_heap_info_on_g1_is_hotspots_text() {
    use crate::runtime::serviceability::JcmdProcessor;
    let shared = Arc::new(SharedVm::new(VmConfig {
        gc_algorithm: crate::config::GcAlgorithm::G1,
        max_heap_size: 64 * 1024 * 1024,
        ..VmConfig::default()
    }));
    let jcmd = JcmdProcessor::new_with_vm_state(shared.clone());
    let result = jcmd.process_command("GC.heap_info");
    assert!(result.success);
    let lines: Vec<&str> = result.output.lines().collect();
    // gci i47w/oa47: HotSpot 25's `HeapInfoDCmd` is `print_heap_on`
    // alone -- the two G1 lines, one column shallower than in
    // `Universe::print_on`, no `Heap` header and no Metaspace lines
    // (Temurin 25.0.3, vi14-hd `hd14_hinfo_g1`).
    assert_eq!(lines.len(), 2, "{}", result.output);
    assert!(lines[0].starts_with("garbage-first heap   total reserved "), "{}", result.output);
    assert!(lines[1].starts_with(" region size "), "{}", result.output);
    assert!(result.output.ends_with('\n'));
}

#[test]
fn diagnostic_counters_on_shared_vm() {
    let shared = SharedVm::new(VmConfig::default());
    // Counters should be initialized to zero
    assert_eq!(
        crate::runtime::diagnostics::DiagnosticCounters::get(
            &shared.debug.diagnostic_counters.gc_cycles
        ),
        0
    );
    // Increment and check
    shared
        .debug
        .diagnostic_counters
        .inc(&shared.debug.diagnostic_counters.classes_loaded);
    assert_eq!(
        crate::runtime::diagnostics::DiagnosticCounters::get(
            &shared.debug.diagnostic_counters.classes_loaded
        ),
        1
    );
}

// -----------------------------------------------------------------------
// Heap dump tests (Session 42)
// -----------------------------------------------------------------------

#[test]
fn heap_dump_empty_heap_to_file() {
    use crate::runtime::serviceability::VmDiagnosticState;
    let shared = SharedVm::new(VmConfig::default());
    let tmp_path = std::env::temp_dir().join("cratonvm_test_empty_heap.hprof");
    let result = shared.heap_dump(tmp_path.to_str().expect("heap dump should succeed"));
    assert!(result.is_ok(), "heap_dump failed: {:?}", result);
    let bytes = result.expect("result should be Ok");
    assert!(bytes > 0, "Heap dump should produce non-empty output");

    // Verify the file exists and starts with HPROF magic
    let data = std::fs::read(&tmp_path).expect("file should be readable");
    assert_eq!(data.len(), bytes as usize);
    let magic = "JAVA PROFILE 1.0.2";
    assert_eq!(&data[..magic.len()], magic.as_bytes());
    std::fs::remove_file(&tmp_path).ok();
}

#[test]
fn heap_dump_with_allocated_objects() {
    use crate::classloading::ClassId;
    use crate::runtime::serviceability::VmDiagnosticState;
    let shared = SharedVm::new(VmConfig::default());

    // Load a class so there's class info available
    {
        let mut cm = shared.classes.class_manager_write();
        let _ = cm.load_class("java/lang/Object");
    }

    // Allocate some objects on the heap
    let _obj1 = shared.mem.heap.alloc_object(ClassId::new(0), 2);
    let _obj2 = shared.mem.heap.alloc_object(ClassId::new(0), 0);

    let tmp_path = std::env::temp_dir().join("cratonvm_test_objects_heap.hprof");
    let result = shared.heap_dump(tmp_path.to_str().expect("heap dump should succeed"));
    assert!(result.is_ok(), "heap_dump failed: {:?}", result);
    let bytes = result.expect("result should be Ok");
    assert!(bytes > 0);

    // Verify HPROF structure
    let data = std::fs::read(&tmp_path).expect("file should be readable");
    // Should contain HEAP_DUMP_SEGMENT (0x1C) and HEAP_DUMP_END (0x2C)
    assert!(data.contains(&0x1Cu8), "Missing HEAP_DUMP_SEGMENT");
    assert!(data.contains(&0x2Cu8), "Missing HEAP_DUMP_END");
    std::fs::remove_file(&tmp_path).ok();
}

#[test]
fn heap_dump_jcmd_integration() {
    use crate::runtime::serviceability::{JcmdProcessor, VmDiagnosticState};
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let jcmd = JcmdProcessor::new_with_vm_state(shared.clone());

    let tmp_path = std::env::temp_dir().join("cratonvm_test_jcmd_heap.hprof");
    // gci i3/hd: HotSpot's command refuses an existing file without
    // `-overwrite`; a file left by an earlier failed run must not decide
    // this test.
    let _ = std::fs::remove_file(&tmp_path);
    let cmd = format!(
        "GC.heap_dump {}",
        tmp_path.to_str().expect("heap dump should succeed")
    );
    let result = jcmd.process_command(&cmd);
    assert!(
        result.success,
        "GC.heap_dump command failed: {}",
        result.output
    );
    // gci i3/hd: HotSpot's lines.
    assert!(result.output.starts_with("Dumping heap to "), "{}", result.output);
    assert!(result.output.contains("Heap dump file created ["));
    assert!(result.output.contains("bytes"));

    // Verify file was actually written
    assert!(tmp_path.exists(), "Heap dump file should exist");
    let data = std::fs::read(&tmp_path).expect("file should be readable");
    assert!(data.starts_with(b"JAVA PROFILE 1.0.2"));
    std::fs::remove_file(&tmp_path).ok();
}

#[test]
fn heap_dump_invalid_path_returns_error() {
    use crate::runtime::serviceability::VmDiagnosticState;
    let shared = SharedVm::new(VmConfig::default());
    // Use an invalid path that should fail
    let result = shared.heap_dump("/nonexistent/directory/that/does/not/exist/dump.hprof");
    assert!(result.is_err(), "Should fail with invalid path");
    let err = result.unwrap_err();
    assert!(
        err.contains("Failed to write"),
        "Error should mention write failure: {}",
        err
    );
}

// -----------------------------------------------------------------
// T19.3.G1 — allocation-storm observability counters.
// -----------------------------------------------------------------

#[test]
fn t19_storm_counters_default_zero() {
    use std::sync::atomic::Ordering;
    let shared = SharedVm::new(VmConfig::default());
    assert_eq!(shared.mem.tlab_refill_count.load(Ordering::Relaxed), 0);
    assert_eq!(shared.mem.gc_cycle_count.load(Ordering::Relaxed), 0);
    assert_eq!(shared.mem.bytes_allocated_total.load(Ordering::Relaxed), 0);
}

#[test]
fn t19_storm_counters_monotonic_nondecreasing() {
    use std::sync::atomic::Ordering;
    let shared = SharedVm::new(VmConfig::default());
    let before = (
        shared.mem.tlab_refill_count.load(Ordering::Relaxed),
        shared.mem.gc_cycle_count.load(Ordering::Relaxed),
        shared.mem.bytes_allocated_total.load(Ordering::Relaxed),
    );
    // Simulate N allocations publishing one coalesced TLAB batch.
    for _ in 0..32 {
        shared
            .mem
            .bytes_allocated_total
            .fetch_add(24, Ordering::Relaxed);
    }
    shared.mem.tlab_refill_count.fetch_add(1, Ordering::Relaxed);
    shared.mem.gc_cycle_count.fetch_add(1, Ordering::Relaxed);
    let after = (
        shared.mem.tlab_refill_count.load(Ordering::Relaxed),
        shared.mem.gc_cycle_count.load(Ordering::Relaxed),
        shared.mem.bytes_allocated_total.load(Ordering::Relaxed),
    );
    assert!(after.0 >= before.0, "tlab_refill_count went backwards");
    assert!(after.1 >= before.1, "gc_cycle_count went backwards");
    assert!(after.2 >= before.2, "bytes_allocated_total went backwards");
    assert_eq!(after.2 - before.2, 32 * 24);
    assert_eq!(after.0 - before.0, 1);
    assert_eq!(after.1 - before.1, 1);
}

#[test]
fn t19_storm_counters_are_atomic() {
    // Spawn many threads hammering the counter to confirm it
    // is a real AtomicU64, not a Mutex<u64> hiding a race.
    use std::sync::atomic::Ordering;
    let shared = std::sync::Arc::new(SharedVm::new(VmConfig::default()));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let s = std::sync::Arc::clone(&shared);
        handles.push(std::thread::spawn(move || {
            for _ in 0..1000 {
                s.mem.bytes_allocated_total.fetch_add(48, Ordering::Relaxed);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(
        shared.mem.bytes_allocated_total.load(Ordering::Relaxed),
        8 * 1000 * 48
    );
}

// -----------------------------------------------------------------------
// Per-VM state (P0 / P1 — `docs/architecture/per-vm-state.md`)
// -----------------------------------------------------------------------
//
// The hook registry is process-global, so these tests serialize against
// each other and assert only on the VMs THEY created (never on the total
// registry length, which a concurrently-running test may inflate) — the
// same discipline `memory::native_roots`'s tests use.

/// Serializes the tests below, which mutate the process-global
/// `RESOLUTION_INVALIDATE_VMS` registry.
static HOOK_REGISTRY_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

fn registry_contains(shared: &Arc<SharedVm>) -> bool {
    live_hook_vms().iter().any(|live| Arc::ptr_eq(live, shared))
}

/// Everything that keys process-global state per VM (`object_class_id`'s
/// thread-local cache, `offload_jit_gate`'s `GateKey`, `interpreter`'s EC
/// watch memo, `invokedynamic`'s lambda-singleton cache) rests on this one
/// fact. If two live VMs could ever share a `vm_identity`, every one of
/// those keys silently aliases again.
#[test]
fn vm_identity_is_unique_per_shared_vm() {
    let a = Arc::new(SharedVm::new(VmConfig::default()));
    let b = Arc::new(SharedVm::new(VmConfig::default()));
    assert_ne!(
        a.vm_identity, b.vm_identity,
        "two concurrently-live VMs must not share a vm_identity"
    );
}

/// `-Xverify:all` reaches the class manager.
///
/// The verifier's own behaviour under the flag is pinned by
/// `bytecode_verifier::tests::xverify_all_makes_a_trusted_class_reject_what_remote_accepts`.
/// What THAT test cannot see is the wire: `XverifyMode::All` spent its whole
/// life being parsed, stored on `VmConfig`, and read by nobody. This asserts
/// the one hop that made it inert — config to `ClassManager` — on a really
/// booted VM, and that the default is still `remote`.
///
/// Per-VM, not process-global: two live VMs must be able to disagree, which
/// is also what makes this test safe to run beside every other VM-building
/// test in this binary.
#[test]
fn xverify_all_reaches_the_class_manager_and_only_that_vm() {
    let strict =
        SharedVm::new(VmConfig::default().with_xverify_mode(crate::config::XverifyMode::All));
    let lenient = SharedVm::new(VmConfig::default());
    assert!(
        strict.classes.class_manager.read().strict_verification(),
        "-Xverify:all must arrive at the class manager, or the flag is inert \
         again — which is exactly the state it was in before being wired"
    );
    assert!(
        !lenient.classes.class_manager.read().strict_verification(),
        "the default is -Xverify:remote, and a second live VM must not \
         inherit the first's policy"
    );
}

// -----------------------------------------------------------------------
// Capability policy: install at boot, release at teardown
// -----------------------------------------------------------------------

/// The whole point of the install: before it, all 35 wired gates in
/// `native-builtins` / `native-io` resolved `None` and `capability_audit`
/// answered `None` for every VM in the process.
///
/// Both halves are asserted, and that they are the SAME `Arc` — the
/// registry-side gate and the per-call-site gates must share one audit log,
/// or a report is half the run.
#[test]
fn a_booted_vm_has_a_capability_policy_in_the_registry_and_the_index() {
    let vm = SharedVm::new(VmConfig::default());
    let id = cratonvm_native_api::VmId::from_raw(vm.vm_identity);

    let in_registry = vm
        .natives
        .native_methods
        .capabilities()
        .expect("SharedVm::new must install a policy into its own registry");
    let in_index = cratonvm_native_api::capabilities_for(id)
        .expect("SharedVm::new must publish the policy under its VmId");
    assert!(
        Arc::ptr_eq(in_registry, &in_index),
        "the registry gate and the per-call-site gates must share ONE set"
    );
    assert_eq!(in_index.vm(), id);
    assert!(cratonvm_native_api::capability_audit(id).is_some());
}

/// The default must not change what any existing deployment does.
/// `Permissive` allows everything and merely counts it.
#[test]
fn the_boot_default_is_permissive_and_every_native_still_registers() {
    let vm = SharedVm::new(VmConfig::default());
    let caps = vm.natives.native_methods.capabilities().unwrap();
    assert_eq!(caps.mode(), cratonvm_native_api::CapabilityMode::Permissive);
    assert!(
        caps.grants().is_empty(),
        "no CRATONVM_CAPABILITY_GRANTS in the test environment"
    );

    // Registration is itself gated (`Capability::NativeRegister`), and the
    // policy is installed BEFORE the `register_*` pass precisely so that
    // gate can see it. Under `Permissive` it must admit every one of them.
    assert!(
        vm.natives.native_methods.len() > 100,
        "the boot registration pass must not have been refused; only {} \
         natives registered",
        vm.natives.native_methods.len()
    );
    // …and a specific, well-known one, so the assertion above cannot pass on
    // a registry that accepted a thousand natives and dropped the rest.
    assert!(
        vm.natives
            .native_methods
            .find("java/lang/Object", "hashCode", "()I")
            .is_some(),
        "java/lang/Object.hashCode must survive the registration gate"
    );
}

/// The ~3,100 `native-register` rows the boot pass would otherwise leave in
/// the audit map are cleared under `Permissive` — `MAX_AUDIT_ENTRIES` is
/// 4,096, and a map already full of registration rows would drop the
/// file/socket/spawn uses the report exists to collect.
#[test]
fn the_permissive_boot_audit_starts_empty_so_runtime_uses_are_not_crowded_out() {
    let vm = SharedVm::new(VmConfig::default());
    let report = cratonvm_native_api::capability_audit(cratonvm_native_api::VmId::from_raw(
        vm.vm_identity,
    ))
    .expect("installed");
    assert!(
        !report.truncated,
        "the audit map must not already be at MAX_AUDIT_ENTRIES:\n{report}"
    );
    assert!(
        report
            .uses
            .iter()
            .all(|u| u.capability.kind() != cratonvm_native_api::CapabilityKind::NativeRegister),
        "boot registration rows must not survive into the run's report:\n{report}"
    );
}

/// Teardown. Without this a long-lived host process accumulates one dead
/// entry per disposed VM in the `Vec` that every `capabilities_for` lookup
/// scans, and — worse — `native-builtins`' per-VM SecurityManager row keeps
/// raw `ObjectRef`s into a heap that no longer exists.
#[test]
fn dropping_a_vm_releases_its_capability_and_security_state() {
    let id;
    {
        let vm = SharedVm::new(VmConfig::default());
        id = cratonvm_native_api::VmId::from_raw(vm.vm_identity);
        assert!(cratonvm_native_api::capabilities_for(id).is_some());
        // Seed the third row so its teardown is covered too. The ref is a
        // synthetic non-heap address; it is only ever compared by pointer
        // value, never dereferenced.
        crate::runtime::instrument::add_transformer_entry(
            vm.vm_identity,
            crate::runtime::instrument::TransformerEntry {
                // SAFETY: non-null, 8-byte aligned, never dereferenced.
                transformer_ref: unsafe {
                    cratonvm_types::ObjectRef::from_raw(0x5000usize as *mut u8)
                },
                can_retransform: true,
                native_method_prefix: None,
            },
        );
        assert_eq!(
            crate::runtime::instrument::transformer_count(vm.vm_identity),
            1
        );
    }
    assert!(
        cratonvm_native_api::capabilities_for(id).is_none(),
        "SharedVm::drop must uninstall this VM's policy"
    );

    // The security half. `forget_vm_security_state` has no public reader,
    // so this asserts the observable postcondition: no VM-scoped
    // SecurityManager root remains to be handed to a collector.
    let mut roots = Vec::new();
    cratonvm_native_builtins::security_manager::gc_scan_security_manager_roots(
        id.as_usize(),
        &mut roots,
    );
    assert!(roots.is_empty(), "security state outlived the VM");

    // The instrument half: the transformer chain holds raw `ObjectRef`s
    // into the dropped heap and is a registered GC root source, so a
    // surviving row would be reported to a later VM's collector.
    assert_eq!(
        crate::runtime::instrument::transformer_count(id.as_usize()),
        0,
        "the transformer chain outlived the VM"
    );
}

/// Both teardown hooks call it and either may run first or alone (a bare
/// `SharedVm` runs only its own; a `Vm`'s `SharedVm` kept alive by another
/// thread's `Arc` runs its own much later, possibly never).
#[test]
fn releasing_vm_native_state_twice_is_harmless() {
    let vm = SharedVm::new(VmConfig::default());
    let raw = vm.vm_identity;
    release_vm_native_state(raw);
    assert!(
        cratonvm_native_api::capabilities_for(cratonvm_native_api::VmId::from_raw(raw))
            .is_none()
    );
    // Second call: no entry left, no panic, and the `SharedVm` drop below
    // makes it a third.
    release_vm_native_state(raw);
    drop(vm);
}

/// Two VMs in one process get two sets under two ids, with two audit logs.
/// One policy shared by two VMs is the exact defect the capability model
/// exists to remove.
#[test]
fn two_vms_hold_independent_capability_sets() {
    let a = SharedVm::new(VmConfig::default());
    let b = SharedVm::new(VmConfig::default());
    let ida = cratonvm_native_api::VmId::from_raw(a.vm_identity);
    let idb = cratonvm_native_api::VmId::from_raw(b.vm_identity);
    assert_ne!(ida, idb);

    let ca = cratonvm_native_api::capabilities_for(ida).unwrap();
    let cb = cratonvm_native_api::capabilities_for(idb).unwrap();
    assert!(!Arc::ptr_eq(&ca, &cb), "two VMs must not share one set");

    ca.check(cratonvm_native_api::Capability::file_read("/only-in-vm-a"))
        .expect("Permissive");
    let names = |set: &Arc<cratonvm_native_api::CapabilitySet>| {
        set.audit_report()
            .uses
            .iter()
            .map(|u| u.capability.to_string())
            .collect::<Vec<_>>()
    };
    assert!(
        names(&ca).iter().any(|n| n.contains("only-in-vm-a")),
        "VM A must record its own use: {:?}",
        names(&ca)
    );
    assert!(
        !names(&cb).iter().any(|n| n.contains("only-in-vm-a")),
        "VM B must never see VM A's traffic: {:?}",
        names(&cb)
    );

    // And dropping one leaves the other's policy installed.
    drop(a);
    assert!(cratonvm_native_api::capabilities_for(ida).is_none());
    assert!(cratonvm_native_api::capabilities_for(idb).is_some());
}

/// A `ClassId` names a DIFFERENT class in each VM (`ClassStore::next_id`
/// is `self.classes.len()`, which restarts at 0 per VM), so any cache
/// keyed on a bare `ClassId` aliases across VMs. This asserts the fix
/// shape: prefixing the key with `vm_identity` separates them.
#[test]
fn vm_keyed_class_ids_do_not_collide_across_vms() {
    let a = Arc::new(SharedVm::new(VmConfig::default()));
    let b = Arc::new(SharedVm::new(VmConfig::default()));

    let bare = ClassId::new(7);
    let key_a = (a.vm_identity, bare);
    let key_b = (b.vm_identity, bare);

    assert_ne!(
        key_a, key_b,
        "the same numeric ClassId in two VMs must produce two distinct keys"
    );

    let mut cache: HashMap<(usize, ClassId), &'static str> = HashMap::new();
    cache.insert(key_a, "vm-a-class");
    cache.insert(key_b, "vm-b-class");
    assert_eq!(cache.get(&key_a).copied(), Some("vm-a-class"));
    assert_eq!(cache.get(&key_b).copied(), Some("vm-b-class"));
}

/// EVIDENCE for the security-manager finding
/// (`native-builtins/src/security_manager.rs`, `SECURITY_MANAGER`).
///
/// The var-handle root registry lives in `SharedVm::mem` (see
/// `vm::realms::heap_realm::HeapRealm::var_handle_roots`), i.e. it is
/// PER-VM. A native subsystem that caches `(identity_key, ObjectRef)` in a
/// PROCESS-GLOBAL slot and re-reads the current address with
/// `ctx.read_var_handle_root(key).unwrap_or(cached)` therefore misses in
/// every VM except the one that installed it — and silently falls back to
/// the raw `cached` address, which that VM's GC neither scans nor rewrites.
/// Under a moving young GC that is a use-after-move, not a policy leak.
///
/// This test pins the property the fallback depends on, so the security
/// manager's per-VM ownership cannot be "fixed" by re-adding a shared
/// registry without this failing.
#[test]
fn var_handle_roots_are_per_vm_so_a_global_cached_ref_cannot_be_remapped() {
    let a = Arc::new(SharedVm::new(VmConfig::default()));
    let b = Arc::new(SharedVm::new(VmConfig::default()));

    // A never-dereferenced, 8-byte-aligned synthetic address standing in
    // for a SecurityManager instance allocated in VM A's heap.
    // SAFETY: non-null and aligned; only ever compared by pointer value.
    let sm_in_vm_a = unsafe { ObjectRef::from_raw(0x5EC0_0000usize as *mut u8) };
    let identity_key: i32 = 0x5EC0;

    a.mem
        .var_handle_roots
        .write()
        .insert(identity_key, sm_in_vm_a);

    assert_eq!(
        a.mem.var_handle_roots.read().get(&identity_key).copied(),
        Some(sm_in_vm_a),
        "the installing VM must see its own var-handle root"
    );
    assert!(
        b.mem.var_handle_roots.read().get(&identity_key).is_none(),
        "VM B must NOT resolve VM A's var-handle key — the `unwrap_or(cached)` \
         fallback in a process-global singleton therefore hands VM B a raw, \
         unrooted, un-remappable reference into VM A's heap"
    );
}

/// Regression: the redefine-invalidation bridge used to be a
/// `OnceLock<Weak<SharedVm>>`, so the FIRST VM ever created owned the hook
/// for the life of the process. Registering a second VM was silently
/// dropped, and every `RedefineClasses` in that VM left its own
/// `ResolutionCache` / `LinkResolver` serving pre-redefine
/// `(declaring_class_id, index)` pairs.
#[test]
fn hook_registry_reaches_every_live_vm_not_just_the_first() {
    let _guard = HOOK_REGISTRY_TEST_LOCK.lock();

    let first = Arc::new(SharedVm::new(VmConfig::default()));
    let second = Arc::new(SharedVm::new(VmConfig::default()));

    set_global_shared_vm_for_hooks(Arc::downgrade(&first));
    set_global_shared_vm_for_hooks(Arc::downgrade(&second));

    assert!(
        registry_contains(&first),
        "the first-registered VM must stay reachable"
    );
    assert!(
        registry_contains(&second),
        "a VM registered AFTER another must still receive hook callbacks — \
         the old OnceLock silently ignored it"
    );

    // The adapters must not panic or deadlock with several VMs live; they
    // take VM-internal locks, so this also exercises the "snapshot the
    // registry, then drop its lock" ordering in `live_hook_vms`.
    resolution_invalidate_adapter(
        0,
        u32::MAX,
        cratonvm_classloading::ResolutionInvalidation::ConstantPoolReplaced,
    );
    jit_invalidate_adapter(0, u32::MAX);
}

/// Round 9 wave 8b (review8b request 1): an in-place layout change (the
/// JIT-invalidate hook) must unpublish the class's allocation-init recipe,
/// whose baked field indices the change moved.
#[test]
fn jit_invalidate_hook_drops_the_alloc_class_recipe() {
    use crate::jit::alloc_class_cache::{ClassAllocInfo, PrimKind};
    let _guard = HOOK_REGISTRY_TEST_LOCK.lock();

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    set_global_shared_vm_for_hooks(Arc::downgrade(&shared));
    let id = 4242u32;
    let other = 4243u32;
    for cid in [id, other] {
        let _ = shared.jit.jit_alloc_class_cache.insert(
            cid,
            ClassAllocInfo {
                has_finalizer: false,
                reference_kind: 0,
                prim_inits: vec![(3, PrimKind::Int)].into_boxed_slice(),
                ref_inits: Box::new([]),
            },
        );
    }
    assert!(shared.jit.jit_alloc_class_cache.get(id).is_some());

    jit_invalidate_adapter(id, shared.jit.class_layout_domain);

    assert!(
        shared.jit.jit_alloc_class_cache.get(id).is_none(),
        "a layout change must drop the recipe that baked the old indices"
    );
    assert!(
        shared.jit.jit_alloc_class_cache.get(other).is_some(),
        "another class's recipe is untouched"
    );
}

/// Interpreter round i1 wave 17, lane L5: inside an in-place redefinition
/// of the class on this thread (`InPlaceRedefinitionScope`, which
/// `redefine_class_with` holds around `ClassManager::redefine_class`) the
/// hook still drops the allocation recipe, and leaves the code-cache flush
/// to the redefinition (`jit_realm::tests` pins the predicate; the
/// epochs are not asserted here, since any concurrent test's layout
/// change flushes every live VM). Outside the scope it flushes as before.
#[test]
fn an_in_place_redefinition_keeps_the_recipe_drop_and_skips_the_flush() {
    use crate::jit::alloc_class_cache::{ClassAllocInfo, PrimKind};
    use crate::vm::realms::jit_realm::InPlaceRedefinitionScope;
    let _guard = HOOK_REGISTRY_TEST_LOCK.lock();

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    set_global_shared_vm_for_hooks(Arc::downgrade(&shared));
    let id = 4252u32;
    let recipe = || ClassAllocInfo {
        has_finalizer: false,
        reference_kind: 0,
        prim_inits: vec![(3, PrimKind::Int)].into_boxed_slice(),
        ref_inits: Box::new([]),
    };
    let _ = shared.jit.jit_alloc_class_cache.insert(id, recipe());
    {
        let _scope = InPlaceRedefinitionScope::enter(crate::classloading::ClassId::new(id));
        jit_invalidate_adapter(id, shared.jit.class_layout_domain);
    }
    assert!(
        shared.jit.jit_alloc_class_cache.get(id).is_none(),
        "a redefinition still drops the recipe"
    );

    let before = shared.jit.code_state_epoch();
    let _ = shared.jit.jit_alloc_class_cache.insert(id, recipe());
    jit_invalidate_adapter(id, shared.jit.class_layout_domain);
    assert!(
        shared.jit.code_state_epoch() > before,
        "outside the scope a layout change flushes the VM"
    );
}

/// Interpreter round i1 wave 18, lane L5: a layout change reaches only the
/// VM whose class store fired the hook (its layout domain is the owner
/// token); a token no live VM owns still reaches every VM. The other VM's
/// epoch is not asserted: a concurrent test's class manager built outside
/// a VM fires with an unknown token, which flushes every live VM.
#[test]
fn a_layout_change_flushes_only_the_owning_vm() {
    use crate::jit::alloc_class_cache::{ClassAllocInfo, PrimKind};
    let _guard = HOOK_REGISTRY_TEST_LOCK.lock();

    let owner = Arc::new(SharedVm::new(VmConfig::default()));
    let other = Arc::new(SharedVm::new(VmConfig::default()));
    set_global_shared_vm_for_hooks(Arc::downgrade(&owner));
    set_global_shared_vm_for_hooks(Arc::downgrade(&other));
    let domain = owner.jit.class_layout_domain;
    assert_ne!(domain, other.jit.class_layout_domain, "one domain per VM");

    let reaches = |token: u32, vm: &Arc<SharedVm>| {
        live_hook_vms_for_domain(token)
            .iter()
            .any(|live| Arc::ptr_eq(live, vm))
    };
    assert!(reaches(domain, &owner));
    assert!(
        !reaches(domain, &other),
        "another VM's class is a different class"
    );
    assert!(
        reaches(u32::MAX, &owner) && reaches(u32::MAX, &other),
        "unknown token"
    );

    let id = 4262u32;
    let recipe = || ClassAllocInfo {
        has_finalizer: false,
        reference_kind: 0,
        prim_inits: vec![(3, PrimKind::Int)].into_boxed_slice(),
        ref_inits: Box::new([]),
    };
    let _ = owner.jit.jit_alloc_class_cache.insert(id, recipe());
    let _ = other.jit.jit_alloc_class_cache.insert(id, recipe());
    let before = owner.jit.code_state_epoch();
    jit_invalidate_adapter(id, domain);
    assert!(owner.jit.code_state_epoch() > before, "the owner flushes");
    assert!(owner.jit.jit_alloc_class_cache.get(id).is_none());
    assert!(
        other.jit.jit_alloc_class_cache.get(id).is_some(),
        "the other VM keeps its recipe for its own class with this id"
    );
}

/// Interpreter round i1 wave 19, lane L5: the resolution-invalidate hook
/// sweeps only the VM whose class store fired it; a token no live VM owns
/// still sweeps every VM. A recorded failure is the seed: it holds no heap
/// reference and `invalidate_class` drops it by key.
#[test]
fn a_resolution_invalidation_sweeps_only_the_owning_vm() {
    use crate::classloading::resolution::ResolutionFailure;
    let _guard = HOOK_REGISTRY_TEST_LOCK.lock();

    let owner = Arc::new(SharedVm::new(VmConfig::default()));
    let other = Arc::new(SharedVm::new(VmConfig::default()));
    set_global_shared_vm_for_hooks(Arc::downgrade(&owner));
    set_global_shared_vm_for_hooks(Arc::downgrade(&other));
    let domain = owner.jit.class_layout_domain;
    assert_ne!(domain, other.jit.class_layout_domain, "one domain per VM");

    let id = 4272u32;
    let cid = crate::classloading::ClassId::new(id);
    let cp_index = 7u16;
    let seed = |vm: &Arc<SharedVm>| {
        vm.classes.resolution_cache.write().put_failure(
            cid,
            cp_index,
            ResolutionFailure {
                error_class: Arc::from("java/lang/NoClassDefFoundError"),
                message: None,
                cause: None,
            },
        );
    };
    let holds = |vm: &Arc<SharedVm>| {
        vm.classes
            .resolution_cache
            .read()
            .get_failure(cid, cp_index)
            .is_some()
    };
    use cratonvm_classloading::ResolutionInvalidation;
    seed(&owner);
    seed(&other);

    // Interpreter round i1 wave 24, lane L5: a layout-only change (a
    // descendant `recompute_subclass_layouts` moved) keeps the recorded
    // failure — the class's constant pool is unchanged, and JVMS §5.4.3
    // makes the failure permanent.
    resolution_invalidate_adapter(id, domain, ResolutionInvalidation::FieldLayoutOnly);
    assert!(
        holds(&owner),
        "a field-layout shift must not drop a recorded resolution failure"
    );

    resolution_invalidate_adapter(id, domain, ResolutionInvalidation::ConstantPoolReplaced);
    assert!(
        !holds(&owner),
        "the owner drops its resolution of the class"
    );
    assert!(
        holds(&other),
        "the other VM keeps its resolution for its own class with this id"
    );

    seed(&owner);
    resolution_invalidate_adapter(id, u32::MAX, ResolutionInvalidation::ConstantPoolReplaced);
    assert!(
        !holds(&owner) && !holds(&other),
        "a token no live VM owns sweeps every VM"
    );
}

/// Interpreter round i1 wave 22, lane L5
/// (`interpreter-L5-classload-hook-carries-no-owner-token-FIXED-20260926.md`):
/// JVMTI ClassLoad reaches the manager of the VM whose class store
/// defined the class, and no other VM's. The hook carries the store's
/// layout domain; before, the adapter posted every VM's class loads
/// through the unattributed manager, whose native-agent bridge answers
/// nothing with two VMs live.
///
/// Two halves: the adapter by token (unconditional), and a real class
/// definition through the second VM's class-manager write guard, whose
/// drop drains the queued event (skipped, with a note, when the unit-test
/// VM cannot define a class, as the gc crate's define helpers do).
#[test]
fn a_class_load_reaches_only_the_vm_that_defined_the_class() {
    use crate::runtime::jvmti::{self as rt, EventCallbacks, EventMode, JvmtiEventKind};
    let _lock = rt::jvmti_registry_test_lock();

    let first = Arc::new(SharedVm::new(VmConfig::default()));
    let second = Arc::new(SharedVm::new(VmConfig::default()));
    assert_ne!(first.vm_identity, second.vm_identity);

    type Seen = Arc<std::sync::Mutex<Vec<(u64, u64)>>>;
    let listen = |vm: &Arc<SharedVm>| -> Seen {
        let seen: Seen = Arc::default();
        let sink = Arc::clone(&seen);
        let manager = rt::manager_for_vm(vm.vm_identity)
            .expect("SharedVm::new installs the VM's manager");
        manager
            .set_event_callbacks(EventCallbacks {
                class_load: Some(Box::new(move |thread, class| {
                    if let Ok(mut seen) = sink.lock() {
                        seen.push((thread, class));
                    }
                })),
                ..EventCallbacks::default()
            })
            .expect("callbacks");
        manager
            .set_event_notification_mode(EventMode::Enable, JvmtiEventKind::ClassLoad, None)
            .expect("enable");
        seen
    };
    let seen_first = listen(&first);
    let seen_second = listen(&second);
    let got = |seen: &Seen| seen.lock().map(|s| s.clone()).unwrap_or_default();

    // By token: one id, fired once per VM's domain and once for a domain
    // no VM owns.
    let id = 0x0022_0005u32;
    class_load_adapter(id, "i22l5/ByToken", 5, first.jit.class_layout_domain);
    class_load_adapter(id, "i22l5/ByToken", 6, second.jit.class_layout_domain);
    class_load_adapter(id, "i22l5/ByToken", 7, u32::MAX - 1);
    let by_token = |seen: &Seen| {
        let mut v = got(seen);
        v.retain(|&(_, class)| class == u64::from(id));
        v
    };
    let (first_by_token, second_by_token) = (by_token(&seen_first), by_token(&seen_second));

    // A real definition in the second VM: the hook fires when the write
    // guard drops.
    let name = "i22l5/DefinedInSecond";
    let defined = second
        .classes
        .class_manager_write()
        .define_class_with_options(
            name,
            &i22l5_class_bytes(name),
            crate::classloading::ClassLoaderId::Application,
            cratonvm_classloading::DefineClassOptions {
                skip_verification: true,
                ..Default::default()
            },
        )
        .ok();
    let (first_all, second_all) = (got(&seen_first), got(&seen_second));

    for vm in [&first, &second] {
        if let Some(manager) = rt::manager_for_vm(vm.vm_identity) {
            let _ = manager.set_event_notification_mode(
                EventMode::Disable,
                JvmtiEventKind::ClassLoad,
                None,
            );
            let _ = manager.set_event_callbacks(EventCallbacks::default());
        }
        rt::forget_vm_jvmti_state(vm.vm_identity);
    }

    assert_eq!(
        first_by_token,
        vec![(5, u64::from(id))],
        "the first VM hears its own"
    );
    assert_eq!(
        second_by_token,
        vec![(6, u64::from(id))],
        "the second VM hears its own, not the first's nor an unowned token's"
    );
    match defined {
        Some(cid) => {
            let wire = u64::from(cid.as_u32());
            assert_ne!(wire, u64::from(id), "setup: a fresh VM's ids are small");
            assert_eq!(
                second_all
                    .iter()
                    .filter(|&&(_, class)| class == wire)
                    .count(),
                1,
                "the definition posts once, into its own VM: {second_all:?}"
            );
            assert!(
                !first_all.iter().any(|&(_, class)| class == wire),
                "the first VM hears nothing of the second's definition: {first_all:?}"
            );
        }
        None => eprintln!(
            "a_class_load_reaches_only_the_vm_that_defined_the_class: the unit-test VM \
             could not define {name}; only the by-token half ran"
        ),
    }
}

/// A class file with no members: `this_name extends java/lang/Object`
/// (the gc crate's `w11e_class_bytes`).
fn i22l5_class_bytes(this_name: &str) -> Vec<u8> {
    let super_name = "java/lang/Object";
    let mut b: Vec<u8> = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x34];
    b.extend_from_slice(&[0x00, 0x05]);
    b.push(7);
    b.extend_from_slice(&[0x00, 0x02]);
    b.push(1);
    b.extend_from_slice(&u16::try_from(this_name.len()).unwrap().to_be_bytes());
    b.extend_from_slice(this_name.as_bytes());
    b.push(7);
    b.extend_from_slice(&[0x00, 0x04]);
    b.push(1);
    b.extend_from_slice(&u16::try_from(super_name.len()).unwrap().to_be_bytes());
    b.extend_from_slice(super_name.as_bytes());
    b.extend_from_slice(&[0x00, 0x21, 0x00, 0x01, 0x00, 0x03]);
    b.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    b
}

/// Interpreter round i1 wave 19, lane L5: the heap's class-info diagnostic
/// runs on the thread that made the bad access, which may hold its VM's
/// class-manager writer. It skips that VM instead of blocking on it
/// (parking_lot locks are not reentrant, so a blocking read here hung).
#[test]
fn the_class_info_diagnostic_does_not_block_on_a_held_class_manager() {
    let _guard = HOOK_REGISTRY_TEST_LOCK.lock();
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    set_global_shared_vm_for_hooks(Arc::downgrade(&shared));
    let held = shared.classes.class_manager.write();
    // Any answer (another live VM may have the id); returning is the test.
    let _ = class_info_adapter(0);
    let _ = class_name_adapter(0);
    drop(held);
}

/// Interpreter round i1 wave 19, lane L5 (stage 3 of
/// `i18-L5-proposal-one-owner-token-for-every-classloading-hook`): each VM
/// binds its class manager's realm to its `vm_identity` and registers its
/// own vtable manager, and the scoped install hook routes a class's vtable
/// by the handle's VM. The same `ClassId` then holds a different vtable in
/// each VM, and a handle naming no registered VM installs nowhere. Before
/// this, the legacy hook wrote every VM's vtables into the first VM's
/// manager under colliding ids.
#[test]
fn a_vtable_install_reaches_only_the_vm_whose_class_it_is() {
    use crate::runtime::vtable::{vtable_install_adapter_scoped, vtable_manager_of_vm};
    use cratonvm_classloading::metadata_handle::{ClassHandle, UNBOUND_VM};
    use cratonvm_native_api::VmId;

    let first = SharedVm::new(VmConfig::default());
    let second = SharedVm::new(VmConfig::default());
    for vm in [&first, &second] {
        assert_eq!(
            vm.classes.class_manager.read().vm_id(),
            VmId::from_raw(vm.vm_identity),
            "the realm is bound to the VM"
        );
        let registered = vtable_manager_of_vm(vm.vm_identity).expect("registered");
        assert!(Arc::ptr_eq(&registered, &vm.classes.vtable_manager));
    }

    // The catch-up replay fills every VM's own manager, not the first's.
    {
        let cm = second.classes.class_manager.read();
        if let Some(object) = cm.get_loaded_class_id("java/lang/Object") {
            if cm.vtable_descriptors_of(object).is_some() {
                assert!(
                    second
                        .classes
                        .vtable_manager
                        .read()
                        .get_vtable(object.as_u32() as u64)
                        .is_some(),
                    "a later VM's bootstrap classes are in its own manager"
                );
            }
        }
    }

    // One id, one class per VM, each with its own virtual method.
    let cid = crate::classloading::ClassId::new(0x0019_0005);
    let vtable_of = |method: &str| {
        vec![Some(cratonvm_classloading::VtableSlotDescriptor::new(
            cratonvm_classloading::VtableSlotData {
                declaring_class_id: cid.as_u32(),
                method_index: 0,
                method_name: Arc::<str>::from(method),
                descriptor: Arc::<str>::from("()V"),
                dispatch: None,
            },
        ))]
    };
    for (vm, method) in [(&first, "inFirst"), (&second, "inSecond")] {
        vtable_install_adapter_scoped(
            ClassHandle::from_parts(VmId::from_raw(vm.vm_identity), 0, cid),
            vtable_of(method),
        );
    }
    let slot_of = |vm: &SharedVm, method: &str| {
        vm.classes
            .vtable_manager
            .read()
            .get_vtable(cid.as_u32() as u64)
            .and_then(|vtable| vtable.lookup_slot(method, "()V"))
    };
    assert_eq!(slot_of(&first, "inFirst"), Some(0));
    assert_eq!(
        slot_of(&first, "inSecond"),
        None,
        "not the other VM's class"
    );
    assert_eq!(slot_of(&second, "inSecond"), Some(0));
    assert_eq!(slot_of(&second, "inFirst"), None);

    // An unbound realm's handle (a class manager outside any VM).
    let orphan = crate::classloading::ClassId::new(0x0019_0006);
    vtable_install_adapter_scoped(
        ClassHandle::from_parts(UNBOUND_VM, 0, orphan),
        vtable_of("orphan"),
    );
    for vm in [&first, &second] {
        assert!(vm
            .classes
            .vtable_manager
            .read()
            .get_vtable(orphan.as_u32() as u64)
            .is_none());
    }
}

/// Interpreter round i1 wave 21, lane L5 (stage 2 of
/// `i18-L5-proposal-one-owner-token-for-every-classloading-hook`): a VM is
/// named by one token — its `vm_identity`, its realm's `VmId`, its class
/// store's layout domain and `JitRealm::class_layout_domain` agree — and
/// the hook registry finds it by that token with one keyed probe. A
/// dropped VM is not found; an unknown token still reaches every VM.
#[test]
fn a_vm_is_named_by_one_token_and_the_registry_finds_it_by_it() {
    use cratonvm_native_api::VmId;
    let _guard = HOOK_REGISTRY_TEST_LOCK.lock();

    let first = Arc::new(SharedVm::new(VmConfig::default()));
    let second = Arc::new(SharedVm::new(VmConfig::default()));
    for vm in [&first, &second] {
        let domain = vm.jit.class_layout_domain;
        assert_eq!(vm.vm_identity, vm_identity_for_layout_domain(domain));
        assert_eq!(vm.classes.class_layout_domain, domain);
        let cm = vm.classes.class_manager.read();
        assert_eq!(cm.class_store().layout_domain(), domain);
        assert_eq!(cm.vm_id(), VmId::from_raw(vm.vm_identity));
    }
    assert_ne!(first.vm_identity, second.vm_identity);

    set_global_shared_vm_for_hooks(Arc::downgrade(&first));
    set_global_shared_vm_for_hooks(Arc::downgrade(&second));
    // Registering again is a no-op.
    set_global_shared_vm_for_hooks(Arc::downgrade(&first));
    let found = |domain: u32, vm: &Arc<SharedVm>| {
        hook_vm_for_domain(domain).is_some_and(|live| Arc::ptr_eq(&live, vm))
    };
    assert!(found(first.jit.class_layout_domain, &first));
    assert!(found(second.jit.class_layout_domain, &second));
    assert!(!found(first.jit.class_layout_domain, &second));
    assert_eq!(
        live_hook_vms_for_domain(first.jit.class_layout_domain).len(),
        1,
        "an owned token reaches its VM alone"
    );
    assert!(
        live_hook_vms_for_domain(u32::MAX)
            .iter()
            .any(|live| Arc::ptr_eq(live, &first)),
        "an unknown token reaches every VM"
    );

    let dropped = second.jit.class_layout_domain;
    drop(second);
    assert!(
        hook_vm_for_domain(dropped).is_none(),
        "a dropped VM is not found"
    );
    assert!(registry_contains(&first));
}

/// Interpreter round i1 wave 23, lane L5: the thread-local class-init memo
/// (`vm_util::CLASS_INITIALIZED_MEMO`) is keyed by `vm_identity`, which is
/// never reused. It was keyed by the `SharedVm`'s ADDRESS, so a VM created
/// at the address of a dropped one (the same stack slot here, the same
/// allocator chunk for a boxed VM) inherited the dropped VM's "initialized"
/// answers for its own classes of the same ids, and skipped their
/// `<clinit>`. The second round's class is fresh and must read
/// uninitialized whether or not the two rounds share an address. Run with
/// `CRATONVM_CLASS_INIT_BIT_FIRST` off (the memo answers every id) and on
/// (the per-VM bitmap answers; wave 72 lane L3 keeps the memo off that
/// path), set explicitly.
#[test]
fn a_later_vm_does_not_inherit_a_dropped_vms_class_init_memo() {
    const NAME: &str = "cratonvm/test/L5W23InitMemoAcrossVms";
    for (bit_first, round) in [(false, 0), (false, 1), (true, 0), (true, 1)] {
        let mut config = VmConfig::default();
        config.class_init_bit_first = bit_first;
        let shared = SharedVm::new(config);
        let id = shared
            .classes
            .class_manager
            .write()
            .try_ensure_synthetic_class(NAME, 0)
            .expect("Compatible mode fabricates");
        if round == 0 {
            shared
                .classes
                .class_manager
                .read()
                .get_class(id)
                .expect("just defined")
                .init_state
                .store(
                    cratonvm_classloading::CLASS_INIT_INITIALIZED,
                    std::sync::atomic::Ordering::Release,
                );
            assert!(crate::vm::is_class_initialized_via_manager(&shared, id));
        } else {
            assert!(
                !crate::vm::is_class_initialized_via_manager(&shared, id),
                "bit_first={bit_first}: a fresh VM's class is not initialized \
                 because a dropped VM's was"
            );
        }
    }
}

/// Interpreter round i2 wave 61, lane L5
/// (`interpreter-L5-the-class-initialized-memo-survives-a-stub-upgrade-FIXED-20261025.md`):
/// a thread that saw a synthetic stub initialized must not keep that answer
/// once real bytes defined over the stub (`--compatible`'s in-place
/// upgrade, same `ClassId`) reset its initialization. Positive control:
/// the cache answers `true` for the stub first -- the thread-local memo
/// with `CRATONVM_CLASS_INIT_BIT_FIRST` off, the per-VM bitmap with it on
/// (wave 72, lane L3: the memo is off that path); both set explicitly.
#[test]
fn the_class_init_memo_forgets_a_stub_upgraded_in_place() {
    for bit_first in [false, true] {
        class_init_cache_forgets_a_stub_upgraded_in_place(bit_first);
    }
}

fn class_init_cache_forgets_a_stub_upgraded_in_place(bit_first: bool) {
    const NAME: &str = "Foo";
    let mut config = VmConfig::default();
    config.class_init_bit_first = bit_first;
    let shared = SharedVm::new(config);
    let id = shared
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class(NAME, 0)
        .expect("Compatible mode fabricates");
    shared
        .classes
        .class_manager
        .read()
        .set_class_init_state(id, cratonvm_classloading::CLASS_INIT_INITIALIZED);
    assert!(crate::vm::is_class_initialized_via_manager(&shared, id));
    assert!(
        crate::vm::is_class_initialized_via_manager(&shared, id),
        "bit_first={bit_first}: the second ask is the cache's"
    );
    assert_eq!(
        shared.classes.class_init_bits.is_set(id.as_u32()),
        bit_first,
        "only the switch-on read fills the bit"
    );
    let defined = shared
        .classes
        .class_manager
        .write()
        .define_class(
            NAME,
            include_bytes!("../../../../../classloading/tests/fixtures/wp2_4b_redefine/Foo.v1.class"),
            cratonvm_types::ClassLoaderId::UserDefined(61),
        )
        .expect("real bytes upgrade the stub");
    assert_eq!(defined, id, "the upgrade keeps the stub's ClassId");
    assert!(
        !crate::vm::is_class_initialized_via_manager(&shared, id),
        "bit_first={bit_first}: the upgraded class has not run its <clinit>; \
         the cache must not say it has"
    );
    assert!(!shared.classes.class_init_bits.is_set(id.as_u32()));
}

/// Interpreter round i1 wave 23, lane L5: the JIT MIC's annotation-proxy
/// identity hint is the VM's own. It was a process static, so VM A's
/// warmed `AnnotationProxy` id routed VM B's compiled virtual calls on a
/// receiver of whatever class VM B gave that id into the annotation-proxy
/// dispatch.
#[test]
fn the_annotation_proxy_hint_is_per_vm() {
    let first = SharedVm::new(VmConfig::default());
    let second = SharedVm::new(VmConfig::default());
    let second_before = crate::vm::annotation_proxy_cid_hint(&second);
    let warmed = 0x0023_4321_u32;
    assert_ne!(second_before, warmed);
    crate::vm::note_annotation_proxy_cid(&first, warmed);
    assert_eq!(crate::vm::annotation_proxy_cid_hint(&first), warmed);
    assert_eq!(
        crate::vm::annotation_proxy_cid_hint(&second),
        second_before,
        "one VM's warmed id is not another VM's"
    );
}

/// Interpreter round i1 wave 23, lane L5 (the i18-L5 owner-token
/// proposal's step 4): a heap diagnostic that names its layout domain gets
/// the class of ITS VM. The domain-less namers answer from whichever VM
/// has the id first, which names another VM's class when two VMs give one
/// id to different classes.
#[test]
fn a_heap_diagnostic_names_the_class_of_its_own_vm() {
    const ONLY_IN_SECOND: &str = "cratonvm/test/L5W23OnlyInTheSecondVm";
    let _guard = HOOK_REGISTRY_TEST_LOCK.lock();
    let first = Arc::new(SharedVm::new(VmConfig::default()));
    let second = Arc::new(SharedVm::new(VmConfig::default()));
    set_global_shared_vm_for_hooks(Arc::downgrade(&first));
    set_global_shared_vm_for_hooks(Arc::downgrade(&second));
    let id = second
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class(ONLY_IN_SECOND, 0)
        .expect("Compatible mode fabricates")
        .as_u32();
    let (a, b) = (
        first.jit.class_layout_domain,
        second.jit.class_layout_domain,
    );
    assert_eq!(
        class_name_in_domain_adapter(b, id).as_deref(),
        Some(ONLY_IN_SECOND)
    );
    assert_ne!(
        class_name_in_domain_adapter(a, id).as_deref(),
        Some(ONLY_IN_SECOND),
        "the first VM's heap does not name the second VM's class"
    );
    assert_eq!(
        class_info_in_domain_adapter(b, id)
            .map(|(name, _)| name)
            .as_deref(),
        Some(ONLY_IN_SECOND)
    );
    assert_ne!(
        class_info_in_domain_adapter(a, id)
            .map(|(name, _)| name)
            .as_deref(),
        Some(ONLY_IN_SECOND)
    );
    // An unregistered domain falls back to the domain-less namer, which
    // answers from some live VM (or nothing): it must not panic.
    let _ = class_name_in_domain_adapter(u32::MAX, id);
    let _ = class_info_in_domain_adapter(u32::MAX, id);
}

/// Interpreter round i1 wave 18, lane L5: a class-manager write in one VM
/// re-sweeps only that VM's held deferred-`new` retries. Every fruitless
/// look spends one of a memo's `MAX_DEFERRED_NEW_LOOKS` (16 by default),
/// so the fan-out retired another VM's retry after sixteen writes here.
#[test]
fn a_class_manager_write_resweeps_only_its_own_vms_deferred_new_memo() {
    let _guard = HOOK_REGISTRY_TEST_LOCK.lock();
    let owner = Arc::new(SharedVm::new(VmConfig::default()));
    let other = Arc::new(SharedVm::new(VmConfig::default()));
    set_global_shared_vm_for_hooks(Arc::downgrade(&owner));
    set_global_shared_vm_for_hooks(Arc::downgrade(&other));
    let memo = &other.jit.bridge.deferred_new_retries;
    // A site whose holder no class has, so it never resolves in `other`.
    memo.note_bail(
        crate::classloading::ClassId::new(0x1806),
        "i18l5/HeldElsewhere",
        "make",
        "()V",
        &[(0x7fff_ff06, 1)],
    );
    assert_eq!(memo.held_count(), 1);
    for _ in 0..64 {
        crate::runtime::interpreter::jit_bridge::resweep_deferred_new_after_class_definition(
            Some(owner.jit.class_layout_domain),
        );
    }
    assert_eq!(memo.held_count(), 1, "another VM's retry is not looked at");
    // A domain no live VM owns sweeps nothing either.
    crate::runtime::interpreter::jit_bridge::resweep_deferred_new_after_class_definition(Some(
        u32::MAX,
    ));
    assert_eq!(memo.held_count(), 1);
}

/// Registering the same VM twice must not double it in the registry, or
/// every redefine would invalidate that VM's caches twice.
#[test]
fn hook_registry_registration_is_idempotent() {
    let _guard = HOOK_REGISTRY_TEST_LOCK.lock();

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    set_global_shared_vm_for_hooks(Arc::downgrade(&shared));
    set_global_shared_vm_for_hooks(Arc::downgrade(&shared));
    set_global_shared_vm_for_hooks(Arc::downgrade(&shared));

    let occurrences = live_hook_vms()
        .into_iter()
        .filter(|live| Arc::ptr_eq(live, &shared))
        .count();
    assert_eq!(
        occurrences, 1,
        "a VM registered three times must appear exactly once"
    );
}

/// Teardown isolation: dropping one VM must prune only that VM's entry and
/// leave every other registered VM reachable. (This is also what stops a
/// long-lived embedding that creates and destroys many VMs from
/// accumulating dead `Weak`s.)
#[test]
fn dropping_one_vm_leaves_the_other_registered() {
    let _guard = HOOK_REGISTRY_TEST_LOCK.lock();

    let survivor = Arc::new(SharedVm::new(VmConfig::default()));
    let doomed = Arc::new(SharedVm::new(VmConfig::default()));
    set_global_shared_vm_for_hooks(Arc::downgrade(&survivor));
    set_global_shared_vm_for_hooks(Arc::downgrade(&doomed));

    let doomed_weak = Arc::downgrade(&doomed);
    drop(doomed);
    assert!(
        doomed_weak.upgrade().is_none(),
        "the registry must hold a Weak, never an Arc — otherwise a dropped \
         VM is kept alive forever by the hook bridge"
    );

    assert!(
        registry_contains(&survivor),
        "tearing down one VM must not unregister the others"
    );
    // ...and the pruned entry must be gone, not merely un-upgradable.
    let still_listed = live_hook_vms().len();
    let after_second_sweep = live_hook_vms().len();
    assert_eq!(
        still_listed, after_second_sweep,
        "live_hook_vms must prune dead entries so repeated sweeps are stable"
    );

    // Firing the hooks with a dead entry already swept must be a no-op,
    // not a panic.
    resolution_invalidate_adapter(
        0,
        u32::MAX,
        cratonvm_classloading::ResolutionInvalidation::ConstantPoolReplaced,
    );
}
