use super::*;

fn src(pairs: &[(&str, &str)]) -> MapSource {
    MapSource::new(pairs.iter().copied())
}

/// One probe must still tell "declared and unset" from "not declared": the
/// first is served from the snapshot, the second falls through to the
/// override table and the real environment.

/// The boolean readers answer the same words as before the allocation-free
/// rewrite, whatever the case or padding.
#[test]
fn boolean_words_ignore_ascii_case_and_padding() {
    for off in ["0", "false", "FALSE", " Off ", "no", "No"] {
        assert!(value_is_off(off), "{off:?} is an off word");
        assert!(!word_is_on(off), "{off:?} is not on");
    }
    for on in ["1", "true", "yes", "on", "maybe"] {
        assert!(!value_is_off(on), "{on:?} is not an off word");
        assert!(word_is_on(on), "{on:?} is on");
    }
    assert!(!value_is_off(""), "empty is ON for an opt-out switch");
    assert!(!word_is_on(""), "and OFF for an opt-in one");
}

/// `runtime_flag_on` is a boolean, not a presence test: the off words are
/// off, and it agrees with `parse::truthy_word` on every one of them.
#[test]
fn runtime_flag_on_reads_the_off_words_as_off() {
    let _lock = process_override_lock();
    // Undeclared, so the per-thread override path serves it without any
    // write to `environ`.
    const NAME: &str = "cratonvm_undeclared_runtime_flag_on_probe";
    assert!(
        std::env::var_os(NAME).is_none(),
        "fixture name must not exist in the real environment"
    );
    assert!(!runtime_flag_on(NAME), "unset must read as off");
    with_thread_overrides(&[(NAME, None)], || {
        assert!(
            !runtime_flag_on(NAME),
            "an override to None must read as off"
        );
    });

    for (value, want) in [
        ("", false),
        ("0", false),
        ("false", false),
        ("OFF", false),
        (" no ", false),
        ("1", true),
        ("true", true),
        ("yes", true),
        ("anything", true),
    ] {
        with_thread_overrides(&[(NAME, Some(value))], || {
            assert_eq!(
                runtime_flag_on(NAME),
                want,
                "runtime_flag_on with {NAME}={value:?}"
            );
        });
        assert_eq!(
            parse::truthy_word(&src(&[(NAME, value)]), NAME),
            want,
            "parse::truthy_word with {NAME}={value:?} must agree"
        );
    }
}

/// An UNDECLARED name is overridable, so a test never has to write to
/// `environ` to arrange one.
///
/// This is the whole point of `VmFlags::undeclared_edits`.
/// `std::env::set_var` is sound only in a single-threaded program — glibc's
/// `setenv` may reallocate and free the `environ` array while another
/// thread is inside `getenv` — and `native-builtins`' `--lib` was doing it
/// from three test modules while ~4,130 tests ran on parallel threads. The
/// override serves the same reads with no write at all.
#[test]
fn an_undeclared_name_is_overridable_without_writing_to_environ() {
    let _lock = process_override_lock();
    // Deliberately not a `CRATONVM_*` name: this must exercise the
    // undeclared path, and the declared path already has its own tests.
    const NAME: &str = "cratonvm_undeclared_override_probe";
    assert!(
        std::env::var_os(NAME).is_none(),
        "fixture name must not exist in the real environment"
    );

    with_thread_overrides(&[(NAME, Some("set-by-override"))], || {
        assert_eq!(runtime_var(NAME).ok().as_deref(), Some("set-by-override"));
        assert_eq!(
            runtime_var_os(NAME),
            Some(OsString::from("set-by-override"))
        );
    });

    // The override wrote nothing, so the process environment is untouched
    // and the read outside it is the real (absent) one.
    assert!(std::env::var_os(NAME).is_none());
    assert!(runtime_var_os(NAME).is_none());

    // The other direction: an existing variable overridden to "as if
    // unset". `PATH` is undeclared and always present.
    assert!(std::env::var_os("PATH").is_some(), "PATH must be set");
    with_thread_overrides(&[("PATH", None)], || {
        assert!(
            runtime_var_os("PATH").is_none(),
            "an override to None must read as unset"
        );
        assert!(matches!(
            runtime_var("PATH"),
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
        ));
    });
    assert!(runtime_var_os("PATH").is_some(), "and it comes back after");
}

/// `synthetic_memoryusage_tostring` is served by the snapshot, and reads
/// the same three truth words its siblings do.
///
/// `native-builtins`' `memoryusage_tostring_shim_enabled` read this name
/// with a raw `std::env::var` and a bespoke `Ok("1") | Ok("true")` table
/// when it landed, so the value the VM acted on came from the live process
/// environment — invisible to `with_thread_overrides`, unreachable from
/// `CRATONVM_REAL=-memoryusage-tostring`, and silently the developer's
/// ambient environment in any test that tried to arrange it. This pins the
/// parse; `flag_groups::tests::the_20260811_declarations_expand_from_their_group_spelling`
/// pins the grouped spelling that feeds it.
#[test]
fn synthetic_memoryusage_tostring_is_snapshot_backed_and_reads_one_true_yes() {
    assert!(
        !VmFlags::from_source(&MapSource::empty())
            .natives
            .synthetic_memoryusage_tostring,
        "the real JDK bytecode is the default",
    );
    for word in ["1", "true", "yes"] {
        assert!(
            VmFlags::from_source(&src(&[("CRATONVM_SYNTHETIC_MEMORYUSAGE_TOSTRING", word)]))
                .natives
                .synthetic_memoryusage_tostring,
            "`{word}` must turn the shim back on",
        );
    }
    for word in ["0", "false", "no", ""] {
        assert!(
            !VmFlags::from_source(&src(&[("CRATONVM_SYNTHETIC_MEMORYUSAGE_TOSTRING", word)]))
                .natives
                .synthetic_memoryusage_tostring,
            "`{word}` must leave the default alone",
        );
    }
}

#[test]
fn undeclared_process_values_keep_live_standard_environment_semantics() {
    let key = if cfg!(windows) { "PATH" } else { "HOME" };
    assert_eq!(runtime_var_os(key), std::env::var_os(key));
    assert_eq!(runtime_var(key), std::env::var(key));
}

#[test]
fn empty_source_matches_all_documented_defaults() {
    let f = VmFlags::from_source(&MapSource::empty());
    // Every opt-in flag is off…
    // `moving_young` is no longer an opt-in. Pin the shipped default
    // directly so an accidental reversion cannot hide behind the constant.
    //
    // If you are here because this assertion failed, read
    // `DEFAULT_MOVING_YOUNG`'s doc comment before changing it: this flag is
    // read by fixes that have nothing to do with compaction, and flipping
    // it has twice disarmed one of them silently, in both directions,
    // without failing any other test. A green suite is not evidence that a
    // flip was safe — the last one shipped with the whole tomcat,
    // hibernate, gc, jit and HotSpot-differential sweep green and still
    // re-opened a class-unloading defect that had been fixed the day
    // before.
    assert!(DEFAULT_MOVING_YOUNG);
    assert!(f.gc.moving_young);
    assert!(!f.gc.card_table_only);
    assert!(!f.gc.dbg_a2);
    assert!(!f.jit.shadow_stack);
    assert!(!f.jit.method_stats);
    // …and every default-ON flag is on.
    assert!(f.gc.old_sweep_jit);
    assert!(f.io.real_net_sockets);
    assert!(f.natives.real_forkjoinpool);
    // Value flags fall back.
    assert_eq!(f.gc.g1_workers, None);
    assert_eq!(f.gc.gc_stress_bytes, None);
    assert_eq!(f.gc.dbg_stale_objref_cycles, 1);
    assert_eq!(f.gc.dbg_watch_cell, 0);
    assert_eq!(f.gc.dbg_blocked_access, BlockedAccessMode::Off);
    assert!(!f.jit.disable_jit);
}

#[test]
fn concurrency_and_socket_legacy_surfaces_require_explicit_opt_out() {
    let f = VmFlags::from_source(&src(&[
        ("CRATONVM_SYNTHETIC_FORKJOINPOOL", "1"),
        ("CRATONVM_SYNTHETIC_NET_SOCKETS", "1"),
    ]));
    assert!(!f.natives.real_forkjoinpool);
    assert!(!f.io.real_net_sockets);

    let f = VmFlags::from_source(&src(&[
        ("CRATONVM_SYNTHETIC_FORKJOINPOOL", "1"),
        ("CRATONVM_REAL_FORKJOINPOOL", "1"),
        ("CRATONVM_SYNTHETIC_NET_SOCKETS", "1"),
        ("CRATONVM_REAL_NET_SOCKETS", "1"),
    ]));
    assert!(f.natives.real_forkjoinpool);
    assert!(f.io.real_net_sockets);
}

#[test]
fn gc_par_threads_keeps_zero_because_zero_disables_parallelism() {
    // `CRATONVM_GC_PAR_THREADS=0` means "no parallel young GC". It is
    // therefore `usize_opt`, NOT `usize_min1` like its neighbour
    // `CRATONVM_G1_WORKERS` — clamping it to >= 1 would silently turn the
    // documented kill-switch into "one worker", and the caller
    // (`young_mark::young_gc_threads`) already does its own `.max(1)`.
    let f = VmFlags::from_source(&src(&[("CRATONVM_GC_PAR_THREADS", "0")]));
    assert_eq!(f.gc.gc_par_threads, Some(0));
    // The lookalike really is clamped — the two must not be unified.
    let g = VmFlags::from_source(&src(&[("CRATONVM_G1_WORKERS", "0")]));
    assert_eq!(g.gc.g1_workers, Some(1));
}

#[test]
fn gc_sweep_anchor_stride_ignores_values_below_64() {
    // Sub-64-byte strides would mint an anchor inside almost every object;
    // the pre-config code filtered them out and fell back to the default.
    let lo = VmFlags::from_source(&src(&[("CRATONVM_GC_SWEEP_ANCHOR_STRIDE", "32")]));
    assert_eq!(lo.gc.gc_sweep_anchor_stride, 8 * 1024 * 1024);
    let ok = VmFlags::from_source(&src(&[("CRATONVM_GC_SWEEP_ANCHOR_STRIDE", "64")]));
    assert_eq!(ok.gc.gc_sweep_anchor_stride, 64);
    let junk = VmFlags::from_source(&src(&[("CRATONVM_GC_SWEEP_ANCHOR_STRIDE", "nope")]));
    assert_eq!(junk.gc.gc_sweep_anchor_stride, 8 * 1024 * 1024);
}

/// gen r4w2/obs: the `[gcpause]` threshold defaults to the historical
/// 100 ms (so the default output is byte-identical), accepts `0`, and a
/// junk value falls back to the default rather than to "print everything".

#[test]
fn gc_parallel_value_flags_default_as_documented() {
    let f = VmFlags::from_source(&MapSource::empty());
    assert_eq!(f.gc.gc_sweep_anchor_stride, 8 * 1024 * 1024);
    assert_eq!(f.gc.gc_par_threads, None);
    assert_eq!(f.gc.gc_par_min_bytes, 16 * 1024 * 1024);
}

/// The moving-young gate is an opt-OUT over [`DEFAULT_MOVING_YOUNG`], not
/// an opt-in, and the opt-out wins over the compatibility opt-in.
///
/// There is deliberately no second flag to also satisfy: the removed
/// `CRATONVM_ALLOW_MOVING_YOUNG` had to be set *in addition* to
/// `CRATONVM_MOVING_YOUNG` before `gen_heap` would run a moving cycle under
/// a live JIT frame, so the documented way to enable the feature could
/// never actually enable it in the case it exists for.
/// [`runtime_flag_default_on`] honours every spelling of "off", and the
/// literal-`0` parser it replaces did not.
///
/// The two JIT kill switches this helper was added for
/// (`CRATONVM_JIT_LICM`, `CRATONVM_JIT_UNROLL`) were
/// `map_or(true, |v| v != "0")`, so `=false` and `=off` left the pass
/// running. An operator bisecting a suspected miscompile with the spelling
/// they would naturally try got "no effect" from a switch that never
/// stopped the pass.
#[test]
fn runtime_flag_default_on_accepts_every_off_spelling() {
    const NAME: &str = "CVMSELFTEST_DEFAULT_ON_PROBE";

    with_thread_overrides(&[(NAME, None)], || {
        assert!(
            runtime_flag_default_on(NAME),
            "unset means ON — that is what 'default on' is"
        );
    });

    for off in ["0", "false", "off", "no", "FALSE", "Off", "NO", "  false  "] {
        with_thread_overrides(&[(NAME, Some(off))], || {
            assert!(
                !runtime_flag_default_on(NAME),
                "{off:?} must turn a default-ON switch off; the parser this \
                     replaced honoured only the literal \"0\""
            );
        });
    }

    for on in ["1", "true", "yes", "on", "anything", ""] {
        with_thread_overrides(&[(NAME, Some(on))], || {
            assert!(
                runtime_flag_default_on(NAME),
                "{on:?} must leave a default-ON switch on"
            );
        });
    }
}

/// gcj j3/x2: [`runtime_flag_default_on_if`] is the default-ON rule when
/// scoped and the opt-in rule otherwise, for every spelling -- unset,
/// off words, on words and the empty value (ON scoped, OFF unscoped).
#[test]
fn gcj3x2_runtime_flag_default_on_if_is_default_on_only_when_scoped() {
    const NAME: &str = "CVMSELFTEST_DEFAULT_ON_IF_PROBE";
    with_thread_overrides(&[(NAME, None)], || {
        assert!(runtime_flag_default_on_if(NAME, true), "unset, scoped: ON");
        assert!(
            !runtime_flag_default_on_if(NAME, false),
            "unset, unscoped: OFF"
        );
    });
    for value in ["0", "false", "off", "no", "1", "true", "yes", "", "banana"] {
        with_thread_overrides(&[(NAME, Some(value))], || {
            assert_eq!(
                runtime_flag_default_on_if(NAME, true),
                runtime_flag_default_on(NAME),
                "{value:?}: scoped"
            );
            assert_eq!(
                runtime_flag_default_on_if(NAME, false),
                runtime_flag_on(NAME),
                "{value:?}: unscoped"
            );
        });
    }
}

/// The new helper can only ever turn a switch OFF where the parser it
/// replaces left it on — never the reverse.
///
/// This is the migration-safety property: adopting
/// [`runtime_flag_default_on`] at a site spelled `map_or(true, |v| v != "0")`
/// cannot enable something that was disabled.
#[test]
fn runtime_flag_default_on_only_widens_the_off_set() {
    const NAME: &str = "CVMSELFTEST_DEFAULT_ON_PROBE2";
    for value in [
        "0",
        "false",
        "off",
        "no",
        "FALSE",
        "Off",
        "NO",
        "  false  ",
        "1",
        "true",
        "yes",
        "on",
        "",
        "banana",
        "00",
    ] {
        with_thread_overrides(&[(NAME, Some(value))], || {
            let legacy = value != "0";
            let now = runtime_flag_default_on(NAME);
            assert!(
                !(now && !legacy),
                "{value:?}: the new parser says ON where `!= \"0\"` said OFF — \
                     that is the one direction this migration must never take"
            );
        });
    }
}

/// gen r4w2/oldgen2: the old-gen trigger hysteresis is a POLICY change, so
/// it is opt-in — unset and `=0` both leave the historical trigger.
#[test]
fn old_trigger_hysteresis_is_opt_in_and_zero_is_off() {
    let on = |v: &str| {
        VmFlags::from_source(&src(&[("CRATONVM_GC_OLD_TRIGGER_HYSTERESIS", v)]))
            .gc
            .old_trigger_hysteresis
    };
    assert!(!VmFlags::from_source(&src(&[])).gc.old_trigger_hysteresis);
    assert!(!on("0"));
    assert!(!on(""));
    assert!(on("1"));
}

/// gen r4w4/concmark4: the hysteresis flag is read twice — the legacy
/// explicitly-on bool (unchanged), and a tri-state the concurrent-first
/// policy defaults. `=0` must be an explicit OFF, not "unset".
#[test]
fn old_trigger_hysteresis_explicit_is_a_tri_state() {
    let explicit = |v: &str| {
        VmFlags::from_source(&src(&[("CRATONVM_GC_OLD_TRIGGER_HYSTERESIS", v)]))
            .gc
            .old_trigger_hysteresis_explicit
    };
    assert_eq!(
        VmFlags::from_source(&src(&[]))
            .gc
            .old_trigger_hysteresis_explicit,
        None
    );
    assert_eq!(explicit("0"), Some(false));
    assert_eq!(explicit(""), Some(false));
    assert_eq!(explicit("1"), Some(true));
}

/// gen r4w4/concmark4: concurrent-first is the default and its switch is an
/// opt-out; the fixed start percentage is off unless a positive number is
/// given; the two concurrent-marker switches are opt-outs.
#[test]
fn the_concurrent_first_old_gen_policy_switches_parse_as_documented() {
    let f = |pairs: &[(&str, &str)]| VmFlags::from_source(&src(pairs));
    let d = f(&[]);
    assert!(!d.gc.old_no_concurrent_first);
    assert_eq!(d.gc.conc_start_percent, None);
    assert!(!d.gc.gen_conc_mark_no_local_stack);
    assert!(!d.gc.gen_conc_no_frozen_eager_scan);
    assert!(
        f(&[("CRATONVM_GC_NO_CONCURRENT_FIRST", "1")])
            .gc
            .old_no_concurrent_first
    );
    assert!(
        !f(&[("CRATONVM_GC_NO_CONCURRENT_FIRST", "0")])
            .gc
            .old_no_concurrent_first
    );
    assert_eq!(
        f(&[("CRATONVM_GC_CONC_START_PERCENT", "45")])
            .gc
            .conc_start_percent,
        Some(45)
    );
    assert_eq!(
        f(&[("CRATONVM_GC_CONC_START_PERCENT", "0")])
            .gc
            .conc_start_percent,
        None
    );
    assert_eq!(
        f(&[("CRATONVM_GC_CONC_START_PERCENT", "x")])
            .gc
            .conc_start_percent,
        None
    );
    assert!(
        f(&[("CRATONVM_GEN_CONC_MARK_NO_LOCAL_STACK", "1")])
            .gc
            .gen_conc_mark_no_local_stack
    );
    assert!(
        f(&[("CRATONVM_GEN_CONC_NO_FROZEN_EAGER_SCAN", "1")])
            .gc
            .gen_conc_no_frozen_eager_scan
    );
}

/// gen r4w5/concmark5: the concurrent-cycle service thread and the remark
/// reference-processing hook are both opt-in, and `=0` is off.
#[test]
fn the_concurrent_service_and_remark_hook_switches_are_opt_in() {
    let f = |pairs: &[(&str, &str)]| VmFlags::from_source(&src(pairs));
    let d = f(&[]);
    assert!(!d.gc.gen_conc_service_thread);
    // gce ve2: default on since 2026-09-29 (`=0` kills).
    assert!(d.gc.gen_conc_remark_refproc_hook);
    assert!(
        f(&[("CRATONVM_GEN_CONC_SERVICE_THREAD", "1")])
            .gc
            .gen_conc_service_thread
    );
    assert!(
        !f(&[("CRATONVM_GEN_CONC_SERVICE_THREAD", "0")])
            .gc
            .gen_conc_service_thread
    );
    assert!(
        f(&[("CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK", "1")])
            .gc
            .gen_conc_remark_refproc_hook
    );
    assert!(
        !f(&[("CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK", "0")])
            .gc
            .gen_conc_remark_refproc_hook
    );
}

/// gen r5w1/refs5: the SATB young-value filter and the HotSpot soft-ref
/// LRU inputs are policy/perf changes, so both are opt-in and `=0` is off.
///
/// gen r5w2/conc6: the allocation-failure door's concurrent start
/// (`CRATONVM_GEN_CONC_ALLOC_FAIL_DOOR`) was asserted opt-in here too. Its
/// default is now ON — without it the default concurrent-first old-gen
/// policy never ran a cycle in compiled code
/// (`gengc-r5w1-refs5-concurrent-cycle-never-starts-from-compiled-code`)
/// — so the assertion is now "on unless `=0`".
/// gen r4w3/oldgen3: the in-place old-gen give-back is a POLICY change, so
/// it is opt-in — unset and `=0` both leave every page where it is.
#[test]
fn old_give_back_is_opt_in_and_zero_is_off() {
    let on = |v: &str| {
        VmFlags::from_source(&src(&[("CRATONVM_GC_OLD_GIVE_BACK", v)]))
            .gc
            .old_give_back
    };
    assert!(!VmFlags::from_source(&src(&[])).gc.old_give_back);
    assert!(!on("0"));
    assert!(!on(""));
    assert!(on("1"));
}

/// gen r4w4/oldgen4: the old-gen shrink, the walk-gap recovery and the
/// refusal-armed compaction are default-ON with `=0` as the kill switch;
/// the PROACTIVE fragmentation trigger is opt-in.

/// gen r4w5/oldcompact5: the compaction around pinned objects on the
/// non-moving path is opt-in, and `=0` is off.
#[test]
fn old_gen_pinned_compaction_is_opt_in() {
    let flags = |k: &str, v: &str| VmFlags::from_source(&src(&[(k, v)])).gc;
    assert!(!VmFlags::from_source(&src(&[])).gc.old_pinned_compact);
    assert!(flags("CRATONVM_GC_OLD_PINNED_COMPACT", "1").old_pinned_compact);
    assert!(!flags("CRATONVM_GC_OLD_PINNED_COMPACT", "0").old_pinned_compact);
}
/// gen r4w6/review6: thrash5's two kill switches are typed fields, default
/// ON, off for the spellings the wave-5 readers honoured (`0`, `false`,
/// `off`), and on for anything else.
/// gcd d2/j: the futile-young backoff is default ON with `=0` its only
/// off spelling; the per-message preallocated OOMEs are default ON since
/// gcf 2026-10-01 (vf2 o2-6: `k1` = `k0` = HotSpot), `=0` kills them.
/// gen r4w6/oldpin6: the resize after a concurrent cycle's sweep is
/// opt-in, and `=0` is off.
#[test]
fn old_gen_shrink_after_a_concurrent_sweep_is_opt_in() {
    let flags = |k: &str, v: &str| VmFlags::from_source(&src(&[(k, v)])).gc;
    assert!(
        !VmFlags::from_source(&src(&[]))
            .gc
            .old_shrink_after_concurrent
    );
    assert!(flags("CRATONVM_GC_OLD_SHRINK_AFTER_CONCURRENT", "1").old_shrink_after_concurrent);
    assert!(!flags("CRATONVM_GC_OLD_SHRINK_AFTER_CONCURRENT", "0").old_shrink_after_concurrent);
    assert!(!VmFlags::from_source(&src(&[])).gc.conc_walk_gap_recovery);
    assert!(flags("CRATONVM_GC_CONC_WALK_GAP_RECOVERY", "1").conc_walk_gap_recovery);
    assert!(!flags("CRATONVM_GC_CONC_WALK_GAP_RECOVERY", "0").conc_walk_gap_recovery);
}

/// gen r5w3/oldgen7: the old generation's interior decommit, its
/// borrowing of the young budget and the top placement of humongous
/// arrays are all opt-in, and `=0` is off.
///
/// gen r5w4/defaults8: the `-Xms` usable-first split was opt-in here too;
/// it is default ON now (`alloc_policy_defaults::GEN_XMS_USABLE_FIRST`, on
/// `GenR5W3XmsTotalProbe`'s evidence), so the assertion is "on unless an
/// off word".
#[test]
fn old_gen_sizing_switches_of_gen_r5w3_are_opt_in() {
    let flags = |k: &str, v: &str| VmFlags::from_source(&src(&[(k, v)])).gc;
    let unset = VmFlags::from_source(&src(&[])).gc;
    assert!(unset.gen_xms_usable_first);
    assert!(!flags("CRATONVM_GEN_XMS_USABLE_FIRST", "false").gen_xms_usable_first);
    assert!(!unset.old_interior_decommit);
    assert!(unset.old_borrow_young);
    // Default ON since the gcd triage (2026-09-28); `=0` restores the best fit.
    assert!(unset.old_humongous_top);
    assert!(flags("CRATONVM_GC_OLD_HUMONGOUS_TOP", "1").old_humongous_top);
    assert!(!flags("CRATONVM_GC_OLD_HUMONGOUS_TOP", "0").old_humongous_top);
    assert!(!flags("CRATONVM_GC_OLD_HUMONGOUS_TOP", "off").old_humongous_top);
    assert!(flags("CRATONVM_GEN_XMS_USABLE_FIRST", "1").gen_xms_usable_first);
    assert!(!flags("CRATONVM_GEN_XMS_USABLE_FIRST", "0").gen_xms_usable_first);
    assert!(flags("CRATONVM_GC_OLD_INTERIOR_DECOMMIT", "1").old_interior_decommit);
    assert!(!flags("CRATONVM_GC_OLD_INTERIOR_DECOMMIT", "0").old_interior_decommit);
    assert!(flags("CRATONVM_GC_OLD_BORROW_YOUNG", "1").old_borrow_young);
    assert!(!flags("CRATONVM_GC_OLD_BORROW_YOUNG", "0").old_borrow_young);
}

#[test]
fn g1_workers_clamps_to_one() {
    assert_eq!(
        VmFlags::from_source(&src(&[("CRATONVM_G1_WORKERS", "0")]))
            .gc
            .g1_workers,
        Some(1)
    );
    assert_eq!(
        VmFlags::from_source(&src(&[("CRATONVM_G1_WORKERS", " 6 ")]))
            .gc
            .g1_workers,
        Some(6)
    );
    assert_eq!(
        VmFlags::from_source(&src(&[("CRATONVM_G1_WORKERS", "many")]))
            .gc
            .g1_workers,
        None
    );
}

#[test]
fn watch_cell_accepts_hex_with_or_without_prefix() {
    assert_eq!(
        VmFlags::from_source(&src(&[("CRATONVM_DBG_WATCH_CELL", "0x7fff00")]))
            .gc
            .dbg_watch_cell,
        0x7fff00
    );
    assert_eq!(
        VmFlags::from_source(&src(&[("CRATONVM_DBG_WATCH_CELL", "7fff00")]))
            .gc
            .dbg_watch_cell,
        0x7fff00
    );
    assert_eq!(
        VmFlags::from_source(&src(&[("CRATONVM_DBG_WATCH_CELL", "zzz")]))
            .gc
            .dbg_watch_cell,
        0
    );
}

#[test]
fn stale_objref_cycles_defaults_and_floors_at_one() {
    assert_eq!(
        VmFlags::from_source(&src(&[("CRATONVM_DBG_STALE_OBJREF_CYCLES", "4")]))
            .gc
            .dbg_stale_objref_cycles,
        4
    );
    assert_eq!(
        VmFlags::from_source(&src(&[("CRATONVM_DBG_STALE_OBJREF_CYCLES", "0")]))
            .gc
            .dbg_stale_objref_cycles,
        1
    );
}

#[test]
fn overlay_source_beats_environment() {
    // No env var set for this name, so the overlay is what shows through.
    let overlay = OverlaySource::new(MapSource::empty().with("CRATONVM_CARD_TABLE_ONLY", "1"));
    assert!(VmFlags::from_source(&overlay).gc.card_table_only);
}

#[test]
fn loader_defaults_are_on_where_the_call_sites_defaulted_on() {
    let f = VmFlags::from_source(&MapSource::empty());
    assert!(f.loader.loader_aware_resolution);
    assert!(f.loader.boot_module_registry);
    assert!(!f.loader.allow_jsr_ret);
    assert!(!f.loader.harden_manifest_classpath);
}

#[test]
fn io_profile_flags_use_the_word_truth_table() {
    for (v, want) in [
        ("1", true),
        ("yes", true),
        ("0", false),
        ("off", false),
        ("FALSE", false),
        ("", false),
    ] {
        assert_eq!(
            VmFlags::from_source(&src(&[("CRATONVM_CONFINE_IO", v)]))
                .io
                .confine_io,
            want,
            "value {v:?}"
        );
    }
}

#[test]
fn synthetic_opt_ins_need_the_exact_string_one() {
    assert!(
        VmFlags::from_source(&src(&[("CRATONVM_SYNTHETIC_RAF", "1")]))
            .io
            .synthetic_raf_forced
    );
    assert!(
        !VmFlags::from_source(&src(&[("CRATONVM_SYNTHETIC_RAF", "true")]))
            .io
            .synthetic_raf_forced
    );
    assert!(
        !VmFlags::from_source(&src(&[("CRATONVM_SYNTHETIC_RAF", "01")]))
            .io
            .synthetic_raf_forced
    );
}

#[test]
fn selector_flags_reject_zero_empty_and_false() {
    for (v, want) in [
        ("1", true),
        ("  x ", true),
        ("0", false),
        ("", false),
        ("FaLsE", false),
    ] {
        assert_eq!(
            VmFlags::from_source(&src(&[("CRATONVM_DBG_SELECTOR", v)]))
                .io
                .dbg_selector,
            want,
            "value {v:?}"
        );
    }
}

#[test]
fn numeric_io_caps_reject_non_positive() {
    assert_eq!(
        VmFlags::from_source(&src(&[("CRATONVM_SELECT_MAX_BLOCK_MS", "0")]))
            .io
            .select_max_block_ms,
        None
    );
    assert_eq!(
        VmFlags::from_source(&src(&[("CRATONVM_SELECT_MAX_BLOCK_MS", "250")]))
            .io
            .select_max_block_ms,
        Some(250)
    );
    assert_eq!(
        VmFlags::from_source(&src(&[("CRATONVM_ZIP_MAX_ENTRY_BYTES", "0")]))
            .io
            .zip_max_entry_bytes,
        None
    );
}

#[test]
fn socket_capture_empty_is_none() {
    assert_eq!(
        VmFlags::from_source(&src(&[("CRATONVM_SOCKET_CAPTURE", "")]))
            .io
            .socket_capture_prefix,
        None
    );
    assert_eq!(
        VmFlags::from_source(&src(&[("CRATONVM_SOCKET_CAPTURE", "/tmp/cap")]))
            .io
            .socket_capture_prefix
            .as_deref(),
        Some("/tmp/cap")
    );
}

#[test]
fn stale_objref_cycles_does_not_trim_matching_the_original_call_site() {
    // The pre-refactor parse was `s.parse::<usize>()` with no `.trim()`,
    // so a padded value fell back to the default. Locked down so the
    // migration is byte-exact rather than merely "reasonable".
    assert_eq!(
        VmFlags::from_source(&src(&[("CRATONVM_DBG_STALE_OBJREF_CYCLES", " 4 ")]))
            .gc
            .dbg_stale_objref_cycles,
        1
    );
}

#[test]
fn boolean_parsers_disagree_exactly_as_documented() {
    // Regression lock on census §10: five parsers, five answers for "0".
    let s = src(&[("X", "0")]);
    assert!(parse::present(&s, "X"));
    assert!(!parse::non_empty_non_zero(&s, "X"));
    assert!(!parse::truthy_word(&s, "X"));
    assert!(!parse::affirmative_word(&s, "X"));
    assert!(!parse::one_or_true(&s, "X"));
    assert!(!parse::on_unless_zero(&s, "X"));

    let s = src(&[("X", "off")]);
    assert!(parse::present(&s, "X"));
    assert!(parse::non_empty_non_zero(&s, "X"));
    assert!(!parse::truthy_word(&s, "X"));
    assert!(!parse::affirmative_word(&s, "X"));
    assert!(parse::on_unless_zero(&s, "X"));
}

/// The `native-builtins` migration (T3.5b) surfaced four more parsers that
/// no existing one matched. Census §10 said the count of disagreeing truth
/// tables was a lower bound; it was. This locks the new ones apart from
/// their nearest neighbours so none of them can be quietly unified.
#[test]
fn native_builtins_parsers_add_four_more_truth_tables() {
    let empty = MapSource::empty();

    // Unset splits the two default-ON word parsers from their default-OFF
    // twins, and `one_true_yes_exact` from everything default-ON.
    assert!(!parse::truthy_word(&empty, "X"));
    assert!(parse::truthy_word_default_true(&empty, "X"));
    assert!(parse::on_unless_zero_or_false(&empty, "X"));
    assert!(parse::on_unless_off_word_cased(&empty, "X"));
    assert!(!parse::one_true_yes_exact(&empty, "X"));

    // The empty string is OFF for `truthy_word` and ON for its twin — the
    // divergence is not only in the default.
    let s = src(&[("X", "")]);
    assert!(!parse::truthy_word(&s, "X"));
    assert!(parse::truthy_word_default_true(&s, "X"));
    assert!(parse::on_unless_zero_or_false(&s, "X"));
    assert!(!parse::non_empty_non_zero_non_false(&s, "X"));

    // Mixed case is a genuine three-way split.
    let s = src(&[("X", "False")]);
    assert!(!parse::on_unless_zero_or_false(&s, "X"));
    assert!(parse::on_unless_off_word_cased(&s, "X"));
    assert!(parse::on_unless_off_word(&s, "X"));
    assert!(!parse::truthy_word_default_true(&s, "X"));

    // `OFF` splits the two `on_unless_off_word*` parsers the other way.
    let s = src(&[("X", "OFF")]);
    assert!(!parse::on_unless_off_word_cased(&s, "X"));
    assert!(parse::on_unless_off_word(&s, "X"));
    assert!(!parse::truthy_word_default_true(&s, "X"));

    // `on` is affirmative but not one-true-yes; whitespace is trimmed by
    // `affirmative_word` and not by `one_true_yes_exact`.
    let s = src(&[("X", "on")]);
    assert!(parse::affirmative_word(&s, "X"));
    assert!(!parse::one_true_yes_exact(&s, "X"));
    let s = src(&[("X", " 1 ")]);
    assert!(parse::affirmative_word(&s, "X"));
    assert!(!parse::one_true_yes_exact(&s, "X"));
    assert!(!parse::exactly_one(&s, "X"));
}

/// `VmFlags::from_env` snapshots `environ` once instead of calling `getenv`
/// per field. The values must be identical to reading through `EnvSource`.
#[test]
fn environ_snapshot_matches_per_lookup_env_source() {
    let snapshot = MapSource::from_process_env();
    for (name, _) in std::env::vars_os() {
        let Ok(name) = name.into_string() else {
            continue;
        };
        assert_eq!(
            snapshot.get(&name),
            EnvSource.get(&name),
            "snapshot disagrees with var_os for {name}"
        );
    }
    // And the whole config built either way is the same shape.
    assert_eq!(
        format!("{:?}", VmFlags::from_source(&snapshot)),
        format!("{:?}", VmFlags::from_source(&EnvSource)),
    );
}

/// Nothing in `NativeFlags` is accidentally default-ON.
#[test]
fn native_flags_defaults_match_the_original_call_sites() {
    let f = VmFlags::from_source(&MapSource::empty());
    // Opt-in debug gates are off…
    assert!(!f.natives.dbg_loader_trace);
    assert!(!f.natives.dbg_h2trace);
    assert!(!f.natives.soft_exit);
    assert!(!f.natives.diag_serviceloader);
    assert!(!f.natives.real_proxy_strict);
    // …and every gate whose call site defaulted ON still does.
    assert!(f.natives.cl_bootstrap_scoped);
    assert!(f.natives.loader_unload);
    assert!(f.natives.uri_strict_chars);
    assert!(f.natives.real_stax_factory);
    assert!(f.natives.real_proxy);
    assert!(f.natives.real_proxy_super);
    assert!(f.natives.real_annotations);
    assert!(f.natives.msc_real_start);
    assert!(f.natives.inherit_thread_ccl);
    assert!(f.natives.inherit_tl_workaround);
    assert!(f.natives.native_string_regex);
    // JIT r23 wave 2c, lane L5: `CRATONVM_REGEX_MATCHER_BYTECODE` (default
    // ON) makes the Matcher natives yield to the JDK bytecode, so the
    // effective gate is OFF by default and ON again under `=0`.
    assert!(!f.natives.native_matcher_find);
    let restored =
        VmFlags::from_source(&MapSource::new([("CRATONVM_REGEX_MATCHER_BYTECODE", "0")]));
    assert!(restored.natives.native_matcher_find);
    assert!(!f.natives.netty_queue_bridge);
    // Value flags fall back to None so the call site's default applies.
    assert_eq!(f.natives.http_max_body, None);
    assert_eq!(f.natives.async_submit_grace_ms, None);
    assert_eq!(f.natives.max_inflated_bytes, None);
}

// ── Test-support overrides ────────────────────────────────────────────
//
// The property under test in every case below is the one the whole
// mechanism exists for: the override must win *after* the process
// snapshot has already latched. Each of these deliberately touches
// `flags()` first so the `OnceLock` is initialised before the override
// is installed — reproducing the situation in which `set_var` silently
// stops working.

/// The scalar used throughout: declared (so it is served from the frozen
/// snapshot rather than `std::env`), and inert — nothing in this crate
/// changes behaviour based on it, so an override cannot perturb a
/// concurrent test even in process scope.
const PROBE: &str = "CRATONVM_MAVEN_REPO_LOCAL";

fn probe() -> Option<String> {
    runtime_var(PROBE).ok()
}

#[test]
fn thread_override_wins_after_the_snapshot_has_latched() {
    let _lock = process_override_lock();
    assert!(
        declared_flag_names().contains(PROBE),
        "probe must be declared"
    );
    let latched = probe();
    with_thread_overrides(&[(PROBE, Some("/scoped/repo"))], || {
        assert_eq!(probe().as_deref(), Some("/scoped/repo"));
    });
    assert_eq!(probe(), latched, "the guard must restore the snapshot");
}

#[test]
fn thread_override_can_make_a_set_flag_look_absent() {
    let _lock = process_override_lock();
    with_thread_overrides(&[(PROBE, Some("/present"))], || {
        assert_eq!(probe().as_deref(), Some("/present"));
        // Nested, and this time removing it entirely.
        with_thread_overrides(&[(PROBE, None)], || {
            assert_eq!(probe(), None);
        });
        assert_eq!(
            probe().as_deref(),
            Some("/present"),
            "the inner guard must restore the OUTER override, not the snapshot"
        );
    });
}

#[test]
fn thread_override_is_invisible_to_other_threads() {
    let _lock = process_override_lock();
    let outside = probe();
    with_thread_overrides(&[(PROBE, Some("/only/mine"))], || {
        assert_eq!(probe().as_deref(), Some("/only/mine"));
        let seen = std::thread::spawn(probe).join().expect("probe thread");
        assert_eq!(
            seen, outside,
            "a thread-scoped override must not leak to a spawned thread"
        );
    });
}

#[test]
fn process_override_reaches_spawned_threads() {
    // Serialised against the other process-scoped test: `override_process`
    // is explicitly documented as requiring caller serialisation.
    let _lock = process_override_lock();
    let outside = probe();
    with_process_overrides(&[(PROBE, Some("/every/thread"))], || {
        let seen = std::thread::spawn(probe).join().expect("probe thread");
        assert_eq!(seen.as_deref(), Some("/every/thread"));
    });
    assert_eq!(probe(), outside, "the guard must restore the snapshot");
}

#[test]
fn thread_scope_wins_over_process_scope() {
    let _lock = process_override_lock();
    with_process_overrides(&[(PROBE, Some("/process"))], || {
        assert_eq!(probe().as_deref(), Some("/process"));
        with_thread_overrides(&[(PROBE, Some("/thread"))], || {
            assert_eq!(probe().as_deref(), Some("/thread"));
        });
        assert_eq!(probe().as_deref(), Some("/process"));
    });
}

/// Dropping the last guard must take the fast path in [`flags`] back out
/// of circulation — otherwise every later `flags()` call in the process
/// pays for a thread-local probe forever.
#[test]
fn the_fast_path_gate_returns_to_zero() {
    let _lock = process_override_lock();
    let _thread_gate = super::TEST_OVERRIDE_GATE
        .write()
        .unwrap_or_else(|p| p.into_inner());
    assert_eq!(OVERRIDES_LIVE.load(Ordering::Relaxed), 0);
    {
        let _outer = override_process(VmFlags::from_env_with_edits(&[(PROBE, Some("a"))]));
        let _inner = override_thread(VmFlags::from_env_with_edits(&[(PROBE, Some("b"))]));
        assert_eq!(OVERRIDES_LIVE.load(Ordering::Relaxed), 2);
    }
    assert_eq!(OVERRIDES_LIVE.load(Ordering::Relaxed), 0);
}

/// An override must be a *complete* configuration, not a patch: every flag
/// the caller did not name keeps the value it would have had.
#[test]
fn an_override_only_changes_the_named_flags() {
    let _lock = process_override_lock();
    let before = format!("{:?}", flags().jit);
    with_thread_overrides(&[(PROBE, Some("/repo"))], || {
        assert_eq!(format!("{:?}", flags().jit), before);
    });
}

/// Edits land before group resolution, so overriding a grouped expression
/// reaches the legacy keys it expands to.
#[test]
fn edits_are_resolved_through_the_flag_groups() {
    let _lock = process_override_lock();
    with_thread_overrides(&[("CRATONVM_JIT", Some("threshold=7"))], || {
        assert_eq!(
            runtime_var("CRATONVM_JIT_THRESHOLD").ok().as_deref(),
            Some("7")
        );
    });
}

/// `resolve_capped_usize`, every branch.
///
/// The branch that was the DEFECT is `too_big_is_clamped_not_defaulted`:
/// `CRATONVM_NATIVE_SHADOW_SINK_CAP=200000` used to resolve to the DEFAULT
/// (4096) rather than the ceiling (65,536), so an operator asking for more
/// got an order of magnitude LESS than the rule allows, silently. The other
/// four exist so that fixing it cannot quietly change the cases that were
/// already right — a `0` cap in particular must never be honoured, because
/// it would report an empty population as a complete one.
const CAPVAR: &str = "CRATONVM_NATIVE_SHADOW_SINK_CAP";

fn cap_with(value: Option<&str>) -> usize {
    with_thread_overrides(&[(CAPVAR, value)], || {
        resolve_capped_usize(CAPVAR, "test sink", 4096, 65_536)
    })
}

#[test]
fn unset_resolves_to_the_default() {
    let _lock = process_override_lock();
    assert_eq!(cap_with(None), 4096);
}

#[test]
fn an_in_range_value_is_honoured_exactly() {
    let _lock = process_override_lock();
    assert_eq!(cap_with(Some("9000")), 9000);
    assert_eq!(
        cap_with(Some("  9000  ")),
        9000,
        "surrounding space is trimmed"
    );
    assert_eq!(
        cap_with(Some("65536")),
        65_536,
        "the ceiling itself is in range"
    );
}

#[test]
fn too_big_is_clamped_not_defaulted() {
    let _lock = process_override_lock();
    // THE BUG. This asserted 4096 for two months by construction.
    assert_eq!(
        cap_with(Some("200000")),
        65_536,
        "a value above the ceiling must CLAMP to the ceiling; falling back to \
             the default hands the operator LESS than the rule allows",
    );
}

#[test]
fn zero_and_nonsense_keep_the_default() {
    let _lock = process_override_lock();
    assert_eq!(
        cap_with(Some("0")),
        4096,
        "a cap of zero reports empty as complete"
    );
    assert_eq!(cap_with(Some("banana")), 4096);
    assert_eq!(cap_with(Some("-1")), 4096, "usize::from_str rejects a sign");
    assert_eq!(cap_with(Some("")), 4096);
    assert_eq!(cap_with(Some("   ")), 4096);
}

/// The warnings must read as ONE SENTENCE.
///
/// The first version of them shipped with 18-space gaps —
/// `ceiling of 65536;                  CLAMPED to 65536` — because a patch
/// script's Python `\`+newline joined the source lines and kept the
/// indentation INSIDE the Rust literal. `cat -A` on the patched line passed,
/// because the damage was in a string literal and not in a control
/// character. It was found by running a six-minute LTO build and reading
/// stderr. This is that check, in 0.03 s.
#[test]
fn the_warnings_read_as_one_sentence() {
    for raw in ["200000", "0", "banana"] {
        let (_, w) = capped_var_verdict(
            Some(raw),
            "CRATONVM_NATIVE_SHADOW_SINK_CAP",
            "interpreter observation sink",
            4096,
            65_536,
        );
        let w = w.expect("this input must warn");
        assert!(
            !w.contains("   "),
            "the warning for {raw:?} carries a run of 3+ spaces, so a line \
                 continuation was eaten: {w:?}",
        );
        assert!(!w.contains('\n'), "one line, not several: {w:?}");
        assert!(w.starts_with("[cratonvm] warning: "), "{w:?}");
        assert!(
            w.contains("CRATONVM_NATIVE_SHADOW_SINK_CAP"),
            "a warning that does not name the variable is not actionable: {w:?}",
        );
    }
}

/// A value that IS honoured must say nothing at all. A knob that warns on
/// its own happy path trains the reader to ignore it.
#[test]
fn an_honoured_value_is_silent() {
    for raw in [
        None,
        Some("9000"),
        Some("65536"),
        Some("  9000  "),
        Some(""),
        Some("   "),
    ] {
        let (_, w) = capped_var_verdict(raw, "X", "sink", 4096, 65_536);
        assert!(w.is_none(), "{raw:?} must not warn, got {w:?}");
    }
}

/// The verdict and the printed value never disagree: the number in the
/// message is the number returned.
#[test]
fn the_message_quotes_the_value_actually_in_force() {
    let (v, w) = capped_var_verdict(Some("200000"), "X", "sink", 4096, 65_536);
    assert_eq!(v, 65_536);
    assert!(w.unwrap().contains("CLAMPED to 65536"));
    let (v, w) = capped_var_verdict(Some("0"), "X", "sink", 4096, 65_536);
    assert_eq!(v, 4096);
    assert!(w.unwrap().contains("default 4096"));
}

/// The two sinks share ONE knob, so they must resolve it identically. This
/// pins the property the shared helper exists to guarantee: before it, the
/// same twelve lines were duplicated in `vm_exec.rs` and `jit/helpers.rs`
/// and could drift.
#[test]
fn both_sinks_resolve_one_knob_the_same_way() {
    let _lock = process_override_lock();
    for v in ["200000", "0", "9000", "banana"] {
        let a = with_thread_overrides(&[(CAPVAR, Some(v))], || {
            resolve_capped_usize(CAPVAR, "interpreter observation sink", 4096, 65_536)
        });
        let b = with_thread_overrides(&[(CAPVAR, Some(v))], || {
            resolve_capped_usize(CAPVAR, "JIT fast-path violation sink", 4096, 65_536)
        });
        assert_eq!(a, b, "the two sinks disagreed about {v}");
    }
}

fn process_override_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// gen r4w4/alloc4: every allocation-policy arm reads its default from
/// `alloc_policy_defaults` when unset, and the same words turn it on or
/// off whichever way the default points — so flipping a default is the
/// constant alone, and a control arm written `=0` is OFF on both sides of
/// a flip.
#[test]
fn every_allocation_policy_arm_parses_against_its_one_default() {
    type Pick = fn(&GcFlags) -> bool;
    // gce e2/o: `CRATONVM_TLAB_SIZE_RETIRED`, `CRATONVM_TLAB_WASTE_SHRINK`
    // and `CRATONVM_GEN_PRISTINE_CHUNKS` were removed, so the table is 7.
    let arms: [(&str, bool, Pick); 7] = [
        (
            "CRATONVM_TLAB_FILLER_SKIP_ZERO",
            alloc_policy_defaults::TLAB_FILLER_SKIP_ZERO,
            |g| g.tlab_filler_skip_zero,
        ),
        (
            "CRATONVM_TLAB_GATE_BUMP_FLOOR",
            alloc_policy_defaults::TLAB_GATE_BUMP_FLOOR,
            |g| g.tlab_gate_bump_floor,
        ),
        // gen r5w4/defaults8: `CRATONVM_TLAB_SHARE_SIZER` left this table
        // (its default is per backend now — see
        // `per_backend_switches_resolve_against_the_heap_family`) and the
        // `-Xms` split, parsed against its constant since the same wave,
        // took its slot.
        (
            "CRATONVM_GEN_XMS_USABLE_FIRST",
            alloc_policy_defaults::GEN_XMS_USABLE_FIRST,
            |g| g.gen_xms_usable_first,
        ),
        (
            "CRATONVM_GEN_ZERO_ONCE",
            alloc_policy_defaults::GEN_ZERO_ONCE,
            |g| g.gen_zero_once,
        ),
        (
            "CRATONVM_GEN_HUMONGOUS_ZERO_UNLOCKED",
            alloc_policy_defaults::GEN_HUMONGOUS_ZERO_UNLOCKED,
            |g| g.gen_humongous_zero_unlocked,
        ),
        (
            "CRATONVM_GEN_TLAB_TAIL_SINK",
            alloc_policy_defaults::GEN_TLAB_TAIL_SINK,
            |g| g.gen_tlab_tail_sink,
        ),
        (
            "CRATONVM_GEN_HUMONGOUS_REF_ZERO_UNLOCKED",
            alloc_policy_defaults::GEN_HUMONGOUS_REF_ZERO_UNLOCKED,
            |g| g.gen_humongous_ref_zero_unlocked,
        ),
    ];
    for (key, default, pick) in arms {
        let read = |v: Option<&str>| with_thread_overrides(&[(key, v)], || pick(&flags().gc));
        assert_eq!(read(None), default, "{key}: unset must read the default");
        for off in ["0", "false", "off", "no"] {
            assert!(!read(Some(off)), "{key}={off} must be OFF");
        }
        for on in ["1", "true", "on", "yes"] {
            assert!(read(Some(on)), "{key}={on} must be ON");
        }
        assert!(
            alloc_policy_switch(&src(&[]), key, true),
            "unset + default-on"
        );
        assert!(
            !alloc_policy_switch(&src(&[]), key, false),
            "unset + default-off"
        );
    }
}

/// gcf f4/r: the `[GC] policy:` line names every `alloc_policy_defaults`
/// constant, resolves the per-backend ones against the heap family, and
/// says `(set)` only for a switch the environment carries.
#[test]
fn gcf_f4r_policy_line_names_every_switch_and_its_origin() {
    let env = |key: &str| runtime_var_os(key).map(|v| v.to_string_lossy().into_owned());
    let line = |generational: bool| {
        with_thread_overrides(&[("CRATONVM_TLAB_SHARE_SIZER", Some("0"))], || {
            flags().gc.policy_line(
                if generational { "generational" } else { "g1" },
                generational,
                &env,
            )
        })
    };
    let gen = line(true);
    assert!(
        gen.starts_with("[GC] policy: backend=generational "),
        "{gen}"
    );
    assert!(gen.contains(" tlab_share_sizer=off(set)"), "{gen}");
    for constant in [
        "tlab_filler_skip_zero",
        "tlab_gate_bump_floor",
        "tlab_share_sizer",
        "gen_zero_once",
        "gen_humongous_zero_unlocked",
        "gen_tlab_tail_sink",
        "gen_humongous_ref_zero_unlocked",
        "softref_hotspot_lru",
        "gen_xms_usable_first",
    ] {
        assert!(
            gen.contains(&format!(" {constant}=")),
            "{constant} missing: {gen}"
        );
    }
    if None::<std::ffi::OsString>.is_none() {
        assert!(gen.contains(" softref_hotspot_lru=on(default)"), "{gen}");
        // jr20 wave 9 / gcE: HotSpot's LRU inputs on every backend.
        assert!(line(false).contains(" softref_hotspot_lru=on(default)"));
        // zr z1/rf: the ZGC line resolves the ZGC default.
        let zgc = flags().gc.policy_line("zgc", false, &env);
        assert!(zgc.contains(" softref_hotspot_lru=on(default)"), "{zgc}");
    }
}

/// gen r5w4/defaults8: a switch with a `BackendDefault` reads `None` when
/// unset (or empty), resolves to the Generational default on a
/// Generational heap and to the other default elsewhere, and an explicit
/// value — in any of the allocation-policy spellings — wins on both
/// families. `SOFTREF_HOTSPOT_LRU` is ON for Generational and OFF for G1
/// and ZGC; `TLAB_SHARE_SIZER`'s Generational flip was reverted at the
/// r5w4 merge and made again by gcg g6/jn (ON for Generational, OFF on G1
/// and ZGC). A G1 or ZGC run with nothing set is
/// byte-for-byte what it was (except that, since jr20 wave 9 / gcE, the
/// VM itself latches `SOFTREF_HOTSPOT_LRU`'s Generational default on every
/// backend: `vm_init.rs`).
#[test]
fn per_backend_switches_resolve_against_the_heap_family() {
    use alloc_policy_defaults::BackendDefault;
    type Pick = fn(&GcFlags, bool) -> bool;
    type Raw = fn(&GcFlags) -> Option<bool>;
    let arms: [(&str, BackendDefault, Pick, Raw, bool); 2] = [
        (
            "CRATONVM_TLAB_SHARE_SIZER",
            alloc_policy_defaults::TLAB_SHARE_SIZER,
            |g, generational| g.tlab_share_sizer_for(generational),
            |g| g.tlab_share_sizer,
            true,
        ),
        (
            "CRATONVM_SOFTREF_HOTSPOT_LRU",
            alloc_policy_defaults::SOFTREF_HOTSPOT_LRU,
            |g, generational| g.softref_hotspot_lru_for(generational),
            |g| g.softref_hotspot_lru,
            true,
        ),
    ];
    for (key, default, pick, raw, generational_on) in arms {
        assert_eq!(
            default,
            BackendDefault {
                generational: generational_on,
                other: false,
            },
            "{key}: only a Generational flip, never G1/ZGC"
        );
        let gc = |v: Option<&str>| {
            let pairs: Vec<(&str, &str)> = v.map(|v| (key, v)).into_iter().collect();
            VmFlags::from_source(&src(&pairs)).gc
        };
        for unset in [None, Some(""), Some("  ")] {
            let g = gc(unset);
            assert_eq!(raw(&g), None, "{key}={unset:?} keeps the default");
            assert_eq!(
                pick(&g, true),
                generational_on,
                "{key}={unset:?}: the Generational default"
            );
            assert!(!pick(&g, false), "{key}={unset:?}: OFF on G1/ZGC");
        }
        for off in ["0", "false", "off", "no", "OFF"] {
            let g = gc(Some(off));
            assert_eq!(raw(&g), Some(false), "{key}={off}");
            assert!(
                !pick(&g, true) && !pick(&g, false),
                "{key}={off} is OFF everywhere"
            );
        }
        for on in ["1", "true", "on", "yes"] {
            let g = gc(Some(on));
            assert_eq!(raw(&g), Some(true), "{key}={on}");
            assert!(
                pick(&g, true) && pick(&g, false),
                "{key}={on} is ON everywhere"
            );
        }
    }
    // zr z1/rf: the soft LRU's ZGC arm. Unset: ON on ZGC, the family
    // default elsewhere (G1 still OFF, Generational still ON); an
    // explicit value wins on ZGC too (`=0` is the ZGC kill switch).
    {
        let gc = |v: Option<&str>| {
            let pairs: Vec<(&str, &str)> = v
                .map(|v| ("CRATONVM_SOFTREF_HOTSPOT_LRU", v))
                .into_iter()
                .collect();
            VmFlags::from_source(&src(&pairs)).gc
        };
        let unset = gc(None);
        assert!(
            unset.softref_hotspot_lru_for_backend(false, true),
            "ZGC: ON"
        );
        assert!(
            !unset.softref_hotspot_lru_for_backend(false, false),
            "G1: OFF"
        );
        assert!(
            unset.softref_hotspot_lru_for_backend(true, false),
            "Gen: ON"
        );
        assert!(!gc(Some("0")).softref_hotspot_lru_for_backend(false, true));
        assert!(gc(Some("1")).softref_hotspot_lru_for_backend(false, false));
    }
    // The resolver itself, both defaults both ways.
    let d = BackendDefault {
        generational: false,
        other: true,
    };
    assert!(!d.resolve(None, true));
    assert!(d.resolve(None, false));
    assert!(d.resolve(Some(true), true));
    assert!(!d.resolve(Some(false), false));
}

/// gci i48/zl3 (`zx48-collector-scope-audit.md` A7): the share sizer's G1
/// arm. Unset: ON on G1 (the Generational default), Generational ON, ZGC
/// OFF; `=0` kills it on G1 and Generational; `=1` still arms ZGC. The
/// `[GC] policy:` line resolves the G1 default on a G1 heap.
#[test]
fn gci_i48_zl3_tlab_share_sizer_g1_arm() {
    const KEY: &str = "CRATONVM_TLAB_SHARE_SIZER";
    let gc = |v: Option<&str>| {
        let pairs: Vec<(&str, &str)> = v.map(|v| (KEY, v)).into_iter().collect();
        VmFlags::from_source(&src(&pairs)).gc
    };
    assert!(
        alloc_policy_defaults::TLAB_SHARE_SIZER_G1,
        "G1 follows Gen: ON"
    );
    assert_eq!(
        alloc_policy_defaults::TLAB_SHARE_SIZER_G1,
        alloc_policy_defaults::TLAB_SHARE_SIZER.generational,
        "same default as the Generational heap"
    );
    for unset in [None, Some(""), Some("  ")] {
        let g = gc(unset);
        assert!(
            g.tlab_share_sizer_for_backend(false, true),
            "{unset:?}: G1 ON"
        );
        assert!(
            g.tlab_share_sizer_for_backend(true, false),
            "{unset:?}: Gen ON"
        );
        assert!(
            !g.tlab_share_sizer_for_backend(false, false),
            "{unset:?}: ZGC OFF"
        );
        assert_eq!(
            g.tlab_share_sizer_for_backend(false, false),
            g.tlab_share_sizer_for(false),
            "the non-G1 arms are tlab_share_sizer_for"
        );
    }
    for off in ["0", "false", "off", "no"] {
        let g = gc(Some(off));
        assert!(
            !g.tlab_share_sizer_for_backend(false, true),
            "{off}: G1 killed"
        );
        assert!(
            !g.tlab_share_sizer_for_backend(true, false),
            "{off}: Gen killed"
        );
    }
    for on in ["1", "true", "on", "yes"] {
        let g = gc(Some(on));
        assert!(
            g.tlab_share_sizer_for_backend(false, false),
            "{on}: ZGC armed"
        );
        assert!(
            g.tlab_share_sizer_for_backend(false, true),
            "{on}: G1 armed"
        );
    }
    let env = |_: &str| -> Option<String> { None };
    let g1 = gc(None).policy_line("g1", false, &env);
    assert!(g1.contains(" tlab_share_sizer=on(default)"), "{g1}");
    let zgc = gc(None).policy_line("zgc", false, &env);
    assert!(zgc.contains(" tlab_share_sizer=off(default)"), "{zgc}");
    let set = |k: &str| -> Option<String> { (k == KEY).then(|| "0".to_string()) };
    let killed = gc(Some("0")).policy_line("g1", false, &set);
    assert!(killed.contains(" tlab_share_sizer=off(set)"), "{killed}");
}

/// gci i4/fl: the opt-in `GcFlags` booleans that parsed PRESENCE
/// (`gci-i3fl-gc-typed-flag-fields-and-shared-readers-still-parse-presence`)
/// read the value: unset and every off word are off, any other word is
/// on. Before the fix `CRATONVM_DBG_FORCE_MOVING=0` ran the unsafe arm.
type GcPick = fn(&GcFlags) -> bool;
const GCI_I4FL_GC_OPT_INS: &[(&str, GcPick)] = &[
    ("CRATONVM_NO_GC_PROMOTION_GUARD", |f| {
        f.no_gc_promotion_guard
    }),
    ("CRATONVM_PROMOTION_OOM_GUARD_BROAD", |f| {
        f.promotion_oom_guard_broad
    }),
    ("CRATONVM_NO_SELECTIVE_PROMOTE", |f| f.no_selective_promote),
    ("CRATONVM_SP_NO_COALESCE", |f| f.sp_no_coalesce),
    ("CRATONVM_NO_DEFRAG_PROMOTE", |f| f.no_defrag_promote),
    ("CRATONVM_CARD_TABLE_ONLY", |f| f.card_table_only),
    ("CRATONVM_GC_FULL_RSET_SCAN", |f| f.full_rset_scan),
    ("CRATONVM_GC_VERIFY_RSET", |f| f.verify_rset),
    ("CRATONVM_GC_SYNC_YOUNG_WIPE", |f| f.gc_sync_young_wipe),
    // CRATONVM_GC_PAR_EVAC_CARD_SEED left this list: default ON since gcj
    // j1yg-pause, pinned in `gcj_j1yg_the_parallel_card_seed_is_default_on`.
    // CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL left this list: default ON
    // (WARNING gci-user-flip-20261013), pinned in
    // `gci_user_flip_20261013_typed_fields_default_on`.
    // CRATONVM_GEN_PINNED_TAIL_WINDOW_LIVE left this list: default ON since the
    // 228-flag flip (1eb8ed7865).
    ("CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG", |f| {
        f.gc_system_gc_moving_young
    }),
    ("CRATONVM_DBG_FORCE_MOVING", |f| f.dbg_force_moving),
    ("CRATONVM_FWD_RESOLVE_STRICT", |f| f.fwd_resolve_strict),
    ("CRATONVM_DBG_SWEEP_ZERO", |f| f.dbg_sweep_zero),
    // CRATONVM_GC_LATE_RESOLVE_DROPPED left this list: default ON since the 228-flag flip.
    ("CRATONVM_NO_EXACT_REFPROC_SURVIVAL", |f| {
        f.no_exact_refproc_survival
    }),
    ("CRATONVM_NO_REFERENT_IDENTITY_SCREEN", |f| {
        f.no_referent_identity_screen
    }),
    ("CRATONVM_NO_OLDGEN_COALESCE", |f| f.no_oldgen_coalesce),
    ("CRATONVM_GC_G1_ONLY_JIT_PINS", |f| f.g1_only_jit_pins),
];

/// WARNING gci-user-flip-20261013: the typed GC fields the user flipped
/// default ON (`truthy_word_default_true`). Unset and empty read ON; the
/// off words kill; the on words stay on.
#[test]
fn gci_user_flip_20261013_typed_fields_default_on() {
    let cases: [(&str, GcPick); 3] = [
        ("CRATONVM_GEN_PINNED_YOUNG_COPY", |f| {
            f.gen_pinned_young_copy
        }),
        ("CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL", |f| {
            f.gen_pinned_young_copy_parallel
        }),
        ("CRATONVM_OLDGEN_COMPACT", |f| f.oldgen_compact),
    ];
    let unset = VmFlags::from_source(&MapSource::empty()).gc;
    for (name, pick) in cases {
        assert!(pick(&unset), "{name} unset reads ON (user flip)");
        for off in ["0", "false", "off", "no", " OFF "] {
            let f = VmFlags::from_source(&src(&[(name, off)])).gc;
            assert!(!pick(&f), "{name}={off:?} must kill");
        }
        for on in ["", "1", "true", "yes"] {
            let f = VmFlags::from_source(&src(&[(name, on)])).gc;
            assert!(pick(&f), "{name}={on:?} must read as on");
        }
    }
}

/// gcj j1yg-pause: the parallel card seed is default ON; every off word
/// kills it.
#[test]
fn gcj_j1yg_the_parallel_card_seed_is_default_on() {
    let unset = VmFlags::from_source(&MapSource::empty()).gc;
    assert!(unset.gc_par_evac_card_seed, "unset reads ON");
    for off in ["0", "false", "off", "no"] {
        let f = VmFlags::from_source(&src(&[("CRATONVM_GC_PAR_EVAC_CARD_SEED", off)])).gc;
        assert!(!f.gc_par_evac_card_seed, "{off:?} must kill");
    }
}

/// The two value companions are `None` whenever their gate is off, so a
/// consumer that tests only the value (`gen_heap`'s OOB write guard tests
/// `dbg_oobfield_value.is_some()`) cannot be armed by `=0`.

/// gci i4/fl review: the Generational opt-ins that parsed
/// `non_empty_non_zero` (`=false` / `=off` / `=no` switched them ON) and
/// `DBG_BLOCKED_ACCESS` (`=false` selected Panic) use the same value rule;
/// unset, empty and `0` read exactly as before.

/// `JitFlags::shadow_stack` reads by value too; its VM twin
/// (`conservative_roots::shadow_stack_enabled`) must use
/// `runtime_flag_on` in the same build (gci i4/fl report, hunk X-SS).
#[test]
fn gci_i4fl_shadow_stack_reads_by_value() {
    assert!(!VmFlags::from_source(&MapSource::empty()).jit.shadow_stack);
    for off in ["", "0", "false", "off", "no"] {
        let f = VmFlags::from_source(&src(&[("CRATONVM_SHADOW_STACK", off)]));
        assert!(!f.jit.shadow_stack, "SHADOW_STACK={off:?}");
    }
    assert!(
        VmFlags::from_source(&src(&[("CRATONVM_SHADOW_STACK", "1")]))
            .jit
            .shadow_stack
    );
}
