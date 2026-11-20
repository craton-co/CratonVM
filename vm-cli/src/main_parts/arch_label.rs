#[allow(unused_imports)]
use super::super::*;

pub(crate) fn run() -> Result<()> {
    // Phase accounting: everything from here to the `main(String[])` call below
    // is startup. That is wider than `Vm::new` alone, deliberately — the
    // `vm_startup` category is defined as "everything before the application's
    // `main` is entered", and the argument parse, classpath resolution,
    // `-XX:` translation, `args_array` construction and any `-javaagent:`
    // `premain` all happen on this thread before `main` runs. Charging them
    // here is what keeps them out of `unattributed_ns`; the `class_load`,
    // `gc_pause` and `compilation` spans wired inside those paths nest and
    // subtract, so widening the span does not hide them.
    //
    // Closed explicitly with `.end()` immediately before the `main` invocation,
    // not at end of scope. Every `?`/`bail!` between here and there drops it,
    // which charges the startup that did happen and leaves the rest
    // unattributed — the honest reading of "the VM failed to boot".
    let phase_startup = phase::enter(phase::Category::VmStartup);
    // gci i27/ob: the `uptime` decoration of an `-Xlog` refusal line (below).
    let launch_origin = std::time::Instant::now();

    // Install the pre-`std::process::exit` hook on `native_system_exit` /
    // `native_runtime_exit`. A silent `System.exit(N)` during real app boot
    // (e.g. Cassandra NodeTool's airline NPE catch path) otherwise tears the
    // process down before any downstream observer can print state. The hook
    // fires immediately before the process exits; when `CRATONVM_DBG_EXIT=1`
    // is set it dumps the dispatch-trace ring so the last Java method run
    // before the exit is visible in stderr for diagnosis. First-installer
    // wins (OnceLock), so installing it once here at the top of `run()` is
    // sufficient. (Installing the hook here only registers the closure; it
    // doesn't run it, so this can safely stay ahead of the tracing-subscriber
    // init below.)
    cratonvm_native_builtins::lang_system::set_pre_exit_hook(|code| {
        cleanup_staged_archive_copies();
        // obsaudit D12 (2026-07-26) — `-XX:StartFlightRecording`'s
        // `dumponexit` (default true). This is the same pre-exit hook
        // `cleanup_staged_archive_copies` already relies on for "run before
        // the process actually exits" — it covers Java-initiated exit
        // (`System.exit`/`Runtime.exit`/`Runtime.halt`, the common case for
        // a real application shutting down cleanly) via `native_system_exit`/
        // `native_runtime_exit`. `process_vm()` resolves the live VM this
        // late without threading it through the hook's own signature.
        //
        // gc-common w11-f: the dump itself moved to `dump_jfr_recording_on_exit`,
        // which `run()` now also calls after the shutdown hooks on the
        // `main`-returned and uncaught-exception arms. Until then those two arms
        // never wrote the recording.
        if let Some(shared) = cratonvm_vm::native::jni::process_vm() {
            dump_jfr_recording_on_exit(&shared);
            // An attached debugger's `VM_DEATH`, after the shutdown hooks,
            // where HotSpot posts it (interpreter round i1 wave 24; a no-op
            // without a debugger).
            cratonvm_vm::report_debugger_vm_death(&shared);
            // The jvmstat perf-data file (`CRATONVM_PERF_DATA_FILE`;
            // interpreter round i3 wave 81, lane L1).
            cratonvm_vm::remove_perf_data_file_at_exit(&shared);
            // The JVMS §5.4.4 member-access census total, on
            // `CRATONVM_DBG=access` (interpreter round i1 wave 27, lane L5).
            cratonvm_vm::report_member_access_census_at_exit(&shared);
            // The unrecorded-wait-kind census, on
            // `CRATONVM_DBG_UNRECORDED_WAIT_KINDS` (interpreter round i2 wave 66,
            // lane L1).
            cratonvm_vm::report_unrecorded_wait_regions_at_exit(&shared);
            // The inlined-chain census and the eager-chains soak verdict, on
            // `CRATONVM_DBG_DEOPT` (JIT round 15 wave 5, lane chain6).
            cratonvm_vm::runtime::interpreter::report_inlined_chain_census_at_exit(&shared);
            // The poll-stress forced-arrival census (granted / refused by
            // reason), when a stress switch is set (JIT round 21 lane fuzzvm,
            // FUZZTOOL21-1). Once per VM.
            cratonvm_vm::jit::helpers::report_poll_stress_census_at_exit(&shared);
            // `CRATONVM_DBG_VTHREAD_CENSUS`: a probe that ends in
            // `System.exit` never reaches `finish_jdk_only`.
            shared.report_vthread_census_at_exit();        }
        if Err::<String, std::env::VarError>(std::env::VarError::NotPresent).ok().as_deref() == Some("1") {
            eprintln!("=== CRATONVM_DBG_EXIT: System.exit({code}) — dispatch trace ===");
            cratonvm_vm::dispatch_trace::dump_to_stderr_unconditional("pre-system-exit");
        }
        maybe_dump_shutdown_reports();
        write_jdk_only_dumps_on_exit();
    });

    // `java`-launcher positional semantics: insert a `--` separator right
    // after the program selector (`-jar <jar>` or the first bare main-class
    // token) so every token past it is treated as a program argument and
    // passed to the Java application verbatim — even `--help`, `--version`,
    // `--list-modules`. Without this, clap would intercept those anywhere.
    // Runs first so the explicit `--` it parks is honoured by every
    // downstream stage.
    // zt t14/x11: `JDK_JAVA_OPTIONS` goes in after argv[0] ([`launch_argv`]).
    let raw_argv: Vec<String> = launch_argv(std::env::args().collect());
    // zt t18/tg8: this launcher's own help (clap's long help) under its own
    // name, now that the HotSpot help options print HotSpot's texts.
    print_cratonvm_help_if_asked(&raw_argv);
    // zt t12/tg5 (`CRATONVM_LAUNCHER_MISSING_VALUE_REFUSALS`, default ON, `=0`
    // kill): a launcher option without its value (`--add-modules`, `-cp`,
    // `-p`, `--add-modules=`, ...) is the JDK launcher's own refusal, made
    // while it parses its options and so before every VM option's (local
    // 25.0.3: `-Xmx1x --add-modules` names `--add-modules`): one `Error:`
    // line on stderr, exit 1. clap refused it later with exit 2.
    // zt t16/tg7: the same parse finds a `-jar` with nothing after it
    // (`CRATONVM_LAUNCHER_USAGE_TEXT`: its error line and the full usage)
    // and, with `CRATONVM_LAUNCHER_JAR_OPTION_SCAN`, reads on past `-jar`.
    let missing_values = true;
    let jar_rules = JarScanRules {
        bare_jar_usage: true,
        scan_past_jar: launcher_jar_option_scan_enabled(),
    };
    // zt t27/lg1 (`CRATONVM_LAUNCHER_BARE_JAR_XLOG_BOOT`): a bare `-jar` under
    // an `-Xlog` selection prints its error line now and its usage from a
    // booted VM (below).
    let mut bare_jar_xlog_boot = false;
    if missing_values || jar_rules.bare_jar_usage {
        if let Some(line) =
            scan_launcher_missing_value_refusal_with(&raw_argv, missing_values, jar_rules)
        {
            if jar_rules.bare_jar_usage
                && line.starts_with(BARE_JAR_ERROR_LINE)
                && launcher_bare_jar_xlog_boot_enabled()
                && usage_query_boots_vm(&raw_argv)
            {
                eprintln!("{BARE_JAR_ERROR_LINE}");
                let _ = std::io::Write::flush(&mut std::io::stderr());
                bare_jar_xlog_boot = true;
            } else {
                eprintln!("{line}");
                let _ = std::io::Write::flush(&mut std::io::stderr());
                std::process::exit(1);
            }
        }
    }
    if let Some(refusal) = scan_source_launcher_conflict_refusal(&raw_argv) {
        eprintln!("{refusal}");
        let _ = std::io::Write::flush(&mut std::io::stderr());
        std::process::exit(1);
    }
    // The bare `-jar` is the last token (`ARG_CHECK` found nothing after
    // it); without it no program is named, the concise usage's path below.
    let raw_argv = if bare_jar_xlog_boot {
        let mut argv = raw_argv;
        argv.pop();
        argv
    } else {
        raw_argv
    };
    // zt t16/tg7: `-jar` as libjli's launch mode and the separate
    // `--enable-native-access` value, before any later stage reads argv.
    let mut raw_argv = hotspot_launcher_argv_shape(raw_argv);
    // zt t18/tg8 (`CRATONVM_LAUNCHER_HOTSPOT_HELP_TEXT`): a help option that
    // libjli reaches ends the option section there; its text is printed
    // where the concise usage is, below, once the options before it were
    // accepted.
    let hotspot_help = {
        take_hotspot_help_query(&mut raw_argv, launcher_jar_option_scan_enabled())
    };
    // zt t18/tg8 (`CRATONVM_LAUNCHER_VM_OPTION_PARSE_ORDER`): HotSpot's
    // `Arguments::parse` (every VM-option refusal, in command-line order,
    // `--patch-module`'s `Missing '='` among them) and then its heap
    // ergonomics run when the VM is created, before the boot layer resolves
    // a module launch. So the ordered scan and the heap-size checks run here,
    // on the options before the program selector.
    let memory_sizes = launcher_memory_size_options_enabled();
    let ordered_vm_options_early =
        launcher_zgc_flag_hotspot_parse_enabled();
    if ordered_vm_options_early {
        let view = vm_option_parse_view(&raw_argv);
        let rules = ZgcFlagRefusalRules::from_switches(&view);
        let heap_flags = launcher_ordered_heap_flag_refusals_enabled();
        let space_tlab =
            true;
        let more = OrderedVmOptionRules {
            memory_sizes,
            patch_module_missing_equals: launcher_patch_module_refusals_enabled(),
            patch_module_java_base_twice: launcher_patch_module_java_base_twice_enabled(),
            young_size: memory_sizes,
            ..OrderedVmOptionRules::default()
        }
        .with_dev_collector_rules(&view);
        // zt t23/hunt4: the obsolete-flag warnings of the options before the
        // first refused one, in order, ahead of that refusal.
        if launcher_obsolete_flag_warnings_enabled() {
            print_obsolete_flag_warnings(&obsolete_flag_warning_lines(&view, |prefix| {
                scan_first_ordered_vm_option_refusal(prefix, rules, heap_flags, space_tlab, more)
                    .is_some()
            }));
        }
        if let Some(refusal) =
            scan_first_ordered_vm_option_refusal(&view, rules, heap_flags, space_tlab, more)
        {
            refusal.exit();
        }
        validate_early_heap_sizes(&view)?;
    }
    // zt t16/tg7 (`CRATONVM_LAUNCHER_PATCH_MODULE_REFUSALS`): HotSpot's own
    // argument parse refuses a `--patch-module` value without `=` when the
    // VM is created, before the boot layer and before a module launch is
    // resolved. zt t18/tg8: in command-line order in the scan above; here
    // only when that scan does not run (a kill of either switch).
    if launcher_patch_module_refusals_enabled() && !ordered_vm_options_early {
        let options = scan_boot_layer_module_options(&raw_argv);
        // zt t20/tg9: `java.base` given twice is that parse's refusal too.
        if let Some(
            refusal @ (PatchModuleRefusal::MissingEquals | PatchModuleRefusal::JavaBaseTwice),
        ) = patch_module_refusal(&options.patch_module)
        {
            exit_with_patch_module_refusal(&refusal);
        }
    }
    // `-m <module>[/<class>]` / `--module`: HotSpot's module launch, turned
    // into the root module plus a main class before anything else reads
    // argv (interpreter round i4 wave 86, lane L5). A refusal is the JDK's
    // text and exit status 1.
    let raw_argv = match rewrite_module_launch(raw_argv) {
        Ok(argv) => argv,
        Err(refusal) => {
            // The boot-layer abort is the VM's `vm_exit_during_initialization`,
            // which HotSpot writes to STDOUT (measured on Temurin 25.0.3); the
            // `LauncherHelper` texts go to stderr.
            if refusal.starts_with("Error occurred during initialization of boot layer") {
                println!("{refusal}");
                let _ = std::io::Write::flush(&mut std::io::stdout());
            } else {
                eprintln!("{refusal}");            }
            std::process::exit(1);
        }
    };
    let mut argv: Vec<String> = insert_program_args_separator(raw_argv);
    // zs s3/ob2: read before any rewrite of the `-XX:` spellings, in
    // command-line order (see `scan_malformed_gc_thread_flag`).
    let malformed_gc_thread_flag = scan_malformed_gc_thread_flag(&argv);
    // zt t9/tg2 (`CRATONVM_LAUNCHER_ZGC_FLAG_HOTSPOT_PARSE`, default ON, `=0`
    // kill): the launcher's HotSpot refusals of the ZGC `-XX:` flags (and,
    // on ZGC, of the GC thread counts) in command-line order, the first one
    // wins, before the version banner and every later arm; see
    // `scan_first_zgc_flag_refusal`. The arms below and in the parsed path
    // stay as the kill switch's behaviour.
    // zt t10/tg3 (`zgc-zt9tg2-*` item 5,
    // `CRATONVM_LAUNCHER_ORDERED_HEAP_FLAG_REFUSALS`): the tenuring,
    // `SurvivorRatio` and TLAB value refusals join that order.
    // zt t18/tg8: here only when it did not run before the module launch
    // (`CRATONVM_LAUNCHER_VM_OPTION_PARSE_ORDER=0`); the memory size options
    // join it under their own switch either way.
    if launcher_zgc_flag_hotspot_parse_enabled() && !ordered_vm_options_early {
        let rules = ZgcFlagRefusalRules::from_switches(&argv);
        let heap_flags = launcher_ordered_heap_flag_refusals_enabled();
        let space_tlab =
            true;
        let more = OrderedVmOptionRules {
            memory_sizes,
            patch_module_missing_equals: false,
            patch_module_java_base_twice: false,
            young_size: memory_sizes,
            ..OrderedVmOptionRules::default()
        }
        .with_dev_collector_rules(&argv);
        // zt t23/hunt4: as in the early scan above.
        if launcher_obsolete_flag_warnings_enabled() {
            print_obsolete_flag_warnings(&obsolete_flag_warning_lines(&argv, |prefix| {
                scan_first_ordered_vm_option_refusal(prefix, rules, heap_flags, space_tlab, more)
                    .is_some()
            }));
        }
        let refused =
            scan_first_ordered_vm_option_refusal(&argv, rules, heap_flags, space_tlab, more);
        if let Some(refusal) = refused {
            refusal.exit();
        }
    } else if !launcher_zgc_flag_hotspot_parse_enabled()
        && launcher_obsolete_flag_warnings_enabled()
    {
        // zt t23/hunt4: no ordered refusal scan runs
        // (`CRATONVM_LAUNCHER_ZGC_FLAG_HOTSPOT_PARSE=0`): every warning, in order.
        print_obsolete_flag_warnings(&obsolete_flag_warning_lines(&argv, |_| false));
    }
    // zt t2/rv (`zgc-zs4rv3-wave-s3-review-residuals-CLOSED-20261019.md` item 12):
    // HotSpot refuses a malformed GC thread value while it parses the options,
    // so before any of the launcher's later `Warning: ignoring ...` arms (a bad
    // `-XX:SoftMaxHeapSize` printed its warning first). Refused here on the
    // argv scan's own ZGC answer, as the version path below already does; the
    // parsed-path refusal stays as the backstop. Not ZGC: unchanged.
    if let Some(option) = malformed_gc_thread_flag.as_deref() {
        if scan_effective_g1(&argv)
            || (scan_effective_zgc(&argv))
        {
            exit_with_improperly_specified_vm_option(option);
        }
    }
    if let Some(detail) = scan_g1_zero_parallel_gc_threads_refusal(&argv) {
        exit_with_vm_initialization_error(&detail);
    }
    // gcj j1/ob (`CRATONVM_GEN_HOTSPOT_VALUE_FLAGS`, default ON, `=0` kill):
    // zt-dev merge: with `CRATONVM_LAUNCHER_ZGC_FLAG_HOTSPOT_PARSE` on, this
    // typed scan runs INSIDE the ordered first-refusal scan above
    // (`OrderedVmOptionRules::gen_values`), so a typed value refusal and a
    // ZGC / heap / memory-size refusal are named in command-line order; it
    // runs on its own here only when that scan does not.
    // Every shared `-XX:<Name>=<value>` of the typed table is parsed as
    // HotSpot 25 parses it, in command-line order, on every collector (dev
    // cabdb9f4ae), and the first bad one ends the launch with HotSpot's
    // lines -- here, before the version banner (HotSpot parses the options
    // before `-version` prints) and before any lenient `Warning: ignoring
    // ...` arm. Collector-specific normalization remains below on
    // Generational only.
    if !launcher_zgc_flag_hotspot_parse_enabled() {
        let space_tlab =
            true;
        // gcj j2/ob2 (`CRATONVM_GEN_ONE_ORDERED_VALUE_PARSE`, default ON):
        // the same ordered scan also checks the syntax-only rows.
        // gcj j2/ob2 rows on j2a (`CRATONVM_GEN_HOTSPOT_VALUE_RANGES`, default
        // ON): and the captured ranges / `bool` refusals.
        let extra = true;
        let ranges = true;
        // And every other option HotSpot 25's own flag table knows
        // (`CRATONVM_LAUNCHER_HOTSPOT_FLAG_TABLE`, default ON): its lock kind,
        // `bool` / non-`bool` spelling, type and range.
        let refusal = if launcher_hotspot_flag_table_enabled() {
            hotspot_flag_table_refusal(
                &argv,
                space_tlab,
                extra,
                ranges,
                launcher_hotspot_flag_locks_enabled(),
            )
        } else if extra || ranges {
            gen_value_flag_refusal_with(&argv, space_tlab, extra, ranges)
        } else {
            gen_hotspot_value_flag_refusal(&argv, space_tlab)
        };
        if let Some(lines) = refusal {
            exit_with_vm_option_lines(&lines);
        }
    }
    if !ordered_vm_options_early {
        validate_early_heap_sizes(&argv)?;
    }
    // And the G1 `AfterErgo` pause constraints no collector of this VM
    // checks (`CRATONVM_LAUNCHER_HOTSPOT_FLAG_TABLE`).
    if launcher_hotspot_flag_table_enabled() {
        let lines = hotspot_after_ergo_constraint_errors(&argv, scan_hotspot_constraints_are_g1(&argv));
        if !lines.is_empty() {
            exit_with_vm_option_lines(&lines);
        }
        // HotSpot Serial (`GenArguments::initialize_size_info`) refuses a
        // zero `NewRatio` while it sizes the generations (local 25.0.3:
        // stdout, exit 1, before a banner); G1 and ZGC take it.
        let new_ratio_zero = argv
            .iter()
            .skip(1)
            .take_while(|a| a.as_str() != "--")
            .filter_map(|a| a.strip_prefix("-XX:NewRatio="))
            .last()
            .and_then(hotspot_parse_uintx)
            == Some(0);
        if new_ratio_zero && scan_effective_generational(&argv) {
            exit_with_vm_initialization_error("Invalid young gen ratio specified");
        }
    }

    // Version banners are handled here, ahead of clap, for one reason: the
    // banner must name the active JDK mode. clap's built-in `--version`
    // prints and exits before any of our code runs, so it can only report
    // the crate version — which says nothing about which of the two
    // standard libraries the VM would boot. Since that is the single most
    // important fact for interpreting a bug report, `-version`,
    // `--version`, `-fullversion`, `-showversion` and `-Xinternalversion`
    // all route through `version_banner` instead.
    // zr z9/hp: a `-version` / `--version` that also selects `-Xlog` output
    // boots the VM before printing its banner, as HotSpot's launcher does
    // (`CreateJavaVM`, then `VersionProps.print`), so the startup lines the
    // selection asks for -- `[gc] Using The Z Garbage Collector`, the
    // `gc,init` block, and at exit the `gc,exit` summary -- are printed from
    // the heap this VM really builds. Without `-Xlog` nothing would be printed
    // by that boot, so the banner keeps its no-VM fast path.
    // `CRATONVM_VERSION_XLOG_BOOT=0` keeps the fast path always.
    let mut version_boot: Option<VersionBoot> = None;
    if let Some((query, token)) = scan_version_query(&argv) {
        // zs s2/ob: HotSpot creates the VM before any banner, so an explicit
        // `-XX:+UseZGC` with a zero GC thread count fails there first (the
        // banner never prints); the parsed-argument check below cannot see a
        // version query, which exits before argument parsing.
        // zs s3/ob2: a malformed value fails argument parsing, before that.
        if let Some(option) = malformed_gc_thread_flag.as_deref() {
            if scan_effective_g1(&argv)
                || (scan_effective_zgc(&argv))
            {
                exit_with_improperly_specified_vm_option(option);
            }
        }
        if let Some(detail) = scan_zgc_zero_gc_thread_refusal(&argv) {
            exit_with_vm_initialization_error(&detail);
        }
        // zs s9/ob6: and HotSpot's default G1 refuses a zero
        // `ParallelGCThreads` there too, when no collector is named.
        if let Some(detail) = scan_zgc_implicit_zero_parallel_gc_threads_refusal(&argv) {
            exit_with_vm_initialization_error(&detail);
        }
        // zt t10/tg3 (`zgc-zt9tg2-*` item 6): HotSpot checks the soft maximum
        // against the maximum heap after ergonomics, before any banner.
        if scan_soft_max_above_max_heap(&argv) {
            exit_with_soft_max_above_max_heap();
        }
        // gcj j2/ob2 (`CRATONVM_GEN_ONE_ORDERED_VALUE_PARSE`, default ON;
        // residual page item 4): on Generational the `AfterMemoryInit`
        // constraint lines refuse before the banner too, as HotSpot creates
        // the VM before it prints one.
        if scan_effective_generational(&argv)
        {
            let space_tlab = true;
            let lines = gen_version_constraint_errors(&argv, space_tlab);
            if !lines.is_empty() {
                exit_with_vm_option_lines(&lines);
            }
        }
        // dev cabdb9f4ae: HotSpot initializes the VM before a version banner,
        // and `-XX:MaxMetaspaceSize=0` fails that initialization (on every
        // collector). zt t24/m5: after the heap-size, `AfterErgo` and
        // `AfterMemoryInit` checks above (HotSpot's metaspace fails in
        // `universe_init`'s class loading, after the heap is sized), before
        // the boot layer's checks below.
        if early_max_metaspace_size(&argv) == Some(0) {
            exit_with_vm_initialization_error("OutOfMemoryError: Metaspace");
        }
        // zt t16/tg7: the boot layer's own order before the banner:
        // `ModuleBootstrap`'s static initializer decodes `--patch-module`
        // (`CRATONVM_LAUNCHER_PATCH_MODULE_REFUSALS`), then the module path
        // is scanned (`CRATONVM_LAUNCHER_VERSION_MODULE_PATH_CHECK`: `-p <bad>
        // --add-opens= -version` names the module path), then the options
        // below are decoded.
        let boot_layer_options = scan_boot_layer_module_options(&argv);
        if launcher_patch_module_refusals_enabled() {
            if let Some(refusal) = patch_module_refusal(&boot_layer_options.patch_module) {
                exit_with_patch_module_refusal(&refusal);
            }
        }
        {
            if let Some(module_path) = boot_layer_options.module_path.as_deref() {
                if let Some(text) = launcher_module_path_find_error(&argv, module_path) {
                    exit_with_boot_layer_refusal(&format!(
                        "java.lang.module.FindException: {text}"
                    ));
                }
            }
        }
        // zt t14/tg6 (`CRATONVM_LAUNCHER_BOOT_LAYER_OPTION_REFUSALS`): HotSpot
        // boots its module system before the banner, so an `--add-opens=`
        // the boot layer cannot decode ends a `-version` launch too.
        if true {
            if let Some(exception) = boot_layer_module_option_refusal(&boot_layer_options) {
                exit_with_boot_layer_refusal(&exception);
            }
        } else if launcher_hotspot_flag_table_enabled()
            && scan_names_no_collector(&argv)
            && scan_effective_zgc(&argv)
        {
            // `CRATONVM_LAUNCHER_HOTSPOT_FLAG_TABLE`: with no collector named
            // HotSpot's default G1 refuses before its banner as the parsed
            // path does.
            let space_tlab = true;
            let lines =
                version_constraint_errors_for(&argv, space_tlab, Some(early_effective_max_heap(&argv)));
            if !lines.is_empty() {
                exit_with_vm_option_lines(&lines);
            }
        }
        let mode = scan_requested_jdk_mode(&argv);
        let compatibility = scan_requested_compatibility_mode(&argv);
        let java_home = scan_explicit_java_home(&argv);
        // Wave 138, L1b: `-Xint -version` names `interpreted mode`, as the
        // property will (`CRATONVM_LAUNCHER_XINT`, default ON).
        let interpreted = launcher_xint_enabled() && launcher_xint_requested(&argv);
        let banner =
            version_banner_with(query, mode, compatibility, java_home.as_deref(), interpreted);
        let library_resolves = version_library_resolves(mode, java_home.as_deref());
        // gcj j2/ob2 (`CRATONVM_GEN_PRINT_FLAGS_FINAL`, opt-in): a
        // `-XX:+PrintFlagsFinal -version` boots too, so the table prints from
        // the configuration the VM really builds, before the banner.
        if version_query_boots_vm(query, &argv, version_xlog_boot_enabled(), library_resolves)
            || version_explicit_large_pages_boots(query, &argv, library_resolves)
            || version_young_ergo_boots(query, &argv, library_resolves)
            || gen_print_flags_final_boots_version(query, &argv, library_resolves)
            // gcj j1/st item 2: HotSpot prints the String-table statistics as
            // a `-version` VM exits, so `-XX:+PrintStringTableStatistics
            // -version` boots one.
            || (library_resolves
                && query == VersionQuery::Version
                && print_string_table_statistics_requested(&argv))
            || (library_resolves && query == VersionQuery::Version
                && argv.iter().skip(1).take_while(|a| a.as_str() != "--")
                    .any(|a| is_start_flight_recording(a)))
        {
            remove_first_launcher_token(&mut argv, &token);
            version_boot = Some(VersionBoot {
                banner,
                to_stdout: query.to_stdout(&token),
                exit_code: query.exits().then_some(0),
                usage_after: false,
            });
        } else {
            // zt t27/lg1 (`CRATONVM_ZGC_MAX_MAP_COUNT_WARNING`, row
            // `tg8_s2`): ZGC's `max_map_count` warning, which HotSpot's VM
            // creation prints before the banner. Only for the queries that
            // end here (a `-showversion` boots its VM, which prints it).
            if query == VersionQuery::Version {
                print_version_zgc_max_map_count_warning(
                    &argv,
                    launch_origin.elapsed().as_secs_f64(),
                );
            }
            if query.to_stdout(&token) {
                // HotSpot writes the single-dash forms to stderr (build tools
                // scrape `java -version` from there) and the double-dash
                // forms to stdout.
                print!("{banner}");
                let _ = std::io::Write::flush(&mut std::io::stdout());
            } else {
                eprint!("{banner}");
                let _ = std::io::Write::flush(&mut std::io::stderr());
            }
        }
        if version_boot.is_none() {
            if query.exits() {
                std::process::exit(0);
            }
            // `-showversion`: banner printed, now launch the program as normal.
            // Drop the flag so clap doesn't see an unknown option.
            remove_first_launcher_token(&mut argv, &token);
        }
    }

    // HotSpot's `Arguments::jvm_args()` (the perf-data file's
    // `java.rt.vmArgs`), from the leading option section as the user wrote
    // it (interpreter round i4 wave 84, lane L1).
    let jvm_args = hotspot_jvm_args(&argv);
    // `-ea[:<name>]` / `-da[:<name>]` / `-esa` / `-dsa`, per VM (wave 107, L5).
    let assertion_directives = launcher_assertion_directives(&argv);
    if let Some(bad) = refused_assertion_spelling(&argv) {
        // HotSpot 25 `Arguments::parse_each_vm_init_arg`: an option it does not
        // match is "Unrecognized option" on the error stream, JNI_EINVAL, then
        // the launcher's two lines and exit 1. The strip arm in
        // `normalize_java_launcher_argv` dropped these spellings silently.
        eprintln!("Unrecognized option: {bad}");
        eprintln!("Error: Could not create the Java Virtual Machine.");
        eprintln!("Error: A fatal exception has occurred. Program will exit.");
        std::process::exit(1);
    }
    // zt t18/hunt2: `--finalization=` (JEP 421); HotSpot refuses a bad value
    // with JNI_EINVAL, then the launcher's two lines and exit 1.
    let finalization_enabled = match launcher_finalization_option(&argv) {
        Ok(enabled) => enabled.unwrap_or(true),
        Err(bad) => {
            eprintln!("Invalid finalization value '{bad}', must be 'disabled' or 'enabled'.");
            eprintln!("Error: Could not create the Java Virtual Machine.");
            eprintln!("Error: A fatal exception has occurred. Program will exit.");
            std::process::exit(1);
        }
    };

    // zt t12/tg5 (`zgc-zt11tg4-*` item 5): the free ratios in HotSpot's
    // `uintx` syntax on an effective ZGC command line (read before argv is
    // consumed; the collector is selected later in the parsed path).
    let zgc_free_ratio_hotspot_syntax = scan_effective_zgc(&argv)
        && launcher_zgc_flag_hotspot_parse_enabled()
        && launcher_zgc_gc_value_flag_refusals_enabled();
    // zt t21/tg10 (`CRATONVM_LAUNCHER_USAGE_XLOG_BOOT`): read before argv is
    // consumed; used only if a usage or help text is printed below.
    let usage_xlog_boot = usage_query_boots_vm(&argv);
    // gcj j2/ob2 (`CRATONVM_GEN_ONE_ORDERED_VALUE_PARSE`, default ON): on
    // Generational the consumers below read each typed value as the ordered
    // scan above read it, and the inert G1 / ZGC knobs are dropped. After
    // `jvm_args`, which keeps the spellings the user wrote.
    let argv = if true {
        let generational = scan_effective_generational(&argv);
        ordered_parse_argv(argv, generational, launcher_hotspot_flag_table_enabled())
    } else if launcher_hotspot_flag_table_enabled() {
        ordered_parse_argv(argv, false, true)
    } else {
        argv
    };
    // Extract -Dkey=value system properties before clap parsing
    let raw_args: Vec<String> = normalize_java_launcher_argv(argv);
    let (filtered_args, system_properties) = extract_system_properties(raw_args);
    // T6 CLI compat: strip HotSpot-style flags before clap so their
    // non-standard spellings (`-XX:+Foo`, `-agentlib:`) don't confuse it.
    let (filtered_args, hotspot_flags) = extract_hotspot_flags(filtered_args);
    let mut args = Args::parse_from(filtered_args);
    // `CRATONVM_LAUNCHER_HOTSPOT_FLAG_TABLE`: the early scan has checked each
    // `-XX:` value against HotSpot 25's table (type and range) and handed it
    // on in decimal, so the consumers below take every value HotSpot takes
    // (a zero included) without a `Warning: ignoring ...` line, and a knob
    // the effective collector does not read is dropped without a note, as
    // HotSpot drops it.
    let hotspot_table = launcher_hotspot_flag_table_enabled();

    // Publish the census output paths for the `System.exit` path, which never
    // returns to `run()` and so cannot see `args`. Done here, immediately after
    // parsing, because a `System.exit` can happen as early as an agent's
    // `premain`. Nothing is written unless at least one of the three flags was
    // given, so a default run publishes three `None`s and the hook returns on
    // its own guard.
    let _ = JDK_ONLY_EXIT_DUMP_PATHS.set(JdkOnlyExitDumpPaths {
        class_origins: args.dump_class_origins.clone(),
        native_registry: args.dump_native_registry.clone(),
        report: args.jdk_only_report.clone(),
        verbose: args.explain_jdk_only,
    });

    // `--dump-phase-report` was already consumed by the launcher — see
    // `launcher_phase_report_path` for why it has to be read that early. What
    // is left to do here is tell the operator when the two disagree: the
    // option is present, but accounting still resolved to off (an explicit
    // `CRATONVM_PHASE_ACCOUNTING=0`, or a `CRATONVM_DBG=-phase-accounting`),
    // so the file they are waiting for will never appear.
    if args.dump_phase_report.is_some() && !phase::enabled() {
        eprintln!(
            "[cratonvm] --dump-phase-report was given but phase accounting resolved to \
             \"off\"; no report will be written"
        );
    }

    // Initialize tracing. B6: route WARN+ diagnostics to stderr so silent
    // swallow sites surface without polluting the program's stdout (which
    // Java's System.out also writes to).
    //
    // This used to be the very first thing in `run()`, ahead of CLI
    // parsing. It's now built after `Args::parse_from` above so the
    // `--print-gpu-decisions` handling below can consult `args`. Nothing in
    // between the old and new init point ever logs through `tracing`
    // (`expand_argfiles`, `insert_program_args_separator`,
    // `normalize_java_launcher_argv`, `extract_system_properties`,
    // `extract_hotspot_flags`, and `set_pre_exit_hook` all checked — the
    // pre-exit hook only *installs* a closure here, it doesn't run it), so
    // moving the subscriber install past them drops no log lines.
    // THE WARN FLOOR IS A DEFAULT, AND IT HAS TO STOP BEING ONE WHEN THE USER
    // NAMES A GLOBAL LEVEL.
    //
    // `add_directive(Level::WARN)` adds a TARGETLESS directive. `RUST_LOG=info`
    // parses to a targetless directive too, and `EnvFilter` resolves a
    // same-target tie by last-added-wins — so adding the floor unconditionally
    // silently DOWNGRADED `RUST_LOG=info` back to WARN. Measured on
    // 2026-09-22: `RUST_LOG=info cratonvm <workload>` emitted zero INFO events.
    //
    // That is not a cosmetic default. `jit/src/platform.rs`'s
    // `REVIEW-NOTE 3: measure before flipping the default` tells a reader to run
    // `CRATONVM_JIT_CODE_ARENA=1 RUST_LOG=info` and read the arena census off
    // stderr, and the census is `tracing::info!`. Anyone following that protocol
    // verbatim got nothing, which is byte-identical to the bug
    // `docs/feature-designs/real-jdk-mode.md`
    // is about — the call was wired in wave 6 and the protocol still did not
    // work, because the thing swallowing the census had moved from the call site
    // to the filter. A measurement protocol that silently produces no output is
    // worse than one that produces the wrong output.
    //
    // So: the floor is added only when `RUST_LOG` names no global level of its
    // own. A targeted directive (`RUST_LOG=cratonvm_vm::runtime::offload=debug`)
    // still gets the floor, which is what keeps warnings from other targets
    // visible; a bare `RUST_LOG=info`, `RUST_LOG=debug` or
    // `RUST_LOG=trace,hyper=warn` is now obeyed.
    let rust_log_sets_a_global_level =
        rust_log_names_a_global_level(std::env::var("RUST_LOG").ok().as_deref());
    let mut env_filter = tracing_subscriber::EnvFilter::from_default_env();
    if !rust_log_sets_a_global_level {
        env_filter = env_filter.add_directive(DEFAULT_LOG_FLOOR.into());
    }

    // `--gpu --print-gpu-decisions` was a silent no-op: the decision lines
    // it promises (one per analyzer verdict) are emitted via
    // `tracing::info!`/`tracing::debug!` in `vm/src/runtime/offload.rs`
    // (`lookup_or_compile`'s "if self.print_decisions" block, and
    // `try_dispatch`'s "ran on device" / "fell back to CPU" lines) under
    // the module-path target `cratonvm_vm::runtime::offload` (crate name is
    // `cratonvm-vm` per vm/Cargo.toml `[package] name`; Cargo/rustc turns
    // `-` into `_` for the actual crate identifier used as a tracing
    // target). The WARN-only default filter above swallows all of that
    // unless the user separately exports RUST_LOG — the flag shouldn't
    // require knowing that.
    //
    // Add a low-priority default that opens up this one target. This is
    // *not* a global verbosity bump: EnvFilter picks the most specific
    // matching directive per callsite by target length, independent of add
    // order, so any RUST_LOG directive for a different target (crate-wide,
    // a sibling module, or this same target at a different level written
    // with a different target string) composes normally on top of this.
    // The one exception is a RUST_LOG directive for this *exact* target
    // string (e.g. `RUST_LOG=cratonvm_vm::runtime::offload=error`): that
    // ties in specificity with the directive added below, and EnvFilter
    // resolves same-target ties by last-added-wins rather than stacking
    // them — since this runs after `from_default_env()` has already parsed
    // RUST_LOG, this directive would win that narrow case. Not worth extra
    // machinery to special-case an already-obscure override.
    //
    // NOTE: the workspace pins `tracing` with the `release_max_level_info`
    // feature (Cargo.toml ~line 46), which compiles `tracing::debug!` down
    // to a no-op in release builds. So in a release build only the
    // INFO-level analyzer-verdict lines from `lookup_or_compile` can ever
    // appear here, by design/compile-time elision — the DEBUG-level "ran on
    // device" / "fell back to CPU" lines in `try_dispatch` simply aren't in
    // the binary to enable. Asking for `debug` below is still correct: it's
    // the right ceiling for debug builds, and a harmless no-op ceiling in
    // release builds.
    #[cfg(feature = "gpu")]
    {
        if args.print_gpu_decisions {
            env_filter = env_filter.add_directive("cratonvm_vm::runtime::offload=debug".parse()?);
        }
    }

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .init();

    // Preserve the module list. A coarse all-modules grant would grant EVERY
    // module and silently turn ALL-UNNAMED into a named grant.
    //
    // zt t18/df7: carried by the VM's configuration (`VmConfig::native_access`,
    // set on `config` below), which the VM installs into its own state when
    // it is made -- before `Vm::new(config)` executes any bytecode, so no
    // Panama downcall, `MemorySegment.ofAddress`, or `reinterpret` can observe
    // the closed gate once the user opted in. Process globals before.
    let mut native_access = cratonvm_native_builtins::panama::NativeAccessConfig::default();
    if let Some(modules) = &args.enable_native_access {
        native_access.policy =
            cratonvm_native_builtins::panama::NativeAccessPolicy::from_module_list(
                modules.split(','),
            );
    }

    // --illegal-native-access: what a RESTRICTED method does without a grant.
    // Read beside the grant above and for the same reason — both must be in
    // place before any bytecode runs, since the first restricted call may come
    // from a static initialiser whose failure JVMS 5.5 makes permanent. A bad
    // value still ends the launch here, before anything else runs.
    {
        use cratonvm_native_builtins::panama::IllegalNativeAccess as Ina;
        let mode = match args.illegal_native_access.as_str() {
            "allow" => Ina::Allow,
            "deny" => Ina::Deny,
            "warn" => Ina::Warn,
            other => {
                eprintln!(
                    "Error: Value '{other}' not recognised for --illegal-native-access; \
                     expected one of allow, warn, deny"
                );
                std::process::exit(1);
            }
        };
        native_access.illegal = mode;
    }

    // --enable-preview: JVMS 4.1 preview class files (`minor_version == 65535`
    // at the running major version) are refused by the reader unless this is
    // set, matching HotSpot, whose default is also off. Two consumers must see
    // the same bit or the VM would refuse to load a preview class file while
    // telling the class library that preview is enabled:
    //   * cratonvm_reader::set_preview_enabled — the class-file parser's gate;
    //   * jdk/internal/misc/PreviewFeatures.isPreviewEnabled — read by
    //     `Class.isUnnamedClass()` and therefore by JUnit's launcher.
    // Measured on Adoptium 25.0.3.9: `PreviewFeatures.isEnabled` is false
    // without the flag and true with it, so the two really are one bit. Set
    // through `cratonvm_native_builtins` rather than the reader directly
    // because `vm-cli` does not depend on `cratonvm-reader` at all — and
    // because one entry point is what keeps the two consumers agreeing.
    //
    // Interpreter round i5 wave 101, lane L5: both consumers read ONE VM's
    // `VmConfig::enable_preview` now (set below, with the other config
    // fields), not a process global the launcher pushed into the reader: an
    // embedding (`JNI_CreateJavaVM`, `cratonvm-embed`) can enable it too, and
    // two VMs of one process may differ.

    // -XX:+ShowCodeDetailsInExceptionMessages (JEP 358): publish to the
    // env_cache so the interpreter's helpful-NPE opcode gate observes it on
    // first read, before Vm::new(config) runs any bytecode (same OnceLock
    // timing rationale as --nojit above). CRATONVM_HELPFUL_NPE_OPCODES, if set,
    // still overrides it.
    // Tri-state: an absent flag resolves to the default-on (matching
    // `VmConfig::default`); `-XX:-...` (→ `=false`) opts out.
    cratonvm_vm::runtime::env_cache::set_show_code_details_in_exception_messages(
        args.show_code_details_in_exception_messages.unwrap_or(true),
    );

    // GPU handlers — only compiled when the `gpu` Cargo feature is on.
    // Without the feature, the CPU execution path below is reached
    // unconditionally and unchanged.
    #[cfg(feature = "gpu")]
    {
        // How an architecture prints, per vendor. `sm_75` and `gfx1100`
        // are the strings their own toolchains use; inventing a common
        // spelling would make both wrong.
        //
        // The `device <n>:` PREFIX is load-bearing and must not move:
        // `tools/bench/bench-gpu/ci-gate.sh`'s gate (a) greps
        // `^device [0-9]+:` to tell "driver is healthy" from "driver
        // vanished", because `--gpu-info` exits 0 either way. Keeping
        // it means that gate works unchanged on an AMD box.
        fn arch_label(caps: &cuda_bridge::DeviceCaps) -> String {
            let mut label = match &caps.arch {
                cuda_bridge::DeviceArch::Cuda { major, minor } => format!("sm_{major}{minor}"),
                cuda_bridge::DeviceArch::Hip(hip) => {
                    format!("{}, wave {}", hip.gcn_arch_name, hip.warp_size)
                }
                cuda_bridge::DeviceArch::OpenCl(opencl) => {
                    format!(
                        "OpenCL {}.{}, warp {}",
                        opencl.opencl_version.0, opencl.opencl_version.1, opencl.warp_size
                    )
                }
                cuda_bridge::DeviceArch::None => "unknown arch".to_string(),
            };
            if caps.supports_shared_memory() {
                label.push_str(", shared-memory APU");
            }
            label
        }

        if args.gpu_info {
            match cuda_bridge::probe() {
                Ok(caps) => {
                    println!(
                        "device {}: {} ({}), {:.2} GiB [{} backend]",
                        caps.ordinal,
                        caps.name,
                        arch_label(&caps),
                        (caps.total_global_mem as f64) / (1024.0 * 1024.0 * 1024.0),
                        cuda_bridge::backend_kind().as_str(),
                    );
                    if let Ok(v) = cuda_bridge::toolchain_version() {
                        match v {
                            cuda_bridge::ToolchainVersion::CudaDriver(raw) => {
                                println!("  driver: CUDA {}.{}", raw / 1000, (raw % 1000) / 10);
                            }
                            cuda_bridge::ToolchainVersion::HipRuntime {
                                runtime,
                                hiprtc: (major, minor),
                            } => {
                                println!("  runtime: HIP {runtime}, hiprtc {major}.{minor}");
                            }
                            cuda_bridge::ToolchainVersion::OpenCl { major, minor } => {
                                println!("  runtime: OpenCL {major}.{minor}");
                            }
                        }
                    }
                }
                Err(e) => {
                    println!(
                        "no {} device available: {e}",
                        cuda_bridge::backend_kind().as_str()
                    );
                }
            }
            return Ok(());
        }

        if args.gpu {
            match cuda_bridge::probe() {
                Ok(caps) => {
                    info!(
                        "gpu offload enabled on device {}: {} ({}) via the {} backend",
                        args.gpu_device,
                        caps.name,
                        arch_label(&caps),
                        cuda_bridge::backend_kind().as_str(),
                    );
                }
                Err(e) => {
                    eprintln!(
                        "[cratonvm-cli] --gpu requested but no {} driver available ({e}); \
                         running on CPU",
                        cuda_bridge::backend_kind().as_str()
                    );
                    args.gpu = false;
                }
            }
        }
    }

    // Strip a literal `--` separator that clap parked in the trailing
    // positional list. We pass `--` through to clap so it knows where
    // program args begin (so tokens like `-mp` aren't mis-parsed as
    // short flags), but the Java program must not see `--` itself in
    // its String[] args. Without this, jboss-modules' Main.main reads
    // args[0] == "--" instead of "-mp" and fails inside getServiceName
    // with NullPointerException.
    if args.class_name.as_deref() == Some("--") {
        args.class_name = None;
    }
    // Preserve every `--` that survives clap, including a trailing one.
    //
    // clap already consumes the first lone `--` as its option-parsing
    // terminator, so in the normal-class and `-jar` launch modes no launcher
    // `--` ever reaches `args.args` — any `--` left there is genuinely a
    // program argument and MUST be delivered to `main` verbatim (stock
    // `java Main a -- b` gives the program `["a", "--", "b"]`, and stock
    // `java Main a --` gives the program `["a", "--"]`; many CLI tools use
    // their own `--` end-of-options convention).
    //
    // [LOW arg-parse fix (1)] A previous version unconditionally popped a
    // trailing `--` here, on the theory that the JBoss-Modules `-mp` path
    // (`normalize_java_launcher_argv` prepends an extra `--` so `-mp` isn't
    // mis-parsed as the `-m`/`-p` short-flag cluster) could leak a spurious
    // trailing `--` as the LAST element of `args.args`. That artifact no
    // longer occurs: `-mp` is in `VALUE_TAKING_OPTS`, so
    // `insert_program_args_separator` consumes its operand instead of
    // injecting a separator, and the `-mp` token itself lands in
    // `class_name` (not `args.args`) — see the
    // `launcher_trailing_double_dash_artifact_is_popped` regression test.
    // The unconditional pop therefore had no remaining legitimate target and
    // instead silently dropped a genuine user trailing `--` (e.g.
    // `java Main a --`), violating JDK launcher semantics. Leave a trailing
    // `--` in place so it reaches the program's `String[] args` verbatim.

    // zr z9/hp: a version boot runs no program: whatever selector the
    // command line names is neither opened nor validated (HotSpot's
    // `-version` exits before the launcher loads a main class or a jar).
    if version_boot.as_ref().is_some_and(VersionBoot::exits) {
        args.class_name = None;
        args.jar = None;
        args.args.clear();
    }
    // zt t18/tg8 (`CRATONVM_LAUNCHER_HOTSPOT_HELP_TEXT`): HotSpot's
    // `PrintUsage` for a help option, from the same point as the concise
    // usage below (argv was cut at the help option, so no program is named).
    if let Some(query) = hotspot_help {
        // zt t21/tg10 (`CRATONVM_LAUNCHER_USAGE_XLOG_BOOT`): with an `-Xlog`
        // selection, from a booted VM (the version boot's path below).
        if !usage_xlog_boot {
            query.print_and_exit();
        }
        version_boot = Some(VersionBoot {
            banner: format!("{}\n", query.text()),
            to_stdout: query.to_stdout(),
            exit_code: Some(0),
            usage_after: false,
        });
        args.class_name = None;
        args.jar = None;
        args.args.clear();
    }
    // Validate: exactly one of class_name or --jar must be provided
    if args.class_name.is_none()
        && args.jar.is_none()
        && version_boot.as_ref().is_none_or(|boot| !boot.exits())
    {
        // zt t16/tg7 (`CRATONVM_LAUNCHER_USAGE_TEXT`): libjli's `*pwhat ==
        // NULL` arm: exit status 1 and the concise usage on stderr, printed
        // after the VM options were accepted (HotSpot prints it from the
        // created VM).
        // zt t27/lg1 (`CRATONVM_LAUNCHER_BARE_JAR_XLOG_BOOT`): a bare `-jar`
        // (its error line already printed) gets the full usage, not the
        // concise one.
        let usage_text = if bare_jar_xlog_boot {
            hotspot_full_usage_text()
        } else {
            hotspot_concise_usage_text()
        };
        if let Some(boot) = version_boot.as_mut() {
            // zt t24/m5: a `-showversion` boot (dev's `exits == false`):
            // its banner from the booted VM, then the usage, exit 1.
            boot.usage_after = true;
            boot.exit_code = Some(1);
        } else if !usage_xlog_boot {
            // zt t21/tg10 (`CRATONVM_LAUNCHER_USAGE_XLOG_BOOT`): with an
            // `-Xlog` selection, from a booted VM (the version boot's path
            // below), exit status 1.
            eprintln!("{usage_text}");
            let _ = std::io::Write::flush(&mut std::io::stderr());
            std::process::exit(1);
        } else {
            version_boot = Some(VersionBoot {
                banner: format!("{usage_text}\n"),
                to_stdout: false,
                exit_code: Some(1),
                usage_after: false,
            });
        }
    }
    // When both --jar and positional arguments are given, treat the
    // positionals as program args (Java-style).  This matches the stock
    // `java -jar foo.jar arg1 arg2` behaviour.
    if args.class_name.is_some() && args.jar.is_some() {
        // Shuffle: move class_name into args[0], then args, then the rest.
        let cn = args.class_name.take().unwrap();
        let mut new_args = vec![cn];
        new_args.extend(std::mem::take(&mut args.args));
        args.args = new_args;
    }

    let is_source_mode = args.source.is_some()
        || (args.jar.is_none()
            && args.class_name.as_deref().is_some_and(|cn| {
                cn.ends_with(".java") && std::path::Path::new(cn).is_file()
            }));

    // Resolve class name and classpath based on launch mode. If a WAR/EAR is
    // staged as a temporary `.jar` copy, keep the cleanup guard alive until
    // `run()` exits so lazy class loading can still read it.
    let mut _staged_archive_cleanup: Option<StagedArchiveCleanup> = None;
    let (class_name, classpath) = if let Some(jar_path_str) = &args.jar {
        // -jar mode: read Main-Class from manifest, build classpath from JAR + manifest Class-Path
        let jar_path = std::path::Path::new(jar_path_str);
        // HotSpot's `java.c` texts for a jar it cannot open or parse, the
        // path as given (interpreter round i2 wave 59, lane L5; probe
        // `L5W59LauncherJarErrors`).
        // Printed as `java.c` prints them, with no `run()` error prefix, and
        // exit 1 (interpreter round i2 wave 60, lane L5; probe
        // `L5W60LauncherJarManifest`).
        if !jar_path.exists() {
            exit_with_launcher_error(&format!("Error: Unable to access jarfile {jar_path_str}"));
        }

        let Some(manifest) = ClassPath::read_jar_manifest(jar_path) else {
            exit_with_launcher_error(&format!("Error: Invalid or corrupt jarfile {jar_path_str}"));
        };

        // JN3: ClassPath::new only accepts entries whose extension is `.jar`
        // (or `.jmod`/`modules`). WARs, EARs, and other Java archive types
        // are silently dropped — so `--jar jenkins.war` would never have
        // its contents indexed and `executable.Main` (the launcher class
        // declared in `Main-Class`) could not be resolved.
        //
        // Work around this in the CLI by materialising any non-`.jar`
        // archive as a sibling temp file with a `.jar` extension and
        // adding that path to the classpath instead. The original `jar_path`
        // is still used for manifest parsing (which doesn't care about the
        // extension), and the `Class-Path` manifest header is resolved
        // relative to the original file's parent so sibling lookups still
        // work.
        let (cp_entry_for_archive, staged_cleanup) = classpath_entry_for_archive(jar_path)?;
        _staged_archive_cleanup = staged_cleanup;

        // Build classpath: JAR (or staged .jar copy) itself + manifest Class-Path entries.
        // The manifest's Class-Path header is still resolved relative to the
        // user-supplied path so sibling JARs are found at their real locations.
        let mut cp = vec![cp_entry_for_archive.to_string_lossy().into_owned()];
        // A `Class-Path` token with an unknown URL scheme (`C:/x.jar`) makes
        // HotSpot's `URLClassPath` skip the whole jar, its `Class-Path` jars
        // included (`ClassPath::new` skips the jar itself; wave 107, L5).
        if !manifest.class_path_drops_jar() {
            cp.extend(manifest.resolve_class_path(jar_path));
        }
        // Nothing else joins the application class path. A Quarkus fast-jar's
        // `quarkus-run.jar` lists only `lib/boot/*` in its `Class-Path`; the
        // application and generated classes (`app/`, `quarkus/`, `lib/main/`)
        // belong to `RunnerClassLoader`, which reads them itself. An earlier
        // launcher walk added those directories to the application loader too,
        // so the parent-first probe `RunnerClassLoader` makes for a package it
        // shares with a boot jar found the generated bean in `app` instead of
        // defining it, and the container failed with a `ClassCastException`
        // between two copies of one class (HotSpot's `app` loader throws
        // `ClassNotFoundException` there).

        // `java.c`'s `JLI_ParseManifest` refuses a jar with no manifest entry
        // before the VM starts (`Invalid or corrupt jarfile`, the path as
        // given); a manifest without `Main-Class` is `LauncherHelper`'s
        // `java.launcher.jar.error3`, whose `{0}` is the `JarFile`'s name
        // (`java.io.File`'s normalised path: `\` on Windows). Interpreter
        // round i2 wave 60, lane L5
        // (`interpreter-L5-launcher-residual-message-differences-FIXED-20261024`).
        let Some(main_class) = manifest.main_class else {
            if ClassPath::jar_has_manifest_entry(jar_path) == Some(false) {
                exit_with_launcher_error(&format!(
                    "Error: Invalid or corrupt jarfile {jar_path_str}"
                ));
            }
            exit_with_launcher_error(&format!(
                "no main manifest attribute, in {}",
                launcher_jar_file_name(jar_path_str)
            ));
        };

        if args.classpath.is_some() {
            eprintln!("Warning: -cp/-classpath is ignored when --jar is used");
        }

        let class_name = main_class.replace('.', "/");
        let cp = expand_aggregate_jars(cp);
        (class_name, cp)
    } else if is_source_mode {
        // Single-file or multi-file source-code program (JEP 330, JEP 458).
        let source_file = args.class_name.take().unwrap_or_default();
        args.args.insert(0, source_file);
        let class_name = "com/sun/tools/javac/launcher/SourceLauncher".to_string();
        let cp = if let Some(cp) = args.classpath.as_ref() {
            VmConfig::parse_classpath(cp)
        } else if let Ok(env_cp) = std::env::var("CLASSPATH") {
            VmConfig::parse_classpath(&env_cp)
        } else {
            Vec::new()
        };
        let cp = expand_aggregate_jars(cp);
        (class_name, cp)
    } else {
        // Class name mode (no class at all under a version boot).
        let cn = args
            .class_name
            .as_deref()
            .unwrap_or_default()
            .replace('.', "/");
        let cp = if let Some(cp) = args.classpath.as_ref() {
            VmConfig::parse_classpath(cp)
        } else if let Ok(env_cp) = std::env::var("CLASSPATH") {
            VmConfig::parse_classpath(&env_cp)
        } else {
            Vec::new()
        };
        let cp = expand_aggregate_jars(cp);
        (cn, cp)
    };

    // -Xverify:* takes precedence over --noverify when both are present
    // (matches HotSpot, where the more specific flag wins).
    let xverify_mode = if let Some(spec) = args.xverify.as_deref() {
        match cratonvm_vm::config::XverifyMode::parse(spec) {
            // `All` used to parse, be stored, and be read by nothing, so the
            // launcher warned rather than silently telling a user asking for
            // maximum verification that they had it. It is wired now
            // (`ClassManager::set_strict_verification`, from `vm_init`): the
            // boot image is typestate-verified strictly, the Pass-3 deferral
            // for user-loader classes is withdrawn, and the link-time
            // bootstrap skip is off. See `config::XverifyMode`.
            Some(m) => Some(m),
            None => {
                eprintln!(
                    "Warning: ignoring unknown -Xverify mode {spec:?}; expected none|remote|all"
                );
                None
            }
        }
    } else {
        None
    };

    // The launcher's starting config. `for_launcher()` is deterministic:
    // real-JDK mode (`LAUNCHER_DEFAULT_JDK_MODE`) regardless of what is
    // installed on this machine. The actual mode — including validation and
    // the `--synthetic-jdk` / `--real-jdk` override — is resolved below,
    // after `--java-home` has been parsed. The library path
    // (`VmConfig::default`) stays synthetic so embedded callers and the
    // in-tree test suite are unaffected; that split is declared by
    // `EMBEDDED_DEFAULT_JDK_MODE` in `vm/src/config.rs`.
    let mut config = VmConfig::for_launcher()
        .with_classpath(classpath)
        .with_verbose_class_loading(args.verbose_class)
        .with_verbose_gc(args.verbose_gc)
        .with_skip_verification(args.noverify);
    // zt t18/df7: `--enable-native-access` / `--illegal-native-access`, read above.
    config.native_access = native_access;

    // Record the user-supplied `-jar` path so `java.class.path` is set
    // to the bare jar (HotSpot contract), not the manifest-expanded
    // transitive classpath. See `VmConfig::launcher_jar` for the full
    // rationale — Liberty/Quarkus boot launchers reflect on this.
    if let Some(jar) = args.jar.as_deref() {
        config = config.with_launcher_jar(jar.to_string());
    }

    if let Some(mode) = xverify_mode {
        config = config.with_xverify_mode(mode);
    }

    // --enable-preview: this VM's bit (interpreter round i5 wave 101, lane
    // L5), read by the class manager's define gate and by
    // `PreviewFeatures.isPreviewEnabled`; see the note where the process
    // switch used to be set, above.
    config.enable_preview = args.enable_preview;

    // Forward the GPU-offload CLI flags into VmConfig. Only compiled
    // when the `gpu` Cargo feature is on; without the feature these
    // fields do not exist on VmConfig (see vm/src/config.rs).
    #[cfg(feature = "gpu")]
    {
        let parse_gpu_quota_bytes = |raw: &str, flag: &str| -> u64 {
            if raw.trim() == "0" {
                return 0;
            }
            match parse_size(raw) {
                Some(bytes) => bytes as u64,
                None => {
                    eprintln!("[cratonvm-cli] invalid size for {flag}: {raw}");
                    std::process::exit(1);
                }
            }
        };
        config.gpu_offload_enabled = args.gpu;
        config.gpu_device_ordinal = args.gpu_device;
        config.gpu_min_work = args.gpu_min_work;
        config.print_gpu_decisions = args.print_gpu_decisions;
        config.gpu_approx_math = args.gpu_approx_math;
        if let Some(raw) = args.gpu_vram_soft_quota.as_deref() {
            config.gpu_vram_soft_quota_bytes =
                parse_gpu_quota_bytes(raw, "--gpu-vram-soft-quota");
        }
        if let Some(raw) = args.gpu_vram_hard_quota.as_deref() {
            config.gpu_vram_hard_quota_bytes =
                parse_gpu_quota_bytes(raw, "--gpu-vram-hard-quota");
        }
        if let Some(raw) = args.gpu_pin_soft_quota.as_deref() {
            config.gpu_pin_soft_quota_bytes = parse_gpu_quota_bytes(raw, "--gpu-pin-soft-quota");
        }
        if let Some(raw) = args.gpu_pin_hard_quota.as_deref() {
            config.gpu_pin_hard_quota_bytes = parse_gpu_quota_bytes(raw, "--gpu-pin-hard-quota");
        }
        if let Some(max_spans) = args.gpu_pin_max_spans {
            config.gpu_pin_max_spans = max_spans;
        }
    }

    // `-Xbootclasspath/a:` / `/p:` (see `normalize_java_launcher_argv`).
    // `CRATONVM_XBOOTCLASSPATH_APPEND=0` keeps the pre-wave-10 mapping: either
    // spelling REPLACES the boot class path, as `--Xbootclasspath` does.
    let xbootclasspath_append =
        true;
    if xbootclasspath_append && args.boot_classpath_prepend.is_some() {
        // HotSpot 25 `Arguments::parse_each_vm_init_arg`: the message on the
        // VM output stream (stdout), JNI_EINVAL, then the launcher's
        // `JVM_ERROR1` on stderr and exit status 1.
        println!("-Xbootclasspath/p is no longer a supported option.");
        eprintln!("Error: Could not create the Java Virtual Machine.");
        eprintln!("Error: A fatal exception has occurred. Program will exit.");
        std::process::exit(1);
    }
    let legacy_boot_replace = if xbootclasspath_append {
        None
    } else {
        args.boot_classpath_prepend
            .clone()
            .or_else(|| args.boot_classpath_append.last().cloned())
    };
    if let Some(bcp) = args
        .boot_classpath
        .as_ref()
        .or(legacy_boot_replace.as_ref())
    {
        config = config.with_boot_classpath(VmConfig::parse_classpath(bcp));
    }
    if let Some(jh) = &args.java_home {
        // Fail fast when --java-home points at a path that doesn't exist.
        // Without this, `resolve_java_home` silently returns None, the boot
        // classpath ends up empty, and the first JDK-class reference (e.g.
        // `INVOKESTATIC java/lang/Boolean.parseBoolean`) surfaces as a
        // confusing `NoSuchMethodError` instead of a clear configuration error.
        let p = std::path::Path::new(jh);
        if !p.is_dir() {
            // Under `--real-jdk` this must carry the strict framing. Contract §8
            // (docs/book/src/user-guide/real-jdk-mode.md) requires the JdkOnly
            // failure to name "--real-jdk, the searched paths and the accepted
            // JDK layout".
            //
            // MEASURED (G82-1): the sibling branch — a `--java-home` that EXISTS
            // but is not a runtime image — does all three, via
            // `require_jdk_image_for_jdk_only`. This one never reaches it: a
            // nonexistent path fails here, during argument parsing, so the run's
            // own trailer reads `jdk mode: <not yet resolved>` and the message
            // mentions neither the policy nor why there is no fallback. That is
            // the branch a TYPO takes, i.e. the commonest way to arrive here.
            //
            // Both branches refuse and neither fabricates, so closure rule 5 held
            // either way; this is about §8's wording, which only one of them met.
            if args.real_jdk {
                anyhow::bail!(
                    "--real-jdk: --java-home path does not exist or is not a \
                     directory: {jh}\n\
                     Under --real-jdk real class bytes are authoritative, so there \
                     is nothing to fall back to — the run cannot continue without a \
                     real JDK image.\n\
                     An acceptable JDK root must contain either:\n  \
                       * jmods/java.base.jmod   (a full JDK 9+ installation), or\n  \
                       * lib/modules            (a JRE or jlink-trimmed runtime image)\n\
                     Fix by one of:\n  \
                       * pass --java-home <PATH> pointing at a JDK 9+ root;\n  \
                       * set JAVA_HOME (or CRATONVM_JAVA_HOME, which wins over it);\n  \
                       * or drop --real-jdk to run with the default compatibility \
                     behaviour.\n\
                     --synthetic-jdk is not an escape here: it conflicts with \
                     --real-jdk and is rejected before this point."
                );
            }
            anyhow::bail!(
                "--java-home path does not exist or is not a directory: {jh}\n\
                 Provide a valid JDK installation (must contain `jmods/` or `lib/modules`)."
            );
        }
        config = config.with_java_home(jh.clone());
    }

    // Container/cgroup awareness (HotSpot's `-XX:+UseContainerSupport`, on by
    // default). When enabled, read the cgroup memory/CPU limits once so the
    // ergonomic default heap is sized off the container limit and
    // `Runtime.availableProcessors()` honors the CPU quota. `-XX:-UseContainer
    // Support` skips detection entirely, so every limit reverts to host values.
    // On non-Linux hosts `detect_container()` reports "not containerized" with
    // all limits `None`, so this is a no-op there.
    let container_info = if args.disable_container_support {
        None
    } else {
        Some(cratonvm_vm::runtime::container::detect_container())    };
    let container_mem_limit = container_info.as_ref().and_then(|i| i.memory_limit);
    // CPU count for Runtime.availableProcessors() / the JMX OS bean. Only set
    // when a cgroup quota was actually detected; `None` ⇒ report host count.
    config.container_effective_processors =
        container_info.as_ref().and_then(|i| i.effective_cpu_count);

    // zt t18/tg8 (`CRATONVM_LAUNCHER_MEMORY_SIZE_OPTIONS`): HotSpot's size
    // syntax for `-Xms` / `-Xss`, and their zero is "not set" (the ergonomic
    // initial heap, the default stack size), as on HotSpot.
    if memory_sizes {
        let zero =
            |raw: &Option<String>| raw.as_deref().and_then(hotspot_parse_uintx_flag) == Some(0);
        if zero(&args.initial_heap) {
            args.initial_heap = None;
        }
        if zero(&args.thread_stack_size) {
            args.thread_stack_size = None;
        }
    }
    // `-Xss` (interpreter round i1 wave 38; parsed since gcd d6/f, which also
    // sizes the carrier below): the default thread stack size
    // raises every thread's frame limit (a thread's own `stackSize` can raise
    // its own further, `FrameStack::at_frame_limit`). Only raises, like a
    // thread's own request.
    if let Some(ss) = &args.thread_stack_size {
        match launcher_memory_size(ss, memory_sizes) {
            Some(bytes) => {
                let limit = cratonvm_vm::runtime::frame::frame_limit_for_stack_bytes(
                    i64::try_from(bytes).unwrap_or(i64::MAX),
                );
                config.max_stack_depth = config.max_stack_depth.max(limit);
            }
            None => eprintln!("Warning: ignoring -Xss{ss} (expected a size such as 512k or 4m)"),
        }
    }
    if let Some(max_heap_str) = &args.max_heap {
        let size = parse_max_heap_size(max_heap_str)
            .with_context(|| format!("Invalid heap size: {max_heap_str}"))?;
        // HotSpot 25 rejects a maximum below 2 MiB before VM bootstrap.
        // Letting it reach class/mirror initialization can exhaust the heap
        // before a Java exception frame or emergency throwable exists.
        validate_minimum_max_heap(size, &format!("-Xmx{max_heap_str}"))?;
        config = config.with_max_heap_size(size);
    } else if let Some(ergo) = ergonomic_default_max_heap(container_mem_limit) {
        // No explicit -Xmx: size the heap like a stock JDK (1/4 of host RAM, or
        // of the cgroup limit inside a container) instead of the fixed 256 MB
        // library default, so Spring/Mockito/JUnit workloads don't thrash GC
        // into a pseudo-hang. See `ergonomic_default_max_heap`.
        if args.verbose_gc {
            let basis = if container_mem_limit.is_some() {
                "1/4 container memory limit"
            } else {
                "1/4 physical RAM"
            };
            eprintln!(
                "[cratonvm] ergonomic default max heap: {} MB ({basis}; \
                 set -Xmx or CRATONVM_DEFAULT_HEAP_ERGONOMICS=0 to override)",
                ergo / (1024 * 1024)
            );
        }
        config = config.with_max_heap_size(ergo);
    }

    // F-16 — `-Xms`. Applied AFTER `-Xmx` so it can be clamped against the heap
    // size actually in force, ergonomic default included. Clamping rather than
    // rejecting: HotSpot treats `-Xms` above `-Xmx` as an error, but a VM that
    // refuses to start over a memory hint is a worse drop-in than one that
    // starts with the heap it was told it may have — and a Surefire fork with a
    // stale `-Xms` is exactly the case this has to survive.
    if let Some(initial) = &args.initial_heap {
        let size = launcher_memory_size(initial, memory_sizes)
            .with_context(|| format!("Invalid initial heap size: {initial}"))?;
        let clamped = size.min(config.max_heap_size);
        if clamped != size && args.verbose_gc {
            eprintln!(
                "[cratonvm] -Xms {} MB exceeds -Xmx {} MB; committing the whole heap",
                size / (1024 * 1024),
                config.max_heap_size / (1024 * 1024)
            );
        }
        config.initial_heap_size = clamped;
        // gcg g3/x: on HotSpot `-Xms` sets `MinHeapSize` too.
        config.min_heap_size = Some(clamped);
    } else if let Some(s) = &args.initial_heap_size_xx {
        // gcg g3/x: `-XX:InitialHeapSize` -- the initial size only (HotSpot
        // keeps the ergonomic minimum). Clamped like `-Xms`.
        match parse_size(s) {
            Some(size) => config.initial_heap_size = size.min(config.max_heap_size),
            // HotSpot's zero: the ergonomic initial size.
            None if hotspot_table && hotspot_parse_uintx(s) == Some(0) => {}
            None => eprintln!("Warning: ignoring -XX:InitialHeapSize={s} (expected a byte size)"),
        }
    }
    // gcg g3/x: `-XX:MinHeapSize` (overrides the minimum `-Xms` implied).
    if let Some(s) = &args.min_heap_size {
        match parse_size(s) {
            Some(size) => config.min_heap_size = Some(size.min(config.max_heap_size)),
            None => eprintln!("Warning: ignoring -XX:MinHeapSize={s} (expected a byte size)"),
        }
    }
    // gcg g3/x: `-XX:MinHeapFreeRatio` / `-XX:MaxHeapFreeRatio`, validated as
    // HotSpot 25 does -- an out-of-range or inconsistent pair refuses to
    // start with HotSpot's own lines and exit status 1
    // (`heap_free_ratios_from_flags`).
    // zt t12/tg5: in HotSpot's `uintx` syntax on an effective ZGC command
    // line (`heap_free_ratios_from_flags_in`).
    match heap_free_ratios_from_flags_in(
        args.min_heap_free_ratio.as_deref(),
        args.max_heap_free_ratio.as_deref(),
        zgc_free_ratio_hotspot_syntax,
    ) {
        Ok((min, max)) => {
            config.min_heap_free_ratio = min;
            config.max_heap_free_ratio = max;
        }
        Err(lines) => {
            for line in lines {
                eprintln!("{line}");
            }
            eprintln!("Error: Could not create the Java Virtual Machine.");
            eprintln!("Error: A fatal exception has occurred. Program will exit.");
            std::process::exit(1);
        }
    }

    // CDS configuration
    if let Some(archive_path) = &args.shared_archive_file {
        config.shared_archive_file = Some(archive_path.clone());
    }
    config.cds_mode = match args.xshare.as_str() {
        "on" => cratonvm_vm::config::CdsMode::On,
        "auto" => cratonvm_vm::config::CdsMode::Auto,
        "dump" => cratonvm_vm::config::CdsMode::Dump,
        "off" => cratonvm_vm::config::CdsMode::Off,
        other => {
            // Warn on a typo rather than silently defaulting to Off.
            eprintln!(
                "Warning: ignoring unknown -Xshare mode {other:?}; expected on|auto|dump|off"
            );
            cratonvm_vm::config::CdsMode::Off
        }
    };

    // ---------------------------------------------------------------
    // JDK mode: explicit selection, then validation, then publication.
    //
    // Previously this was `use_synthetic_jdk = detect_real_jdk().is_none()`
    // (in `with_host_jdk_default`) plus an ad-hoc "--java-home implies
    // real" rule here. That made the *standard library* — and therefore
    // the set of bugs a run could hit — a function of the host machine,
    // with nothing in the VM's output recording which one was used.
    //
    // Now: the mode comes from `--synthetic-jdk` / `--real-jdk` or the
    // fixed launcher default; an unavailable mode aborts the launch with
    // an actionable message; and the outcome is published for the
    // `-version` banner and the fatal-error path. `--java-home` no longer
    // *selects* real-JDK mode (it is already the default) — it only points
    // the (already selected) real-JDK mode at a specific installation.
    //
    // The compatibility *policy* (`--real-jdk`) is resolved FIRST, so that
    // `--real-jdk --synthetic-jdk` is diagnosed as the policy conflict it is
    // rather than as the generic "two standard libraries" conflict
    // `resolve_jdk_mode` would report for the same argv.
    // ---------------------------------------------------------------
    let compatibility_mode = resolve_compatibility_mode(
        args.real_jdk,
        args.fast_boot,
        args.synthetic_jdk,
    )?;
    note_no_stubs_env_without_jdk_only(compatibility_mode);
    let (jdk_mode, resolved_java_home) = match resolve_jdk_mode(
        args.synthetic_jdk,
        // Contract §9: `--real-jdk` implies `JdkMode::Real`, and so does
        // `--fast-boot` / `--compatible` — all name a policy
        // over real class bytes. They are rejected above in the combinations
        // that contradict each other, so anything reaching here agrees.
        args.real_jdk || args.fast_boot,
        config.java_home.as_deref(),
    ) {
        Ok(resolved) => resolved,
        // The resolved mode, not `args.real_jdk`: since the default flipped,
        // a run with no policy flag at all is strict, and it deserves the
        // same specific diagnosis as one that spelled `--real-jdk` out.
        Err(e) if compatibility_mode.is_jdk_only() => {
            return Err(e.context(
                "real-jdk mode requires a real JDK runtime image: real class bytes \
                 are authoritative under this policy, so there is nothing to fall \
                 back to. It is the launcher default. Point the launcher at an \
                 installation with --java-home <PATH>, or pass --fast-boot to run \
                 with the compatibility layer (which still needs \
                 a real JDK), or --synthetic-jdk for the standalone library.",
            ));
        }
        Err(e) => return Err(e),
    };
    config = config.with_jdk_mode(jdk_mode);
    config = config.with_compatibility_mode(compatibility_mode);
    // Backstop for the same conflict: `resolve_compatibility_mode` guards the
    // launcher's own flags, `validate_compatibility` guards the assembled
    // config (which an embedder or a future argv rewrite could reach another
    // way). Cheap, and it keeps the invariant with the type that owns it.
    config
        .validate_compatibility()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    // Pin the validated JDK root onto the config so boot-classpath
    // discovery inside the VM resolves the same installation this launcher
    // validated, instead of re-running the environment probe and possibly
    // landing somewhere else.
    if let Some(home) = resolved_java_home.as_ref() {
        if config.java_home.is_none() {
            config = config.with_java_home(home.to_string_lossy().into_owned());
        }
    }
    if compatibility_mode.is_jdk_only() {
        note_jdk_only_image_below_supported(detect_jdk_feature(config.java_home.as_deref()));
    }
    let _ = ACTIVE_JDK_MODE.set((
        jdk_mode,
        resolved_java_home.map(|p| p.to_string_lossy().into_owned()),
    ));
    tracing::info!("{}", active_jdk_mode_line());
    tracing::info!("compatibility mode: {}", compatibility_mode.as_str());
    // Remembered for `maybe_dump_shutdown_reports`, which runs on BOTH exit
    // arms and does not have `args`. See `GC_STATS_REQUESTED`.
    if args.verbose_gc {
        GC_STATS_REQUESTED.store(true, std::sync::atomic::Ordering::Release);
    }
    if args.verbose_class || args.verbose_gc {
        eprintln!("[cratonvm] {}", active_jdk_mode_line());
        eprintln!(
            "[cratonvm] compatibility mode: {}",
            compatibility_mode.as_str()
        );
    }
    // Only when `--real-jdk` was spelled out. It is the default now, so
    // printing this unconditionally would put a four-line preamble on every
    // `cratonvm Foo` — and a banner every run is a banner nobody reads. A
    // caller that asked explicitly is doing strict-mode work and wants the
    // caveat; `--verbose-class` / `--verbose-gc` above still report the mode
    // on any run.
    // nio21 wave 8 lane L4: and only under `CRATONVM_DBG=launcher-outcome`. The
    // orchestrators compare `--real-jdk` runs with HotSpot byte for byte, and
    // HotSpot has no such banner; `--real-jdk-report` carries the census.
    if args.real_jdk && launcher_outcome_lines_on() {
        // Wave 1 is measurement-first (contract §10): only class fabrication
        // and stub registration actually enforce. Say so, so a clean run is
        // not mistaken for a fully-enforced one.
        eprintln!(
            "[cratonvm] --real-jdk (the default): real class bytes are \
             authoritative. Wave-1 enforcement covers class fabrication and \
             synthetic-stub registration; remaining violations are recorded and \
             counted. Pass --real-jdk-report <FILE> for the census."
        );
    }

    // AOT configuration
    config.aot_mode = match args.aot_mode.as_str() {
        "training" => cratonvm_vm::config::AotMode::Training,
        "production" => cratonvm_vm::config::AotMode::Production,
        "off" => cratonvm_vm::config::AotMode::Off,
        other => {
            // Warn on a typo rather than silently defaulting to Off.
            eprintln!(
                "Warning: ignoring unknown -XX:AOTMode value {other:?}; \
                 expected off|training|production"
            );
            cratonvm_vm::config::AotMode::Off
        }
    };
    if let Some(cache_path) = &args.aot_cache {
        // AOTCache serves as input in production mode and output in training mode
        match config.aot_mode {
            cratonvm_vm::config::AotMode::Production => {
                config.aot_cache_input = Some(cache_path.clone());
            }
            cratonvm_vm::config::AotMode::Training => {
                if config.aot_cache_output.is_none() {
                    config.aot_cache_output = Some(cache_path.clone());
                }
            }
            _ => {
                config.aot_cache_input = Some(cache_path.clone());
            }
        }
    }
    if let Some(output_path) = &args.aot_cache_output {
        config.aot_cache_output = Some(output_path.clone());
    }

    // Garbage-collector selection (`-XX:+UseG1GC` / `-XX:-UseG1GC` / any other
    // `-XX:+Use*GC`, normalized to `--XX:UseGc <name>`). Absent → keep the
    // default, which is **ZGC** as of 2026-08-10 (see `VmConfig::default` for
    // the measurement). A recognized selector (`g1` | `z`/`zgc` |
    // `generational`) sets `gc_algorithm`; an unsupported collector
    // (Serial/Parallel/Shenandoah/Epsilon) warns and falls back to Generational
    // so a `java` drop-in keeps booting.
    //
    // The unsupported-collector fallback deliberately stays **Generational**
    // rather than following the default: someone who asked for
    // `-XX:+UseSerialGC` asked for a collector this VM does not have, and the
    // conservative copying collector is the safer thing to hand them than
    // whichever backend happens to be default that month.
    if let Some(sel) = &args.gc_selector {
        match cratonvm_vm::config::parse_gc_algorithm(sel) {
            Some(algo) => config.gc_algorithm = algo,
            None => {
                // gcj j1/ob: `-XX:+UseSerialGC` names the HotSpot collector
                // Generational is the twin of (`docs/GC.md`), and HotSpot
                // prints nothing for it; the warning stays for the collectors
                // this VM has no twin of (Parallel, Shenandoah, Epsilon).
                if !serial_selector_is_silent(sel, true) {
                    eprintln!(
                        "Warning: unsupported garbage collector -XX:+Use{sel}GC; CratonVM \
                         implements ZGC-real (the default, -XX:+UseZGC), G1 \
                         (-XX:+UseG1GC) and Generational (-XX:+UseGenerationalGC). \
                         Falling back to Generational."
                    );
                }
                config.gc_algorithm = cratonvm_vm::config::GcAlgorithm::Generational;
            }
        }
    }

    // gcg g4/yg: HotSpot's heap-size ergonomics that need the collector.
    // (1) `CRATONVM_GC_HOTSPOT_INITIAL_HEAP` (default ON since zr triage
    // 2026-10-12, `=0` restores the 16 MiB start): without `-Xms` /
    // `-XX:InitialHeapSize`, the initial heap is HotSpot's `physical / 64`
    // (`InitialRAMPercentage`), clamped to `[MinHeapSize, MaxHeapSize]`
    // (`gcg-g3x-proposal-hotspot-ergonomic-initial-heap-DONE-20261015`). The
    // embedding entry points keep their 16 MiB (AGENTS.md).
    //
    // WARNING gci-i48-initial-heap-all-collectors-20261017
    // gci zl1 (zx48 A5, user decision 2026-10-17): default ON on EVERY
    // collector, `=0` kills on every collector -- HotSpot 25's
    // `InitialRAMPercentage` is collector-independent (vi35-fz E3: Gen `=0`
    // finalized 0/64, HotSpot 64/64). This flips the Generational and G1
    // default path AGAINST a failed rule: the gci round's peak-RSS rule
    // failed there (Gen -Xmx2g 187.7 vs 141.7 MB, the G1 default 554.8 vs
    // 492.6 MB; `proposal-flip-triage-20261015.md` #3), which is why the gci
    // merge of 2026-10-15 had left it ON on ZGC only and opt-in (`=1`) on
    // Generational and G1.
    //
    // gci i35/fz (vi31-fz `fz31_f_ed_only`, 0/64 vs HotSpot 64/64):
    // `CRATONVM_GEN_SERIAL_EDEN_INITIAL` models HotSpot Serial's young
    // generation for HotSpot's INITIAL heap, and without `-Xms` that is this
    // ergonomic size (1 GiB at -Xmx2g on a 64 GiB host), never `VmConfig`'s
    // 16 MiB default. Its "follow implies the ergonomic initial heap unless
    // `CRATONVM_GC_HOTSPOT_INITIAL_HEAP` is explicitly off" rule is now the
    // default itself; an explicit `=0` still keeps the 16 MiB start (the
    // user's choice).
    let hotspot_initial_heap =
        true;
    if args.initial_heap.is_none() && args.initial_heap_size_xx.is_none() && hotspot_initial_heap {
        let physical = physical_ram_bytes();
        let basis = match (physical, container_mem_limit) {
            (Some(p), Some(c)) => Some(p.min(c)),
            (p, c) => p.or(c),
        };
        if let Some(basis) = basis {
            config.initial_heap_size =
                hotspot_ergonomic_initial_heap(basis, config.min_heap_size, config.max_heap_size);
        }
    }
    // (2) HotSpot aligns `MaxHeapSize`, `InitialHeapSize` and `MinHeapSize`
    // up to the collector's `HeapAlignment` (`hotspot_heap_alignment`).
    // Serial: 2 MiB (measured on 25.0.3: `-Xmx9m` is a 10M heap, `-Xms5000k`
    // a 6M initial heap). G1: `max(region, 2 MiB)` -- not the region alone,
    // as this comment said until gci zl1 -- with the region computed from
    // the unaligned `-Xmx` or `-XX:G1HeapRegionSize` (validated, and refused
    // when out of range, further down). ZGC: the 2 MiB granule. Every
    // collector since gci zl1 (zx48 A4, 2026-10-17; Generational only
    // before); `CRATONVM_GC_HEAP_ALIGN_HOTSPOT=0` keeps the typed sizes on
    // every collector (`gcg-g3x-xlog-gc-review-findings-FIXED-20261016`
    // item 10). zs s2/ob: on ZGC `CRATONVM_ZGC_HEAP_ALIGN_HOTSPOT=0` also
    // keeps the typed sizes (local 25.0.3 `-XX:+UseZGC`: `-Xmx9m` -> `Max
    // Capacity: 10M`, `-Xms5000k` -> `Min Capacity: 6M`; the soft max is not
    // aligned).
    let g1_region_flag = args
        .g1_region_size
        .as_deref()
        .and_then(|s| g1_heap_region_size_from_flag(s).ok().flatten());
    let zgc_align = !algorithm_is_zgc(config.gc_algorithm)
        || true;
    // zt t11/tg4 (`t11-x8-report.md` C13 / T2): kept unfiltered for the
    // `SoftMaxHeapSize` constraint below, which HotSpot checks against the
    // aligned maximum whatever the alignment switches do here.
    let hotspot_alignment =
        hotspot_heap_alignment(config.gc_algorithm, config.max_heap_size, g1_region_flag);
    let heap_alignment = hotspot_alignment.filter(|_| {
        zgc_align
    });
    if let Some(alignment) = heap_alignment {
        let (max, initial, min) = hotspot_aligned_heap_sizes(
            config.max_heap_size,
            config.initial_heap_size,
            config.min_heap_size,
            alignment,
        );
        config.max_heap_size = max;
        config.initial_heap_size = initial;
        config.min_heap_size = min;
    }

    // THERE IS NO `-Xms` WARNING HERE ANY MORE, and its absence is the fix
    // rather than an omission.
    //
    // Until 2026-09-20 `-Xms` reached the G1 backend and no other: the other
    // two accepted it and dropped it in silence, including ZGC, which is the
    // DEFAULT. A warning stood here from that date, because a dropped flag with
    // no diagnostic is the worse half of the defect and the launcher is the
    // only place that can raise one -- only it can tell an explicit `-Xms` from
    // `VmConfig::initial_heap_size`'s 16 MiB default, which reaches the heap
    // constructor on every run.
    //
    // As of 2026-09-21 all three backends commit: G1 through
    // `G1CollectorConfig::initial_heap_size`, ZGC through
    // `ZgcRealHeap::with_capacity_and_initial`, Generational through
    // `GenerationalHeap::commit_initial_heap`. `VmHeap::new_with_heap_sizing`
    // still makes every arm state what it did (`XmsDisposition`), and no arm
    // answers `Dropped` any longer, so there is nothing left to warn about.
    // See
    // `docs/feature-designs/real-jdk-mode.md`.

    // Young generation sizing — `-Xmn`, `-XX:NewSize`, `-XX:MaxNewSize`,
    // `-XX:NewRatio` (gen r4w2/alloc2, 2026-09-23). Applied AFTER `-Xmx` (the
    // plan divides the heap actually in force, ergonomic default included) and
    // AFTER the collector selection (only Generational honours them). Until
    // this date all four were swallowed by the `-X…` / `-XX:…` catch-alls with
    // no diagnostic, and the young pair was always `-Xmx / 2`.
    //
    // Every line below prints at most once per run: the launcher resolves the
    // request once, here. A clamp is a `Warning:` (the operator asked for a
    // geometry they are not getting); the resolved split is printed under
    // `--verbose:gc` only; a collector that ignores the flags says so in one
    // unconditional line, because a silent drop is the defect being fixed.
    // With none of the four given nothing here runs and the heap is built
    // exactly as before.
    let young_ergo = true;
    {
        let size_flag = |spelling: &str, raw: &Option<String>| -> Option<usize> {
            let s = raw.as_ref()?;
            // gc-known-issues retire (`CRATONVM_GEN_HOTSPOT_YOUNG_ERGO`): in
            // HotSpot's syntax (`t`, `0x` hex: `-Xmn1t` was `Warning: ignoring
            // -Xmn1t`), as the memory size options are read.
            let parsed = launcher_memory_size(s, young_ergo && hotspot_table).filter(|&v| v != 0);
            // HotSpot takes an explicit zero young size (its ergonomics
            // raise it to the minimum); it is not a request here.
            // gc-known-issues retire (`CRATONVM_GEN_HOTSPOT_YOUNG_ERGO`): but
            // a typed `-XX:NewSize=0` / `-XX:MaxNewSize=0` is a command-line
            // size to HotSpot's ergonomics (the plan ignores a zero), so it
            // is kept as `Some(0)`; `-Xmn0` is refused before this.
            if parsed.is_none() && hotspot_table && hotspot_parse_uintx(s) == Some(0) {
                return (young_ergo && spelling != "-Xmn").then_some(0);
            }
            if parsed.is_none() {
                eprintln!("Warning: ignoring {spelling}{s} (expected a byte size)");
            }
            parsed
        };
        let mut young = cratonvm_vm::YoungGenSizing {
            xmn: size_flag("-Xmn", &args.young_xmn),
            new_size: size_flag("-XX:NewSize=", &args.young_new_size),
            max_new_size: size_flag("-XX:MaxNewSize=", &args.young_max_new_size),
            new_ratio: None,
        };
        if let Some(s) = &args.young_new_ratio {
            match s.trim().parse::<usize>() {
                Ok(r) => young.new_ratio = Some(r),
                Err(_) => eprintln!(
                    "Warning: ignoring -XX:NewRatio={s} (expected a non-negative integer)"
                ),
            }
        }
        if !young.is_default() {
            if config.gc_algorithm == cratonvm_vm::config::GcAlgorithm::Generational {
                if let Some(plan) = cratonvm_vm::plan_young_gen(config.max_heap_size, &young) {
                    // gc-known-issues retire: HotSpot's own `[gc,ergo]` lines
                    // (`hotspot_young_ergo_warnings`, below) replace this
                    // collector's clamp notes, which stay `--verbose:gc` only.
                    if !young_ergo || args.verbose_gc {
                        for note in &plan.adjustments {
                            eprintln!("Warning: {note}");
                        }
                    }
                    if args.verbose_gc {
                        eprintln!("{}", plan.summary());                    }                }            } else if config.gc_algorithm == cratonvm_vm::config::GcAlgorithm::G1
                && true
            {
                // gci i43/gy: G1 honours the four (`G1Collector::set_young_gen_sizing`); HotSpot
                // prints nothing for them, and its `[warning][gc,ergo]` lines come from the VM.
            } else if !hotspot_table || args.verbose_gc {
                // HotSpot ZGC takes the four without a word; the note is
                // `--verbose:gc` only under the flag table.
                eprintln!(
                    "[cratonvm] note: {} honoured by the Generational collector only \
                     (-XX:+UseGenerationalGC); ignored under {:?}",
                    young.given_flag_names().join(", "),
                    config.gc_algorithm,
                );
            }
            config.young_gen_sizing = young;
        }
    }

    // gen r4w6/young6 (2026-09-24): the HotSpot tenuring flags —
    // `-XX:MaxTenuringThreshold`, `-XX:InitialTenuringThreshold`,
    // `-XX:TargetSurvivorRatio` and `-XX:+PrintTenuringDistribution`. Applied
    // AFTER the collector selection, like the young sizing above: only
    // Generational honours them (any of the three values engages its adaptive
    // tenuring; see `cratonvm_gc::gen_heap::TenuringConfig`). A collector
    // that ignores the flags says so in one line. With none given nothing here
    // runs and the heap tenures exactly as before.
    //
    // gci i23/tn (`gci-i22sz-g1-survivor-target-is-twice-hotspots-RETIRED-UNFIXED-20261018`
    // item 5): a malformed or out-of-range value refuses to start with
    // HotSpot 25's lines and exit status 1 (`tenuring_config_from_flags`;
    // it was a `Warning:` and ignored). G1 honours `MaxTenuringThreshold` and
    // `TargetSurvivorRatio` (`VmHeap::set_tenuring_config`), and HotSpot G1
    // ignores `InitialTenuringThreshold` silently, so under G1 the note names
    // `-XX:+PrintTenuringDistribution` alone (no G1 print); under ZGC it
    // names every flag given, as before.
    {
        let tenuring = match tenuring_config_from_flags(&hotspot_flags) {
            Ok(tenuring) => tenuring,
            Err(lines) => {
                for line in lines {
                    eprintln!("{line}");
                }
                eprintln!("Error: Could not create the Java Virtual Machine.");
                eprintln!("Error: A fatal exception has occurred. Program will exit.");
                std::process::exit(1);
            }
        };
        if !tenuring.is_default() {
            let ignored: Vec<&str> = match config.gc_algorithm {
                cratonvm_vm::config::GcAlgorithm::Generational => Vec::new(),
                cratonvm_vm::config::GcAlgorithm::G1 => tenuring
                    .given_flag_names()
                    .into_iter()
                    .filter(|name| *name == "-XX:+PrintTenuringDistribution")
                    .collect(),
                _ => tenuring.given_flag_names(),
            };
            // HotSpot takes the flags on every collector without a word; the
            // note is `--verbose:gc` only under the flag table.
            if !ignored.is_empty() && (!hotspot_table || args.verbose_gc) {
                eprintln!(
                    "[cratonvm] note: {} honoured by the Generational collector only \
                     (-XX:+UseGenerationalGC); ignored under {:?}",
                    ignored.join(", "),
                    config.gc_algorithm,
                );
            }
            config.tenuring = tenuring;
        }
    }

    // `--nojit` used to force Generational here. That override is GONE as of
    // 2026-08-10, and its removal is part of the default flip rather than an
    // aside: with ZGC as the default, leaving it in place would have made
    // `--nojit` silently change the COLLECTOR as well as the compiler. Every
    // A/B that uses `--nojit` to isolate a JIT effect would then have been
    // varying two things at once — and it would have put interpreter-only runs
    // on the arm carrying the 63-class HANG column this flip exists to escape.
    // Its stated reason was a G1 reference-slot concern, which never applied to
    // the generational-vs-ZGC choice; `-XX:+UseGenerationalGC` expresses it
    // explicitly for callers who still want it.

    // G1 tuning knobs (§7 item 4). Parsed from the normalized `--XX:*` value
    // args and stored on the config; applied to `G1CollectorConfig` only when
    // the G1 backend is selected (vm_init via `G1ConfigOverrides`). A malformed
    // value warns and is ignored (keeps the collector default), matching the
    // lenient-with-warning policy used for the GC selector.
    // zt t11/tg4 (`CRATONVM_LAUNCHER_ZGC_GC_VALUE_FLAG_REFUSALS`): on ZGC the
    // ordered scan refused every value HotSpot refuses, so a value that got
    // here is one HotSpot 25 takes silently (`=0`, `=0x10`, `=3k`): no
    // warning. The knobs act on G1 only (`MaxGCPauseMillis` also on ZGC under
    // `CRATONVM_ZGC_IGNORE_MAX_GC_PAUSE_MILLIS=0`, read in HotSpot's syntax).
    let zgc_quiet_g1_values = algorithm_is_zgc(config.gc_algorithm)
        && launcher_zgc_flag_hotspot_parse_enabled()
        && launcher_zgc_gc_value_flag_refusals_enabled();
    if let Some(s) = &args.g1_ihop {
        let ihop = "InitiatingHeapOccupancyPercent";
        let quiet = zgc_quiet_g1_values && hotspot_takes_gc_value_flag(ihop, s);
        // HotSpot's `range(0, 100)`: a zero starts marking at once
        // (`CRATONVM_LAUNCHER_HOTSPOT_FLAG_TABLE`).
        let lo = if hotspot_table { 0 } else { 1 };
        match s.parse::<u8>() {
            Ok(p) if (lo..=100).contains(&p) => config.g1_ihop_percent = Some(p),
            _ if quiet => {}
            _ => eprintln!(
                "Warning: ignoring -XX:InitiatingHeapOccupancyPercent={s} (expected 1..=100)"
            ),
        }
    }
    if let Some(on) = args.zgc_uncommit {
        config.zgc_uncommit = on;
    }
    if let Some(raw) = args.zgc_uncommit_delay.as_deref() {
        if let Some(seconds) = hotspot_parse_uintx(raw) {
            config.zgc_uncommit_delay_secs = seconds;
        } else {
            eprintln!("Warning: ignoring -XX:ZUncommitDelay={raw} (expected a non-negative integer)");
        }
    }
    if let Some(s) = &args.zgc_soft_max_heap_size {
        // zt t7/x4b (`CRATONVM_ZGC_SOFT_MAX_ZERO_HOTSPOT`, default ON, `=0`
        // kill): on ZGC a 0 is kept, as HotSpot 25.0.3 keeps it (`Soft Max
        // Capacity: 0M`, no warning); the heap reads the same key.
        let zero_kept =
            algorithm_is_zgc(config.gc_algorithm) && zgc_soft_max_zero_hotspot_enabled();
        // zt t9/tg2 (`launcher_zgc_flags_take_hotspot_syntax`): the value
        // takes HotSpot's `size_t` syntax (`0x100000`, `1T`). It is also what
        // lets the zero above be kept: `parse_size` refuses 0, so
        // `-XX:SoftMaxHeapSize=0` still printed the warning and was dropped.
        let hotspot_syntax = launcher_zgc_flags_take_hotspot_syntax(config.gc_algorithm);
        let hotspot_size = if hotspot_syntax {
            hotspot_parse_uintx_flag(s).and_then(|n| usize::try_from(n).ok())
        } else {
            None
        };
        match hotspot_size.or_else(|| parse_size(s)) {
            Some(sz) if sz > 0 || zero_kept => config.zgc_soft_max_heap_size = Some(sz),
            // HotSpot's zero off ZGC: no soft limit below the maximum
            // (`CRATONVM_LAUNCHER_HOTSPOT_FLAG_TABLE`).
            _ if hotspot_table && hotspot_parse_uintx(s) == Some(0) => {}
            _ => eprintln!("Warning: ignoring -XX:SoftMaxHeapSize={s} (expected a byte size)"),
        }
    }
    // zt t28/sm (`CRATONVM_ZGC_SOFT_MAX_ERGONOMIC`, default ON, `=0` kill;
    // row `lg1_gi_def`): HotSpot 25's ZGC sets `SoftMaxHeapSize` to 90 % of
    // `MaxHeapSize` when the command line sizes neither the maximum heap
    // (`-Xmx`, `-XX:MaxHeapSize`, `-XX:MaxRAMPercentage`, `-XX:MaxRAMFraction`)
    // nor the soft maximum (`ZArguments::initialize_heap_flags_and_sizes`,
    // `FLAG_SET_ERGO_IF_DEFAULT`): local 25.0.4 `-XX:+UseZGC` reads
    // `MaxHeapSize` 2082471936 and `SoftMaxHeapSize` 1874224742 `{ergonomic}`,
    // `Soft Max Capacity: 1787M`; with `-Xmx1g` or `-XX:MaxRAMPercentage=50`
    // the two are equal. Serial and G1 keep `MaxHeapSize`.
    if algorithm_is_zgc(config.gc_algorithm)
        && config.zgc_soft_max_heap_size.is_none()
        && args.max_heap.is_none()
        && zgc_soft_max_ergonomic_applies(&jvm_args)
    {
        config.zgc_ergonomic_soft_max_heap_size = Some(hotspot_ergonomic_soft_max(config.max_heap_size));
    }
    // zt t7/x4b: a ZGC boolean without its `+`/`-` (`-XX:ZProactive=false`)
    // ends the launch on ZGC, as HotSpot's `Missing +/- setting` does (the
    // first one in command-line order); `=0` and off ZGC ignore it.
    if let Some(option) = args.zgc_boolean_without_sign.first() {
        if algorithm_is_zgc(config.gc_algorithm) {
            exit_with_missing_plus_minus(option);
        }
    }
    // zt t6/tc (zt1tp item 4): ZGC's director knobs, per VM.
    if let Some(on) = args.zgc_proactive {
        config.zgc_proactive = on;
    }
    if let Some(s) = &args.zgc_allocation_spike_tolerance {
        // zt t7/x4 (`CRATONVM_ZGC_SPIKE_TOLERANCE_HOTSPOT_PARSE`, default ON,
        // `=0` kill): on ZGC the value takes HotSpot 25's double syntax
        // (`hotspot_parse_double_flag`): any finite number is kept, zero and
        // negative included (the flag has no range), and anything else ends
        // the launch with `Improperly specified VM option`, exit 1. Off ZGC,
        // and under `=0`, t6's rule: a positive number, else a warning.
        // zt t9/tg2: off ZGC too under `launcher_zgc_flags_take_hotspot_syntax`
        // (a value HotSpot takes on G1 is taken without the warning).
        let double_syntax = algorithm_is_zgc(config.gc_algorithm)
            || launcher_zgc_flags_take_hotspot_syntax(config.gc_algorithm);
        if double_syntax {
            let Some(t) = hotspot_parse_double_flag(s) else {
                let option = format!("ZAllocationSpikeTolerance={s}");
                exit_with_improperly_specified_vm_option(&option);
            };
            config.zgc_allocation_spike_tolerance = Some(t);
        } else if hotspot_table && !algorithm_is_zgc(config.gc_algorithm) {
            // A ZGC knob the effective collector does not read: HotSpot takes
            // any value its parse took, silently.
        } else {
            match s.trim().parse::<f64>() {
                Ok(t) if t.is_finite() && t > 0.0 => {
                    config.zgc_allocation_spike_tolerance = Some(t)
                }
                _ => eprintln!(
                    "Warning: ignoring -XX:ZAllocationSpikeTolerance={s} (expected a positive \
                     number)"
                ),
            }
        }
    }
    // zt t7/tg (zt1tp item 3): HotSpot's `double` flag; `<= 0` is its
    // default (off), so only a value that does not parse is refused.
    // zt t7 orchestrator: on ZGC it takes HotSpot 25's double syntax, as
    // `-XX:ZAllocationSpikeTolerance` does (same switch,
    // `CRATONVM_ZGC_SPIKE_TOLERANCE_HOTSPOT_PARSE`): `abc` or ` 1` end the
    // launch with `Improperly specified VM option`, exit 1; `0x1p1` is 2.0.
    // zt t9/tg2 (zt7tg item 2, `CRATONVM_LAUNCHER_ZGC_FLAG_HOTSPOT_PARSE`): on
    // ZGC an explicit `-XX:ZCollectionIntervalMajor` is the interval and the
    // alias is not read, as HotSpot 25.0.3 copies the alias only into a
    // still-default Major (`-XX:ZCollectionIntervalMajor=0
    // -XX:ZCollectionInterval=3` keeps Major at 0: the rule is off).
    let major_read =
        algorithm_is_zgc(config.gc_algorithm) && launcher_zgc_flag_hotspot_parse_enabled();
    let interval = match &args.zgc_collection_interval_major {
        Some(s) if major_read => Some(("ZCollectionIntervalMajor", s)),
        _ => args
            .zgc_collection_interval
            .as_ref()
            .map(|s| ("ZCollectionInterval", s)),
    };
    if let Some((name, s)) = interval {
        // zt t9/tg2: off ZGC too, as `-XX:ZAllocationSpikeTolerance` above.
        let double_syntax = algorithm_is_zgc(config.gc_algorithm)
            || launcher_zgc_flags_take_hotspot_syntax(config.gc_algorithm);
        if double_syntax {
            let Some(t) = hotspot_parse_double_flag(s) else {
                let option = format!("{name}={s}");
                exit_with_improperly_specified_vm_option(&option);
            };
            config.zgc_collection_interval_secs = (t > 0.0).then_some(t);
        } else {
            match s.trim().parse::<f64>() {
                Ok(t) if t.is_finite() => {
                    config.zgc_collection_interval_secs = (t > 0.0).then_some(t);
                }
                _ => eprintln!("Warning: ignoring -XX:{name}={s} (expected a number)"),
            }
        }
    }
    // zt t27/gn5 (gn page item 14): `-XX:ZCollectionIntervalMinor`, read as
    // the major interval above is (HotSpot's `double`; `<= 0`, its default
    // -1, is off). Only a ZGC heap in its generational mode acts on it
    // (`ZgcRealHeap::set_collection_interval_minor`).
    if let Some(s) = &args.zgc_collection_interval_minor {
        let double_syntax = algorithm_is_zgc(config.gc_algorithm)
            || launcher_zgc_flags_take_hotspot_syntax(config.gc_algorithm);
        if double_syntax {
            let Some(t) = hotspot_parse_double_flag(s) else {
                let option = format!("ZCollectionIntervalMinor={s}");
                exit_with_improperly_specified_vm_option(&option);
            };
            config.zgc_collection_interval_minor_secs = (t > 0.0).then_some(t);
        } else if let Ok(t) = s.trim().parse::<f64>() {
            // Off the HotSpot syntax switches a value that does not parse is
            // dropped, as the flag was before.
            if t.is_finite() {
                config.zgc_collection_interval_minor_secs = (t > 0.0).then_some(t);
            }
        }
    }
    // zt t10/tg3 (`zgc-zt9tg2-*` item 2): read only by a ZGC heap, and not
    // under `CRATONVM_ZGC_COLLECTION_INTERVAL_ONLY=0`.
    if let Some(on) = args.zgc_collection_interval_only {
        config.zgc_collection_interval_only = on;
    }
    if let Some(s) = &args.g1_region_size {
        // gci i11/gm finding 13: HotSpot 25's `range(0, 512M)` -- a larger value
        // refuses to start with HotSpot's lines and exit status 1, as the heap
        // free ratios do (`heap_free_ratios_from_flags`).
        // zt t11/tg4: on ZGC a value HotSpot takes is taken silently.
        let quiet = zgc_quiet_g1_values && hotspot_takes_gc_value_flag("G1HeapRegionSize", s);
        match g1_heap_region_size_from_flag(s) {
            Ok(Some(sz)) => config.g1_region_size = Some(sz),
            // 0 is HotSpot's "decide ergonomically".
            Ok(None) if parse_size(s) == Some(0) => {}
            Ok(None) if hotspot_table && hotspot_parse_uintx(s) == Some(0) => {}
            Ok(None) if quiet => {}
            Ok(None) => {
                eprintln!("Warning: ignoring -XX:G1HeapRegionSize={s} (expected a byte size)")
            }
            Err(lines) => {
                for line in lines {
                    eprintln!("{line}");
                }
                eprintln!("Error: Could not create the Java Virtual Machine.");
                eprintln!("Error: A fatal exception has occurred. Program will exit.");
                std::process::exit(1);
            }
        }
    }
    if let Some(s) = &args.g1_max_pause {
        // zt t11/tg4: on ZGC read in HotSpot's `uintx` syntax (`=3k`, `=0x10`).
        let parsed = if zgc_quiet_g1_values {
            hotspot_parse_uintx_flag(s)
        } else {
            s.parse::<u64>().ok()
        };
        let quiet = zgc_quiet_g1_values && hotspot_takes_gc_value_flag("MaxGCPauseMillis", s);
        match parsed {
            Some(ms) if ms > 0 => config.g1_max_gc_pause_ms = Some(ms),
            _ if quiet => {}
            _ => eprintln!(
                "Warning: ignoring -XX:MaxGCPauseMillis={s} (expected a positive integer)"
            ),
        }
    }
    if let Some(s) = &args.g1_mixed_live_threshold {
        // HotSpot's `range(0, 100)`.
        let lo = if hotspot_table { 0 } else { 1 };
        match s.parse::<u8>() {
            Ok(p) if (lo..=100).contains(&p) => config.g1_mixed_gc_live_threshold_percent = Some(p),
            _ => eprintln!(
                "Warning: ignoring -XX:G1MixedGCLiveThresholdPercent={s} (expected 1..=100)"
            ),
        }
    }
    if let Some(s) = &args.g1_heap_waste {
        match s.parse::<u8>() {
            Ok(p) if p <= 100 => config.g1_heap_waste_percent = Some(p),
            _ => eprintln!("Warning: ignoring -XX:G1HeapWastePercent={s} (expected 0..=100)"),
        }
    }
    if let Some(s) = &args.g1_string_dedup {
        config.g1_string_dedup = Some(s == "true");
    }
    // zs s3/ob2: on ZGC (explicit or default) a malformed value is HotSpot's
    // argument-parsing failure, which comes before the zero check below; the
    // two flags then take HotSpot's value syntax (`0x10`, `3k`).
    let zgc_effective = algorithm_is_zgc(config.gc_algorithm);
    // gcj j1/ob (item 2 of
    // `launcher-gc-thread-flag-and-xx-value-refusals-on-g1-and-generational-FIXED-20261022`):
    // and on Generational (`CRATONVM_GEN_HOTSPOT_VALUE_FLAGS`): a malformed
    // value was refused before the banner (`gen_hotspot_value_flag_refusal`),
    // so here the two flags only take HotSpot's syntax (`0x10`, `3k`). A zero
    // keeps its stderr warning (HotSpot Serial's answer has no local row).
    // G1 uses the same HotSpot uint syntax.
    let gen_effective = matches!(
        config.gc_algorithm,
        cratonvm_vm::config::GcAlgorithm::Generational
    );
    let hotspot_thread_syntax = matches!(config.gc_algorithm, cratonvm_vm::config::GcAlgorithm::G1)
        || (zgc_effective)
        || (gen_effective);
    if hotspot_thread_syntax {
        if let Some(option) = malformed_gc_thread_flag.as_deref() {
            exit_with_improperly_specified_vm_option(option);
        }
    }
    let parse_thread_count = |s: &str| -> Option<usize> {
        if hotspot_thread_syntax {
            hotspot_parse_uint_flag(s).and_then(|n| usize::try_from(n).ok())
        } else {
            s.parse::<usize>().ok()
        }
    };
    // zs s3/ob2: a default ZGC drops a zero `ConcGCThreads` without a word
    // (HotSpot's default G1 accepts it silently; it refuses a zero
    // `ParallelGCThreads`, which keeps the warning); an explicit ZGC refuses
    // both just below.
    // zs s4/rv3: only an IMPLICIT ZGC; an explicit one that reaches here (the
    // refusal's `=0` kill) keeps the warn-and-continue line it documents.
    let zgc_explicit = args
        .gc_selector
        .as_deref()
        .and_then(cratonvm_vm::config::parse_gc_algorithm)
        .is_some_and(algorithm_is_zgc);
    let silent_zero =
        zgc_effective && !zgc_explicit;
    // zs s2/ob: HotSpot 25 refuses an explicit `-XX:+UseZGC` with a zero
    // `-XX:ParallelGCThreads` / `-XX:ConcGCThreads` before the VM exists
    // (`ZArguments::initialize`); see `zgc_zero_gc_thread_refusal`.
    if let Some(detail) = zgc_zero_gc_thread_refusal(
        zgc_explicit,
        args.zgc_parallel_gc_threads.as_deref(),
        args.conc_gc_threads.as_deref(),
        true,
        hotspot_thread_syntax,
    ) {
        exit_with_vm_initialization_error(&detail);
    }
    // zs s9/ob6: with no collector named, HotSpot's default G1 refuses a zero
    // `ParallelGCThreads`; the implicit ZGC gives that answer.
    if let Some(detail) = zgc_implicit_zero_parallel_gc_threads_refusal(
        zgc_effective && !zgc_explicit,
        args.zgc_parallel_gc_threads.as_deref(),
        true,
        hotspot_thread_syntax,
    ) {
        exit_with_vm_initialization_error(&detail);
    }
    // zt t10/tg3 (`zgc-zt9tg2-timer-and-launcher-residuals-SUPERSEDED-20261019.md` item
    // 6, `CRATONVM_LAUNCHER_SOFT_MAX_ABOVE_MAX_REFUSED`): HotSpot 25 refuses
    // a `-XX:SoftMaxHeapSize` above the maximum heap on every collector
    // (`SoftMaxHeapSizeConstraintFunc`, after ergonomics; local 25.0.3 checks
    // it after the option parse and after ZGC's own initialization refusals
    // just above), where this launcher clamped it to `-Xmx` and booted. The
    // maximum here is final: explicit or ergonomic, and aligned.
    // zt t11/tg4 (`t11-x8-report.md` C13 / T2): aligned as HotSpot aligns
    // it (`hotspot_alignment`, idempotent on a maximum already aligned
    // above); with `CRATONVM_GC_HEAP_ALIGN_HOTSPOT=0` the maximum is the
    // typed one and alignment 1 refused `-Xmx9m -XX:SoftMaxHeapSize=10m`,
    // which HotSpot and the `-version` path take.
    if let Some(soft) = config.zgc_soft_max_heap_size {
        let alignment = hotspot_alignment.unwrap_or(1);
        if soft_max_heap_size_refused(soft as u64, config.max_heap_size, alignment)
        {
            exit_with_soft_max_above_max_heap();
        }
    }
    // Both launcher spellings configure the same per-VM evacuation count.
    let gen_ranges = gen_effective && hotspot_thread_syntax;
    if let Some(s) = args
        .zgc_parallel_gc_threads
        .as_ref()
        .or(args.g1_parallel_gc_threads.as_ref())
    {
        match parse_thread_count(s) {
            Some(n) if n > 0 => {
                config.g1_parallel_gc_threads = Some(n);
                config.zgc_parallel_gc_threads = Some(n);
            }
            Some(0) if matches!(config.gc_algorithm, cratonvm_vm::config::GcAlgorithm::G1) => {
                exit_with_vm_initialization_error(
                    "The flag -XX:+UseG1GC can not be combined with -XX:ParallelGCThreads=0",
                );
            }
            Some(0) if gen_ranges => {
                // Serial accepts a commanded zero and diagnostics retain its origin.
                config.g1_parallel_gc_threads = Some(0);
                config.zgc_parallel_gc_threads = Some(0);
            }
            _ => eprintln!(
                "Warning: ignoring -XX:ParallelGCThreads={s} (expected a positive integer)"
            ),
        }
    }
    if let Some(s) = &args.conc_gc_threads {
        match parse_thread_count(s) {
            Some(n) if n > 0 => config.zgc_conc_gc_threads = Some(n),
            Some(0) if config.gc_algorithm == cratonvm_vm::config::GcAlgorithm::G1 => {
                config.zgc_conc_gc_threads = Some(0);
            }
            Some(0) if silent_zero || gen_ranges => {}
            // HotSpot's G1 takes a zero (its ergonomic count).
            Some(0)
                if hotspot_table
                    && matches!(config.gc_algorithm, cratonvm_vm::config::GcAlgorithm::G1) => {}
            _ => eprintln!("Warning: ignoring -XX:ConcGCThreads={s} (expected a positive integer)"),
        }
    }
    if let Some(b) = args.use_dynamic_gc_threads {
        config.zgc_dynamic_gc_threads = Some(b);
    }
    // gcj j2/ob2 rows on j2a (`ob2_cap_mdm0`): HotSpot takes an explicit zero
    // (by reading `jdk.internal.misc.VM.saveProperties`, a zero cap).
    let direct_zero = args
        .max_direct_memory
        .as_deref()
        .is_some_and(|s| (gen_ranges || hotspot_table) && hotspot_parse_uintx(s) == Some(0));
    if direct_zero {
        config.max_direct_memory_size = Some(0);
    } else if let Some(s) = &args.max_direct_memory {
        match parse_size(s) {
            Some(sz) if sz > 0 => config.max_direct_memory_size = Some(sz),
            _ => eprintln!("Warning: ignoring -XX:MaxDirectMemorySize={s} (expected a byte size)"),
        }
    }
    // gcd d6/f: `-Xss`, the stack of a platform thread constructed without a
    // `stackSize` (`vm_exec::carrier_stack_bytes` decides the carrier).
    if let Some(s) = &args.thread_stack_size {
        match launcher_memory_size(s, memory_sizes) {
            Some(sz) => config.default_thread_stack_size = Some(sz),
            None => eprintln!("Warning: ignoring -Xss{s} (expected a byte size)"),
        }
    }
    // `-XX:MaxMetaspaceSize` (gc-common w6-f). Installed on this VM's class
    // manager right after `Vm::new` below — per VM, not a config global.
    // Charges accrue from the first define whether or not a limit is set
    // (`ClassManager::metaspace_charged_bytes`), so installing it after boot
    // only means nothing `Vm::new` itself defines can be refused.
    // gcj j2/ob2 rows on j2a (`ob2_cap_mms0`): HotSpot 25.0.3 answers
    // `-XX:MaxMetaspaceSize=0` with `Error occurred during initialization of
    // VM` / `OutOfMemoryError: Metaspace` on stdout.
    if args
        .max_metaspace
        .as_deref()
        .is_some_and(|s| hotspot_parse_uintx(s) == Some(0))
    {
        exit_with_vm_initialization_error("OutOfMemoryError: Metaspace");
    }
    let max_metaspace_bytes = max_metaspace_limit(args.max_metaspace.as_deref());
    if let Some(s) = &args.max_java_stack_trace_depth {
        // `0` is accepted: it means "unlimited", as on HotSpot.
        match s.trim().parse::<usize>() {
            Ok(n) => config.max_java_stack_trace_depth = n,
            _ => eprintln!(
                "Warning: ignoring -XX:MaxJavaStackTraceDepth={s} (expected a non-negative integer)"
            ),
        }
    }
    if let Some(s) = &args.soft_ref_lru_policy_ms_per_mb {
        // gen r5w2/conc6. `0` is accepted, as on HotSpot (an idle soft
        // reference may be cleared at the next collection).
        match parse_soft_ref_lru_policy(s) {
            Some(ms) => config.soft_ref_lru_policy_ms_per_mb = Some(ms),
            None => eprintln!(
                "Warning: ignoring -XX:SoftRefLRUPolicyMSPerMB={s} (expected a non-negative integer)"
            ),
        }
    }

    // gci i43/lf (`gci-i42ta-launcher-drops-survivor-ratio-and-tlab-flags-RETIRED-UNFIXED-20261018`):
    // `-XX:SurvivorRatio` and the TLAB flags. A malformed or out-of-range
    // value, or a value HotSpot's `AfterMemoryInit` constraints refuse,
    // refuses to start with HotSpot 25's lines and exit status 1; legal
    // values are carried per VM (`VmConfig::survivor_ratio`,
    // `VmConfig::tlab_flags`). Placed after the heap sizes and the G1 region
    // size are final, which the constraints read. They fell into the silent
    // `-XX:` catch-all until 2026-10-16. `CRATONVM_LAUNCHER_SPACE_TLAB_FLAGS=0`
    // accepts and drops them again (nothing validated, nothing carried).
    {
        // With no collector named HotSpot runs its default G1, whose geometry
        // the constraint lines print (`SurvivorRatio ... maximum (256)`,
        // `ergonomic TLAB maximum size (524288)` for a 256 MiB heap), under
        // `CRATONVM_LAUNCHER_HOTSPOT_FLAG_TABLE`; the implicit ZGC's own
        // geometry otherwise.
        let constraint_gc = if hotspot_table
            && args.gc_selector.is_none()
            && algorithm_is_zgc(config.gc_algorithm)
        {
            cratonvm_vm::config::GcAlgorithm::G1
        } else {
            config.gc_algorithm
        };
        let checked = space_tlab_flags_from_hotspot_flags(&hotspot_flags).and_then(|(sr, tlab)| {
            let geometry = hotspot_space_tlab_geometry(
                constraint_gc,
                config.max_heap_size,
                config.g1_region_size,
            );
            let mut lines = space_tlab_constraint_errors(sr, &tlab, geometry);
            if matches!(constraint_gc, cratonvm_vm::config::GcAlgorithm::G1) {
                if let Some((_, _, tlab_max)) = geometry {
                    lines.extend(g1_plab_constraint_errors(tlab.min_tlab_size, tlab_max));
                }
            }
            if lines.is_empty() {
                Ok((sr, tlab))
            } else {
                Err(lines)
            }
        });
        match checked {
            Ok((survivor_ratio, tlab)) => {
                config.survivor_ratio = survivor_ratio;
                config.tlab_flags = tlab;
            }
            Err(lines) => {
                for line in lines {
                    eprintln!("{line}");
                }
                eprintln!("Error: Could not create the Java Virtual Machine.");
                eprintln!("Error: A fatal exception has occurred. Program will exit.");
                std::process::exit(1);
            }
        }
    }

    // gc-known-issues retire (`launcher-hotspot-flag-table-residuals-FIXED-20261025`
    // item 2, `CRATONVM_GEN_HOTSPOT_YOUNG_ERGO`): HotSpot 25's young-size
    // `[warning][gc,ergo]` lines for the collector it would run, printed by
    // the VM with its stdout decorators; Serial's bootstrap failure ends the
    // launch here, after them, as HotSpot's initialization does.
    if young_ergo && !config.young_gen_sizing.is_default() {
        let hotspot_argv: Vec<String> = std::iter::once(String::new())
            .chain(jvm_args.iter().cloned())
            .collect();
        let (warnings, boot_failure) =
            hotspot_young_ergo_warnings(&config, &hotspot_argv, &jvm_args);
        if let Some(lines) = boot_failure {
            use std::io::Write as _;
            let mut out = std::io::stdout().lock();
            for line in cratonvm_vm::vm::g1_young_sizer_warning_lines(
                config.xlog_spec.as_deref(),
                launch_origin.elapsed().as_secs_f64(),
                &warnings,
            ) {
                let _ = writeln!(out, "{line}");
            }
            for line in lines {
                let _ = writeln!(out, "{line}");
            }
            let _ = out.flush();
            std::process::exit(1);
        }
        config.hotspot_gc_ergo_warnings = warnings;
    }

    // Missing native audit. NEW-10: `--dump-missing-natives FILE` and
    // T2.1.3: `--dump-missing-natives-grouped FILE` both implicitly
    // enable audit mode so the user doesn't have to pass the flag
    // separately.
    config.audit_missing_natives = args.audit_missing_natives
        || args.dump_missing_natives.is_some()
        || args.dump_missing_natives_grouped.is_some();

    // -XX:±ShowCodeDetailsInExceptionMessages — record on the VmConfig too, so
    // the resolved config reflects the flag (env_cache was already set from
    // `args` above for the interpreter gate). Absent → keep the default-on
    // `VmConfig` value; an explicit flag (`=true`/`=false`) overrides it.
    config.show_code_details_in_exception_messages = args
        .show_code_details_in_exception_messages
        .unwrap_or(config.show_code_details_in_exception_messages);

    // JDWP debug server.
    //
    // `jdwp_port` is only ever read behind `#[cfg(feature = "experimental-debug")]`
    // in `vm_init`, and that feature is not in `cratonvm-vm`'s default set nor
    // enabled by `cratonvm-cli` — so in every shipped launcher binary this is
    // accepted and does nothing. `jdwp_suspend` is not read anywhere at all, in
    // any configuration. Both used to be silent; a debugger that never attaches
    // is not a subtle failure to be left unexplained.
    if let Some(port) = args.jdwp_port {
        if !cratonvm_vm::config::JDWP_SERVER_COMPILED_IN {
            eprintln!(
                "Warning: --jdwp-port {port} has no effect in this build; the JDWP \
                 server is behind the 'experimental-debug' feature, which this \
                 binary was not built with."
            );
        }
        if args.jdwp_suspend {
            eprintln!(
                "Warning: --jdwp-suspend is not implemented; the VM will not wait \
                 for a debugger to attach."
            );
        }
        config = config.with_jdwp(port, args.jdwp_suspend);
    } else if args.jdwp_suspend {
        eprintln!("Warning: --jdwp-suspend has no effect without --jdwp-port.");
    }

    // zt t16/tg7 (`CRATONVM_LAUNCHER_PATCH_MODULE_REFUSALS`): the boot layer
    // decodes `--patch-module` in `ModuleBootstrap`'s static initializer,
    // before it scans the module path. The arm below keeps the `=0` kill's
    // texts.
    if launcher_patch_module_refusals_enabled() {
        if let Some(refusal) = patch_module_refusal(&args.patch_module) {
            exit_with_patch_module_refusal(&refusal);
        }
    }
    // JPMS module system flags
    if let Some(mp) = &args.module_path {
        config.module_path = VmConfig::parse_classpath(mp);
        // The boot layer scans the whole module path (service binding reads
        // every observable module), so a JAR it cannot derive a module from,
        // two modules of one name in one directory or a non-JAR file stops a
        // class-path launch too (interpreter round i5 wave 103, lane L5).
        // At the booted `JarFile.runtimeVersion()` (wave 105, lane L5).
        let feature = launch_jar_runtime_feature(
            (!config.use_synthetic_jdk)
                .then(|| detect_jdk_feature(config.java_home.as_deref()))
                .flatten(),
            last_system_property(&system_properties, "jdk.util.jar.version"),
            last_system_property(&system_properties, "jdk.util.jar.enableMultiRelease"),
        );
        if let Some(text) =
            cratonvm_classloading::module::module_path_find_error_at(&config.module_path, feature)
        {
            println!(
                "Error occurred during initialization of boot layer\n\
                 java.lang.module.FindException: {text}"
            );
            let _ = std::io::Write::flush(&mut std::io::stdout());
            std::process::exit(1);
        }
    }
    // zt t14/tg6 (`CRATONVM_LAUNCHER_BOOT_LAYER_OPTION_REFUSALS`): the boot
    // layer decodes these after it found the module path (`boot2`, then
    // `addExtraReads` / `addExtraExportsAndOpens`), and refuses a value it
    // cannot parse.
    {
        let options = BootLayerModuleOptions {
            add_reads: args.add_reads.clone(),
            add_exports: args.add_exports.clone(),
            add_opens: args.add_opens.clone(),
            ..BootLayerModuleOptions::default()
        };
        if let Some(exception) = boot_layer_module_option_refusal(&options) {
            exit_with_boot_layer_refusal(&exception);
        }
    }
    for s in &args.add_reads {
        config.add_reads.extend(VmConfig::parse_add_reads(s));
    }
    for s in &args.add_exports {
        if let Some(tuple) = VmConfig::parse_add_exports(s) {
            config.add_exports.push(tuple);
        } else {
            eprintln!(
                "Warning: invalid --add-exports format: {s} (expected module/package=target)"
            );
        }
    }
    for s in &args.add_opens {
        if let Some(tuple) = VmConfig::parse_add_exports(s) {
            config.add_opens.push(tuple);
        } else {
            eprintln!("Warning: invalid --add-opens format: {s} (expected module/package=target)");
        }
    }
    config.add_modules = args.add_modules.clone();
    if is_source_mode {
        config.add_modules.push("ALL-DEFAULT".to_string());
    }
    // Interpreter round i4 wave 87, lane L5: `--patch-module`,
    // `--limit-modules`, `--upgrade-module-path`.
    for spec in &args.patch_module {
        match spec.split_once('=') {
            Some((module, paths)) if !module.is_empty() && !paths.is_empty() => {
                let paths: Vec<String> = std::env::split_paths(paths)
                    .map(|p| p.to_string_lossy().into_owned())
                    .filter(|p| !p.is_empty())
                    .collect();
                match config.patch_modules.iter_mut().find(|(m, _)| m == module) {
                    // ModulePatcher: a second --patch-module for one module
                    // is an error on HotSpot.
                    Some(_) => {
                        println!(
                            "Error occurred during initialization of boot layer\n\
                             java.lang.IllegalArgumentException: --patch-module specified more \
                             than once for module {module}"
                        );
                        std::process::exit(1);
                    }
                    None => config.patch_modules.push((module.to_string(), paths)),
                }
            }
            _ => {
                println!(
                    "Error occurred during initialization of boot layer\n\
                     java.lang.IllegalArgumentException: Unable to parse --patch-module \
                     <module>=<value>: {spec}"
                );
                std::process::exit(1);
            }
        }
    }
    config.limit_modules = args
        .limit_modules
        .iter()
        .flat_map(|s| s.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if let Some(upgrade) = &args.upgrade_module_path {
        config.upgrade_module_path = VmConfig::parse_classpath(upgrade);
    }
    // A `--limit-modules` name that is neither in the image nor on the
    // (upgrade) module path: the boot layer's `FindException` (on stdout,
    // as `rewrite_module_launch`'s).
    if !config.limit_modules.is_empty() {
        let java_home = launcher_java_home(config.java_home.clone());
        let mut entries = config.module_path.clone();
        entries.extend(config.upgrade_module_path.iter().cloned());
        // At the booted `JarFile.runtimeVersion()` (wave 105, lane L5).
        let feature = launch_jar_runtime_feature(
            (!config.use_synthetic_jdk)
                .then(|| detect_jdk_feature(java_home.as_deref()))
                .flatten(),
            last_system_property(&system_properties, "jdk.util.jar.version"),
            last_system_property(&system_properties, "jdk.util.jar.enableMultiRelease"),
        );
        let observable = cratonvm_classloading::module::scan_module_path_at(&entries, feature);
        for name in &config.limit_modules {
            let on_path = observable.iter().any(|m| m.descriptor.name == *name);
            if !on_path && jdk_image_has_module(java_home.as_deref(), name) == Some(false) {
                println!(
                    "Error occurred during initialization of boot layer\n\
                     java.lang.module.FindException: Module {name} not found"
                );
                std::process::exit(1);
            }
        }
    }

    // System properties from -Dkey=value flags
    config.system_properties = system_properties;
    config.jvm_args = jvm_args;
    config.assertion_directives = assertion_directives;
    // zt t18/hunt2: `--finalization=` (JEP 421), this VM's bit.
    config.finalization_enabled = finalization_enabled;

    if is_source_mode {
        config
            .system_properties
            .push(("sun.java.launcher.mode".to_string(), "source".to_string()));
        if let Some(ver) = &args.source {
            config
                .system_properties
                .push(("jdk.internal.javac.source".to_string(), ver.clone()));
            config
                .jvm_args
                .push(format!("-Djdk.internal.javac.source={ver}"));
        }
        config.jvm_args.push("--add-modules=ALL-DEFAULT".to_string());
    }

    // `-Xbootclasspath/a:` rides HotSpot's own carrier, the private property
    // `jdk.boot.class.path.append` (entries joined in command-line order);
    // `SharedVm::new` appends it to the bootstrap search after the image.
    if xbootclasspath_append && !args.boot_classpath_append.is_empty() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        let appended = args.boot_classpath_append.join(sep);
        let existing = config
            .system_properties
            .iter()
            .rposition(|(k, _)| k == "jdk.boot.class.path.append");
        match existing {
            Some(i) if !config.system_properties[i].1.is_empty() => {
                let value = &mut config.system_properties[i].1;
                value.push_str(sep);
                value.push_str(&appended);
            }
            Some(i) => config.system_properties[i].1 = appended,
            None => config
                .system_properties
                .push(("jdk.boot.class.path.append".to_string(), appended)),
        }
    }

    // `sun.java.command` and `sun.java.launcher`, which HotSpot's launcher sets
    // and this one did not. Measured absent 2026-08-04 by diffing
    // `System.getProperties()` against HotSpot 25.
    //
    // Not cosmetic. `sun.java.command` is how a process identifies itself to
    // itself: Spring Boot's `ApplicationHome`, log4j/logback default file
    // naming, JMX `RuntimeMXBean`, and several agent/attach paths read it, and
    // a `null` there turns into a wrong log path or a silent feature-off rather
    // than an error. The launcher is the only layer that knows the value, which
    // is why the property table in `native-builtins` cannot supply it.
    //
    // Format follows the launcher: the main class (dotted, as typed) or the jar
    // path, then the program arguments, space-separated. An explicit `-D` wins,
    // matching `java -Dsun.java.command=…`.
    if !config
        .system_properties
        .iter()
        .any(|(k, _)| k == "sun.java.command")
    {
        let head = if is_source_mode {
            "jdk.compiler/com.sun.tools.javac.launcher.SourceLauncher".to_string()
        } else {
            args.jar
                .clone()
                .or_else(|| args.class_name.clone())
                .unwrap_or_default()
        };
        if !head.is_empty() {
            let command = if args.args.is_empty() {
                head
            } else {
                format!("{head} {}", args.args.join(" "))
            };
            config
                .system_properties
                .push(("sun.java.command".to_string(), command));
        }
    }
    if !config
        .system_properties
        .iter()
        .any(|(k, _)| k == "sun.java.launcher")
    {
        config
            .system_properties
            .push(("sun.java.launcher".to_string(), "SUN_STANDARD".to_string()));
    }

    // Container support (enabled by default, disabled with --XX:-UseContainerSupport)
    if args.disable_container_support {
        config = config.with_container_support(false);
    }

    // Unified logging (-Xlog)
    // gch h4/fl item 5 (applied wave 9, lane G; gci i1/ob, gch-h4fl-xlog-class-load-residuals):
    // HotSpot's `-verbose:class` is `-Xlog:class+load=info,class+unload=info` on stdout, added to
    // whatever `-Xlog` selected.
    // gci i27/ob (gcg-g3x item 11): HotSpot 25 refuses a bad `-Xlog`
    // selection while parsing its arguments (`LogSelection::parse` ->
    // `Arguments::parse_each_vm_init_arg` -> JNI_EINVAL): the
    // `[error][logging]` line on stdout, `Invalid -Xlog option ...` and the
    // launcher's two lines on stderr, exit 1. Here it warned and ran.
    // `CRATONVM_XLOG_REFUSE_INVALID_SELECTION=0` keeps the old tolerance.
    if cratonvm_vm::runtime::unified_logging::xlog_refuse_invalid_selection_enabled() {
        if let Some((out, err)) = cratonvm_vm::runtime::unified_logging::xlog_refusal_lines(
            &args.xlog,
            launch_origin.elapsed().as_secs_f64(),
        ) {
            for line in out {
                println!("{line}");
            }
            {
                use std::io::Write as _;
                let _ = std::io::stdout().flush();
            }
            for line in err {
                eprintln!("{line}");
            }
            std::process::exit(1);
        }
    }
    let mut xlog = args.xlog.clone();
    if args.verbose_class {
        xlog.push("class+load=info,class+unload=info:stdout".to_string());
    }
    if !xlog.is_empty() {
        config = config.with_xlog_spec(xlog.join(";"));
    }

    // T6.1.2 — `-XX:+HeapDumpOnOutOfMemoryError` / `-XX:HeapDumpPath=...`.
    // The interpreter's OOM path already honors these config fields (see
    // `maybe_dump_heap_on_oom` in `vm/src/runtime/interpreter.rs`); we
    // only need to thread the CLI values through.
    if let Some(flag) = hotspot_flags.heap_dump_on_oom {
        config.heap_dump_on_oom = flag;
    }
    if let Some(path) = hotspot_flags.heap_dump_path.clone() {
        config.heap_dump_path = Some(path);
    }
    // gc-common w8-g: `-XX:±DisableExplicitGC` / `-XX:±ExplicitGCInvokesConcurrent`.
    // Neither given leaves the default (`Full`), whatever the mode.
    config.explicit_gc = cratonvm_vm::config::ExplicitGcPolicy::from_hotspot_flags(
        hotspot_flags.disable_explicit_gc,
        hotspot_flags.explicit_gc_invokes_concurrent,
    );
    // gci i43/lf (`gci-i42mx-...` item 8): the flag's own value, which the
    // fold above loses when `-XX:+DisableExplicitGC` is also given (JFR's
    // `isExplicitGCConcurrent` reports the flag, as HotSpot's).
    config.explicit_gc_invokes_concurrent = hotspot_flags.explicit_gc_invokes_concurrent == Some(true);
    // gen r4w5/thrash5 — the other three `report_java_out_of_memory`
    // options; the VM acts on them at the same chokepoint as the dump
    // (`maybe_dump_heap_on_oom_for`). Absent, the config keeps its defaults.
    if let Some(flag) = hotspot_flags.exit_on_oom {
        config.exit_on_out_of_memory_error = flag;
    }
    if let Some(flag) = hotspot_flags.crash_on_oom {
        config.crash_on_out_of_memory_error = flag;
    }
    if let Some(cmds) = hotspot_flags.on_oom_error.clone() {
        config.on_out_of_memory_error = Some(cmds);
    }
    // gcf f2/o — `-XX:±UseGCOverheadLimit`; absent leaves `None` (the
    // collector's default, `gc_and_alloc::gc_overhead_limit_cycles`).
    if let Some(flag) = hotspot_flags.use_gc_overhead_limit {
        config.use_gc_overhead_limit = Some(flag);
    }
    // Interpreter round i2 wave 59, lane L1 — `-XX:±DisableAttachMechanism`.
    // zt t12/x9: `CRATONVM_PERF_DATA_FILE=0` (the config's default, read from
    // the environment) still wins over `-XX:+UsePerfData` /
    // `-XX:-PerfDisableSharedMem`: the flags only ever turn the file off
    // here, as the in-memory arm's `=0` rule already reads it
    // (`launcher_perf_data_file`).
    config.perf_data_file = launcher_perf_data_file(
        config.perf_data_file,
        hotspot_flags.use_perf_data,
        hotspot_flags.perf_disable_shared_mem,
    );
    if let Some(flag) = hotspot_flags.disable_attach_mechanism {
        config.disable_attach_mechanism = flag;
    }
    // Interpreter round i2 wave 60, lane L1: `-XX:+EnableDynamicAgentLoading`
    // and its `-` form.
    if hotspot_flags.enable_dynamic_agent_loading.is_some() {
        config.enable_dynamic_agent_loading = hotspot_flags.enable_dynamic_agent_loading;
    }
    // nio21 wave 16, lane L5: `-XX:±OmitStackTraceInFastThrow`; absent leaves
    // `None` (the per-VM default, ON as on HotSpot).
    if hotspot_flags.omit_stack_trace_in_fast_throw.is_some() {
        config.omit_stack_trace_in_fast_throw = hotspot_flags.omit_stack_trace_in_fast_throw;
    }
    // Interpreter round i4 wave 84, lane L2: `-XX:±ShowHiddenFrames`.
    if let Some(flag) = hotspot_flags.show_hidden_frames {
        config.show_hidden_frames = flag;
    }
    // Interpreter round i4 wave 85, lane L2: `-XX:±StackTraceInThrowable`.
    if let Some(flag) = hotspot_flags.stack_trace_in_throwable {
        config.stack_trace_in_throwable = flag;
    }
    // Interpreter round i5 wave 102, lane L6:
    // `-XX:DiagnoseSyncOnValueBasedClasses=<n>`. HotSpot also wants
    // `-XX:+UnlockDiagnosticVMOptions`; this launcher accepts it alone, as it
    // does `-XX:+ShowHiddenFrames`. A bad value refuses to start, as HotSpot.
    if let Some(raw) = hotspot_flags.diagnose_sync_on_value_based_classes.as_deref() {
        match diagnose_sync_on_value_based_classes_from_flag(raw) {
            Ok(mode) => config.diagnose_sync_on_value_based_classes = mode,
            Err(lines) => {
                for line in lines {
                    eprintln!("{line}");
                }
                eprintln!("Error: Could not create the Java Virtual Machine.");
                eprintln!("Error: A fatal exception has occurred. Program will exit.");
                std::process::exit(1);
            }
        }
    }
    // Interpreter round i3 wave 79, lane L3: `-Xrs` / `-XX:±ReduceSignalUsage`.
    if let Some(flag) = hotspot_flags.reduce_signal_usage {
        config.reduce_signal_usage = flag;
    }
    // zt t21/tg10: `-XX:±ClassUnloading` / `-Xnoclassgc`; absent keeps
    // HotSpot's default (`true`).
    if let Some(flag) = hotspot_flags.class_unloading {
        config.class_unloading = flag;
    }
    // zt t23/hunt4: `-XX:±AlwaysPreTouch`; absent keeps HotSpot's default
    // (`false`). The ZGC heap honours it (`CRATONVM_ZGC_ALWAYS_PRE_TOUCH`).
    if let Some(flag) = hotspot_flags.always_pre_touch {
        config.always_pre_touch = flag;
    }
    // gcj j1/st: `-XX:±PrintStringTableStatistics` and `-XX:StringTableSize`;
    // absent keeps HotSpot's defaults (off, 65536).
    if let Some(flag) = hotspot_flags.print_string_table_statistics {
        config.print_string_table_statistics = flag;
    }
    if let Some(size) = hotspot_flags.string_table_size {
        config.string_table_size = Some(size);
    }
    // zt t24/hunt7: `-XX:+UseTransparentHugePages` (Linux); absent keeps
    // HotSpot's default (`false`). The ZGC heap honours it
    // (`CRATONVM_ZGC_TRANSPARENT_HUGE_PAGES`).
    config.use_transparent_huge_pages = effective_use_transparent_huge_pages(
        cfg!(target_os = "linux"),
        hotspot_flags.use_transparent_huge_pages,
        hotspot_flags.use_large_pages,
    );
    // zt t25/hunt7b: `-XX:+UseLargePages` (the last `±` wins); the ZGC heap
    // reports HotSpot's no-pages warning for it (`CRATONVM_ZGC_TRANSPARENT_HUGE_PAGES`).
    config.use_large_pages = hotspot_flags.use_large_pages == Some(true);

    // Keep the complete option string for the image's DCmdStart parser.
    // Starting before Java boot would use the legacy, flattened JFR writer.
    if config.use_synthetic_jdk {
        // Synthetic-library embeddings keep their explicit legacy recorder;
        // no real jdk.jfr classes exist in that library to run DCmdStart.
        if let Some(raw) = &hotspot_flags.jfr_start_recording {
            config.jfr_start_recording = Some(
                parse_jfr_start_recording_opts_with(raw, gen_jfr_name_maxsize_options_enabled())
                    .map_err(|e| anyhow::anyhow!("{e}"))?,
            );
            config.jfr_enabled_events = parse_jfr_event_settings(raw)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
    } else {
        config.jfr_startup_options = hotspot_flags.jfr_start_recordings.clone();
    }

    // T6.3.3 — `-agentlib:`, `-agentpath:`, `-javaagent:`. The options
    // are stashed here and handed to the JVMTI `AgentRegistry` at VM
    // startup; registering them in a dedicated config field lets the
    // startup path load them in the canonical Agent_OnLoad order.
    //
    // WP2.4-C: split out the `-javaagent:` JAR specs into Java-agent
    // descriptors. The two sets are disjoint by syntax (`-javaagent:`
    // points at a JAR + `Premain-Class` manifest attribute, while
    // `-agentlib:` / `-agentpath:` point at native libraries with a
    // C `Agent_OnLoad` entry point) so we can route them independently.
    let mut java_agents: Vec<cratonvm_vm::runtime::agent_loader::LoadedAgent> = Vec::new();
    if !hotspot_flags.agent_options.is_empty() {
        let mut native_agent_opts: Vec<String> = Vec::new();
        for opt in &hotspot_flags.agent_options {
            if opt.starts_with("-javaagent:") {
                match cratonvm_vm::runtime::agent_loader::parse_javaagent_spec(opt) {
                    Ok(agent) => java_agents.push(agent),
                    Err(e) => exit_with_javaagent_jar_failure(&e),
                }
            } else if let Some(options) = jdwp_agent_options_of(opt)
                .filter(|_| cratonvm_vm::config::JDWP_SERVER_COMPILED_IN)
            {
                // Interpreter round i4 wave 85, lane L1: the JDK's JDWP back
                // end, mapped onto the built-in server (a build without it
                // leaves the option to the startup-agent path, which refuses
                // it with HotSpot's missing-library error).
                if config.jdwp_agent.is_some() {
                    exit_with_jdwp_agent_loaded_twice();
                }
                match parse_jdwp_agent_options(options) {
                    Ok(JdwpAgentParse::Config(agent)) => config.jdwp_agent = Some(agent),
                    Ok(JdwpAgentParse::Help) => {
                        println!("{JDWP_HELP}");
                        std::process::exit(0);
                    }
                    Err(failure) => exit_with_jdwp_agent_failure(&failure),
                }
            } else {
                native_agent_opts.push(opt.clone());
            }
        }
        config.jvmti_agent_options = native_agent_opts;
    }

    // Validate the class name before proceeding. A name no class can have
    // gets HotSpot's answer for it -- the system loader finds no such class,
    // `java.launcher.cls.error1` -- rather than a launcher syntax error
    // (interpreter round i2 wave 59, lane L5; probe `L5W59LauncherClassNames`).
    let validated = if version_boot.as_ref().is_some_and(VersionBoot::exits) {
        Ok(())
    } else {
        validate_class_name(&class_name)
    };
    if let Err(e) = validated {
        tracing::info!("main class name refused: {e}");
        let dotted = class_name.replace('/', ".");
        exit_with_launcher_error(&format!(
            "Error: Could not find or load main class {dotted}\n\
             Caused by: java.lang.ClassNotFoundException: {dotted}"
        ));
    }

    info!("Starting CratonVM");
    info!("Class: {class_name}");
    info!("Classpath: {:?}", config.classpath);

    // Resolve PrintFlagsFinal rows against the finalized launcher config,
    // but emit them only once the VM exists. HotSpot initializes the VM
    // before printing this table; this also puts it before a pending
    // `-showversion` banner below.
    let print_flags_final_lines = if gen_print_flags_final_enabled()
        && print_flags_final_requested(config.jvm_args.iter())
    {
        Some(gen_print_flags_final_lines(&config))
    } else {
        None
    };

    // Create VM and execute main method
    let image_census = image_census::Census::at_p0();
    // Phase 5 WP5: P0 of a boot image. `Restored` maps the image (and rebinds
    // the process state) instead of booting; `Capture` arms the arena so the
    // boot below can be written at P1; `Cold` is the unchanged path.
    let mut boot_image = boot_image_launch::at_p0(&boot_image_launch::P0Inputs {
        java_home: config.java_home.as_deref().map(std::path::Path::new),
        classpath: &config.classpath,
        argv: &std::env::args()
            .chain(std::env::var("JDK_JAVA_OPTIONS").ok().map(|v| format!("JDK_JAVA_OPTIONS={v}")))
            .collect::<Vec<_>>(),
        eligible: boot_image_eligibility(
            &args,
            &config,
            version_boot.is_some(),
            print_flags_final_lines.is_some(),
            !java_agents.is_empty(),
            max_metaspace_bytes.is_some(),
        ),
    });
    let mut restored_launcher_signals = None;
    let mut vm = if let boot_image_launch::AtP0::Restored(restored) = &mut boot_image {
        let (vm, signals) = restored.take_vm();
        restored_launcher_signals = Some(signals);
        vm
    } else {
        // A capture's VM must not reference a block mimalloc served before
        // P0: re-own the configuration's heap data in the armed arena.
        let config = if boot_image.is_capture() { config.clone() } else { config };
        match Vm::try_new(config) {
            Ok(vm) => vm,
            Err(error) => {
                print!("{}", error.report());
                let _ = std::io::Write::flush(&mut std::io::stdout());
                return Err(UncaughtReported("VM initialization heap exhaustion".into()).into());
            }
        }
    };
    let restored = boot_image.is_restored();
    if let Some(census) = &image_census {
        census.at_p1("stage-a: after Vm::try_new");
    }
    if let Some(lines) = print_flags_final_lines {
        use std::io::Write as _;
        let mut out = std::io::stdout().lock();
        for line in lines {
            let _ = writeln!(out, "{line}");
        }
        let _ = out.flush();
    }
    // zr z9/hp: the version boot's VM exists, so its startup lines have been
    // logged; print the banner (after them, as HotSpot does), then the
    // `gc,exit` summary HotSpot's `before_exit` prints, and exit 0.
    if max_metaspace_bytes.is_some() {
        vm.shared
            .classes
            .class_manager_write()
            .set_max_metaspace_bytes(max_metaspace_bytes);
    }

    if let Some(boot) = version_boot.take() {
        if boot.exits() && !vm.shared.config.jfr_startup_options.is_empty() {
            let phase1 = vm.boot_init_phase1();
            vm.boot_module_system(phase1);
            vm.shared.set_init_level(3);
            vm.shared.set_init_level(4);
            vm.boot_publish_jdk_booted().map_err(|err| java_bootstrap_error(&vm, err))?;
        }
        if boot.to_stdout {
            print!("{}", boot.banner);
            let _ = std::io::Write::flush(&mut std::io::stdout());
        } else {
            eprint!("{}", boot.banner);
            let _ = std::io::Write::flush(&mut std::io::stderr());
        }
        // zt t24/m5: a `-showversion` boot that names no program.
        if boot.usage_after {
            eprintln!("{}", hotspot_concise_usage_text());
            let _ = std::io::Write::flush(&mut std::io::stderr());
        }
        // dev cabdb9f4ae: a `-showversion` boot (`exit_code == None`) goes on
        // to the program.
        if let Some(exit_code) = boot.exit_code {
            if !vm.shared.config.jfr_startup_options.is_empty() {
                let mut ctx = cratonvm_vm::vm::NativeContextImpl { shared: &vm.shared, thread: &mut vm.main_thread };
                cratonvm_native_builtins::lang_system::run_shutdown_hooks(&mut ctx, "version-exit");
            }
            // zt t10/tg3 (the third shutdown arm X1 left out): HotSpot's
            // `before_exit` stops the ZGC director before this summary; a
            // `-XX:ZCollectionInterval=<tiny> -Xlog:gc -version` boot has a tick.
            stop_zgc_director_tick_and_wait(&mut vm);
            cratonvm_vm::runtime::unified_logging::log_gc_exit_summary(|| {
                cratonvm_vm::runtime::unified_logging::gc_exit_heap_lines(&vm.shared)
            });
            // gcj j1/ob: then (Generational) the `safepoint,stats` exit summary.
            cratonvm_vm::runtime::unified_logging::log_safepoint_stats_exit_summary(
                vm.shared.mem.heap.is_generational(),
            );
            // gcj j1/st item 2: then `-XX:+PrintStringTableStatistics`' block
            // (HotSpot prints it under `-version` too).
            cratonvm_vm::print_string_table_statistics_at_exit(&vm.shared);
            let _ = std::io::Write::flush(&mut std::io::stdout());
            // zt t21/tg10: 1 for the concise usage (`VersionBoot::exit_code`).
            std::process::exit(exit_code);
        }
    }

    // Stash a WEAK handle for `maybe_dump_shutdown_reports`, which runs on the
    // `System.exit` arm where this scope's `vm` is not in reach. See
    // `VM_FOR_SHUTDOWN` for why weak and not strong.
    if let Some(arc) = vm.shared.try_get_arc() {
        let _ = VM_FOR_SHUTDOWN.set(std::sync::Arc::downgrade(&arc));
    }

    // Real-JDK: drain the violation logs now, not only at shutdown. Native
    // registration refusals all happen inside `Vm::new`, so this is their real
    // time of occurrence — reporting them at shutdown would print them after
    // whatever failure they caused. See `ViolationWatermark` for what this
    // drain still covers now that class-origin violations arrive live.
    let mut jdk_only_watermark = ViolationWatermark::default();
    // `--explain-jdk-only` is admitted here as well as `--trace-jdk-only`.
    // `finish_jdk_only` already accepts either (see its gate), so gating the
    // vm-init drain and the live sink on `trace_jdk_only` ALONE meant
    // `--real-jdk --explain-jdk-only` on its own got only a shutdown batch:
    // every mid-run fabrication reported detached from the code that caused it,
    // which is precisely what `ViolationWatermark`'s own doc says the live sink
    // exists to prevent. Measured 2026-08-12: none of the nine runtime
    // fabrication families appears in `--explain-jdk-only` output, while all
    // thirteen boot-time refusals do.
    if args.trace_jdk_only || args.explain_jdk_only {
        trace_jdk_only_violations(
            &vm.shared,
            &mut jdk_only_watermark,
            "vm-init",
            args.explain_jdk_only,
        );
        // Contract §9 asks `--trace-jdk-only` to *"log every violation as it
        // happens"*. Class-origin violations occur throughout the run, so a
        // shutdown-batched drain reports them detached from the code that
        // caused them. Install a live sink now — after the drain above, so
        // the violations recorded inside `Vm::new` are reported exactly once,
        // by that drain, and everything from here on is reported exactly once,
        // by the sink. `trace_jdk_only_violations` sees the sink and advances
        // its origin watermark without re-printing.
        //
        // The sink is a field on this VM's `ClassManager`, not a process
        // global (contract §2): two VMs in one process each see only their
        // own violations.
        let jdk_feature = detect_jdk_feature(vm.shared.config.java_home.as_deref());
        let explain = args.explain_jdk_only;
        vm.shared
            .classes
            .class_manager_write()
            .set_violation_sink(std::sync::Arc::new(move |violation| {
                eprintln!(
                    "[cratonvm][jdk-only:live] {}",
                    render_violation(violation, jdk_feature, explain)
                );
            }));
    }

    // BUG-03 — publish the main thread's TLAB address now that `vm` is at its
    // final, address-stable location on the `main-vm` thread. This lets the
    // cross-thread STW JIT root scan recover the main thread's un-retired
    // reserved TLAB tail if it is forcibly stopped while executing JIT code
    // (workers / foreign threads publish theirs at their own start). Casting to
    // a raw pointer ends the borrow immediately, so the subsequent registry
    // call does not conflict.
    {
        let main_tlab = &vm.main_thread.tlab as *const _ as usize;
        let main_tid = vm.main_thread.thread_id;
        vm.shared
            .threads
            .thread_registry
            .set_tlab_addr(main_tid, main_tlab);
        // xt-hardening (2026-07-03): publish main's OS thread id for the
        // takeover's counted-set excusal (workers publish at their start).
        vm.shared
            .threads
            .thread_registry
            .set_os_tid_current(main_tid);
    }

    // T19.H1: optional watchdog that dumps interpreter frames and aborts when
    // the user explicitly bounds execution. Normal Java programs may be
    // long-running services, so no watchdog is armed by default. Set
    // `--stack-dump-on-timeout=N` or `CRATONVM_DEFAULT_WATCHDOG_SEC=N` to opt
    // in; `--stack-dump-on-timeout=0` and `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1`
    // disable the env-default path.
    //
    // Native-call and dispatch rings are recorded only for explicit diagnostic
    // requests because they add hot-path work on every native-method entry.
    let explicit_watchdog = matches!(args.stack_dump_on_timeout, Some(s) if s > 0);
    let ring_recording_requested = explicit_watchdog
        || Err::<String, std::env::VarError>(std::env::VarError::NotPresent).ok().as_deref() == Some("1");
    let default_watchdog_env = if std::env::var("CRATONVM_DISABLE_DEFAULT_WATCHDOG")
        .ok()
        .as_deref()
        == Some("1")
    {
        None
    } else {
        std::env::var("CRATONVM_DEFAULT_WATCHDOG_SEC").ok()    };
    let effective_watchdog =
        resolve_watchdog_timeout(args.stack_dump_on_timeout, default_watchdog_env.as_deref());
    // Shared "run() completed" flag for the stack-dump watchdog. When `run()`
    // returns (normally OR via `?`/early-return), the RAII guard below sets
    // this to `true`; the watchdog checks it after its deadline sleep and
    // again immediately before `abort()`, exiting cleanly without aborting a
    // run that finished just after a tight deadline. Only meaningful when a
    // watchdog is actually armed, but harmless otherwise.
    let watchdog_completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // RAII guard: its Drop runs on every exit path of `run()` (normal return,
    // early `return`, and `?` propagation), so the completed flag is always
    // set once we leave this function. Instantiated right after the watchdog
    // is spawned (see below).
    struct WatchdogCompletionGuard {
        flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }
    impl Drop for WatchdogCompletionGuard {
        fn drop(&mut self) {
            self.flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    // Holds the guard alive until `run()` returns. `None` when no watchdog is
    // armed (nothing to cancel).
    let mut _watchdog_completion_guard: Option<WatchdogCompletionGuard> = None;

    if ring_recording_requested {
        cratonvm_native_api::native_ring::enable(true);
        vm.shared.natives.native_methods.flush_native_ring_names();
        cratonvm_vm::dispatch_trace::enable();
    }

    // `--stack-sample-ms`: periodic, time-weighted Java-frame profiler. Arms
    // sampling mode (which makes the interpreter CONSUME each dump request
    // instead of latching it once per nested `execute()`), then re-arms the
    // request every interval. Never aborts the process, so it composes with a
    // normal run; the run just gets slower in proportion to the sample rate.
    if let Some(ms) = args.stack_sample_ms.filter(|ms| *ms > 0) {
        vm.shared.enable_stack_sampling();
        let shared_for_sampler = std::sync::Arc::clone(&vm.shared);
        let sampler_completed = std::sync::Arc::clone(&watchdog_completed);
        std::thread::Builder::new()
            .name("cratonvm-stack-sampler".into())
            .spawn(move || {
                eprintln!("=== stack sampler: armed at {ms}ms intervals ===");
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                    if sampler_completed.load(std::sync::atomic::Ordering::SeqCst) {
                        return;
                    }
                    shared_for_sampler.request_stack_sample();
                }
            })
            .ok();
    }

    if let Some(secs) = effective_watchdog {
        // Enable the native-call ring buffer so the watchdog's "0 Java
        // threads dumped" fallback can show the last ~64 native methods
        // every thread entered. Without this, the ring's
        // `dump_to_stderr` reports "recording disabled" and a hang in
        // pure Rust runtime code has no actionable diagnostic.
        // Recording cost (single relaxed AtomicBool load on entry, plus
        // a parking_lot::Mutex when set) is negligible per call but adds
        // up across a full run, so we only arm it when a diagnostic was
        // EXPLICITLY requested (see `ring_recording_requested` above). The
        // A watchdog armed through the env default still aborts + dumps Java
        // frames; ring detail is reserved for runs that asked for it.
        if ring_recording_requested {
            cratonvm_native_api::native_ring::enable(true);
            vm.shared.natives.native_methods.flush_native_ring_names();
            // T19.H1 — also enable the dispatch-trace ring. The native-call
            // ring records only opaque fn-pointers from two dispatch sites;
            // the dispatch trace records *named* class.method.desc for every
            // bytecode-method entry and every `safe_native_call`, which is
            // the actionable diagnostic for "main thread is in native code".
            cratonvm_vm::dispatch_trace::enable();
        }
        // Arm the interpreter's dump gate BEFORE the watchdog exists, so a
        // thread that is already deep inside one `execute_frame` when the
        // deadline fires still observes the request (see
        // `SharedVm::arm_stack_dump_watch`).
        vm.shared.arm_stack_dump_watch();
        let shared_for_watchdog = std::sync::Arc::clone(&vm.shared);
        // RKC16N.5 — capture the audit-dump paths into the watchdog
        // thread so a hung run still produces a missing-natives
        // census. Without this, the only flush path is the
        // clean-shutdown branch at the end of `main()`, and every
        // watchdog-killed run loses the JSON we use for KC16/KC26
        // boot debugging.
        let watchdog_dump_path = args.dump_missing_natives.clone();
        let watchdog_dump_grouped_path = args.dump_missing_natives_grouped.clone();
        let watchdog_completed_for_thread = std::sync::Arc::clone(&watchdog_completed);
        std::thread::Builder::new()
            .name("cratonvm-stack-watchdog".into())
            .spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(secs));
                // Cancellation: if `run()` already finished (e.g. it completed
                // just after a tight deadline), don't dump or abort — exit
                // cleanly. Checked here right after the deadline sleep, and
                // again immediately before `abort()` below.
                if watchdog_completed_for_thread.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                // Banner first so the user can tell we got this far.
                eprintln!(
                    "=== T19.H1 watchdog: deadline of {secs}s elapsed; \
                     requesting thread stack dumps ==="
                );
                shared_for_watchdog.request_stack_dump();

                // Give interpreter threads a short grace period to hit
                // the hot-loop check and flush their dumps. We poll the
                // ack counter so we exit the grace period as soon as
                // every reachable thread has responded.
                let grace_start = std::time::Instant::now();
                let grace = std::time::Duration::from_secs(3);
                while grace_start.elapsed() < grace {
                    let acks = shared_for_watchdog.stack_dump_ack_count();
                    // We can't know the exact thread count without
                    // holding the registry lock, but the registry only
                    // grows, so reaching any positive ack count and
                    // then stabilising is the signal we want. Short
                    // sleeps keep the abort latency bounded.
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    let acks2 = shared_for_watchdog.stack_dump_ack_count();
                    if acks > 0 && acks == acks2 {
                        break;
                    }
                }

                let total_acks = shared_for_watchdog.stack_dump_ack_count();
                // Print the per-thread summary HERE, not at request time: only
                // now is it known which threads answered, and that is what lets
                // the summary tell a RUNNING-and-dumped thread apart from a
                // RUNNING-and-silent one (JIT-compiled code or a long native
                // call). See `SharedVm::dump_thread_summary_after_dumps`.
                shared_for_watchdog.dump_thread_summary_after_dumps();
                // A stall whose reactor threads are parked in `select` — i.e.
                // working — is not explained by any thread dump: the question
                // is what the selector had to hand them, and `interest_ops` vs
                // `ready_ops` per registered key answers it at the moment of
                // the stall. Costs nothing until this deadline fires.
                cratonvm_native_io::nio_selector::dump_selector_state_to_stderr();
                eprintln!(
                    "=== T19.H1 watchdog: {total_acks} thread(s) dumped; \
                     aborting process ==="
                );

                // KC-watchdog-native: when zero Java threads ack'd a dump,
                // no thread reached an interpreter dispatch point. That is
                // TWO states, not one, and this block used to assert the
                // wrong one of them as fact ("main thread is in native
                // (Rust) code"):
                //
                //   * parked in native (Rust) code — a JNI/native-method
                //     loop or a deadlock on a runtime mutex; or
                //   * RUNNING in JIT-COMPILED code, which the interpreter's
                //     dump hook cannot observe at all because the hook lives
                //     in the dispatch loop the thread is not executing.
                //
                // `--nojit` separates them in one re-run, and the message
                // below now says so. Guessing cost
                // known-issues/netty/brotli-integration-test-hangs-outside-the-interpreter
                // an entire investigation: nothing was blocked, a compiled
                // `ByteBuf.writeByte` loop was simply 200x too slow.
                //
                // Either way the Java-frame dump produces nothing, so fall
                // back to:
                //   1. The watchdog thread's own native backtrace (cheap
                //      and tells you WHERE in the runtime the watchdog
                //      is reached from — usually right after the sleep,
                //      which is uninteresting, but confirms the watchdog
                //      thread didn't itself deadlock).
                //   2. The PID + a hint to attach an external debugger
                //      (cdb / WinDbg / `rust-lldb -p <pid>`) for full
                //      thread coverage. We can't safely walk other
                //      threads' native stacks from a portable Rust
                //      thread without OS-specific facilities (Windows
                //      `MiniDumpWriteDump`, Linux `ptrace`, etc.).
                if total_acks == 0 {
                    let pid = std::process::id();
                    eprintln!(
                        "=== T19.H1 watchdog: no Java thread reached an \
                         interpreter dispatch point. That is EITHER \
                         JIT-compiled code (which this hook cannot observe \
                         — it lives in the dispatch loop) OR native (Rust) \
                         code. Do NOT assume the second: re-run with \
                         --nojit, and if the frame dumps appear there the \
                         thread was in compiled code and was RUNNING, not \
                         stuck. pid={pid}. To settle it on the live process \
                         instead, attach a native debugger before the 3s \
                         post-dump grace ends (Windows: `cdb -p {pid}` then \
                         `~* k`; Linux: `gdb -p {pid}` then `thread apply \
                         all bt`) — a thread burning CPU in \
                         `jit_invoke_*`/compiled frames is the first case, \
                         one parked in a futex/read is the second. ==="
                    );
                    let bt = std::backtrace::Backtrace::force_capture();
                    eprintln!(
                        "--- T19.H1 watchdog native backtrace (watchdog \
                         thread; FYI only) ---\n{bt}\n--- end native \
                         backtrace ---"
                    );
                    // PERF: ring recording is opt-in now (see
                    // `ring_recording_requested`). If it wasn't requested,
                    // the ring dumps below will say "recording disabled";
                    // tell the operator how to capture them next time so
                    // the diagnostic isn't a dead end.
                    if !ring_recording_requested {
                        eprintln!(
                            "=== T19.H1 watchdog: native-call/dispatch \
                             rings were not recording (opt-in). Re-run with \
                             `--stack-dump-on-timeout=N` or \
                             `CRATONVM_ENABLE_NATIVE_RING=1` to capture the \
                             last native methods leading up to the hang. ==="
                        );
                    }
                    // KC-watchdog-native: dump the native-call ring
                    // buffer. The last entry with `STILL-IN-NATIVE`
                    // marks the hang site.
                    cratonvm_native_api::native_ring::dump_to_stderr();
                    // T19.H1: dump the dispatch-trace ring too — it
                    // records *named* class.method.desc for the last
                    // 256 bytecode-method entries and native dispatches
                    // (across all threads), so the last few NAT/BC
                    // entries pinpoint the hung native and its caller.
                    cratonvm_vm::dispatch_trace::dump_to_stderr_unconditional(
                        "watchdog-native-hang",
                    );
                }

                // RKC16N.5 — flush the missing-natives audit BEFORE
                // `process::abort()` so a hung or watchdog-killed run
                // still produces the diagnostic JSON. Errors are
                // logged but never unwrap; abort still happens.
                shared_for_watchdog.dump_missing_natives();
                if let Some(path) = watchdog_dump_path.as_deref() {
                    match shared_for_watchdog.dump_missing_natives_json(path) {
                        Ok(()) => eprintln!(
                            "=== T19.H1 watchdog: missing-natives audit \
                             flushed (json={path}) ==="
                        ),
                        Err(e) => eprintln!(
                            "=== T19.H1 watchdog: failed to flush \
                             missing-natives JSON to {path}: {e} ==="
                        ),
                    }
                }
                if let Some(path) = watchdog_dump_grouped_path.as_deref() {
                    match shared_for_watchdog.dump_missing_natives_grouped_json(path) {
                        Ok(()) => eprintln!(
                            "=== T19.H1 watchdog: missing-natives audit \
                             flushed (grouped json={path}) ==="
                        ),
                        Err(e) => eprintln!(
                            "=== T19.H1 watchdog: failed to flush \
                             grouped missing-natives JSON to {path}: {e} ==="
                        ),
                    }
                }

                // Flush the stderr handle so our banners land before
                // the abort kills the process.
                use std::io::Write;
                let _ = std::io::stderr().flush();

                // Final cancellation check: `run()` may have completed during
                // the post-dump grace window. Don't abort a finished run.
                if watchdog_completed_for_thread.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                std::process::abort();
            })
            .context("failed to spawn stack-dump watchdog thread")?;
        // Arm the RAII guard so the completed flag is set on every exit path
        // of `run()` once the watchdog is live.
        _watchdog_completion_guard = Some(WatchdogCompletionGuard {
            flag: std::sync::Arc::clone(&watchdog_completed),
        });
        eprintln!("[cratonvm] stack-dump watchdog armed: will dump + abort after {secs}s");
    }

    // T14: Run System.initPhase1() when booting from real JDK classes
    // BEFORE loading the user main class.
    //
    // In HotSpot this is called from Threads::create_vm() after the
    // bootstrap classloader is initialized but before any user class is
    // resolved. It sets up system properties, encodings, and the standard
    // I/O streams (System.in/out/err).
    //
    // Previously the main class was loaded first, which forced eager
    // resolution of `java/lang/Object`, `String`, etc. under an
    // uninitialised system-properties / charset subsystem. A
    // NoClassDefFoundError originating in the half-bootstrapped JDK then
    // surfaced as the confusing "Could not find or load main class …"
    // instead of a clean bootstrap error.
    //
    // Phase 5 WP6: a restored boot image is past this point already (the
    // image is the state these calls produced), here and through P1 below.
    if !restored && vm.shared.config.java_home.is_some() {
        // nio21 wave 21 L5: the `initPhase1` slot (the call, its fallback,
        // the level-2 bump and the deferred tail's `phase1` placement) is
        // `Vm::boot_init_phase1`, shared with `libcratonvm::bootstrap`; the
        // code moved there unchanged.
        let phase1 = vm.boot_init_phase1();

        // Initialise the module system. This is HotSpot's `System.initPhase2`
        // slot in the boot sequence, and it is the only place it belongs.
        //
        // WP1.3: initPhase2 / initPhase3 are pure-Java methods on
        // `java.lang.System`. `initPhase2(boolean, boolean)` is
        // `ModuleBootstrap.boot()` plus `VM.initLevel(2)`; `initPhase3`
        // installs `ClassLoader.scl` and sets the TCCL. We do not invoke
        // either, and the reason for initPhase2 is now a MEASUREMENT rather
        // than a policy — see docs/feature-designs/real-jdk-mode.md.
        //
        // Measured 2026-08-12, one process per arm, by invoking
        // `System.initPhase2` reflectively inside the VM under test: it
        // reaches `ModuleBootstrap.boot()` → `SystemModuleFinders.ofSystem()`
        // → `ofModuleInfos()` → `ImageReader.getModuleNames()` and dies inside
        // `ImageReader$SharedImageReader.imageFileAttributes()` with
        // `UncheckedIOException` / `NoSuchFileException: ` — an EMPTY path —
        // returning JNI_ERR (-1) and changing NOTHING: the provider counts
        // below stay at 0. So running it is not a fix we merely have not
        // written; it is a route that is currently closed.
        //
        // WHY it is closed, precisely, because that is the durable part:
        // `ImageReaderFactory` is boot-loader-defined, so it builds the
        // runtime-image path with
        // `sun.nio.fs.DefaultFileSystemProvider.theFileSystem().getPath(...)`
        // rather than `FileSystems.getDefault()`. In CratonVM those are two
        // DIFFERENT `WindowsFileSystem` instances (HotSpot: one, identity-equal),
        // and `Path`s minted by the boot-loader one are inert — `Files.exists`
        // answers false, `readAttributes` throws `NoSuchFileException`, and
        // `equals` against the same path from the default filesystem is false,
        // for a `Path` whose own `toString()` is correct. Repairing that is the
        // prerequisite for ever running the real `initPhase2`, and it is not in
        // this file.
        //
        // What DOES initialise the module system here is `ModuleLayer.boot()`.
        // Its native (`register_jboss_jdkspecific`, last-writer-wins over the
        // `phases_late` stub, both `NativeKind::Bridge` so strict mode keeps
        // them) runs `build_boot_layer` → `populate_boot_layer_modules` →
        // `ServicesCatalog.getServicesCatalog(scl).register(module)` for every
        // registered module. That IS this VM's `ModuleBootstrap.boot()`. Like
        // the JDK's it runs exactly once, and it runs HERE — before the level
        // advances below — because `VM.initLevel(4)` wakes every
        // `awaitInitLevel` waiter, and a thread woken at SYSTEM_BOOTED is
        // entitled to assume the module system came up at level 2. Its one
        // externally ordered dependency is `ClassLoader.getSystemClassLoader()`,
        // which is a native with no init-level gate, so it does not need the
        // bump that follows.
        //
        // Until this runs, `ServiceLoader` returns ZERO module-declared
        // providers while classpath `META-INF/services` providers keep working
        // — which is exactly why a `ServiceLoader` probe reads green and this
        // stayed hidden. Measured under `--real-jdk`:
        // `java.nio.file.spi.FileSystemProvider` 2 → 0,
        // `java.util.spi.ToolProvider` 9 → 0, `javax.tools.JavaCompiler` 1 → 0,
        // and `ToolProvider.getSystemJavaCompiler()` null, which sent H2's
        // `SourceCompiler` down a `com.sun.tools.javac` path HotSpot never
        // takes. In `--real-jdk` the `SyntheticStub` `ServiceLoader` natives
        // covered the gap; `--real-jdk` refuses that kind at registration, so
        // the same omission stopped being invisible and became a wrong answer.
        //
        // The failure is NOT swallowed. HotSpot treats a non-zero `initPhase2`
        // as fatal to VM creation. We warn rather than abort because this
        // substitute is narrower than the JDK's phase — an application that
        // never looks up a service is unharmed — but the operator is told,
        // because from here on every module-declared provider is silently
        // absent.
        //
        // SBR2 wave 9 (lane MODLOAD): the layer is built by calling the builder
        // directly, NOT through `ModuleLayer.boot()`: under `--real-jdk` that
        // method is the JDK's own `getstatic System.bootLayer` (its native is
        // retired), which answers null until something builds the layer. The
        // builder is the same function the compatible-mode native runs, so the
        // two modes build the identical layer. Right after it, `--real-jdk`
        // runs the JDK-object half of the boot (`finish_boot_layer`: the real
        // `ModuleReference`s, the system `ModuleFinder`, the boot
        // `Configuration`, `BuiltinClassLoader.loadModule`); it is a no-op in
        // `--compatible`.
        //
        // nio21 wave 21 L5: the build, its log line and the deferred tail's
        // `layer` placement are `Vm::boot_module_system`, shared with
        // `libcratonvm::bootstrap`; the code moved there unchanged.
        vm.boot_module_system(phase1);
    }

    // Interpreter round i1 wave 47, lane L4: read the JDK's boot/platform
    // module tables (`ModuleLoaderMap$Modules`) into their per-VM memo now,
    // while running Java is safe, so a HotSpot-worded exception message can
    // name a `java.sql` class's loader `'platform'` without running Java
    // mid-raise (`exceptions::message_loader_id`). A failed read leaves the
    // memo cold and the message as it was.
    if !restored && vm.shared.config.java_home.is_some() {
        let mut ctx = cratonvm_vm::vm::NativeContextImpl {
            shared: &vm.shared,
            thread: &mut vm.main_thread,
        };
        let warmed = cratonvm_native_builtins::classloader::warm_builtin_module_sets(&mut ctx);
        tracing::debug!(warmed, "boot/platform module tables read");
        // Wave 48, lane L5: the same table for the access and
        // loader-constraint texts the class manager words
        // (`access_control::message_loader_of`).
        if let Some(modules) =
            cratonvm_native_builtins::classloader::memoised_platform_modules(&vm.shared.loader_identity)
        {
            vm.shared
                .classes
                .class_manager
                .write()
                .module_registry
                .set_platform_loader_modules(modules);
        }
        // zt df4 §9c: and the boot set, so an image module in neither is an
        // application-loader module (the resolution door's initiating rows),
        // as `CRATONVM_IMAGE_APP_MODULE_LOADERS` reports such a class's loader.
        let app_modules = vm.shared.natives.native_methods.image_app_module_loaders();
        if let Some(modules) = app_modules
            .then(|| {
                cratonvm_native_builtins::classloader::memoised_boot_modules(
                    &vm.shared.loader_identity,
                )
            })
            .flatten()
        {
            vm.shared
                .classes
                .class_manager
                .write()
                .module_registry
                .set_boot_loader_modules(modules);
        }
    }

    // The launcher owns catchable process termination; embedders retain
    // control of their host signals. -Xrs preserves the OS dispositions.
    //
    // Phase 5 WP3: through the VM's deferred starts -- a boot image's capture
    // run installs neither the dispositions (kernel state) nor the dispatcher
    // thread before its snapshot point; the restore path's
    // `run_deferred_starts` fills this slot, which this frame still owns, so
    // the guard drops here at the end of `main` as before. A normal run fills
    // it on this call.
    // A restored run takes the capture's slot from the image root (its start
    // is queued in the image and runs at P1).
    let _launcher_signals = match restored_launcher_signals.take() {
        Some(slot) => slot,
        None => vm.shared.deferred_starts.start_or_defer_into(
            &vm.shared,
            "LauncherSignals",
            cratonvm_vm::runtime::launcher_signals::LauncherSignals::start,
        ),
    };

    // WP1.3: right before `main()` starts, advance to level 4 —
    // HotSpot's "VM fully initialized" state.  This is the signal
    // that `ClassLoader.getSystemClassLoader()` may return `scl` if
    // it is populated (in cratonvm it usually isn't, so callers fall
    // through to `getBuiltinAppClassLoader()` without harm), that
    // `Thread.currentThread().getName()` is safe, and that every
    // subsystem keyed on `VM.awaitInitLevel(4)` can proceed.  We
    // also briefly pass through level 3 so any probe between 2 and
    // 4 observes a level 3 transition.
    if !restored {
        vm.shared.set_init_level(3);
        vm.shared.set_init_level(4);
        // gch h1/jp (gcg-g1o): the launcher preallocates the OOME at its own
        // call below; `Vm::invoke`'s lazy preallocation is for embedders that
        // never do.
        vm.own_the_oome_preallocation();
    }

    // spring-bug-05: advance the REAL `jdk.internal.misc.VM.initLevel` static
    // field. `set_init_level` above only updates CratonVM's internal counter and
    // the `VM.initLevel()` *method* native — but `VM.isModuleSystemInited()`
    // (Proxy.java's gate at ProxyBuilder) reads the static *field* directly
    // (`initLevel >= MODULE_SYSTEM_INITED`). Since we never run the real
    // `System.initPhase2/3` (which would call `VM.initLevel(int)`), the field
    // stays 0 and every JDK dynamic Proxy throws `InternalError: Proxy is not
    // supported until module system is fully initialized`. The setter
    // `VM.initLevel(I)V` is real bytecode (not natively shadowed): invoking it
    // sets the field to SYSTEM_BOOTED (4) and wakes `awaitInitLevel` waiters,
    // exactly as a fully-booted HotSpot would. Its initLevel write is best
    // effort; java.lang.invoke initialization must succeed before main starts.
    //
    // The shared bootstrap initializes MethodHandleNatives in both real-JDK
    // policies; its initializer publishes javaLangInvokeInited exactly once.
    if !restored && vm.shared.config.java_home.is_some() {
        vm.boot_publish_jdk_booted().map_err(|err| java_bootstrap_error(&vm, err))?;
        if let Some(census) = &image_census {
            census.at_p1("stage-b: after boot_publish_jdk_booted");
        }
    }
    // Phase 5 WP5/WP6: P1 (stage b). A capture writes its image here and a
    // restored run rebinds what needs the VM; both then start the threads the
    // capture deferred, and go on to the main class as a cold run does.
    // The launcher's main-class step (load the class, `LauncherHelper.
    // checkAndLoadMain`) depends only on the command line, which is the image's
    // key, and HotSpot runs it before `main` too: a capture does it here, so the
    // image carries its answer and a restored run skips ~6 ms of interpreted
    // Java. Only for a plain class launch over a real JDK from directory class
    // path entries (a jar's descriptors, the module path and an agent keep
    // the late step below). `early_main` is that step's result either way.
    let mut early_main: Option<(
        std::result::Result<cratonvm_vm::classloading::ClassId, cratonvm_types::error::VmError>,
        Option<std::result::Result<cratonvm_vm::vm::LauncherMain, MethodCallFailed>>,
    )> = None;
    if let boot_image_launch::AtP0::Restored(r) = &boot_image {
        early_main = r.early_main().map(|(id, is_static, no_arg)| {
            let class_id = cratonvm_vm::classloading::ClassId::new(id);
            (Ok(class_id), Some(Ok(cratonvm_vm::vm::LauncherMain { class_id, is_static, no_arg })))
        });
    }
    let early_ok = args.jar.is_none()
        && java_agents.is_empty()
        && vm.launcher_runs_launcher_helper()
        && !class_name.starts_with("com/sun/tools/javac/launcher/")
        && !vm.shared.config.jvm_args.iter().any(|a| a.contains("jdk.module"))
        && !vm.shared.config.classpath.is_empty()
        && vm.shared.config.classpath.iter().all(|e| std::path::Path::new(e).is_dir());
    if let (boot_image_launch::AtP0::Capture(capture), true) = (&mut boot_image, early_ok) {
        let load = vm.shared.load_class_transformed(&mut vm.main_thread, &class_name);
        let answer = match &load {
            Ok(_) => Some(vm.launcher_check_main_via_helper(&class_name, None)),
            Err(_) => None,
        };
        match &answer {
            Some(Ok(m)) => capture.set_early_main(m.class_id.as_u32(), m.is_static, m.no_arg),
            _ => capture.fail_early("the early main-class step did not complete".into()),
        }
        early_main = Some((load, answer));
    }
    match std::mem::replace(&mut boot_image, boot_image_launch::AtP0::Cold) {
        boot_image_launch::AtP0::Capture(capture) => capture.at_p1(&vm, &_launcher_signals),
        boot_image_launch::AtP0::Restored(r) => r.at_p1(&vm),
        boot_image_launch::AtP0::Cold => {}
    }

    // The boot module layer is materialised in the `System.initPhase2` slot
    // above, before the level advances — NOT here. It used to be invoked at
    // this point as a behavioural patch; that placement left a window in which
    // `VM.initLevel(4)` had already woken every `awaitInitLevel` waiter while
    // the services catalog was still empty. There is exactly one such call.

    // Pre-allocate the singleton java.lang.OutOfMemoryError while the heap is
    // still fresh, so a later 100%-full-heap OOM (in either user code or a
    // premain) can be thrown without allocating the throwable — which would
    // otherwise hard-abort in the non-fallible String allocator. Idempotent and
    // best-effort: if the class isn't loadable yet it leaves the slot empty and
    // the OOM paths keep their prior behaviour.
    cratonvm_vm::runtime::exceptions::ensure_singleton_oom(&vm.shared, &mut vm.main_thread);

    // WP2.4-C — run every `-javaagent:` agent's `premain(String,
    // Instrumentation)` hook BEFORE the application's `main`. Before it, the
    // `-agentlib:jdwp` server (interpreter round i4 wave 85, lane L1) starts
    // where the JDK back end's `VMInit` callback starts it: it listens (the
    // `Listening for transport dt_socket at address:` line) or connects,
    // and with `suspend=y` sends `VM_START` and holds this thread until the
    // debugger resumes, so a breakpoint in the main class's static
    // initializer is set in time. (HotSpot runs the agents' `VMInit`
    // callbacks in command-line order; this runs JDWP's first.)
    if let Err(failure) = cratonvm_vm::start_jdwp_agent(&vm.shared, &mut vm.main_thread) {
        exit_with_jdwp_agent_failure(&failure);
    }
    // A failed premain start ends the VM, as JPLIS's `abortJVM` does (interpreter round i4 wave 84,
    // lane L1: `AgentLoadError::JplisAbort`, its stderr already printed).
    if !java_agents.is_empty() {
        let res = cratonvm_vm::runtime::agent_loader::invoke_premains(
            &vm.shared,
            &mut vm.main_thread,
            &java_agents,
        );
        if let Err(e) = res {
            if matches!(e, cratonvm_vm::runtime::agent_loader::AgentLoadError::JplisAbort { .. }) {
                // JPLIS's stderr is printed (interpreter round i4 wave 84).
                abort_as_jplis_abort_jvm();
            }
            finish_jdk_only(&args, &vm.shared, &mut jdk_only_watermark);
            bail!("javaagent premain aborted VM: {e}");
        }
    }

    // Resolve and load the user main class.
    //
    // Two orderings constrain this point. It must come after `initPhase1` (see
    // the comment on that block above) — that is why it is not next to the
    // classpath setup. And it must come after `invoke_premains`, because the
    // `java.lang.instrument` contract is that a transformer registered by
    // `premain` is offered *every subsequent definition*, and the application's
    // main class is the first one an agent expects to see. HotSpot starts its
    // agents during VM creation and loads the main class afterwards, from the
    // launcher; loading it first made the main class the one class a
    // `-javaagent:` could never instrument. `load_class_transformed` is the
    // entry point that offers the bytes to the chain (a no-op with no agent).
    //
    // `CRATONVM_LAUNCHER_HELPER_JAR_NO_PRELOAD` (default ON since wave 134, `=0` off; interpreter round
    // i6 wave 121, lane L5, `i58-L5-proposal-the-launcher-runs-launcherhelper`):
    // a `-jar` launch the JDK's helper handles (`LM_JAR`) with no `-javaagent:`
    // does not pre-load the manifest's class. HotSpot's `LauncherHelper` starts
    // the jar's `Launcher-Agent-Class` BEFORE it loads the main class, so that
    // agent's transformer sees the main class's definition; the pre-load made it
    // the one class such an agent could never see. With no `-javaagent:` there
    // is nothing for the pre-load to offer the bytes to first. The helper's own
    // `JarFile` read starts the common `Cleaner` as HotSpot's does. If the
    // helper cannot run, the fallback below loads the class then.
    let skip_preload = args.jar.is_some()
        && java_agents.is_empty()
        && vm.launcher_runs_helper_jar();
    let (early_load, early_answer) = match early_main {
        Some((load, answer)) => (Some(load), answer),
        None => (None, None),
    };
    let main_class_load = if early_load.is_some() {
        early_load
    } else if skip_preload {
        // Wave 133 (lane L7b): HotSpot's helper opens the jar, which starts the
        // common `Cleaner`, before the `Launcher-Agent-Class` or the main class;
        // start it here so no arm depends on which `JarFile` the mode builds.
        vm.launcher_start_common_cleaner_for_jar();
        None
    } else {
        Some(
            vm.shared
                .load_class_transformed(&mut vm.main_thread, &class_name),
        )
    };
    // HotSpot's application loader starts the common `Cleaner` while it reads
    // the main class's bytes; see `launcher_start_common_cleaner`.
    if let Some(Ok(main_id)) = &main_class_load {
        vm.launcher_start_common_cleaner(*main_id);
    }
    // `CRATONVM_LAUNCHER_HELPER`: over a real JDK the JDK's own
    // `LauncherHelper.checkAndLoadMain` loads (finding the class the line
    // above defined) and validates the main class, printing HotSpot's
    // `java.launcher.*` text and exiting 1 on any failure, the load's
    // included; its `isStaticMain` / `noArgMain` say how `main` is called.
    // A helper the VM cannot run falls back to the VM's own check below
    // (interpreter round i2 wave 59, lane L5;
    // `i58-L5-proposal-the-launcher-runs-launcherhelper`).
    let helper_answer = if let Some(answer) = early_answer {
        match answer {
            Err(MethodCallFailed::InternalError(e)) => {
                tracing::warn!("LauncherHelper.checkAndLoadMain could not run ({e}); the VM checks main itself");
                None
            }
            answer => Some(answer),
        }
    } else if vm.launcher_runs_launcher_helper() {
        // `-jar` hands the JAR itself over (`LM_JAR`) under
        // `CRATONVM_LAUNCHER_HELPER_JAR` (interpreter round i2 wave 60, lane L5).
        match vm.launcher_check_main_via_helper(&class_name, args.jar.as_deref()) {
            Err(MethodCallFailed::InternalError(e)) => {
                tracing::warn!("LauncherHelper.checkAndLoadMain could not run ({e}); the VM checks main itself");
                None
            }
            answer => Some(answer),
        }
    } else {
        None
    };
    let launcher_main = match helper_answer {
        Some(answer) => answer,
        None => {
            // The pre-load skipped (`CRATONVM_LAUNCHER_HELPER_JAR_NO_PRELOAD`)
            // happens here when the helper could not run.
            let main_class_load = match main_class_load {
                Some(load) => load,
                None => {
                    let load = vm
                        .shared
                        .load_class_transformed(&mut vm.main_thread, &class_name);
                    // Wave 122 (lane L9): the skipped pre-load also skipped
                    // the common `Cleaner` start above, and the helper's
                    // `JarFile` read that stands for it did not run here.
                    if let Ok(main_id) = &load {
                        vm.launcher_start_common_cleaner(*main_id);
                    }
                    load
                }
            };
            Ok(launcher_check_main_natively(
                &args,
                &mut vm,
                &class_name,
                main_class_load,
                &mut jdk_only_watermark,
            )?)
        }
    };

    // Phase accounting: startup ends here, execution begins. This is the
    // single span the `coarse` level's reconciliation rests on — every other
    // category nests inside it and subtracts (see §10.5 of
    // `docs/observability/phase-accounting.md`). At `fine` it records nothing
    // and the `interpretation` / `jit_execution` spans carry the split instead,
    // which is why it can be opened unconditionally.
    //
    // `catch_unwind` swallows a panic rather than unwinding through these, so
    // both `.end()` calls below are reached on every path out of the invoke.
    phase_startup.end();
    let phase_exec = phase::enter(phase::Category::JavaExecution);

    // Invoke `main` the way `java.c`'s `JavaMain` does: its JNI lookup
    // initialises the main class, an instance `main` runs on an object made
    // by the no-argument constructor, and the `String[]` is passed unless
    // `main` takes no parameters (`Vm::launcher_prepare_main` /
    // `launcher_invoke_main`; interpreter round i2 wave 59, lane L5).
    let main_start = std::time::Instant::now();
    // Whether `main` itself was entered. A throwable from before (the main
    // class's `<clinit>`, the instance receiver's allocation or constructor)
    // is `java.c`'s `CHECK_EXCEPTION_NULL_LEAVE` case, which HotSpot reports
    // through `ExceptionDescribe`, not the thread's uncaught-exception
    // dispatch (`Vm::describe_launcher_exception`; zs s2/nd2's
    // `premain_exception_describe`).
    let mut entered_main = false;
    let main_pin_base = vm.main_thread.native_pin_roots.len();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // The helper's own throwable is reported as `main`'s uncaught
        // exception (`java.c`'s `CHECK_EXCEPTION_NULL_LEAVE`).
        let launcher_main = launcher_main?;
        // `CreateApplicationArgs` precedes JNI main lookup/initialization.
        // Root its result while `<clinit>` and a receiver constructor may GC
        // (zs s4/nd4, probe `Zs4Nd4StaticArgs`: built after, a `<clinit>`
        // that fills the heap failed before `main` where HotSpot enters it).
        // `CRATONVM_ZGC_LAUNCHER_ARGS_FIRST=0` (ZGC only) restores the old
        // order: built after the receiver, which stays pinned meanwhile.
        // Every pin from `main_pin_base` up is released after the call.
        let args_pin = if vm.launcher_args_before_init() {
            let array = vm.launcher_main_arguments(&args.args)?;
            let pin = vm.main_thread.native_pin_roots.len();
            vm.main_thread.native_pin_roots.push(array);
            Some(pin)
        } else {
            None
        };
        let receiver_pin = vm.launcher_prepare_main(&launcher_main)?;
        let args_array = match args_pin {
            Some(pin) => vm.main_thread.native_pin_roots[pin],
            None => vm.launcher_main_arguments(&args.args)?,
        };
        entered_main = true;
        // zt t7/cl2: separates "main never ran" from "main ran and its
        // output was lost" (`CRATONVM_DBG_PRINT_LOSS` names the latter).
        if launcher_outcome_lines_on() {
            eprintln!(
                "[cratonvm] main entered: static={} no_arg={}",
                launcher_main.is_static, launcher_main.no_arg
            );
        }
        vm.launcher_invoke_main(&launcher_main, receiver_pin, args_array)
    }));
    if launcher_outcome_lines_on() {
        let how = match &result {
            Ok(Ok(_)) => "returned normally".to_string(),
            Ok(Err(MethodCallFailed::ExceptionThrown(exc))) => {
                format!("threw {}", vm.describe_exception(*exc))
            }
            Ok(Err(MethodCallFailed::InternalError(e))) => format!("internal error: {e}"),
            Err(_) => "panicked".to_string(),
        };
        eprintln!("[cratonvm] main ended (entered={entered_main}): {how}");
    }
    // zt t7/cl3: where `System.out` / `System.err` stand after `main`, for a
    // run whose output went nowhere with no print-loss note (the JDK's own
    // `PrintStream` bytecode absorbs its `IOException`s, outside the notes).
    if false {
        let ctx = cratonvm_vm::vm::NativeContextImpl {
            shared: &vm.shared,
            thread: &mut vm.main_thread,
        };
        for line in cratonvm_native_builtins::lang_system::system_stream_report(&ctx) {
            eprintln!("[cratonvm] print-state: {line}");
        }
    }
    vm.main_thread.native_pin_roots.truncate(main_pin_base);
    let main_elapsed = main_start.elapsed();
    phase_exec.end();
    // Everything from here to the end of `run()` is teardown: the census
    // dumps, the non-daemon-thread join, the exception renderer. Bound to a
    // named guard held to end of scope so the `bail!` paths below charge it
    // too.
    let _phase_shutdown = phase::enter(phase::Category::VmShutdown);
    tracing::info!("main() completed in {:.2}s", main_elapsed.as_secs_f64());

    // W7-90: the read-side slot-map sweep's PRIMARY trigger.
    //
    // `read_alias::verify_declared_slot_maps` had no caller at all until this
    // line, which is indistinguishable from a detector that reports all-clear —
    // the species this whole campaign is about, sitting inside the instrument
    // built to detect it. Seven `SlotMap`s were published to a sweep that never
    // ran (W7-69 §7.2, W7-75 §7, W7-77 §7).
    //
    // Here rather than at registration because the sweep's one hard requirement
    // is that the class be LOADED, and W7-69 established that at registration
    // time most are not: `java.nio.DirectByteBuffer` is package-private and is
    // not among the 323 classes `bootstrap_core_classes` names, so its
    // `declared_fields` comes back empty and "not loaded" is indistinguishable
    // from "no fields". Immediately after `main` returns is the point in the
    // process with the most classes loaded, and it is where every other
    // self-gated census in this launcher already prints.
    //
    // ABOVE the `match result` deliberately, so a workload that PANICKED out of
    // `main` still produces its census — the same argument the missing-natives
    // dump below makes for being unconditional. `System.exit` never reaches
    // this line; `lang_system::native_system_exit` carries the second trigger
    // for that path.
    //
    // Gated with no `else`: observation only, and the report is printed and
    // dropped. With the flag off this is one `OnceLock` load and a branch, once.
    if cratonvm_native_api::layout_alias::enabled() {
        let _ = vm.sweep_declared_slot_maps("main-returned");
    }

    let result = match result {
        Ok(r) => r,
        Err(panic) => {
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            finish_jdk_only(&args, &vm.shared, &mut jdk_only_watermark);
            bail!("main() panicked: {msg}");
        }
    };

    // NEW-10: before reporting the invocation result, write the
    // missing-natives audit log to the user-specified JSON path. We
    // do this unconditionally (regardless of Ok/Err) so a crashing
    // program still produces a census file. Any I/O error surfaces
    // as a warning — the primary invocation result takes precedence.
    if let Some(path) = &args.dump_missing_natives {
        match vm.shared.dump_missing_natives_json(path) {
            Ok(()) => {
                let count = vm.shared.get_missing_natives().len();
                eprintln!("[cratonvm] wrote {count} missing-native entries to {path}");
            }
            Err(e) => {
                eprintln!(
                    "[cratonvm] warning: could not write missing-natives JSON to {path}: {e}"
                );
            }
        }
    }

    // Real-JDK census artefacts: the class-origin dump, the schema-2 native
    // registry census and the violation report, all three written by the
    // `SharedVm` methods that own them (`dump_class_origins_json`,
    // `dump_native_census_json`, `dump_jdk_only_report_json`) — the launcher
    // supplies the paths and the `--explain-jdk-only` flag and nothing else.
    // Written unconditionally, exactly like the missing-natives dump above, so
    // a program that threw still produces a census. The failing paths earlier
    // in this function write them too.
    finish_jdk_only(&args, &vm.shared, &mut jdk_only_watermark);

    // T2.1.3: grouped-by-module census.
    if let Some(path) = &args.dump_missing_natives_grouped {
        match vm.shared.dump_missing_natives_grouped_json(path) {
            Ok(()) => {
                let grouped = vm.shared.classify_missing_natives_by_module();
                let total: usize = grouped.values().map(|v| v.len()).sum();
                eprintln!(
                    "[cratonvm] wrote {total} missing-native entries across \
                     {} modules to {path}",
                    grouped.len()
                );
            }
            Err(e) => {
                eprintln!(
                    "[cratonvm] warning: could not write grouped missing-natives \
                     JSON to {path}: {e}"
                );
            }
        }
    }

    // A counter nobody can read is not a diagnostic. This reports under the
    // flag that PRODUCED the numbers, not under `intrinsic-stats`, so
    // `CRATONVM_DBG_SITE_ALIAS=1` alone is enough to get the totals.
    if None::<std::ffi::OsString>.is_some() {
        eprintln!(
            "[cratonvm] site-alias: distinct JitSiteKeys={} recycled-key hits={}",
            cratonvm_vm::jit::helpers::site_alias_key_count(),
            cratonvm_vm::jit::helpers::site_alias_hit_count()
        );
    }

    // Interpreter intrinsic-table stats. `CRATONVM_INTRINSIC_STATS=1` prints
    // the steady-state intrinsic-dispatch hit count on shutdown — the
    // counter that verifies acceptance criterion §9 of
    // feature_roadmap_interpreter_intrinsic_table.md.
    if matches!(
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
        Ok("1")
    ) {
        eprintln!(
            "[cratonvm] interpreter intrinsic dispatches: {}",
            cratonvm_vm::runtime::interpreter::intrinsic_hit_count()
        );
        // Compiled code's own funnel bypass. Reported beside the interpreter's
        // counter because the two answer the same question for different
        // execution tiers, and the JIT half is the one that was missing: a
        // compiled loop's calls do not reach the interpreter's inline cache, so
        // a run whose first number moves and whose second stays at zero has NOT
        // been sped up where it is hot. See `jit::helpers::LEAF_NATIVE_HITS`.
        eprintln!(
            "[cratonvm] compiled leaf-native dispatches: {}",
            cratonvm_vm::jit::helpers::leaf_native_hit_count()
        );
        // The non-leaf half of the same cache: these skipped `invoke_or_native`
        // but still entered the funnel. Reported separately because it is the
        // larger population and the one that carries `java.util.concurrent`.
        eprintln!(
            "[cratonvm] compiled site-cached native dispatches (non-leaf): {}",
            cratonvm_vm::jit::helpers::site_cached_native_hit_count()
        );
        // The subset of the line above served as a plain field load rather
        // than a native call: signature-polymorphic `VarHandle` read modes on
        // an ordinary instance field. Zero here beside a non-zero site-cached
        // count means every VarHandle site refused the plan, which is a
        // different fault from "no VarHandle site was reached".
        eprintln!(
            "[cratonvm]   of which VarHandle instance-field reads served directly: {}",
            cratonvm_vm::jit::helpers::varhandle_field_read_hit_count()
        );
        // And the array-element accesses (`AtomicIntegerArray`,
        // `AtomicLongArray`, netty's `MpscIntQueue`) served as one atomic
        // instruction on the element.
        eprintln!(
            "[cratonvm]   of which VarHandle int[]/long[] element accesses served directly: {}",
            cratonvm_vm::jit::helpers::varhandle_array_element_served_count()
        );
        // Compiled `newarray` sites that bump the TLAB inline instead of
        // calling `jit_newarray`. BOTH halves, always: a workload whose arrays
        // all took the helper says nothing about the bump, so a zero on the
        // left has to be distinguishable from "emitted and refused". The
        // decline reasons are the work list.
        let (array_bump, array_stub) = cratonvm_jit::x64::inline_array_site_counts();
        eprintln!(
            "[cratonvm] compiled newarray sites: inline_tlab_bump={array_bump} stub_only={array_stub}"
        );
        for (why, n) in cratonvm_jit::x64::inline_array_declines() {
            eprintln!("[cratonvm]   inline newarray declined {n}x: {why}");
        }
        // zr z3/ja: compiled ZGC `anewarray` sites, both tiers (zero off ZGC,
        // where the arm is never offered).
        let (ref_bump, ref_declined) = cratonvm_jit::x64::inline_ref_array_site_counts();
        eprintln!(
            "[cratonvm] compiled zgc anewarray sites: inline_tlab_bump={ref_bump} declined={ref_declined}"
        );
        // H7-1's instrument: how many times a compiled site's HashMap/
        // ConcurrentMap get/put ladder bound the direct-call helper instead of
        // falling back through the funnel. `H10-1` N3 asks that this be
        // measured rather than only predicted.
        //
        // This line said "(0,0,0) is expected under `--real-jdk` (all four
        // triples are `bridge`, refused at bind time)" until 2026-09-22. Both
        // halves of that were wrong. The refusal never sent the call to
        // bytecode -- the tail ran the same `Bridge` -- so it has been
        // withdrawn, and all four triples now BIND under `--real-jdk` as they
        // always did under the default arm. Non-zero in both modes on a
        // workload that hits the ladder (e.g. RJitMapTierDiff, RMapResizeGc) is
        // the expected shape; a zero says no compiled site saw the call.
        //
        // The `osr=` pair is `H12-1` N2. The OSR door bound `HashMap.get`/`put`
        // with no counter at all, so a prediction about the triple above could
        // be confirmed by a measurement blind to the door doing the binding.
        {
            let (hashmap_get, hashmap_put, chm_get) =
                cratonvm_jit::collection_direct_helper_sites();
            let (osr_get, osr_put) = cratonvm_jit::collection_direct_helper_sites_osr();
            eprintln!(
                "[cratonvm] compiled collection direct-helper binds: hashmap_get={hashmap_get} hashmap_put={hashmap_put} concurrent_hashmap_get={chm_get} osr_hashmap_get={osr_get} osr_hashmap_put={osr_put}"
            );
        }
        // The same read, reached WITHOUT the funnel — a thin direct call baked
        // into compiled code by `VARHANDLE_READ_DIRECT_FNS`. The pair is the
        // engagement evidence for that bind: `served` counts calls that never
        // entered `jit_invoke_dispatch` at all, `declined` counts the ones that
        // did. A non-zero bind count in the "thin direct-helper binds" line with
        // `served=0` here is the specific failure this exists to name — the site
        // compiled and every execution refused.
        {
            let (served, declined) = cratonvm_vm::jit::helpers::varhandle_read_direct_counts();
            eprintln!(
                "[cratonvm] VarHandle read thin direct calls: served={served} declined={declined}",
            );
        }
        // The WRITE direction's two routes, which the read direction has had all
        // along and this one did not. `write thin direct calls` is the bind
        // baked into compiled code; `field CAS in-funnel` is the short circuit
        // inside `jit_invoke_dispatch`, the twin of the read fast path above.
        //
        // The CAS pair is load-bearing in a way the census is NOT: a served CAS
        // still calls `count_jit_native_dispatch`, exactly as a served read
        // does, so `--dump-native-registry` reports the SAME
        // `VarHandle.compareAndSet` count whether the fast path ran or not.
        // These counters are the only way to tell the two apart.
        {
            // The `(sites: ...)` tail is the same one the CAS line below has
            // carried since it landed, and its absence here was a hole rather
            // than a choice. `served`/`declined` count EXECUTIONS; the site pair
            // counts BINDS, per compile door. Without the binds, a
            // `served=0 declined=0` line cannot distinguish "no site ever bound"
            // from "sites bound and nothing executed them" — which is the exact
            // distinction the comment above this block says the read direction's
            // pair exists to make, and the one the CAS line's own comment calls
            // load-bearing.
            //
            // Until this line landed, `varhandle_write_direct_helper_sites()`
            // had ZERO callers workspace-wide, so
            // `VARHANDLE_WRITE_SITES_SINGLEPASS` and `VARHANDLE_WRITE_SITES_OSR`
            // were written by both compile doors (`jit/src/lib.rs`'s single-pass
            // walk and `vm/src/runtime/interpreter/jit_bridge.rs`'s OSR door) and
            // read by nothing. The read direction's sites reach `tiered.rs` and
            // the CAS direction's reach the line below, so this was the one of
            // the three VarHandle families whose bind count was dropped
            // (`docs/feature-designs/jit-ir-optimizations.md`).
            let (served, declined) = cratonvm_vm::jit::helpers::varhandle_write_direct_counts();
            let (vw_sp, vw_osr) = cratonvm_jit::varhandle_write_direct_helper_sites();
            eprintln!(
                "[cratonvm] VarHandle write thin direct calls: served={served} \
                 declined={declined} (sites: singlepass={vw_sp} osr={vw_osr})",
            );
            let (cas_served, cas_declined) =
                cratonvm_vm::jit::helpers::varhandle_field_cas_counts();
            eprintln!(
                "[cratonvm] VarHandle field CAS in-funnel: served={cas_served} declined={cas_declined}",
            );
            // The bound CAS route. Read it TOGETHER with the in-funnel pair
            // above: once `compareAndSet` sites bind, the in-funnel count is
            // supposed to fall to what the interpreter and the declined sites
            // still send through it, and a bind that moved nothing looks
            // exactly like a bind that was never reached unless both are
            // printed.
            let (cd_served, cd_declined) = cratonvm_vm::jit::helpers::varhandle_cas_direct_counts();
            let (cd_sp, cd_osr) = cratonvm_jit::varhandle_cas_direct_helper_sites();
            eprintln!(
                "[cratonvm] VarHandle CAS thin direct calls: served={cd_served} declined={cd_declined} (sites: singlepass={cd_sp} osr={cd_osr})",
            );
        }
        // The exact-receiver `java/util/regex/Matcher` leaf, which is neither of
        // the two above: it is the one by-name fast path that decides per
        // dispatch rather than at cache-fill time. Reported separately because
        // its number is the only thing that distinguishes "the leaf served this
        // call" from "the leaf declined and the generic tail served it, counting
        // it properly" — the two are indistinguishable in the native census,
        // which is what left the §4 gap this leaf was filed for unfalsifiable
        // for as long as it stood.
        eprintln!(
            "[cratonvm] compiled Matcher-leaf dispatches: {}",
            cratonvm_vm::jit::helpers::matcher_leaf_hit_count()
        );
        // A zero above is ambiguous — "nothing here is a leaf" and "every site
        // was refused for a reason nobody intended" look identical — so the
        // fill-time refusal reasons are reported alongside it.
        for (reason, count) in cratonvm_vm::jit::helpers::leaf_native_refusals() {
            eprintln!("[cratonvm]   leaf sites refused, {reason}: {count}");
        }
        // JIT round 22 wave 1, lane runtime (INDY21-5): the `MutableCallSite` /
        // `VolatileCallSite` retarget recompiles this VM asked for
        // (`CRATONVM_JIT_INDY_MUTABLE_RETARGET_RECOMPILE`, opt-in). Both zero
        // with the switch off; with it on, a zero says no workload site was
        // retargeted away from its planned target long enough to ask one.
        {
            let (will_run, refused) =
                cratonvm_vm::runtime::interpreter::indy_retarget_recompile_census(&vm.shared);
            eprintln!(
                "[cratonvm] invokedynamic mutable-site retarget recompiles: requested={will_run} refused={refused}"
            );
        }
        // `Thread.currentThread()` is served one level earlier still: the
        // compilers bake a direct `CALL` to `jit_thread_current_thread_direct`,
        // so those sites never reach the leaf path above, or any dispatch
        // helper at all. The per-door site counts prove the bind is not inert;
        // the denominators are what named the compile door that was missing.
        // See `native-call-funnel-per-call-floor-item2-20260805.md`.
        let (sp_sites, ir_sites, osr_sites) = cratonvm_jit::thread_current_thread_bound_sites();
        let (sp_seen, ir_seen) = cratonvm_jit::static_sites_seen();
        eprintln!(
            "[cratonvm] compiled Thread.currentThread direct calls: {} \
             (sites bound per compile door: single-pass {sp_sites}/{sp_seen}, \
             IR {ir_sites}/{ir_seen}, OSR {osr_sites}; the two denominators are \
             invokestatic sites those ladders examined)",
            cratonvm_vm::jit::helpers::jit_funnel_bypass_count()
        );
        // JNI up-calls that found no thread context. Non-zero means some native
        // called back into Java with the context torn down under it — the shape
        // that lost netty's TLSv1.3 client certificate. Zero is expected for any
        // run whose native calls all originate from Java.
        eprintln!(
            "[cratonvm] JNI up-calls answered with NO thread context: {}",
            cratonvm_vm::native::jni::jni_upcalls_without_context()
        );
        // Why a compiled callee is reached through a Rust helper at all.
        //
        // The netty census (`[DISP_CENSUS]`, `CRATONVM_DBG=mic-prof`) says 98.4%
        // of `jit_invoke_dispatch` calls are `DISPATCH_CACHE` hits — a callee
        // that IS compiled, entered through the helper on every call. These two
        // say why: a statically bound site is offered a direct `CALL` exactly
        // once, while its caller is being compiled, and a callee that is not
        // compiled yet at that instant leaves the site on the helper forever.
        let (bind_hits, bind_misses) = cratonvm_jit::direct_callee_bind_counts();
        eprintln!(
            "[cratonvm] direct callee binds: {bind_hits} bound, {bind_misses} left on the dispatch helper (statically bound sites where a ladder asked for a direct target)"
        );
        // The OSR door's share of that pair. It is a SUBSET of the line above,
        // not a third total.
        //
        // Printed separately because its absence was mistaken for its answer:
        // `osr-door-refused-every-exception-table-callee-FIXED-20260824.md` read
        // `0 bound, 0 left` in both arms of a gate flip and concluded the gate
        // is unreachable from an OSR body. The conclusion was right and the
        // instrument was not -- the OSR ladder was binding and refusing all
        // along and reporting neither, so a door that reported nothing and a
        // door that did nothing printed the same line. A hot loop is always an
        // OSR body, so a zero HERE is the one worth noticing.
        let (osr_hits, osr_misses) = cratonvm_jit::osr_direct_callee_bind_counts();
        eprintln!(
            "[cratonvm]   of which the OSR door: {osr_hits} bound, {osr_misses} left on the dispatch helper"
        );
        // WHICH gate refused. A bare miss total cannot separate a compile-ORDER
        // accident (the callee simply was not compiled yet — repairable by
        // re-binding) from a standing policy refusal (an exception table, a
        // native shadow — which no re-bind touches), and those two want
        // opposite fixes. `unattributed` is the mutator-side door's arms that
        // return a bare `None`; a large value there means this list is the one
        // to extend next, not that the misses are unexplained.
        let reasons = cratonvm_jit::direct_callee_bind_refusal_reasons();
        let attributed: u64 = reasons.iter().map(|(_, c)| *c).sum();
        for (reason, count) in &reasons {
            eprintln!("[cratonvm]   bind refused, {reason}: {count}");
        }
        eprintln!(
            "[cratonvm]   bind refused, unattributed: {}",
            bind_misses.saturating_sub(attributed)
        );
        // OSRENTRY6-2 (JIT round 19 proposal wave 7, lane interp7): the OSR
        // door's own refusals, per VM, with the slow door's bare `None`
        // classified by its cause -- the split EAGER19-4's late-bind OSR waiter
        // is decided on (`callee-class-not-found` and
        // `declaring-class-not-initialized` are the rows a waiter can repair).
        let osr_reasons = cratonvm_vm::jit::helpers::osr_door_bind_refusal_reasons(&vm.shared);
        for (reason, count) in osr_reasons {
            eprintln!("[cratonvm]   OSR door bind refused, {reason}: {count}");
        }
    }

    // JDK-ONLY-WAVE2 §8/§11 census dumps (`CRATONVM_DBG_CHECK_OVERRIDE=1`).
    // Both are no-ops without the flag; they exist to drive the per-family
    // deletion exercise those two records describe but never measured.
    cratonvm_vm::vm::dump_check_override_census();
    cratonvm_vm::vm::dump_canonical_census();
    // The enforcement dial's per-door census (`H17-3` N1). Silent unless
    // `CRATONVM_ENFORCE_NATIVE_SHADOW` is armed or `CRATONVM_DBG_DIAL_DOORS`
    // is set; `reached - yielded` per door is the price the dial is not
    // charging.
    cratonvm_vm::vm::dump_dial_door_census();

    // WS1 diagnostic: final JIT-dispatch-helper profile dump on shutdown
    // (env-gated inside `dump_now` callers; `enabled()` re-checked here).
    if cratonvm_vm::jit::helpers::mic_prof::enabled() {
        cratonvm_vm::jit::helpers::mic_prof::dump_now();
        eprintln!(
            "[MIC_PROF] gc_collections={} total_dispatches={}",
            vm.shared.mem.heap.collection_count(),
            cratonvm_vm::dispatch_trace::total_dispatches()
        );
    }

    // B6: Silent-exit guard. If main() returned Ok but the VM has recorded
    // one or more swallowed errors during class init / invokedynamic / native
    // calls, surface a WARN to stderr so users (and CI) don't mistake a
    // silent exit for a successful run. Exit code stays 0 for compatibility
    // with programs that legitimately produce no stdout.
    // nio21 wave 8 lane L4: under `CRATONVM_DBG=exit-census` (HotSpot prints
    // nothing here; `CRATONVM_STRICT_SWALLOWS=1` still escalates).
    if matches!(result, Ok(_)) && exit_census_on() {
        let swallowed = vm
            .shared
            .debug
            .swallow_counter
            .load(std::sync::atomic::Ordering::Relaxed);
        if swallowed > 0 {
            cratonvm_types::exit_report::emit(format_args!(
                "WARN: main() completed with {swallowed} swallowed VM error(s) \
                 (class-init / invokedynamic / native). Re-run with \
                 RUST_LOG=warn to see each site, or \
                 CRATONVM_STRICT_SWALLOWS=1 to escalate the first swallow to a \
                 panic for diagnosis."
            ));
        }
    }

    // §5 acceptance metric — aggregate G1 pause summary (p50/p99/max young +
    // mixed) to stderr at shutdown when GC stats are requested. Driven by
    // `--verbose:gc` or the `CRATONVM_GC_STATS` env knob so a gauntlet runner
    // can collect the table without `RUST_LOG`. No-op for the generational
    // collector and when no G1 collection ran.
    // `CRATONVM_DBG_G1ACCESSOR` too: the accessor census rides inside
    // `print_gc_summary`, and a census whose only output path is gated behind a
    // DIFFERENT flag prints nothing when you ask for it — which reads as
    // "zero accessor calls" rather than "you never enabled the report".
    // `GC_STATS_REQUESTED` already carries `--verbose:gc` (it is set as soon as
    // the arguments are parsed), so this is the same question
    // `gc_stats_requested()` asks -- and asking it through that function is
    // what keeps the two arms from drifting apart.
    let gc_stats_requested = gc_stats_requested();
    // A stale or refused arena translation is a correctness event, so it is
    // reported whether or not anyone asked for statistics -- gating one behind
    // a stats flag turns "nobody asked" into "nothing happened". With the flag
    // on, the line prints unconditionally so the DENOMINATOR is available too.
    cratonvm_native_builtins::arena_translation_exit_summary(gc_stats_requested);
    // UNGATED, and above the `gc_stats_requested` block on purpose:
    // `CRATONVM_ZGC_METRICS_TSV` writes a machine-readable row for a harness to
    // `join`, and requiring `--verbose:gc` for it would put the row behind the
    // log parsing it exists to replace. `emit_zgc_end_of_run_metrics` claims
    // once per process and is a no-op unless the knob is set, so the cost on a
    // default run is one relaxed swap.
    vm.shared.mem.heap.emit_zgc_end_of_run_metrics();
    if gc_stats_requested {
        // Claimed rather than called: `maybe_dump_shutdown_reports` prints the
        // same summary on the `System.exit` arm, and both arms reach this
        // process. Whichever gets here first prints; see `GC_SUMMARY_PRINTED`.
        if claim_gc_summary_print() {
            // gen r5w2/obs6: see the other summary site.
            cratonvm_vm::record_vm_allocation_totals(
                vm.shared
                    .mem
                    .bytes_allocated_total
                    .load(std::sync::atomic::Ordering::Relaxed),
            );
            vm.shared.mem.heap.print_gc_summary();
            // zr z7/rs: what the pre-`main` mimalloc hook did.
            eprintln!("{}", mimalloc_tuning::summary_line());
            // zr z10/mt: big zeroed blocks outside the arena (Linux).
            eprintln!("{}", sparse_block_alloc::summary_line());
            // gcf f4/o: the allocation-failure doors, every collector.
            eprintln!(
                "{}",
                cratonvm_vm::runtime::interpreter::oome_ladder_doors_line(&vm.shared)
            );
            // zr z3/lt: ZGC boxing-helper OOME rung (ZGC only).
            if let Some(line) = cratonvm_vm::runtime::interpreter::zgc_jit_box_line(&vm.shared) {
                eprintln!("{line}");
            }
            // zr z3/ja: stamped / dropped-stale collection requests, every collector.
            if let Some(line) =
                cratonvm_vm::runtime::interpreter::zgc_stale_gc_requests_line(&vm.shared)
            {
                eprintln!("{line}");
            }
            // zr z5/cr: ZGC allocation-refusal collections and the futile rule.
            if let Some(line) = cratonvm_vm::runtime::interpreter::zgc_refusal_line(&vm.shared) {
                eprintln!("{line}");
            }
            // zt t8/rv4: the ZGC metaspace back-fill retry passes.
            if let Some(line) =
                cratonvm_vm::runtime::interpreter::zgc_metaspace_backfill_line(&vm.shared)
            {
                eprintln!("{line}");
            }
            // zt t20/x16: the ZGC GC-notification census (D10).
            if let Some(line) =
                cratonvm_vm::runtime::interpreter::zgc_gc_notifications_line(&vm.shared)
            {
                eprintln!("{line}");
            }
            // The retained class-layout ledger, every collector.
            eprintln!(
                "{}",
                cratonvm_vm::runtime::interpreter::unload_layouts_line(&vm.shared)
            );
            // gcg g1/p: compiled-`new` helper entries -- printed after this
            // block (JIT r21 wave 2, lane alloc).
            // Time-to-safepoint; see `ttsp_exit_line`.
            eprintln!("{}", ttsp_exit_line(&vm.shared.mem.gc_barrier.ttsp_stats()));
            // gci i6/jr: the release -> resume census of the barrier's waiters.
            eprintln!("{}", vm.shared.mem.gc_barrier.resume_census_line());
        }
    }

    // gcg g1/p: compiled-`new` helper entries, under `CRATONVM_DBG_JIT_METHOD_STATS`
    // alone. JIT r21 wave 2 (lane alloc): this line used to be printed only
    // inside the GC-stats block above, so `CRATONVM_DBG_JIT_METHOD_STATS=1`
    // without `CRATONVM_GC_STATS=1` / `--verbose:gc` printed nothing. The
    // per-VM claim (`take_compiled_new_paths_line_for`) keeps the
    // `System.exit` arm's copy from printing it a second time.
    if let Some(line) =
        cratonvm_vm::jit::alloc_class_cache::take_compiled_new_paths_line_for(&vm.shared)
    {
        eprintln!("{line}");
    }

    // Per-segment `getstatic` cost, plus whether the lock-free statics index is
    // actually being hit (`CRATONVM_DBG_GETSTATIC_PROF=1`).
    cratonvm_vm::jit::helpers::gs_prof::dump();

    // gch h3/jx: the JNI phase census on the normal-return arm, while `run()` still owns the
    // VM; `maybe_dump_shutdown_reports` asks again for the `System.exit` arm (once per VM).
    if let Some(line) = vm.shared.natives.jni_phase_census.take_exit_line() {
        eprintln!("{line}");
    }

    // gc-common w12-e: how `main` ended decides the rest of the teardown, and
    // `result` is consumed by the render below, so take both answers now.
    // `main` threw a Java exception: HotSpot's `JavaMain` still reaches
    // `DestroyJavaVM`, which waits for the non-daemon threads.
    // An `InternalError` is a CratonVM failure with no HotSpot equivalent;
    // it keeps the historical shape (no wait).
    let main_ended = MainEnding::of(&result);

    // RENDER FIRST (gc-common w12-e). The uncaught throwable is turned into
    // its `Exception in thread "main"` text HERE, before the non-daemon wait
    // and before the shutdown hooks, which is the order HotSpot uses (its
    // `LEAVE()` dispatches the uncaught exception before `DestroyJavaVM`).
    // Two consequences:
    //
    // * the throwable is read at the address `main` left it at. Since the
    //   render used to run after the hooks, it was parked in
    //   `uncaught_exception_pending` (rooted and remapped) and re-read after
    //   them (`bug-h2-testopenclose-throwable-is-java-lang-object-20260829`).
    //   Nothing after the render needs the object, so nothing is parked now;
    // * the result is a `String`, which the non-daemon wait below can print
    //   before it blocks, the way HotSpot does.
    //
    // `outcome` is what `run()` returns. Since nio21 wave 8 (lane L4) an
    // uncaught exception's report is PRINTED here and `outcome` carries only
    // `UncaughtReported`, which the launcher's error arm prints nothing for
    // (HotSpot's stderr holds the report alone, no CratonVM prefix); an
    // `InternalError` is still a CratonVM failure the launcher prints with its
    // prefix after the hooks.
    let outcome: Result<()> = match result {
        Ok(_) => Ok(()),
        Err(MethodCallFailed::InternalError(e)) => {
            Err(anyhow::anyhow!("Error in thread \"main\" {e}"))
        }
        Err(MethodCallFailed::ExceptionThrown(mut exc_ref)) => 'uncaught: {
            // zs s2/nd2: `CRATONVM_LAUNCHER_PREMAIN_EXCEPTION_DESCRIBE=0` sends
            // a pre-`main` throwable to the uncaught dispatch below instead.
            if !entered_main && premain_exception_describe() {
                vm.describe_launcher_exception(exc_ref);
                break 'uncaught Err(UncaughtReported("launcher exception described".into()).into());
            }

            // gcf f4/r (`gcf-f2r-main-thread-uncaught-exception-ignores-the-
            // default-handler`): HotSpot's launcher detaches the main thread,
            // whose `JavaThread::exit` runs `Thread.dispatchUncaughtException`:
            // a per-thread handler, a `ThreadGroup` override or the default
            // handler runs, and only without one does `ThreadGroup` print the
            // trace. Measured on 25.0.3: with a handler stderr holds nothing,
            // the hooks still run, rc 1. Without one this runs nothing and the
            // render below prints what `ThreadGroup.uncaughtException` would.
            //
            // nio21 wave 8 lane L4: with no handler installed the JDK chain runs
            // too (`ThreadGroup.uncaughtException` prints the report through
            // `System.err`), as it does for every other thread; see
            // `uncaught_main_via_thread_group`. Either way the outcome is
            // `UncaughtReported`: the launcher prints nothing more, rc 1.
            if vm.shared.config.java_home.is_some() {
                // HotSpot's `System.out` has already written an unterminated
                // `print` by now (autoflush); ours may still hold it in the
                // fd-1 line writer, which would put it after the report in a
                // merged `2>&1` capture.
                let _ = std::io::Write::flush(&mut std::io::stdout());
                let (moved, dispatched) =
                    vm.dispatch_main_uncaught_exception(exc_ref, uncaught_main_via_thread_group());
                exc_ref = moved;
                if dispatched != cratonvm_vm::vm::MainUncaughtDispatch::NoHandler {
                    break 'uncaught Err(UncaughtReported(format!("{dispatched:?}")).into());
                }
            }
            // Try to read exception class name and message, plus the cause chain
            // so users can see the underlying reason for wrapper exceptions like
            // ExceptionInInitializerError or InvocationTargetException.
            //
            // Also render the captured Java-side stack trace from the
            // `stackTrace` field on each Throwable when present. NOTE: in the
            // current cratonvm, `Throwable.fillInStackTrace` (see
            // `native-builtins/src/lang_misc.rs`) only stashes frames into the
            // VM-wide Throwable trace registry keyed by identity
            // hash — it does NOT populate the heap-side `stackTrace` /
            // `backtrace` field. The Java code only writes that field lazily
            // when something calls `Throwable.getStackTrace()`. For unhandled
            // exceptions that escape `main()`, that has typically never
            // happened, so the renderer below will usually find a null array
            // and emit no `\tat ...` lines. Promoting the synthetic capture
            // to populate the heap field (or wiring this CLI to read from
            // `throwable_stacks` directly) is roadmap item T2.2.18 — see
            // `roadmap-100.md` line 471.
            //
            // INTENTIONAL (reviewed): omitting the `\tat ...` frames here is an
            // acceptable, honest degradation — NOT a wrong-result stub. The
            // renderer prints the real exception class, message, and the full
            // `Caused by:` cause chain (all read from live heap fields); only
            // the per-frame stack-trace lines are absent when the heap-side
            // `stackTrace` array was never materialised. We never fabricate
            // synthetic frames, so what is printed is always faithful; the
            // missing frames are a known limitation tracked by T2.2.18, not a
            // silent incorrect value.
            //
            // gcf f2/r: FIRST, the report HotSpot prints -- the throwable's own
            // `printStackTrace` (module prefixes, `Suppressed:`, the whole
            // cause chain). The Rust walk below is the fallback for a VM that
            // cannot run it (no JDK, the call failing) or for
            // `CRATONVM_UNCAUGHT_RUST_RENDER=1`.
            // nio21 wave 9 lane L4: the thread's current name and the
            // platform separator, as `ThreadGroup.uncaughtException` and
            // `printStackTrace(System.err)` would write them.
            let (thread_name, line_sep) = {
                let mut ctx = cratonvm_vm::vm::NativeContextImpl {
                    shared: &vm.shared,
                    thread: &mut vm.main_thread,
                };
                (
                    uncaught_thread_name(&mut ctx),
                    uncaught_line_separator(&ctx),
                )
            };
            let printed = if vm.shared.config.java_home.is_some()
                && uncaught_report_runs_print_stack_trace()
            {
                let mut ctx = cratonvm_vm::vm::NativeContextImpl {
                    shared: &vm.shared,
                    thread: &mut vm.main_thread,
                };
                let (moved, printed) = uncaught_report_via_print_stack_trace(&mut ctx, exc_ref);
                exc_ref = moved;
                printed
            } else {
                None
            };
            let rust_render = printed.is_none();
            // JIT round 23 wave 1 L11 (proposal w8-L4 item 3): over a JDK the
            // Rust walk below is retired. It is reached there only when the JDK
            // chain (`ThreadGroup.uncaughtException`, default ON) or the
            // `printStackTrace(PrintWriter)` render could not run, and it then
            // printed a report HotSpot never prints (`[cratonvm-cli]` lines,
            // a field-read cause chain). HotSpot's launcher answers that case
            // with `ExceptionDescribe`: the prefix, then the throwable's own
            // `printStackTrace()`, anything that throws discarded
            // (`Vm::describe_launcher_exception`). The walk stays for a VM with
            // no JDK and under `CRATONVM_UNCAUGHT_RUST_RENDER=1`;
            // `CRATONVM_UNCAUGHT_JDK_DESCRIBE_FALLBACK=0` restores it here.
            if rust_render
                && vm.shared.config.java_home.is_some()
                && uncaught_report_runs_print_stack_trace()
            {
                // Names the thread by its Java `name` field, as
                // `ExceptionDescribe` does (a `Thread.setName` in `main`).
                vm.describe_launcher_exception(exc_ref);
                break 'uncaught Err(UncaughtReported("described as ExceptionDescribe".into()).into());
            }
            let mut cur = exc_ref;
            let mut lines: Vec<String> = printed.unwrap_or_default();
            let mut prefix = format!("Exception in thread \"{thread_name}\"");
            // The previous throwable's `\tat` lines: a `Caused by:` elides the
            // frames it shares with them (gcf f1/r).
            let mut enclosing_frames: Option<Vec<String>> = None;
            // Cap the cause-chain walk so a self-referential or pathologically
            // deep chain can't loop forever. When the cap is hit with an
            // unrendered cause still pending, a marker line is emitted (see the
            // `next_cause` handling at the end of the loop) so deeply-wrapped
            // exceptions are not silently truncated.
            const MAX_CAUSE_DEPTH: usize = 8;
            let cause_walk_bound = if rust_render { MAX_CAUSE_DEPTH } else { 0 };
            for depth in 0..cause_walk_bound {
                let cid = vm.shared.mem.heap.class_id_of(cur);
                // PERF: resolve the class name AND the Throwable field indices
                // under a single read guard. These were two back-to-back
                // `class_manager.read()` calls; both are pure reads with no
                // intervening work, so one guard is behavior-identical and
                // avoids a redundant lock/unlock per cause-chain iteration.
                //
                // Find fields by name so we work regardless of layout.
                // Also probe `target` (used by InvocationTargetException
                // in lieu of Throwable.cause — see its `getCause()` override)
                // so that `Caused by:` chains still walk through the wrapper.
                let (cname, msg_idx, cause_idx, stack_idx, target_idx) = {
                    let cm = vm.shared.classes.class_manager.read();
                    let cname = cm
                        .get_class(cid)
                        .map(|c| c.name.to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    let mut msg_i: Option<usize> = None;
                    let mut cause_i: Option<usize> = None;
                    let mut stack_i: Option<usize> = None;
                    let mut target_i: Option<usize> = None;
                    // Walk from Throwable down
                    let mut walk = Some(cid);
                    while let Some(k) = walk {
                        if let Some(cls) = cm.get_class(k) {
                            let mut inst = 0usize;
                            for f in &cls.fields {
                                if !f.is_static() {
                                    let abs = cls.first_field_index + inst;
                                    if &*f.name == "detailMessage" && msg_i.is_none() {
                                        msg_i = Some(abs);
                                    }
                                    if &*f.name == "cause" && cause_i.is_none() {
                                        cause_i = Some(abs);
                                    }
                                    if &*f.name == "stackTrace" && stack_i.is_none() {
                                        stack_i = Some(abs);
                                    }
                                    if &*f.name == "target" && target_i.is_none() {
                                        target_i = Some(abs);
                                    }
                                    inst += 1;
                                }
                            }
                            walk = cls.superclass;
                        } else {
                            break;
                        }
                    }
                    (cname, msg_i, cause_i, stack_i, target_i)
                };
                let message = if let Some(i) = msg_idx {
                    let v = vm.shared.mem.heap.get_field(cur, i);
                    if let Value::Object(Some(s)) = v {
                        cratonvm_vm::vm::read_java_string(&vm.shared.mem.heap, s)
                            .unwrap_or_default()
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };
                // An implicit NPE's JEP 358 text lives in `extendedMessage`,
                // where `NullPointerException.getMessage()` finds it
                // (interpreter round i4 wave 99, lane L9).
                let message = if message.is_empty() {
                    cratonvm_vm::runtime::exceptions::npe_computed_extended_message(
                        &vm.shared, cur,
                    )
                    .and_then(|s| cratonvm_vm::vm::read_java_string(&vm.shared.mem.heap, s))
                    .unwrap_or(message)
                } else {
                    message
                };
                // HotSpot prints `Throwable.toString()`, i.e. the BINARY name
                // (`java.lang.IllegalStateException`); `cname` is the internal
                // name and stays so for the by-name checks below.
                let shown_name = binary_class_name(&cname);
                let line = if message.is_empty() {
                    format!("{prefix} {shown_name}")
                } else {
                    format!("{prefix} {shown_name}: {message}")
                };
                lines.push(line);

                // Render `\tat ...` frames from `stackTrace[]` if populated.
                // Each element is a `StackTraceElement` with fields, in
                // declaration order: declaringClass (String), methodName
                // (String), fileName (String, nullable), lineNumber (int).
                // We resolve those four field indices the same way as the
                // Throwable fields above so we work regardless of layout.
                let mut emitted_frames = false;
                // This throwable's `\tat` lines, collected first so a cause can
                // elide the frames it shares with the throwable that encloses
                // it, as `Throwable.printEnclosedStackTrace` does.
                let mut frame_lines: Vec<String> = Vec::new();
                if let Some(si) = stack_idx {
                    let stack_val = vm.shared.mem.heap.get_field(cur, si);
                    if let Value::Object(Some(arr)) = stack_val {
                        let len = vm.shared.mem.heap.array_length(arr);
                        if len > 0 {
                            emitted_frames = true;
                            // Resolve StackTraceElement field indices once
                            // from the first non-null element's class.
                            let mut ste_idx: Option<(usize, usize, usize, usize)> = None;
                            for i in 0..len {
                                let elem =
                                    vm.shared.mem.heap.get_array_element(arr, i).ok().and_then(
                                        |v| {
                                            if let Value::Object(Some(o)) = v {
                                                Some(o)
                                            } else {
                                                None
                                            }
                                        },
                                    );
                                let Some(elem_ref) = elem else { continue };
                                if ste_idx.is_none() {
                                    let ecid = vm.shared.mem.heap.class_id_of(elem_ref);
                                    let cm = vm.shared.classes.class_manager.read();
                                    let mut dc: Option<usize> = None;
                                    let mut mn: Option<usize> = None;
                                    let mut fn_: Option<usize> = None;
                                    let mut ln: Option<usize> = None;
                                    let mut walk = Some(ecid);
                                    while let Some(k) = walk {
                                        if let Some(cls) = cm.get_class(k) {
                                            let mut inst = 0usize;
                                            for f in &cls.fields {
                                                if !f.is_static() {
                                                    let abs = cls.first_field_index + inst;
                                                    match &*f.name {
                                                        "declaringClass" if dc.is_none() => {
                                                            dc = Some(abs)
                                                        }
                                                        "methodName" if mn.is_none() => {
                                                            mn = Some(abs)
                                                        }
                                                        "fileName" if fn_.is_none() => {
                                                            fn_ = Some(abs)
                                                        }
                                                        "lineNumber" if ln.is_none() => {
                                                            ln = Some(abs)
                                                        }
                                                        _ => {}
                                                    }
                                                    inst += 1;
                                                }
                                            }
                                            walk = cls.superclass;
                                        } else {
                                            break;
                                        }
                                    }
                                    if let (Some(a), Some(b), Some(c), Some(d)) = (dc, mn, fn_, ln)
                                    {
                                        ste_idx = Some((a, b, c, d));
                                    }
                                }
                                let Some((dc, mn, fn_, ln)) = ste_idx else {
                                    continue;
                                };
                                let read_str = |idx: usize| -> Option<String> {
                                    match vm.shared.mem.heap.get_field(elem_ref, idx) {
                                        Value::Object(Some(s)) => {
                                            cratonvm_vm::vm::read_java_string(
                                                &vm.shared.mem.heap,
                                                s,
                                            )
                                        }
                                        _ => None,
                                    }
                                };
                                let class_name =
                                    read_str(dc).unwrap_or_else(|| "<unknown>".to_string());
                                let method_name =
                                    read_str(mn).unwrap_or_else(|| "<unknown>".to_string());
                                let file_name = read_str(fn_);
                                let line_no = match vm.shared.mem.heap.get_field(elem_ref, ln) {
                                    Value::Int(i) => i,
                                    _ => -1,
                                };
                                // HotSpot format:
                                //   \tat <class>.<method>(<file>:<line>)
                                // If fileName is null/empty, use "Unknown Source".
                                // If lineNumber < 0, omit ":<line>".
                                let location = match (file_name.as_deref(), line_no) {
                                    (Some(f), n) if !f.is_empty() && n >= 0 => format!("{f}:{n}"),
                                    (Some(f), _) if !f.is_empty() => f.to_string(),
                                    _ => "Unknown Source".to_string(),
                                };
                                frame_lines
                                    .push(format!("\tat {class_name}.{method_name}({location})"));
                            }
                        }
                    }
                }

                // Fallback: when `Throwable.stackTrace[]` was never populated
                // (the array is null or empty — the typical case for an
                // exception that escapes `main()` without anyone calling
                // `getStackTrace()`), pull frames from the VM-wide registry
                // keyed by identity hash
                // — that's where `Throwable.fillInStackTrace` actually
                // stashes the captured frames in this VM. See
                // `vm/src/vm/vm_init.rs::Vm::throwable_stack_for`.
                if !emitted_frames {
                    if let Some(frames) = vm.throwable_stack_for(cur) {
                        // The registry is OUTERMOST-first (`main` at index 0,
                        // see `throwable_stack_trace_entry_for`); HotSpot's
                        // report is innermost-first, the throw site on top
                        // (gce-e2f-uncaught-exception-report-lists-frames-
                        // outermost-first).
                        frame_lines = uncaught_report_frame_lines(&frames);
                    }
                }
                // HotSpot prints a cause's frames up to the ones it shares with
                // its enclosing trace, then `\t... N more` (gcf f1/r).
                let shown = frames_not_in_common(&frame_lines, enclosing_frames.as_deref());
                lines.extend(frame_lines[..shown].iter().cloned());
                if shown < frame_lines.len() {
                    lines.push(format!("\t... {} more", frame_lines.len() - shown));
                }
                enclosing_frames = Some(frame_lines);

                // Follow cause. Throwable.cause is the canonical chain link,
                // but InvocationTargetException stores the wrapped exception
                // in its own `target` field and its `getCause()` override
                // returns that — so the heap-level `cause` is null/self while
                // the real cause lives in `target`. Probe both.
                let mut next_cause = {
                    let mut next = None;
                    if let Some(i) = cause_idx {
                        if let Value::Object(Some(c)) = vm.shared.mem.heap.get_field(cur, i) {
                            if c != cur {
                                next = Some(c);
                            }
                        }
                    }
                    if next.is_none() {
                        if let Some(i) = target_idx {
                            if let Value::Object(Some(t)) = vm.shared.mem.heap.get_field(cur, i) {
                                if t != cur {
                                    next = Some(t);
                                }
                            }
                        }
                    }
                    next
                };
                if next_cause.is_none() && cname == "java/lang/reflect/InvocationTargetException" {
                    let ite_decl = vm
                        .shared
                        .classes
                        .class_manager
                        .read()
                        .get_loaded_class_id("java/lang/reflect/InvocationTargetException");
                    // gc-common w12-e: the calls below run Java, which can
                    // collect (other threads are still running: the render now
                    // precedes the non-daemon wait). `cur` and `exc_ref` are
                    // Rust locals read again afterwards (`t != cur`, the next
                    // call's receiver, and `throwable_stack_for(exc_ref)` after
                    // the loop), so pin both in the thread's native pin roots,
                    // which every collection scans and remaps, and re-read them
                    // after each call.
                    let pin_base = vm.main_thread.native_pin_roots.len();
                    vm.main_thread.native_pin_roots.push(exc_ref);
                    vm.main_thread.native_pin_roots.push(cur);
                    for (meth, desc) in [
                        ("getTargetException", "()Ljava/lang/Throwable;"),
                        ("getCause", "()Ljava/lang/Throwable;"),
                    ] {
                        if next_cause.is_some() {
                            break;
                        }
                        let invoke_res = if let Some(ite_cid) = ite_decl {
                            invoke_on_class_shared_no_retarget(
                                &vm.shared,
                                &mut vm.main_thread,
                                ite_cid,
                                meth,
                                desc,
                                &[Value::Object(Some(cur))],
                            )
                        } else {
                            invoke_on_class_shared(
                                &vm.shared,
                                &mut vm.main_thread,
                                cid,
                                meth,
                                desc,
                                &[Value::Object(Some(cur))],
                            )
                        };
                        if let Some(&moved) = vm.main_thread.native_pin_roots.get(pin_base) {
                            exc_ref = moved;
                        }
                        if let Some(&moved) = vm.main_thread.native_pin_roots.get(pin_base + 1) {
                            cur = moved;
                        }
                        match invoke_res {
                            Ok(Some(Value::Object(Some(t)))) if t != cur => {
                                next_cause = Some(t);
                            }
                            Ok(_) => {}
                            Err(e) => {
                                lines.push(format!(
                                    "[cratonvm-cli] InvocationTargetException.{meth}() failed: {e:?}"
                                ));
                            }
                        }
                    }
                    vm.main_thread.native_pin_roots.truncate(pin_base);
                }
                // When walking through PropertyBatchUpdateException, also
                // surface the nested PropertyAccessExceptions array contents.
                // The standard `getMessage()` joins their messages with ";",
                // but Spring's BeanCreationException wrapper inlines that
                // joined string before any per-sub-cause framing is rendered,
                // so the actual offending property (and the IAE/NPE thrown by
                // the setter) is lost in plain `Caused by:` chains.  Drill one
                // level so each sub-cause's class + message + cause chain is
                // visible — this is the only stable surface for diagnosing
                // setter-injection failures because PBUE itself doesn't
                // `initCause` the first sub-exception.
                if cname == "org/springframework/beans/PropertyBatchUpdateException" {
                    // Find the propertyAccessExceptions field by name.
                    let arr_idx = {
                        let cm = vm.shared.classes.class_manager.read();
                        let mut found: Option<usize> = None;
                        let mut walk = Some(cid);
                        while let Some(k) = walk {
                            if let Some(cls) = cm.get_class(k) {
                                let mut inst = 0usize;
                                for f in &cls.fields {
                                    if !f.is_static() {
                                        let abs = cls.first_field_index + inst;
                                        if &*f.name == "propertyAccessExceptions" && found.is_none()
                                        {
                                            found = Some(abs);
                                        }
                                        inst += 1;
                                    }
                                }
                                walk = cls.superclass;
                            } else {
                                break;
                            }
                        }
                        found
                    };
                    if let Some(ai) = arr_idx {
                        match vm.shared.mem.heap.get_field(cur, ai) {
                            Value::Object(Some(arr)) => {
                                let n = vm.shared.mem.heap.array_length(arr);
                                lines.push(format!(
                                    "[cratonvm-cli] PropertyBatchUpdateException.propertyAccessExceptions length={n}"
                                ));
                                for i in 0..n {
                                    let elem = vm.shared.mem.heap.get_array_element(arr, i).ok();
                                    if let Some(Value::Object(Some(eref))) = elem {
                                        let ecid = vm.shared.mem.heap.class_id_of(eref);
                                        // PERF: one read guard for the sub-exception class
                                        // name and its field indices (back-to-back reads).
                                        // Read detailMessage and cause from this sub-exception
                                        let (ename, smsg, scause, spname) = {
                                            let cm = vm.shared.classes.class_manager.read();
                                            let ename = cm
                                                .get_class(ecid)
                                                .map(|c| c.name.to_string())
                                                .unwrap_or_else(|| "?".to_string());
                                            let mut mi: Option<usize> = None;
                                            let mut ci: Option<usize> = None;
                                            let mut pn: Option<usize> = None;
                                            let mut walk = Some(ecid);
                                            while let Some(k) = walk {
                                                if let Some(cls) = cm.get_class(k) {
                                                    let mut inst = 0usize;
                                                    for f in &cls.fields {
                                                        if !f.is_static() {
                                                            let abs = cls.first_field_index + inst;
                                                            match &*f.name {
                                                                "detailMessage" if mi.is_none() => {
                                                                    mi = Some(abs)
                                                                }
                                                                "cause" if ci.is_none() => {
                                                                    ci = Some(abs)
                                                                }
                                                                "propertyName" if pn.is_none() => {
                                                                    pn = Some(abs)
                                                                }
                                                                _ => {}
                                                            }
                                                            inst += 1;
                                                        }
                                                    }
                                                    walk = cls.superclass;
                                                } else {
                                                    break;
                                                }
                                            }
                                            let read_s = |idx: Option<usize>| -> String {
                                                idx.and_then(|i| {
                                                    match vm.shared.mem.heap.get_field(eref, i) {
                                                        Value::Object(Some(s)) => {
                                                            cratonvm_vm::vm::read_java_string(
                                                                &vm.shared.mem.heap,
                                                                s,
                                                            )
                                                        }
                                                        _ => None,
                                                    }
                                                })
                                                .unwrap_or_default()
                                            };
                                            (ename, read_s(mi), ci, read_s(pn))
                                        };
                                        lines.push(format!(
                                            "[cratonvm-cli]   [{i}] {ename} property='{spname}' message={smsg:?}"
                                        ));
                                        if let Some(frames) = vm.throwable_stack_for(eref) {
                                            if !frames.is_empty() {
                                                lines.push(format!(
                                                    "[cratonvm-cli]       ({} captured frames)",
                                                    frames.len()
                                                ));
                                                for frame in frames.iter().take(12) {
                                                    let loc =
                                                        match (frame.file.as_deref(), frame.line) {
                                                            (Some(f), n)
                                                                if !f.is_empty() && n >= 0 =>
                                                            {
                                                                format!("{f}:{n}")
                                                            }
                                                            (Some(f), _) if !f.is_empty() => {
                                                                f.to_string()
                                                            }
                                                            _ => "Unknown Source".to_string(),
                                                        };
                                                    lines.push(format!(
                                                        "\t\tat {}.{}({})",
                                                        frame.class, frame.method, loc
                                                    ));
                                                }
                                            }
                                        }
                                        // Follow cause(s) for this sub-exception (one level deep,
                                        // up to 6 deep just in case).
                                        let mut sub_cur = scause.and_then(|ci| {
                                            if let Value::Object(Some(c)) =
                                                vm.shared.mem.heap.get_field(eref, ci)
                                            {
                                                if c != eref {
                                                    Some(c)
                                                } else {
                                                    None
                                                }
                                            } else {
                                                None
                                            }
                                        });
                                        for _d in 0..6 {
                                            let Some(sc) = sub_cur else { break };
                                            let sc_cid = vm.shared.mem.heap.class_id_of(sc);
                                            // PERF: one read guard for the sub-cause class
                                            // name and its field indices (back-to-back reads).
                                            let (sc_name, sc_msg, sc_cause_idx) = {
                                                let cm = vm.shared.classes.class_manager.read();
                                                let sc_name = cm
                                                    .get_class(sc_cid)
                                                    .map(|c| c.name.to_string())
                                                    .unwrap_or_else(|| "?".to_string());
                                                let mut mi: Option<usize> = None;
                                                let mut ci: Option<usize> = None;
                                                let mut walk = Some(sc_cid);
                                                while let Some(k) = walk {
                                                    if let Some(cls) = cm.get_class(k) {
                                                        let mut inst = 0usize;
                                                        for f in &cls.fields {
                                                            if !f.is_static() {
                                                                let abs =
                                                                    cls.first_field_index + inst;
                                                                match &*f.name {
                                                                    "detailMessage"
                                                                        if mi.is_none() =>
                                                                    {
                                                                        mi = Some(abs)
                                                                    }
                                                                    "cause" if ci.is_none() => {
                                                                        ci = Some(abs)
                                                                    }
                                                                    _ => {}
                                                                }
                                                                inst += 1;
                                                            }
                                                        }
                                                        walk = cls.superclass;
                                                    } else {
                                                        break;
                                                    }
                                                }
                                                let m = mi
                                                    .and_then(|i| {
                                                        match vm.shared.mem.heap.get_field(sc, i) {
                                                            Value::Object(Some(s)) => {
                                                                cratonvm_vm::vm::read_java_string(
                                                                    &vm.shared.mem.heap,
                                                                    s,
                                                                )
                                                            }
                                                            _ => None,
                                                        }
                                                    })
                                                    .unwrap_or_default();
                                                (sc_name, m, ci)
                                            };
                                            lines.push(format!("[cratonvm-cli]       Caused by: {sc_name}: {sc_msg}"));
                                            if let Some(frames) = vm.throwable_stack_for(sc) {
                                                if !frames.is_empty() {
                                                    lines.push(format!("[cratonvm-cli]         ({} captured frames)", frames.len()));
                                                    for frame in frames.iter().take(16) {
                                                        let loc = match (
                                                            frame.file.as_deref(),
                                                            frame.line,
                                                        ) {
                                                            (Some(f), n)
                                                                if !f.is_empty() && n >= 0 =>
                                                            {
                                                                format!("{f}:{n}")
                                                            }
                                                            (Some(f), _) if !f.is_empty() => {
                                                                f.to_string()
                                                            }
                                                            _ => "Unknown Source".to_string(),
                                                        };
                                                        lines.push(format!(
                                                            "\t\t\tat {}.{}({})",
                                                            frame.class, frame.method, loc
                                                        ));
                                                    }
                                                }
                                            }
                                            sub_cur = sc_cause_idx.and_then(|ci| {
                                                if let Value::Object(Some(c)) =
                                                    vm.shared.mem.heap.get_field(sc, ci)
                                                {
                                                    if c != sc {
                                                        Some(c)
                                                    } else {
                                                        None
                                                    }
                                                } else {
                                                    None
                                                }
                                            });
                                        }
                                    } else {
                                        lines.push(format!("[cratonvm-cli]   [{i}] <null>"));
                                    }
                                }
                            }
                            Value::Object(None) => {
                                lines.push("[cratonvm-cli] PropertyBatchUpdateException.propertyAccessExceptions = null".into());
                            }
                            _ => {}
                        }
                    } else {
                        lines.push("[cratonvm-cli] PropertyBatchUpdateException.propertyAccessExceptions field NOT FOUND on class".into());
                    }
                }
                if let Some(c) = next_cause {
                    // If this is the last iteration the cap allows, the cause
                    // `c` would never be rendered — emit a marker so deeply
                    // wrapped exceptions are not silently cut off.
                    if depth + 1 >= MAX_CAUSE_DEPTH {
                        lines.push("\t... (deeper causes truncated)".to_string());
                        break;
                    }
                    cur = c;
                    prefix = "Caused by:".to_string();
                    continue;
                }
                break;
            }
            // The two CratonVM diagnostics below exist for the Rust walk's blind
            // spots; a report the throwable printed itself is HotSpot's text and
            // gets nothing appended (gcf f2/r).
            let had_caused_by = lines.iter().any(|l| l.starts_with("Caused by:"));
            if rust_render
                && !had_caused_by
                && lines
                    .iter()
                    .any(|l| l.contains("java.lang.reflect.InvocationTargetException"))
            {
                if let Some(frames) = vm.throwable_stack_for(exc_ref) {
                    if !frames.is_empty() {
                        lines.push(
                            "[cratonvm-cli] Throwable stack (fillInStackTrace) for InvocationTargetException:"
                                .to_string(),
                        );
                        let mut innermost_first = uncaught_report_frame_lines(&frames);
                        innermost_first.truncate(24);
                        lines.extend(innermost_first);
                    }
                }
            }
            // Missing-stack-trace diagnostic (2026-05-21).
            //
            // When an exception escapes `main()` and NEITHER the heap-side
            // `Throwable.stackTrace[]` NOR the VM-wide retained trace
            // capture produced a single `\tat ...` frame, the bare
            // `Exception in thread "main" <class>: <msg>` line is useless
            // for diagnosis — exactly the keycloak26 `NullPointerException:
            // charset` symptom. Rather than exit silently, emit an explicit
            // marker so the failure mode is unambiguous, retry the
            // identity-hash trace lookup for the head exception, and point
            // the operator at the live-stack diagnostic env var.
            let emitted_any_frame = lines.iter().any(|l| l.starts_with("\tat "));
            if rust_render && !emitted_any_frame {
                lines.push(
                    "[cratonvm-cli] (no Java stack frames were captured for this exception)"
                        .to_string(),
                );
                // Last-ditch: dump whatever the head exception's
                // retained trace entry holds, even if the cause-chain
                // walk above skipped it.
                match vm.throwable_stack_for(exc_ref) {
                    Some(frames) if !frames.is_empty() => {
                        lines.push(format!(
                            "[cratonvm-cli] recovered {} captured frame(s) from the trace store:",
                            frames.len()
                        ));
                        let mut innermost_first = uncaught_report_frame_lines(&frames);
                        innermost_first.truncate(40);
                        lines.extend(innermost_first);
                    }
                    _ => {
                        lines.push(
                            "[cratonvm-cli] the retained trace store has no entry for this \
                             throwable either — the exception was likely thrown on a \
                             non-main thread, or its constructor was shadowed by a native \
                             that skipped fillInStackTrace."
                                .to_string(),
                        );
                        lines.push(
                            "[cratonvm-cli] re-run with CRATONVM_DBG_CHARSET=1 to dump the \
                             full live Java thread stack at throw time (NullPointerException: \
                             charset), or CRATONVM_DBG_ATHROW=1 for every exception throw."
                                .to_string(),
                        );
                    }
                }
            }
            // nio21 wave 8 lane L4: printed HERE, unprefixed, the way
            // `ThreadGroup.uncaughtException` prints it -- before the
            // non-daemon wait and the shutdown hooks (HotSpot's `LEAVE()`
            // order), and exactly once. It used to travel to the launcher as
            // the `Err` text and come out as `[cratonvm] main-vm run()
            // returned Err: Exception in thread "main" ...` after the hooks
            // (and a second time when a non-daemon thread was alive).
            {
                use std::io::Write as _;
                let _ = std::io::stdout().flush();
                let mut err = std::io::stderr().lock();
                let _ = write!(err, "{}{line_sep}", lines.join(line_sep.as_str()));
                let _ = err.flush();
            }
            Err(UncaughtReported("rendered by the launcher".to_string()).into())
        }
    };

    // T19.K1 — wait for non-daemon threads before exiting.
    //
    // Per the JVM specification, the VM keeps running until every
    // non-daemon thread has terminated. Daemon threads (GC workers,
    // event loops, finalisers) are best-effort: when the last
    // non-daemon thread finishes, the VM exits, abandoning any
    // remaining daemons.
    //
    // For HelloWorld and any program that doesn't `Thread.start()`
    // a user thread, `wait_for_non_daemon_threads` returns
    // immediately (the snapshot is empty). For Quarkus / Keycloak,
    // the embedded HTTP listener and Vert.x worker pool are
    // non-daemon; this loop blocks until they exit.
    //
    // gc-common w12-e: ALSO when `main` threw
    // (`common-w11f-uncaught-exception-exit-abandons-non-daemon-threads`).
    // This used to wait only when `main` returned, on the theory that waiting
    // "after a fatal error would just delay the stack trace". HotSpot does
    // not treat it as fatal: `JavaMain` runs `LEAVE()`, which prints the
    // uncaught trace and then calls `DestroyJavaVM`, and `destroy_vm` waits
    // until the calling thread is the last non-daemon thread. Only then do
    // the hooks run and the process exit with 1. Skipping the wait killed a
    // worker pool (or a server's listener) that `main` had started before it
    // threw. The trace is printed first, here, only when there IS something
    // to wait for, so a program with no live non-daemon thread prints exactly
    // what it did before. `InternalError` keeps the historical no-wait shape
    // (see `MainEnding`).
    //
    // The deadline parameter is `None` (wait indefinitely): a
    // legitimate non-daemon thread could be a long-running service
    // and bounding the wait would surprise users. CI runs that need
    // to bound execution can use the existing
    // `--stack-dump-on-timeout` watchdog which `abort()`s the
    // process from a separate thread — this loop will be
    // interrupted by the watchdog's `process::abort()` call. A harness
    // whose `main` throws while a non-daemon thread never ends now hangs
    // here, exactly as it does on HotSpot.
    // Interpreter round i5 wave 101, lane L6
    // (`CRATONVM_MAIN_THREAD_ENDS_BEFORE_DESTROY`, default on): HotSpot's
    // `LEAVE()` detaches the main thread BEFORE `DestroyJavaVM` waits, so
    // `mainThread.join()` in a non-daemon thread returns and `isAlive()` reads
    // false during the wait. The uncaught exception was dispatched above.
    if main_ended.waits_for_non_daemon_threads() {
        vm.end_main_thread_java_side();
    }
    if main_ended.waits_for_non_daemon_threads() {
        // T19.K1 — diagnostic only when the wait is actually
        // observable (i.e. there ARE non-daemon threads). HelloWorld
        // and any program that doesn't `Thread.start()` a user
        // thread skips this message and exits silently. Long-running
        // apps (Quarkus, Keycloak, embedded Jetty) print one line so
        // the user can tell the wait is what's holding the process
        // alive — useful when a CI run mysteriously sits at "main
        // returned" forever.
        let pending = vm
            .shared
            .threads
            .thread_registry
            .alive_non_daemon_thread_ids()
            .len();
        // Round 15 wave 6 (lane sweepC, `r15w5-threadvm2-review-findings`
        // R3): HotSpot prints nothing while it waits, and since pool workers
        // keep their own daemon flag every `submit(task); shutdown();`
        // program whose task outlives `main` printed the notice. It goes to
        // `tracing` now; `CRATONVM_EXIT_WAIT_NOTICE=1` restores the stderr
        // line.
        let notice_on_stderr =
            pending > 0 && false;
        // nio21 wave 8 lane L4: an uncaught exception's report is already out
        // (the handler, `ThreadGroup.uncaughtException` or the launcher's
        // render printed it above, before this wait, as HotSpot does), so
        // neither arm prints it again. Both used to, and the launcher's error
        // arm then printed it a second time after the hooks.
        if pending > 0 && !notice_on_stderr {
            tracing::info!(
                "cratonvm: main() {}; VM held alive by {pending} non-daemon thread(s)",
                main_ended.verb()
            );
        }
        if notice_on_stderr {
            if main_ended == MainEnding::Threw {
                eprintln!(
                    "[cratonvm] main() threw; VM held alive by {pending} \
                     non-daemon thread(s) before exiting with code 1 (JVM-spec \
                     behaviour). Send SIGINT/SIGTERM, use System.exit(), or pass \
                     --stack-dump-on-timeout to bound execution."
                );
                let _ = std::io::Write::flush(&mut std::io::stderr());
            } else {
                eprintln!(
                    "[cratonvm] main() returned; VM held alive by {pending} \
                     non-daemon thread(s) (JVM-spec behaviour). Send SIGINT/\
                     SIGTERM, use System.exit(), or pass --stack-dump-on-timeout \
                     to bound execution."
                );
            }
        }
        // GC-barrier fix: this thread never runs Java bytecode again once it
        // reaches this wait (it is parked in a raw `pthread_join` loop, not
        // Java-level parking), so it can never cooperatively reach an
        // interpreter safepoint and call `arrive_and_wait`. Publish the main
        // thread's roots and enter the full per-thread blocked-region protocol
        // for the whole wait, mirroring native socket/pipe waits.
        vm.begin_main_thread_blocking_region("vm-main:wait-non-daemon");
        let joined = vm
            .shared
            .threads
            .thread_registry
            .wait_for_non_daemon_threads(None);
        vm.end_main_thread_blocking_region();
        if joined > 0 {
            tracing::info!(
                "cratonvm: joined {joined} non-daemon thread(s) after main() {}",
                main_ended.verb()
            );
        }
    }

    // W7-92 — the launcher's two exit paths: `main` returned, and `main` threw.
    // HotSpot runs shutdown hooks on BOTH (measured, 25.0.3+9: the `normal`,
    // `nondaemon` and `uncaught` rows of the record's oracle table), so this
    // runs for every `outcome`. `System.exit` / `Runtime.exit` never reach
    // this line; `lang_system::native_system_exit` carries the trigger for
    // those.
    //
    // AFTER `wait_for_non_daemon_threads`, and that ordering is not cosmetic:
    // HotSpot's `nondaemon` transcript prints `KEEPER-DONE` BEFORE the hook
    // output. Shutdown does not begin until the last non-daemon thread ends,
    // and since gc-common w12-e that holds on the uncaught arm too.
    //
    // Called directly rather than through `vm.invoke("java/lang/Shutdown",
    // "runHooks", "()V", &[])`. The Java route would depend on
    // `java/lang/Shutdown` resolving, which is a real-JDK-mode assumption —
    // and a bridge that silently does nothing in synthetic-JDK mode is the
    // exact failure shape this record is about. The native registration on
    // that triple still exists for JDK-side callers; both land on the same
    // drained list, so neither can double-run the hooks.
    //
    // Ordering on the uncaught-exception path: HotSpot prints the stack trace
    // and THEN runs the hooks. Since nio21 wave 8 (lane L4) the report is
    // printed where `outcome` is computed, before the wait and before this
    // call, whether or not a non-daemon thread is alive, so the order matches
    // (W7-92 §7's follow-up half; gc-common w12-e closed the other half).
    {
        let mut ctx = cratonvm_vm::vm::NativeContextImpl {
            shared: &vm.shared,
            thread: &mut vm.main_thread,
        };
        let trigger = if main_ended == MainEnding::Returned {
            "main-returned"
        } else {
            "uncaught"
        };
        cratonvm_native_builtins::lang_system::run_shutdown_hooks(&mut ctx, trigger);
    }
    // gc-common w11-f: `dumponexit` on these two arms too, after the hooks
    // (HotSpot's dump is itself a shutdown hook). See
    // `dump_jfr_recording_on_exit`.
    dump_jfr_recording_on_exit(&vm.shared);
    // gcf f2/r: HotSpot's `before_exit` prints the `gc,exit` heap summary after
    // the hooks; the `System.exit` arm prints it from
    // `maybe_dump_shutdown_reports`. Claimed once inside the logger.
    // zt t9/tg2: HotSpot's `before_exit` stops the ZGC director here.
    // zt t11/tg4: and waits for a tick pause the stop came too late for.
    stop_zgc_director_tick_and_wait(&mut vm);
    // gci i14/hd + zr z9/hp: the collector's own lines (Serial, G1, ZGC).
    cratonvm_vm::runtime::unified_logging::log_gc_exit_summary(|| {
        cratonvm_vm::runtime::unified_logging::gc_exit_heap_lines(&vm.shared)
    });
    // gcj j1/ob: then (Generational) the `safepoint,stats` exit summary
    // (HotSpot prints it after the VM thread's terminating `Halt`).
    cratonvm_vm::runtime::unified_logging::log_safepoint_stats_exit_summary(
        vm.shared.mem.heap.is_generational(),
    );
    // gcj j1/st item 2: then `-XX:+PrintStringTableStatistics`' block.
    cratonvm_vm::print_string_table_statistics_at_exit(&vm.shared);
    // An attached debugger's `VM_DEATH` (interpreter round i1 wave 24): the
    // `System.exit` arm reports it from the pre-exit hook.
    cratonvm_vm::report_debugger_vm_death(&vm.shared);
    // The jvmstat perf-data file (`CRATONVM_PERF_DATA_FILE`; the `System.exit`
    // arm removes it from the pre-exit hook). Interpreter round i3 wave 81.
    cratonvm_vm::remove_perf_data_file_at_exit(&vm.shared);
    // The member-access census total (`CRATONVM_DBG=access`; the `System.exit`
    // arm prints it from the pre-exit hook). Interpreter round i1 wave 27.
    cratonvm_vm::report_member_access_census_at_exit(&vm.shared);
    // The unrecorded-wait-kind census (`CRATONVM_DBG_UNRECORDED_WAIT_KINDS`;
    // the `System.exit` arm prints it from the pre-exit hook). Interpreter
    // round i2 wave 66, lane L1.
    cratonvm_vm::report_unrecorded_wait_regions_at_exit(&vm.shared);
    // The inlined-chain census and the eager-chains soak verdict
    // (`CRATONVM_DBG_DEOPT`; the `System.exit` arm prints it from the pre-exit hook).
    cratonvm_vm::runtime::interpreter::report_inlined_chain_census_at_exit(&vm.shared);
    // The poll-stress forced-arrival census (JIT round 21 lane fuzzvm,
    // FUZZTOOL21-1; the `System.exit` arm prints it from the pre-exit hook).
    cratonvm_vm::jit::helpers::report_poll_stress_census_at_exit(&vm.shared);

    // Per-VM diagnostic state must be read while this VM still owns it.
    maybe_dump_shutdown_reports_for(Some(&vm.shared));
    outcome
}

