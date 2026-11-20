/// The dial must be consulted at every door that can run a `Bridge` in
/// front of concrete bytecode -- not just at step 1.
///
/// # Why this is a source scan and not a behavioural test
///
/// `H17-2` §5 established the defect BY GREP, not by inference: on
/// 2026-08-21 `jdk_only_enforce_shadow_for` had exactly one live call site
/// in the whole repository, inside `resolve_step1_native`. A dispatch that
/// does not pass through that line cannot be affected by the environment
/// variable, in any mode, ever -- so the regression this guards against is
/// structural, and the instrument that found it was structural. A
/// behavioural test would need a VM with a real class library, a
/// process-global env var read through a memoised slot, and a Java-visible
/// witness -- four of the six this directory has reached for are already
/// measured blind. This needs none of them and cannot go blind.
///
/// # What the doors cost, MEASURED
///
/// Instrumented build, `DialWitness direct`, armed for `java/util/HashMap`,
/// one case per process (`H0-8` rule 1):
///
/// ```text
///   [DIAL_DOOR_CENSUS] armed=true reached=947 yielded=57 leaked=890
///   [DIAL_DOOR] step1             reached=15   yielded=15   leaked=0
///   [DIAL_DOOR] invoke_or_native  reached=115  yielded=0    leaked=115
///   [DIAL_DOOR] force_intercept   reached=28   yielded=28   leaked=0
///   [DIAL_DOOR] cache_revalidate  reached=774  yielded=0    leaked=774
///   [DIAL_DOOR] cache_populate    reached=1    yielded=0    leaked=1
///   [DIAL_DOOR] jit_fast_native   reached=3    yielded=3    leaked=0
///   [DIAL_DOOR] stackless_force   reached=11   yielded=11   leaked=0
/// ```
///
/// **94% of armed `Bridge` dispatches never asked the dial**, and on a hot
/// workload (`DialWitness jit`, 60 000 iterations) it was 299 431 of
/// 299 469 -- so a suite, which is a hot workload, was very nearly unarmed.
/// The three doors with a non-zero `leaked` column are the three pinned
/// here. `force_intercept`, `stackless_force`, `jit_fast_native` and
/// `elidable_ctor` pass `bytecode_available: true` unconditionally under
/// `--real-jdk` and were already stricter than the dial, which is why they
/// are absent.
#[test]
fn every_leaking_door_asks_the_enforcement_dial() {
    // (source, what it is, anchor proving the scan reads the right text,
    // how many QUALIFIED dial consultations must be there)
    //
    // The needle is the QUALIFIED path, so neither the definition (in this
    // file, unqualified) nor a doc-comment mention (inside backticks, no
    // `(`) can inflate the count.
    let doors: &[(&str, &str, &str, usize)] = &[
        (
            concat!(include_str!("../../../vm/vm_exec/mod.rs"), "\n", include_str!("../../../vm/vm_exec/method.rs"), "\n", include_str!("../../../vm/vm_exec/report_pad_access.rs"), "\n", include_str!("../../../vm/vm_exec/diagnose_sync_on_value_based_class.rs"), "\n", include_str!("../../../vm/vm_exec/thread_interrupted_field_slot_of.rs"), "\n", include_str!("../../../vm/vm_exec/clear_breakpoints_of_redefined_class.rs"), "\n", include_str!("../../../vm/vm_exec/load_class.rs"), "\n", include_str!("../../../vm/vm_exec/native_new_initializes_class_first.rs"), "\n", include_str!("../../../vm/vm_exec/new_object.rs"), "\n", include_str!("../../../vm/vm_exec/gen_referent_read_keep_alive.rs"), "\n", include_str!("../../../vm/vm_exec/append_native_standin_frames.rs"), "\n", include_str!("../../../vm/vm_exec/unloaded_native_libraries.rs"), "\n", include_str!("../../../vm/vm_exec/extract_annotations_from_attributes.rs"), "\n", include_str!("../../../vm/vm_exec/invoke_static_settling_with_first.rs"), "\n", include_str!("../../../vm/vm_exec/invoke_special_shared.rs"), "\n", include_str!("../../../vm/vm_exec/proxy_resolve_declaring_method.rs"), "\n", include_str!("../../../vm/vm_exec/invoke_on_class_shared_inner.rs"), "\n", include_str!("../../../vm/vm_exec/jni_table_native_call.rs")),
            "vm/src/vm/vm_exec.rs :: invoke_or_native -- the registry-first probe, its \
             array-type alias retry, and both arms of its superclass walk",
            "fn invoke_or_native",
            4,
        ),
        (
            include_str!("../dispatch_static.rs"),
            "vm/src/runtime/interpreter/dispatch_static.rs :: populate_invoke_cache",
            "fn populate_invoke_cache",
            1,
        ),
        // Round 13 wave 7 (lane shadow): the virtual twin, which had none.
        // The direct receiver lookup and both arms of its ancestor walk,
        // mirroring `invoke_or_native`'s three.
        (
            include_str!("../dispatch_virtual.rs"),
            "vm/src/runtime/interpreter/dispatch_virtual.rs :: populate_virtual_invoke_cache",
            "fn populate_virtual_invoke_cache",
            3,
        ),
    ];
    for (src, what, anchor, want) in doors {
        // Establish the corpus is real before concluding anything from a
        // count: a file that stopped containing the door at all would make
        // this pass, or fail, for the wrong reason.
        assert!(
            src.contains(anchor),
            "{what}: the anchor `{anchor}` is gone, so this scan is looking at the wrong \
             text and whatever it counts means nothing"
        );
        let calls = src
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && t.contains("::jdk_only_dial_yields_to_bytecode(")
            })
            .count();
        assert!(
            calls >= *want,
            "{what} consults the enforcement dial at {calls} site(s), expected at least \
             {want}. `CRATONVM_ENFORCE_NATIVE_SHADOW` is meant to simulate a retirement, \
             and a retirement removes the REGISTRATION -- so every door misses, not just \
             the cold step-1 one. A door that stops asking silently returns the dial to \
             the state H17-2 measured: armed failures real, armed zeros unreliable, and \
             every armed cell in docs/known-issues/jdk-only/ pricing a hybrid that no \
             retirement can reach."
        );
    }
}

/// The warm invoke-cache door -- this file's own, and the one that carried
/// 82% of the leak.
///
/// Scoped to `revalidate_cached_native`'s body rather than counted over the
/// file, because the needle is unqualified here and this module's own
/// source would otherwise satisfy the search it performs.
#[test]
fn the_warm_invoke_cache_door_asks_the_enforcement_dial() {
    let src = include_str!("../native_override.rs");
    let start = src
        .find("pub(super) fn revalidate_cached_native(")
        .expect("`revalidate_cached_native` is gone, so this scan reads the wrong text");
    let rest = &src[start..];
    // The next `#[cfg(test)]` is this very module; everything between is
    // the function and the plain items that follow it.
    let end = rest.find("\n#[cfg(test)]").unwrap_or(rest.len());
    let body = &rest[..end];
    assert!(
        body.contains("jdk_only_dial_yields_to_bytecode("),
        "`revalidate_cached_native` no longer asks the enforcement dial. It is the door a \
         warmed native target is redeemed through, and it carried 774 of the 890 leaked \
         armed dispatches in the measurement quoted above -- 299 292 of 299 431 on a hot \
         one. A warmed entry published before the dial was consulted pins the bridge for \
         the life of the process, which is exactly how an armed run came to price a \
         hybrid."
    );
}

/// Every production file that forces a native either asks the dial, or is
/// named here with a reason.
///
/// # Why a source scan, and not a counter
///
/// `enforcement_dial` in `--real-jdk-report` counts a door only when that
/// door calls `note_dial_door`. A door that calls neither the dial nor the
/// census is therefore **invisible to the census**: `reached` does not
/// rise, `declined_no_bytecode` does not rise, and an armed report reads
/// exactly as it does when every door is wired. **Counting cannot find a
/// missing counter.**
///
/// That was not hypothetical. On 2026-08-22, after the fourteen-door fix,
/// `dispatch_virtual.rs` and `jit_bridge.rs` were each still forcing
/// natives through `force_native_over_real_jdk_bytecode` with ZERO dial
/// calls and ZERO census calls anywhere in the file — while the armed
/// report showed `reached == yielded` and nothing amiss. Lane WORKER 2 hit
/// the same gap from the other end: an armed `ConcurrentHashMap` dropping
/// stores and calling each one a fresh insert.
///
/// So this gate is structural. Adding a new undialled force path is a RED
/// TEST rather than something the next lane discovers from a corrupted map.
#[test]
fn every_force_native_file_asks_the_dial_or_is_exempt() {
    // (file, permanent?, reason).
    //
    // `permanent == false` is a HOLE that has been written down, and the
    // count of those is what should reach zero. `permanent == true` is a
    // file where the dial is structurally not applicable — it still calls
    // the helper, so the scan must account for it, but it is not
    // outstanding work. Filing both under one heading makes the list read
    // as twice the remaining problem.
    const FORCE_SITES_EXEMPT: &[(&str, bool, &str)] = &[];

    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }

    fn calls_force(src: &str) -> bool {
        src.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .any(|l| l.contains("force_native_over_real_jdk_bytecode("))
    }

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    walk(&root, &mut files);

    let mut offenders: Vec<String> = Vec::new();
    let mut checked = 0usize;
    for f in &files {
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        // `tests.rs` asserts the force TABLE's contents and dispatches
        // nothing; `native_override.rs` defines both the helper and the dial.
        if name == "tests.rs" || name == "native_override.rs" {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(f) else {
            continue;
        };
        if !calls_force(&src) {
            continue;
        }
        checked += 1;
        let asks = src.contains("jdk_only_dial_yields_to_bytecode")
            || src.contains("enforce_shadow_scope()");
        let exempt = FORCE_SITES_EXEMPT.iter().any(|(n, _, _)| *n == name);
        if !asks && !exempt {
            offenders.push(name);
        }
    }

    assert!(
        checked > 0,
        "no file under vm/src calls `force_native_over_real_jdk_bytecode(`, so this scan \
         is searching for a spelling that no longer exists and would pass on a tree with \
         no force gate at all"
    );
    assert!(
        offenders.is_empty(),
        "{offenders:?} force natives through `force_native_over_real_jdk_bytecode` without \
         ever consulting the enforcement dial, and without a row in FORCE_SITES_EXEMPT.\n\n\
         The `enforcement_dial` report CANNOT catch this: a door that never calls \
         `note_dial_door` does not raise `reached`, so an armed run looks identical to a \
         correctly-wired one. That is how these two files stayed undialled through a lane \
         that measured 890 of 947 leaked dispatches and believed it had closed all of \
         them.\n\n\
         Either consult the dial at the force site, or add a row with the reason."
    );

    // The exemption list must not rot, in EITHER direction, and the second
    // direction is the one that actually happens.
    //
    //   * a row naming a file that no longer forces anything is a stale
    //     approval;
    //   * a row naming a file that has since been WIRED is worse — it is a
    //     standing approval for a hole somebody already filled, and nothing
    //     would ever remove it.
    //
    // With both checked, this list can only shrink: wiring a force site
    // turns the gate red until its row is deleted.
    // ZERO, and held there. Every force site under vm/src now either consults
    // the dial or is a reasoned `permanent: true` non-hole. A new unwired
    // site is a red test on its own — nobody has to notice a count creep up.
    let holes = FORCE_SITES_EXEMPT
        .iter()
        .filter(|(_, perm, _)| !*perm)
        .count();
    assert_eq!(
        holes, 0,
        "{holes} force site(s) are UNWIRED holes. A `permanent: true` row is a \
         reasoned non-hole and does not count toward this. If a hole genuinely has \
         to exist for a while, write the reason and the retiring condition in the \
         row and raise this bound deliberately — do not flip the flag to true, \
         which is what turns a to-do into a permanent approval."
    );

    for (name, _, _) in FORCE_SITES_EXEMPT {
        let found = files
            .iter()
            .find(|f| f.file_name().unwrap().to_string_lossy() == *name)
            .and_then(|f| std::fs::read_to_string(f).ok());
        let Some(src) = found else {
            panic!(
                "FORCE_SITES_EXEMPT names `{name}`, which is not a file under vm/src. \
                 Drop the row — a row that matches nothing reads as a reviewed hole."
            );
        };
        assert!(
            calls_force(&src),
            "FORCE_SITES_EXEMPT names `{name}`, which no longer calls the force helper. \
             Drop the row — a stale exemption reads as a reviewed hole."
        );
        assert!(
            !(src.contains("jdk_only_dial_yields_to_bytecode")
                || src.contains("enforce_shadow_scope()")),
            "FORCE_SITES_EXEMPT still names `{name}`, but it NOW CONSULTS THE DIAL. \
             Delete the row. An exemption that outlives the hole it excused is a \
             standing approval nobody will ever revisit, and it makes the list read \
             as bigger than the remaining problem."
        );
    }
}

/// The scans above can fail.
///
/// A guard that cannot fail reads exactly like one that works; this file
/// already carries a note about three such guards found in one evening. If
/// the helper is renamed, every count above silently drops to zero and the
/// assertions become a report about a string that appears nowhere -- so pin
/// the spelling against its definition site.
#[test]
fn the_door_scans_are_looking_for_a_name_that_exists() {
    let src = include_str!("../native_override.rs");
    assert!(
        src.contains("pub(crate) fn jdk_only_dial_yields_to_bytecode("),
        "`jdk_only_dial_yields_to_bytecode` was renamed or removed, so the two door scans \
         above now search for a string that appears nowhere and would pass on a tree with \
         no dial wiring at all"
    );
}
