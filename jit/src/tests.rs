// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The crate-root unit-test suite, split out of `lib.rs` on 2026-09-16.
//!
//! This is the one extraction in this round that is drawn around a *build
//! configuration* rather than around a subject. Everything in here is
//! `#[cfg(test)]`: it is compiled only by `cargo test`, it is linked into no
//! shipped artefact, and nothing in the crate's production paths can name it.
//! That makes its boundary the cheapest real one available -- the compiler
//! already proves that no production code depends on this text, so moving it
//! cannot change what the JIT does.
//!
//! It is worth saying why that matters more here than it would in a small
//! crate. `lib.rs` is the file a reader opens to find out how compilation is
//! driven, and nearly a quarter of it was test bodies that only `cargo test`
//! ever reads. Interleaving them with the driver meant that scrolling from the
//! compile entry point to the single-pass tier crossed thousands of lines of
//! assertions, so the shape of the file stopped reflecting the shape of the
//! code. The tests did not get harder to read by moving; the driver got easier.
//!
//! # Why this is a file module and not a nested one
//!
//! Every test in here reaches back into the crate root through `super::`, and
//! many of the items it reaches for are PRIVATE to the crate root -- module
//! bodies, helper functions, counters that exist for exactly one assertion. A
//! private item is visible to its own module and to every descendant of it, so
//! those paths only keep resolving while this module stays a DIRECT child of
//! the crate root. `mod tests;` in `lib.rs` backed by `tests.rs` beside it
//! keeps the module path at `crate::tests`, exactly where the inline `mod
//! tests { .. }` block had it, which is why the bodies below did not have to be
//! touched: `super::X` still means the same `X` it meant before the split.
//!
//! The four sibling suites that stayed behind in `lib.rs`
//! (`code_buffer_retry_tests`, `long_box_direct_bind_tests`,
//! `direct_exc_table_publish_policy`, `varhandle_read_direct_bind_tests`) are
//! small and sit directly under the code they exercise; moving them would have
//! bought a few hundred lines at the cost of four more files, so they stayed.
//!
//! The only edit made to the bodies was removing one level of indentation,
//! which the `\`-continued string literals in here are immune to: Rust drops
//! the newline and all leading whitespace after a trailing backslash, so the
//! assertion messages are byte-identical to what they were inline.

// ───────────────────────────────────────────────────────────────────────
// Backend parity: the resolver-table split, and the driver it exists for
// ───────────────────────────────────────────────────────────────────────

/// Serialises the two census tests below against each other.
///
/// `backend_parity`'s census is process-global and MONOTONE — its own
/// header says so, and says why a caller must read a DELTA and a test
/// wanting an exact delta "has to serialise against other tests in the same
/// binary, because `cargo test` runs them in parallel threads on one
/// process". `the_parity_flag_off_runs_no_shadow_compile` asserts a delta
/// of exactly zero and `the_parity_flag_on_compares_and_still_installs_ir`
/// moves the same counter, so without this lock the first would fail
/// whenever the second happened to land inside its window. They are the
/// only two movers in THIS binary — `jit/tests/backend_parity_analysis.rs`
/// moves it too, but that is a separate test binary and so a separate
/// process, with its own lock for its own pair.
static BACKEND_PARITY_CENSUS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// `static int probe() { return 1; }` as a `CachedBytecodeMethod` named
/// `name` — a method with no constant-pool reference of any kind, so every
/// resolver table below comes back empty and neither test can fail for a
/// reason about resolution.
///
/// Each test passes its own `name` because `compile_gate::admit` keys its
/// bail-list and its per-method state on the class/method/descriptor
/// triple, and two tests sharing one identity would be able to bail-list
/// each other.
///
/// Two trailing zero bytes because `try_compile_inner` derives the body
/// length as `code.len() - 2`; the fixtures in `x64/single_pass_only.rs`
/// carry the same padding for the same reason.
#[cfg(target_arch = "x86_64")]
fn parity_probe_method(name: &'static str) -> CachedBytecodeMethod {
    use std::sync::Arc;
    CachedBytecodeMethod {
        declaring_class_id: cratonvm_types::ClassId::new(1),
        class_name: Arc::from(name),
        method_name: Arc::from("probe"),
        method_descriptor: Arc::from("()I"),
        source_file: None,
        // iconst_1; ireturn, then the two-byte tail.
        code: Arc::from([0x04, 0xac, 0x00, 0x00].as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 1,
        max_locals: 1,
        num_params: 0,
        is_synchronized: false,
        is_static: true,
        force_native_cache: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    }
}

/// The resolver-table split must be behaviour-preserving, and what makes
/// that checkable is proving the TABLES ARE THE WHOLE INPUT to the backend.
///
/// Before the split, `single_pass_tier` resolved its twenty-odd per-site
/// tables into locals and handed them straight to
/// `x64::compile_with_param_slots`. There was no way to run the backend a
/// second time off one resolution, and therefore no way to notice if the
/// resolution had left per-compile state somewhere the backend also reads —
/// a counter, a thread-local, a half-consumed intern — which a second run
/// would see differently. This builds the tables once and runs the backend
/// twice off them. Byte-identical bodies say the backend's output is a
/// function of the tables, which is exactly the property the shadow compile
/// in `backend_parity_shadow_compile` and the "the default path emits the
/// same bytes" claim on `single_pass_tier` both rest on.
///
/// It is NOT a claim that `build_single_pass_tables` is pure — that
/// function's own doc lists the three things it is not pure about. It is
/// the narrower claim the split needs.
///
/// Byte equality is a fair assertion to make here because the backend is
/// already held to it: `jit/tests/x64_artifact_corpus.rs`'s
/// `corpus_is_deterministic_within_a_process` compiles its whole corpus
/// twice in one process and requires identical output, for the same
/// reasons (hash-map iteration order, address-dependent decisions). If that
/// test goes red, expect this one to go red with it and fix that one first.
///
/// The clone is the test's, never production's. Neither artifact is
/// installed and both are dropped here, which is what keeps the one hazard
/// on `SinglePassTables::Clone` — two installed bodies sharing one arena —
/// out of reach.
#[test]
// x86-64 only: this drives the x64 single-pass backend directly.
#[cfg(target_arch = "x86_64")]
fn single_pass_table_build_is_callable_twice() {
    // The entry counter (default-on since round 9 wave 3) bakes a per-compile
    // box address into every prologue; byte comparison across compiles needs
    // it off on this thread.
    let _no_entry_counter = cratonvm_types::flags::override_thread(
        cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
            "CRATONVM_JIT_ENTRY_COUNTER",
            Some("0"),
        )]),
    );
    let cached = parity_probe_method("pkg/ParityTables");
    // SAFETY: every `JitRuntimeHelpers` field is a `usize` and the struct is
    // `#[repr(C)]`, so an all-zero bit pattern is valid. `probe` references
    // no runtime helper and the emitted code is never executed.
    let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    let req = CompileRequest::new(&cached, &helpers);

    let code: &[u8] = &cached.code;
    let code_len = code.len().saturating_sub(2);
    let scan = x64::jit_scan(code, code_len, &cached.method_descriptor)
        .expect("`probe` must scan as JIT-compatible");
    let admission = compile_gate::CompileAdmission::for_backend_test();

    let mut attempted = false;
    let (tables, parts) = build_single_pass_tables(
        &req,
        &mut attempted,
        false, // self_call_identity_stable
        false, // local_handlers_disarmed
        &metrics::CompileRecorder::disabled(),
        code,
        code_len,
        scan,
        0,     // prologue_param_slots: a static no-arg method
        false, // precise_exception_frames
        None,  // resolved_string_layout
    )
    .expect("a resolver-free int method must resolve its (empty) tables");

    let first = tables
        .clone()
        .run_backend(
            &admission,
            code,
            code_len,
            cached.max_locals as usize,
            &helpers,
            None,
            &parts,
        )
        .expect("the first backend run must produce a body");
    let second = tables
        .run_backend(
            &admission,
            code,
            code_len,
            cached.max_locals as usize,
            &helpers,
            None,
            &parts,
        )
        .expect("the second backend run must produce a body");

    assert_eq!(
        first.code_bytes(),
        second.code_bytes(),
        "two backend runs off ONE table build emitted different bodies. \
         Either table construction left per-compile state the backend also \
         reads, or the backend is not a function of its arguments — and in \
         both cases `backend_parity_shadow_compile`'s second compile would \
         be measuring something other than what the baseline tier would \
         have installed."
    );
}

/// With `CRATONVM_JIT_BACKEND_PARITY` OFF — the default — an optimizing
/// compile must not run a shadow single-pass compile at all.
///
/// The cost of the flag is a whole discarded compile per optimizing
/// compile, so "off means off" is the property that keeps it out of
/// production. The census is the witness: `comparisons` is bumped once per
/// `compare_bodies` and once per `note_comparison_blind`, which are the two
/// exits of the shadow path, so a delta of zero means the path was never
/// entered.
#[test]
// x86-64 only: on another architecture `try_compile_inner` returns from its
// `#[cfg(target_arch = "aarch64")]` branch before either tier runs.
#[cfg(target_arch = "x86_64")]
fn the_parity_flag_off_runs_no_shadow_compile() {
    let _serial = BACKEND_PARITY_CENSUS_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Same two pins as `step3_optimize_toggle_routes_c1_singlepass_and_c2_ir`:
    // this is a ROUTING test, and the C1->C2 acceptance gate would refuse a
    // two-bytecode probe on evidence grounds while the moving-young gate
    // would switch the optimizing tier off entirely.
    let _accept = crate::ir_evidence::AcceptAlways::on();
    crate::x64::set_moving_young_override(Some(false));

    let cached = parity_probe_method("pkg/ParityOff");
    // SAFETY: as in `single_pass_table_build_is_callable_twice`.
    let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };

    let before = crate::backend_parity::backend_parity_census().comparisons;
    let compiled = cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_JIT_BACKEND_PARITY", None)],
        || {
            try_compile_request(&CompileRequest {
                optimize: true,
                ..CompileRequest::new(&cached, &helpers)
            })
        },
    )
    .expect("optimize=true must compile `probe`");
    let after = crate::backend_parity::backend_parity_census().comparisons;

    assert!(
        compiled.used_ir_backend,
        "the probe must take the optimizing tier, or this test proves \
         nothing about the flag-off shadow path — it would be asserting \
         that a path nobody reached did not run"
    );
    assert_eq!(
        before, after,
        "the parity census moved with `CRATONVM_JIT_BACKEND_PARITY` unset. \
         Every optimizing compile in the process is then paying for a \
         discarded single-pass compile, which is what the flag exists to \
         keep out of production."
    );
}

/// With the flag ON over a method the optimizing tier takes, the comparison
/// must actually happen — and the body that gets installed must still be
/// the optimizing one.
///
/// Both halves matter. Without the first the flag would be a no-op that
/// reports a clean run by never looking, which is the failure mode
/// `backend_parity`'s header refuses to ship. Without the second the
/// diagnostic would be changing which code runs, and a harness that
/// perturbs the thing it measures is worse than no harness.
///
/// `comparisons`, not `comparisons_sighted`: the shadow compile is allowed
/// to decline or to produce a body the decoder cannot walk, and both of
/// those are blind comparisons rather than findings. What is asserted is
/// that the driver RAN, which is what the counter records.
#[test]
// x86-64 only, for the same reason as the sibling above.
#[cfg(target_arch = "x86_64")]
fn the_parity_flag_on_compares_and_still_installs_ir() {
    let _serial = BACKEND_PARITY_CENSUS_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _accept = crate::ir_evidence::AcceptAlways::on();
    crate::x64::set_moving_young_override(Some(false));

    let cached = parity_probe_method("pkg/ParityOn");
    // SAFETY: as in `single_pass_table_build_is_callable_twice`.
    let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };

    let before = crate::backend_parity::backend_parity_census().comparisons;
    let compiled = cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_JIT_BACKEND_PARITY", Some("1"))],
        || {
            try_compile_request(&CompileRequest {
                optimize: true,
                ..CompileRequest::new(&cached, &helpers)
            })
        },
    )
    .expect("optimize=true must compile `probe` with the parity flag on");
    let after = crate::backend_parity::backend_parity_census().comparisons;

    assert!(
        after > before,
        "the parity flag was on over a method the optimizing tier took and \
         the census did not move: the shadow single-pass compile never ran, \
         so the harness would report a clean corpus without having looked \
         at one body"
    );
    assert!(
        compiled.used_ir_backend,
        "the parity flag changed which body was installed. It is a \
         diagnostic: the shadow artifact is measured and dropped, and the \
         optimizing body is installed exactly as it is with the flag off"
    );
}

/// The `System.arraycopy` intrinsic's scratch homes must be allocated
/// clear of the five operands' own frame homes.
///
/// `pop_stack` reclaims a Frame slot sitting at the top of the spill
/// region, so the intrinsic's five pops rewind `next_spill_offset` back
/// over the very slots `flush_scratch_registers` spilled the oop operands
/// into. Basing the scratch homes on `next_spill_offset` therefore aliases
/// them BY CONSTRUCTION — and the emitter's own workaround (load all five
/// into distinct GPRs before storing any) only protects its own reads. The
/// deopt snapshot is a second consumer: it names each operand's original
/// home and reads it when the bail fires, long after `s_src_pos`'s store
/// has overwritten `dst`'s home with srcPos. A reference-array copy — which
/// always bails here, by design — then resumed with `Object(srcPos)` where
/// `dst` belonged and threw NullPointerException on a valid copy.
///
/// A source witness because reproducing it needs a full `Vm`, an OSR-
/// compiled method and a specific spill layout; the executable half is
/// `regression-suite/src/RJitArraycopyRefDeopt.java`. Anchored on code
/// text, not line numbers.
#[test]
fn arraycopy_scratch_homes_are_allocated_clear_of_the_operand_homes() {
    let src = std::fs::read_to_string(format!(
        "{}/src/x64/op_invoke.rs",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("read op_invoke.rs");

    let at = src
        .find("let s_src = ")
        .expect("the arraycopy scratch-home allocation must still exist");
    let from = at.saturating_sub(2500);
    let window = &src[from..at];

    assert!(
        window.contains("let mut scratch_base"),
        "the arraycopy intrinsic's scratch homes are not computed from a base \
         that clears the operand homes. Basing them on `next_spill_offset` \
         aliases `dst`'s spilled home, and the deopt snapshot reads that home."
    );
    assert!(
        window.contains("StackSlot::Frame(off)") && window.contains("scratch_base.max("),
        "the scratch base must be pushed past EVERY operand that lives in a \
         frame slot; a base that does not consult the operand homes cannot \
         know it clears them"
    );
    // The five homes must all be derived from that base, not from
    // `next_spill_offset` — the whole point of computing it.
    let decls = &src[at..(at + 400).min(src.len())];
    assert!(
        !decls.contains("next_spill_offset"),
        "an `s_*` scratch home is still taken from `next_spill_offset`, which \
         is exactly the aliasing this base was introduced to remove"
    );
}

/// Every publication into `jit_cache` must stamp
/// `CompiledMethod::requires_wrapped_entry`.
///
/// The bit is what tells an unwrapped consumer that this body is an
/// `ACC_SYNCHRONIZED` method's and carries no monitor prologue. A `put`
/// that forgets it publishes a body that every raw-entry door will happily
/// CALL unlocked — which is the defect, not a variant of it: `bumpStatic`
/// lost ~35 of 240 000 monitor-protected increments per run that way.
///
/// A source witness because the alternative is a full `Vm` plus a
/// background compile thread; it is anchored on code text, not line
/// numbers, and on the `put` call itself, so a new publication site cannot
/// be added without either stamping or failing here.
#[test]
fn every_jit_cache_publication_stamps_the_wrapped_entry_requirement() {
    let src = std::fs::read_to_string(format!(
        "{}/../vm/src/runtime/interpreter/jit_bridge.rs",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("read jit_bridge.rs");

    let puts: Vec<usize> = src
        .match_indices("jit_cache.put(")
        .map(|(i, _)| i)
        .collect();
    assert!(
        !puts.is_empty(),
        "no `jit_cache.put(` sites found — retarget this witness"
    );
    for at in puts {
        // The stamp is the last statement before the write lock is taken,
        // so look back over a window comfortably wider than the
        // `stamp_compilation_epoch` call that also sits in it.
        let from = at.saturating_sub(900);
        assert!(
            src[from..at].contains("requires_wrapped_entry ="),
            "a `jit_cache.put(` at byte {at} publishes a body without stamping \
             `requires_wrapped_entry`. An unstamped synchronized body is served \
             to the raw-entry dispatch doors and runs with no monitor."
        );
    }
}

/// The by-name callee entry point must refuse a wrapped-entry body on its
/// `jit_cache` FAST PATH, not only on the compile path behind it.
///
/// This is the exact shape of the defect. `try_jit_compile_callee_slow`
/// refused `ACC_SYNCHRONIZED` callees all along; `try_jit_compile_callee`
/// answers from `jit_cache` first and never reached that refusal, so a body
/// the background tiering door published FOR the wrapped entry was handed
/// to callers that supply no monitor. A gate in front of a slow path guards
/// nothing once the fast path can answer.

/// A copy of `src` with every ASCII whitespace character removed, for
/// witnesses that must survive `cargo fmt`.
///
/// A source witness exists to say "this guard is still in the code". When
/// it anchors on an exact string it also, silently, asserts how that string
/// is WRAPPED — and then a formatting pass makes it fail while reporting
/// that the guard is gone. `3de6b9c64` did exactly that to
/// `the_dispatch_helpers_jit_cache_arm_refuses_a_wrapped_entry_body`:
/// `jit_cache.get(` became `jit_cache\n    .get(`, the witness stopped
/// matching, and its message said the arm "must still exist" about an arm
/// that had not changed at all.
///
/// Stripping whitespace collapses every legal formatting of the same
/// expression onto one string, so the assertion is about the code.
///
/// It does NOT strip comments, and it must not: matching the CODE form is
/// how these witnesses avoid passing against a deleted check that a nearby
/// comment still describes. The patterns below all contain punctuation no
/// prose carries (`|compiled|!`), which is what keeps that true.
fn code_only(src: &str) -> String {
    src.chars().filter(|c| !c.is_ascii_whitespace()).collect()
}

#[test]
fn the_callee_cache_fast_path_refuses_a_wrapped_entry_body() {
    let src = std::fs::read_to_string(format!(
        "{}/../vm/src/runtime/interpreter/jit_bridge.rs",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("read jit_bridge.rs");
    let flat = code_only(&src);
    assert!(
        flat.len() > 100_000,
        "jit_bridge.rs collapsed to {} chars — the read did not reach the \
         file, so everything below would pass vacuously",
        flat.len()
    );

    let at = flat
        .find("ifletSome(compiled)=jit_cache.get(class_name,method_name,descriptor,probe_class_id)")
        .expect("the callee `jit_cache` fast path must still exist");
    let end = flat[at..]
        .find("returnSome((compiled,entry,needs_ctx));")
        .map(|off| at + off)
        .expect("the fast path must still hand back an entry");
    // The CODE form, not the bare identifier: this arm carries an
    // explanatory comment that names the field, and matching that would
    // let the witness pass against a deleted check. The sibling witness
    // below was caught doing exactly that.
    assert!(
        flat[at..end].contains("ifcompiled.requires_wrapped_entry"),
        "`try_jit_compile_callee`'s `jit_cache` fast path hands back a compiled \
         entry without asking `requires_wrapped_entry`. Every caller of this \
         function CALLs that entry raw, with no monitor."
    );
}

/// The dispatch helper's own `jit_cache` arm must refuse one too.
///
/// It does not go through `try_jit_compile_callee` at all, and it CACHES
/// what it takes in `DISPATCH_CACHE` — so serving a synchronized body once
/// makes every later call at that site run unlocked as well.
#[test]
fn the_dispatch_helpers_jit_cache_arm_refuses_a_wrapped_entry_body() {
    let src = std::fs::read_to_string(format!(
        "{}/../vm/src/jit/helpers.rs",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("read vm/src/jit/helpers.rs");

    // Anchor on the arm's own `get`, not on the `jit_cache.read()` above
    // it: the file has an earlier reader that only inspects `has_indy_trap`
    // and never hands out an entry, and matching that one would make this
    // witness pass while the real arm went unguarded.
    // Anchor on the arm that takes an `info`-keyed entry, matched without
    // assuming a line ending: the file has an earlier `jit_cache` reader
    // that only inspects `has_indy_trap` and never hands out an entry, and
    // matching that one would make this witness pass while the real arm
    // went unguarded.
    let flat = code_only(&src);
    assert!(
        flat.len() > 100_000,
        "helpers.rs collapsed to {} chars — the read did not reach the file, \
         so everything below would pass vacuously",
        flat.len()
    );

    // The arm is identified by what it reads (`info.*`, the dispatch site's
    // own metadata), which is what distinguishes it from the two other
    // `requires_wrapped_entry` filters in this file.
    let at = flat
        .find("ifletSome(compiled)=jit_cache.get(info.class_name,info.method_name,info.descriptor,info_class_id,)")
        .expect("the dispatch helper's jit_cache arm must still exist");

    // ORDERING, not proximity. The old form asked whether the guard
    // appeared within 1400 characters, which says nothing about whether it
    // gates anything. What matters is that the filter runs BEFORE the
    // `DISPATCH_CACHE` insert: serving a synchronized body once is bad, and
    // caching it makes every later call at the site run unlocked too.
    let cache_at = flat[at..]
        .find("DISPATCH_CACHE.with(")
        .map(|off| at + off)
        .expect("the arm must still populate DISPATCH_CACHE");
    // The CODE form. Matching the bare identifier passed against a
    // `.filter(|_c| true)` because the explanatory comment above the
    // filter still named the field -- a probe that could not fail.
    assert!(
        flat[at..cache_at].contains(".filter(|compiled|!compiled.requires_wrapped_entry)"),
        "`jit_invoke_dispatch`'s `jit_cache` arm fills `DISPATCH_CACHE` with a \
         raw entry without asking `requires_wrapped_entry`"
    );
}

/// `pack_multianewarray_site` must survive the round trip for every class
/// id and cp index a real site can carry, and must not let one field bleed
/// into the other.
///
/// The packing exists only because Windows x64 gives a helper four register
/// arguments and three are already spent; if it ever silently truncated, the
/// helper would resolve the WRONG constant-pool entry and allocate an array
/// of the wrong class — which is precisely the defect the packed site was
/// introduced to fix, reappearing one layer down.
#[test]
fn multianewarray_site_packing_round_trips() {
    for &cid in &[0u32, 1, 7, 4096, 0x0001_0000, 0x7FFF_FFFF, u32::MAX] {
        for &cp in &[0u16, 1, 7, 255, 256, 4095, u16::MAX] {
            let (got_cid, got_cp) =
                crate::unpack_multianewarray_site(crate::pack_multianewarray_site(cid, cp));
            assert_eq!(
                (got_cid, got_cp),
                (cid, cp),
                "multianewarray site packing lost information for \
                 (class_id={cid}, cp_idx={cp})"
            );
        }
    }
    // The two fields must be independent: changing only the cp index must
    // not move the class id, and vice versa.
    assert_ne!(
        crate::pack_multianewarray_site(5, 1),
        crate::pack_multianewarray_site(5, 2)
    );
    assert_ne!(
        crate::pack_multianewarray_site(5, 1),
        crate::pack_multianewarray_site(6, 1)
    );
}

/// The `multianewarray` lowering must hand the helper the packed SITE, not
/// a pre-digested element type.
///
/// A leaf element-type code names no class, so the helper could only
/// allocate with `ClassId(0)`: a JIT-compiled `new String[a][b]` came back
/// with `getClass() == [Ljava.lang.Object;` and every `checkcast` to the
/// declared array type threw. Commons Math's `DSCompiler.getCompiler`
/// publishes such an array through an `AtomicReference` and casts it back on
/// the next call — 118 of `DerivativeStructureTest`'s 124 methods failed
/// under the JIT and none under `--nojit`.
///
/// This is a source witness because the alternative is a full `Vm` plus a
/// hand-built classfile; it is anchored on code text, not line numbers.
#[test]
fn multianewarray_lowering_passes_the_resolved_site_not_an_element_type() {
    let src = std::fs::read_to_string(format!(
        "{}/src/x64/op_object.rs",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("read op_object.rs");

    let arm = src
        .find("// multianewarray — allocate multi-dimensional array (2D only)")
        .expect("the multianewarray arm must still exist");
    let end = src[arm..]
        .find("self.helpers.multianewarray_2d")
        .map(|off| arm + off)
        .expect("the arm must still call the multianewarray_2d helper");
    let body = &src[arm..end];

    assert!(
        body.contains("multianewarray_info"),
        "the multianewarray lowering must look its site up in \
         `multianewarray_info`"
    );
    assert!(
        !body.contains("unwrap_or(10)"),
        "the multianewarray lowering must not fall back to a default \
         element type: there is no default array CLASS, and a site with no \
         resolved entry has to bail rather than allocate the wrong type"
    );
    assert!(
        body.contains("emit_mov_imm64"),
        "the packed site is a 64-bit immediate (class id + cp index); a \
         32-bit move would truncate the cp index away"
    );
}

/// The helper must resolve through the interpreter's own multianewarray
/// body, not carry a second transcription of JVMS §multianewarray.
///
/// The two WERE separate copies, and only the interpreter's resolved the
/// per-level component classes. That is the whole defect; a second copy
/// reappearing is the whole regression.
#[test]
fn multianewarray_helper_calls_the_shared_interpreter_body() {
    let helpers = std::fs::read_to_string(format!(
        "{}/../vm/src/jit/helpers.rs",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("read vm/src/jit/helpers.rs");
    let f = helpers
        .find("pub unsafe extern \"C\" fn jit_multianewarray_2d(")
        .expect("the multianewarray helper must still exist");
    let body = &helpers[f..f + 4000];
    assert!(
        body.contains("multianewarray_alloc("),
        "jit_multianewarray_2d must call `interpreter::multianewarray_alloc`; \
         a private allocation loop here is how the JIT came to stamp \
         `ClassId(0)` on every level"
    );
    assert!(
        !body.contains("ClassId::new(0)"),
        "jit_multianewarray_2d must not allocate any level with `ClassId(0)`: \
         that is what made `new String[a][b]` read back as \
         `[Ljava.lang.Object;`"
    );
}

/// The optimizing tier must decline exactly the shape that crashed
/// `SparseRealVectorTest`, and nothing broader.
///
/// Both directions matter and they fail differently. Refusing too much is
/// silent — the method drops to single-pass and only a benchmark notices,
/// which is why the measured reach is pinned here as well as the witness.
/// Refusing too little is a hard `InternalError` on the first trap.
#[test]
fn ir_declines_an_unresumable_protected_trap_and_only_that() {
    use cratonvm_reader::attribute::ExceptionTableEntry;

    fn range(start: u16, end: u16) -> Vec<ExceptionTableEntry> {
        vec![ExceptionTableEntry {
            start_pc: start,
            end_pc: end,
            handler_pc: end,
            catch_type: 1,
        }]
    }

    // `OpenIntToDoubleHashMap$Iterator.advance()` in miniature: a putfield
    // (side effect) and a baload (deopt-guarded trap) inside one range.
    //   0: aload_0        (0x2a)
    //   1: aload_0        (0x2a)
    //   2: getfield  #1   (0xb4 0x00 0x01)
    //   5: putfield  #2   (0xb5 0x00 0x02)
    //   8: aload_0        (0x2a)
    //   9: iconst_0       (0x03)
    //  10: baload         (0x33)
    //  11: return         (0xb1)
    let advance_like = [
        0x2a, 0x2a, 0xb4, 0x00, 0x01, 0xb5, 0x00, 0x02, 0x2a, 0x03, 0x33, 0xb1,
    ];
    let site = ir_unresumable_protected_trap(&advance_like, advance_like.len(), &range(0, 11));
    assert!(site.is_some(), "the witness shape must be declined");

    // No exception table at all: the trap propagates out, nothing to route.
    assert_eq!(
        ir_unresumable_protected_trap(&advance_like, advance_like.len(), &[]),
        None,
        "an unprotected trap is not this gate's business"
    );

    // The trap is OUTSIDE the protected range.
    assert_eq!(
        ir_unresumable_protected_trap(&advance_like, advance_like.len(), &range(0, 2)),
        None,
        "a range that does not cover the trap must not be declined"
    );

    // Read-only range: a baload with nothing committed before it. Replaying
    // from entry is harmless here, so declining would cost a compile and
    // buy nothing — the narrowing term the H2 precedent asks for.
    //   0: aload_0, 1: iconst_0, 2: baload, 3: ireturn
    let read_only = [0x2a, 0x03, 0x33, 0xac];
    assert_eq!(
        ir_unresumable_protected_trap(&read_only, read_only.len(), &range(0, 3)),
        None,
        "a side-effect-free protected trap must still compile"
    );
    // ...and the OTHER half of that narrowing term, which for a long time
    // was only an assumption: the consumer must actually be willing to
    // replay this body. `bytecode_commits_side_effect` is what the
    // interpreter's deopt sink asks before it raises `InternalError:
    // precise deoptimization unavailable`, and it must agree with the
    // admission above on exactly this shape. If these two ever disagree,
    // the population admitted BECAUSE the replay is harmless is the
    // population that dies.
    assert!(
        !bytecode_commits_side_effect(&read_only, read_only.len()),
        "the shape admitted because its replay is harmless must read as \
         side-effect-free at the consumer too"
    );

    // A protected range whose only throwing site is an invoke: those exit
    // through the sentinel + exception routing, which needs no resume.
    //   0: aload_0, 1: invokevirtual #3, 4: return
    let invoke_only = [0x2a, 0xb6, 0x00, 0x03, 0xb1];
    assert_eq!(
        ir_unresumable_protected_trap(&invoke_only, invoke_only.len(), &range(0, 4)),
        None,
        "an invoke is not a deopt-guarded inline trap"
    );

    // Side effect present but no deopt-guarded trap: putstatic only.
    //   0: iconst_0, 1: putstatic #4, 4: return
    let store_only = [0x03, 0xb3, 0x00, 0x04, 0xb1];
    assert!(
        bytecode_commits_side_effect(&store_only, store_only.len()),
        "a putstatic is a side effect a replay would duplicate"
    );
    assert_eq!(
        ir_unresumable_protected_trap(&store_only, store_only.len(), &range(0, 4)),
        None,
        "a side effect with no trap has nothing to deopt on"
    );

    // Every opcode family the IR tier lowers to a deopt guard must be
    // recognised — this is the list that has to stay in step with
    // `ir_lower.rs`, and the one that silently rots if nobody pins it.
    for (op, label) in [
        (0x2eu8, "iaload"),
        (0x33u8, "baload"),
        (0x4fu8, "iastore"),
        (0x54u8, "bastore"),
        (0x6cu8, "idiv"),
        (0x70u8, "irem"),
        (0xb4u8, "getfield"),
        (0xbeu8, "arraylength"),
    ] {
        // `putstatic` supplies the side effect so the trap is the variable
        // under test; two-byte operands for the field ops.
        let code = [0x03, 0xb3, 0x00, 0x04, op, 0x00, 0x01, 0xb1];
        let len = if matches!(op, 0xb4 | 0xb5) { 8 } else { 6 };
        assert!(
            ir_unresumable_protected_trap(&code[..len], len, &range(0, (len - 1) as u16)).is_some(),
            "{label} must be recognised as a deopt-guarded trap"
        );
    }
}

/// `jit_ir_exception_stub_throw_bci`'s probe must stay ADMISSIBLE here.
///
/// That e2e test is the only thing that shows the IR exception stub's
/// `jit_set_throw_bci` stamp reaching the interpreter's handler search and
/// running a `finally`; its codegen twin can only assert bytes. It also
/// needs the OPTIMIZING tier specifically — the single-pass backend and the
/// interpreter were always correct here — so it opens with an anti-vacuity
/// assertion and reports FAILED rather than passing when the tier declines
/// its probe.
///
/// On 2026-08-17 this rule landed and declined it, and the test sat red for
/// four days saying "this run proves nothing" while nobody read it. The e2e
/// test cannot prevent that recurrence: it needs a built binary and a JDK,
/// so it is skippable and slow. THIS test needs neither and runs in
/// `cargo test -p cratonvm-jit`.
///
/// Both shapes are pinned, because only the pair says the probe is admitted
/// for the right reason rather than because the rule stopped working.
#[test]
fn the_throw_bci_probe_body_stays_admissible_to_the_optimizing_tier() {
    // `IrExceptionStubThrowBciProbe.body(int, int[])`, javac 25, verbatim:
    //
    //   0: aload_1  1: iconst_0  2: aload_1  3: iconst_0  4: iaload
    //   5: iconst_1 6: iadd      7: iastore  8: iload_0
    //   9: invokestatic #16
    //  12: aload_1 13: iconst_0 14: aload_1 15: iconst_0 16: iaload
    //  17: iconst_1 18: isub    19: iastore 20: goto 34
    //  23: astore_2 24: aload_1 25: iconst_0 26: aload_1 27: iconst_0
    //  28: iaload  29: iconst_1 30: isub    31: iastore
    //  32: aload_2 33: athrow   34: return
    let body = [
        0x2b, 0x03, 0x2b, 0x03, 0x2e, 0x04, 0x60, 0x4f, 0x1a, 0xb8, 0x00, 0x10, 0x2b, 0x03, 0x2b,
        0x03, 0x2e, 0x04, 0x64, 0x4f, 0xa7, 0x00, 0x0e, 0x3d, 0x2b, 0x03, 0x2b, 0x03, 0x2e, 0x04,
        0x64, 0x4f, 0x2c, 0xbf, 0xb1,
    ];
    let table = |start: u16, end: u16| {
        vec![cratonvm_reader::attribute::ExceptionTableEntry {
            start_pc: start,
            end_pc: end,
            handler_pc: 23,
            // 0 = catch-all, i.e. the `finally` whose region does not span
            // the method — the property the defect needs.
            catch_type: 0,
        }]
    };

    // The shape the probe carries TODAY: the increment is hoisted above the
    // `try`, so the range covers `iload_0` + `invokestatic` and nothing the
    // IR tier lowers to a deopt guard.
    assert_eq!(
        ir_unresumable_protected_trap(&body, body.len(), &table(8, 12)),
        None,
        "the throw-bci probe's `body` must stay admissible to the optimizing \
         tier — `vm/tests/jit_ir_exception_stub_throw_bci.rs` proves nothing \
         about the IR exception stub without it. If this rule must decline \
         the shape, the probe needs rewriting in the same commit, NOT a \
         relaxed precondition."
    );

    // The shape it carried BEFORE 2026-08-21, with `n[0] = n[0] + 1` inside
    // the `try`: range 0..12 catches the `iaload` at 4 next to the `iastore`
    // at 7 and the invoke at 9. Pinned so this test cannot pass because the
    // rule stopped recognising anything.
    assert_eq!(
        ir_unresumable_protected_trap(&body, body.len(), &table(0, 12)),
        Some((4, 0x2e)),
        "the pre-2026-08-21 probe shape must still be DECLINED — if it is \
         not, this rule has stopped working and the row above is vacuous"
    );
}

/// RBC.6's admission list must match what the lowerings actually publish.
///
/// The failure mode this pins is asymmetric. Admitting an opcode whose
/// lowering does NOT publish hands a handler a frame nobody wrote — a
/// miscompile that only surfaces when an exception really crosses that
/// site. Excluding one that DOES publish is invisible: the method just
/// stays interpreted forever, which is how `getfield`/`putfield` sat for
/// five days after their capability landed, and `getstatic`/`checkcast`
/// sat until 2026-08-11 while they held down Tomcat's WebSocket send path.
#[test]
// x86-64 only: the API under test (`first_unsupported_precise_frame_site`) is itself
// `#[cfg(target_arch = "x86_64")]`, so on aarch64 this test does not
// merely fail -- it does not COMPILE, and took the whole crate's test
// binary with it. Found by actually building for aarch64 in an emulated
// container; a cfg-gated API needs cfg-gated tests.
#[cfg(target_arch = "x86_64")]
fn rbc6_admits_exactly_the_opcodes_whose_lowerings_publish() {
    // Publishes via `emit_post_invoke_exception_check` (reason-9) on every
    // throwing path, or cannot throw at all.
    for op in [
        0xb2u8, // getstatic  — helper arm publishes; inline arm needs an
        //           already-initialized class and so cannot throw
        0xb4, // getfield
        0xb5, // putfield
        0xb6, // invokevirtual
        0xb7, // invokespecial
        0xb8, // invokestatic
        0xb9, // invokeinterface
        0xbb, // new         — every non-scalar-replaced arm funnels through
        //           `emit_post_alloc_oom_check`, which publishes; the
        //           scalar-replaced arm emits no call and cannot throw
        0xbf, // athrow      — protected sites jump to the reason-9 stub
        0xc0, // checkcast   — single arm, always publishes
        0xc2, // monitorenter
        0xc3, // monitorexit
        0x12, // ldc         — String/Class arms end in `emit_post_alloc_oom_check`
        //           (the same publishing guard `new` uses); a numeric constant is
        //           a bare push and cannot throw
        0x13, // ldc_w       — the same lowering, 2-byte index
        0xbe, // arraylength — its in-place null check publishes (round 9);
              //           the hoisted-in-a-loop shape is refused by
              //           `first_unsupported_precise_frame_site` itself
    ] {
        assert!(
            super::precise_frame_publishing_opcode(op),
            "opcode {op:#04x} publishes a precise frame but is not admitted —                  every method with one in a protected range stays interpreted"
        );
    }

    // Do NOT publish. `aastore` (a reference store's type check, deliberately absent from
    // `precise_frame_publishing_opcode`), the integer divide and `multianewarray` are the
    // sites still on `first_unsupported_precise_frame_site`'s named list. (`ldc` was here
    // until 2026-09-19 on a claim that it reached its helpers "through the shared sentinel
    // stub"; both emitters end in `emit_post_alloc_oom_check`, which records a reason-9
    // frame at a protected bci. `arraylength` was here until round 9.)
    for op in [
        0x53u8, // aastore
        0x6c,   // idiv
        0xc5,   // multianewarray
    ] {
        assert!(
            !super::precise_frame_publishing_opcode(op),
            "opcode {op:#04x} does not publish a precise frame — admitting it                  hands a handler a frame that was never written"
        );
    }
}

/// The candidate set and the exemption are different questions.
///
/// `getstatic`/`checkcast` stay in `may_throw_without_precise_frame` (they
/// really can throw); what changed is that
/// `precise_frame_publishing_opcode` now exempts them. A protected
/// `checkcast` must therefore no longer refuse a compile, while a protected
/// `arraylength` still must.
#[test]
// x86-64 only: the API under test (`first_unsupported_precise_frame_site`) is itself
// `#[cfg(target_arch = "x86_64")]`, so on aarch64 this test does not
// merely fail -- it does not COMPILE, and took the whole crate's test
// binary with it. Found by actually building for aarch64 in an emulated
// container; a cfg-gated API needs cfg-gated tests.
#[cfg(target_arch = "x86_64")]
fn rbc6_gate_clears_getstatic_checkcast_and_straight_line_arraylength() {
    use cratonvm_reader::attribute::ExceptionTableEntry;
    // One protected range covering the whole body.
    let table = [ExceptionTableEntry {
        start_pc: 0,
        end_pc: 4,
        handler_pc: 4,
        catch_type: 0,
    }];
    // `checkcast #0` (3 bytes) then `nop`.
    let checkcast = [0xc0u8, 0x00, 0x00, 0x00];
    assert_eq!(
        super::first_unsupported_precise_frame_site(&checkcast, checkcast.len(), &table),
        None,
        "a protected checkcast must no longer refuse the compile"
    );
    // `getstatic #0` (3 bytes) then `nop`.
    let getstatic = [0xb2u8, 0x00, 0x00, 0x00];
    assert_eq!(
        super::first_unsupported_precise_frame_site(&getstatic, getstatic.len(), &table),
        None,
        "a protected getstatic must no longer refuse the compile"
    );
    // Straight-line `aload_0; arraylength` — admitted since round 9: its
    // in-place null check publishes a precise frame.
    let arraylength = [0x2au8, 0xbe, 0x00, 0x00];
    assert_eq!(
        super::first_unsupported_precise_frame_site(&arraylength, arraylength.len(), &table),
        None,
        "a straight-line protected arraylength publishes and must not refuse"
    );
    // The same pair inside a loop is admitted too (round 9 wave 3/4): under
    // precise frames `x64/driver.rs` drops any `ArrayLenHoist` with a
    // protected site, so the arraylength keeps its publishing in-loop check.
    // 0: aload_0; 1: arraylength; 2: pop; 3: goto -3 (-> 0); 6: nop
    let looped = [0x2au8, 0xbe, 0x57, 0xa7, 0xff, 0xfd, 0x00];
    let loop_table = [ExceptionTableEntry {
        start_pc: 0,
        end_pc: 6,
        handler_pc: 6,
        catch_type: 0,
    }];
    assert_eq!(
        super::first_unsupported_precise_frame_site(&looped, looped.len(), &loop_table),
        None,
        "a protected aload; arraylength inside a loop is admitted: the driver keeps it in the loop under precise frames"
    );
}

/// The exact bytecode shape RBC.6 was refusing on netty's
/// `AdaptivePoolingAllocator$Magazine.allocate`, both ways round.
///
/// `throw new IllegalStateException()` inside a `try` is
/// `new`/`dup`/`invokespecial`/`athrow` — four bytecodes, two of which were
/// unadmitted. The method was reported refused at the `new` (pc=338,
/// op=0xbb); admitting only that opcode would have moved the refusal to the
/// `athrow` seven bytes later and measured nothing, which is why the two
/// landed together.
///
/// The negative half is what makes this test non-vacuous: with the
/// admission withdrawn the same bytes MUST refuse, and refuse at the `new`.
/// Without it a gate that had quietly become unconditional would read green.
#[test]
// x86-64 only: the API under test (`first_unsupported_precise_frame_site`) is itself
// `#[cfg(target_arch = "x86_64")]`, so on aarch64 this test does not
// merely fail -- it does not COMPILE, and took the whole crate's test
// binary with it. Found by actually building for aarch64 in an emulated
// container; a cfg-gated API needs cfg-gated tests.
#[cfg(target_arch = "x86_64")]
fn rbc6_admits_a_protected_throw_new_and_refuses_it_when_withdrawn() {
    use cratonvm_reader::attribute::ExceptionTableEntry;
    // pc 0: new #0        (3 bytes)
    // pc 3: dup           (1)
    // pc 4: invokespecial (3)
    // pc 7: athrow        (1)
    let code = [0xbbu8, 0x00, 0x00, 0x59, 0xb7, 0x00, 0x00, 0xbf];
    let table = [ExceptionTableEntry {
        start_pc: 0,
        end_pc: 8,
        handler_pc: 8,
        catch_type: 0,
    }];
    assert_eq!(
        super::first_unsupported_precise_frame_site(&code, code.len(), &table),
        None,
        "a protected `throw new X()` must compile — this is the netty              AdaptivePoolingAllocator$Magazine.allocate shape"
    );

    // Prove the RED. `precise_alloc_athrow_enabled` caches in a `OnceLock`,
    // so this asks the predicate the same question the walk does rather
    // than setting the env var (which a sibling test in this process may
    // already have latched).
    let withdrawn = |op: u8| -> bool {
        if matches!(op, 0xbb | 0xbf) {
            return false;
        }
        super::precise_frame_publishing_opcode(op)
    };
    assert!(
        !withdrawn(0xbb) && !withdrawn(0xbf),
        "the withdrawal must take both opcodes out — withdrawing one leaves              the other holding the same method down"
    );
    assert!(
        withdrawn(0xb7),
        "the withdrawal must not disturb invokespecial, which sits between              them in this very sequence"
    );
}

/// The exact bytecode shape that kept `ConcurrentHashMap.putVal` interpreted.
///
/// `throw new IllegalStateException("Recursive update")` inside `synchronized
/// (f)` is `new`/`dup`/`ldc`/`invokespecial`/`athrow`, and once `new` and
/// `athrow` were admitted the `ldc` between them was the one bytecode left, so
/// the refusal moved to it (`pc=372, op=0x12`) and `putVal` stayed
/// interpreted in every mode. `ldc` and `ldc_w` are checked in both index
/// widths, and a protected `idiv` is checked to still refuse so the
/// admission cannot have become unconditional.
#[test]
// x86-64 only, for the reason the two tests above give.
#[cfg(target_arch = "x86_64")]
fn rbc6_admits_a_protected_throw_new_with_a_message_constant() {
    use cratonvm_reader::attribute::ExceptionTableEntry;
    // pc 0: new #0        (3 bytes)
    // pc 3: dup           (1)
    // pc 4: ldc #0        (2)      -- the site that was refused
    // pc 6: invokespecial (3)
    // pc 9: athrow        (1)
    let code = [0xbbu8, 0x00, 0x00, 0x59, 0x12, 0x00, 0xb7, 0x00, 0x00, 0xbf];
    let table = [ExceptionTableEntry {
        start_pc: 0,
        end_pc: 10,
        handler_pc: 10,
        catch_type: 0,
    }];
    assert_eq!(
        super::first_unsupported_precise_frame_site(&code, code.len(), &table),
        None,
        "a protected `throw new X(\"msg\")` must compile — this is the          ConcurrentHashMap.putVal shape"
    );
    // The two-byte-index form is a different opcode and a different length.
    let wide = [
        0xbbu8, 0x00, 0x00, 0x59, 0x13, 0x00, 0x00, 0xb7, 0x00, 0x00, 0xbf,
    ];
    let wide_table = [ExceptionTableEntry {
        start_pc: 0,
        end_pc: 11,
        handler_pc: 11,
        catch_type: 0,
    }];
    assert_eq!(
        super::first_unsupported_precise_frame_site(&wide, wide.len(), &wide_table),
        None,
        "ldc_w publishes through the same guard and must not refuse"
    );
    // Not vacuous: a site that really does not publish still refuses, at its own pc.
    let idiv = [0x12u8, 0x00, 0x6c, 0x00];
    let idiv_table = [ExceptionTableEntry {
        start_pc: 0,
        end_pc: 4,
        handler_pc: 4,
        catch_type: 0,
    }];
    assert_eq!(
        super::first_unsupported_precise_frame_site(&idiv, idiv.len(), &idiv_table),
        Some((2, 0x6c)),
        "an ldc must not mask a protected idiv behind it"
    );
}

/// A `String` access site must pin its method to the single-pass backend.
///
/// Every call-site intrinsic is registered in exactly ONE place — the
/// single-pass invoke loop at the end of `try_compile_inner` — and the IR
/// lowerer binds direct calls to real function addresses only, so it has no
/// route to an intrinsic sentinel. A method that tiers up to the optimizing
/// tier therefore silently LOSES its intrinsics, and the loss is invisible:
/// the resolver, the codegen ladder and this file's own String-intrinsic
/// tests all stay green while `java/nio/StringCharBuffer.get()C` runs at
/// 504 ns/call instead of 135.
///
/// The foil matters as much as the positive case: a method with no String
/// site must NOT be pinned, or this gate would quietly demote the whole
/// program to C1.
#[test]
fn a_string_access_site_pins_its_method_to_the_single_pass_backend() {
    let layout = super::StringFieldLayout::new(0, Some(1), 2, 7);
    let resolve = |idx: u16| -> Option<(String, String, String)> {
        match idx {
            1 => Some((
                "java/lang/String".to_string(),
                "charAt".to_string(),
                "(I)C".to_string(),
            )),
            _ => Some((
                "java/lang/Math".to_string(),
                "sqrt".to_string(),
                "(D)D".to_string(),
            )),
        }
    };
    // invokevirtual @pc=0 -> cp #1 (String.charAt), invokestatic @pc=3.
    let with_string: [(usize, u16, u8); 2] = [(0, 1, 0xb6), (3, 2, 0xb8)];
    assert!(
        super::has_string_intrinsic_site(&with_string, Some(&resolve), Some(layout)),
        "a String.charAt call site must keep its method on the backend that              can emit the inline decode"
    );

    // No String site: the method must stay eligible for the optimizing
    // tier. `Math.sqrt` IS an intrinsic, but a static one both the IR and
    // OSR ladders bind themselves — it is not what this gate is about.
    let plain: [(usize, u16, u8); 1] = [(3, 2, 0xb8)];
    assert!(
        !super::has_string_intrinsic_site(&plain, Some(&resolve), Some(layout)),
        "a method with no String access site must not be demoted to C1"
    );

    // No resolved layout ⇒ the single-pass backend would not emit the
    // intrinsic either, so pinning would cost a C2 body and buy nothing.
    assert!(
        !super::has_string_intrinsic_site(&with_string, Some(&resolve), None),
        "without a String layout there is no intrinsic to preserve"
    );
}

/// The two `None` inputs are DIFFERENT facts, and the pin now says which.
///
/// `has_string_intrinsic_site` answers `false` — "do not pin" — for "no
/// String site here", for "no layout resolved" and for "no resolver at this
/// door" alike, so the admission line could not tell a method the pin
/// declined from one the pin never saw. That is why five separate
/// hypotheses about which methods keep the inline `charAt` decode were each
/// refuted by their own measurement without converging on an answer. These
/// assertions are the whole difference, and the reason the arms may not be
/// collapsed back into a bool.
///
/// Deliberately env-independent: `Pinned` vs `DisabledByFlag` turns on
/// `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN`, which any other test in the
/// process can be holding, and a test that reads the ambient environment is
/// a flake with a good excuse.
#[test]
fn a_missing_pin_input_is_reported_as_a_blind_spot_not_as_no_site() {
    let layout = super::StringFieldLayout::new(0, Some(1), 2, 7);
    let resolve = |idx: u16| -> Option<(String, String, String)> {
        match idx {
            1 => Some((
                "java/lang/String".to_string(),
                "charAt".to_string(),
                "(I)C".to_string(),
            )),
            _ => Some((
                "java/lang/Math".to_string(),
                "sqrt".to_string(),
                "(D)D".to_string(),
            )),
        }
    };
    // invokevirtual @pc=0 -> cp #1 (String.charAt), invokestatic @pc=3.
    let with_string: [(usize, u16, u8); 2] = [(0, 1, 0xb6), (3, 2, 0xb8)];
    // invokestatic only — nothing the registration path would ever consider.
    let plain: [(usize, u16, u8); 1] = [(3, 2, 0xb8)];

    assert_eq!(
        super::string_intrinsic_pin_verdict(&with_string, Some(&resolve), None),
        super::StringPinVerdict::BlindNoLayout,
        "a String-declared site with no resolved layout is a blind spot, not an absence"
    );
    assert_eq!(
        super::string_intrinsic_pin_verdict(&with_string, None, Some(layout)),
        super::StringPinVerdict::BlindNoResolver,
        "with no resolver not one callee name is readable, so nothing can be concluded"
    );
    // The narrowing that keeps this diagnostic from becoming noise: a method
    // with no `invokevirtual`/`invokeinterface` at all is not blind, it has
    // nothing to be blind about. Without this, "no layout at this door"
    // would print against essentially every method in the program.
    assert_eq!(
        super::string_intrinsic_pin_verdict(&plain, Some(&resolve), None),
        super::StringPinVerdict::NoSite,
    );
    assert_eq!(
        super::string_intrinsic_pin_verdict(&plain, None, Some(layout)),
        super::StringPinVerdict::NoSite,
    );

    // Each blind arm names the input that was missing, because "the pin did
    // not fire" and "the pin could not look" want different fixes.
    assert!(super::StringPinVerdict::BlindNoLayout
        .admission_note()
        .is_some_and(|n| n.contains("String field layout")));
    assert!(super::StringPinVerdict::BlindNoResolver
        .admission_note()
        .is_some_and(|n| n.contains("constant-pool invoke resolver")));
    assert!(super::StringPinVerdict::NoSite.admission_note().is_none());
}

/// Two VMs, two answers — the property JDK-ONLY-WAVE2 §2 was filed about.
///
/// `direct_native_helper`'s policy input used to be the process-global
/// `JIT_COMPATIBILITY_MODE` latch, which only ever moved toward strict. A
/// `Compatible` VM sharing a process with a `JdkOnly` one therefore lost
/// the thin direct-call helpers: the strict VM latched, and every later
/// compilation in the process — whosever it was — got `0` back.
///
/// This test could not have been written against that design. There was no
/// per-call policy to vary, and the (since deleted) latch setter's doc
/// explicitly forbade latching from a unit test
/// in this crate precisely because it was irreversible and `mod tests`
/// shares one binary. Now the policy is an argument, so the two cases are
/// independent by construction and a test can simply ask for both.
#[test]
fn direct_native_helper_answers_per_vm_not_per_process() {
    // A registered helper address. `build_helpers` now publishes these
    // unconditionally; whether they may be BOUND is the policy question
    // below, and it is asked per compilation.
    let entry: usize = 0xdead_beef;

    // No resolver: strict refuses and records. This is also the §4
    // fail-closed case — a compile with no way to ask the registry cannot
    // bake a native in front of real bytes.
    let before = jdk_only_direct_native_refusals();
    assert_eq!(
        direct_native_helper(
            entry,
            true,
            None,
            "java/lang/Integer",
            "valueOf",
            "(I)Ljava/lang/Integer;"
        ),
        0,
        "a JdkOnly compilation with no resolver must not bind a thin direct-call helper"
    );
    assert!(
        jdk_only_direct_native_refusals() > before,
        "the refusal must be counted, not silent"
    );

    // Compatible still binds — and critically, does so AFTER the strict
    // call above. Under the latch this is exactly the assertion that
    // failed, because the strict answer poisoned the process.
    assert_eq!(
        direct_native_helper(
            entry,
            false,
            None,
            "java/lang/Integer",
            "valueOf",
            "(I)Ljava/lang/Integer;"
        ),
        0xdead_beef,
        "a Compatible compilation lost its helper because another VM was strict"
    );

    // And the unset sentinel is still the unset sentinel in both modes.
    let entry: usize = 0;
    assert_eq!(direct_native_helper(entry, false, None, "c", "m", "()V"), 0);
    assert_eq!(direct_native_helper(entry, true, None, "c", "m", "()V"), 0);
}

/// JDK-ONLY-WAVE2 §4: under `JdkOnly` the bind decision is the registry's
/// `NativeKind`, not a hard-coded triple list.
///
/// §1.4 names `Intrinsic` as the reviewed exception that MAY shadow
/// concrete bytecode. The pre-2026-08-06 gate refused all seven ladders
/// wholesale, which is stricter than the contract: measured against
/// `scripts/baselines/jdk-only-kind-map-25-linux.tsv`, three of them
/// (`StringLatin1.toLowerCase`, `Integer.valueOf(I)`, `Integer.intValue`)
/// are registered `Intrinsic`.
#[test]
fn direct_native_helper_asks_the_registry_not_a_name_list() {
    let entry: usize = 0xfeed_face;

    let intrinsic = |_c: &str, _m: &str, _d: &str| true;
    let bridge = |_c: &str, _m: &str, _d: &str| false;

    // Intrinsic: §1.4's reviewed exception, so it binds even under strict.
    assert_eq!(
        direct_native_helper(
            entry,
            true,
            Some(&intrinsic),
            "java/lang/Integer",
            "valueOf",
            "(I)Ljava/lang/Integer;"
        ),
        0xfeed_face,
        "a reviewed Intrinsic must still bind under JdkOnly (§1.4)"
    );

    // Bridge (and SyntheticStub, and unregistered — the resolver answers
    // `false` for all three): refused, and recorded.
    let before = jdk_only_direct_native_refusals();
    assert_eq!(
        direct_native_helper(
            entry,
            true,
            Some(&bridge),
            "java/util/HashMap",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
        ),
        0,
        "a Bridge must not shadow concrete bytecode under JdkOnly"
    );
    assert!(jdk_only_direct_native_refusals() > before);

    // Compatible is unchanged by the kind either way — the resolver is
    // only consulted on the strict arm.
    assert_eq!(
        direct_native_helper(
            entry,
            false,
            Some(&bridge),
            "java/util/HashMap",
            "put",
            "()V"
        ),
        0xfeed_face
    );
}

/// H7-1 — the gate that is the ONLY thing keeping the interpreter and
/// compiled code from answering a map lookup two different ways under
/// `--jdk-only`, pinned per triple.
///
/// `H4-1` O1 called the six collection direct helpers "blocking" because
/// their failure mode is a tier-dependent wrong answer no arm diffs for.
/// The reason strict mode is nonetheless consistent today is entirely this
/// function: all four triples the collection ladders ask about are
/// `bridge` in `scripts/baselines/jdk-only-kind-map-25-linux.tsv`, so the
/// `Intrinsic`-only admission refuses every one and the call falls to the
/// generic, policy-checked dispatcher — the same route the interpreter
/// takes. **That fact is load-bearing and was written down nowhere.**
///
/// The second half is the part that would have rotted: the ladders ask
/// about the triple named at the CALL SITE, which for the two
/// `invokeinterface` arms is `java/util/Map.get` and
/// `java/util/concurrent/ConcurrentMap.get` — not the `java/util/HashMap`
/// / `java/util/concurrent/ConcurrentHashMap` row whose native the helper
/// actually runs. Both rows in each pair are `bridge` today, so the two
/// questions currently agree; a retag moving only the interface row to
/// `Intrinsic` would have bound a helper whose implementation is still a
/// `Bridge`, under strict mode, silently. `direct_native_helper_for_impl`
/// closes that, and the last block here is the executable statement of it.
#[test]
fn strict_mode_refuses_every_collection_direct_helper() {
    let entry: usize = 0x0c0f_fee0;

    // What the registry says today for the four triples the collection
    // ladders name. Source: scripts/baselines/jdk-only-kind-map-25-linux.tsv
    // (rows 6554, 6560, 6851, 7602), all `bridge`.
    // "Is this triple a reviewed `Intrinsic`?" — `false` for all four,
    // because all four are `bridge`. Written as a constant `false` rather
    // than a name list so the test cannot drift into asserting a list it
    // maintains itself; the TSV is the source and it is cited above.
    let as_measured_today = |_c: &str, _m: &str, _d: &str| -> bool { false };

    const COLLECTION_LADDER_TRIPLES: [(&str, &str, &str); 4] = [
        // `jit_hashmap_get_direct`, invokevirtual arm.
        (
            "java/util/HashMap",
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
        ),
        // `jit_hashmap_put_direct`.
        (
            "java/util/HashMap",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        ),
        // `jit_hashmap_get_direct`, invokeinterface arm — the INTERFACE.
        (
            "java/util/Map",
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
        ),
        // `jit_concurrent_hashmap_get_direct` — also the INTERFACE.
        (
            "java/util/concurrent/ConcurrentMap",
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
        ),
    ];

    for (class, method, descriptor) in COLLECTION_LADDER_TRIPLES {
        let before = jdk_only_direct_native_refusals();
        assert_eq!(
            direct_native_helper(
                entry,
                true,
                Some(&as_measured_today),
                class,
                method,
                descriptor
            ),
            0,
            "{class}.{method}{descriptor} is a Bridge: binding it under \
             --jdk-only would give compiled code a second entry into the \
             map's state while the interpreter took the bytecode route"
        );
        assert!(
            jdk_only_direct_native_refusals() > before,
            "{class}.{method}{descriptor} must be REFUSED, not merely unbound \
             — a silent zero is indistinguishable from an unwired cell"
        );
        // …and compatible mode still binds it, which is why this whole
        // family is a compatible-mode concern and moves strict mode by
        // exactly zero.
        assert_eq!(
            direct_native_helper(
                entry,
                false,
                Some(&as_measured_today),
                class,
                method,
                descriptor
            ),
            0x0c0f_fee0,
            "{class}.{method}{descriptor} must still bind under --real-jdk"
        );
    }

    // Retagging ONLY the interface row must NOT open the door: the helper
    // this ladder binds runs `native_hashmap_get_exact`, registered on
    // `java/util/HashMap`, which is still a `Bridge`.
    let interface_only_intrinsic =
        |class: &str, _m: &str, _d: &str| -> bool { class == "java/util/Map" };
    let before = jdk_only_direct_native_refusals();
    assert_eq!(
        direct_native_helper_for_impl(
            entry,
            true,
            Some(&interface_only_intrinsic),
            "java/util/Map",
            "java/util/HashMap",
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;"
        ),
        0,
        "an Intrinsic on the INTERFACE row must not admit a helper whose \
         implementing row is still a Bridge"
    );
    assert!(
        jdk_only_direct_native_refusals() > before,
        "the implementing-row refusal must be counted too"
    );

    // Both rows Intrinsic: §1.4's reviewed exception genuinely covers the
    // code that runs, so it binds.
    let both_intrinsic = |class: &str, _m: &str, _d: &str| -> bool {
        matches!(class, "java/util/Map" | "java/util/HashMap")
    };
    assert_eq!(
        direct_native_helper_for_impl(
            entry,
            true,
            Some(&both_intrinsic),
            "java/util/Map",
            "java/util/HashMap",
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;"
        ),
        0x0c0f_fee0,
        "when BOTH rows are reviewed Intrinsics the bind is admitted"
    );

    // And when the site class already IS the implementing class, the
    // second question is skipped rather than asked twice. That property is
    // asserted through the RESOLVER's own call count rather than through
    // `jdk_only_direct_native_refusals`, which is a process-global that
    // every other test in this binary also moves — an exact delta on it
    // would be a race, not a check.
    let asked = std::cell::Cell::new(0usize);
    let counting = |_c: &str, _m: &str, _d: &str| -> bool {
        asked.set(asked.get() + 1);
        false
    };
    assert_eq!(
        direct_native_helper_for_impl(
            entry,
            true,
            Some(&counting),
            "java/util/HashMap",
            "java/util/HashMap",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
        ),
        0
    );
    assert_eq!(
        asked.get(),
        1,
        "site class == implementing class: the registry must be asked once, not twice"
    );
}
/// Poll `cond` until it holds, for up to ~1s.
///
/// Several globals these tests observe are *eventually* consistent, not
/// immediately so, and cargo runs this binary's tests on a thread pool, so
/// a single sample races every other test in the process:
///
///  * `jit_code_region_covering` is a `try_lock` and returns
///    `JitRegionLookup::Locked` — a documented, legitimate answer — while
///    any other thread is registering or unregistering a buffer.
///  * `defer_jit_owner` does NOT drop a superseded artifact while
///    `ACTIVE_JIT_EXECUTIONS != 0`; it queues it. So a body whose last
///    owner this test released stays alive until whichever unrelated test
///    is currently executing JIT code returns.
///
/// Both resolve on their own, so retry rather than serialising the suite.
/// This does NOT paper over a wrong answer: a condition that is genuinely
/// false stays false for the whole window and still fails the assertion.
fn eventually(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if cond() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    cond()
}

/// [`eventually`], for a condition that waits on the **deferred retirement
/// queue** — "this published body was released", "this artifact's last
/// owner went away".
///
/// The difference is that this one also CAUSES the event it is waiting
/// for. `defer_jit_owner` queues an artifact rather than freeing it while
/// `ACTIVE_JIT_EXECUTIONS != 0`, and the queue is drained only by
/// `jit_execution_leave` observing the counter reach zero. That counter is
/// process-global, so in a test binary the drain is somebody else's job:
/// whichever sibling test happens to leave JIT execution last. Polling
/// alone therefore waits on an event that may simply not happen — if this
/// test is the last one running, nothing is left to trigger it, and the
/// assertion fails for a reason that has nothing to do with what it is
/// checking. That is what
/// `test_inline_cache_reclamation_waits_for_jit_quiescence` was failing on,
/// about 1 run in 12 at `--test-threads=32`.
///
/// An `enter`/`leave` pair on a thread holding no compiled frame is inert
/// except that its `leave` re-runs the quiescence check, so pumping it
/// between probes turns "wait for someone else" into "make it happen once
/// the process is actually quiescent".
///
/// It does NOT paper over a wrong answer: a body that is genuinely still
/// owned stays owned for the whole window, exactly as with [`eventually`].
fn eventually_drained(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if cond() {
            return true;
        }
        jit_execution_leave(jit_execution_enter());
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    cond()
}

use super::*;

#[test]
fn dynamic_dispatch_cache_eligibility_is_independent_of_recursion_metadata() {
    assert!(invoke_kind_uses_inline_cache(0), "invokevirtual");
    assert!(invoke_kind_uses_inline_cache(2), "invokeinterface");
    assert!(!invoke_kind_uses_inline_cache(1), "invokespecial");
    assert!(!invoke_kind_uses_inline_cache(3), "invokestatic");
}

/// BUG-1 companion — routing of NON-tail static self-recursive call sites.
///
/// WITHOUT the caller-supplied identity proof the site must retain
/// `invoke_dispatch`, so class-loader identity is resolved at dispatch
/// time (dee2e26f hardening). WITH the proof
/// (`CompileRequest::self_call_identity_stable` — builtin-loaded class whose
/// name maps back to its own ClassId) the site takes the raw guarded
/// direct self-CALL (bt18-regression fix, 2026-07-18); the flag is
/// consume-once so the NEXT compile reverts to dispatch.
/// Proven from the emitted machine code: `emit_call_absolute` bakes the
/// helper address as a `MOV RAX, imm64`, so the 8-byte LE address pattern
/// appearing in the code identifies which helper the site calls.
#[test]
// x86-64 only: this asserts how `try_compile_inner` ROUTES a compile
// (single-pass vs IR vs dispatch), and on another architecture that
// function takes its `#[cfg(target_arch = "aarch64")]` branch and
// returns before any of that routing happens. Gated so the crate's
// test run is green on aarch64 rather than carrying known reds.
#[cfg(target_arch = "x86_64")]
fn self_recursive_nontail_site_routes_through_dispatch() {
    use std::sync::Arc;

    // `static int f(int n) { return n <= 0 ? 0 : f(n - 1) + 1; }`
    //  0: iload_0
    //  1: ifgt  -> 6
    //  4: iconst_0
    //  5: ireturn
    //  6: iload_0
    //  7: iconst_1
    //  8: isub
    //  9: invokestatic #1   (self — NON-tail: iadd follows)
    // 12: iconst_1
    // 13: iadd
    // 14: ireturn
    let code: &[u8] = &[
        0x1a, 0x9d, 0x00, 0x05, 0x03, 0xac, 0x1a, 0x04, 0x64, 0xb8, 0x00, 0x01, 0x04, 0x60, 0xac,
    ];
    let cached = CachedBytecodeMethod {
        declaring_class_id: cratonvm_types::ClassId::new(1),
        class_name: Arc::from("pkg/Rec"),
        method_name: Arc::from("f"),
        method_descriptor: Arc::from("(I)I"),
        source_file: None,
        code: Arc::from(code),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 3,
        max_locals: 1,
        num_params: 1,
        is_synchronized: false,
        is_static: true,
        force_native_cache: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    };
    let resolver = |idx: u16| -> Option<(String, String, String)> {
        (idx == 1).then(|| ("pkg/Rec".to_string(), "f".to_string(), "(I)I".to_string()))
    };
    // Distinctive fake addresses — the code is never executed, only
    // pattern-searched. SAFETY (zeroed): all-usize #[repr(C)] struct.
    const GUARD_ADDR: usize = 0x7161_7264_5f61_6472; // "qard_adr"-ish tag
    const DISPATCH_ADDR: usize = 0x6469_7370_5f61_6472;
    let contains = |hay: &[u8], addr: usize| -> bool {
        let needle = (addr as u64).to_le_bytes();
        hay.windows(needle.len()).any(|w| w == needle)
    };

    // (a) Guard wired: loader-correct dispatch is still required.
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the test wires only the slots it exercises.
    let mut helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    helpers.self_call_stack_guard = GUARD_ADDR;
    helpers.invoke_dispatch = DISPATCH_ADDR;
    let compiled = try_compile(
        &cached,
        None,
        None,
        None,
        Some(&resolver),
        None,
        None,
        None,
        None,
        None,
        &helpers,
        None,
        None,
        None,
        None,
        false,
        false,
        false,
        false,
        false,
        false,
        None,
    )
    .expect("self-recursive method must compile through dispatch");
    let bytes = compiled.code_bytes().to_vec();
    assert!(
        !contains(&bytes, GUARD_ADDR),
        "non-tail self-call must not bake a raw self-entry guard"
    );
    assert!(
        contains(&bytes, DISPATCH_ADDR),
        "non-tail self-call must use invoke_dispatch"
    );

    // (b) Guard UNWIRED → historical dispatch routing.
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the test wires only the slots it exercises.
    let mut helpers_off: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    helpers_off.invoke_dispatch = DISPATCH_ADDR;
    let compiled_off = try_compile(
        &cached,
        None,
        None,
        None,
        Some(&resolver),
        None,
        None,
        None,
        None,
        None,
        &helpers_off,
        None,
        None,
        None,
        None,
        false,
        false,
        false,
        false,
        false,
        false,
        None,
    )
    .expect("guard-unwired self-recursive method must compile");
    let bytes_off = compiled_off.code_bytes().to_vec();
    assert!(
        contains(&bytes_off, DISPATCH_ADDR),
        "unwired guard must keep the historical invoke_dispatch routing"
    );

    // (c) bt18-regression fix: with the caller-supplied identity proof,
    // the non-tail site takes the raw guarded direct self-CALL — the
    // stack-guard helper is baked and no invoke_dispatch round trip
    // remains for the recursion.
    let compiled_direct = try_compile_request(&CompileRequest {
        cp_invoke_resolver: Some(&resolver),
        self_call_identity_stable: true,
        ..CompileRequest::new(&cached, &helpers)
    })
    .expect("identity-proven self-recursive method must compile");
    let bytes_direct = compiled_direct.code_bytes().to_vec();
    assert!(
        contains(&bytes_direct, GUARD_ADDR),
        "identity-proven non-tail self-call must bake the self-call stack guard"
    );
    assert!(
        !contains(&bytes_direct, DISPATCH_ADDR),
        "identity-proven non-tail self-call must not round-trip through invoke_dispatch"
    );

    // (d) The proof is consume-once: the very next compile reverts to
    // loader-correct dispatch routing.
    let compiled_after = try_compile(
        &cached,
        None,
        None,
        None,
        Some(&resolver),
        None,
        None,
        None,
        None,
        None,
        &helpers,
        None,
        None,
        None,
        None,
        false,
        false,
        false,
        false,
        false,
        false,
        None,
    )
    .expect("post-proof compile must fall back to dispatch");
    assert!(
        contains(&compiled_after.code_bytes().to_vec(), DISPATCH_ADDR),
        "identity proof must not leak into the next compile"
    );
}

/// deopt-osr Step 9 follow-up (a): `stamp_deopt_epoch_guard` writes the
/// creation epoch + live-cell pointer into the artifact's retained guard, and
/// the resulting guard reports superseded once the live epoch advances past
/// the creation epoch. A null guard (production artifact) is a safe no-op.
#[test]
fn fua_stamp_deopt_epoch_guard_and_supersede() {
    use std::sync::atomic::{AtomicU64, Ordering};
    let mut buf = ExecutableBuffer::new(64).unwrap();
    buf.emit(&[0xC3]); // ret
    let mut cm = CompiledMethod::new(buf);

    // No guard emitted (production): stamping is a no-op and must not panic.
    assert!(cm.deopt_epoch_guard.is_null());
    cm.stamp_deopt_epoch_guard(5, std::ptr::null());

    // Attach a retained guard (as `emit_deopt_stubs` would) and a stable live
    // cell (as `method_epochs` would).
    let guard: &'static crate::deopt::DeoptEpochGuard =
        Box::leak(Box::new(crate::deopt::DeoptEpochGuard::new()));
    cm.deopt_epoch_guard = guard as *const _;
    let live: &'static AtomicU64 = Box::leak(Box::new(AtomicU64::new(3)));

    // Install at the current live epoch (3) ⇒ fresh, not superseded.
    cm.stamp_deopt_epoch_guard(3, live as *const _);
    assert_eq!(guard.creation_epoch.load(Ordering::Relaxed), 3);
    assert!(!guard.is_superseded());

    // A later invalidation advances the live epoch past 3 ⇒ superseded.
    live.store(4, Ordering::Relaxed);
    assert!(guard.is_superseded());
}

// ── wire-tiered-manager Step 3: per-call C1/C2 backend toggle ──────────
//
// `optimize=true` (C2) must take the optimizing IR pipeline; `optimize=false`
// (the fast C1 tier) must skip it and route to the single-pass `x64::compile`
// backend. Proven via the thread-local `IR_LOWER_COMPILES` counter, which the
// IR-lowering success path bumps. The counter is thread-local and each cargo
// `#[test]` runs on its own thread, so parallel compile tests can't perturb it.
#[test]
// x86-64 only: this asserts how `try_compile_inner` ROUTES a compile
// (single-pass vs IR vs dispatch), and on another architecture that
// function takes its `#[cfg(target_arch = "aarch64")]` branch and
// returns before any of that routing happens. Gated so the crate's
// test run is green on aarch64 rather than carrying known reds.
#[cfg(target_arch = "x86_64")]
fn step3_optimize_toggle_routes_c1_singlepass_and_c2_ir() {
    // ROUTING, not acceptance. The C1->C2 acceptance gate
    // (`ir_evidence`) refuses a body that applied no transform the
    // baseline tier lacks, which every method in this test is --
    // they are three-bytecode probes. Forcing `Always` keeps this a
    // test of one thing.
    let _accept = crate::ir_evidence::AcceptAlways::on();
    // This test exercises the optimizing IR pipeline, which is gated off
    // whenever the young generation can relocate. Pin the policy so the
    // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
    crate::x64::set_moving_young_override(Some(false));
    use std::sync::Arc;

    // `static int add(int a, int b) { return a + b; }`
    //   iload_0 (0x1a); iload_1 (0x1b); iadd (0x60); ireturn (0xac)
    // Pure int arithmetic → ir_compatible, no category-2, no branch/call/field.
    let cached = CachedBytecodeMethod {
        declaring_class_id: cratonvm_types::ClassId::new(1),
        class_name: Arc::from("pkg/Add"),
        method_name: Arc::from("add"),
        method_descriptor: Arc::from("(II)I"),
        source_file: None,
        code: Arc::from([0x1a, 0x1b, 0x60, 0xac].as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 2,
        max_locals: 2,
        num_params: 2,
        is_synchronized: false,
        is_static: true,
        force_native_cache: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    };
    // SAFETY: every `JitRuntimeHelpers` field is a `usize` and the struct is
    // `#[repr(C)]`, so an all-zero bit pattern is valid (no niches/padding).
    // `add` references no runtime helper, and the test never executes the
    // generated machine code, so the null helper addresses are never called.
    let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };

    // C2 — optimize=true → optimizing IR pipeline.
    IR_LOWER_COMPILES.with(|c| c.set(0));
    let c2 = try_compile(
        &cached, None, None, None, None, None, None, None, None, None, &helpers, None, None, None,
        None, true, false, false, false, false, false,
        None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
    );
    let c2_used_ir = IR_LOWER_COMPILES.with(|c| c.get());
    assert!(c2.is_some(), "optimize=true (C2) must compile `add`");
    assert_eq!(
        c2_used_ir, 1,
        "optimize=true (C2) must route `add` through the IR pipeline"
    );

    // C1 — optimize=false → single-pass x64 backend, IR pipeline skipped.
    IR_LOWER_COMPILES.with(|c| c.set(0));
    let c1 = try_compile(
        &cached, None, None, None, None, None, None, None, None, None, &helpers, None, None, None,
        None, false, false, false, false, false, false,
        None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
    );
    let c1_used_ir = IR_LOWER_COMPILES.with(|c| c.get());
    assert!(
        c1.is_some(),
        "optimize=false (C1) must still compile `add` via the single-pass backend"
    );
    assert_eq!(
        c1_used_ir, 0,
        "optimize=false (C1) must NOT take the IR pipeline"
    );

    // Both tiers produced runnable native code.
    assert!(!c1.unwrap().code_bytes().is_empty());
    assert!(!c2.unwrap().code_bytes().is_empty());
}

// ── activate-ir-optimizer step 3: int-category `getfield` → `Op::Load` ──
//
// A method whose only heap op is an int-field read must now take the
// optimizing IR pipeline (the builder lowers `getfield` to `Op::Load`).
// Proven via `IR_LOWER_COMPILES`: a vacuous fall-through to single-pass
// would leave the counter at 0. (The integration harness
// `ir_vs_singlepass.rs` proves the *executed* result is correct; this
// proves the IR path — not single-pass — produced the body.)
#[test]
// x86-64 only: this asserts how `try_compile_inner` ROUTES a compile
// (single-pass vs IR vs dispatch), and on another architecture that
// function takes its `#[cfg(target_arch = "aarch64")]` branch and
// returns before any of that routing happens. Gated so the crate's
// test run is green on aarch64 rather than carrying known reds.
#[cfg(target_arch = "x86_64")]
fn step3_getfield_int_routes_through_ir() {
    use std::sync::Arc;

    // `static int get(Corpus o) { return o.x; }`
    //   aload_0 (0x2a); getfield #2 (0xb4 0x00 0x02); ireturn (0xac)
    let cached = CachedBytecodeMethod {
        declaring_class_id: cratonvm_types::ClassId::new(1),
        class_name: Arc::from("pkg/Corpus"),
        method_name: Arc::from("get"),
        method_descriptor: Arc::from("(Lpkg/Corpus;)I"),
        source_file: None,
        // Trailing 0x00 0x00: the VM pads bytecode with two bytes that
        // `try_compile` strips via `code.len() - 2`; without them the
        // `ireturn` is truncated away.
        code: Arc::from([0x2a, 0xb4, 0x00, 0x02, 0xac, 0x00, 0x00].as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 2,
        max_locals: 1,
        num_params: 1,
        is_synchronized: false,
        is_static: true,
        force_native_cache: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    };
    // SAFETY: see `step3_optimize_toggle_…`; an all-zero `JitRuntimeHelpers`
    // is valid and never called (the inline getfield emits no helper call,
    // and this test does not execute the generated code).
    let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    // Resolve cp index 2 → field index 0, int (`I`).
    // (field_index, type_tag, compact_slot) — `None` = the field has no
    // registered compact slot (a plain non-compact int field).
    let field_resolver = |cp: u16| -> Option<(usize, u8, Option<(u32, bool)>)> {
        if cp == 2 {
            Some((0, b'I', None))
        } else {
            None
        }
    };

    // C2 — optimize=true → IR pipeline lowers `getfield` to `Op::Load`.
    IR_LOWER_COMPILES.with(|c| c.set(0));
    let c2 = try_compile(
        &cached,
        None,
        Some(&field_resolver),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        &helpers,
        None,
        None,
        None,
        None,
        true,
        false,
        false,
        false,
        false,
        false,
        None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
    );
    assert!(c2.is_some(), "optimize=true (C2) must compile `get`");
    let expected_ir_compiles = if cratonvm_types::compact_ref_fields_enabled() {
        0
    } else {
        1
    };
    assert_eq!(
        IR_LOWER_COMPILES.with(|c| c.get()),
        expected_ir_compiles,
        "compact field layout bails to single-pass; legacy layout routes int getfield through IR"
    );

    // Without the field resolver the builder cannot resolve the field, so
    // the IR path must bail (counter stays 0). The whole compile then
    // returns None — single-pass also needs the resolver to build
    // `field_info` — which is the expected, safe fallback.
    IR_LOWER_COMPILES.with(|c| c.set(0));
    let _ = try_compile(
        &cached, None, None, None, None, None, None, None, None, None, &helpers, None, None, None,
        None, true, false, false, false, false, false,
        None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
    );
    assert_eq!(
        IR_LOWER_COMPILES.with(|c| c.get()),
        0,
        "an unresolved getfield must NOT take the IR pipeline"
    );
}

// ── activate-ir-optimizer inc 16: EA bridge handles full-layout ops ──
//
// Escape analysis scalar-replaces a non-escaping allocation by killing the
// `Op::New` and its field stores and redirecting field loads to the stored
// value. That only works if the IR→EA bridge translates the *production*
// full-layout `Op::Load`/`Op::Store` (`[ctrl, mem, base, offset, value]`,
// `MemKind`-tagged) into the EA's compact `[holder]`/`[holder, value]`
// layout with the real field index from the `Const` offset operand — the
// exact thing inc 16 fixed. This builds such a graph by hand (the builder
// does not emit `Op::New` yet) and proves the round-trip.
#[test]
fn ea_bridge_scalar_replaces_full_layout_new_store_load() {
    use crate::ir::{Graph, IrType, MemKind, Op, NO_NODE};

    // Object o = new Foo(); o.f1 = 42; return o.f1;  (single int field at
    // index 1, to also exercise a non-zero field index vs MemKind::Int=0).
    let mut g = Graph {
        nodes: Vec::new(),
        entry: 0,
        exit: NO_NODE,
        safepoints: Vec::new(),
        uses: Default::default(),
        receiver_param: None,
    };
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let newobj = g.add(
        Op::New {
            class_id: 7,
            num_fields: 2,
        },
        IrType::Ref,
        vec![ctrl, mem],
        None,
    );
    let val = g.add(Op::Const(42), IrType::Int, vec![], None);
    let off = g.add(Op::Const(1), IrType::Int, vec![], None); // field index 1
    let store = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, mem, newobj, off, val],
        None,
    );
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, store, newobj, off],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, load], None);
    g.exit = ret;

    let (ea, id_map) = escape_analysis_from_ir(&g);
    let result = escape_analysis::analyze_escapes(&ea);
    assert!(
        !result.scalar_replaceable.is_empty(),
        "a non-escaping New with a matching field store/load must be \
         scalar-replaceable once the bridge resolves the full layout"
    );

    apply_ea_to_ir(&mut g, &id_map, &result);
    assert_eq!(g.nodes[newobj as usize].op, Op::Dead, "New killed");
    assert_eq!(g.nodes[store as usize].op, Op::Dead, "Store killed");
    assert_eq!(g.nodes[load as usize].op, Op::Dead, "Load killed");
    assert_eq!(
        g.nodes[ret as usize].inputs[1], val,
        "the load result must be redirected to the stored value (Const 42)"
    );
}

// A New that escapes (returned by reference) must NOT be scalar-replaced —
// the bridge fix preserves the escape rule (a missed escape would scalar-
// replace an object a real use still needs: the kafka bug-25 class).
#[test]
fn ea_bridge_keeps_escaping_new() {
    use crate::ir::{Graph, IrType, MemKind, Op, NO_NODE};

    let mut g = Graph {
        nodes: Vec::new(),
        entry: 0,
        exit: NO_NODE,
        safepoints: Vec::new(),
        uses: Default::default(),
        receiver_param: None,
    };
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let newobj = g.add(
        Op::New {
            class_id: 7,
            num_fields: 1,
        },
        IrType::Ref,
        vec![ctrl, mem],
        None,
    );
    let val = g.add(Op::Const(42), IrType::Int, vec![], None);
    let off = g.add(Op::Const(0), IrType::Int, vec![], None);
    let _store = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, mem, newobj, off, val],
        None,
    );
    // Return the *reference* — the object escapes globally.
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, newobj], None);
    g.exit = ret;

    let (ea, _id_map) = escape_analysis_from_ir(&g);
    let result = escape_analysis::analyze_escapes(&ea);
    assert!(
        result.scalar_replaceable.is_empty(),
        "an escaping New must not be scalar-replaceable"
    );
}

// A non-escaping New whose field is LOADED but never STORED is scalar-
// replaced, and the load of that field must resolve to the zero default
// (`Const(0)`) of the freshly-allocated object — NOT be killed without a
// replacement (the latent `apply_ea_to_ir` bug). Soundness rests on the
// object being zero-initialised (the caller only admits allocations whose
// constructor sets no non-zero field).
#[test]
fn ea_unstored_field_load_resolves_to_zero_default() {
    use crate::ir::{Graph, IrType, MemKind, Op, NO_NODE};

    let mut g = Graph {
        nodes: Vec::new(),
        entry: 0,
        exit: NO_NODE,
        safepoints: Vec::new(),
        uses: Default::default(),
        receiver_param: None,
    };
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let newobj = g.add(
        Op::New {
            class_id: 7,
            num_fields: 2,
        },
        IrType::Ref,
        vec![ctrl, mem],
        None,
    );
    let val = g.add(Op::Const(42), IrType::Int, vec![], None);
    let off0 = g.add(Op::Const(0), IrType::Int, vec![], None);
    let off1 = g.add(Op::Const(1), IrType::Int, vec![], None);
    let store = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, mem, newobj, off0, val],
        None,
    );
    // Load field 1 — never stored.
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, store, newobj, off1],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, load], None);
    g.exit = ret;

    let (ea, id_map) = escape_analysis_from_ir(&g);
    let result = escape_analysis::analyze_escapes(&ea);
    assert!(
        !result.scalar_replaceable.is_empty(),
        "a non-escaping new is scalar-replaceable even with an un-stored field"
    );
    apply_ea_to_ir(&mut g, &id_map, &result);
    assert!(
        !g.nodes.iter().any(|n| matches!(n.op, Op::New { .. })),
        "the New is scalar-replaced away"
    );
    let ret_node = g
        .nodes
        .iter()
        .find(|n| matches!(n.op, Op::Return))
        .expect("a Return");
    let retval = ret_node.inputs[1];
    assert_eq!(
        g.nodes[retval as usize].op,
        Op::Const(0),
        "the un-stored field's load must resolve to the zero default"
    );
}

// ── apply_ea_to_ir: safepoints and the memory-token chain ────────────
//
// `apply_ea_to_ir` runs after `ir_optimize::optimize` and is the last
// mutator before `ir_schedule::schedule`, so nothing downstream repairs a
// snapshot slot it strands or a memory-token chain it breaks. The four tests
// below pin both.

/// `Object o = new Foo(); o.f1 = 42; return o.f1;` in the PRODUCTION full
/// layout (`[ctrl, mem, base, offset, value]`), with no safepoints yet.
struct EaFixture {
    g: crate::ir::Graph,
    mem: crate::ir::NodeId,
    newobj: crate::ir::NodeId,
    val: crate::ir::NodeId,
    store: crate::ir::NodeId,
    load: crate::ir::NodeId,
}

fn ea_full_layout_fixture() -> EaFixture {
    use crate::ir::{Graph, IrType, MemKind, Op, NO_NODE};

    let mut g = Graph {
        nodes: Vec::new(),
        entry: 0,
        exit: NO_NODE,
        safepoints: Vec::new(),
        uses: Default::default(),
        receiver_param: None,
    };
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let newobj = g.add(
        Op::New {
            class_id: 7,
            num_fields: 2,
        },
        IrType::Ref,
        vec![ctrl, mem],
        None,
    );
    let val = g.add(Op::Const(42), IrType::Int, vec![], None);
    let off = g.add(Op::Const(1), IrType::Int, vec![], None); // field index 1
    let store = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, mem, newobj, off, val],
        None,
    );
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, store, newobj, off],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, load], None);
    g.entry = start;
    g.exit = ret;
    EaFixture {
        g,
        mem,
        newobj,
        val,
        store,
        load,
    }
}

fn run_ea(g: &mut crate::ir::Graph) {
    let (ea, id_map) = escape_analysis_from_ir(g);
    let result = escape_analysis::analyze_escapes(&ea);
    assert!(
        !result.scalar_replaceable.is_empty(),
        "fixture precondition: the non-escaping New is scalar-replaceable"
    );
    apply_ea_to_ir(g, &id_map, &result);
}

/// No live node may take its memory token from a removed node — the
/// invariant `ir_verify`'s memory-chain lane reports on, asserted directly so
/// the test does not depend on which lanes that module currently enables.
fn assert_memory_chain_intact(g: &crate::ir::Graph) {
    for (idx, n) in g.nodes.iter().enumerate() {
        if n.op == crate::ir::Op::Dead {
            continue;
        }
        for (i, &inp) in n.inputs.iter().enumerate() {
            if inp == crate::ir::NO_NODE || !ea_is_memory_token_slot(n, i) {
                continue;
            }
            let target = g
                .nodes
                .get(inp as usize)
                .unwrap_or_else(|| panic!("n{idx} memory token n{inp} is out of range"));
            assert_ne!(
                target.op,
                crate::ir::Op::Dead,
                "n{idx}:{:?} takes its memory token from removed node n{inp}",
                n.op
            );
        }
    }
}

/// Run the verifier's frame-state lane (and only it, on top of the always-on
/// structural lane) over a post-EA graph.
fn assert_frame_state_lane_clean(g: &crate::ir::Graph) {
    let mut opts = crate::ir_verify::VerifyOptions::structural();
    opts.check_frame_states = true;
    if let Err(b) = crate::ir_verify::verify_graph(g, "post-escape-analysis", opts) {
        panic!("frame-state lane rejected a post-apply_ea_to_ir graph: {b}");
    }
}

// A scalar-replaced object that is LIVE ACROSS A SAFEPOINT either
// materialises correctly or is not scalar-replaced. With
// `CRATONVM_SCALAR_DEOPT` unset (the default, and what the test harness
// runs with) no `FrameValue::VirtualObject` descriptor is emitted, so
// killing the `Op::New` would leave the snapshot slot resolving to
// `FrameValue::Undefined` — `Value::Int(0)`, i.e. a NULL where a live object
// was. The pass refuses the allocation elision; the field load is still
// forwarded, so the cost is one allocation, not the optimization.
#[test]
fn ea_refuses_to_elide_an_allocation_a_safepoint_names() {
    use crate::ir::{Op, SafepointSnapshot, NO_NODE};

    let mut f = ea_full_layout_fixture();
    f.g.safepoints.push(SafepointSnapshot {
        bci: 3,
        locals: vec![NO_NODE],
        stack: vec![f.newobj],
        monitors: Vec::new(),
    });
    run_ea(&mut f.g);

    // The premise — "no virtual-object descriptor will be emitted for it" —
    // is the DEFAULT path's, and `CRATONVM_SCALAR_DEOPT` is the flag that
    // makes it false. With the flag on the refusal this test is named for
    // does not apply and the opposite is the correct answer, so assert that
    // instead of asserting the default's behaviour into a build that does
    // not have it.
    if scalar_deopt_descriptor_available() {
        assert_eq!(
            f.g.nodes[f.newobj as usize].op,
            Op::Dead,
            "with a descriptor available the snapshot is no longer a reason \
             to keep the allocation"
        );
        return;
    }
    assert!(
        matches!(f.g.nodes[f.newobj as usize].op, Op::New { .. }),
        "the allocation must survive: a deopt snapshot names it and no \
         virtual-object descriptor will be emitted for it"
    );
    assert!(
        matches!(f.g.nodes[f.store as usize].op, Op::Store(_)),
        "the initialising store must survive with the allocation, or the \
         surviving object would reach deopt with an unwritten field"
    );
    assert_eq!(
        f.g.nodes[f.load as usize].op,
        Op::Dead,
        "the field load is still forwarded to the stored value"
    );
    let ret = f.g.nodes[f.g.exit as usize].inputs[1];
    assert_eq!(ret, f.val, "the load's consumer reads the stored constant");
    assert_eq!(
        f.g.safepoints[0].stack[0], f.newobj,
        "the snapshot slot still names the (live) allocation"
    );
    assert_memory_chain_intact(&f.g);
    assert_frame_state_lane_clean(&f.g);
}

// The mirror case: no snapshot slot names the allocation, so eliding it
// cannot strand a frame state and the full scalar replacement applies.
#[test]
fn ea_elides_an_allocation_no_safepoint_names() {
    use crate::ir::{Op, SafepointSnapshot, NO_NODE};

    let mut f = ea_full_layout_fixture();
    f.g.safepoints.push(SafepointSnapshot {
        bci: 3,
        locals: vec![NO_NODE],
        stack: vec![f.val],
        monitors: Vec::new(),
    });
    run_ea(&mut f.g);

    assert_eq!(f.g.nodes[f.newobj as usize].op, Op::Dead, "New killed");
    assert_eq!(f.g.nodes[f.store as usize].op, Op::Dead, "Store killed");
    assert_eq!(f.g.nodes[f.load as usize].op, Op::Dead, "Load killed");
    assert_eq!(
        f.g.nodes[f.g.exit as usize].inputs[1], f.val,
        "the load result is redirected to the stored value"
    );
    assert_memory_chain_intact(&f.g);
    assert_frame_state_lane_clean(&f.g);
}

// A snapshot slot naming a SCALAR-REPLACED LOAD must follow the value, the
// way `ir::Graph::replace_all_uses` does for every other rewrite. The pass
// used to rewrite `graph.nodes` only, so the slot kept naming the killed
// load and the deopt frame rebuilt that local from `FrameValue::Undefined`.
#[test]
fn ea_safepoint_slot_naming_a_replaced_load_follows_the_value() {
    use crate::ir::{Op, SafepointSnapshot, NO_NODE};

    let mut f = ea_full_layout_fixture();
    f.g.safepoints.push(SafepointSnapshot {
        bci: 13,
        locals: vec![f.load],
        stack: vec![NO_NODE],
        monitors: Vec::new(),
    });
    run_ea(&mut f.g);

    assert_eq!(
        f.g.nodes[f.load as usize].op,
        Op::Dead,
        "the load is killed"
    );
    assert_eq!(
        f.g.safepoints[0].locals[0], f.val,
        "the snapshot slot follows the load to its replacement value"
    );
    assert_memory_chain_intact(&f.g);
    assert_frame_state_lane_clean(&f.g);
}

// Killing a store in this pass must leave the memory-token chain intact: the
// next memory operation is spliced onto the killed store's OWN incoming
// token, not left naming an `Op::Dead` node (and not, as the old code did,
// rewritten to the killed load's *data* replacement — an ordering edge
// silently turned into a data edge).
#[test]
fn ea_killed_store_leaves_the_memory_chain_intact() {
    use crate::ir::{IrType, MemKind, Op};

    let mut f = ea_full_layout_fixture();
    // A later, unrelated field read whose memory token is the (about to be
    // killed) load: `[ctrl, mem=load, base=param, offset]`.
    let ctrl = f.g.nodes[f.newobj as usize].inputs[0];
    let param = f.g.add(Op::Param(0), IrType::Ref, vec![], None);
    let off0 = f.g.add(Op::Const(0), IrType::Int, vec![], None);
    let trailing = f.g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, f.load, param, off0],
        None,
    );

    run_ea(&mut f.g);

    assert_eq!(f.g.nodes[f.store as usize].op, Op::Dead, "Store killed");
    assert_eq!(f.g.nodes[f.load as usize].op, Op::Dead, "Load killed");
    assert_eq!(
        f.g.nodes[trailing as usize].inputs[1], f.mem,
        "the trailing load is spliced onto the chain the killed store/load \
         inherited from, not left pointing into the hole"
    );
    assert_ne!(
        f.g.nodes[trailing as usize].inputs[1], f.val,
        "a memory token must never be rewritten to a data replacement"
    );
    assert_memory_chain_intact(&f.g);
}

// ── Wiring the finished escape-analysis work ─────────────────────────
//
// `docs/jit/escape-analysis.md` §6 and `docs/jit/lock-elimination.md` §6
// list four consumer-side edits. The tests below pin the three that changed
// behaviour here (all-or-nothing lock elision, per-load forwarding, the
// identity bridge) plus the cold-path producer that makes
// `EscapeState::PartialEscape` reachable at all.

/// `[ctrl, mem, obj, off, val]` full-layout graph with a *stand-in* monitor
/// pair on a returned (therefore escaping) object, and a deopt snapshot
/// naming the second of the pair. Returns `(graph, obj, m_enter, m_exit)`.
///
/// The fixture predates `ir::Op::MonitorEnter`/`MonitorExit` and keeps
/// ordinary nodes as stand-ins. Nothing is lost: `apply_ea_to_ir`'s lock
/// loop asks three questions of a monitor — is it snapshot-named, is its
/// memory token spliceable, is its value still read — and none of the three
/// is monitor-specific. Two ordinary `Op::Store`s answer them the same way.
fn ea_lock_group_fixture() -> (
    crate::ir::Graph,
    crate::ir::NodeId,
    crate::ir::NodeId,
    crate::ir::NodeId,
) {
    use crate::ir::{Graph, IrType, MemKind, Op, SafepointSnapshot, NO_NODE};

    let mut g = Graph {
        nodes: Vec::new(),
        entry: 0,
        exit: NO_NODE,
        safepoints: Vec::new(),
        uses: Default::default(),
        receiver_param: None,
    };
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let newobj = g.add(
        Op::New {
            class_id: 7,
            num_fields: 1,
        },
        IrType::Ref,
        vec![ctrl, mem],
        None,
    );
    let off = g.add(Op::Const(0), IrType::Int, vec![], None);
    let val = g.add(Op::Const(42), IrType::Int, vec![], None);
    // The stand-in monitor pair. `m_enter` passes every check
    // `apply_ea_to_ir` makes (its token is `mem`, and `m_exit` names it only
    // in a TOKEN slot, so it has no value use); `m_exit` fails exactly one,
    // `ea_snapshot_names`.
    let m_enter = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, mem, newobj, off, val],
        None,
    );
    let m_exit = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, m_enter, newobj, off, val],
        None,
    );
    // Publishing the object keeps scalar replacement out of this test: only
    // the lock path can touch these nodes.
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, newobj], None);
    g.entry = start;
    g.exit = ret;
    g.safepoints.push(SafepointSnapshot {
        bci: 3,
        locals: vec![NO_NODE],
        stack: vec![m_exit],
        monitors: Vec::new(),
    });
    (g, newobj, m_enter, m_exit)
}

// A lock group is ALL-OR-NOTHING PER OBJECT.
//
// `apply_ea_to_ir` used to filter the flat `elide_locks` list per NODE: a
// monitor a safepoint slot named was skipped and its siblings were killed
// anyway. That is how a balanced monitor sequence becomes a `monitorexit`
// whose `monitorenter` is gone — `IllegalMonitorStateException` — or a
// monitor held past the end of the frame. It now reads the grouped
// `lock_elisions`, where one refusal refuses the whole object.
//
// This is the edit `docs/jit/lock-elimination.md` §6.1 requires to land
// BEFORE monitor ops are bridged.
#[test]
fn ea_a_partially_refusable_lock_group_is_refused_whole() {
    use crate::ir::Op;

    let (mut g, newobj, m_enter, m_exit) = ea_lock_group_fixture();
    let (ea, id_map) = escape_analysis_from_ir(&g);
    let mut result = escape_analysis::analyze_escapes(&ea);
    assert!(
        result.scalar_replaceable.is_empty(),
        "precondition: the returned object escapes, so scalar replacement \
         cannot be what kills (or spares) these nodes"
    );

    // The offer the analysis would make once monitors exist: both nodes,
    // grouped under one object.
    let ea_enter = id_map[m_enter as usize];
    let ea_exit = id_map[m_exit as usize];
    result.lock_elisions = vec![escape_analysis::LockElisionPlan {
        object: id_map[newobj as usize],
        monitors: vec![ea_enter, ea_exit],
    }];
    result.elide_locks = vec![ea_enter, ea_exit];

    apply_ea_to_ir(&mut g, &id_map, &result);

    assert_ne!(
        g.nodes[m_exit as usize].op,
        Op::Dead,
        "the snapshot-named monitor is refused (unchanged behaviour)"
    );
    assert_ne!(
        g.nodes[m_enter as usize].op,
        Op::Dead,
        "THE REGRESSION THIS PINS: its sibling must be refused too. Killing \
         it alone leaves an unbalanced monitor sequence."
    );
    assert_memory_chain_intact(&g);
}

// The flat `elide_locks` list is no longer what drives the applier — it
// stays on `EscapeAnalysisResult` for diagnostics only. A result that offers
// monitors ONLY through the flat list must change nothing, because a flat
// list cannot express "these two go together".
#[test]
fn ea_the_flat_elide_locks_list_no_longer_drives_the_applier() {
    use crate::ir::Op;

    let (mut g, _newobj, m_enter, m_exit) = ea_lock_group_fixture();
    let (ea, id_map) = escape_analysis_from_ir(&g);
    let mut result = escape_analysis::analyze_escapes(&ea);
    result.lock_elisions.clear();
    result.elide_locks = vec![id_map[m_enter as usize], id_map[m_exit as usize]];

    apply_ea_to_ir(&mut g, &id_map, &result);

    assert_ne!(g.nodes[m_enter as usize].op, Op::Dead);
    assert_ne!(g.nodes[m_exit as usize].op, Op::Dead);
}

// POSITIONAL LOAD FORWARDING. `Foo o = new Foo(); o.x = 7; int a = o.x;
// o.x = 42; return a;` must return 7.
//
// `info.field_values[0]` is 42 (the LAST store), and forwarding it here is
// the miscompilation this branch already paid for once. The applier used to
// guard against it with its own id-comparison heuristic — "refuse the object
// if any store to the loaded field has a higher node id than the load" —
// which is correct but refuses the optimization outright. It now asks
// `escape_analysis::ScalarReplacementInfo::load_value`, which answers the
// store that actually precedes THIS load, so the object is replaced *and*
// the load reads 7.
#[test]
fn ea_load_before_a_later_store_forwards_the_pre_store_value() {
    use crate::ir::{Graph, IrType, MemKind, Op, NO_NODE};

    let mut g = Graph {
        nodes: Vec::new(),
        entry: 0,
        exit: NO_NODE,
        safepoints: Vec::new(),
        uses: Default::default(),
        receiver_param: None,
    };
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let newobj = g.add(
        Op::New {
            class_id: 7,
            num_fields: 1,
        },
        IrType::Ref,
        vec![ctrl, mem],
        None,
    );
    let off = g.add(Op::Const(0), IrType::Int, vec![], None);
    let seven = g.add(Op::Const(7), IrType::Int, vec![], None);
    let forty_two = g.add(Op::Const(42), IrType::Int, vec![], None);
    // Node ids are creation order = program order, which is what the
    // analysis's dominance stand-in reads: store(7) < load < store(42).
    let store7 = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, mem, newobj, off, seven],
        None,
    );
    let load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, store7, newobj, off],
        None,
    );
    // A load consumes a memory token but produces none, so the second store
    // chains onto `store7`.
    let store42 = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, store7, newobj, off, forty_two],
        None,
    );
    let ret = g.add(Op::Return, IrType::Void, vec![ctrl, load], None);
    g.entry = start;
    g.exit = ret;

    let (ea, id_map) = escape_analysis_from_ir(&g);
    let result = escape_analysis::analyze_escapes(&ea);
    let info = result
        .scalar_replaceable
        .first()
        .expect("the non-escaping object is scalar-replaceable");
    assert!(
        info.dominance_proved,
        "precondition: a branch-free graph, so program order IS dominance"
    );
    assert_eq!(
        info.field_values[0],
        Some(id_map[forty_two as usize]),
        "precondition: `field_values` — the source the applier used to read \
         — names the LAST store's value, which is the wrong answer here"
    );

    apply_ea_to_ir(&mut g, &id_map, &result);

    let retval = g.nodes[ret as usize].inputs[1];
    assert_eq!(
        g.nodes[retval as usize].op,
        Op::Const(7),
        "the load must read the value the field held AT THE LOAD (7), not \
         the value it ends up holding (42)"
    );
    assert_eq!(g.nodes[load as usize].op, Op::Dead, "Load forwarded");
    assert_eq!(g.nodes[store7 as usize].op, Op::Dead, "Store(7) killed");
    assert_eq!(g.nodes[store42 as usize].op, Op::Dead, "Store(42) killed");
    assert_eq!(g.nodes[newobj as usize].op, Op::Dead, "New killed");
    assert_memory_chain_intact(&g);
}

// IDENTITY SENSITIVITY. An `if_acmpeq` on a fresh object observes its
// ADDRESS, so the object may not be scalar-replaced — two replaced objects
// with equal fields are indistinguishable, two heap objects are not.
//
// The outcome was already fail-closed before the bridge emitted
// `Op::RefCompare` (an `ir::Op::Cmp` hit `ir_op_to_ea_op`'s `Op::Other`
// catch-all, and the use walk refuses `Op::Other`), but it was invisible.
// `stats.identity_blocked` and `identity_observations` are what make the
// refusal reportable, and they are only populated when the observation is
// named for what it is.
#[test]
fn ea_an_identity_compared_object_is_not_replaced() {
    use crate::ir::{CmpOp, Graph, IrType, MemKind, Op, NO_NODE};

    // `null_rhs`: compare against `aconst_null` (an `ifnull`) instead of
    // against another reference.
    let build = |null_rhs: bool| -> (Graph, crate::ir::NodeId) {
        let mut g = Graph {
            nodes: Vec::new(),
            entry: 0,
            exit: NO_NODE,
            safepoints: Vec::new(),
            uses: Default::default(),
            receiver_param: None,
        };
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let newobj = g.add(
            Op::New {
                class_id: 7,
                num_fields: 1,
            },
            IrType::Ref,
            vec![ctrl, mem],
            None,
        );
        let off = g.add(Op::Const(0), IrType::Int, vec![], None);
        let val = g.add(Op::Const(42), IrType::Int, vec![], None);
        let _store = g.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![ctrl, mem, newobj, off, val],
            None,
        );
        let rhs = if null_rhs {
            // `IrBuilder::aconst_null` — an `Op::Const(0)` typed `Ref`.
            g.add(Op::Const(0), IrType::Ref, vec![], None)
        } else {
            g.add(Op::Param(0), IrType::Ref, vec![], None)
        };
        let cmp = g.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![newobj, rhs], None);
        let ret = g.add(Op::Return, IrType::Void, vec![ctrl, cmp], None);
        g.entry = start;
        g.exit = ret;
        (g, newobj)
    };

    // (a) `o == p` — a real address comparison.
    let (g, newobj) = build(false);
    let (ea, id_map) = escape_analysis_from_ir(&g);
    let result = escape_analysis::analyze_escapes(&ea);
    assert!(
        result.scalar_replaceable.is_empty(),
        "an identity-compared object must not be scalar-replaced"
    );
    assert_eq!(
        result.stats.identity_blocked, 1,
        "and the refusal must be ATTRIBUTED to identity: this counts only \
         CONFINED objects refused for that reason, so it also proves the \
         object was otherwise replaceable (a non-vacuous test)"
    );
    assert!(
        result
            .identity_observations
            .iter()
            .any(|&(alloc, _)| alloc == id_map[newobj as usize]),
        "the observation is reported, not merely acted on"
    );

    // (b) `o == null` — an `ifnull`, which the builder lowers to the SAME
    // `Op::Cmp` with two `Ref`-typed operands. A fresh allocation is
    // non-null by construction, so this reads no address; classifying it as
    // an identity observation would refuse every null-checked object in the
    // program.
    let (g, _) = build(true);
    let (ea, _) = escape_analysis_from_ir(&g);
    let result = escape_analysis::analyze_escapes(&ea);
    assert!(
        result.identity_observations.is_empty(),
        "a null check is not an identity observation"
    );
    assert_eq!(result.stats.identity_blocked, 0);
}

// COLD-PATH CLASSIFICATION. An object whose only escape site is on a
// profiled never-taken branch is reported `EscapeState::PartialEscape`.
//
// `escape_analysis::Graph::cold_nodes` had no producer, so `partial_escapes`
// was always empty and the whole lattice level was dead code. The producer
// is `ea_cold_control_nodes`, fed from the `ir_branch_hints` map
// `try_compile_inner` already computes for the lowerer.
#[test]
fn ea_a_cold_path_escape_is_classified_partial() {
    use crate::ir::{Graph, IrType, MemKind, Op, NO_NODE};
    use std::collections::HashMap;

    // Foo o = new Foo(); o.x = 42; if (c) escape(o);   — with `c` profiled
    // as never taken, so the call that publishes `o` is cold.
    let mut g = Graph {
        nodes: Vec::new(),
        entry: 0,
        exit: NO_NODE,
        safepoints: Vec::new(),
        uses: Default::default(),
        receiver_param: None,
    };
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let newobj = g.add(
        Op::New {
            class_id: 7,
            num_fields: 1,
        },
        IrType::Ref,
        vec![ctrl, mem],
        None,
    );
    let off = g.add(Op::Const(0), IrType::Int, vec![], None);
    let val = g.add(Op::Const(42), IrType::Int, vec![], None);
    let store = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, mem, newobj, off, val],
        None,
    );
    let cond = g.add(Op::Param(0), IrType::Int, vec![], None);
    // bci 10 is the key `ir_branch_hints` is looked up by.
    let iff = g.add(Op::If, IrType::Control, vec![ctrl, cond], Some(10));
    let t_edge = g.add(Op::Proj(0), IrType::Control, vec![iff], Some(10));
    let f_edge = g.add(Op::Proj(1), IrType::Control, vec![iff], Some(10));
    // The publication, on the TRUE edge. `info_ptr: 0` is rejected by
    // `ir_call_is_identity_hash` without a dereference, so this stays a
    // plain `escape_analysis::Op::Call`.
    let call = g.add(
        Op::Call { info_ptr: 0 },
        IrType::Void,
        vec![t_edge, store, newobj],
        Some(10),
    );
    let merge = g.add(Op::Merge, IrType::Control, vec![t_edge, f_edge], None);
    let ret = g.add(Op::Return, IrType::Void, vec![merge], None);
    g.entry = start;
    g.exit = ret;

    // (a) No profile — the historical behaviour, byte for byte.
    let (ea, _) = escape_analysis_from_ir(&g);
    let unprofiled = escape_analysis::analyze_escapes(&ea);
    assert!(
        unprofiled.partial_escapes.is_empty(),
        "with no cold-path producer the classification is unreachable"
    );

    // (b) The branch is profiled usually-NOT-taken, so its `Proj(0)` (the
    // branch target) is the cold edge and the call on it is cold.
    let hints: HashMap<usize, bool> = [(10usize, false)].into_iter().collect();
    let cold = ea_cold_control_nodes(&g, &hints);
    assert!(cold.contains(&t_edge), "the never-taken edge is cold");
    assert!(cold.contains(&call), "and so is everything pinned to it");
    assert!(
        !cold.contains(&f_edge) && !cold.contains(&merge) && !cold.contains(&ret),
        "a merge is cold only when EVERY predecessor is"
    );

    let (mut ea, id_map) = escape_analysis_from_ir(&g);
    ea_mark_cold_from_branch_hints(&mut ea, &g, &id_map, &hints);
    let result = escape_analysis::analyze_escapes(&ea);
    let pe = result
        .partial_escapes
        .first()
        .expect("the object escapes only on the cold path");
    assert_eq!(pe.alloc_node, id_map[newobj as usize]);
    assert_eq!(
        pe.without_refinement,
        escape_analysis::EscapeState::ArgEscape,
        "the unrefined state the connection graph keeps — every ACTING \
         consumer still sees this one"
    );
    assert_eq!(pe.escape_sites, vec![id_map[call as usize]]);
    assert_eq!(
        result.escape_states.get(&id_map[newobj as usize]),
        Some(&escape_analysis::EscapeState::PartialEscape)
    );
    assert!(
        result.scalar_replaceable.is_empty(),
        "PartialEscape is a REPORT and must not enable anything: the object \
         is still offered to no acting consumer"
    );
}

// ── Op::New emission + scalar replacement, end-to-end via the builder ──
//
// The builder lowers `new` to `Op::New` and elides a trivial `<init>` on a
// fresh object; escape analysis then scalar-replaces the non-escaping
// allocation (no heap alloc, the field load becomes the stored value). This
// drives the FULL path (build -> optimize -> EA -> apply) from bytecode.
#[test]
fn ir_new_scalar_replaces_end_to_end() {
    use crate::ir::{IrBuilder, Op};
    use std::collections::{HashMap, HashSet};

    // static int f() { Foo o = new Foo(); o.x = 42; return o.x; }
    //   0: new #1            bb 00 01
    //   3: dup               59
    //   4: invokespecial #2  b7 00 02   (Foo.<init>()V — trivial, elided)
    //   7: dup               59
    //   8: bipush 42         10 2a
    //  10: putfield #3       b5 00 03
    //  13: getfield #3       b4 00 03
    //  16: ireturn           ac
    let code = [
        0xbb, 0x00, 0x01, 0x59, 0xb7, 0x00, 0x02, 0x59, 0x10, 0x2a, 0xb5, 0x00, 0x03, 0xb4, 0x00,
        0x03, 0xac, 0x00, 0x00,
    ];
    let mut builder = IrBuilder::new(0, 1);
    let mut new_info = HashMap::new();
    new_info.insert(0usize, (7u32, 1usize)); // new @0: class 7, 1 field
    let mut init_pcs = HashSet::new();
    init_pcs.insert(4usize); // <init> @4 is trivial + elidable
    builder.set_new_info(new_info, init_pcs);
    let mut fi = HashMap::new();
    fi.insert(10usize, (0usize, b'I')); // putfield field 0
    fi.insert(13usize, (0usize, b'I')); // getfield field 0
    builder.set_field_info(fi);
    // The builder used to refuse getfield/putfield outright whenever
    // compact layout was on (the default), which made this test vacuous
    // in every default run. The layout constraint now lives in
    // `ir_lower::lower_inner`, where the layout-naive displacement is
    // actually emitted, so the scalar-replacement assertions below now
    // run for real.
    let mut graph = builder.build(&code, 17).expect("IR build");
    assert!(
        graph.nodes.iter().any(|n| matches!(n.op, Op::New { .. })),
        "builder must emit an Op::New for `new`"
    );

    ir_optimize::optimize(&mut graph);
    let (ea, id_map) = escape_analysis_from_ir(&graph);
    let result = escape_analysis::analyze_escapes(&ea);
    assert!(
        !result.scalar_replaceable.is_empty(),
        "the non-escaping new must be scalar-replaceable"
    );
    apply_ea_to_ir(&mut graph, &id_map, &result);

    // The FIELD ACCESS is still scalar-replaced: the load folds to the
    // stored constant.
    let ret = graph
        .nodes
        .iter()
        .find(|n| matches!(n.op, Op::Return))
        .expect("a Return");
    let retval = ret.inputs[1];
    assert_eq!(
        graph.nodes[retval as usize].op,
        Op::Const(42),
        "the field load must resolve to the stored value (42)"
    );
    // The ALLOCATION goes too, in every configuration (#49). The builder
    // records a snapshot at every bytecode boundary and the fresh reference
    // sits on the stack at several of them, but none of those is a program
    // point a deopt can arrive at: this body has no guard, call or other
    // transferring node left once the allocation, its store and its load
    // are retired. Until 2026-09-12 any snapshot slot naming the `Op::New`
    // kept it unless `CRATONVM_SCALAR_DEOPT` supplied a descriptor.
    assert!(
        !graph.nodes.iter().any(|n| matches!(n.op, Op::New { .. })),
        "no consultable snapshot names the allocation, so it is elided"
    );
    // The unconsultable snapshots still name the dead node until the
    // pipeline's prune drops them; after it, none does.
    ir_prune_unconsumable_snapshots(&mut graph, &[]);
    assert_no_snapshot_names_a_dead_node(&graph);
}

/// Every safepoint snapshot slot must name a live node — the condition
/// `ir_verify::check_frame_states` enforces, restated where the EA tests can
/// assert it directly.
fn assert_no_snapshot_names_a_dead_node(graph: &crate::ir::Graph) {
    for (si, sp) in graph.safepoints.iter().enumerate() {
        for (kind, i, s) in sp
            .locals
            .iter()
            .enumerate()
            .map(|(i, &s)| ("local", i, s))
            .chain(sp.stack.iter().enumerate().map(|(i, &s)| ("stack", i, s)))
        {
            if s == crate::ir::NO_NODE {
                continue;
            }
            let node = graph
                .nodes
                .get(s as usize)
                .unwrap_or_else(|| panic!("safepoint[{si}] {kind}[{i}] = n{s} out of range"));
            assert_ne!(
                node.op,
                crate::ir::Op::Dead,
                "safepoint[{si}] at bci {} {kind}[{i}] = n{s} names a removed node",
                sp.bci
            );
        }
    }
}

// REGRESSION (astore gap): the same scalar-replacement end-to-end, but the
// fresh object is round-tripped through a LOCAL via `astore`/`aload` — the
// shape REAL javac emits (`new; dup; invokespecial; astore_N; aload_N; …`).
// The `ir_new_scalar_replaces_end_to_end` test above keeps the ref on the
// stack via `dup`, so it never exercised `astore` — and the builder had NO
// `astore` handler, so EVERY production allocation method bailed to
// single-pass and `new` scalar replacement NEVER fired live (inc 17/19's
// "== HotSpot" probe was vacuous: it matches whether or not SR fires). With
// `astore` lowered, this folds to `Const(42)` exactly like the dup form.
#[test]
fn ir_new_scalar_replaces_through_astore_local() {
    use crate::ir::{IrBuilder, Op};
    use std::collections::{HashMap, HashSet};

    // static int f() { Foo o = new Foo(); o.x = 42; return o.x; }  (javac shape)
    //   0: new #1            bb 00 01
    //   3: dup               59
    //   4: invokespecial #2  b7 00 02   (Foo.<init>()V — elided)
    //   7: astore_0          4b
    //   8: aload_0           2a
    //   9: bipush 42         10 2a
    //  11: putfield #3       b5 00 03
    //  14: aload_0           2a
    //  15: getfield #3       b4 00 03
    //  18: ireturn           ac
    let code = [
        0xbb, 0x00, 0x01, 0x59, 0xb7, 0x00, 0x02, 0x4b, 0x2a, 0x10, 0x2a, 0xb5, 0x00, 0x03, 0x2a,
        0xb4, 0x00, 0x03, 0xac, 0x00, 0x00,
    ];
    let mut builder = IrBuilder::new(0, 1);
    let mut new_info = HashMap::new();
    new_info.insert(0usize, (7u32, 1usize)); // new @0: class 7, 1 field
    let mut init_pcs = HashSet::new();
    init_pcs.insert(4usize); // <init> @4 is trivial + elidable
    builder.set_new_info(new_info, init_pcs);
    let mut fi = HashMap::new();
    fi.insert(11usize, (0usize, b'I')); // putfield field 0
    fi.insert(15usize, (0usize, b'I')); // getfield field 0
    builder.set_field_info(fi);
    // The builder used to refuse getfield/putfield outright whenever
    // compact layout was on (the default), which made this test vacuous
    // in every default run. The layout constraint now lives in
    // `ir_lower::lower_inner`, where the layout-naive displacement is
    // actually emitted, so the scalar-replacement assertions below now
    // run for real.
    let mut graph = builder
        .build(&code, 19)
        .expect("IR build must succeed with astore lowered");
    assert!(
        graph.nodes.iter().any(|n| matches!(n.op, Op::New { .. })),
        "builder must emit an Op::New for `new`"
    );

    ir_optimize::optimize(&mut graph);
    let (ea, id_map) = escape_analysis_from_ir(&graph);
    let result = escape_analysis::analyze_escapes(&ea);
    assert!(
        !result.scalar_replaceable.is_empty(),
        "the astore-local non-escaping new must be scalar-replaceable"
    );
    apply_ea_to_ir(&mut graph, &id_map, &result);
    let ret = graph
        .nodes
        .iter()
        .find(|n| matches!(n.op, Op::Return))
        .expect("a Return");
    let retval = ret.inputs[1];
    assert_eq!(
        graph.nodes[retval as usize].op,
        Op::Const(42),
        "the field load (via astore/aload local) must resolve to the stored value (42)"
    );
    // Same rule as `ir_new_scalar_replaces_end_to_end` (#49): the object sits
    // in local 0 and on the stack at several snapshots, none of which a
    // deopt can consult once the allocation's own nodes are retired, so the
    // allocation is elided in every configuration.
    assert!(
        !graph.nodes.iter().any(|n| matches!(n.op, Op::New { .. })),
        "no consultable snapshot names the allocation, so it is elided"
    );
    ir_prune_unconsumable_snapshots(&mut graph, &[]);
    assert_no_snapshot_names_a_dead_node(&graph);
}

/// The pipeline between `IrBuilder::build` and scheduling, in
/// `try_compile_inner`'s order: dead-local pruning, the null-check fold, the
/// optimizer, every escape-analysis round (with the scalar-replacement map
/// when precise resume is on), and the unconsumable-snapshot prune.
fn run_ir_pipeline_through_escape_analysis(
    code: &[u8],
    code_len: usize,
    builder: crate::ir::IrBuilder,
) -> (crate::ir::Graph, Option<ir_lower::ScalarReplacementMap>) {
    let mut graph = builder.build(code, code_len).expect("IR build");
    ir_prune_dead_snapshot_locals(&mut graph, code, code_len);
    ir_fold_null_checks_on_fresh_allocations(&mut graph);
    ir_optimize::optimize(&mut graph);
    let mut sr_map: Option<ir_lower::ScalarReplacementMap> = None;
    let mut pinned: std::collections::HashSet<crate::ir::NodeId> = std::collections::HashSet::new();
    for _ in 0..3 {
        let (ea, id_map) = escape_analysis_from_ir(&graph);
        let result = escape_analysis::analyze_escapes(&ea);
        if result.scalar_replaceable.is_empty() && result.lock_elisions.is_empty() {
            break;
        }
        if scalar_deopt_descriptor_available() && !result.scalar_replaceable.is_empty() {
            let round = build_scalar_replacement_map(&graph, &id_map, &result, &pinned);
            for vo in round.objects.values() {
                for fv in vo.field_values.iter().flatten() {
                    pinned.insert(*fv);
                }
            }
            match sr_map.as_mut() {
                Some(acc) => acc.objects.extend(round.objects),
                None => sr_map = Some(round),
            }
        }
        if apply_ea_to_ir_pinned(&mut graph, &id_map, &result, &pinned) == 0 {
            break;
        }
    }
    ir_prune_unconsumable_snapshots(&mut graph, &[]);
    (graph, sr_map)
}

/// `static int f(int x)` with `Foo o = new Foo(); o.x = x;` and
/// `new_info`/`field_info` rows for the given putfield/getfield pcs.
fn foo_builder(num_locals: usize, putfield_pc: usize, getfield_pc: usize) -> crate::ir::IrBuilder {
    use std::collections::{HashMap, HashSet};
    let mut builder = crate::ir::IrBuilder::new(1, num_locals);
    let mut new_info = HashMap::new();
    new_info.insert(0usize, (7u32, 1usize)); // new @0: class 7, 1 field
    let mut init_pcs = HashSet::new();
    init_pcs.insert(4usize); // <init> @4 is trivial + elidable
    builder.set_new_info(new_info, init_pcs);
    let mut fi = HashMap::new();
    fi.insert(putfield_pc, (0usize, b'I'));
    fi.insert(getfield_pc, (0usize, b'I'));
    builder.set_field_info(fi);
    builder
}

/// #49: a snapshot local the bytecode can never read again is cut, and one
/// it still reads is not.
#[test]
fn dead_snapshot_locals_are_cut_and_live_ones_kept() {
    use crate::ir::{IrBuilder, NO_NODE};
    //  0: iload_0  1: istore_1  2: iload_0  3: ireturn
    // Local 1 is written at 1 and never read; local 0 is read at 2.
    let code = [0x1a, 0x3c, 0x1a, 0xac, 0, 0];
    let mut graph = IrBuilder::new(1, 2).build(&code, 4).expect("build");
    let at = |g: &crate::ir::Graph, bci: usize| {
        g.safepoints
            .iter()
            .find(|s| s.bci == bci)
            .map(|s| s.locals.clone())
            .expect("a snapshot")
    };
    assert_ne!(
        at(&graph, 2)[1],
        NO_NODE,
        "precondition: the builder recorded local 1"
    );
    assert!(ir_prune_dead_snapshot_locals(&mut graph, &code, 4) > 0);
    assert_eq!(at(&graph, 2)[1], NO_NODE, "local 1 is dead at bci 2");
    assert_ne!(at(&graph, 2)[0], NO_NODE, "local 0 is read at bci 2");
    assert_eq!(
        at(&graph, 3)[0],
        NO_NODE,
        "nothing is read after the return's operand"
    );
}

/// #49, the plain shape: `Foo o = new Foo(); o.x = x; int r = o.x;` followed
/// by a division whose zero guard is a real deopt point. After optimization
/// in a default run there is NO allocation left, and — the part a recipe
/// could not fake — no surviving snapshot names the removed allocation at
/// all: the object is dead at the guard, so the elision needed no
/// virtual-object descriptor.
#[test]
fn an_allocation_dead_at_a_later_guard_is_elided_by_default() {
    use crate::ir::Op;
    // static int f(int x) { Foo o = new Foo(); o.x = x; int r = o.x; return 100 / x + r; }
    let code = [
        0xbb, 0x00, 0x01, //  0: new #1
        0x59, //  3: dup
        0xb7, 0x00, 0x02, //  4: invokespecial Foo.<init> (elided)
        0x4c, //  7: astore_1
        0x2b, //  8: aload_1
        0x1a, //  9: iload_0
        0xb5, 0x00, 0x03, // 10: putfield x
        0x2b, // 13: aload_1
        0xb4, 0x00, 0x03, // 14: getfield x
        0x3d, // 17: istore_2
        0x10, 0x64, // 18: bipush 100
        0x1a, // 20: iload_0
        0x6c, // 21: idiv      <- the guard
        0x1c, // 22: iload_2
        0x60, // 23: iadd
        0xac, // 24: ireturn
        0x00, 0x00,
    ];
    let (graph, _sr) = run_ir_pipeline_through_escape_analysis(&code, 25, foo_builder(3, 10, 14));
    assert!(
        graph
            .nodes
            .iter()
            .any(|n| matches!(n.op, Op::Guard { bci: 21 })),
        "precondition: the division's guard is a real deopt point"
    );
    assert!(
        !graph.nodes.iter().any(|n| matches!(n.op, Op::New { .. })),
        "the non-escaping allocation must be elided"
    );
    assert_eq!(
        graph.safepoints.iter().map(|s| s.bci).collect::<Vec<_>>(),
        vec![21],
        "only the snapshot a deopt can consult survives"
    );
    assert_no_snapshot_names_a_dead_node(&graph);
}

/// #49, with `if (o != null)` in the way. The null check on a fresh
/// allocation folds to a constant before escape analysis, so it neither
/// escapes the object nor counts as a value use that pins it.
#[test]
fn a_null_check_on_a_fresh_allocation_does_not_pin_it() {
    use crate::ir::Op;
    let code = [
        0xbb, 0x00, 0x01, //  0: new #1
        0x59, //  3: dup
        0xb7, 0x00, 0x02, //  4: invokespecial Foo.<init> (elided)
        0x4c, //  7: astore_1
        0x2b, //  8: aload_1
        0x1a, //  9: iload_0
        0xb5, 0x00, 0x03, // 10: putfield x
        0x2b, // 13: aload_1
        0xb4, 0x00, 0x03, // 14: getfield x
        0x3d, // 17: istore_2
        0x2b, // 18: aload_1
        0xc6, 0x00, 0x06, // 19: ifnull +6 -> 25
        0x84, 0x02, 0x01, // 22: iinc 2, 1
        0x10, 0x64, // 25: bipush 100
        0x1a, // 27: iload_0
        0x6c, // 28: idiv      <- the guard
        0x1c, // 29: iload_2
        0x60, // 30: iadd
        0xac, // 31: ireturn
        0x00, 0x00,
    ];
    // The fold alone, on the unoptimized graph: the compare is gone.
    let mut built = foo_builder(3, 10, 14).build(&code, 32).expect("IR build");
    assert_eq!(
        ir_fold_null_checks_on_fresh_allocations(&mut built),
        1,
        "`o == null` on the fresh `new` folds"
    );

    let (graph, _sr) = run_ir_pipeline_through_escape_analysis(&code, 32, foo_builder(3, 10, 14));
    assert!(
        !graph.nodes.iter().any(|n| matches!(n.op, Op::New { .. })),
        "a null check must not keep the allocation alive"
    );
    assert_no_snapshot_names_a_dead_node(&graph);
}

/// #49, the other half: the allocation IS live at a real deopt point (the
/// field is read after the division), so eliding it needs the
/// virtual-object recipe. With precise resume on (the default) the
/// allocation goes, and the guard's deopt frame carries a
/// `FrameValue::VirtualObject` the resume materializes. With it off the
/// allocation must stay.
#[test]
fn an_allocation_live_at_a_guard_is_elided_with_a_materialization_recipe() {
    use crate::deopt::FrameValue;
    use crate::ir::Op;
    // static int f(int x) { Foo o = new Foo(); o.x = x; int q = 100 / x; return o.x + q; }
    let code = [
        0xbb, 0x00, 0x01, //  0: new #1
        0x59, //  3: dup
        0xb7, 0x00, 0x02, //  4: invokespecial Foo.<init> (elided)
        0x4c, //  7: astore_1
        0x2b, //  8: aload_1
        0x1a, //  9: iload_0
        0xb5, 0x00, 0x03, // 10: putfield x
        0x10, 0x64, // 13: bipush 100
        0x1a, // 15: iload_0
        0x6c, // 16: idiv      <- the guard; `o` is still live here
        0x3d, // 17: istore_2
        0x2b, // 18: aload_1
        0xb4, 0x00, 0x03, // 19: getfield x
        0x1c, // 22: iload_2
        0x60, // 23: iadd
        0xac, // 24: ireturn
        0x00, 0x00,
    ];
    let (graph, sr_map) =
        run_ir_pipeline_through_escape_analysis(&code, 25, foo_builder(3, 10, 19));
    if !scalar_deopt_descriptor_available() {
        assert!(
            graph.nodes.iter().any(|n| matches!(n.op, Op::New { .. })),
            "without precise resume a consultable snapshot keeps the allocation"
        );
        return;
    }
    assert!(
        !graph.nodes.iter().any(|n| matches!(n.op, Op::New { .. })),
        "with a recipe available the allocation is elided"
    );
    let sr_map = sr_map.expect("the elided object is described");
    let schedule = ir_schedule::schedule(&graph);
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the elided graph calls no helper.
    let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    let cm = ir_lower::lower_with_scalar_deopt(&graph, &schedule, 1, 3, &helpers, Some(&sr_map))
        .expect("lower");
    let point = cm
        ._deopt_point_boxes
        .iter()
        .find(|p| p.bci == 16)
        .expect("the division's guard publishes a deopt point");
    match point.frame_state.locals.get(1) {
        Some(FrameValue::VirtualObject(state)) => {
            assert_eq!(state.class_id, 7);
            assert_eq!(state.num_fields, 1);
            assert!(
                !matches!(
                    state.field_values[0],
                    FrameValue::MaterializationRequired(_)
                        | FrameValue::Undefined
                        | FrameValue::Unsupported
                ),
                "the stored field must be described, got {:?}",
                state.field_values[0]
            );
        }
        other => panic!("local 1 must be the object's recipe, got {other:?}"),
    }
    assert!(crate::deopt::frame_state_is_resumable(&point.frame_state));
    assert!(
        !ir_artifact_names_an_undescribable_elided_object(&cm),
        "the backstop must not discard a fully described artifact"
    );
}

// A `<init>` whose receiver is NOT a fresh `new` (e.g. a super() call on
// `this`) must NOT be elided — the builder bails even if the pc is admitted.
#[test]
fn ir_new_bails_on_init_of_nonfresh_receiver() {
    use crate::ir::IrBuilder;
    use std::collections::{HashMap, HashSet};
    // aload_0; invokespecial #2; return  — `super.<init>()` on `this`.
    //   0: aload_0           2a
    //   1: invokespecial #2  b7 00 02
    //   4: return            b1
    let code = [0x2a, 0xb7, 0x00, 0x02, 0xb1, 0x00, 0x00];
    let mut builder = IrBuilder::new(1, 1);
    let mut init_pcs = HashSet::new();
    init_pcs.insert(1usize); // admit pc 1 — but receiver is `this`, not a New
    builder.set_new_info(HashMap::new(), init_pcs);
    assert!(
        builder.build(&code, 5).is_none(),
        "eliding a <init> on a non-fresh receiver must bail to single-pass"
    );
}

// activate-ir-optimizer (scalar-new wiring): a `new`-bearing method whose
// construction is elidable routes through the IR pipeline (scalar-replaced)
// ONLY when the elidable-`<init>` resolver is supplied — the production soak
// gate. Without it the builder bails on the `invokespecial`, keeping `new`
// scalar replacement off by default.
#[test]
// x86-64 only: this asserts how `try_compile_inner` ROUTES a compile
// (single-pass vs IR vs dispatch), and on another architecture that
// function takes its `#[cfg(target_arch = "aarch64")]` branch and
// returns before any of that routing happens. Gated so the crate's
// test run is green on aarch64 rather than carrying known reds.
#[cfg(target_arch = "x86_64")]
fn scalar_new_wiring_routes_through_ir_only_with_resolver() {
    // ROUTING, not acceptance. The C1->C2 acceptance gate
    // (`ir_evidence`) refuses a body that applied no transform the
    // baseline tier lacks, which every method in this test is --
    // they are three-bytecode probes. Forcing `Always` keeps this a
    // test of one thing.
    let _accept = crate::ir_evidence::AcceptAlways::on();
    use std::sync::Arc;
    // static int f() { Foo o = new Foo(); o.x = 42; return o.x; }
    let cached = CachedBytecodeMethod {
        declaring_class_id: cratonvm_types::ClassId::new(1),
        class_name: Arc::from("pkg/Mk"),
        method_name: Arc::from("f"),
        method_descriptor: Arc::from("()I"),
        source_file: None,
        code: Arc::from(
            [
                0xbb, 0x00, 0x01, 0x59, 0xb7, 0x00, 0x02, 0x59, 0x10, 0x2a, 0xb5, 0x00, 0x03, 0xb4,
                0x00, 0x03, 0xac, 0x00, 0x00,
            ]
            .as_slice(),
        ),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 8,
        max_locals: 1,
        num_params: 0,
        is_synchronized: false,
        is_static: true,
        force_native_cache: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    };
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the test wires only the slots it exercises.
    let mut helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    // `lower_inner` refuses a graph whose `new`/field ops have no helper to
    // call. Those guards used to be unreachable here because escape
    // analysis elided the allocation and folded its loads and stores away;
    // it no longer does (an allocation named by a safepoint slot is not
    // elided unless the scalar-deopt path is on), so `Op::New`, `Op::Load`
    // and `Op::Store` all survive to lowering. A real VM never presents a
    // null helper; supply the three this graph reaches. The compiled code
    // is never executed by this test, only inspected for routing.
    helpers.new_object = 1;
    helpers.getfield = 1;
    helpers.putfield_int = 1;
    let new_resolver = |cp: u16| -> Option<JitNewSite> {
        if cp == 1 {
            Some(JitNewSite::Resolved {
                class_id: 7,
                num_fields: 1,
                has_prim_init: false,
                has_finalizer: false,
            })
        } else {
            None
        }
    };
    // (field_index, type_tag, compact_slot) — a plain non-compact int
    // field resolves with `(_, _, None)`: no registered compact slot.
    let field_resolver = |cp: u16| -> Option<(usize, u8, Option<(u32, bool)>)> {
        if cp == 3 {
            Some((0, b'I', None))
        } else {
            None
        }
    };
    let elidable = |cp: u16| -> bool { cp == 2 };

    // With the elidable resolver → the IR pipeline scalar-replaces the new.
    IR_LOWER_COMPILES.with(|c| c.set(0));
    let r = try_compile(
        &cached,
        None,
        Some(&field_resolver),
        None,
        None,
        None,
        Some(&new_resolver),
        None,
        None,
        None,
        &helpers,
        None,
        None,
        None,
        Some(&elidable),
        true,
        false,
        false,
        false,
        false,
        false,
        None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
    );
    // Compact field layout used to bail the IR builder on
    // `new`/getfield/putfield before the elidable-`<init>` check was
    // ever reached, so under the default this test asserted only that
    // nothing happened. The builder no longer refuses those opcodes.
    assert!(r.is_some(), "an elidable `new` method must compile via IR");
    assert_eq!(
        IR_LOWER_COMPILES.with(|c| c.get()),
        1,
        "the elidable `new` method must route through the IR pipeline"
    );

    // Without it → the builder bails on the `invokespecial` → not the IR path.
    IR_LOWER_COMPILES.with(|c| c.set(0));
    let _ = try_compile(
        &cached,
        None,
        Some(&field_resolver),
        None,
        None,
        None,
        Some(&new_resolver),
        None,
        None,
        None,
        &helpers,
        None,
        None,
        None,
        None,
        true,
        false,
        false,
        false,
        false,
        false,
        None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
    );
    assert_eq!(
        IR_LOWER_COMPILES.with(|c| c.get()),
        0,
        "without the elidable resolver, `new` must NOT take the IR pipeline"
    );
}

// ── cold-`new` fix: a `new` of a NOT-YET-LOADED class must still compile ──
//
// The gap this guards (jit-compile-bail-unresolved-new-cold-class.md):
// `resolve_jit_new_site` only sees already-loaded classes, so a hot method
// whose only un-taken branch does `throw new SomeException(...)` reported
// `None`, `try_compile_inner` bailed the WHOLE compile at the `new_resolve`
// site, and after `MAX_TIER_FAIL_RETRIES` the method interpreted forever
// (json-smart's `JSONParserBase.readMain`: 293,940 interpreted invocations).
//
// Differential, so it cannot pass vacuously: the SAME bytecode is compiled
// three ways. `Resolved` proves the shape is compilable at all; `Deferred`
// with the CP helper wired must ALSO compile (the fix); `Deferred` with the
// helper unwired must still bail (a hand-built test table must never get a
// CALL to address 0).
#[test]
// x86-64 only: this asserts how `try_compile_inner` ROUTES a compile
// (single-pass vs IR vs dispatch), and on another architecture that
// function takes its `#[cfg(target_arch = "aarch64")]` branch and
// returns before any of that routing happens. Gated so the crate's
// test run is green on aarch64 rather than carrying known reds.
#[cfg(target_arch = "x86_64")]
fn deferred_new_site_compiles_when_cp_helper_is_wired() {
    use std::sync::Arc;
    // `static int f() { new Cold(); pop; return 0; }`
    //   new #1; pop; iconst_0; ireturn
    let mk = |name: &str| CachedBytecodeMethod {
        declaring_class_id: cratonvm_types::ClassId::new(1),
        class_name: Arc::from("pkg/ColdNew"),
        method_name: Arc::from(name),
        method_descriptor: Arc::from("()I"),
        source_file: None,
        code: Arc::from([0xbb, 0x00, 0x01, 0x57, 0x03, 0xac, 0x00, 0x00].as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 2,
        max_locals: 1,
        num_params: 0,
        is_synchronized: false,
        is_static: true,
        force_native_cache: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    };

    // Non-null placeholders: this test only compiles, never executes, so the
    // backend just needs the slots to read as "wired".
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the test wires only the slots it exercises.
    let mut wired: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    wired.new_object = 0x1000;
    wired.new_object_cp = 0x2000;
    let mut unwired = wired;
    unwired.new_object_cp = 0;

    let resolved = |cp: u16| -> Option<JitNewSite> {
        (cp == 1).then_some(JitNewSite::Resolved {
            class_id: 7,
            num_fields: 1,
            has_prim_init: true,
            has_finalizer: true,
        })
    };
    let deferred = |cp: u16| -> Option<JitNewSite> {
        (cp == 1).then_some(JitNewSite::Deferred {
            holder_class_id: 1,
            cp_idx: cp,
        })
    };

    let compile = |cached: &CachedBytecodeMethod,
                   helpers: &JitRuntimeHelpers,
                   r: &dyn Fn(u16) -> Option<JitNewSite>| {
        try_compile(
            cached,
            None,
            None,
            None,
            None,
            None,
            Some(r),
            None,
            None,
            None,
            helpers,
            None,
            None,
            None,
            None,
            // Single-pass backend: this is about the CP-resolution
            // pre-pass, not the IR tier.
            false,
            false,
            false,
            false,
            false,
            false,
            None,
        )
    };

    assert!(
        compile(&mk("resolved"), &wired, &resolved).is_some(),
        "control arm: a resolved `new` of this shape must compile — if it \
         does not, the two Deferred assertions below prove nothing"
    );
    assert!(
        compile(&mk("deferred"), &wired, &deferred).is_some(),
        "THE FIX: a `new` whose class is not loaded yet must compile to the \
         CP-indexed helper instead of bailing the whole method"
    );
    assert!(
        compile(&mk("deferred_unwired"), &unwired, &deferred).is_none(),
        "with `new_object_cp` unwired (hand-built test tables) a deferred \
         site must keep the historical bail, never emit a CALL to 0"
    );
}

// ── activate-ir-optimizer Gap B: int invokestatic → Op::Call routing ──
//
// A method whose only invoke is an int-only `invokestatic` in an oop-free
// body must route through the IR pipeline ONLY when `ir_emit_calls` is on
// (the production gate). This guards against a *vacuous* validation: the
// integration harness proves the executed result is correct, but single-pass
// ALSO dispatches `invokestatic` correctly, so result-equality alone would
// not prove the IR path fired. `IR_LOWER_COMPILES` proves it does (==1 with
// the flag) and does not (==0 without — the builder bails on the invoke).
#[test]
// x86-64 only: this asserts how `try_compile_inner` ROUTES a compile
// (single-pass vs IR vs dispatch), and on another architecture that
// function takes its `#[cfg(target_arch = "aarch64")]` branch and
// returns before any of that routing happens. Gated so the crate's
// test run is green on aarch64 rather than carrying known reds.
#[cfg(target_arch = "x86_64")]
fn ir_call_wiring_routes_through_ir_only_with_flag() {
    // ROUTING, not acceptance. The C1->C2 acceptance gate
    // (`ir_evidence`) refuses a body that applied no transform the
    // baseline tier lacks, which every method in this test is --
    // they are three-bytecode probes. Forcing `Always` keeps this a
    // test of one thing.
    let _accept = crate::ir_evidence::AcceptAlways::on();
    // This test exercises the optimizing IR pipeline, which is gated off
    // whenever the young generation can relocate. Pin the policy so the
    // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
    crate::x64::set_moving_young_override(Some(false));
    use std::sync::Arc;

    // `static int f(int a, int b) { return g(a, b); }`
    //   iload_0; iload_1; invokestatic #2; ireturn
    let cached = CachedBytecodeMethod {
        declaring_class_id: cratonvm_types::ClassId::new(1),
        class_name: Arc::from("pkg/Caller"),
        method_name: Arc::from("f"),
        method_descriptor: Arc::from("(II)I"),
        source_file: None,
        code: Arc::from([0x1a, 0x1b, 0xb8, 0x00, 0x02, 0xac, 0x00, 0x00].as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 2,
        max_locals: 2,
        num_params: 2,
        is_synchronized: false,
        is_static: true,
        force_native_cache: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    };
    // SAFETY: all-zero `JitRuntimeHelpers` is valid; this test only COMPILES
    // (never executes the body), so the baked `invoke_dispatch` is not called.
    let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    let invoke_resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(II)I".into()))
        } else {
            None
        }
    };

    // ir_emit_calls = true → invokestatic lowers to Op::Call → IR pipeline.
    IR_LOWER_COMPILES.with(|c| c.set(0));
    let with = try_compile(
        &cached,
        None,
        None,
        None,
        Some(&invoke_resolver),
        None,
        None,
        None,
        None,
        None,
        &helpers,
        None,
        None,
        None,
        None,
        true,  // optimize
        true,  // ir_emit_calls
        false, // ir_emit_special_calls (testing invokestatic, not special)
        false, // ir_emit_long
        false, // ir_emit_virtual_calls
        false, // ir_emit_fp
        None,  // cp_invokedynamic_descriptor_resolver: no indy in these test methods
    );
    assert!(
        with.is_some(),
        "int invokestatic must compile with ir_emit_calls"
    );
    assert_eq!(
        IR_LOWER_COMPILES.with(|c| c.get()),
        1,
        "int invokestatic must route through the IR pipeline when ir_emit_calls is on"
    );
    assert!(
        with.as_ref().unwrap().needs_context(),
        "an Op::Call method must be needs_context"
    );

    // ir_emit_calls = false → builder bails on the invoke → single-pass.
    IR_LOWER_COMPILES.with(|c| c.set(0));
    let _without = try_compile(
        &cached,
        None,
        None,
        None,
        Some(&invoke_resolver),
        None,
        None,
        None,
        None,
        None,
        &helpers,
        None,
        None,
        None,
        None,
        true,  // optimize
        false, // ir_emit_calls OFF
        false, // ir_emit_special_calls OFF
        false, // ir_emit_long OFF
        false, // ir_emit_virtual_calls OFF
        false, // ir_emit_fp OFF
        None,  // cp_invokedynamic_descriptor_resolver: no indy in these test methods
    );
    assert_eq!(
        IR_LOWER_COMPILES.with(|c| c.get()),
        0,
        "without ir_emit_calls, invokestatic must NOT take the IR pipeline"
    );
}

/// inc 24 (Gap B): a resolved non-`<init>` `invokespecial` routes through the
/// IR pipeline ONLY when `ir_emit_special_calls` is on — independent of
/// `ir_emit_calls` (which gates invokestatic). The two toggles are crossed
/// here to prove the SPECIAL flag alone admits invokespecial. Guards against
/// a vacuous validation: single-pass ALSO dispatches invokespecial, so
/// result-equality alone (the integration harness) would not prove the IR
/// path ran.
#[test]
// x86-64 only: this asserts how `try_compile_inner` ROUTES a compile
// (single-pass vs IR vs dispatch), and on another architecture that
// function takes its `#[cfg(target_arch = "aarch64")]` branch and
// returns before any of that routing happens. Gated so the crate's
// test run is green on aarch64 rather than carrying known reds.
#[cfg(target_arch = "x86_64")]
fn ir_special_call_wiring_routes_through_ir_only_with_flag() {
    // ROUTING, not acceptance. The C1->C2 acceptance gate
    // (`ir_evidence`) refuses a body that applied no transform the
    // baseline tier lacks, which every method in this test is --
    // they are three-bytecode probes. Forcing `Always` keeps this a
    // test of one thing.
    let _accept = crate::ir_evidence::AcceptAlways::on();
    // This test exercises the optimizing IR pipeline, which is gated off
    // whenever the young generation can relocate. Pin the policy so the
    // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
    crate::x64::set_moving_young_override(Some(false));
    use std::sync::Arc;

    // `static int f(Obj o, int n) { return o.g(n); }`  (g private → invokespecial)
    //   aload_0; iload_1; invokespecial #2; ireturn
    let cached = CachedBytecodeMethod {
        declaring_class_id: cratonvm_types::ClassId::new(1),
        class_name: Arc::from("pkg/Caller"),
        method_name: Arc::from("f"),
        method_descriptor: Arc::from("(Lpkg/Obj;I)I"),
        source_file: None,
        code: Arc::from([0x2a, 0x1b, 0xb7, 0x00, 0x02, 0xac, 0x00, 0x00].as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 2,
        max_locals: 2,
        num_params: 2,
        is_synchronized: false,
        is_static: true,
        force_native_cache: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    };
    // SAFETY: all-zero `JitRuntimeHelpers` is valid; this test only COMPILES
    // (never executes the body), so the baked `invoke_dispatch` is not called.
    let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    // Target: the private instance method `g(I)I` — receiver implicit, so the
    // IR builder marshals it as arg0 (num_jit_args = 1 desc + 1 receiver).
    let invoke_resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Obj".into(), "g".into(), "(I)I".into()))
        } else {
            None
        }
    };

    // ir_emit_special_calls = true (ir_emit_calls OFF) → invokespecial lowers
    // to Op::Call → IR pipeline. Crossing the flags proves SPECIAL is the gate.
    IR_LOWER_COMPILES.with(|c| c.set(0));
    let with = try_compile(
        &cached,
        None,
        None,
        None,
        Some(&invoke_resolver),
        None,
        None,
        None,
        None,
        None,
        &helpers,
        None,
        None,
        None,
        None,
        true,  // optimize
        false, // ir_emit_calls (invokestatic) OFF
        true,  // ir_emit_special_calls ON
        false, // ir_emit_long
        false, // ir_emit_virtual_calls OFF
        false, // ir_emit_fp OFF
        None,  // cp_invokedynamic_descriptor_resolver: no indy in these test methods
    );
    assert!(
        with.is_some(),
        "invokespecial must compile with ir_emit_special_calls"
    );
    assert_eq!(
        IR_LOWER_COMPILES.with(|c| c.get()),
        1,
        "invokespecial must route through the IR pipeline when ir_emit_special_calls is on"
    );
    assert!(
        with.as_ref().unwrap().needs_context(),
        "an Op::Call method must be needs_context"
    );

    // ir_emit_special_calls = false (ir_emit_calls ON) → the builder bails on
    // the invokespecial → single-pass. Proves invokestatic's gate does NOT
    // admit invokespecial.
    IR_LOWER_COMPILES.with(|c| c.set(0));
    let _without = try_compile(
        &cached,
        None,
        None,
        None,
        Some(&invoke_resolver),
        None,
        None,
        None,
        None,
        None,
        &helpers,
        None,
        None,
        None,
        None,
        true,  // optimize
        true,  // ir_emit_calls (invokestatic) ON
        false, // ir_emit_special_calls OFF
        false, // ir_emit_long OFF
        false, // ir_emit_virtual_calls OFF
        false, // ir_emit_fp OFF
        None,  // cp_invokedynamic_descriptor_resolver: no indy in these test methods
    );
    assert_eq!(
        IR_LOWER_COMPILES.with(|c| c.get()),
        0,
        "without ir_emit_special_calls, invokespecial must NOT take the IR pipeline"
    );
}

/// inc 25: a long-using method routes through the IR pipeline ONLY with
/// `ir_emit_long` — otherwise `method_uses_category2` bails the whole
/// pipeline to single-pass. Guards against a vacuous validation: the
/// integration harness compiles `optimize=true` either way (single-pass is
/// the fall-through), so result-equality alone would not prove the IR path
/// ran — `IR_LOWER_COMPILES` does.
#[test]
// x86-64 only: this asserts how `try_compile_inner` ROUTES a compile
// (single-pass vs IR vs dispatch), and on another architecture that
// function takes its `#[cfg(target_arch = "aarch64")]` branch and
// returns before any of that routing happens. Gated so the crate's
// test run is green on aarch64 rather than carrying known reds.
#[cfg(target_arch = "x86_64")]
fn ir_long_wiring_routes_through_ir_only_with_flag() {
    // ROUTING, not acceptance. The C1->C2 acceptance gate
    // (`ir_evidence`) refuses a body that applied no transform the
    // baseline tier lacks, which every method in this test is --
    // they are three-bytecode probes. Forcing `Always` keeps this a
    // test of one thing.
    let _accept = crate::ir_evidence::AcceptAlways::on();
    // This test exercises the optimizing IR pipeline, which is gated off
    // whenever the young generation can relocate. Pin the policy so the
    // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
    crate::x64::set_moving_young_override(Some(false));
    use std::sync::Arc;

    // `static long add(long a, long b) { return a + b; }`
    //   lload_0; lload_2; ladd; lreturn
    let cached = CachedBytecodeMethod {
        declaring_class_id: cratonvm_types::ClassId::new(1),
        class_name: Arc::from("pkg/L"),
        method_name: Arc::from("add"),
        method_descriptor: Arc::from("(JJ)J"),
        source_file: None,
        code: Arc::from([0x1e, 0x20, 0x61, 0xad, 0x00, 0x00].as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 4,
        max_locals: 4,
        num_params: 2,
        is_synchronized: false,
        is_static: true,
        force_native_cache: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    };
    // SAFETY: all-zero `JitRuntimeHelpers` is valid; this test only COMPILES
    // (never executes), and a pure long-arithmetic method calls no helper.
    let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };

    // ir_emit_long = true → the long method takes the IR pipeline.
    IR_LOWER_COMPILES.with(|c| c.set(0));
    let with = try_compile(
        &cached, None, None, None, None, None, None, None, None, None, &helpers, None, None, None,
        None, true, false, false, true, false, false,
        None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
    );
    assert!(with.is_some(), "long method must compile with ir_emit_long");
    assert_eq!(
        IR_LOWER_COMPILES.with(|c| c.get()),
        1,
        "a long method must route through the IR pipeline when ir_emit_long is on"
    );

    // ir_emit_long = false → method_uses_category2 bails → single-pass.
    IR_LOWER_COMPILES.with(|c| c.set(0));
    let _without = try_compile(
        &cached, None, None, None, None, None, None, None, None, None, &helpers, None, None, None,
        None, true, false, false, false, false, false,
        None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
    );
    assert_eq!(
        IR_LOWER_COMPILES.with(|c| c.get()),
        0,
        "without ir_emit_long, a long method must NOT take the IR pipeline"
    );
}

/// inc 30 (FP value tier): a method that uses `float`/`double` internally
/// (with an FP-free `int` signature) routes through the IR pipeline ONLY when
/// `ir_emit_fp` is on. Single-pass also compiles the method, so result
/// equality alone would not prove the IR path ran — `IR_LOWER_COMPILES`
/// proves it (==1 with the flag, ==0 without → vacuous single-pass fallback).
#[test]
// x86-64 only: this asserts how `try_compile_inner` ROUTES a compile
// (single-pass vs IR vs dispatch), and on another architecture that
// function takes its `#[cfg(target_arch = "aarch64")]` branch and
// returns before any of that routing happens. Gated so the crate's
// test run is green on aarch64 rather than carrying known reds.
#[cfg(target_arch = "x86_64")]
fn ir_fp_wiring_routes_through_ir_only_with_flag() {
    // ROUTING, not acceptance. The C1->C2 acceptance gate
    // (`ir_evidence`) refuses a body that applied no transform the
    // baseline tier lacks, which every method in this test is --
    // they are three-bytecode probes. Forcing `Always` keeps this a
    // test of one thing.
    let _accept = crate::ir_evidence::AcceptAlways::on();
    // This test exercises the optimizing IR pipeline, which is gated off
    // whenever the young generation can relocate. Pin the policy so the
    // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
    crate::x64::set_moving_young_override(Some(false));
    use std::sync::Arc;

    // `static int f(int a) { return (int)((float)a + 2.0f); }`
    //   iload_0; i2f; fconst_2; fadd; f2i; ireturn
    let cached = CachedBytecodeMethod {
        declaring_class_id: cratonvm_types::ClassId::new(1),
        class_name: Arc::from("pkg/F"),
        method_name: Arc::from("f"),
        method_descriptor: Arc::from("(I)I"),
        source_file: None,
        code: Arc::from([0x1a, 0x86, 0x0d, 0x62, 0x8b, 0xac, 0x00, 0x00].as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 4,
        max_locals: 1,
        num_params: 1,
        is_synchronized: false,
        is_static: true,
        force_native_cache: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    };
    // SAFETY: all-zero `JitRuntimeHelpers` is valid; this test only COMPILES
    // (never executes), and a pure FP-arithmetic method calls no helper.
    let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };

    // ir_emit_fp = true → the FP method takes the IR pipeline.
    IR_LOWER_COMPILES.with(|c| c.set(0));
    let with = try_compile(
        &cached, None, None, None, None, None, None, None, None, None, &helpers, None, None, None,
        None, true, false, false, false, false, true,
        None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
    );
    assert!(with.is_some(), "FP method must compile with ir_emit_fp");
    assert_eq!(
        IR_LOWER_COMPILES.with(|c| c.get()),
        1,
        "an FP method must route through the IR pipeline when ir_emit_fp is on"
    );

    // ir_emit_fp = false → method_uses_fp bails the IR path → single-pass.
    IR_LOWER_COMPILES.with(|c| c.set(0));
    let _without = try_compile(
        &cached, None, None, None, None, None, None, None, None, None, &helpers, None, None, None,
        None, true, false, false, false, false, false,
        None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
    );
    assert_eq!(
        IR_LOWER_COMPILES.with(|c| c.get()),
        0,
        "without ir_emit_fp, an FP method must NOT take the IR pipeline"
    );
}

/// inc 26 (Gap B) + jit-inlining-and-ir-calls: a resolved `invokevirtual`
/// routes through the IR pipeline.
///
/// **The polarity of this test inverted on 2026-07-26.** It used to assert
/// "…ONLY when `ir_emit_virtual_calls` is on", because the IR lowered
/// virtual calls through the generic dispatch helper with no inline cache
/// and admitting them was a throughput regression versus single-pass.
/// `ir_lower::emit_inline_cache_call` now emits the MIC + 4-way-PIC
/// cascade, so the capability became opt-OUT: the caller's parameter can
/// still force it ON, but only the diagnostic
/// `CRATONVM_JIT_IR_CALL_VIRTUAL=0` (here, its thread-local test override)
/// turns it off.
///
/// Guards against a vacuous validation: single-pass ALSO dispatches
/// invokevirtual, so result-equality alone would not prove the IR path ran.
#[test]
// x86-64 only: this asserts how `try_compile_inner` ROUTES a compile
// (single-pass vs IR vs dispatch), and on another architecture that
// function takes its `#[cfg(target_arch = "aarch64")]` branch and
// returns before any of that routing happens. Gated so the crate's
// test run is green on aarch64 rather than carrying known reds.
#[cfg(target_arch = "x86_64")]
fn ir_virtual_call_wiring_routes_through_ir_only_with_flag() {
    // ROUTING, not acceptance. The C1->C2 acceptance gate
    // (`ir_evidence`) refuses a body that applied no transform the
    // baseline tier lacks, which every method in this test is --
    // they are three-bytecode probes. Forcing `Always` keeps this a
    // test of one thing.
    let _accept = crate::ir_evidence::AcceptAlways::on();
    // This test exercises the optimizing IR pipeline, which is gated off
    // whenever the young generation can relocate. Pin the policy so the
    // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
    crate::x64::set_moving_young_override(Some(false));
    use std::sync::Arc;

    // `static int f(Obj o, int n) { return o.g(n); }`  (g virtual → invokevirtual)
    //   aload_0; iload_1; invokevirtual #2; ireturn
    // Modelled as a `static` caller (receiver `o` is param 0) exactly like the
    // invokespecial wiring test, so the local/param counts line up.
    let cached = CachedBytecodeMethod {
        declaring_class_id: cratonvm_types::ClassId::new(1),
        class_name: Arc::from("pkg/Caller"),
        method_name: Arc::from("f"),
        method_descriptor: Arc::from("(Lpkg/Obj;I)I"),
        source_file: None,
        code: Arc::from([0x2a, 0x1b, 0xb6, 0x00, 0x02, 0xac, 0x00, 0x00].as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 2,
        max_locals: 2,
        num_params: 2,
        is_synchronized: false,
        is_static: true,
        force_native_cache: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    };
    // SAFETY: all-zero `JitRuntimeHelpers` is valid; this test only COMPILES
    // (never executes the body), so the baked `invoke_dispatch` is not called.
    let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    let invoke_resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Obj".into(), "g".into(), "(I)I".into()))
        } else {
            None
        }
    };

    // ir_emit_virtual_calls = true (calls/special OFF) → invokevirtual lowers
    // to Op::Call → IR pipeline. Crossing the flags proves VIRTUAL is the gate.
    IR_LOWER_COMPILES.with(|c| c.set(0));
    let with = try_compile(
        &cached,
        None,
        None,
        None,
        Some(&invoke_resolver),
        None,
        None,
        None,
        None,
        None,
        &helpers,
        None,
        None,
        None,
        None,
        true,  // optimize
        false, // ir_emit_calls (invokestatic) OFF
        false, // ir_emit_special_calls OFF
        false, // ir_emit_long
        true,  // ir_emit_virtual_calls ON
        false, // ir_emit_fp OFF
        None,  // cp_invokedynamic_descriptor_resolver: no indy in these test methods
    );
    assert!(
        with.is_some(),
        "invokevirtual must compile with ir_emit_virtual_calls"
    );
    assert_eq!(
        IR_LOWER_COMPILES.with(|c| c.get()),
        1,
        "invokevirtual must route through the IR pipeline when ir_emit_virtual_calls is on"
    );
    assert!(
        with.as_ref().unwrap().needs_context(),
        "an Op::Call method must be needs_context"
    );

    // The VM's centrally parsed default is true, so the IR pipeline takes
    // the method without any compiler-local environment lookup.
    IR_LOWER_COMPILES.with(|c| c.set(0));
    let by_default = try_compile(
        &cached,
        None,
        None,
        None,
        Some(&invoke_resolver),
        None,
        None,
        None,
        None,
        None,
        &helpers,
        None,
        None,
        None,
        None,
        true,  // optimize
        false, // ir_emit_calls OFF
        false, // ir_emit_special_calls OFF
        false, // ir_emit_long
        true,  // ir_emit_virtual_calls: VM's default-ON policy
        false, // ir_emit_fp OFF
        None,
    );
    assert!(by_default.is_some());
    assert_eq!(
        IR_LOWER_COMPILES.with(|c| c.get()),
        1,
        "invokevirtual must take the IR pipeline BY DEFAULT now that ir_lower \
         emits MIC/PIC inline caches — the capability is opt-out, not opt-in"
    );

    // Explicit opt-out, modelled by the thread-local override, wins over
    // the caller's true policy and routes the method to single-pass.
    __set_ir_virtual_calls_override(Some(false));
    IR_LOWER_COMPILES.with(|c| c.set(0));
    let _without = try_compile(
        &cached,
        None,
        None,
        None,
        Some(&invoke_resolver),
        None,
        None,
        None,
        None,
        None,
        &helpers,
        None,
        None,
        None,
        None,
        true,  // optimize
        true,  // ir_emit_calls ON
        true,  // ir_emit_special_calls ON
        false, // ir_emit_long
        true,  // central policy ON; test override forces it OFF
        false, // ir_emit_fp OFF
        None,  // cp_invokedynamic_descriptor_resolver: no indy in these test methods
    );
    let took_ir = IR_LOWER_COMPILES.with(|c| c.get());
    __set_ir_virtual_calls_override(None);
    assert_eq!(
        took_ir, 0,
        "with CRATONVM_JIT_IR_CALL_VIRTUAL=0, invokevirtual must NOT take the IR pipeline"
    );
}

// ── Stage A.4 (precise oop maps) — param oop mask ──────────────
//
// `compute_param_oop_mask` must mark exactly the JVM local slots that hold a
// reference parameter on entry, using the SAME slot walk as
// `compute_param_jvm_slots` (category-2 J/D consume two slots; arrays and
// `L…;` are references; primitives are not; instance `this` is slot 0). A
// wrong bit here would, under the eventual moving path, either rewrite a
// primitive (false positive) or miss an oop (false negative) — so pin it.
#[test]
fn test_compute_param_oop_mask() {
    // static, all primitive → no oop slots.
    assert_eq!(compute_param_oop_mask("(II)I", true), 0b00);
    // instance, primitive params → only `this` (slot 0).
    assert_eq!(compute_param_oop_mask("(II)I", false), 0b1);
    // static, one reference param at slot 0.
    assert_eq!(compute_param_oop_mask("(Ljava/lang/Object;I)V", true), 0b1);
    // instance, one reference param → this (0) + param (1).
    assert_eq!(compute_param_oop_mask("(Ljava/lang/Object;)V", false), 0b11);
    // static, long (slots 0,1; non-oop) then reference at slot 2.
    assert_eq!(
        compute_param_oop_mask("(JLjava/lang/Object;)V", true),
        0b100
    );
    // static, array ref (slot 0), long (slots 1,2), array-of-ref (slot 3).
    assert_eq!(
        compute_param_oop_mask("([IJ[Ljava/lang/String;)V", true),
        0b1001
    );
    // bt18's `static Node make(int)` — the live oop is the LOCAL `n`, not a
    // param, so the param mask is empty (the dataflow seeds it as `astore`d).
    assert_eq!(compute_param_oop_mask("(I)Lpkg/Node;", true), 0b0);
    // instance, double param (slots 1,2; non-oop) → only `this`.
    assert_eq!(compute_param_oop_mask("(D)V", false), 0b1);
    // no params: static → 0; instance → this only.
    assert_eq!(compute_param_oop_mask("()V", true), 0b0);
    assert_eq!(compute_param_oop_mask("()V", false), 0b1);
}

// ── JitMICSlot layout tests (CRIT-8 prerequisite) ──────────────
//
// The JIT codegen in `jit/src/x64.rs` emits raw `MOV` instructions
// against a `JitMICSlot*` using the `CACHED_*_OFFSET` constants.
// If the struct layout drifts (e.g. someone reorders a field,
// changes padding, or `parking_lot::Mutex` grows), those MOVs will
// silently read the wrong bytes. Pin the offsets here.
//
// We don't use `core::mem::offset_of!` because the workspace MSRV
// (see `Cargo.toml: rust-version`) is 1.75; the macro stabilized
// in 1.77.

#[test]
fn test_jit_mic_slot_offsets() {
    let slot = JitMICSlot::new();
    let base = &slot as *const JitMICSlot as usize;
    let off_class_id = (&slot.cached_class_id as *const _ as usize) - base;
    let off_entry_ptr = (&slot.cached_entry_word as *const _ as usize) - base;
    let off_hits = (&slot.hits as *const _ as usize) - base;
    assert_eq!(
        off_class_id,
        JitMICSlot::CACHED_CLASS_ID_OFFSET,
        "cached_class_id offset drift: expected {}, got {}",
        JitMICSlot::CACHED_CLASS_ID_OFFSET,
        off_class_id
    );
    assert_eq!(
        off_entry_ptr,
        JitMICSlot::CACHED_ENTRY_PTR_OFFSET,
        "cached_entry_word offset drift: expected {}, got {}",
        JitMICSlot::CACHED_ENTRY_PTR_OFFSET,
        off_entry_ptr
    );
    // The needs-context byte moved into the entry word; its 8 reserved
    // bytes keep the counters where they were.
    assert_eq!(off_hits, 24, "hits offset drift: got {off_hits}");
}

// ── JitPICSlot layout tests (CRIT-8 prerequisite) ──────────────
//
// The JIT codegen emits raw `MOV` instructions against a
// `JitPICSlot*` using the `CLASS_ID_OFFSETS` / `ENTRY_PTR_OFFSETS`
// constants for inline 4-way PIC dispatch. Pin the offsets here so any layout drift (field
// reorder, padding change, `parking_lot::Mutex` resize) fails
// loudly instead of silently misreading bytes.

#[test]
fn test_jit_pic_slot_offsets() {
    let slot = JitPICSlot::new();
    let base = &slot as *const JitPICSlot as usize;
    for i in 0..JIT_PIC_ENTRIES {
        let actual = (&slot.class_ids[i] as *const _ as usize) - base;
        assert_eq!(
            actual,
            JitPICSlot::CLASS_ID_OFFSETS[i],
            "CLASS_ID_OFFSETS[{i}] drift: {actual} vs {}",
            JitPICSlot::CLASS_ID_OFFSETS[i]
        );
        let actual = (&slot.entry_words[i] as *const _ as usize) - base;
        assert_eq!(
            actual,
            JitPICSlot::ENTRY_PTR_OFFSETS[i],
            "ENTRY_PTR_OFFSETS[{i}] drift: {actual} vs {}",
            JitPICSlot::ENTRY_PTR_OFFSETS[i]
        );
    }
    // The per-way needs-context bytes moved into the entry words; the 8
    // reserved bytes keep the counters (and the hashed table after them)
    // where generated code expects them.
    assert_eq!((&slot.hits as *const _ as usize) - base, 56);
    assert_eq!((&slot.misses as *const _ as usize) - base, 88);
}

// ── JitCodeRegion tests ─────────────────────────────────────────

#[test]
fn test_jit_code_region_register_and_contains() {
    let mut region = JitCodeRegion::new();
    let fake_ptr = 0x1000 as *const u8;
    assert!(!region.contains(fake_ptr));
    region.register(fake_ptr, 256);
    assert!(region.contains(fake_ptr));
    // Middle of region
    assert!(region.contains(0x1080 as *const u8));
    // One past end should NOT be contained
    assert!(!region.contains(0x1100 as *const u8));
}

#[test]
fn test_jit_code_region_deregister() {
    let mut region = JitCodeRegion::new();
    let fake_ptr = 0x2000 as *const u8;
    region.register(fake_ptr, 128);
    assert!(region.contains(fake_ptr));
    region.deregister(fake_ptr);
    assert!(!region.contains(fake_ptr));
}

/// The registry is split into a shared `base` and a small `recent` half so a
/// registration does not copy the whole table. Every answer must still be
/// the answer one sorted table gives, across merges and across removals
/// from either half.
#[test]
fn code_ranges_answer_like_one_sorted_table_across_merges_and_removals() {
    // A band no real mapping uses, so parallel tests cannot collide with it.
    const BAND: usize = 0x7f3c_0000_0000;
    const N: usize = 300;
    let in_band = |start: usize| (BAND..BAND + N * 0x100).contains(&start);
    let entry_of = |i: usize| BAND + i * 0x100;
    // Register out of order, so both halves see interleaved inserts.
    for i in (0..N).rev().step_by(2).chain((0..N).step_by(2)) {
        register_jit_code_range(entry_of(i), 0x80, entry_of(i) + 1);
    }
    for i in 0..N {
        assert_eq!(
            lookup_jit_code_range(entry_of(i) + 0x7c),
            Some(entry_of(i) + 1)
        );
        assert_eq!(
            lookup_jit_code_range(entry_of(i) + 0x80),
            None,
            "gap after {i}"
        );
    }
    let band: Vec<(usize, usize)> = jit_code_ranges_snapshot()
        .into_iter()
        .filter(|(start, _)| in_band(*start))
        .collect();
    assert_eq!(band.len(), N);
    assert!(
        band.windows(2).all(|w| w[0].0 < w[1].0),
        "snapshot must be sorted"
    );
    let mut buf = Vec::new();
    snapshot_code_ranges_into(&mut buf);
    assert!(
        buf.windows(2).all(|w| w[0].0 <= w[1].0),
        "copy must be sorted"
    );

    // Remove every third range: some live in `base`, the latest in `recent`.
    for i in (0..N).step_by(3) {
        unregister_jit_code_range(entry_of(i));
        // Removing an absent range is a no-op, not a panic or a duplicate.
        unregister_jit_code_range(entry_of(i));
    }
    for i in 0..N {
        let expect = (i % 3 != 0).then_some(entry_of(i) + 1);
        assert_eq!(lookup_jit_code_range(entry_of(i)), expect, "range {i}");
    }
    for i in 0..N {
        unregister_jit_code_range(entry_of(i));
    }
    assert!(jit_code_ranges_snapshot()
        .into_iter()
        .all(|(start, _)| !in_band(start)));
}

/// Compile ids are recycled, but only once the retirement generation stamped
/// at release has been graced. A mirror still holding a released id must
/// not resolve it to a different body. A full table is refused, not wrapped.
#[test]
fn compile_ids_are_recycled_only_after_grace() {
    let mut ids = CompileIdAllocator::new(4);
    assert_eq!(ids.reserve(|_| true), Some(1), "0 is never issued");
    assert_eq!(ids.reserve(|_| true), Some(2));
    ids.release(1, 10);
    // Not graced yet: a fresh id is issued instead.
    assert_eq!(ids.reserve(|stamp| stamp < 10), Some(3));
    // Graced: the released id comes back, oldest first.
    assert_eq!(ids.reserve(|stamp| stamp <= 10), Some(1));
    ids.release(2, 11);
    // The table is full and nothing is graced: refused.
    assert_eq!(ids.reserve(|_| false), None);
    assert_eq!(ids.reserve(|_| true), Some(2));
    assert_eq!(ids.reserve(|_| true), None);

    // The process table: reserve, bind, lock-free lookup, release.
    let id = reserve_compile_id();
    assert_ne!(id, 0);
    assert_eq!(lookup_compile_id(id), None, "reserved but unbound");
    let fake_cm = 0x7f3d_0000_1000usize;
    bind_compile_id(id, fake_cm);
    assert_eq!(lookup_compile_id(id), Some(fake_cm));
    release_compile_id(id);
    // (A parallel test may already have been reissued the id once graced,
    // so the assertion is only that it no longer names this body.)
    assert_ne!(lookup_compile_id(id), Some(fake_cm));
}

/// A reservation owns its id until it is handed off. Handed off, the id
/// stays reserved when the reservation drops (the artifact releases it);
/// an empty reservation releases nothing. The drop-before-hand-off path is
/// `release_compile_id`, pinned above; asserting it here would race a
/// parallel test reissuing the id once graced.
#[test]
fn a_handed_off_compile_id_outlives_its_reservation() {
    use std::sync::atomic::Ordering::Acquire;
    let mut reservation = CompileIdReservation::reserve();
    let id = reservation.hand_off();
    assert_ne!(id, 0, "a fresh process table has ids to issue");
    assert_eq!(reservation.id(), 0, "hand_off leaves nothing to release");
    drop(reservation);
    let slot = compile_id_slot(id).expect("a reserved id has a slot");
    assert_eq!(
        slot.load(Acquire),
        COMPILE_ID_RESERVED,
        "dropping a handed-off reservation must not release the artifact's id"
    );
    release_compile_id(id);
    drop(CompileIdReservation::none());
}

#[test]
fn test_jit_code_region_multiple_regions() {
    let mut region = JitCodeRegion::new();
    region.register(0x1000 as *const u8, 256);
    region.register(0x3000 as *const u8, 512);
    assert!(region.contains(0x1080 as *const u8));
    assert!(region.contains(0x3100 as *const u8));
    assert!(!region.contains(0x2000 as *const u8));
}

// ── validate_code_ptr tests ─────────────────────────────────────

/// The memo must not change any ANSWER, only the cost of reaching it.
///
/// Both arms are exercised in one process on purpose: routing this through
/// `CRATONVM_JIT_NO_CODE_PTR_MEMO` would be a `set_var` race against every
/// other test in this binary (see
/// `reference_set_var_in_a_parallel_test_suite_is_a_data_race`), so the
/// test drives the two implementations directly instead.
#[test]
fn code_ptr_memo_agrees_with_the_locked_lookup_on_every_probe() {
    // A real region, so both arms have something to find.
    let buf = ExecutableBuffer::new(4096).expect("executable buffer");
    let base = buf.as_ptr() as usize;
    let probes = [
        base,
        base + 4,
        base + 4092,
        base + 4096, // one past the end
        base.wrapping_sub(4),
        0x1000,
    ];
    for p in probes {
        let ptr = p as *const u8;
        let memo_answer = validate_code_ptr(ptr).is_ok();
        let locked_answer = {
            let regions = jit_code_regions().lock().unwrap_or_else(|e| e.into_inner());
            !ptr.is_null() && (p % 4 == 0) && regions.contains(ptr)
        };
        assert_eq!(
            memo_answer,
            locked_answer,
            "memo and locked lookup disagree at {p:#x} (region {base:#x}..{:#x})",
            base + 4096
        );
    }
    // A second pass, now that the memo is warm, must give the same answers:
    // a warm memo that starts admitting addresses outside its region is the
    // failure this whole design has to exclude.
    for p in probes {
        let ptr = p as *const u8;
        let memo_answer = validate_code_ptr(ptr).is_ok();
        let locked_answer = {
            let regions = jit_code_regions().lock().unwrap_or_else(|e| e.into_inner());
            !ptr.is_null() && (p % 4 == 0) && regions.contains(ptr)
        };
        assert_eq!(memo_answer, locked_answer, "warm memo disagrees at {p:#x}");
    }
}

/// A dropped buffer must stop validating, memo or no memo.
///
/// This is the one thing an address-keyed cache can get wrong, and the only
/// reason [`REGIONS_EPOCH`] exists. Without the epoch the memo would keep
/// admitting a pointer into a region that has been unmapped -- and the
/// caller's next act is `transmute` and `call`.
#[test]
fn dropping_a_region_invalidates_a_warm_memo() {
    let (base, len) = {
        let buf = ExecutableBuffer::new(4096).expect("executable buffer");
        let base = buf.as_ptr() as usize;
        // Warm the memo on this region.
        assert!(
            validate_code_ptr(base as *const u8).is_ok(),
            "a live region must validate"
        );
        (base, 4096usize)
    };
    // `buf` is dropped: `deregister` ran and bumped the epoch.
    let _ = len;
    assert!(
        validate_code_ptr(base as *const u8).is_err(),
        "a pointer into an unmapped region must stop validating even though \
         the memo was warm for it"
    );
}

/// The epoch is what the memo trusts, so it must actually move.
#[test]
fn registering_and_deregistering_move_the_regions_epoch() {
    let before = REGIONS_EPOCH.load(std::sync::atomic::Ordering::Acquire);
    let buf = ExecutableBuffer::new(4096).expect("executable buffer");
    let after_register = REGIONS_EPOCH.load(std::sync::atomic::Ordering::Acquire);
    assert!(
        after_register > before,
        "register must bump the epoch ({before} -> {after_register})"
    );
    drop(buf);
    let after_drop = REGIONS_EPOCH.load(std::sync::atomic::Ordering::Acquire);
    assert!(
        after_drop > after_register,
        "deregister must bump the epoch ({after_register} -> {after_drop})"
    );
}

/// The kill switch's `off_key` must be the key the reader reads.
///
/// Mirrors `native_site_cache_default_is_on_and_the_kill_switch_kills`: a
/// kill switch wired to a key nothing reads is a switch that reports itself
/// present and does nothing.
#[test]
fn code_ptr_memo_default_is_on_and_its_kill_switch_is_declared() {
    assert!(
        code_ptr_memo_enabled(),
        "the memo is default-ON; no test in this binary sets \
         CRATONVM_JIT_NO_CODE_PTR_MEMO"
    );
    let entry = cratonvm_types::flag_groups::INVENTORY
        .iter()
        .find(|e| e.token == "code-ptr-memo")
        .expect("code-ptr-memo must be a declared JIT token");
    assert_eq!(
        entry.off_key,
        Some("CRATONVM_JIT_NO_CODE_PTR_MEMO"),
        "the kill switch's off_key must be the key `code_ptr_memo_enabled` reads"
    );
    assert_eq!(entry.on_key, None, "a default-ON kill switch has no on_key");
}

#[test]
fn test_validate_code_ptr_null() {
    assert_eq!(
        validate_code_ptr(std::ptr::null()),
        Err("null JIT code pointer")
    );
}

#[test]
fn test_validate_code_ptr_misaligned() {
    assert_eq!(
        validate_code_ptr(0x1001 as *const u8),
        Err("misaligned JIT code pointer")
    );
}

// ── ExecutableBuffer tests ──────────────────────────────────────

#[test]
fn test_executable_buffer_new_and_emit() {
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    assert_eq!(buf.pos(), 0);
    buf.emit(&[0x90, 0x90, 0x90]); // NOP NOP NOP
    assert_eq!(buf.pos(), 3);
    assert_eq!(buf.as_slice(), &[0x90, 0x90, 0x90]);
}

#[test]
fn test_executable_buffer_emit_byte() {
    let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
    buf.emit_byte(0xCC);
    buf.emit_byte(0xC3);
    assert_eq!(buf.pos(), 2);
    assert_eq!(buf.as_slice(), &[0xCC, 0xC3]);
}

/// An erased range must be JUMPED over, not merely filled with NOPs.
///
/// This is the failure mode that has to be pinned by a byte assertion
/// rather than by behaviour: a range of `0x90` is perfectly *correct*, it
/// just executes. The IR backend shipped exactly that for the ~46-byte
/// shadow-stack thread fetch, and `CratonBench fib` — entered 2.27e9
/// times — ran ~2x slower than on the single-pass body. Nothing failed;
/// it was only slow. So assert the opcode.
#[test]
fn erase_range_with_jump_over_emits_a_jump_not_a_nop_sled() {
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    // A 46-byte span, the size the IR thread fetch actually occupies,
    // between a leading and a trailing sentinel.
    buf.emit_byte(0xCC);
    let start = buf.pos();
    buf.emit(&[0xAA; 46]);
    let end = buf.pos();
    buf.emit_byte(0xC3);

    buf.erase_range_with_jump_over(start, end);

    let code = buf.as_slice();
    assert_eq!(code[0], 0xCC, "byte before the range must not move");
    assert_eq!(code[start], 0xEB, "range must begin with JMP rel8");
    assert_eq!(
        code[start + 1],
        44,
        "displacement must land exactly on the first byte after the range"
    );
    assert!(
        code[start + 2..end].iter().all(|&b| b == 0x90),
        "the skipped remainder should be NOP-filled"
    );
    assert_eq!(code[end], 0xC3, "byte after the range must not move");
    assert!(
        !buf.overflowed(),
        "a 46-byte erase must not bail the buffer"
    );
}

/// A span too short to hold `JMP rel8`, or too long for its displacement,
/// still has to be erased — just without the jump.
#[test]
fn erase_range_with_jump_over_falls_back_to_nops_when_a_jump_will_not_fit() {
    // 1 byte: no room for the 2-byte JMP.
    let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
    buf.emit(&[0xAA, 0xC3]);
    buf.erase_range_with_jump_over(0, 1);
    assert_eq!(buf.as_slice()[0], 0x90);
    assert_eq!(buf.as_slice()[1], 0xC3, "the range end is exclusive");

    // 130 bytes: `len - 2` exceeds the i8 displacement range.
    let mut big = ExecutableBuffer::new(256).expect("alloc failed");
    big.emit(&[0xAA; 130]);
    big.erase_range_with_jump_over(0, 130);
    assert!(
        big.as_slice()[..130].iter().all(|&b| b == 0x90),
        "an over-long span must fall back to a full NOP fill, never a truncated rel8"
    );
    assert!(!big.overflowed(), "the fallback must not bail the compile");
}

/// An empty or inverted range is a no-op, not a panic or a stray patch.
#[test]
fn erase_range_with_jump_over_ignores_an_empty_range() {
    let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
    buf.emit(&[0xAA, 0xBB]);
    buf.erase_range_with_jump_over(1, 1);
    buf.erase_range_with_jump_over(2, 1);
    assert_eq!(buf.as_slice(), &[0xAA, 0xBB]);
    assert!(!buf.overflowed());
}

#[test]
fn test_executable_buffer_emit_checked_success() {
    let mut buf = ExecutableBuffer::new(8).expect("alloc failed");
    assert!(buf.emit_checked(&[1, 2, 3, 4]));
    assert_eq!(buf.pos(), 4);
}

#[test]
fn test_executable_buffer_emit_checked_overflow() {
    let mut buf = ExecutableBuffer::new(4).expect("alloc failed");
    buf.emit(&[1, 2, 3]);
    // Only 1 byte left, trying to emit 2 should fail
    assert!(!buf.emit_checked(&[4, 5]));
    // Position should be unchanged
    assert_eq!(buf.pos(), 3);
}

#[test]
fn test_executable_buffer_patch_and_read_i32() {
    let mut buf = ExecutableBuffer::new(32).expect("alloc failed");
    buf.emit(&[0; 8]); // 8 zero bytes
    buf.try_patch_i32(0, 0x12345678).expect("in-bounds patch");
    assert_eq!(buf.read_i32(0), 0x12345678);
    buf.try_patch_i32(4, -42).expect("in-bounds patch");
    assert_eq!(buf.read_i32(4), -42);
}

#[test]
fn test_executable_buffer_emit_overflow_marks_flag() {
    // An emit past capacity must NOT panic: it records `overflowed` and
    // skips the write so the compile driver can bail gracefully.
    let mut buf = ExecutableBuffer::new(4).expect("alloc failed");
    assert!(!buf.overflowed());
    buf.emit(&[1, 2, 3, 4, 5]); // 5 bytes into 4-capacity buffer
    assert!(buf.overflowed());
    assert_eq!(buf.pos(), 0, "overflowing emit must not advance len");
    buf.emit_byte(0xCC);
    assert_eq!(buf.pos(), 0, "overflowing emit_byte must not advance len");
}

#[test]
fn test_try_patch_i32_out_of_bounds_returns_err_does_not_panic() {
    // Task #20: a patch site pointing past `len` must return
    // `Err(PatchFailed)`, mark the buffer overflowed, and never panic.
    // The compile driver relies on the overflow flag to bail to the
    // interpreter when codegen has slipped past its size estimate.
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    buf.emit(&[0; 8]); // only 8 bytes emitted, so offset 8..=11 is OOB
    let result = buf.try_patch_i32(8, 0xDEAD_BEEFu32 as i32);
    assert!(matches!(
        result,
        Err(CompileError::PatchFailed {
            kind: "i32",
            offset: 8
        })
    ));
    assert!(
        buf.overflowed(),
        "OOB patch must set the sticky overflow flag for the compile driver"
    );

    // Same contract for try_patch_byte.
    let mut buf2 = ExecutableBuffer::new(64).expect("alloc failed");
    buf2.emit(&[0; 2]);
    let result2 = buf2.try_patch_byte(99, 0xCC);
    assert!(matches!(
        result2,
        Err(CompileError::PatchFailed {
            kind: "byte",
            offset: 99
        })
    ));
    assert!(buf2.overflowed());
}

#[test]
fn test_try_call_nine_args_returns_too_many_args_err_no_panic() {
    // Task #20: invoking a compiled method with more arguments than
    // the JIT's hand-rolled call thunks support must return
    // `Err(TooManyArgs)` rather than silently returning 0 or
    // panicking. Both `try_call` and `try_call_with_context` honor
    // this contract.
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    buf.emit(&[0xC3]); // RET — just a valid landing pad
    let cm = CompiledMethod::new(buf);

    // 9 args exceeds the 8-arg ceiling of `try_call`.
    let nine = [0i64; 9];
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let r = unsafe { cm.try_call(&nine) };
    assert!(
        matches!(r, Err(CompileError::TooManyArgs(9))),
        "expected TooManyArgs(9), got {r:?}"
    );

    // 8 args exceeds the 7-arg ceiling of `try_call_with_context`
    // (one register is consumed by the implicit vm_ptr).
    let mut buf2 = ExecutableBuffer::new(64).expect("alloc failed");
    buf2.emit(&[0xC3]);
    let cm2 = CompiledMethod::new_with_context(buf2);
    let eight = [0i64; 8];
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let r2 = unsafe { cm2.try_call_with_context(0, &eight) };
    assert!(
        matches!(r2, Err(CompileError::TooManyArgs(8))),
        "expected TooManyArgs(8) for context call, got {r2:?}"
    );
}

#[test]
fn test_try_call_returns_ok_for_in_bounds_zero_arg_method() {
    // Task #44: positive test. After removing the panicking `call`
    // wrapper, `try_call` is the canonical happy-path entry point.
    // A minimal compiled method that returns 0 (XOR EAX,EAX; RET)
    // must produce `Ok(0)` when invoked with no arguments.
    // Encoded as: 31 C0 (xor eax, eax) C3 (ret).
    let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
    buf.emit(&[0x31, 0xC0, 0xC3]);
    let cm = CompiledMethod::new(buf);
    // SAFETY: the emitted code is a well-formed x86-64 leaf (XOR
    // EAX,EAX; RET) using the platform C ABI for a no-arg
    // `extern "C" fn() -> i64`. CompiledMethod::new finalized the
    // buffer, so the page is executable.
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        let r = unsafe { cm.try_call(&[]) };
        assert_eq!(r, Ok(0), "try_call({{}}) should return Ok(0)");
    }
    // On non-x86_64 targets the emitted bytes are not valid, so
    // suppress the call but still exercise the type-level
    // try_call contract (TooManyArgs path) to keep the test
    // platform-independent.
    #[cfg(not(target_arch = "x86_64"))]
    {
        let big = [0i64; 9];
        let r = unsafe { cm.try_call(&big) };
        assert!(matches!(r, Err(CompileError::TooManyArgs(9))));
    }
}

#[test]
fn test_try_call_invalid_code_ptr_returns_err_not_silent_zero() {
    // Task #44 acceptance criterion 5: demonstrate that a JIT
    // "compile-failed" / runtime-invalid-pointer result propagates
    // as `Err(CompileError::InvalidCodePtr(_))` instead of being
    // silently downgraded to a `0` return value (which is what the
    // now-removed `call` wrapper did via `tracing::warn!` +
    // return 0).
    //
    // We synthesize the invalid-pointer condition by constructing
    // a `CompiledMethod` whose `entry` field has been overwritten
    // with a value outside any known JIT code region. The pointer
    // is also misaligned (1 byte) so `validate_code_ptr` would
    // reject it on alignment alone even if the region check ever
    // changes.
    let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
    buf.emit(&[0xC3]);
    let mut cm = CompiledMethod::new(buf);
    // Stomp on the entry pointer: misaligned + outside any region.
    cm.entry = 0x1 as *const u8;

    // SAFETY: the call is gated by `validate_code_ptr` inside
    // `try_call`, which rejects the synthetic pointer before any
    // transmute-to-fn-pointer happens. No real code is executed.
    let r = unsafe { cm.try_call(&[]) };
    assert!(
        matches!(r, Err(CompileError::InvalidCodePtr(_))),
        "expected InvalidCodePtr, got {r:?} (silent-zero would be Ok(0) — that contract is gone)"
    );

    // Equivalent contract for the context-needing variant: must
    // also surface InvalidCodePtr rather than swallowing it.
    let mut buf2 = ExecutableBuffer::new(16).expect("alloc failed");
    buf2.emit(&[0xC3]);
    let mut cm2 = CompiledMethod::new_with_context(buf2);
    cm2.entry = 0x3 as *const u8;
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let r2 = unsafe { cm2.try_call_with_context(0, &[]) };
    assert!(
        matches!(r2, Err(CompileError::InvalidCodePtr(_))),
        "expected InvalidCodePtr for context call, got {r2:?}"
    );
}

/// Both sides of the `CRATONVM_JIT_OSR_DEAD_LOCALS` kill switch, through
/// the pure `can_osr_enter_with` so one process can pin both. The
/// no-native-offset refusal must hold either way: that one is not a
/// dead-local question at all.
#[test]
fn test_can_osr_enter_dead_masked_entry_both_sides() {
    let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    let mut cm = CompiledMethod::new(buf);
    cm.osr_pc_to_native = Some(vec![-1, 0, 4]);
    cm.osr_dead_mask = Some(vec![0, 0x80, 0]);

    for allow in [false, true] {
        assert!(
            !cm.can_osr_enter_with(0, allow),
            "negative native offset must bail regardless of the dead-local knob (allow={allow})"
        );
        assert!(
            cm.can_osr_enter_with(2, allow),
            "zero dead mask remains OSR-eligible (allow={allow})"
        );
    }
    assert!(
        !cm.can_osr_enter_with(1, false),
        "CRATONVM_JIT_OSR_DEAD_LOCALS=0 restores the blanket refusal"
    );
    assert!(
        cm.can_osr_enter_with(1, true),
        "default: the trampoline skips the masked locals, so the entry is admitted"
    );
}

/// The public entry point agrees with the pure one under this process's
/// (latched) flag value, whichever way the environment set it.
#[test]
fn test_can_osr_enter_matches_the_flagged_variant() {
    let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    let mut cm = CompiledMethod::new(buf);
    cm.osr_pc_to_native = Some(vec![0]);
    cm.osr_dead_mask = Some(vec![0x80]);

    let allow = osr_dead_local_entry_allowed();
    assert_eq!(cm.can_osr_enter(0), cm.can_osr_enter_with(0, allow));
}

/// `osr_enter`'s dead-mask short-circuit, pinned on the one side a unit
/// test can pin.
///
/// Only the REFUSING side is assertable here. Admitting means actually
/// running `osr_trampoline`, which builds a frame (pushes rbp, subtracts
/// `osr_frame_size`, spills the callee-saved set at `osr_callee_saved_base`)
/// and jumps to the target — so a stub body has to be a real compiled
/// method with matching frame metadata, not a bare `RET`. Entering a `RET`
/// stub returns into the middle of the trampoline's own prologue and
/// access-violates; an earlier version of this test did exactly that and
/// took the whole `cratonvm-jit` binary down with 0xC0000005.
///
/// The admitted path's coverage is `probes/OsrDeadLocalProbe.java`, which
/// executes eight real coalescing shapes end to end and checksums every
/// result against HotSpot. The decision itself is pinned above by
/// `test_can_osr_enter_dead_masked_entry_both_sides`.
#[cfg(target_arch = "x86_64")]
#[test]
fn test_osr_enter_refuses_dead_mask_under_the_kill_switch() {
    let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
    buf.emit(&[0xC3]); // RET — never entered on this path.
    let mut cm = CompiledMethod::new(buf);
    cm.osr_pc_to_native = Some(vec![0]);
    cm.osr_dead_mask = Some(vec![0x80]);

    if osr_dead_local_entry_allowed() {
        // Default: the refusal is gone, so there is nothing to assert that
        // does not require entering the stub. Assert the decision instead.
        assert!(cm.can_osr_enter_with(0, true));
    } else {
        assert_eq!(
            // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
            unsafe { cm.osr_enter(0, &[], 0, 0) },
            None,
            "CRATONVM_JIT_OSR_DEAD_LOCALS=0 must still bail before the trampoline"
        );
    }
}

// ── Validated OSR entry (C2 review P1, "Add on-stack replacement") ──
//
// The loop these tests model, with javac's bcis:
//
//     0: iconst_0                  // int i = 0
//     1: istore_1
//     2: goto 8
//     5: <body>                    // sum += a[i]; i++
//     8: iload_1                   // <-- LOOP HEADER = the OSR entry bci
//     9: … if_icmplt 5             // the back-edge targets bci 8
//
// Locals: 0 = `int[] a` (ref), 1 = `int i`, 2 = `int sum`.
//
// Every assertion below is on the *pure* validator and on the pure
// post-exit resume decision. Neither enters compiled code: these fixtures
// carry OSR metadata over a bare `RET` body, and actually entering such a
// stub through the trampoline access-violates (see
// `test_osr_enter_refuses_dead_mask_under_the_kill_switch` for the history).

/// The loop-header bci used by every fixture in this group.
const OSR_T_HEADER: usize = 8;

/// An artifact that publishes exactly one OSR entry, at [`OSR_T_HEADER`],
/// with `num_locals` memory-homed locals and no deopt metadata (the
/// production shape: `deopt_points` is empty unless `CRATONVM_DEOPT_REAL`
/// was on at compile time).
fn osr_t_artifact(num_locals: usize) -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
    buf.emit(&[0xC3]); // RET — metadata-only fixture, never executed.
    let mut cm = CompiledMethod::new(buf);
    cm.method_label = "craton/probe/OsrEntry.sum([I)I".to_string();
    let mut table = vec![-1i32; OSR_T_HEADER + 2];
    table[OSR_T_HEADER] = 0;
    cm.osr_pc_to_native = Some(table);
    cm.osr_num_locals = num_locals;
    cm.osr_local_assignments = Some(vec![None; num_locals]);
    cm.osr_xmm_assignments = Some(vec![None; num_locals]);
    cm
}

/// An `OsrExit`-tagged deopt point at `bci` — the loop-boundary snapshot
/// that doubles as the *entry* contract at the same bci.
///
/// `ResumeSemantics::for_reason(OsrExit)` is `REEXECUTE`, which is what a
/// loop-header snapshot means: the header iteration has not run.
fn osr_t_exit_point(
    bci: u32,
    locals: Vec<deopt::FrameValue>,
    stack: Vec<deopt::FrameValue>,
) -> deopt::DeoptimizationPoint {
    deopt::DeoptimizationPoint {
        native_offset: 0,
        bci,
        reason: deopt::DeoptReason::OsrExit,
        action: deopt::DeoptAction::Reinterpret,
        speculation_id: 0,
        frame_state: deopt::FrameState {
            method_key: "craton/probe/OsrEntry.sum:([I)I".to_string(),
            bci,
            locals,
            stack,
            monitors: Vec::new(),
            caller: None,
        },
        semantics: deopt::ResumeSemantics::for_reason(deopt::DeoptReason::OsrExit),
    }
}

/// The three-slot contract every fixture here uses: `[ref, int, int]`.
fn osr_t_contract_locals() -> Vec<deopt::FrameValue> {
    vec![
        deopt::FrameValue::StackSlotRef(-16),
        deopt::FrameValue::Register(0),
        deopt::FrameValue::Register(1),
    ]
}

/// The interpreter tags for `[int[] a, int i, int sum]`.
const OSR_T_TAGS: [u8; 3] = [
    cratonvm_types::VTAG_OBJECT,
    cratonvm_types::VTAG_INT,
    cratonvm_types::VTAG_INT,
];

/// The refusal tag carried by a bailout, or `None` if it is not an OSR
/// refusal at all.
fn osr_t_tag(b: &bailout::Bailout) -> Option<&'static str> {
    match &b.reason {
        bailout::BailoutReason::UnsupportedShape(tag) => Some(*tag),
        _ => None,
    }
}

/// The taxonomy is a closed set with no duplicates, and every tag routes to
/// the `unsupported_shape` bailout counter (never a panic, never a bare
/// `None`).
#[test]
fn osr_refusal_taxonomy_is_closed_and_counted() {
    let mut seen: Vec<&str> = Vec::new();
    for tag in OSR_REFUSAL_TAGS {
        assert!(!seen.contains(&tag), "duplicate OSR refusal tag {tag}");
        assert!(
            tag.starts_with("osr-entry-") || tag.starts_with("osr-exit-"),
            "{tag} does not name its phase"
        );
        seen.push(tag);
    }
    let before = bailout::bailout_count("unsupported_shape").expect("registered category");
    let b = osr_refusal(OSR_REFUSE_SLOT_TYPE, "probe");
    assert_eq!(b.category(), "unsupported_shape");
    assert!(b.to_string().contains(OSR_REFUSE_SLOT_TYPE), "{b}");
    assert!(
        bailout::bailout_count("unsupported_shape").unwrap() > before,
        "every OSR refusal must be counted"
    );
    // Permanence splits the taxonomy: artifact-only answers may be memoed
    // through `mark_osr_entry_rejected`; state-dependent ones may not.
    for tag in OSR_PERMANENT_REFUSAL_TAGS {
        assert!(
            OSR_REFUSAL_TAGS.contains(&tag),
            "{tag} is permanent but not in the taxonomy"
        );
        assert!(osr_refusal_is_permanent(&osr_refusal(tag, "probe")));
    }
    assert!(!osr_refusal_is_permanent(&osr_refusal(
        OSR_REFUSE_SLOT_TYPE,
        "probe"
    )));
    assert!(!osr_refusal_is_permanent(&osr_refusal(
        OSR_REFUSE_OPERAND_STACK,
        "probe"
    )));
    // A non-OSR bailout is never an OSR memo candidate.
    assert!(!osr_refusal_is_permanent(&bailout::Bailout::new(
        bailout::BailoutReason::RegisterPressure
    )));
}

// ── osr-01 item 4: the two views of one frame ────────────────────
//
// `osr_entry_frame_state` (the precise contract `validate_osr_entry`
// type-checks against) and the register homes (`osr_local_assignments` /
// `osr_xmm_assignments`, which is what `osr_trampoline` seeds through) are
// two views of the same compile. Until now nothing compared them, so an
// artifact could validate one entry and perform another.
//
// The fixtures below build the disagreement directly rather than trying to
// provoke it out of a real compile — a compiler bug that has never been
// observed cannot be reproduced, and the point of the check is that it
// would be caught if it ever happened.

/// An artifact with a precise contract at the header: `[ref, int, int]`.
fn osr_t_precise(num_locals: usize) -> CompiledMethod {
    let mut cm = osr_t_artifact(num_locals);
    cm.deopt_points = vec![osr_t_exit_point(
        OSR_T_HEADER as u32,
        osr_t_contract_locals(),
        Vec::new(),
    )];
    cm
}

/// The baseline the three refusals below are perturbations of: a precise
/// contract over memory-homed locals agrees with the register-home view
/// vacuously, because a memory-homed slot admits every type.
///
/// Stated as its own test so a check that refused *everything* could not
/// pass the refusal tests below and look correct.
#[test]
fn a_memory_homed_precise_contract_has_no_home_disagreement() {
    let cm = osr_t_precise(3);
    let locals = [0x1234_5678i64, 200, 4950];
    assert!(cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .is_ok());
}

/// The corruption this check exists for, in its most direct form: the
/// contract says local 1 is a `double`, but its only home is a GPR. The
/// trampoline moves the double's bits into a general-purpose register the
/// compiled body reads as an integer, and the frame-slot store is elided
/// because a home exists — so nothing downstream can notice.
#[test]
fn a_wide_fp_local_homed_only_in_a_gpr_refuses_the_entry() {
    let mut cm = osr_t_precise(3);
    cm.deopt_points[0].frame_state.locals[1] = deopt::FrameValue::XmmDouble(2);
    cm.osr_local_assignments = Some(vec![None, Some(3), None]);
    let locals = [0x1234_5678i64, 0x4008_0000_0000_0000u64 as i64, 4950];
    let tags = [
        cratonvm_types::VTAG_OBJECT,
        cratonvm_types::VTAG_DOUBLE,
        cratonvm_types::VTAG_INT,
    ];
    let err = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &tags))
        .expect_err("a double with only a GPR home is a compiler bug, not an entry");
    assert_eq!(osr_t_tag(&err), Some(OSR_REFUSE_CONTRACT_DISAGREEMENT));
    let msg = err.to_string();
    assert!(msg.contains("local 1"), "{msg}");
    assert!(msg.contains("double"), "{msg}");
}

/// The other direction, and the more dangerous one: a reference whose only
/// home is an XMM register is seeded into the FP file with the frame-slot
/// store elided, so the GC's precise map has nothing to find and the body
/// reads a stale word.
#[test]
fn a_reference_homed_only_in_an_xmm_refuses_the_entry() {
    let mut cm = osr_t_precise(3);
    cm.osr_xmm_assignments = Some(vec![Some(1), None, None]);
    let locals = [0x1234_5678i64, 200, 4950];
    let err = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect_err("a reference with only an XMM home must be refused");
    assert_eq!(osr_t_tag(&err), Some(OSR_REFUSE_CONTRACT_DISAGREEMENT));
    assert!(err.to_string().contains("local 0"), "{err}");
}

/// Over-refusal check, and the reason `osr_homes_admit` answers with a
/// *set*. A JVM slot index is legally reused by locals of different types
/// in disjoint live ranges, so the allocator can give one slot BOTH a GPR
/// and an XMM home — `DualPivotQuicksort.mixedInsertionSort` does exactly
/// this. The trampoline seeds both, so neither home contradicts the other.
#[test]
fn a_slot_with_both_register_homes_is_not_a_disagreement() {
    let mut cm = osr_t_precise(3);
    cm.osr_local_assignments = Some(vec![None, Some(3), None]);
    cm.osr_xmm_assignments = Some(vec![None, Some(1), None]);
    let locals = [0x1234_5678i64, 200, 4950];
    assert!(
        cm.validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
            .is_ok(),
        "a slot reused across disjoint live ranges may carry both homes"
    );
}

/// A masked-dead slot is not seeded at all, so its homes describe nothing
/// that happens at this entry and must not be cross-checked. Without this
/// the check would refuse entries the dead mask exists to make safe.
#[test]
fn a_masked_dead_slot_is_not_cross_checked() {
    let mut cm = osr_t_precise(3);
    cm.osr_xmm_assignments = Some(vec![Some(1), None, None]);
    let mut mask = vec![0u64; OSR_T_HEADER + 2];
    mask[OSR_T_HEADER] = 0b1; // local 0 is dead here
    cm.osr_dead_mask = Some(mask);
    let locals = [0x1234_5678i64, 200, 4950];
    // The kill switch turns a non-zero mask into a blanket refusal, which
    // would make this pass for the wrong reason.
    if !osr_dead_local_entry_allowed() {
        return;
    }
    assert!(
        cm.validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
            .is_ok(),
        "a slot the trampoline skips carries no home claim"
    );
}

/// A snapshot shorter than the compiled frame describes a prefix; the tail
/// is simply untyped, not contradicted. (`validate_osr_entry` already falls
/// back to the register-home expectation for those slots.)
#[test]
fn a_short_snapshot_does_not_manufacture_a_disagreement() {
    let mut cm = osr_t_precise(3);
    cm.deopt_points[0].frame_state.locals.truncate(1);
    cm.osr_xmm_assignments = Some(vec![None, None, Some(3)]);
    let locals = [0x1234_5678i64, 200, 4950];
    let tags = [
        cratonvm_types::VTAG_OBJECT,
        cratonvm_types::VTAG_INT,
        cratonvm_types::VTAG_DOUBLE,
    ];
    assert!(cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &tags))
        .is_ok());
}

/// The verdict is a pure function of the artifact and the entry pc — it
/// does not read the offered locals — so it may be memoed. Asserted here as
/// well as in the taxonomy test because memoing a *state-dependent* refusal
/// is the same bug in the other direction: a silent, permanent loss of OSR.
#[test]
fn the_home_disagreement_refusal_is_memoable() {
    assert!(osr_refusal_is_permanent(&osr_refusal(
        OSR_REFUSE_CONTRACT_DISAGREEMENT,
        "probe"
    )));
}

/// A hot loop's OSR request must name the loop header, because the entry
/// bci is what the compiled artifact publishes a native offset for — and
/// the loop, which keeps running while the compile is queued, must not
/// enqueue a task per iteration.
///
/// This used to drive `TieredCompilationManager::on_backedge` across its
/// `osr_threshold`. That door had no production caller (the interpreter's
/// per-frame back-edge schedule is the throttle, and it calls
/// `request_osr`) and was deleted.
#[test]
fn a_long_running_loop_requests_osr_at_the_backedge() {
    use tiered::{CompilationTier, MethodKey, TieredCompilationManager};
    // A manager of our own, and a method key no other test names: OSR
    // denials and per-method state are per manager, so nothing here is
    // shared with another test.
    let mgr = TieredCompilationManager::with_default_policy();
    let key = MethodKey::new("craton/probe/OsrEntry", "sum", "([I)I");
    let bci = OSR_T_HEADER as u32;

    let task = mgr
        .request_osr(&key, bci)
        .expect("a hot back-edge must request an OSR compile");
    assert_eq!(
        task.osr_bci,
        Some(bci),
        "the request must name the loop header, not the method entry"
    );
    assert_eq!(task.target_tier, CompilationTier::C2);
    assert_eq!(task.method_key, key);
    for i in 0..63 {
        assert!(
            mgr.request_osr(&key, bci).is_none(),
            "back-edge {i}: an already-queued OSR request must not be re-enqueued"
        );
    }
    assert_eq!(mgr.queue_size(), 1);
}

/// The JIT key hash XOR-folded per-component hashes, so equal components
/// cancelled and swapped components collided. The negative memos key on it
/// without the cache's full-key check, so a collision there refused an
/// innocent method.
#[test]
fn jit_key_hash_does_not_collide_on_equal_or_swapped_components() {
    let id = cratonvm_types::ClassId::new(0);
    assert_ne!(
        compute_jit_key_hash("Foo", "Foo", "()V", id),
        compute_jit_key_hash("Bar", "Bar", "()V", id),
        "equal class and method names must not cancel out"
    );
    assert_ne!(
        compute_jit_key_hash("a/A", "b", "()V", id),
        compute_jit_key_hash("b", "a/A", "()V", id),
        "swapped components must not collide"
    );
    assert_ne!(
        compute_jit_key_hash("ab", "c", "()V", id),
        compute_jit_key_hash("a", "bc", "()V", id),
        "a component boundary is part of the key"
    );
    assert_ne!(
        compute_jit_key_hash("a/A", "m", "()V", id),
        compute_jit_key_hash("a/A", "m", "()V", cratonvm_types::ClassId::new(3)),
        "the declaring class id is part of the key"
    );
}

/// Verdicts go with their class, and a verdict recorded against older
/// bytecode (redefine epoch) or older compile state (install epoch) stops
/// counting.
#[test]
fn jit_verdicts_are_cleared_with_their_class_and_expire_with_their_epoch() {
    let class = "craton/test/VerdictClearing";
    let id = cratonvm_types::ClassId::new(0);
    mark_jit_bail_listed(id, class, "bailed", "()V");
    record_compile_refusal(id, class, "bailed", "()V", "test-refusal");
    mark_osr_entry_rejected(id, class, "loop", "()V", 12);
    assert!(is_jit_bail_listed(id, class, "bailed", "()V"));
    assert!(jit_bail_reason_for(id, class, "bailed", "()V").is_some());
    assert!(is_osr_entry_rejected(id, class, "loop", "()V", 12));
    assert!(
        !is_osr_entry_rejected(id, class, "loop", "()V", 13),
        "per pc"
    );
    assert!(
        !is_jit_bail_listed(id, class, "notBailed", "()V"),
        "a verdict is looked up by its exact names"
    );

    assert_eq!(forget_jit_verdicts_for_class(id, class), 2);
    assert!(!is_jit_bail_listed(id, class, "bailed", "()V"));
    assert!(jit_bail_reason_for(id, class, "bailed", "()V").is_none());
    assert!(!is_osr_entry_rejected(id, class, "loop", "()V", 12));

    // Epoch expiry, checked on the stamp itself: bumping the real epochs
    // would expire every other test's verdicts in this binary.
    let about_bytecode = VerdictStamp {
        redefine_epoch: 3,
        install_epoch: None,
    };
    assert!(about_bytecode.is_current_at(3, 100));
    assert!(
        about_bytecode.is_current_at(3, 101),
        "a bytecode verdict survives a code-cache flush"
    );
    assert!(
        !about_bytecode.is_current_at(4, 100),
        "but not a redefinition"
    );
    let about_compile_state = VerdictStamp {
        redefine_epoch: 3,
        install_epoch: Some(100),
    };
    assert!(about_compile_state.is_current_at(3, 100));
    assert!(
        !about_compile_state.is_current_at(3, 101),
        "a compile-state verdict expires with the install epoch"
    );
}

/// Two same-named classes in different loaders keep separate verdicts, and
/// forgetting one class's verdicts leaves the other's alone.
#[test]
fn same_named_classes_with_different_ids_keep_separate_verdicts() {
    let class = "craton/test/VerdictPerLoader";
    // Ids far above anything a unit test's fixtures allocate, so a forget
    // by id cannot reach another test's entries.
    let app = cratonvm_types::ClassId::new(0x7A00_0001);
    let plugin = cratonvm_types::ClassId::new(0x7A00_0002);
    let no_id = cratonvm_types::ClassId::new(0);

    mark_jit_bail_listed(app, class, "m", "()V");
    record_compile_refusal(app, class, "m", "()V", "test-refusal");
    mark_osr_entry_rejected(app, class, "loop", "()V", 7);
    assert!(is_jit_bail_listed(app, class, "m", "()V"));
    assert!(jit_bail_reason_for(app, class, "m", "()V").is_some());
    assert!(is_osr_entry_rejected(app, class, "loop", "()V", 7));
    assert!(
        !is_jit_bail_listed(plugin, class, "m", "()V"),
        "one loader's bail-list verdict must not refuse the other loader's class"
    );
    assert!(jit_bail_reason_for(plugin, class, "m", "()V").is_none());
    assert!(!is_osr_entry_rejected(plugin, class, "loop", "()V", 7));
    assert!(
        !is_jit_bail_listed(no_id, class, "m", "()V"),
        "an id-less lookup does not see an identified class's verdict"
    );

    mark_jit_bail_listed(plugin, class, "other", "()V");
    mark_osr_entry_rejected(plugin, class, "loop", "()V", 9);

    // Forgetting the app's class drops its two methods' entries only.
    assert_eq!(forget_jit_verdicts_for_class(app, class), 2);
    assert!(!is_jit_bail_listed(app, class, "m", "()V"));
    assert!(jit_bail_reason_for(app, class, "m", "()V").is_none());
    assert!(!is_osr_entry_rejected(app, class, "loop", "()V", 7));
    assert!(
        is_jit_bail_listed(plugin, class, "other", "()V"),
        "forgetting one loader's class must leave the other's verdicts"
    );
    assert!(is_osr_entry_rejected(plugin, class, "loop", "()V", 9));

    // An id-less forget falls back to the name and errs towards forgetting.
    assert_eq!(forget_jit_verdicts_for_class(no_id, class), 2);
    assert!(!is_jit_bail_listed(plugin, class, "other", "()V"));
    assert!(!is_osr_entry_rejected(plugin, class, "loop", "()V", 9));
}

/// The IR refusal memo is per declaring class identity as well as per
/// redefine epoch.
#[test]
fn the_ir_refusal_memo_key_separates_same_named_classes() {
    let hash = ir_method_memo_hash("craton/test/IrMemoPerLoader", "m", "()V");
    let app = cratonvm_types::ClassId::new(11);
    let plugin = cratonvm_types::ClassId::new(12);
    assert_ne!(
        ir_refusal_memo_key(hash, app, 0),
        ir_refusal_memo_key(hash, plugin, 0),
        "the class id is part of the key"
    );
    assert_ne!(
        ir_refusal_memo_key(hash, app, 0),
        ir_refusal_memo_key(hash, app, 1),
        "the redefine epoch is part of the key"
    );
    assert_eq!(
        ir_refusal_memo_key(hash, app, 3),
        ir_refusal_memo_key(hash, app, 3)
    );
}

#[test]
fn compile_state_dependent_osr_refusals_are_memoable_and_classified() {
    for tag in OSR_COMPILE_STATE_REFUSAL_TAGS {
        assert!(
            OSR_PERMANENT_REFUSAL_TAGS.contains(&tag),
            "{tag} must be memoable in the first place"
        );
        assert!(osr_refusal_depends_on_compile_state(&osr_refusal(
            tag, "probe"
        )));
    }
    assert!(!osr_refusal_depends_on_compile_state(&osr_refusal(
        OSR_REFUSE_PC_NOT_AN_ENTRY,
        "probe"
    )));
}

/// The happy path, on the production artifact shape (no deopt metadata):
/// the plan's resume bci IS the entry bci, which is the interpreter's own
/// pc — so a refusal repeats no work.
#[test]
fn a_validated_osr_entry_resumes_at_the_interpreters_own_pc() {
    let cm = osr_t_artifact(3);
    let locals = [0x1234_5678i64, 200, 4950];
    let state = OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS);

    let plan = cm.validate_osr_entry(&state).expect("clean entry");
    assert_eq!(plan.entry_pc, OSR_T_HEADER);
    assert_eq!(
        plan.resume_bci, state.pc,
        "the fallback resume point must be the pc the interpreter is already at"
    );
    assert_eq!(plan.native_offset, 0);
    assert_eq!(plan.contract, OsrContractSource::RegisterHomes);
    assert_eq!(
        plan.exit_policy,
        OsrExitPolicy::PropagateOnly,
        "an artifact with no deopt points has no frame-deopt exit to transfer through"
    );
    assert_eq!(plan.expected_locals.len(), 3);
    assert!(plan
        .expected_locals
        .iter()
        .all(|e| *e == OsrSlotExpectation::Unconstrained));

    // A pc the artifact published no entry for is refused, not guessed at.
    let at_body = OsrEntryState::at(5, &locals, &OSR_T_TAGS);
    assert_eq!(
        osr_t_tag(&cm.validate_osr_entry(&at_body).unwrap_err()),
        Some(OSR_REFUSE_PC_NOT_AN_ENTRY)
    );
}

/// A type mismatch in an incoming slot refuses with a named reason, under
/// both contract strengths.
///
/// Inferred (register-home) contract: an XMM-homed local can only hold FP,
/// so an incoming `int` there would be seeded into an XMM register and read
/// back as a double — a silent miscompile, which is what this check exists
/// to turn into a refusal.
///
/// Precise (`FrameState`) contract: the exact JVM type is known, so an
/// `int` offered where the compiled body reads a reference is caught even
/// though both are GPR-homed 64-bit words.
#[test]
fn an_incoming_slot_type_mismatch_refuses_with_a_named_reason() {
    // --- inferred contract ---
    let mut cm = osr_t_artifact(3);
    // Local 2 is XMM-homed: the compiled body treats it as a double.
    cm.osr_xmm_assignments = Some(vec![None, None, Some(3)]);
    let locals = [0x1234_5678i64, 200, 4950];

    let ok_tags = [
        cratonvm_types::VTAG_OBJECT,
        cratonvm_types::VTAG_INT,
        cratonvm_types::VTAG_DOUBLE,
    ];
    assert!(cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &ok_tags))
        .is_ok());

    let err = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect_err("an int offered to an xmm-homed slot must be refused");
    assert_eq!(osr_t_tag(&err), Some(OSR_REFUSE_SLOT_TYPE));
    let msg = err.to_string();
    assert!(msg.contains("local 2"), "{msg}");
    assert!(msg.contains("xmm-homed"), "{msg}");
    assert!(msg.contains("int"), "{msg}");

    // --- precise contract ---
    let mut cm = osr_t_artifact(3);
    cm.deopt_points = vec![osr_t_exit_point(
        OSR_T_HEADER as u32,
        vec![
            deopt::FrameValue::StackSlotRef(-16),
            deopt::FrameValue::Register(0),
            deopt::FrameValue::Register(1),
        ],
        Vec::new(),
    )];
    assert!(cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .is_ok());

    let ref_as_int = [
        cratonvm_types::VTAG_INT,
        cratonvm_types::VTAG_INT,
        cratonvm_types::VTAG_INT,
    ];
    let err = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &ref_as_int))
        .expect_err("an int offered where the body reads a reference must be refused");
    assert_eq!(osr_t_tag(&err), Some(OSR_REFUSE_SLOT_TYPE));
    assert!(err.to_string().contains("local 0"), "{err}");

    // A `jsr` return address has no JIT representation at all.
    let retaddr = [
        cratonvm_types::VTAG_RETADDR,
        cratonvm_types::VTAG_INT,
        cratonvm_types::VTAG_INT,
    ];
    assert_eq!(
        osr_t_tag(
            &cm.validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &retaddr))
                .unwrap_err()
        ),
        Some(OSR_REFUSE_RETURNADDRESS)
    );
}

/// An OSR entry whose contract contains a slot the compiled frame cannot
/// describe refuses — it never guesses a value.
///
/// `MaterializationRequired` is the case that matters: it means an
/// optimization deleted a value that WAS live and left no rebuild recipe.
/// Reconstructing it as `Undefined`/zero would be a silent
/// wrong-reconstruction, which is precisely why that variant exists as a
/// separate marker from `Undefined`.
#[test]
fn an_undescribable_slot_refuses_the_osr_entry() {
    let locals = [0x1234_5678i64, 200, 4950];

    for (slot_value, what) in [
        (
            deopt::FrameValue::MaterializationRequired(deopt::EliminatedValue::allocation(
                17,
                42,
                deopt::EliminationCause::ScalarReplacedObject,
            )),
            "MaterializationRequired",
        ),
        (deopt::FrameValue::Unsupported, "Unsupported"),
        (
            deopt::FrameValue::VirtualObjectRef(0),
            "VirtualObjectRef (needs a heap allocation to rebuild)",
        ),
    ] {
        let mut cm = osr_t_artifact(3);
        cm.deopt_points = vec![osr_t_exit_point(
            OSR_T_HEADER as u32,
            vec![
                slot_value,
                deopt::FrameValue::Register(0),
                deopt::FrameValue::Register(1),
            ],
            Vec::new(),
        )];
        let result = cm.validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS));
        let err = match result {
            Ok(plan) => panic!("{what} must refuse the entry, got {plan:?}"),
            Err(e) => e,
        };
        assert_eq!(
            osr_t_tag(&err),
            Some(OSR_REFUSE_UNDESCRIBABLE_SLOT),
            "{what} refused with the wrong reason: {err}"
        );
    }
}

/// The refusal names the offending slot, is permanent (so the caller may
/// memo it), and is distinct from the whole-artifact "some other exit is
/// unresumable" answer.
#[test]
fn an_undescribable_slot_names_the_slot_it_refused() {
    let locals = [0x1234_5678i64, 200, 4950];
    let mut cm = osr_t_artifact(3);
    cm.deopt_points = vec![osr_t_exit_point(
        OSR_T_HEADER as u32,
        vec![
            deopt::FrameValue::MaterializationRequired(deopt::EliminatedValue::allocation(
                17,
                42,
                deopt::EliminationCause::ScalarReplacedObject,
            )),
            deopt::FrameValue::Register(0),
            deopt::FrameValue::Register(1),
        ],
        Vec::new(),
    )];
    let err = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect_err("a MaterializationRequired slot in the entry contract must refuse");
    assert_eq!(osr_t_tag(&err), Some(OSR_REFUSE_UNDESCRIBABLE_SLOT));
    assert!(err.to_string().contains("local 0"), "{err}");
    assert!(osr_refusal_is_permanent(&err));

    // An unresumable slot at some OTHER deopt point is a different, still
    // pre-entry, refusal: entering would commit iterations that the bail
    // could then only discard.
    let mut cm = osr_t_artifact(3);
    cm.deopt_points = vec![
        osr_t_exit_point(
            OSR_T_HEADER as u32,
            vec![
                deopt::FrameValue::StackSlotRef(-16),
                deopt::FrameValue::Register(0),
                deopt::FrameValue::Register(1),
            ],
            Vec::new(),
        ),
        osr_t_exit_point(5, vec![deopt::FrameValue::Unsupported], Vec::new()),
    ];
    let err = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect_err("an unresumable exit anywhere in the body must refuse the entry");
    assert_eq!(osr_t_tag(&err), Some(OSR_REFUSE_UNRESUMABLE_EXIT));
}

/// The trampoline seeds locals only, so a live operand stack is refused
/// rather than silently dropped — and an inlined scope, which
/// `FrameState::caller` still cannot describe, is refused too.
#[test]
fn unrepresentable_entry_shapes_are_refused() {
    let cm = osr_t_artifact(3);
    let locals = [0x1234_5678i64, 200, 4950];
    let stack = [7i64];
    let stack_tags = [cratonvm_types::VTAG_INT];
    let with_stack = OsrEntryState {
        pc: OSR_T_HEADER,
        locals: &locals,
        local_tags: &OSR_T_TAGS,
        stack: &stack,
        stack_tags: &stack_tags,
    };
    assert_eq!(
        osr_t_tag(&cm.validate_osr_entry(&with_stack).unwrap_err()),
        Some(OSR_REFUSE_OPERAND_STACK)
    );

    // Local count must match the compiled frame exactly.
    let short = [0x1234_5678i64, 200];
    assert_eq!(
        osr_t_tag(
            &cm.validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &short, &OSR_T_TAGS[..2]))
                .unwrap_err()
        ),
        Some(OSR_REFUSE_LOCAL_COUNT)
    );

    // Inlined scope: `FrameState::caller` is `None` at every producer, so an
    // artifact that inlined something and can also take a frame-deopt exit
    // cannot tell an outer-scope bail from one inside the callee.
    let mut inlined = osr_t_artifact(3);
    inlined.inlined_methods = vec![(
        "craton/probe/OsrEntry".to_string(),
        "helper".to_string(),
        "(I)I".to_string(),
    )];
    inlined.deopt_points = vec![osr_t_exit_point(
        OSR_T_HEADER as u32,
        vec![
            deopt::FrameValue::StackSlotRef(-16),
            deopt::FrameValue::Register(0),
            deopt::FrameValue::Register(1),
        ],
        Vec::new(),
    )];
    let err = inlined
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect_err("an inlined region the deopt metadata cannot describe must refuse");
    assert_eq!(osr_t_tag(&err), Some(OSR_REFUSE_INLINED_SCOPE));
    assert!(osr_refusal_is_permanent(&err));

    // An artifact with an unconditional uncommon trap is never worth
    // entering: it bails on every execution reaching that site.
    let mut trapping = osr_t_artifact(3);
    trapping.has_indy_trap = true;
    assert_eq!(
        osr_t_tag(
            &trapping
                .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
                .unwrap_err()
        ),
        Some(OSR_REFUSE_UNCONDITIONAL_TRAP)
    );
}

/// Entry, then a forced deopt at the loop boundary: the resume lands on the
/// SAME bci the entry used, carrying the same local layout — with the
/// JIT-advanced values, not the pre-entry ones.
#[test]
fn a_forced_deopt_returns_to_the_same_bci_with_the_same_locals() {
    let mut cm = osr_t_artifact(3);
    let contract_locals = vec![
        deopt::FrameValue::StackSlotRef(-16),
        deopt::FrameValue::Register(0),
        deopt::FrameValue::Register(1),
    ];
    cm.deopt_points = vec![osr_t_exit_point(
        OSR_T_HEADER as u32,
        contract_locals.clone(),
        Vec::new(),
    )];

    let array = 0x1234_5678i64;
    let entry_locals = [array, 200, 4950];
    let plan = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &entry_locals, &OSR_T_TAGS))
        .expect("clean entry");
    assert_eq!(plan.contract, OsrContractSource::PreciseFrameState);
    assert_eq!(
        plan.exit_policy,
        OsrExitPolicy::ExactTransfer,
        "every exit of this artifact reconstructs a resumable frame"
    );

    // The forced deopt: the OSR'd body ran to the loop boundary again with
    // i = 700, sum = 244_650, and reconstructed its frame there.
    let rframe = deopt::ReconstructedFrame {
        method_key: "craton/probe/OsrEntry.sum:([I)I".to_string(),
        bci: OSR_T_HEADER as u32,
        locals: vec![
            deopt::FrameValue::Object(array as u64),
            deopt::FrameValue::Int(700),
            deopt::FrameValue::Int(244_650),
        ],
        stack: Vec::new(),
        monitors: Vec::new(),
        caller_frames: Vec::new(),
    };
    let resume = plan
        .resume_after_exit(&cm, &rframe)
        .expect("a loop-boundary exit must resume exactly");
    assert_eq!(
        resume, OSR_T_HEADER,
        "the resume bci is the loop header the entry used"
    );
    assert_eq!(resume, plan.entry_pc);

    // Same slot layout, same per-slot types as the entry contract…
    assert_eq!(rframe.locals.len(), plan.expected_locals.len());
    for (i, want) in contract_locals.iter().enumerate() {
        assert_eq!(
            OsrSlotType::from_frame_value(&rframe.locals[i]),
            OsrSlotType::from_frame_value(want),
            "local {i} changed JVM type across the OSR transition"
        );
    }
    // …but the induction variable carries the JIT's advanced value.
    assert_eq!(rframe.locals[1], deopt::FrameValue::Int(700));
    assert_ne!(
        rframe.locals[1],
        deopt::FrameValue::Int(entry_locals[1] as i64)
    );
}

/// No iteration executes twice across the transition.
///
/// The recorded defect ("an OSR bail re-runs loop iterations") is: compiled
/// code commits N iterations, then bails, and the interpreter resumes at the
/// *entry* bci from its own stale locals — replaying all N. This test
/// executes the whole trip through the API and asserts every iteration index
/// is executed exactly once, in order.
///
/// The structural guarantee behind it: `resume_bci` (== the entry bci) is
/// only reachable when `validate_osr_entry` FAILED, i.e. before compiled
/// code ran; after entry the only sanctioned resume point is
/// `resume_after_exit`, which returns the reconstructed frame's own bci or
/// refuses.
#[test]
fn no_iteration_executes_twice_across_the_osr_transition() {
    const TRIP: usize = 1000;
    let mut executed: Vec<usize> = Vec::with_capacity(TRIP);

    let mut cm = osr_t_artifact(3);
    cm.deopt_points = vec![osr_t_exit_point(
        OSR_T_HEADER as u32,
        vec![
            deopt::FrameValue::StackSlotRef(-16),
            deopt::FrameValue::Register(0),
            deopt::FrameValue::Register(1),
        ],
        Vec::new(),
    )];

    // Phase 1 — interpreted warm-up. `i` is the interpreter's local 1.
    let mut i = 0usize;
    while i < 200 {
        executed.push(i);
        i += 1;
    }

    // A refused entry must leave the interpreter exactly where it was: it
    // resumes at `resume_bci` == its own pc, having executed nothing. Model
    // that with a deliberately mistyped slot.
    let bad_tags = [
        cratonvm_types::VTAG_INT,
        cratonvm_types::VTAG_INT,
        cratonvm_types::VTAG_INT,
    ];
    let locals = [0x1234_5678i64, i as i64, 0];
    let refused = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &bad_tags))
        .unwrap_err();
    assert_eq!(osr_t_tag(&refused), Some(OSR_REFUSE_SLOT_TYPE));
    assert_eq!(executed.len(), 200, "a refusal must execute nothing");

    // Phase 2 — the entry actually validates.
    let plan = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect("clean entry");
    assert_eq!(plan.resume_bci, OSR_T_HEADER);

    // Phase 3 — compiled code runs iterations [200, 700) and is then forced
    // to deopt at the loop boundary.
    let jit_start = i;
    while i < 700 {
        executed.push(i);
        i += 1;
    }
    assert!(i > jit_start, "the JIT must have committed real iterations");
    let rframe = deopt::ReconstructedFrame {
        method_key: "craton/probe/OsrEntry.sum:([I)I".to_string(),
        bci: OSR_T_HEADER as u32,
        locals: vec![
            deopt::FrameValue::Object(0x1234_5678),
            deopt::FrameValue::Int(i as i64),
            deopt::FrameValue::Int(0),
        ],
        stack: Vec::new(),
        monitors: Vec::new(),
        caller_frames: Vec::new(),
    };

    // Phase 4 — the resume. The interpreter's induction variable comes from
    // the RECONSTRUCTED frame, never from its own pre-entry copy.
    let resume_bci = plan
        .resume_after_exit(&cm, &rframe)
        .expect("a loop-boundary exit must resume exactly");
    assert_eq!(resume_bci, OSR_T_HEADER);
    let resumed_i = match rframe.locals[1] {
        deopt::FrameValue::Int(v) => v as usize,
        ref other => panic!("induction variable came back as {other:?}"),
    };
    assert_eq!(
        resumed_i, i,
        "the resumed frame must carry the JIT's advanced counter, not the pre-entry one"
    );
    i = resumed_i;
    while i < TRIP {
        executed.push(i);
        i += 1;
    }

    assert_eq!(
        executed,
        (0..TRIP).collect::<Vec<_>>(),
        "every iteration must execute exactly once, in order"
    );
}

/// A bail the artifact cannot describe must NOT hand back a resume point.
///
/// This is the other half of the no-replay guarantee: when the frame is
/// unresumable the API refuses, so the caller has no sanctioned way to fall
/// back to the entry bci (which would replay every committed iteration).
#[test]
fn an_undescribable_exit_refuses_to_name_a_resume_point() {
    let mut cm = osr_t_artifact(3);
    cm.deopt_points = vec![osr_t_exit_point(
        OSR_T_HEADER as u32,
        vec![
            deopt::FrameValue::StackSlotRef(-16),
            deopt::FrameValue::Register(0),
            deopt::FrameValue::Register(1),
        ],
        Vec::new(),
    )];
    let locals = [0x1234_5678i64, 200, 4950];
    let plan = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect("clean entry");

    let base = deopt::ReconstructedFrame {
        method_key: "craton/probe/OsrEntry.sum:([I)I".to_string(),
        bci: OSR_T_HEADER as u32,
        locals: vec![
            deopt::FrameValue::Object(0x1234_5678),
            deopt::FrameValue::Int(700),
            deopt::FrameValue::Int(0),
        ],
        stack: Vec::new(),
        monitors: Vec::new(),
        caller_frames: Vec::new(),
    };

    // A deleted value in a local: the live frame's stale word is genuinely
    // wrong, not merely unread, so it cannot be left in place.
    let mut materialize = base.clone();
    materialize.locals[1] = deopt::FrameValue::MaterializationRequired(
        deopt::EliminatedValue::unknown(deopt::EliminationCause::EliminatedStore),
    );
    let err = plan.resume_after_exit(&cm, &materialize).unwrap_err();
    assert_eq!(osr_t_tag(&err), Some(OSR_REFUSE_EXIT_REPLAY));
    assert!(err.to_string().contains("local 1"), "{err}");

    // `Unsupported` in a LOCAL is the documented exception: refusing would
    // replay the exit's committed side effects from pre-entry state, so the
    // live frame's current value stands and the resume goes ahead.
    let mut unsupported_local = base.clone();
    unsupported_local.locals[2] = deopt::FrameValue::Unsupported;
    assert_eq!(
        plan.resume_after_exit(&cm, &unsupported_local).unwrap(),
        OSR_T_HEADER
    );

    // `Unsupported` on the operand STACK has no such argument.
    let mut unsupported_stack = base.clone();
    unsupported_stack.stack = vec![deopt::FrameValue::Unsupported];
    assert_eq!(
        osr_t_tag(&plan.resume_after_exit(&cm, &unsupported_stack).unwrap_err()),
        Some(OSR_REFUSE_EXIT_REPLAY)
    );

    // An inlined caller chain: describable or not, the VM's in-place
    // OSR-exit transfer is single-frame, so it cannot be resumed here.
    let mut inlined = base.clone();
    inlined.caller_frames = vec![base.clone()];
    assert_eq!(
        osr_t_tag(&plan.resume_after_exit(&cm, &inlined).unwrap_err()),
        Some(OSR_REFUSE_INLINED_SCOPE)
    );

    // A bci this artifact never recorded a deopt point for is a mis-routed
    // stash, not a resume point.
    let mut stray = base.clone();
    stray.bci = 5;
    assert_eq!(
        osr_t_tag(&plan.resume_after_exit(&cm, &stray).unwrap_err()),
        Some(OSR_REFUSE_EXIT_REPLAY)
    );
}

/// Two different rules wear the same refusal tag, and the 2026-08-18
/// multi-frame transfer relaxed exactly one of them.
///
/// When this test was written (2026-08-17) it asserted a single rule — "a
/// deopt point with a caller scope refuses the OSR entry" — and it kept
/// passing after the relaxation, which is the drift it was pinned to catch.
/// It was passing for a different reason than it was written for: its
/// fixture put the caller scope on a point at the ENTRY bci, so the refusal
/// came from the entry-contract check rather than from the exit policy.
/// The two are now asserted apart:
///
///  * **The ENTRY CONTRACT may never carry a caller scope.** An OSR entry pc
///    is always an outer-scope block start — `osr_pc_to_native` is indexed
///    by the OUTER method's code array — so a contract naming an inlined
///    scope is malformed, not deep. Unchanged, and unchangeable by the
///    transfer work.
///  * **A non-entry deopt point MAY carry one**, up to
///    `MAX_OSR_INLINE_RESUME_DEPTH`, since
///    `transfer_osr_exit_chain_into_live_frame` writes the outermost scope
///    into the live frame and pushes the rest. Past the budget it refuses,
///    naming the budget — and the budget is the VM's own, so admission
///    cannot accept a chain the transfer would decline.
#[test]
fn an_inlined_caller_scope_refuses_the_entry_contract_but_not_a_deopt_point() {
    let locals = [0x1234_5678i64, 200, 4950];
    let scope_of_depth = |depth: usize| {
        let mut fs = deopt::FrameState {
            method_key: "craton/probe/OsrEntry.caller:()V".to_string(),
            bci: 12,
            locals: vec![deopt::FrameValue::Int(7)],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        };
        for _ in 1..depth {
            let inner = fs.clone();
            fs = deopt::FrameState {
                caller: Some(Box::new(inner)),
                ..deopt::FrameState {
                    method_key: "craton/probe/OsrEntry.caller:()V".to_string(),
                    bci: 12,
                    locals: vec![deopt::FrameValue::Int(7)],
                    stack: Vec::new(),
                    monitors: Vec::new(),
                    caller: None,
                }
            };
        }
        fs
    };

    // Sanity: the artifact with a clean point is admitted, so nothing below
    // can pass because the fixture is malformed some other way.
    let mut flat = osr_t_artifact(3);
    flat.deopt_points = vec![osr_t_exit_point(
        OSR_T_HEADER as u32,
        osr_t_contract_locals(),
        Vec::new(),
    )];
    flat.validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect("a clean artifact must be admitted");

    // ── the ENTRY CONTRACT: still refused ────────────────────────────
    let mut contract = osr_t_artifact(3);
    let mut entry_point =
        osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new());
    entry_point.frame_state.caller = Some(Box::new(scope_of_depth(1)));
    contract.deopt_points = vec![entry_point];
    let err = contract
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect_err("an entry contract with a caller scope must refuse");
    assert_eq!(osr_t_tag(&err), Some(OSR_REFUSE_INLINED_SCOPE));
    assert!(
        err.to_string().contains("entry contract"),
        "the refusal must name WHICH rule fired: {err}"
    );

    // ── a NON-entry deopt point: admitted within the budget ──────────
    let mut inlined = osr_t_artifact(3);
    let mut deep_point = osr_t_exit_point(40, osr_t_contract_locals(), Vec::new());
    deep_point.frame_state.caller = Some(Box::new(scope_of_depth(1)));
    inlined.deopt_points = vec![
        osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new()),
        deep_point,
    ];
    inlined
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect("a describable caller scope within the budget is resumable now");

    // ── and refused past it, naming the budget ───────────────────────
    let mut too_deep = osr_t_artifact(3);
    let mut deep_point = osr_t_exit_point(40, osr_t_contract_locals(), Vec::new());
    deep_point.frame_state.caller = Some(Box::new(scope_of_depth(
        deopt::MAX_OSR_INLINE_RESUME_DEPTH + 1,
    )));
    too_deep.deopt_points = vec![
        osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new()),
        deep_point,
    ];
    let err = too_deep
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect_err("past the budget the transfer cannot materialise it atomically");
    assert_eq!(osr_t_tag(&err), Some(OSR_REFUSE_INLINED_SCOPE));
    assert!(
        osr_refusal_is_permanent(&err),
        "the point list is a pure function of the artifact, so the refusal is memoable"
    );

    // An UNDESCRIBABLE caller slot still refuses, and not via the depth
    // rule: `first_unresumable_slot` walks every scope, so a chain needs no
    // special case for it.
    let mut unresumable = osr_t_artifact(3);
    let mut bad_point = osr_t_exit_point(40, osr_t_contract_locals(), Vec::new());
    let mut bad_scope = scope_of_depth(1);
    bad_scope.locals = vec![deopt::FrameValue::Unsupported];
    bad_point.frame_state.caller = Some(Box::new(bad_scope));
    unresumable.deopt_points = vec![
        osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new()),
        bad_point,
    ];
    let err = unresumable
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect_err("an undescribable caller slot must still refuse");
    assert_eq!(osr_t_tag(&err), Some(OSR_REFUSE_UNRESUMABLE_EXIT));
    assert!(
        err.to_string().contains("caller-scope"),
        "the refusal must name the SCOPE the bad slot is in: {err}"
    );
}

/// The lane's "what to refuse", and the two halves of getting it right:
/// an ambiguous resume bci refuses the ENTRY, and copies that agree do not.
///
/// > An OSR exit whose resume bci has more than one possible native image,
/// > or none.
///
/// The refusal is at ADMISSION on purpose. Refusing at exit is not the
/// mirror image: by then the body has committed iterations and the caller's
/// only remaining move is the safe reject, which re-runs all of them — the
/// exact defect this lane exists for. So the test asserts the refusal
/// happens where nothing has run, and that it is memoable.
///
/// The two NON-refusing cases carry as much weight as the refusing one, and
/// one of them is here because a measurement put it here: refusing a
/// `reason`-only disagreement took 10 of CratonBench's 11 OSR refusals and
/// cost `matrixKernel(I)I` its OSR permanently.
#[test]
fn an_ambiguous_resume_bci_refuses_the_entry_and_a_copy_does_not() {
    let locals = [0x1234_5678i64, 200, 4950];

    // Two images of the loop header that disagree on the SEMANTICS.
    //
    // This used to be spelled `REEXECUTE` vs `RETHROW`, on the reasoning
    // that a `RESUME` point is refused by the per-point rule before this
    // check sees it while a `RETHROW` point is explicitly allowed to exist.
    // Both halves of that were true and the conclusion was wrong: a
    // `RETHROW` point is not a competing resume IMAGE — the same sentence
    // that admits it says such points are "stashed separately and never
    // routed to a resume" — so a bci carrying one plus one `REEXECUTE`
    // point has exactly one image and no ambiguity.
    //
    // Refusing it was not academic. After the RBC.6b lift (2026-08-17) a
    // `try { foo(x); } catch (...)` loop puts a speculative-dispatch guard
    // and a `PendingException` frame on the same invoke bci, which is the
    // ORDINARY shape of the population that lift admits;
    // `probes/OsrExcTableProbe.java` reported `osr_entered=0
    // osr_entry_refused_ambiguous_image=15` with every correctness arm
    // green. `osr_exit::resume_image` now skips rethrow points, and
    // `a_rethrow_point_is_not_a_competing_resume_image` is that case.
    //
    // What remains here is the genuine disagreement between two things that
    // both claim to be resume points, which is the wrong-code half.
    let mut cm = osr_t_artifact(3);
    let mut other = osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new());
    other.native_offset = 0x90;
    other.semantics = deopt::ResumeSemantics::RESUME;
    cm.deopt_points = vec![
        osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new()),
        other,
    ];
    let err = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .unwrap_err();
    // The per-point `RESUME` rule fires first (it walks the list before the
    // ambiguity scan), so THAT is what this pair reports. Assert the
    // ambiguity scan itself directly, where nothing else can shadow it.
    assert_eq!(osr_t_tag(&err), Some(OSR_REFUSE_UNRESUMABLE_EXIT));
    assert!(
        osr_refusal_is_permanent(&err),
        "the point list is a pure function of the artifact, so the refusal is memoable"
    );
    assert!(
        osr_exit::first_ambiguous_resume_bci(&cm.deopt_points).is_some(),
        "two points that both claim to be resume points and disagree are ambiguous"
    );

    // And the rethrow pairing, which must NOT be ambiguous — the correction
    // above, asserted at the same level as the refusal it replaced.
    let mut rethrow_pair = osr_t_artifact(3);
    let mut exc = osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new());
    exc.native_offset = 0x90;
    exc.reason = deopt::DeoptReason::PendingException;
    exc.semantics = deopt::ResumeSemantics::for_reason(deopt::DeoptReason::PendingException);
    assert_eq!(exc.semantics, deopt::ResumeSemantics::RETHROW);
    rethrow_pair.deopt_points = vec![
        osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new()),
        exc,
    ];
    let plan = rethrow_pair
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect("a RETHROW point sharing a bci must not refuse the entry");
    assert_eq!(plan.exit_policy, OsrExitPolicy::ExactTransfer);

    // Over-refusal guard 1: several native images of ONE bytecode is
    // exactly what a loop transform produces, and they agree on everything
    // a by-bci lookup reads, so the pick cannot be wrong.
    let mut copies = osr_t_artifact(3);
    let mut second = osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new());
    second.native_offset = 0x90;
    let mut third = osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new());
    third.native_offset = 0xE0;
    copies.deopt_points = vec![
        osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new()),
        second,
        third,
    ];
    let plan = copies
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect("agreeing copies must still admit the entry");
    assert_eq!(plan.exit_policy, OsrExitPolicy::ExactTransfer);

    // Over-refusal guard 2, the measured one: the loop-boundary exit map
    // and the speculative-BCE range guard on the same header bci. Different
    // reasons, same `REEXECUTE` semantics — the ordinary shape of a
    // compiled counted loop, and the resume bci is not in doubt.
    let mut counted = osr_t_artifact(3);
    let mut guard = osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new());
    guard.native_offset = 0x3c1;
    guard.reason = deopt::DeoptReason::BoundsCheck;
    assert_eq!(
        guard.semantics,
        deopt::ResumeSemantics::REEXECUTE,
        "a reason disagreement never implies a semantics one"
    );
    counted.deopt_points = vec![
        osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new()),
        guard,
    ];
    assert!(
        counted
            .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
            .is_ok(),
        "refusing this cost CratonBench.matrixKernel its OSR, permanently"
    );
}

/// `osr_exit_points` is cross-checked against where exits are actually
/// taken — the lane's step 4, which had no consumer at all before.
///
/// The first two are both legitimate; the last two are disagreements and
/// must read zero. The `invokedynamic` trap is the case that makes set
/// membership alone insufficient: it shares the exit-map machinery, so it
/// lands in `osr_exit_points` exactly like a loop boundary and is told
/// apart only by its recorded reason.
#[test]
fn the_exit_site_classification_names_every_outcome() {
    use osr_exit::OsrExitSite;

    let mut cm = osr_t_artifact(3);
    let mut trap = osr_t_exit_point(30, osr_t_contract_locals(), Vec::new());
    trap.native_offset = 0x90;
    trap.reason = deopt::DeoptReason::UnreachedCode;
    cm.deopt_points = vec![
        osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new()),
        trap,
    ];
    // Both bcis are in the set — that is what the shared emitter does.
    cm.osr_exit_points = vec![OSR_T_HEADER, 30];

    assert_eq!(
        cm.classify_osr_exit_site(OSR_T_HEADER as u32),
        OsrExitSite::LoopBoundary
    );
    assert_eq!(
        cm.classify_osr_exit_site(30),
        OsrExitSite::OffLoopBoundary(deopt::DeoptReason::UnreachedCode),
        "membership alone would have called the indy trap a loop boundary"
    );
    assert_eq!(cm.classify_osr_exit_site(99), OsrExitSite::Unrecorded);

    // The cross-check: drop the header from the set while its `OsrExit`
    // point stays. One function writes both, so this cannot happen — and
    // nothing could have said so before.
    cm.osr_exit_points = vec![30];
    assert_eq!(
        cm.classify_osr_exit_site(OSR_T_HEADER as u32),
        OsrExitSite::ExitMapMissing
    );
}

/// `ResumeSemantics` decides whether a snapshot bci is a place the
/// interpreter may be *parked*, and only `REEXECUTE` is.
///
/// A `RESUME` point means the bytecode at `bci` already took effect, so
/// parking there re-executes it — the same double-execution defect as
/// replaying a loop iteration, one bytecode at a time. Its successor bci
/// needs the method's bytecode, which this crate does not have, so such a
/// point disqualifies the OSR entry up front and is refused again on the
/// way out.
#[test]
fn only_reexecute_semantics_yield_an_exact_resume_point() {
    // Sanity: the convention this rests on.
    assert_eq!(
        deopt::ResumeSemantics::for_reason(deopt::DeoptReason::OsrExit),
        deopt::ResumeSemantics::REEXECUTE
    );

    let locals = [0x1234_5678i64, 200, 4950];

    // A `RESUME`-semantics point anywhere in the artifact refuses the ENTRY
    // — before any iteration is committed.
    let mut cm = osr_t_artifact(3);
    let mut resume_point =
        osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new());
    resume_point.semantics = deopt::ResumeSemantics::RESUME;
    cm.deopt_points = vec![resume_point];
    let err = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect_err("a RESUME-semantics exit must refuse the entry");
    assert_eq!(osr_t_tag(&err), Some(OSR_REFUSE_UNRESUMABLE_EXIT));
    assert!(err.to_string().contains("RESUME"), "{err}");

    // A `RETHROW` point may EXIST — it is stashed separately and never
    // routed to a resume — so it must not disqualify the entry…
    let mut cm = osr_t_artifact(3);
    let mut rethrow = osr_t_exit_point(5, osr_t_contract_locals(), Vec::new());
    rethrow.reason = deopt::DeoptReason::PendingException;
    rethrow.semantics = deopt::ResumeSemantics::RETHROW;
    cm.deopt_points = vec![
        osr_t_exit_point(OSR_T_HEADER as u32, osr_t_contract_locals(), Vec::new()),
        rethrow,
    ];
    let plan = cm
        .validate_osr_entry(&OsrEntryState::at(OSR_T_HEADER, &locals, &OSR_T_TAGS))
        .expect("a RETHROW point elsewhere in the body must not refuse the entry");

    // …but a frame that arrives naming it is refused: its bci is a throwing
    // instruction, not a resume point.
    let rframe = deopt::ReconstructedFrame {
        method_key: "craton/probe/OsrEntry.sum:([I)I".to_string(),
        bci: 5,
        locals: vec![
            deopt::FrameValue::Object(0x1234_5678),
            deopt::FrameValue::Int(700),
            deopt::FrameValue::Int(0),
        ],
        stack: Vec::new(),
        monitors: Vec::new(),
        caller_frames: Vec::new(),
    };
    let err = plan.resume_after_exit(&cm, &rframe).unwrap_err();
    assert_eq!(osr_t_tag(&err), Some(OSR_REFUSE_EXIT_REPLAY));
    assert!(err.to_string().contains("REEXECUTE"), "{err}");
    // …and it says WHY, rather than "not a recorded deopt point": since
    // `resume_image` stopped counting rethrow points as resume images, a
    // bci whose only point is one answers `None` there, and the generic
    // "not recorded" wording would send the reader looking for a
    // mis-routed stash that does not exist.
    assert!(err.to_string().contains("RETHROW"), "{err}");
}

/// A crash handler must be able to tell "no compiled body covers this
/// address" from "I could not read the registry". Collapsing the two into
/// one `None` is what made the 2026-07-28 wild-jump report unreadable.
#[test]
fn jit_name_lookup_separates_absence_from_an_unreadable_registry() {
    // Addresses far outside anything this process maps, so the assertion
    // cannot be perturbed by a concurrently-running test's registrations.
    let base = 0x5EED_0000_0000usize;
    register_jit_method_name(base, 0x100, "Probe.absent()V".to_string());
    // …but it CAN be perturbed by a concurrent test holding the registry
    // lock, because this accessor is a `try_lock` and `Locked` is a third
    // legitimate answer — which is the whole point of the type this test is
    // about. Retry past it rather than reading it as a wrong answer. The
    // address argument above is about registrations; this is about the
    // lock, and only the sibling test below had noticed the difference.
    assert!(
        eventually(|| matches!(
            lookup_jit_method_name_detailed(base + 0x10),
            JitNameLookup::Found(ref n) if n == "Probe.absent()V"
        )),
        "a registered range must resolve to its name"
    );
    assert!(
        eventually(|| matches!(
            lookup_jit_method_name_detailed(base + 0x100),
            JitNameLookup::NotFound
        )),
        "one past the end of the only registered range is an absence"
    );

    let _held = jit_name_ranges().lock().expect("registry lock");
    assert!(matches!(
        lookup_jit_method_name_detailed(base + 0x10),
        JitNameLookup::Locked
    ));
}

/// Every executable buffer registers a live region, including the ones no
/// `JitCache::put` ever names. A crash handler uses that to separate "this
/// address is a buffer we still hold" from "this address is nothing".
#[test]
fn live_code_region_covers_a_buffer_the_name_registry_never_saw() {
    let buf = ExecutableBuffer::new(4096).expect("alloc failed");
    let base = buf.as_ptr() as usize;
    // `Locked` is a legitimate answer from this `try_lock` accessor, not a
    // miss — retry past any concurrent registration rather than reading it
    // as "the region is absent".
    assert!(
        eventually(|| matches!(
            jit_code_region_covering(base),
            JitRegionLookup::Found(b, cap) if b == base && cap == 4096
        )),
        "the live buffer must be covered at its base"
    );
    assert!(
        eventually(|| matches!(
            jit_code_region_covering(base + 4095),
            JitRegionLookup::Found(..)
        )),
        "the live buffer must be covered at its last byte"
    );
    // One past the end is outside THIS buffer. Phrased as "not this base"
    // rather than "NotFound" because a concurrently-created buffer may
    // legitimately be mapped immediately after it.
    assert!(!matches!(
        jit_code_region_covering(base + 4096),
        JitRegionLookup::Found(b, _) if b == base
    ));
    drop(buf);
    assert!(
        eventually(|| !matches!(
            jit_code_region_covering(base),
            JitRegionLookup::Found(b, _) if b == base
        )),
        "dropping the buffer must unregister its region"
    );
}

#[test]
fn test_executable_buffer_finalize_and_make_writable() {
    let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    buf.finalize();
    // make_writable and finalize again should not crash
    buf.make_writable();
    buf.finalize();
}

// ── CompiledMethod tests ────────────────────────────────────────

#[test]
fn test_compiled_method_new_pure() {
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    let cm = CompiledMethod::new(buf);
    assert!(!cm.needs_context());
    assert!(!cm.needs_heap());
    assert!(!cm.entry_ptr().is_null());
}

#[test]
fn test_compiled_method_new_with_context() {
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    let cm = CompiledMethod::new_with_context(buf);
    assert!(cm.needs_context());
    assert!(cm.needs_heap());
}

// ── JitMICSlot tests ────────────────────────────────────────────

#[test]
fn test_jit_mic_slot_default() {
    let slot = JitMICSlot::new();
    // A fresh slot holds the empty sentinel, not 0: class id 0 is a real
    // receiver (`java/lang/Object`). Round 9, mic-empty-slot-class-zero-window.
    assert_eq!(
        slot.cached_class_id
            .load(std::sync::atomic::Ordering::Relaxed),
        JitMICSlot::EMPTY_CLASS_ID
    );
    assert!(slot.cached_class_name.lock().is_none());
}

#[test]
fn test_jit_mic_slot_prepopulate() {
    let slot = JitMICSlot::new();
    slot.prepopulate(42);
    assert_eq!(
        slot.cached_class_id
            .load(std::sync::atomic::Ordering::Relaxed),
        42
    );
}

// ── JitPICSlot tests ────────────────────────────────────────────

#[test]
fn test_jit_pic_slot_empty_misses() {
    let pic = JitPICSlot::new();
    assert!(pic.lookup(42).is_none());
    assert_eq!(pic.entries_used(), 0);
    assert_eq!(pic.misses.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[test]
fn test_jit_pic_slot_install_and_hit() {
    let pic = JitPICSlot::new();
    pic.install(1, "A", 0x1000, false, false);
    pic.install(2, "B", 0x2000, true, false);
    pic.install(3, "C", 0x3000, false, false);
    assert_eq!(pic.entries_used(), 3);
    assert_eq!(pic.lookup(1), Some((0x1000, false)));
    assert_eq!(pic.lookup(2), Some((0x2000, true)));
    assert_eq!(pic.lookup(3), Some((0x3000, false)));
    assert!(pic.lookup(99).is_none());
}

#[test]
fn test_jit_pic_slot_clear_entries_drops_compiled_targets() {
    let pic = JitPICSlot::new();
    pic.install(1, "A", 0x1000, false, false);
    pic.install(2, "B", 0x2000, true, false);
    pic.install(3, "C", 0x3000, false, false);
    assert_eq!(pic.entries_used(), 3);

    pic.clear_entries();

    assert_eq!(pic.entries_used(), 0);
    assert!(pic.lookup(1).is_none());
    assert!(pic.lookup(2).is_none());
    assert!(pic.lookup(3).is_none());
    for i in 0..JIT_PIC_ENTRIES {
        let (class_id, entry, needs_context) = pic.way(i).expect("way exists");
        assert_eq!(entry, 0);
        assert!(!needs_context);
        assert!(
            class_id == 0 || class_id == JitPICSlot::RETIRED_WAY_CLASS_ID,
            "a cleared way must not keep a guard a receiver can match (way {i}: {class_id})"
        );
        assert!(pic.class_names[i].lock().is_none());
    }
}

/// A fifth receiver must NOT displace a live way. Generated code may be
/// between a way's class compare and its entry-word load, so overwriting
/// the way would pair the old receiver's guard with the new receiver's
/// body. The overflow receiver is served by the hashed table instead.
#[test]
fn test_jit_pic_slot_full_ways_are_never_evicted() {
    let pic = JitPICSlot::new();
    pic.install(1, "A", 0x1000, false, false);
    pic.install(2, "B", 0x2000, false, false);
    pic.install(3, "C", 0x3000, false, false);
    pic.install(4, "D", 0x4000, false, false);
    for _ in 0..10 {
        pic.lookup(1);
    }
    pic.install(5, "E", 0x5000, true, false);
    assert_eq!(pic.lookup(1), Some((0x1000, false)));
    assert_eq!(pic.lookup(2), Some((0x2000, false)));
    assert_eq!(pic.lookup(3), Some((0x3000, false)));
    assert_eq!(pic.lookup(4), Some((0x4000, false)));
    assert!(
        pic.lookup(5).is_none(),
        "no live way may be evicted for class 5"
    );
    assert_eq!(pic.lookup_megamorphic(5), Some((0x5000, true)));
}

#[test]
fn test_jit_pic_slot_seed_from_mic() {
    let mic = JitMICSlot::new();
    mic.update(7, "Seven", 0x7000, true, false);
    mic.record_hit();
    mic.record_hit();
    let pic = JitPICSlot::new();
    pic.seed_from_mic(&mic, false);
    assert_eq!(pic.entries_used(), 1);
    assert_eq!(pic.lookup(7), Some((0x7000, true)));
    let name = pic.class_names[0].lock();
    assert_eq!(name.as_deref(), Some("Seven"));
}

#[test]
fn test_jit_pic_slot_seed_empty_mic_noop() {
    let mic = JitMICSlot::new();
    let pic = JitPICSlot::new();
    pic.seed_from_mic(&mic, false);
    assert_eq!(pic.entries_used(), 0);
}

/// A recompiled target for a cached receiver is never written over the
/// old way. The old way is retired (guard first, then word) and the new
/// target goes into a different way, in both the inline ways and the
/// hashed table.
#[test]
fn test_jit_pic_slot_recompiled_target_retires_the_stale_way() {
    let pic = JitPICSlot::new();
    pic.install(1, "A", 0x1000, false, false);
    pic.install(1, "A2", 0x2000, true, false);
    assert_eq!(pic.entries_used(), 1);
    assert_eq!(pic.lookup(1), Some((0x2000, true)));
    assert_eq!(
        pic.way(0),
        Some((JitPICSlot::RETIRED_WAY_CLASS_ID, 0, false)),
        "the stale way must be retired, not retargeted"
    );
    assert_eq!(pic.lookup_megamorphic(1), Some((0x2000, true)));
}

#[test]
fn test_jit_mega_offsets_match_generated_stub_contract() {
    let pic = JitPICSlot::new();
    let base = &pic as *const JitPICSlot as usize;
    assert_eq!(
        &pic.mega_class_ids as *const _ as usize - base,
        JitPICSlot::MEGA_CLASS_IDS_OFFSET
    );
    assert_eq!(
        &pic.mega_entry_words as *const _ as usize - base,
        JitPICSlot::MEGA_ENTRY_PTRS_OFFSET
    );
}

#[test]
fn test_jit_pic_secondary_cache_serves_overflow_and_clears() {
    let pic = JitPICSlot::new();
    // Entries are even: bit 0 of a word is the context tag.
    for class_id in 1..=5 {
        pic.install(
            class_id,
            &format!("C{class_id}"),
            0x1000 + (u64::from(class_id) << 4),
            class_id % 2 == 0,
            false,
        );
    }
    assert_eq!(pic.entries_used(), JIT_PIC_ENTRIES);
    for class_id in 1..=5 {
        assert_eq!(
            pic.lookup_megamorphic(class_id),
            Some((0x1000 + (u64::from(class_id) << 4), class_id % 2 == 0))
        );
    }
    pic.clear_entries();
    for class_id in 1..=5 {
        assert!(pic.lookup_megamorphic(class_id).is_none());
    }
}

#[test]
fn test_jit_pic_slot_thresholds_are_sensible() {
    assert!(MIC_TO_PIC_THRESHOLD >= 2);
    assert!(JIT_PIC_ENTRIES >= 2);
}

// ── T17.Β.1 — PIC 4-way probe semantics ────────────────────────

/// Fill every `(class_id, entry_ptr)` entry and issue a
/// lookup for each. The pure-Rust [`JitPICSlot::lookup`] mirrors
/// what the x64 emission does with `CMP RAX, [RBX+off]; JE
/// entry_i` for each slot. All entries must hit and the
/// miss counter must stay at 0.
#[test]
fn t17_b_pic_all_entries_hit() {
    let pic = JitPICSlot::new();
    pic.install(11, "A", 0xAAAA_0000, false, false);
    pic.install(22, "B", 0xBBBB_0000, true, false);
    pic.install(33, "C", 0xCCCC_0000, false, false);
    pic.install(44, "D", 0xDDDD_0000, true, false);
    assert_eq!(pic.entries_used(), JIT_PIC_ENTRIES);

    // Each receiver class_id is one of the 3 probed slots.
    assert_eq!(pic.lookup(11), Some((0xAAAA_0000, false)));
    assert_eq!(pic.lookup(22), Some((0xBBBB_0000, true)));
    assert_eq!(pic.lookup(33), Some((0xCCCC_0000, false)));
    assert_eq!(pic.lookup(44), Some((0xDDDD_0000, true)));

    // Miss counter stays at 0 — no probe fell through.
    assert_eq!(
        pic.misses.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "miss counter should not advance on 3 PIC hits"
    );

    // Per-slot hit counters all incremented exactly once.
    for i in 0..JIT_PIC_ENTRIES {
        assert_eq!(
            pic.hits[i].load(std::sync::atomic::Ordering::Relaxed),
            1,
            "slot {i} should have exactly 1 hit"
        );
    }
}

/// A receiver whose class_id is not in any slot must
/// fall through to the generic-helper path. In the pure-Rust
/// mirror, that surfaces as `lookup` returning `None` and the
/// miss counter incrementing.
#[test]
fn t17_b_pic_miss_falls_through() {
    let pic = JitPICSlot::new();
    pic.install(11, "A", 0xAAAA_0000, false, false);
    pic.install(22, "B", 0xBBBB_0000, false, false);
    pic.install(33, "C", 0xCCCC_0000, false, false);
    pic.install(44, "D", 0xDDDD_0000, false, false);

    // 5th receiver type — no slot matches.
    let res = pic.lookup(55);
    assert!(
        res.is_none(),
        "miss on an unknown class_id must return None"
    );
    assert_eq!(
        pic.misses.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "miss counter must increment on a fall-through"
    );

    // Successive misses also increment the counter.
    pic.lookup(66);
    pic.lookup(77);
    assert_eq!(
        pic.misses.load(std::sync::atomic::Ordering::Relaxed),
        3,
        "miss counter must track every fall-through"
    );

    // No hit counters were touched on misses.
    for i in 0..JIT_PIC_ENTRIES {
        assert_eq!(
            pic.hits[i].load(std::sync::atomic::Ordering::Relaxed),
            0,
            "slot {i} must not record a hit on a miss"
        );
    }
}

/// A PIC seeded from its MIC keeps the seeded receiver in way 0, and a
/// later receiver lands in a fresh way rather than over it.
#[test]
fn t17_b_seeded_way_is_not_overwritten_by_a_new_receiver() {
    let mic = JitMICSlot::new();
    mic.update(7, "Seven", 0x7000, true, false);
    let pic = JitPICSlot::new_at(mic.bci);
    pic.seed_from_mic(&mic, false);
    assert_eq!(pic.entries_used(), 1);
    assert_eq!(pic.lookup(7), Some((0x7000, true)));

    pic.install(8, "Eight", 0x8000, false, false);
    assert_eq!(pic.entries_used(), 2);
    assert_eq!(pic.lookup(7), Some((0x7000, true)));
    assert_eq!(pic.lookup(8), Some((0x8000, false)));
}

// ── DescriptorParamIter tests ───────────────────────────────────

#[test]
fn test_descriptor_param_iter_simple() {
    let params: Vec<u8> = DescriptorParamIter::new("(II)I").collect();
    assert_eq!(params, vec![b'I', b'I']);
}

#[test]
fn test_descriptor_param_iter_mixed_types() {
    let params: Vec<u8> = DescriptorParamIter::new("(IJLjava/lang/String;[I)V").collect();
    assert_eq!(params, vec![b'I', b'J', b'L', b'[']);
}

#[test]
fn test_descriptor_param_iter_no_params() {
    let params: Vec<u8> = DescriptorParamIter::new("()V").collect();
    assert!(params.is_empty());
}

#[test]
fn test_descriptor_param_iter_array_of_objects() {
    let params: Vec<u8> = DescriptorParamIter::new("([Ljava/lang/Object;)V").collect();
    assert_eq!(params, vec![b'[']);
}

// ── JitCache tests ──────────────────────────────────────────────

#[test]
fn test_scalar_selfrec_ir_structural_admission() {
    let fib = [
        0x1a, 0x04, 0xa3, 0x00, 0x05, // iload_0; iconst_1; if_icmpgt
        0x1a, 0xac, // iload_0; ireturn
        0x1a, 0x04, 0x64, 0xb8, 0x00, 0x02, // fib(n - 1)
        0x1a, 0x05, 0x64, 0xb8, 0x00, 0x02, // fib(n - 2)
        0x60, 0xac, // iadd; ireturn
    ];
    assert!(scalar_selfrec_ir_would_engage(&fib, fib.len(), "(I)I"));
    assert!(!scalar_selfrec_ir_would_engage(&fib, fib.len(), "(I)J"));
    assert!(!scalar_selfrec_ir_would_engage(&[0x1a, 0xac], 2, "(I)I"));
}

#[test]
fn test_jit_cache_new_is_empty() {
    let cache = JitCache::new();
    assert!(cache.is_empty());
    assert_eq!(cache.len(), 0);
}

#[test]
fn test_jit_cache_put_and_get() {
    let mut cache = JitCache::new();
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    let cm = CompiledMethod::new(buf);

    let class: Arc<str> = Arc::from("TestClass");
    let method: Arc<str> = Arc::from("testMethod");
    let desc: Arc<str> = Arc::from("(II)I");

    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cratonvm_types::ClassId::new(1),
        cm,
    );
    assert_eq!(cache.len(), 1);
    assert!(!cache.is_empty());

    let result = cache.get(&class, &method, &desc, cratonvm_types::ClassId::new(1));
    assert!(result.is_some());
}

/// **Every publication advances `jit_cache_generation()`** — first-time
/// insertion, replacement, and OSR body alike.
///
/// This is the load-bearing step of the recycled-`JitSiteKey` argument, and
/// until now it rested on a comment plus four hand-written `fetch_add`
/// calls. See
/// `site-alias-diagnostic-and-the-recycled-jitsitekey-question-CLOSED-20260806.md`.
///
/// The chain it closes: a `JitInvokeInfo` box is owned by its
/// `CompiledMethod` (`_jit_invoke_infos`), so its ADDRESS — which is what a
/// `JitSiteKey` is — is freed and re-issued. Every per-thread memo keyed on
/// one is cleared by `flush_raw_entry_dispatch_caches`, which fires on a
/// generation change and runs before any memo probe. That only closes the
/// hole because a recycled address cannot become *dispatchable* without a
/// publication: compiled code referencing the new `JitInvokeInfo` has to be
/// published before it can run. So if a publication could land without
/// advancing the generation, the flush would not fire and one call site
/// would serve another's dispatch — the defect `383e7f5cf` fixed, in a new
/// disguise.
///
/// **Non-vacuous:** each step asserts the body is actually REACHABLE
/// afterwards, not just that a counter moved. A `put` that silently
/// declined (stale publication epoch, `prepare_for_publication` refusal)
/// publishes nothing and correctly does not bump — so a test that only
/// watched the counter could pass while proving nothing about publication.
///
/// **Strictly greater, not `+ 1`:** `JIT_CACHE_GENERATION` is a process
/// global and the test binary runs tests in parallel, so a concurrent
/// publication in another test may also bump it. `>` is the assertion that
/// is both true and stable; `== before + 1` would be a flake.
///
/// **Verified as a negative control.** Deleting the `fetch_add` at the
/// first-time publication site turns this red on the first arm, with the
/// message it was written to produce — so it is pinning the bump, not
/// riding on a counter something else moves.
#[test]
fn every_publication_advances_the_jit_cache_generation() {
    fn ret_body(osr: bool) -> CompiledMethod {
        let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
        buf.emit(&[0xC3]); // RET
        let mut cm = CompiledMethod::new(buf);
        cm.compiled_via_osr = osr;
        cm
    }

    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("GenerationClass");
    let method: Arc<str> = Arc::from("hot");
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(1);

    // 1. First-time insertion. The comment at the bump site calls this out
    //    specifically ("bump on EVERY publication, not just replacements"),
    //    because it is the one a replacement-only bump would miss.
    let before = jit_cache_generation();
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        ret_body(false),
    );
    let first = cache
        .get(&class, &method, &desc, cid)
        .expect("first publication must be reachable");
    assert!(
        jit_cache_generation() > before,
        "a first-time publication did not advance the JIT cache generation; \
         a recycled JitInvokeInfo address could then inherit the previous \
         site's memoized dispatch"
    );

    // 2. Replacement. The superseded artifact's `JitInvokeInfo` boxes are
    //    what get freed and re-issued, so this is the case the whole
    //    argument is about.
    let before = jit_cache_generation();
    let first_entry = first.entry_ptr();
    drop(first);
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        ret_body(false),
    );
    let replaced = cache
        .get(&class, &method, &desc, cid)
        .expect("replacement must be reachable");
    assert_ne!(
        replaced.entry_ptr(),
        first_entry,
        "the second put must have superseded the first, or this arm proves nothing"
    );
    assert!(
        jit_cache_generation() > before,
        "a replacement publication did not advance the JIT cache generation"
    );

    // 3. OSR body. A separate entry point with its own bump; it publishes
    //    alongside the method-entry body rather than superseding it.
    let before = jit_cache_generation();
    cache.put_osr(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        ret_body(true),
    );
    assert!(
        cache.get_osr(&class, &method, &desc, cid).is_some(),
        "OSR publication must be reachable"
    );
    assert!(
        jit_cache_generation() > before,
        "an OSR publication did not advance the JIT cache generation"
    );
}

#[test]
fn test_jit_cache_osr_and_method_entry_bodies_coexist() {
    let mut cache = JitCache::new();
    let class: Arc<str> = Arc::from("LoopClass");
    let method: Arc<str> = Arc::from("hotLoop");
    let desc: Arc<str> = Arc::from("(I)I");

    let cid = cratonvm_types::ClassId::new(1);
    let mut entry_buf = ExecutableBuffer::new(64).expect("alloc failed");
    entry_buf.emit(&[0xC3]);
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(entry_buf),
    );

    let mut osr_buf = ExecutableBuffer::new(64).expect("alloc failed");
    osr_buf.emit(&[0xC3]);
    let mut osr = CompiledMethod::new(osr_buf);
    osr.compiled_via_osr = true;
    cache.put_osr(class.clone(), method.clone(), desc.clone(), cid, osr);

    let entry = cache.get(&class, &method, &desc, cid).expect("entry body");
    let osr = cache
        .get_osr(&class, &method, &desc, cid)
        .expect("osr body");
    assert_ne!(entry.entry_ptr(), osr.entry_ptr());
    assert!(!entry.compiled_via_osr);
    assert!(osr.compiled_via_osr);
    assert_eq!(cache.len(), 2);

    let osr_entry = osr.entry_ptr();
    let mut c2_buf = ExecutableBuffer::new(64).expect("alloc failed");
    c2_buf.emit(&[0xC3]);
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(c2_buf),
    );
    assert_eq!(
        cache
            .get_osr(&class, &method, &desc, cid)
            .expect("osr survives method-entry supersede")
            .entry_ptr(),
        osr_entry
    );
    assert_eq!(cache.len(), 2);
}

#[test]
fn test_jit_cache_put_replacement_reclaims_after_last_reader() {
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("ReplaceClass");
    let method: Arc<str> = Arc::from("replaceMethod");
    let desc: Arc<str> = Arc::from("()V");

    let cid = cratonvm_types::ClassId::new(1);
    let mut old_buf = ExecutableBuffer::new(64).expect("alloc failed");
    old_buf.emit(&[0xC3]); // RET
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(old_buf),
    );
    let old = cache
        .get(&class, &method, &desc, cid)
        .expect("old compiled method");
    let old_entry = old.entry_ptr() as usize;
    register_jit_code_range(old_entry, old.code_len(), Arc::as_ptr(&old) as usize);
    assert!(lookup_jit_code_range(old_entry).is_some());

    let mut new_buf = ExecutableBuffer::new(64).expect("alloc failed");
    new_buf.emit(&[0xC3]); // RET
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(new_buf),
    );

    assert!(
        lookup_jit_code_range(old_entry).is_some(),
        "an outstanding lock-free reader must own the replaced artifact"
    );
    drop(old);
    assert!(
        eventually_drained(|| lookup_jit_code_range(old_entry).is_none()),
        "the replaced artifact must unregister after its last Arc is released"
    );
    if let Some(new_cm) = cache.get(&class, &method, &desc, cid) {
        unregister_jit_code_range(new_cm.entry_ptr() as usize);
    }
}

/// A thread executing compiled code holds NO `Arc` — only a return address
/// on its stack. So a tier-up `put` that displaces the body it is running
/// would drop the last strong reference and `munmap` the mapping out from
/// under it (`SIGSEGV at pc=X, addr=X`, the PC landing in a hole between
/// core-dump segments, ~10% of runs under load). `put` therefore hands the
/// superseded artifact to [`defer_jit_owner`], which holds it until
/// `ACTIVE_JIT_EXECUTIONS` reaches zero.
#[test]
fn replaced_body_survives_until_jit_execution_is_quiescent() {
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("RetireWhileRunningClass");
    let method: Arc<str> = Arc::from("m");
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(1);

    let mut old_buf = ExecutableBuffer::new(64).expect("alloc failed");
    old_buf.emit(&[0xC3]); // RET
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(old_buf),
    );
    let old = cache
        .get(&class, &method, &desc, cid)
        .expect("old compiled method");
    let old_entry = old.entry_ptr() as usize;
    register_jit_code_range(old_entry, old.code_len(), Arc::as_ptr(&old) as usize);
    // Release the reader: the shard snapshot is now the ONLY strong owner,
    // exactly as it is for a body that is merely being executed.
    drop(old);
    assert!(lookup_jit_code_range(old_entry).is_some());

    let execution = jit_execution_enter();
    let mut new_buf = ExecutableBuffer::new(64).expect("alloc failed");
    new_buf.emit(&[0xC3]); // RET
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(new_buf),
    );
    assert!(
        lookup_jit_code_range(old_entry).is_some(),
        "a tier-up must not release the body a thread is executing"
    );
    jit_execution_leave(execution);

    assert!(
        eventually_drained(|| lookup_jit_code_range(old_entry).is_none()),
        "the retired body must be released once JIT execution is quiescent"
    );
    if let Some(new_cm) = cache.get(&class, &method, &desc, cid) {
        unregister_jit_code_range(new_cm.entry_ptr() as usize);
    }
}

/// Drive a published body down to a single owner and hand that owner to
/// the caller, together with its entry address.
///
/// `ACTIVE_JIT_EXECUTIONS` is process-global and this suite runs in
/// parallel, so "the cache released its reference" is not observable on the
/// first try — a sibling test's execution epoch can hold the drain off.
/// Pump the quiescence check until the count settles.
fn sole_owner_of_a_published_body(
    cache: &JitCache,
    class: &Arc<str>,
    method: &Arc<str>,
    desc: &Arc<str>,
    cid: cratonvm_types::ClassId,
) -> (Arc<CompiledMethod>, usize) {
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(buf),
    );
    let cm = cache.get(class, method, desc, cid).expect("published body");
    let entry = cm.entry_ptr() as usize;
    register_jit_code_range(entry, cm.code_len(), Arc::as_ptr(&cm) as usize);
    cache.remove(class, method, desc, cid);
    assert!(
        eventually_drained(|| Arc::strong_count(&cm) == 1),
        "the retirement queue should have released the cache's reference \
         (strong count is {})",
        Arc::strong_count(&cm),
    );
    (cm, entry)
}

/// The last owner of a published body is regularly a **per-thread dispatch
/// cache entry**, and such a cache is evicted by the very thread that
/// dispatches through it — a generation flush, a staleness eviction, a
/// replacement. So the eviction can run while a frame of that body is on
/// that thread's own stack, and a bare `Arc` drop there unmaps executable
/// memory with no quiescence proof behind it.
///
/// [`RetainedCode`] is what makes the release structural instead of a rule
/// every container has to remember. Measured on
/// `BasicErrorControllerIntegrationTests` before it existed: 50 published
/// bodies per run released outside the queue, seven of them with
/// `active_jit_executions` at 1, 2 or 3 — the crash signature of
/// `jit-code-buffer-released-outside-retirement-queue-fixed-20260803.md`.
#[test]
fn retained_code_releases_a_published_body_through_the_queue() {
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("RetainedCodeQueueClass");
    let method: Arc<str> = Arc::from("m");
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(1);
    let (cm, entry) = sole_owner_of_a_published_body(&cache, &class, &method, &desc, cid);

    let held = RetainedCode::new(cm);
    let execution = jit_execution_enter();
    drop(held);
    assert!(
        lookup_jit_code_range(entry).is_some(),
        "the last owner must not unmap a published body while a thread is inside compiled code"
    );
    jit_execution_leave(execution);

    assert!(
        eventually_drained(|| lookup_jit_code_range(entry).is_none()),
        "and must release it once JIT execution is quiescent"
    );
}

/// The counter behind [`published_code_free_audit`] is the *enforcement* of
/// the rule above, and it has to be able to fail: a plain `Arc` drop that
/// happens to be the last reference to a published body must be recorded.
///
/// Both counters are process-global and monotone, so a sibling test can only
/// push them higher — never hide the increment this test causes itself.
#[test]
fn the_free_audit_records_a_release_that_skipped_the_queue() {
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("UnqueuedReleaseAuditClass");
    let method: Arc<str> = Arc::from("m");
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(1);
    let (cm, entry) = sole_owner_of_a_published_body(&cache, &class, &method, &desc, cid);

    let (published_before, unqueued_before) = published_code_free_audit();
    // A bare `Arc`, dropped by hand: exactly the shape the two thread-local
    // dispatch caches used to have.
    let own = Arc::as_ptr(&cm) as usize;
    drop(cm);
    let (_, unqueued_after) = published_code_free_audit();

    // The unmap itself is now DEFERRED whenever a sibling test is inside JIT
    // execution: `ExecutableBuffer::drop` rescues an unqueued published
    // mapping into the retirement queue rather than unmapping it on the spot.
    // So the release is counted once the queue gets to it, not necessarily by
    // the time `drop` returns.
    assert!(
        eventually_drained(|| published_code_free_audit().0 > published_before),
        "the release of a published body must be counted"
    );
    assert!(
        unqueued_after > unqueued_before,
        "and a release that never reached the retirement queue must be counted as one"
    );
    // `is_none()` would be wrong, and flaked about 1 run in 20 at
    // `--test-threads=32`: `mmap` regularly hands the page this body just
    // released straight to a concurrently-running test, which registers its
    // own range at the same address. What this test owns is that the range
    // no longer binds `entry` to OUR artifact — a `Some(other)` is somebody
    // else's business, and is exactly the reading
    // `clear_all_unregisters_every_code_range` already spells out.
    assert_ne!(
        lookup_jit_code_range(entry),
        Some(own),
        "the unqueued release really did unmap it — which is why it is counted"
    );
}

/// The live executable buffer covering `addr`, retrying the crash-safe
/// `try_lock` lookup instead of reading `Locked` as an answer.
fn live_region_base(addr: usize) -> Option<usize> {
    for _ in 0..1000 {
        match jit_code_region_covering(addr) {
            JitRegionLookup::Found(base, _) => return Some(base),
            JitRegionLookup::NotFound => return None,
            JitRegionLookup::Locked => std::thread::yield_now(),
        }
    }
    panic!("the code-region registry stayed locked");
}

/// The fix for the published-code use-after-free that the audit above only
/// COUNTED (`jit-code-uaf-outside-retirement-queue-FIXED-20260918.md`).
///
/// The shape is the one a C2 compile task's `superseded_body` local had: the
/// cache withdraws a published body, the retirement queue releases ITS
/// reference, and a leftover `Arc` somewhere becomes the last owner. Before,
/// that drop unmapped the body on the spot, with no question asked about
/// whether a thread was executing inside it — and in a Spring suite run one
/// was. Now the mapping is rescued into the retirement queue and stays mapped,
/// bytes intact, until the queue's proof covers it.
#[test]
fn an_unqueued_last_drop_under_running_jit_code_is_rescued_not_unmapped() {
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("RescuedOrphanMappingClass");
    let method: Arc<str> = Arc::from("m");
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(1);
    let (cm, entry) = sole_owner_of_a_published_body(&cache, &class, &method, &desc, cid);
    let base = live_region_base(entry).expect("the published body is mapped");

    // A thread is inside compiled code — this one, standing in for the thread
    // that was executing the superseded body.
    let token = jit_execution_enter();
    let (_, unqueued_before) = published_code_free_audit();
    drop(cm);
    assert!(
        published_code_free_audit().1 > unqueued_before,
        "the stray last drop is still counted: it names a holder that should \
         have released through the queue"
    );
    // While this thread is inside compiled code the mapping is ours alone, so
    // its address cannot have been handed to anyone else: covering it at the
    // same base means it was NOT unmapped.
    assert_eq!(
        live_region_base(entry),
        Some(base),
        "a published body whose last owner dropped outside the queue must not \
         be unmapped while a thread is inside compiled code"
    );
    jit_execution_leave(token);

    // And it is released once the queue can prove quiescence. After that the
    // page may be reused by a concurrently running test, so "released" is
    // either no live buffer at `base` or an AUTHORISED free recorded for it.
    assert!(
        eventually_drained(|| {
            live_region_base(entry) != Some(base)
                || matches!(
                    recent_code_free_covering(entry),
                    Some((b, _, _, flags))
                        if b == base
                            && flags & CODE_FREE_AUTHORISED != 0
                            && flags & CODE_FREE_PUBLISHED != 0
                )
        }),
        "the rescued mapping must be released by the queue once JIT execution \
         is quiescent"
    );
}

#[test]
fn test_jit_cache_remove_reclaims_code_range() {
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("RemoveClass");
    let method: Arc<str> = Arc::from("removeMethod");
    let desc: Arc<str> = Arc::from("()V");

    let cid = cratonvm_types::ClassId::new(1);
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(buf),
    );
    let cm = cache
        .get(&class, &method, &desc, cid)
        .expect("compiled method");
    let entry = cm.entry_ptr() as usize;
    register_jit_code_range(entry, cm.code_len(), Arc::as_ptr(&cm) as usize);
    drop(cm);

    cache.remove(&class, &method, &desc, cid);

    assert!(cache.get(&class, &method, &desc, cid).is_none());
    assert!(
        eventually_drained(|| lookup_jit_code_range(entry).is_none()),
        "removed code must unregister when no caller or reader owns it"
    );
}

#[test]
fn test_replaced_callee_is_owned_by_baked_direct_caller() {
    let cache = JitCache::new();
    let cid = cratonvm_types::ClassId::new(7);
    let callee_class: Arc<str> = Arc::from("OwnedCallee");
    let callee_method: Arc<str> = Arc::from("target");
    let desc: Arc<str> = Arc::from("()V");

    let mut callee_buf = ExecutableBuffer::new(64).expect("alloc callee");
    callee_buf.emit(&[0xC3]);
    cache.put(
        callee_class.clone(),
        callee_method.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(callee_buf),
    );
    let old_entry = cache
        .get(&callee_class, &callee_method, &desc, cid)
        .expect("callee")
        .entry_ptr() as usize;

    let mut caller_buf = ExecutableBuffer::new(64).expect("alloc caller");
    caller_buf.emit(&[0xC3]);
    let mut caller = CompiledMethod::new(caller_buf);
    caller._direct_callee_entries.push(old_entry);
    let caller_class: Arc<str> = Arc::from("OwningCaller");
    let caller_method: Arc<str> = Arc::from("call");
    cache.put(
        caller_class.clone(),
        caller_method.clone(),
        desc.clone(),
        cid,
        caller,
    );

    let mut replacement_buf = ExecutableBuffer::new(64).expect("alloc replacement");
    replacement_buf.emit(&[0xC3]);
    cache.put(
        callee_class.clone(),
        callee_method.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(replacement_buf),
    );
    assert!(
        lookup_jit_code_range(old_entry).is_some(),
        "the caller's baked edge must pin its superseded callee"
    );

    cache.remove(&caller_class, &caller_method, &desc, cid);
    assert!(
        eventually_drained(|| lookup_jit_code_range(old_entry).is_none()),
        "dropping the final direct caller must reclaim the old body"
    );
}

/// A body whose baked direct-call target cannot be pinned must NOT reach
/// the cache.
///
/// `prepare_for_publication` upgrades every `_direct_callee_entries` address
/// into a strong `Arc` so the emitted `call` keeps its callee mapped. When
/// one no longer resolves — the callee was superseded between the compile
/// driver baking its address and this publication — the body carries a call
/// to an address nothing owns. Publishing it anyway (the historical
/// default-OFF `CRATONVM_JIT_STRICT_CALLEE_ROOTS` behaviour) is the
/// retired `jit-wild-jump-page-aligned-pc-20260728` crash: the callee is
/// retired on an ordinary tier-up while NO thread is inside compiled code —
/// a legal, quiescent retirement that `defer_jit_owner` cannot help with,
/// because a baked address holds no `Arc` — and the caller's next
/// invocation jumps into the unmapped page (`SIGSEGV`, `pc == addr`, at the
/// callee's page-aligned entry).
#[test]
fn a_body_with_an_unpinnable_baked_callee_is_not_published() {
    // The scenario needs an entry no `put` ever registered an owner for,
    // whose buffer is already gone: exactly what `resolve_jit_entry_owner`
    // cannot pin. That address is recyclable, so a concurrently allocating
    // test can adopt it at any point -- including between establishing it
    // and publishing the caller. Retry the whole scenario when that
    // happens, and tell the two outcomes apart by re-reading ownership at
    // the assertion: a publish while the orphan is STILL unowned is the
    // real defect and fails, exactly as it always did.
    let class: Arc<str> = Arc::from("DanglingCaller");
    let method: Arc<str> = Arc::from("call");
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(4713);

    let mut attempts = 0;
    loop {
        attempts += 1;
        assert!(
            attempts < 100,
            "a concurrently allocating test kept adopting the orphan address"
        );

        let cache = JitCache::new();
        let orphan_entry = {
            let mut buf = ExecutableBuffer::new(64).expect("alloc orphan callee");
            buf.emit(&[0xC3]);
            let cm = CompiledMethod::new(buf);
            let entry = cm.entry_ptr() as usize;
            drop(cm);
            entry
        };
        if resolve_jit_entry_owner(orphan_entry).is_some() {
            continue; // adopted before we could use it
        }

        let mut caller_buf = ExecutableBuffer::new(64).expect("alloc caller");
        caller_buf.emit(&[0xC3]);
        let mut caller = CompiledMethod::new(caller_buf);
        caller._direct_callee_entries.push(orphan_entry);
        cache.put(class.clone(), method.clone(), desc.clone(), cid, caller);

        if cache.get(&class, &method, &desc, cid).is_none() {
            break; // the body was refused, which is the invariant
        }

        // It WAS published. Legitimate only if someone adopted the orphan
        // between the check above and the `put`, making the callee
        // genuinely pinnable; otherwise this is the defect.
        assert!(
            resolve_jit_entry_owner(orphan_entry).is_some(),
            "a body whose baked `call` target is unowned must not be published; \
             it stays interpreted and recompiles once the callee is live again"
        );
    }
}

/// An inline cache that publishes a compiled entry MUST retain the
/// artifact owning it: generated code (`jit/src/x64.rs`'s MIC/PIC cascade)
/// loads the raw pointer into R11 and `CALL R11`s it with no validation,
/// so an entry whose last `Arc` has dropped is a call into an unmapped —
/// or recycled — executable mapping. The virtual dispatch helper used to
/// publish the MIC's entry with a bare atomic store on its
/// "class cached, target unresolved" path, taking no owner at all; that is
/// the NodeConnectionsServiceTests SIGSEGV.
#[test]
fn mic_update_retains_the_callee_artifact() {
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("OwnedTarget");
    let method: Arc<str> = Arc::from("run");
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(4711);
    let mut buf = ExecutableBuffer::new(64).expect("alloc owned target");
    buf.emit(&[0xC3]);
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(buf),
    );
    let published = cache
        .get(&class, &method, &desc, cid)
        .expect("target published");
    let entry = published.entry_ptr() as usize;
    // Identity, not address: once this artifact is unmapped its address can
    // be handed straight back to a concurrently-allocating test, and
    // `resolve_jit_entry_owner` would then correctly report SOMEONE at
    // `entry`. A `Weak` to our own body cannot be confused that way.
    let artifact = Arc::downgrade(&published);
    drop(published);

    let slot = JitMICSlot::new();
    slot.update(cid.as_u32(), &class, entry as u64, false, false);
    assert_eq!(
        slot.cached_entry().0 as usize,
        entry,
        "a live entry must be published"
    );
    assert!(
        slot.compiled_owner.lock().is_some(),
        "publishing an entry must take a strong owner for its artifact"
    );

    // Withdrawing the cache's own reference must not release the artifact
    // while the slot still points at it.
    cache.remove(&class, &method, &desc, cid);
    assert!(
        resolve_jit_entry_owner(entry).is_some(),
        "the slot's owner must keep the callee artifact alive"
    );

    slot.clear_compiled_entry();
    assert!(
        eventually_drained(|| artifact.strong_count() == 0),
        "clearing the last holder must release the artifact"
    );
}

/// The structural half of the same fix: if any caller hands an inline
/// cache an entry whose artifact is already gone, the cache must refuse it
/// instead of handing it to generated code. A *registered* address whose
/// `Weak` no longer upgrades is exactly that case, and is distinguishable
/// from a native target, which was never registered and stays publishable.
#[test]
fn inline_caches_refuse_a_dead_artifact_entry_but_allow_unregistered_ones() {
    const DEAD_ENTRY: usize = 0x0DEAD_1000;
    const NATIVE_ENTRY: u64 = 0x0CAFE_2000;
    {
        let mut buf = ExecutableBuffer::new(64).expect("alloc dead target");
        buf.emit(&[0xC3]);
        let arc = Arc::new(CompiledMethod::new(buf));
        jit_entry_owners()
            .lock()
            .insert(DEAD_ENTRY, Arc::downgrade(&arc));
        drop(arc);
    }

    let slot = JitMICSlot::new();
    slot.update(21, "DeadOwner", DEAD_ENTRY as u64, false, false);
    assert_eq!(
        slot.cached_entry_word
            .load(std::sync::atomic::Ordering::Acquire),
        0,
        "a dead artifact's entry must never be published to generated code"
    );

    let native_slot = JitMICSlot::new();
    native_slot.update(22, "NativeOwner", NATIVE_ENTRY, false, false);
    assert_eq!(
        native_slot.cached_entry().0,
        NATIVE_ENTRY,
        "non-artifact targets (natives, trampolines) stay publishable"
    );

    let pic = JitPICSlot::new();
    pic.install(21, "DeadOwner", DEAD_ENTRY as u64, false, false);
    assert!(
        pic.lookup_megamorphic(21).is_none(),
        "the hashed overflow table must refuse a dead entry too"
    );
    for i in 0..JIT_PIC_ENTRIES {
        assert_eq!(
            pic.entry_words[i].load(std::sync::atomic::Ordering::Acquire),
            0,
            "no PIC way may hold a dead entry"
        );
    }
}

#[test]
fn test_inline_cache_reclamation_waits_for_jit_quiescence() {
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("DeferredTarget");
    let method: Arc<str> = Arc::from("run");
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(19);
    let mut buf = ExecutableBuffer::new(64).expect("alloc deferred target");
    buf.emit(&[0xC3]);
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        CompiledMethod::new(buf),
    );
    let entry = cache
        .get(&class, &method, &desc, cid)
        .expect("target")
        .entry_ptr() as usize;
    let slot = JitMICSlot::new();
    slot.update(cid.as_u32(), &class, entry as u64, false, false);

    let execution = jit_execution_enter();
    slot.clear_compiled_entry();
    cache.remove(&class, &method, &desc, cid);
    assert!(
        lookup_jit_code_range(entry).is_some(),
        "a raw cache reader may still be between load and call"
    );
    jit_execution_leave(execution);
    // `eventually_drained`, not a bare assertion: `ACTIVE_JIT_EXECUTIONS`
    // is process-global, so this thread's `leave` is the final transition
    // only if no sibling test is inside its own execution epoch. What is
    // being checked is that the deferred owner is released once JIT
    // execution IS quiescent — and the pump is what makes that observable
    // here rather than whenever some unrelated test happens to leave.
    assert!(
        eventually_drained(|| lookup_jit_code_range(entry).is_none()),
        "the quiescent transition must drain deferred code owners"
    );
}

/// A thread parked inside compiled code — a pool worker blocked in
/// `LinkedBlockingQueue.take()` — used to hold `ACTIVE_JIT_EXECUTIONS` up
/// for as long as it stayed parked, so nothing retired after it parked was
/// ever released and the code cache filled to its cap. While blocked, the
/// thread can only return into what its stack names: a retired body it
/// does not name must be reclaimed without waiting for it to wake, and one
/// it does name must be kept until it leaves.
#[test]
fn a_thread_parked_in_compiled_code_holds_only_what_its_stack_names() {
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("ParkedInJitClass");
    let desc: Arc<str> = Arc::from("()V");
    let cid = cratonvm_types::ClassId::new(4723);
    let publish = |name: &str| {
        let method: Arc<str> = Arc::from(name);
        let mut buf = ExecutableBuffer::new(64).expect("alloc body");
        buf.emit(&[0xC3]);
        cache.put(
            class.clone(),
            method.clone(),
            desc.clone(),
            cid,
            CompiledMethod::new(buf),
        );
        let cm = cache.get(&class, &method, &desc, cid).expect("published");
        let entry = cm.entry_ptr() as usize;
        (method, Arc::downgrade(&cm), entry)
    };
    let (unrelated_method, unrelated, _) = publish("unrelated");
    let (held_method, held, held_entry) = publish("held");

    let (parked_tx, parked_rx) = std::sync::mpsc::channel::<()>();
    let (wake_tx, wake_rx) = std::sync::mpsc::channel::<()>();
    let parked = std::thread::spawn(move || {
        let execution = jit_execution_enter();
        // This thread's stack band while parked: a return address into
        // `held`, and nothing that points into `unrelated`.
        let band: [usize; 8] = [0, 0x10, held_entry + 4, 0, 0, 0, 0, 0];
        let lo = band.as_ptr() as usize;
        let hi = lo + std::mem::size_of_val(&band);
        // SAFETY: `band` is a live local array on this thread's stack for
        // the whole blocked window.
        unsafe { jit_thread_blocked_enter(lo, hi) };
        parked_tx.send(()).expect("the test waits for the park");
        wake_rx.recv().expect("the test wakes the thread");
        jit_thread_blocked_leave();
        std::hint::black_box(&band);
        jit_execution_leave(execution);
    });
    parked_rx.recv().expect("the thread parks");

    cache.remove(&class, &unrelated_method, &desc, cid);
    cache.remove(&class, &held_method, &desc, cid);
    assert!(
        eventually_drained(|| unrelated.strong_count() == 0),
        "a retired body no parked thread can return into must be reclaimed \
         while that thread is still parked inside compiled code"
    );
    assert_ne!(
        held.strong_count(),
        0,
        "a retired body the parked thread's stack names must be kept"
    );

    wake_tx.send(()).expect("the parked thread is waiting");
    parked.join().expect("the parked thread finishes");
    assert!(
        eventually_drained(|| held.strong_count() == 0),
        "and released once the thread has left compiled code"
    );
}

#[test]
fn test_sharded_cache_supports_concurrent_lock_free_reads_and_publication() {
    let cache = Arc::new(JitCache::new());
    let mut workers = Vec::new();
    for worker in 0..4u32 {
        let cache = cache.clone();
        workers.push(std::thread::spawn(move || {
            for method_index in 0..32u32 {
                let class: Arc<str> = Arc::from(format!("Concurrent{worker}"));
                let method: Arc<str> = Arc::from(format!("m{method_index}"));
                let desc: Arc<str> = Arc::from("()V");
                let cid = cratonvm_types::ClassId::new(worker * 32 + method_index + 1);
                let mut buf = ExecutableBuffer::new(16).expect("alloc concurrent body");
                buf.emit(&[0xC3]);
                cache.put(
                    class.clone(),
                    method.clone(),
                    desc.clone(),
                    cid,
                    CompiledMethod::new(buf),
                );
                assert!(cache.get(&class, &method, &desc, cid).is_some());
            }
        }));
    }
    for worker in workers {
        worker.join().expect("cache worker");
    }
    assert_eq!(cache.len(), 128);
    assert_eq!(cache.clear_all(), 128);
    assert!(cache.is_empty());
}

#[test]
fn test_code_range_snapshot_stays_searchable_during_publication() {
    let base = 0x7f00_0000_0000usize;
    for i in 0..128usize {
        register_jit_code_range(base + i * 0x1000, 0x100, i + 1);
    }
    let readers = (0..4)
        .map(|_| {
            std::thread::spawn(move || {
                for _ in 0..1_000 {
                    for i in 0..128usize {
                        assert_eq!(lookup_jit_code_range(base + i * 0x1000 + 0x40), Some(i + 1));
                    }
                }
            })
        })
        .collect::<Vec<_>>();
    for reader in readers {
        reader.join().expect("range reader");
    }
    for i in 0..128usize {
        unregister_jit_code_range(base + i * 0x1000);
    }
}

/// `clear_all` must evict every entry and drop every published body —
/// `CompiledMethod::drop` is what returns the executable mapping AND
/// withdraws that body's `JIT_CODE_RANGES` registration.
///
/// The post-conditions below are deliberately phrased against state this
/// test OWNS rather than against the process-global range table. This lib
/// test binary runs its tests on a thread pool and `JIT_CODE_RANGES` is
/// process-global, so the old `lookup_jit_code_range(entry).is_none()`
/// assertion was order-dependent: `clear_all` unmaps our code pages, a
/// concurrently-running test then gets one of those very addresses back
/// from `ExecutableBuffer::new` and registers ITS range over it, and the
/// lookup correctly reports `Some(<that other test's body>)`. Holding a
/// `Weak` to each body keeps its `Arc` allocation (not the body) alive, so
/// `own_*` can never be recycled underneath the comparison and
/// `strong_count() == 0` proves *our* `Drop` ran.
// T2.2 epoch invalidation for the interpreter's Bytecode invoke cache
//
// These tests guard the contract the interpreter's cached-invoke `Bytecode`
// arms now rely on: instead of calling `JitCache::get` (three string hashes
// + three string compares) on every interpreted call just to notice that a
// background compile published a body, they memoize
// `jit_cache_generation()` in `CachedBytecodeMethod::jit_probe_generation`
// and only re-probe when the live generation differs.
//
// Every assertion below is written to be robust under a parallel `cargo
// test`: the generation is a process-global, so other tests can advance it
// concurrently. That can only push two sampled values FURTHER apart, so
// "must differ" assertions are race-free; no test here asserts that the
// generation stayed equal across a window in which another test could run.

fn probe_test_method(
    class: &str,
    method: &str,
    desc: &str,
    cid: cratonvm_types::ClassId,
) -> CachedBytecodeMethod {
    CachedBytecodeMethod {
        declaring_class_id: cid,
        class_name: Arc::from(class),
        method_name: Arc::from(method),
        method_descriptor: Arc::from(desc),
        source_file: None,
        code: Arc::from([0xb1u8].as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 0,
        max_locals: 0,
        num_params: 0,
        is_synchronized: false,
        is_static: true,
        force_native_cache: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    }
}

fn probe_ret_body() -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    CompiledMethod::new(buf)
}

/// THE staleness test. Replays the interpreter's exact epoch protocol
/// end-to-end and proves that a **first-time** publication (not a
/// replacement) forces the memoized call site back onto the slow path,
/// where it picks up the newly published body.
///
/// This is the case that used to be broken: `JitCache::put` advanced the
/// generation only when it *replaced* an existing entry, so a call site that
/// had already memoized "no compiled body for this method" would have stayed
/// pinned to the interpreter forever after the background worker published
/// its first body.
#[test]
fn jit_probe_epoch_forces_reprobe_after_first_publication() {
    let cache = JitCache::new();
    let cid = cratonvm_types::ClassId::new(4101);
    let class: Arc<str> = Arc::from("ProbeEpochA");
    let method: Arc<str> = Arc::from("hot");
    let desc: Arc<str> = Arc::from("()V");
    let cached = probe_test_method(&class, &method, &desc, cid);

    // 1. Cold: the call site probes, misses, and memoizes the miss --
    //    exactly what the `Bytecode` arm does on its first interpreted call.
    let gen_at_probe = jit_cache_generation();
    assert!(
        cache.get(&class, &method, &desc, cid).is_none(),
        "nothing published yet"
    );
    cached.record_jit_probe_miss(gen_at_probe);
    assert!(
        cached.jit_probe_is_current(gen_at_probe),
        "the memo must suppress a re-probe while the generation is unchanged"
    );

    // 2. A background compile publishes this method's FIRST body.
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        probe_ret_body(),
    );

    // 3. The memo must now report stale, so the interpreter re-probes...
    let gen_after_publish = jit_cache_generation();
    assert!(
        !cached.jit_probe_is_current(gen_after_publish),
        "a first-time publication MUST advance the JIT cache generation, or an \
         interpreted call site that already memoized a probe miss would never \
         notice the new body"
    );
    // ...and the re-probe finds the freshly published body.
    assert!(
        cache.get(&class, &method, &desc, cid).is_some(),
        "the re-probe must pick up the published body"
    );
}

/// An OSR publication lands in a separate map (`osr_methods`) but reaches the
/// same `Bytecode` call sites, so it must advance the generation too. Same
/// first-publication (non-replacement) shape as above.
#[test]
fn jit_probe_epoch_forces_reprobe_after_first_osr_publication() {
    let cache = JitCache::new();
    let cid = cratonvm_types::ClassId::new(4102);
    let class: Arc<str> = Arc::from("ProbeEpochB");
    let method: Arc<str> = Arc::from("loopy");
    let desc: Arc<str> = Arc::from("(I)I");
    let cached = probe_test_method(&class, &method, &desc, cid);

    let gen_at_probe = jit_cache_generation();
    cached.record_jit_probe_miss(gen_at_probe);

    let mut osr = probe_ret_body();
    osr.compiled_via_osr = true;
    cache.put_osr(class.clone(), method.clone(), desc.clone(), cid, osr);

    assert!(
        !cached.jit_probe_is_current(jit_cache_generation()),
        "put_osr must advance the generation on a first publication"
    );
    assert!(cache.get_osr(&class, &method, &desc, cid).is_some());
}

/// A C1->C2 supersede republishes under an existing key. That path already
/// bumped (it is a replacement), but it is part of the audited contract, so
/// pin it: a call site that had flipped to `Jit` and then been downgraded
/// back to `Bytecode` must still re-probe and find the C2 body.
#[test]
fn jit_probe_epoch_forces_reprobe_after_supersede_republication() {
    let cache = JitCache::new();
    let cid = cratonvm_types::ClassId::new(4103);
    let class: Arc<str> = Arc::from("ProbeEpochC");
    let method: Arc<str> = Arc::from("tiered");
    let desc: Arc<str> = Arc::from("()J");
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        probe_ret_body(),
    );
    let c1_entry = cache
        .get(&class, &method, &desc, cid)
        .expect("C1 body")
        .entry_ptr() as usize;

    let cached = probe_test_method(&class, &method, &desc, cid);
    let gen_at_probe = jit_cache_generation();
    cached.record_jit_probe_miss(gen_at_probe);

    // C2 replaces the C1 entry under the same key.
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        probe_ret_body(),
    );

    assert!(
        !cached.jit_probe_is_current(jit_cache_generation()),
        "a superseding republication must advance the generation"
    );
    let c2_entry = cache
        .get(&class, &method, &desc, cid)
        .expect("C2 body")
        .entry_ptr() as usize;
    assert_ne!(c1_entry, c2_entry, "C2 must have replaced the C1 artifact");
}

/// Invalidation/eviction must advance the generation as well. It is not a
/// staleness hazard for the negative memo (a removal cannot make "there is
/// no body" wrong), but the interpreter's `Jit` entries are re-derived
/// through this same probe, so pin the behaviour rather than leave it to
/// chance.
#[test]
fn jit_probe_epoch_advances_on_invalidation() {
    let cache = JitCache::new();
    let cid = cratonvm_types::ClassId::new(4104);
    let class: Arc<str> = Arc::from("ProbeEpochD");
    let method: Arc<str> = Arc::from("gone");
    let desc: Arc<str> = Arc::from("()V");
    cache.put(
        class.clone(),
        method.clone(),
        desc.clone(),
        cid,
        probe_ret_body(),
    );
    assert!(cache.get(&class, &method, &desc, cid).is_some());

    let cached = probe_test_method(&class, &method, &desc, cid);
    let gen_before_remove = jit_cache_generation();
    cached.record_jit_probe_miss(gen_before_remove);

    cache.remove(&class, &method, &desc, cid);
    assert!(cache.get(&class, &method, &desc, cid).is_none());
    assert!(
        !cached.jit_probe_is_current(jit_cache_generation()),
        "an eviction must advance the generation"
    );
}

/// The memoized invocation-counter key must be exactly the canonical
/// `invoc_key_parts` key every other door computes -- the key indexes
/// `ProfileStore`'s per-method warmup counters, so a door with a different
/// value would count a method's warmup in a counter nobody else reads.
#[test]
fn memoized_invoc_key_matches_the_open_coded_hash() {
    let cid = cratonvm_types::ClassId::new(7);
    let cached = probe_test_method("pkg/Hash", "someMethod", "(Ljava/lang/String;I)Z", cid);
    let expected = cratonvm_jit_api::invoc_key_parts(
        cid.as_u32(),
        &cached.method_name,
        &cached.method_descriptor,
    );
    assert_eq!(cached.invoc_key(), expected);
    // Memoized: stable across calls.
    assert_eq!(cached.invoc_key(), expected);
    // And it survives a clone (the manual `Clone` impl).
    assert_eq!(cached.clone().invoc_key(), expected);
}

#[test]
fn test_jit_cache_clear_all_evicts_entries() {
    let cache = JitCache::new();
    let class_a: Arc<str> = Arc::from("TestClassA");
    let method_a: Arc<str> = Arc::from("testA");
    let desc_a: Arc<str> = Arc::from("()V");
    let class_b: Arc<str> = Arc::from("TestClassB");
    let method_b: Arc<str> = Arc::from("testB");
    let desc_b: Arc<str> = Arc::from("(I)I");

    let cid_a = cratonvm_types::ClassId::new(1);
    let cid_b = cratonvm_types::ClassId::new(2);
    let mut buf_a = ExecutableBuffer::new(16).expect("alloc failed");
    buf_a.emit(&[0xC3]); // RET
    cache.put(
        class_a.clone(),
        method_a.clone(),
        desc_a.clone(),
        cid_a,
        CompiledMethod::new(buf_a),
    );

    let mut buf_b = ExecutableBuffer::new(16).expect("alloc failed");
    buf_b.emit(&[0xC3]); // RET
    cache.put(
        class_b.clone(),
        method_b.clone(),
        desc_b.clone(),
        cid_b,
        CompiledMethod::new(buf_b),
    );

    let cm_a = cache
        .get(&class_a, &method_a, &desc_a, cid_a)
        .expect("compiled A");
    let cm_b = cache
        .get(&class_b, &method_b, &desc_b, cid_b)
        .expect("compiled B");
    let entry_a = cm_a.entry_ptr() as usize;
    let entry_b = cm_b.entry_ptr() as usize;
    // The `cm_ptr` each body registered in `JIT_CODE_RANGES` (see
    // `JitCache::put`), used below to tell our own registration apart from a
    // recycled-address one belonging to another test.
    let own_a = Arc::as_ptr(&cm_a) as usize;
    let own_b = Arc::as_ptr(&cm_b) as usize;
    let weak_a = Arc::downgrade(&cm_a);
    let weak_b = Arc::downgrade(&cm_b);
    drop(cm_a);
    drop(cm_b);

    assert_eq!(cache.len(), 2);
    assert_eq!(cache.clear_all(), 2);
    assert!(cache.is_empty());
    assert!(cache.get(&class_a, &method_a, &desc_a, cid_a).is_none());
    assert!(cache.get(&class_b, &method_b, &desc_b, cid_b).is_none());

    // The cache held the last strong reference, so eviction must have run
    // `CompiledMethod::drop` — which unmaps the code and unregisters the
    // range — for both bodies.
    assert!(
        eventually_drained(|| weak_a.strong_count() == 0),
        "clear_all must release the last owner of body A"
    );
    assert!(
        eventually_drained(|| weak_b.strong_count() == 0),
        "clear_all must release the last owner of body B"
    );
    // And no code range still binds our entry addresses to OUR bodies. A
    // `Some(other)` here just means a concurrently-running test has already
    // been handed the freed page back; that is not this cache's business.
    assert_ne!(
        lookup_jit_code_range(entry_a),
        Some(own_a),
        "clear_all must withdraw body A's own code-range registration"
    );
    assert_ne!(
        lookup_jit_code_range(entry_b),
        Some(own_b),
        "clear_all must withdraw body B's own code-range registration"
    );
}

/// `clear_all` must return every unreferenced executable mapping, and with
/// it the `COMMITTED_JIT_CODE_BYTES` those mappings account for.
///
/// `COMMITTED_JIT_CODE_BYTES` is process-global and this lib test binary
/// runs its tests on a thread pool, so absolute before/after totals are NOT
/// a stable observable: every other test that allocates or drops an
/// `ExecutableBuffer` moves the same counter. The old
/// `populated >= before + 8 * 4096` / `reclaimed <= populated - 8 * 4096`
/// pair therefore failed whenever concurrent frees (resp. allocations)
/// happened to outpace this test's own bookkeeping between the two loads.
///
/// What this test owns instead:
///  * the counter is an exact ledger — `ExecutableBuffer::new` adds
///    `capacity` and its `Drop` subtracts the same `capacity` — so at any
///    instant it equals the summed capacity of all *live* buffers. That
///    makes `>= our own live bytes` a sound lower bound no matter what
///    other threads are doing.
///  * `Drop` is the *only* site that decrements the counter and releases
///    the mapping, so proving `clear_all` dropped the last owner of each
///    body proves exactly `committed_by_test` bytes went back.
#[test]
fn test_clear_all_releases_committed_executable_bytes() {
    const BODY_BYTES: usize = 4096;
    const BODIES: u32 = 8;

    let cache = JitCache::new();
    let mut bodies = Vec::with_capacity(BODIES as usize);
    for i in 0..BODIES {
        let mut buf = ExecutableBuffer::new(BODY_BYTES).expect("alloc cache body");
        buf.emit(&[0xC3]);
        let class: Arc<str> = Arc::from("ReclaimBytes");
        let method: Arc<str> = Arc::from(format!("m{i}"));
        let desc: Arc<str> = Arc::from("()V");
        let cid = cratonvm_types::ClassId::new(i + 1);
        cache.put(
            class.clone(),
            method.clone(),
            desc.clone(),
            cid,
            CompiledMethod::new(buf),
        );
        let cm = cache.get(&class, &method, &desc, cid).expect("published");
        bodies.push(Arc::downgrade(&cm));
    }
    let committed_by_test = BODIES as usize * BODY_BYTES;

    // Our eight mappings are live, so the global ledger must be at least
    // that large — every other contribution to it is a live buffer too.
    let populated = COMMITTED_JIT_CODE_BYTES.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        populated >= committed_by_test,
        "committed ledger {populated} is below this test's own live mappings \
         ({committed_by_test} bytes)"
    );

    assert_eq!(cache.clear_all(), BODIES as usize);

    // Every body lost its last owner, so `CompiledMethod::drop` ran for all
    // eight — unmapping the code and returning `committed_by_test` bytes.
    for (i, weak) in bodies.iter().enumerate() {
        assert!(
            eventually_drained(|| weak.strong_count() == 0),
            "clear_all must return body m{i}'s executable mapping"
        );
    }
}

#[test]
fn test_jit_cache_get_missing() {
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("Missing");
    let method: Arc<str> = Arc::from("missing");
    let desc: Arc<str> = Arc::from("()V");
    assert!(cache
        .get(&class, &method, &desc, cratonvm_types::ClassId::new(1))
        .is_none());
}

/// A type-check site's `(ptr, len)` pair is the KEY of two thread-local
/// memos that outlive any one compiled method
/// (`JIT_TYPECHECK_TARGET_CACHE`, `JIT_TYPECHECK_ANSWER_CACHE` in
/// `vm/src/jit/helpers.rs`). It must therefore be a permanent, unique
/// identity for one class name.
///
/// Regression: these names used to be `Box<str>`s owned by
/// `CompiledMethod::_jit_strings`, freed on tier-up / invalidation /
/// eviction, so the allocator recycled the block for the next
/// compilation's name of the same length and both memos then answered for
/// the class that used to live there. `java/lang/String` and
/// `java/lang/Number` are both 16 bytes, which is how a `String` came to
/// pass `instanceof java/lang/Number` and blow up
/// `scala.runtime.BoxesRunTime.equals2`'s very next `checkcast`.
#[test]
fn typecheck_class_names_are_interned_by_content_not_owned_by_a_compilation() {
    let (p_string, l_string) = intern_typecheck_class_name("java/lang/String");
    assert_eq!(l_string, 16);

    // Churn the allocator the way a run of compile-then-drop cycles does.
    // Nothing here may be handed the interned block back.
    let mut churn: Vec<Box<str>> = Vec::new();
    for _ in 0..512 {
        churn.push(String::from("java/lang/Number").into_boxed_str());
        churn.push(String::from("java/lang/Object").into_boxed_str());
        if churn.len() > 8 {
            churn.drain(..4);
        }
    }
    drop(churn);

    let (p_number, l_number) = intern_typecheck_class_name("java/lang/Number");
    assert_eq!(l_number, l_string, "the two names are the same length");
    assert_ne!(
        p_string, p_number,
        "two different class names must never share a (ptr, len) key"
    );

    // Same content -> same pointer, so two sites checking the same class
    // share one memo entry instead of evicting each other.
    assert_eq!(
        intern_typecheck_class_name("java/lang/String"),
        (p_string, l_string)
    );

    // And the bytes still read back as themselves after all that churn.
    for (ptr, len, expect) in [
        (p_string, l_string, "java/lang/String"),
        (p_number, l_number, "java/lang/Number"),
    ] {
        // SAFETY: interned entries are leaked and never freed.
        let s = unsafe { std::str::from_utf8(std::slice::from_raw_parts(ptr, len)) }.unwrap();
        assert_eq!(s, expect);
    }
}

/// Two loaders' same-named classes are two classes, and a type-check site
/// must be able to say which one it meant.
///
/// The class dictionary is keyed by `(ClassLoaderId, name)`, but a
/// compiled type-check site used to carry only the name — so
/// `jit_typecheck_resolve` re-resolved it at run time, could land on the
/// wrong copy, and covered for that with a loader-blind name walk that
/// accepted *either* copy. Interning by `(name, resolved ClassId)` gives
/// each copy its own site identity and lets the helper answer by identity.
#[test]
fn a_typecheck_site_is_interned_under_the_class_id_its_loader_resolved() {
    let (p_a, l_a) = intern_typecheck_target("com/example/Forked", Some(4_242));
    let (p_b, l_b) = intern_typecheck_target("com/example/Forked", Some(4_243));
    assert_eq!(l_a, l_b);
    assert_ne!(
        p_a, p_b,
        "one name resolved to two loaders' classes must be two sites"
    );
    assert_eq!(typecheck_target_for_site(p_a), Some(4_242));
    assert_eq!(typecheck_target_for_site(p_b), Some(4_243));

    // Stable: recompiling the same site rejoins the same identity.
    assert_eq!(
        intern_typecheck_target("com/example/Forked", Some(4_242)),
        (p_a, l_a)
    );

    // A site whose target was not loaded at compile time records nothing,
    // and so keeps the pre-existing name-resolution behaviour.
    let (p_none, _) = intern_typecheck_class_name("com/example/Forked");
    assert_ne!(p_none, p_a);
    assert_eq!(typecheck_target_for_site(p_none), None);
}

/// T10.3 — Verify the FxHashMap swap preserves insert/lookup semantics for
/// 100 distinct JitKey entries with different class/method/descriptor mixes.
#[test]
fn t10_jit_cache_fxhash_insert_lookup() {
    let mut cache = JitCache::new();
    let mut keys: Vec<(Arc<str>, Arc<str>, Arc<str>, cratonvm_types::ClassId)> =
        Vec::with_capacity(100);
    for i in 0..100 {
        let class: Arc<str> = Arc::from(format!("pkg/Cls{i}"));
        let method: Arc<str> = Arc::from(format!("m{i}"));
        let desc: Arc<str> = Arc::from(format!("(I)I{i}"));
        let cid = cratonvm_types::ClassId::new((i + 1) as u32);
        let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
        buf.emit(&[0xC3]); // RET
        let cm = CompiledMethod::new(buf);
        cache.put(class.clone(), method.clone(), desc.clone(), cid, cm);
        keys.push((class, method, desc, cid));
    }
    assert_eq!(cache.len(), 100);
    for (class, method, desc, cid) in &keys {
        assert!(
            cache.get(class, method, desc, *cid).is_some(),
            "missing key {}/{}/{}",
            class,
            method,
            desc
        );
    }
    // Negative lookup: a class name we never inserted.
    let missing_cls: Arc<str> = Arc::from("pkg/Unseen");
    let missing_m: Arc<str> = Arc::from("x");
    let missing_d: Arc<str> = Arc::from("()V");
    assert!(cache
        .get(
            &missing_cls,
            &missing_m,
            &missing_d,
            cratonvm_types::ClassId::new(9999)
        )
        .is_none());
}

// ── count_param_slots tests ─────────────────────────────────────

#[test]
fn test_count_param_slots_basic() {
    assert_eq!(count_param_slots("(II)I"), 2);
    assert_eq!(count_param_slots("()V"), 0);
    assert_eq!(count_param_slots("(I)V"), 1);
}

#[test]
fn test_count_param_slots_long_double() {
    assert_eq!(count_param_slots("(JD)V"), 2);
    assert_eq!(count_param_slots("(IJI)I"), 3);
}

#[test]
fn test_count_param_slots_objects_arrays() {
    assert_eq!(count_param_slots("(Ljava/lang/String;)V"), 1);
    assert_eq!(count_param_slots("([I[Ljava/lang/Object;I)V"), 3);
    assert_eq!(count_param_slots("([[I)V"), 1);
}

#[test]
fn test_count_param_slots_empty_string() {
    assert_eq!(count_param_slots(""), 0);
}

// ── indy_arg_type_tags tests ─────────────────────────────────────

#[test]
fn indy_arg_type_tags_matches_the_writingservlet_concat_shape() {
    // WritingServlet.doGet's `makeConcatWithConstants` bootstrap
    // descriptor: (int length, String buffered, long elapsedNanos).
    assert_eq!(
        indy_arg_type_tags("(ILjava/lang/String;J)Ljava/lang/String;"),
        vec![b'I', b'L', b'J']
    );
}

#[test]
fn indy_arg_type_tags_one_tag_per_compact_slot() {
    assert_eq!(
        count_param_slots("(IJDF)V"),
        indy_arg_type_tags("(IJDF)V").len()
    );
    assert_eq!(indy_arg_type_tags("(IJDF)V"), vec![b'I', b'J', b'D', b'F']);
}

#[test]
fn indy_arg_type_tags_arrays_are_l() {
    assert_eq!(
        indy_arg_type_tags("([I[Ljava/lang/Object;)V"),
        vec![b'L', b'L']
    );
}

#[test]
fn indy_arg_type_tags_empty() {
    assert_eq!(indy_arg_type_tags("()V"), Vec::<u8>::new());
    assert_eq!(indy_arg_type_tags(""), Vec::<u8>::new());
}

#[test]
fn test_compute_param_jvm_slots_category2_instance() {
    assert_eq!(
        compute_param_jvm_slots("(Ljava/lang/Object;JZ)V", false),
        (vec![0, 1, 2, 4], 5)
    );
    assert_eq!(
        compute_param_jvm_slots("(JLjava/lang/Object;)V", true),
        (vec![0, 2], 3)
    );
}

#[test]
fn invokestatic_self_call_tail_jump_predicate() {
    let tail_call = [0x1a, 0xb8, 0x00, 0x01, 0xac, 0x00, 0x00];
    assert!(invokestatic_self_call_uses_tail_jump(&tail_call, 5, 1));

    let non_tail_call = [0x1a, 0xb8, 0x00, 0x01, 0x1a, 0xac, 0x00, 0x00];
    assert!(!invokestatic_self_call_uses_tail_jump(&non_tail_call, 6, 1));

    let void_return_tail_shape = [0xb8, 0x00, 0x01, 0xb1, 0x00, 0x00];
    assert!(!invokestatic_self_call_uses_tail_jump(
        &void_return_tail_shape,
        4,
        0
    ));
}

// ── return_type tests ───────────────────────────────────────────

#[test]
// x86-64 only: this asserts how `try_compile_inner` ROUTES a compile
// (single-pass vs IR vs dispatch), and on another architecture that
// function takes its `#[cfg(target_arch = "aarch64")]` branch and
// returns before any of that routing happens. Gated so the crate's
// test run is green on aarch64 rather than carrying known reds.
#[cfg(target_arch = "x86_64")]
fn recursive_compile_cycle_routes_parent_direct_call_through_dispatch() {
    // Held for the whole test: the recursive-cycle set this clears and
    // then asserts on is process-global. See
    // `jit_recursive_cycle_test_lock`.
    let _cycle_lock = super::jit_recursive_cycle_test_lock();
    // This test exercises the optimizing IR pipeline, which is gated off
    // whenever the young generation can relocate. Pin the policy so the
    // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
    crate::x64::set_moving_young_override(Some(false));
    use std::sync::Arc;

    clear_jit_recursive_cycle_methods_for_test();
    // This test exercises the direct-callee-compile path (`callee_compiler`
    // below); see `direct_jit_callee_calls_enabled`.
    // `CRATONVM_JIT_DIRECT_CALLEE_CALLS` is a *declared* flag, served from
    // the process-wide snapshot that latches on the first read of any flag
    // (`cratonvm_types::flags`). `set_var` here therefore did nothing at
    // all once any earlier test in this binary had touched a flag — the
    // assertions below were riding on the flag's default (enabled) rather
    // than on the value this test asked for, and would have silently
    // stopped testing the direct-callee path the day that default flipped.
    // The override pins it for real, on this thread only, so it cannot
    // perturb a parallel test.
    let _direct_callee_calls = cratonvm_types::flags::override_thread(
        cratonvm_types::flags::VmFlags::from_env_with_edits(&[
            ("CRATONVM_JIT_DIRECT_CALLEE_CALLS", Some("1")),
            // Pins the pre-wave-3 rule this test asserts; the new default is
            // `r9w3_runtime3_tests::a_published_cycle_participant_is_bound_directly`.
            ("CRATONVM_JIT_CYCLE_DIRECT_BIND", Some("0")),
        ]),
    );

    let a_cached = CachedBytecodeMethod {
        declaring_class_id: cratonvm_types::ClassId::new(1),
        class_name: Arc::from("pkg/A"),
        method_name: Arc::from("a"),
        method_descriptor: Arc::from("()V"),
        source_file: None,
        code: Arc::from([0xb8, 0x00, 0x01, 0xb1, 0x00, 0x00].as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 0,
        max_locals: 0,
        num_params: 0,
        is_synchronized: false,
        is_static: true,
        force_native_cache: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    };
    let b_cached = CachedBytecodeMethod {
        declaring_class_id: cratonvm_types::ClassId::new(2),
        class_name: Arc::from("pkg/B"),
        method_name: Arc::from("b"),
        method_descriptor: Arc::from("()V"),
        source_file: None,
        code: Arc::from([0xb8, 0x00, 0x01, 0xb1, 0x00, 0x00].as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 0,
        max_locals: 0,
        num_params: 0,
        is_synchronized: false,
        is_static: true,
        force_native_cache: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    };
    // SAFETY: every helper address is an integer slot. This test only
    // inspects emitted metadata and never executes the generated code.
    let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    let a_resolver = |cp_idx: u16| -> Option<(String, String, String)> {
        (cp_idx == 1).then(|| ("pkg/B".to_string(), "b".to_string(), "()V".to_string()))
    };
    let b_resolver = |cp_idx: u16| -> Option<(String, String, String)> {
        (cp_idx == 1).then(|| ("pkg/A".to_string(), "a".to_string(), "()V".to_string()))
    };
    let callee_compiler =
        |class_name: &str, method_name: &str, descriptor: &str| -> Option<(usize, bool)> {
            assert_eq!((class_name, method_name, descriptor), ("pkg/B", "b", "()V"));
            let compiled_b = try_compile(
                &b_cached,
                None,
                None,
                None,
                Some(&b_resolver),
                None,
                None,
                None,
                None,
                None,
                &helpers,
                None,
                None,
                None,
                None,
                false,
                false,
                false,
                false,
                false,
                false,
                None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
            )?;
            assert_eq!(
                compiled_b._jit_invoke_infos.len(),
                1,
                "B -> A must use dispatch after seeing A on the compile stack"
            );
            let compiled_b = Box::leak(Box::new(compiled_b));
            Some((compiled_b.entry_ptr() as usize, compiled_b.needs_context()))
        };

    let compiled_a = try_compile(
        &a_cached,
        None,
        None,
        None,
        Some(&a_resolver),
        Some(&callee_compiler),
        None,
        None,
        None,
        None,
        &helpers,
        None,
        None,
        None,
        None,
        false,
        false,
        false,
        false,
        false,
        false,
        None, // cp_invokedynamic_descriptor_resolver: no indy in these test methods
    )
    .expect("A should compile");

    assert!(jit_direct_call_requires_dispatch("pkg/A", "a", "()V"));
    assert!(jit_direct_call_requires_dispatch("pkg/B", "b", "()V"));
    assert_eq!(
        compiled_a._jit_invoke_infos.len(),
        1,
        "A -> B must fall back to dispatch once B is marked as a cycle participant"
    );

    clear_jit_recursive_cycle_methods_for_test();
}

/// A statically-bound JIT-to-JIT direct call MUST still carry a
/// `JitInvokeInfo` for its pc.
///
/// `emit_inline_callee_deopt_check` is emitted only when the codegen has
/// one, and it is the only thing that can notice the raw CALL's callee
/// trapped: without it the callee's `i64::MIN` sentinel reaches the
/// caller's shared exception-check stub, which reloads the sentinel and
/// returns, and the frame the callee stashed under ITS OWN key travels up
/// to a consumer that cannot attribute it. Measured on H2 `TestScript`:
/// 582 unserviced direct call sites, one of which
/// (`ValueVarchar.get(String,CastDataProvider)` → `StringUtils.cache`)
/// produced the orphan in
/// `jit-direct-call-mints-an-orphaned-deopt-frame`.
///
/// The bind used to `continue` straight past the registration at the end
/// of the scan loop, so this asserted 0 before the fix.
#[test]
// x86-64 only: this asserts how `try_compile_inner` ROUTES a compile
// (single-pass vs IR vs dispatch), and on another architecture that
// function takes its `#[cfg(target_arch = "aarch64")]` branch and
// returns before any of that routing happens. Gated so the crate's
// test run is green on aarch64 rather than carrying known reds.
#[cfg(target_arch = "x86_64")]
fn statically_bound_direct_callee_call_still_registers_invoke_info() {
    // The other clearer of the process-global recursive-cycle set;
    // see `jit_recursive_cycle_test_lock`.
    let _cycle_lock = super::jit_recursive_cycle_test_lock();
    crate::x64::set_moving_young_override(Some(false));
    use std::sync::Arc;

    clear_jit_recursive_cycle_methods_for_test();
    let _direct_callee_calls = cratonvm_types::flags::override_thread(
        cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
            "CRATONVM_JIT_DIRECT_CALLEE_CALLS",
            Some("1"),
        )]),
    );

    let mk = |class: &str, name: &str, id: u32, code: &[u8]| CachedBytecodeMethod {
        declaring_class_id: cratonvm_types::ClassId::new(id),
        class_name: Arc::from(class),
        method_name: Arc::from(name),
        method_descriptor: Arc::from("()V"),
        source_file: None,
        code: Arc::from(code),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 0,
        max_locals: 0,
        num_params: 0,
        is_synchronized: false,
        is_static: true,
        force_native_cache: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    };
    // `d` calls `e` once and returns; `e` is a leaf, so nothing marks it a
    // recursive-cycle target and the direct bind actually happens.
    let d_cached = mk("pkg/D", "d", 11, &[0xb8, 0x00, 0x01, 0xb1, 0x00, 0x00]);
    let e_cached = mk("pkg/E", "e", 12, &[0xb1, 0x00, 0x00]);

    // SAFETY: every helper address is an integer slot. This test only
    // inspects emitted metadata and never executes the generated code.
    let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    let d_resolver = |cp_idx: u16| -> Option<(String, String, String)> {
        (cp_idx == 1).then(|| ("pkg/E".to_string(), "e".to_string(), "()V".to_string()))
    };
    let callee_compiler =
        |class_name: &str, method_name: &str, descriptor: &str| -> Option<(usize, bool)> {
            assert_eq!((class_name, method_name, descriptor), ("pkg/E", "e", "()V"));
            let compiled_e = try_compile(
                &e_cached, None, None, None, None, None, None, None, None, None, &helpers, None,
                None, None, None, false, false, false, false, false, false, None,
            )?;
            let compiled_e = Box::leak(Box::new(compiled_e));
            Some((compiled_e.entry_ptr() as usize, compiled_e.needs_context()))
        };

    let compiled_d = try_compile(
        &d_cached,
        None,
        None,
        None,
        Some(&d_resolver),
        Some(&callee_compiler),
        None,
        None,
        None,
        None,
        &helpers,
        None,
        None,
        None,
        None,
        false,
        false,
        false,
        false,
        false,
        false,
        None,
    )
    .expect("D should compile");

    assert!(
        !jit_direct_call_requires_dispatch("pkg/E", "e", "()V"),
        "the fixture must exercise the DIRECT bind, not the dispatch fallback"
    );
    // Non-vacuity: without this the assertion below passes on a compile
    // where the site fell back to `jit_invoke_dispatch` (which registers a
    // `JitInvokeInfo` of its own), i.e. on a fixture that never exercised
    // the direct bind at all.
    assert_eq!(
        compiled_d._direct_callee_entries.len(),
        1,
        "the fixture must bind E's compiled entry as a DIRECT call"
    );
    assert_eq!(
        compiled_d._jit_invoke_infos.len(),
        1,
        "a direct JIT-to-JIT call must still register a JitInvokeInfo for its pc, \
         or the codegen cannot emit the callee-deopt service check"
    );

    clear_jit_recursive_cycle_methods_for_test();
}

#[test]
fn tomcat_bcel_read_interfaces_direct_calls_use_dispatch() {
    assert!(jit_direct_call_requires_dispatch(
        "org/apache/tomcat/util/bcel/classfile/ClassParser",
        "readInterfaces",
        "()V",
    ));
    assert!(!jit_direct_call_requires_dispatch(
        "org/apache/tomcat/util/bcel/classfile/ClassParser",
        "readFields",
        "()V",
    ));
}

#[test]
fn test_return_type_basic() {
    assert_eq!(return_type("(II)I"), b'I');
    assert_eq!(return_type("()V"), b'V');
    assert_eq!(return_type("(I)J"), b'J');
    assert_eq!(return_type("()Ljava/lang/String;"), b'L');
}

#[test]
fn test_return_type_no_closing_paren() {
    // Malformed descriptor — should return 'V' as default
    assert_eq!(return_type("II"), b'V');
}

// ── return_type edge cases ─────────────────────────────────────

#[test]
fn test_return_type_array() {
    assert_eq!(return_type("()[I"), b'[');
}

#[test]
fn test_return_type_double() {
    assert_eq!(return_type("(II)D"), b'D');
}

// ── Escape analysis IR wiring tests ────────────────────────────

#[test]
fn test_ea_from_ir_returns_id_map() {
    // Build a minimal IR graph: Start(0) → Const(1) → Return(2)
    let mut g = ir::Graph {
        nodes: Vec::new(),
        entry: 0,
        exit: 0,
        safepoints: Vec::new(),
        uses: Default::default(),
        receiver_param: None,
    };
    let start = g.add(ir::Op::Start, ir::IrType::Void, vec![], None);
    let c = g.add(ir::Op::Const(42), ir::IrType::Int, vec![], None);
    let ret = g.add(ir::Op::Return, ir::IrType::Void, vec![start, c], None);
    g.entry = start;
    g.exit = ret;

    let (ea_graph, id_map) = escape_analysis_from_ir(&g);

    // id_map length matches IR node count
    assert_eq!(id_map.len(), g.nodes.len());
    // Entry maps to EA Start (0), Exit maps to EA Return (1)
    assert_eq!(id_map[start as usize], 0);
    assert_eq!(id_map[ret as usize], 1);
    // Const node should map to a valid EA node (not usize::MAX)
    assert_ne!(id_map[c as usize], usize::MAX);
    // EA graph should have nodes
    assert!(ea_graph.nodes.len() >= 3);
}

#[test]
fn test_apply_ea_marks_dead_nodes() {
    // Build an IR graph with: Start, New, Const, Store, Load, Return
    // The New doesn't escape, so EA should find it scalar-replaceable.
    let mut g = ir::Graph {
        nodes: Vec::new(),
        entry: 0,
        exit: 0,
        safepoints: Vec::new(),
        uses: Default::default(),
        receiver_param: None,
    };
    let start = g.add(ir::Op::Start, ir::IrType::Void, vec![], None);
    let alloc = g.add(
        ir::Op::New {
            class_id: 1,
            num_fields: 2,
        },
        ir::IrType::Ref,
        vec![start],
        None,
    );
    let c42 = g.add(ir::Op::Const(42), ir::IrType::Int, vec![], None);
    // Store field 0 (MemKind::Int = discriminant 0)
    let store = g.add(
        ir::Op::Store(ir::MemKind::Int),
        ir::IrType::Void,
        vec![alloc, c42],
        None,
    );
    // Load field 0 (MemKind::Int = discriminant 0)
    let load = g.add(
        ir::Op::Load(ir::MemKind::Int),
        ir::IrType::Int,
        vec![alloc],
        None,
    );
    let ret = g.add(ir::Op::Return, ir::IrType::Void, vec![start, load], None);
    g.entry = start;
    g.exit = ret;

    // Run EA
    let (ea_graph, id_map) = escape_analysis_from_ir(&g);
    let ea_result = escape_analysis::analyze_escapes(&ea_graph);

    // Apply to IR
    if !ea_result.scalar_replaceable.is_empty() || !ea_result.elide_locks.is_empty() {
        apply_ea_to_ir(&mut g, &id_map, &ea_result);
    }

    // If EA found the alloc as scalar-replaceable, the allocation,
    // store, and load should all be Dead now.
    if !ea_result.scalar_replaceable.is_empty() {
        assert_eq!(g.nodes[alloc as usize].op, ir::Op::Dead);
        assert_eq!(g.nodes[store as usize].op, ir::Op::Dead);
        assert_eq!(g.nodes[load as usize].op, ir::Op::Dead);
        // The return node's input that was the load should now point
        // to the stored constant value (c42).
        assert!(
            g.nodes[ret as usize].inputs.contains(&c42),
            "Return should reference the constant directly after scalar replacement"
        );
    }
}

// --- Phase 80.3: Value Enum Layout Safety Tests ---

#[test]
fn value_size_is_16_bytes() {
    assert_eq!(std::mem::size_of::<Value>(), 16);
}

#[test]
fn value_alignment_at_most_8() {
    assert!(std::mem::align_of::<Value>() <= 8);
}

#[test]
fn objectref_is_pointer_sized() {
    assert_eq!(
        std::mem::size_of::<ObjectRef>(),
        std::mem::size_of::<*mut u8>()
    );
}

#[test]
fn value_to_bytes_roundtrip() {
    // Verify value_to_bytes produces correct bytes for known values.
    let val = Value::Int(0x12345678);
    let bytes = super::value_to_bytes(val);
    // The Int discriminant and payload must be present somewhere in the 16 bytes.
    // Check the i32 payload bytes appear.
    let payload = 0x12345678_i32.to_le_bytes();
    let found = (0..13).any(|i| bytes[i..i + 4] == payload);
    assert!(found, "Int payload must be present in byte representation");
}

#[test]
fn probe_object_ptr_offset_in_range() {
    // The offset must be a valid position within a 16-byte Value.
    let off = super::probe_object_ptr_offset();
    assert!(
        off < 9,
        "offset must leave room for 8-byte pointer within 16 bytes, got {off}"
    );
}

#[test]
fn probe_object_null_template_reconstructs_none() {
    // The null template (lo, hi) must reconstruct to Value::Object(None)
    // when written back and transmuted.
    let (lo, hi) = super::probe_object_null_template();
    let mut bytes = [0u8; 16];
    bytes[0..8].copy_from_slice(&lo.to_le_bytes());
    bytes[8..16].copy_from_slice(&hi.to_le_bytes());
    // SAFETY: `Value` is 16 bytes (asserted at compile time), and these bytes are the template the VM derived from a real `Value::Object(None)`.
    let reconstructed: Value = unsafe { std::mem::transmute(bytes) };
    assert_eq!(
        reconstructed,
        Value::Object(None),
        "null template must reconstruct to Value::Object(None)"
    );
}

// ===================================================================
// Session 33: JIT Monomorphic Inline Cache (MIC) tests
// ===================================================================

#[test]
fn s33_mic_slot_new_is_empty() {
    let mic = JitMICSlot::new();
    assert_eq!(
        mic.cached_class_id
            .load(std::sync::atomic::Ordering::Relaxed),
        JitMICSlot::EMPTY_CLASS_ID
    );
    assert!(mic.cached_class_name.lock().is_none());
    assert_eq!(mic.cached_entry(), (0, false));
    assert_eq!(mic.hits.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(mic.misses.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(mic.total_observations(), 0);
    assert_eq!(mic.hit_rate_pct(), 0);
}

#[test]
fn s33_mic_slot_prepopulate() {
    let mic = JitMICSlot::new();
    mic.prepopulate(42);
    assert_eq!(
        mic.cached_class_id
            .load(std::sync::atomic::Ordering::Relaxed),
        42
    );
    // Entry word should still be 0 (prepopulate only sets class_id)
    assert_eq!(
        mic.cached_entry_word
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

#[test]
fn s33_mic_slot_update_all_fields() {
    let mic = JitMICSlot::new();
    // Even: bit 0 of an entry word is the context tag.
    mic.update(7, "com/example/MyClass", 0xDEAD_BEE0, true, false);
    assert_eq!(
        mic.cached_class_id
            .load(std::sync::atomic::Ordering::Acquire),
        7
    );
    assert_eq!(
        mic.cached_class_name.lock().as_deref(),
        Some("com/example/MyClass")
    );
    assert_eq!(mic.cached_entry(), (0xDEAD_BEE0, true));
    assert_eq!(
        mic.cached_entry_word
            .load(std::sync::atomic::Ordering::Acquire),
        0xDEAD_BEE0 | JIT_IC_NEEDS_CONTEXT_TAG
    );
}

/// An address with bit 0 set cannot be encoded next to the context tag, so
/// it is refused rather than published as a different address.
#[test]
fn s33_mic_slot_refuses_an_entry_with_the_tag_bit_set() {
    let mic = JitMICSlot::new();
    mic.update(7, "com/example/MyClass", 0xDEAD_BEEF, false, false);
    assert_eq!(mic.cached_entry(), (0, false));
}

#[test]
fn s33_mic_slot_clear_compiled_entry_keeps_receiver_cache() {
    let mic = JitMICSlot::new();
    mic.update(7, "com/example/MyClass", 0xDEAD_BEE0, true, false);
    mic.clear_compiled_entry();

    assert_eq!(
        mic.cached_class_id
            .load(std::sync::atomic::Ordering::Acquire),
        7
    );
    assert_eq!(
        mic.cached_class_name.lock().as_deref(),
        Some("com/example/MyClass")
    );
    assert_eq!(mic.cached_entry(), (0, false));
}

#[test]
fn s33_mic_slot_hit_miss_counters() {
    let mic = JitMICSlot::new();
    for _ in 0..10 {
        mic.record_hit();
    }
    for _ in 0..5 {
        mic.record_miss();
    }
    assert_eq!(mic.hits.load(std::sync::atomic::Ordering::Relaxed), 10);
    assert_eq!(mic.misses.load(std::sync::atomic::Ordering::Relaxed), 5);
    assert_eq!(mic.total_observations(), 15);
    // hit_rate = 10/15 * 100 = 66
    assert_eq!(mic.hit_rate_pct(), 66);
}

#[test]
fn s33_mic_slot_is_monomorphic() {
    let mic = JitMICSlot::new();
    // Not enough observations
    for _ in 0..5 {
        mic.record_hit();
    }
    assert!(!mic.is_monomorphic());
    // 10 hits, 0 misses → 100% hit rate, ≥10 obs → monomorphic
    for _ in 0..5 {
        mic.record_hit();
    }
    assert!(mic.is_monomorphic());
}

#[test]
fn s33_mic_slot_mostly_hits_is_monomorphic() {
    let mic = JitMICSlot::new();
    for _ in 0..18 {
        mic.record_hit();
    }
    for _ in 0..2 {
        mic.record_miss();
    }
    assert!(mic.is_monomorphic());
}

/// A raw inline MIC has no helper boundary between its guard load and
/// indirect CALL, so retargeting a populated slot risks pairing one
/// receiver's class guard with another receiver's entry. `update` is
/// therefore monomorphic for the slot's lifetime: the first class to
/// install wins, and a later `update` for a DIFFERENT class is a no-op
/// (that receiver simply takes the ordinary helper path instead).
#[test]
fn s33_mic_slot_update_first_install_wins_no_retarget() {
    let mic = JitMICSlot::new();
    mic.update(1, "A", 100, false, false);
    mic.update(2, "B", 200, true, false);
    assert_eq!(
        mic.cached_class_id
            .load(std::sync::atomic::Ordering::Acquire),
        1
    );
    assert_eq!(mic.cached_class_name.lock().as_deref(), Some("A"));
    assert_eq!(mic.cached_entry(), (100, false));
}

/// A re-`update` for the SAME class as already installed must stay a
/// no-op (idempotent), not tear the entry/name/context triple.
#[test]
fn s33_mic_slot_update_same_class_is_idempotent() {
    let mic = JitMICSlot::new();
    mic.update(1, "A", 100, false, false);
    mic.update(1, "A", 998, true, false);
    assert_eq!(mic.cached_entry(), (100, false));
}

/// `prepopulate` seeds only `cached_class_id` (a profile-derived guard
/// hint) with `cached_entry_word` still 0. The first `update` for that
/// SAME class id must still publish a real entry — this is the shape
/// `mark_current_jit_compile_method_recursive_cycle`'s caller
/// (profile-guided MIC seeding, see `try_compile_inner`) relies on.
#[test]
fn s33_mic_slot_update_resolves_prepopulated_hint() {
    let mic = JitMICSlot::new();
    mic.prepopulate(7);
    assert_eq!(mic.cached_entry(), (0, false));
    mic.update(7, "Hinted", 0x4242, true, false);
    assert_eq!(
        mic.cached_class_id
            .load(std::sync::atomic::Ordering::Acquire),
        7
    );
    assert_eq!(mic.cached_entry(), (0x4242, true));
    assert_eq!(mic.cached_class_name.lock().as_deref(), Some("Hinted"));
}

#[test]
fn s33_mic_slot_concurrent_updates() {
    use std::sync::Arc;
    let mic = Arc::new(JitMICSlot::new());
    let mut handles = Vec::new();
    for i in 0..8 {
        let mic_clone = mic.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..100 {
                if i % 2 == 0 {
                    mic_clone.record_hit();
                } else {
                    mic_clone.record_miss();
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    // 4 threads * 100 hits + 4 threads * 100 misses = 800
    assert_eq!(mic.total_observations(), 800);
    assert_eq!(mic.hits.load(std::sync::atomic::Ordering::Relaxed), 400);
    assert_eq!(mic.misses.load(std::sync::atomic::Ordering::Relaxed), 400);
}

#[test]
fn s33_mic_slot_hit_rate_boundary() {
    // Exactly 90% hit rate should count as monomorphic
    let mic = JitMICSlot::new();
    for _ in 0..9 {
        mic.record_hit();
    }
    for _ in 0..1 {
        mic.record_miss();
    }
    // 10 total, 90% hits → monomorphic
    assert!(mic.is_monomorphic());
}

#[test]
fn s33_mic_slot_entry_ptr_zero_means_unresolved() {
    let mic = JitMICSlot::new();
    mic.prepopulate(5);
    // Even with class_id populated, a zero word means no direct dispatch
    assert_eq!(
        mic.cached_entry_word
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    // After update with non-zero entry, it's resolved
    mic.update(5, "Foo", 0x1234, false, false);
    assert_ne!(
        mic.cached_entry_word
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

// =======================================================================
// Phase G (RG.1 .. RG.12) — JIT / interpreter correctness invariants
//
// These tests pin the shape of every Phase G item: either the JIT accepts
// and lowers the opcode itself, or it must cleanly bail so the interpreter
// handles it correctly. Each test is self-contained and does not require
// the full VM to run.
// =======================================================================

use crate::x64::{is_jit_compatible, jit_scan};

/// Build a minimal bytecode header that ends in `ireturn` so `jit_scan`
/// sees a valid terminator after the probe opcodes.
fn ireturn_tail(mut prefix: Vec<u8>) -> Vec<u8> {
    // Push a zero int so ireturn has an operand.
    prefix.push(0x03); // iconst_0
    prefix.push(0xac); // ireturn
    prefix
}

fn lreturn_tail(mut prefix: Vec<u8>) -> Vec<u8> {
    prefix.push(0x09); // lconst_0
    prefix.push(0xad); // lreturn
    prefix
}

fn freturn_tail(mut prefix: Vec<u8>) -> Vec<u8> {
    prefix.push(0x0b); // fconst_0
    prefix.push(0xae); // freturn
    prefix
}

fn dreturn_tail(mut prefix: Vec<u8>) -> Vec<u8> {
    prefix.push(0x0e); // dconst_0
    prefix.push(0xaf); // dreturn
    prefix
}

/// RG.1 — A method containing `invokedynamic` (0xba) no longer vetoes
/// JIT compilation at the scan stage.
///
/// Previously ANY invokedynamic anywhere in a method's bytecode —
/// reachable or not — permanently blacklisted the WHOLE method from JIT
/// compilation. The overwhelmingly common source of a "surprise"
/// invokedynamic in otherwise-ordinary hot methods is
/// `assert cond : "msg" + var;` (javac lowers the message concat via
/// `StringConcatFactory`, guarded by the `assertionsDisabled` dead
/// branch), so this blanket veto forced hot per-call-site methods that
/// merely CONTAIN a dead assert into the interpreter forever (see
/// `...binary-docvalues-range-hang...md` for the concrete
/// Lucene `FSTCompiler`/`NodeHash` repro).
///
/// The new design: the scanner accepts 0xba and simply records the site
/// (`indy_ops`); the codegen (which DOES have CP access, unlike this
/// scan) lowers the instruction to an UNCONDITIONAL jump to the existing
/// uncommon-trap deopt stub (`DeoptReason::UnreachedCode`). If this exact
/// program point is ever actually reached at runtime (assertions
/// enabled, or a genuinely live indy), the method permanently reverts to
/// interpreter-only execution for the rest of the process — i.e. today's
/// status quo for that one method — so the interpreter's real
/// invokedynamic dispatch (makeConcat, lambda bootstraps) still runs
/// whenever the instruction is genuinely exercised. In the common case
/// (assertions disabled, dead branch) the trap is never taken and the
/// surrounding hot method compiles and runs at full JIT speed.
#[test]
fn rg1_jit_accepts_invokedynamic_at_scan_stage() {
    let code = ireturn_tail(vec![
        0xba, 0x00, 0x01, 0x00, 0x00, // invokedynamic #1, 0, 0
    ]);
    assert!(
        is_jit_compatible(&code, code.len(), "()I"),
        "the scanner must no longer bail on invokedynamic — it defers to \
         an unconditional uncommon-trap deopt in the codegen instead"
    );
    // The scan also records the site so the codegen can resolve its
    // descriptor and lower it to the deopt stub.
    let scan = x64::jit_scan(&code, code.len(), "()I").expect("scan must succeed");
    assert_eq!(
        scan.indy_ops,
        vec![(0, 1u16)],
        "invokedynamic site (pc, cp_index) must be recorded for codegen resolution"
    );
}

/// RG.2 — JIT accepts fcmpl/fcmpg/dcmpl/dcmpg opcodes. The scanner groups
/// them with lcmp in the 0x94..=0x98 range. NaN canonicalization itself is
/// verified in the compiler — here we only pin the scanner shape.
#[test]
fn rg2_jit_accepts_fp_compare_opcodes() {
    // Float: fconst_0 (0x0b), fconst_1 (0x0c), fcmpl (0x95), ireturn
    let fcmpl = vec![0x0b, 0x0c, 0x95, 0x03, 0xac];
    assert!(
        is_jit_compatible(&fcmpl, fcmpl.len(), "()I"),
        "fcmpl must be JIT-compatible"
    );

    // fconst_0, fconst_1, fcmpg
    let fcmpg = vec![0x0b, 0x0c, 0x96, 0x03, 0xac];
    assert!(
        is_jit_compatible(&fcmpg, fcmpg.len(), "()I"),
        "fcmpg must be JIT-compatible"
    );

    // dconst_0 (0x0e), dconst_1 (0x0f), dcmpl (0x97)
    let dcmpl = vec![0x0e, 0x0f, 0x97, 0x03, 0xac];
    assert!(
        is_jit_compatible(&dcmpl, dcmpl.len(), "()I"),
        "dcmpl must be JIT-compatible"
    );

    // dconst_0, dconst_1, dcmpg (0x98)
    let dcmpg = vec![0x0e, 0x0f, 0x98, 0x03, 0xac];
    assert!(
        is_jit_compatible(&dcmpg, dcmpg.len(), "()I"),
        "dcmpg must be JIT-compatible"
    );
}

/// RG.3 — JIT accepts lcmp (0x94).
#[test]
fn rg3_jit_accepts_lcmp() {
    // lconst_0 (0x09), lconst_1 (0x0a), lcmp (0x94), ireturn
    let lcmp = vec![0x09, 0x0a, 0x94, 0x03, 0xac];
    assert!(
        is_jit_compatible(&lcmp, lcmp.len(), "()I"),
        "lcmp must be JIT-compatible"
    );
}

/// RG.4 — JIT accepts tableswitch and lookupswitch and correctly skips
/// over their variable-length payloads during scanning.
#[test]
fn rg4_jit_accepts_switch_opcodes() {
    // tableswitch at pc=0 needs padding to 4-byte-aligned, so we prepend
    // iconst_0 (1 byte) + nop padding; but the scanner handles alignment
    // from the tableswitch opcode's own pc. Use a 1-byte prefix so pc=1,
    // and tableswitch at pc=1 needs 3 bytes of padding.
    //
    // Layout: [iconst_0=0x03] [tableswitch=0xaa] [pad0 pad0 pad0]
    //         [default=0,0,0,8] [low=0,0,0,0] [high=0,0,0,0]
    //         [offset0=0,0,0,8] [ireturn=0xac]
    //
    // default/offset0 are 32-bit signed offsets from the tableswitch pc.
    let mut ts = vec![0x03, 0xaa, 0, 0]; // iconst_0, tableswitch, 2 pad bytes (1..4 boundary)
    ts.extend_from_slice(&[0, 0, 0, 8]); // default offset = +8 (ireturn)
    ts.extend_from_slice(&[0, 0, 0, 0]); // low = 0
    ts.extend_from_slice(&[0, 0, 0, 0]); // high = 0 (1 entry)
    ts.extend_from_slice(&[0, 0, 0, 8]); // offset[0] = +8
    ts.push(0x03); // iconst_0
    ts.push(0xac); // ireturn
    assert!(
        is_jit_compatible(&ts, ts.len(), "()I"),
        "tableswitch must be JIT-compatible"
    );

    // lookupswitch: [iconst_0] [lookupswitch=0xab] [pad] [default=+14]
    //               [npairs=1] [match=0, offset=+14] [iconst_0] [ireturn]
    let mut ls = vec![0x03, 0xab, 0, 0]; // iconst_0, lookupswitch, 2 pad bytes
    ls.extend_from_slice(&[0, 0, 0, 16]); // default = +16
    ls.extend_from_slice(&[0, 0, 0, 1]); // npairs = 1
    ls.extend_from_slice(&[0, 0, 0, 0]); // match = 0
    ls.extend_from_slice(&[0, 0, 0, 16]); // offset = +16
    ls.push(0x03);
    ls.push(0xac);
    assert!(
        is_jit_compatible(&ls, ls.len(), "()I"),
        "lookupswitch must be JIT-compatible"
    );
}

/// RG.5 (updated by RBC.6) — the scanner now ACCEPTS explicit `athrow`
/// and records `has_athrow`; the compile gates restrict compilation to
/// methods with NO local exception handlers (the codegen lowers athrow
/// to "stash pending exception + return the i64::MIN deopt sentinel",
/// which cannot dispatch to an in-method handler — see
/// `try_compile_inner`'s `exception_table.is_empty()` gate and the OSR
/// trigger's decline in `vm/src/runtime/interpreter.rs::try_osr`).
#[test]
fn rg5_jit_scan_accepts_athrow_and_flags_it() {
    // aconst_null (0x01), athrow (0xbf), then iconst_0/ireturn filler.
    let code = vec![0x01, 0xbf, 0x03, 0xac];
    let scan =
        x64::jit_scan(&code, code.len(), "()I").expect("athrow method must pass jit_scan (RBC.6)");
    assert!(
        scan.has_athrow,
        "jit_scan must record has_athrow so compile gates can apply"
    );
    // A method without athrow must NOT set the flag.
    let plain = vec![0x03, 0xac];
    let scan =
        x64::jit_scan(&plain, plain.len(), "()I").expect("trivial method must pass jit_scan");
    assert!(!scan.has_athrow);
}

/// RG.6 — JIT scanner accepts `monitorenter` (0xc2) and `monitorexit`
/// (0xc3) so synchronized blocks are JIT-eligible. The compiler then
/// either elides the exact lock site (escape analysis proves its receiver
/// thread-local) or lowers it through the direct thin-lock runtime stub.
#[test]
fn rg6_jit_accepts_monitor_enter_exit() {
    // aconst_null, dup (0x59), monitorenter, monitorexit, pop (0x57), ireturn
    let code = vec![0x01, 0x59, 0xc2, 0xc3, 0x57, 0x03, 0xac];
    assert!(
        is_jit_compatible(&code, code.len(), "()I"),
        "monitorenter/exit must be accepted so synchronized methods can JIT"
    );
}

/// A handler that falls THROUGH to a loop back edge reads every local that
/// edge reads. `run_jit_callee_handler` asks exactly this question (through
/// the public [`handler_resume_requires_precise_locals`]) before it is
/// rebuild a compiled callee's handler frame from the callee's incoming
/// arguments alone, so a `false` here is a null local at runtime.
///
/// The shape is Spring Boot's
/// `BindConverter.convert(Object, TypeDescriptor, TypeDescriptor)`:
/// `for (ConversionService d : this.delegates) { try { … } catch (…) { … } }`,
/// whose catch block ends by continuing the loop. Local 2 here is the
/// enhanced-for's synthetic iterator — assigned before the try, read by the
/// back edge, and reachable from the handler only through its trailing
/// `goto`.
// Calls `precise_exception_frame_sites_supported`, whose admitted-opcode
// set is the x64 backend's — its siblings below carry the same gate.
#[cfg(target_arch = "x86_64")]
#[test]
fn handler_falling_through_to_a_loop_back_edge_reads_the_iterator_local() {
    use cratonvm_reader::attribute::ExceptionTableEntry;

    let code = vec![
        0x03, // 0:  iconst_0
        0x3c, // 1:  istore_1              n = 0
        0x2a, // 2:  aload_0               the List parameter
        0xb9, 0x00, 0x01, 0x01, 0x00, // 3:  invokeinterface List.iterator
        0x4d, // 8:  astore_2              the synthetic iterator
        0x2c, // 9:  aload_2               <- loop head
        0xb9, 0x00, 0x02, 0x01, 0x00, // 10: invokeinterface Iterator.hasNext
        0x99, 0x00, 0x11, // 15: ifeq -> 32
        0x2c, // 18: aload_2               <- protected range starts
        0xb9, 0x00, 0x03, 0x01, 0x00, // 19: invokeinterface Iterator.next
        0x57, // 24: pop
        0xa7, 0xff, 0xf0, // 25: goto -> 9 (back edge)
        0x4e, // 28: astore_3              <- handler: catch (…) e
        0xa7, 0xff, 0xec, // 29: goto -> 9 (handler CONTINUES the loop)
        0x1b, // 32: iload_1
        0xac, // 33: ireturn
    ];
    let table = vec![ExceptionTableEntry {
        start_pc: 18,
        end_pc: 28,
        handler_pc: 28,
        catch_type: 0,
    }];
    assert!(
        handler_resume_requires_precise_locals(
            &code,
            code.len(),
            &table,
            "(Ljava/util/List;)I",
            true
        ),
        "the handler's trailing goto reaches `aload_2`, the iterator local"
    );
    // And it is a shape the precise-frame relaxation admits — which is why
    // it COMPILES instead of staying interpreted, and therefore why the
    // reconstruction question arises at all.
    assert!(precise_exception_frame_sites_supported(
        &code,
        code.len(),
        &table
    ));
}

/// Negative control for the test above: the same loop, but the handler
/// RETURNS. Nothing it reaches reads a non-parameter local, so the
/// params-only reconstruction is sound and must not be refused — the
/// `iload_1` at pc 32 is past the handler's own `ireturn` and must not be
/// scanned into (an earlier raw-pc-order version of this dataflow did
/// exactly that and over-rejected).
#[test]
fn handler_that_returns_does_not_read_a_non_param_local() {
    use cratonvm_reader::attribute::ExceptionTableEntry;

    let code = vec![
        0x03, // 0:  iconst_0
        0x3c, // 1:  istore_1
        0x2a, // 2:  aload_0
        0xb9, 0x00, 0x01, 0x01, 0x00, // 3:  invokeinterface List.iterator
        0x4d, // 8:  astore_2
        0x2c, // 9:  aload_2
        0xb9, 0x00, 0x02, 0x01, 0x00, // 10: invokeinterface Iterator.hasNext
        0x99, 0x00, 0x11, // 15: ifeq -> 32
        0x2c, // 18: aload_2
        0xb9, 0x00, 0x03, 0x01, 0x00, // 19: invokeinterface Iterator.next
        0x57, // 24: pop
        0xa7, 0xff, 0xf0, // 25: goto -> 9
        0x4e, // 28: astore_3           <- handler
        0x03, // 29: iconst_0
        0xac, // 30: ireturn
        0x00, // 31: nop (padding so 32 is the ifeq target)
        0x1b, // 32: iload_1
        0xac, // 33: ireturn
    ];
    let table = vec![ExceptionTableEntry {
        start_pc: 18,
        end_pc: 28,
        handler_pc: 28,
        catch_type: 0,
    }];
    assert!(!handler_resume_requires_precise_locals(
        &code,
        code.len(),
        &table,
        "(Ljava/util/List;)I",
        true
    ));
}

#[cfg(target_arch = "x86_64")]
#[test]
fn synchronized_cleanup_has_precise_exception_site_coverage() {
    use cratonvm_reader::attribute::ExceptionTableEntry;

    // javac-style synchronized cleanup: save the lock in local 1, execute
    // a non-throwing body, release it, and use a catch-all handler to
    // release + rethrow. The handler reads the non-parameter lock local,
    // while every potentially throwing protected instruction is a monitor
    // runtime call that publishes a reason-9 snapshot.
    let code = vec![
        0x2a, // 0: aload_0
        0x59, // 1: dup
        0x4c, // 2: astore_1
        0xc2, // 3: monitorenter
        0x03, // 4: iconst_0
        0x3d, // 5: istore_2
        0x84, 0x02, 0x01, // 6: iinc 2, 1
        0x2b, // 9: aload_1
        0xc3, // 10: monitorexit
        0xb1, // 11: return
        0x4e, // 12: astore_3
        0x2b, // 13: aload_1
        0xc3, // 14: monitorexit
        0x2d, // 15: aload_3
        0xbf, // 16: athrow
    ];
    let table = vec![
        ExceptionTableEntry {
            start_pc: 4,
            end_pc: 11,
            handler_pc: 12,
            catch_type: 0,
        },
        ExceptionTableEntry {
            start_pc: 12,
            end_pc: 15,
            handler_pc: 12,
            catch_type: 0,
        },
    ];
    assert!(local_handler_reads_unsafe_local(
        &code,
        code.len(),
        &table,
        "(Ljava/lang/Object;)V",
        true,
    ));
    assert!(precise_exception_frame_sites_supported(
        &code,
        code.len(),
        &table,
    ));
}

#[cfg(target_arch = "x86_64")]
#[test]
fn protected_virtual_and_interface_calls_are_precise_exception_covered() {
    use cratonvm_reader::attribute::ExceptionTableEntry;

    // 0xb6/0xb9 ARE admitted (2026-07-31). They were admitted once before
    // (2026-07-27) and removed again by `a523715a84`, because admitting
    // them let Spring's `SimpleApplicationEventMulticaster.invokeListener`
    // compile and then read its own pre-`try` `errorHandler` local back as
    // null inside the handler
    // (springboot-rerun-20260728-small-residuals-cluster, Case 3).
    //
    // What makes them safe now is that `a523715a84` did not only narrow
    // this list — it also ADDED the `protected_precise_handler_call`
    // suppression to `x64.rs`, which forces a protected virtual/interface
    // call onto the dispatch path (ending in
    // `emit_post_invoke_exception_check`, which records the caller's
    // complete state) instead of the inline MIC/PIC cascade that CALLs a
    // raw entry with nothing recording the caller's locals. That was the
    // actual defect; the narrowing was belt to its braces.
    //
    // The acceptance test for the braces holding on their own is
    // `probes/HandlerLocalAcrossProtectedInvokeProbe.java`, which builds
    // the Spring shape — a non-parameter local assigned before the `try`,
    // read back in the handler — through `invokevirtual`,
    // `invokeinterface` and `invokespecial` throw sites, plus a nested
    // `try`, a reassignment after the protected call, and the
    // null-`errorHandler` path that must skip the protected region
    // entirely. Re-verify with THAT probe, not by inspection, before
    // touching this list again.
    let code = vec![
        0x2a, // 0: aload_0
        0xb6, 0x00, 0x01, // 1: invokevirtual #1
        0x57, // 4: pop
        0x2a, // 5: aload_0
        0xb9, 0x00, 0x02, 0x01, 0x00, // 6: invokeinterface #2, count=1
        0x57, // 11: pop
        0xb1, // 12: return
        0x4c, // 13: astore_1
        0x2b, // 14: aload_1
        0xbf, // 15: athrow
    ];
    let table = vec![ExceptionTableEntry {
        start_pc: 0,
        end_pc: 13,
        handler_pc: 13,
        catch_type: 0,
    }];
    assert!(precise_exception_frame_sites_supported(
        &code,
        code.len(),
        &table,
    ));

    // One un-admitted throwing opcode anywhere in the same protected
    // range is still enough to withhold coverage for the whole method —
    // this list is a conjunction, not a majority vote.
    //
    // The witness was `aaload` (0x32), "its inline bounds/null check bails
    // to the shared sentinel stub, which records no frame". That stopped
    // being true when the bounds check and the array null check grew
    // publishing exits (deopt-stub reasons 11 and 10) and array loads were
    // admitted, so the witness moved to `aastore` (0x53) — which is
    // deliberately still un-admitted for a reason of its own: its
    // ZGC-barrier fallback arm calls the void `jit_aastore` helper and takes
    // "deliberately NO `emit_post_invoke_exception_check`". See
    // `precise_array_access_enabled`.
    let mut with_aastore = code.clone();
    with_aastore.splice(1..1, [0x53]);
    let aastore_table = vec![ExceptionTableEntry {
        start_pc: 0,
        end_pc: 14,
        handler_pc: 14,
        catch_type: 0,
    }];
    assert!(!precise_exception_frame_sites_supported(
        &with_aastore,
        with_aastore.len(),
        &aastore_table,
    ));
}

/// `instanceof` inside a protected range no longer withholds coverage
/// (2026-08-05). It was never a throwing opcode in this backend: the 0xc1
/// arm emits one call to `jit_instanceof`, which returns 0 or 1 on every
/// path and never stashes a pending exception, and the codegen emits no
/// post-call exception check after it because there is nothing to check.
///
/// This is a static-admission test. The behavioural acceptance test is
/// `JitPreciseHandlerFrame.instanceofStep`
/// (`vm/tests/jit_local_exception_handler_tests.rs`), whose handler reads a
/// non-parameter local and whose protected range holds an `instanceof`
/// alongside a throwing `invokeinterface` — run THAT before touching the
/// list again.
#[cfg(target_arch = "x86_64")]
#[test]
fn protected_instanceof_is_precise_exception_covered() {
    use cratonvm_reader::attribute::ExceptionTableEntry;

    // `JitPreciseHandlerFrame.instanceofStep`'s shape, reduced: the range
    // holds an `instanceof` and an `invokeinterface`, the handler follows.
    let code = vec![
        0x2b, // 0: aload_1
        0xc1, 0x00, 0x01, // 1: instanceof #1
        0x57, // 4: pop
        0x2b, // 5: aload_1
        0xb9, 0x00, 0x02, 0x01, 0x00, // 6: invokeinterface #2, count=1
        0x57, // 11: pop
        0xb1, // 12: return
        0x4c, // 13: astore_1
        0xb1, // 14: return
    ];
    let table = vec![ExceptionTableEntry {
        start_pc: 0,
        end_pc: 13,
        handler_pc: 13,
        catch_type: 0,
    }];
    assert!(
        precise_exception_frame_sites_supported(&code, code.len(), &table),
        "an `instanceof` in a protected range must not withhold coverage"
    );
    assert_eq!(
        first_unsupported_precise_frame_site(&code, code.len(), &table),
        None
    );

    // The contrast case needs an opcode that genuinely still withholds.
    //
    // It used to be `checkcast` (0xc0), which was a valid foil until
    // 2026-08-11: `checkcast` throws, but its ONE lowering always follows
    // `helpers.checkcast` with `emit_post_invoke_exception_check`, so it
    // was publishing all along and is now admitted
    // (`precise_getstatic_checkcast_enabled`). Using it here would assert
    // the opposite of what the backend does.
    //
    // `arraylength` (0xbe) filled that role until round 9 (2026-09-18), when
    // its in-place null check started publishing (`emit_precise_array_npe_
    // check`) and it was admitted. `ldc` (0x12) filled it until 2026-09-19,
    // when its two emitters were found to end in `emit_post_alloc_oom_check`
    // -- which records a reason-9 frame at a protected bci -- and it was
    // admitted under `precise_alloc_athrow_enabled`. The integer divide
    // (`idiv`, 0x6c) is the foil now: nothing in its lowering publishes a
    // frame at the bci, and it is one byte, so the splice below stays simple.
    let mut with_idiv = code.clone();
    with_idiv.splice(1..1, [0x6c]);
    let idiv_table = vec![ExceptionTableEntry {
        start_pc: 0,
        end_pc: 14,
        handler_pc: 14,
        catch_type: 0,
    }];
    assert_eq!(
        first_unsupported_precise_frame_site(&with_idiv, with_idiv.len(), &idiv_table,),
        Some((1, 0x6c)),
        "idiv must still withhold coverage, and must name its own pc"
    );

    // The opcode that left the list on 2026-09-19, in the same shape.
    let mut with_ldc = code.clone();
    with_ldc.splice(1..1, [0x12, 0x01]);
    let ldc_table = vec![ExceptionTableEntry {
        start_pc: 0,
        end_pc: 15,
        handler_pc: 15,
        catch_type: 0,
    }];
    assert_eq!(
        first_unsupported_precise_frame_site(&with_ldc, with_ldc.len(), &ldc_table,),
        None,
        "a protected ldc publishes through emit_post_alloc_oom_check and must not withhold coverage"
    );

    // And the opcode that changed sides must now be clear, in the same
    // shape the old assertion used — so this test fails if the admission
    // is ever reverted without revisiting the argument for it.
    let mut with_checkcast = code.clone();
    with_checkcast.splice(1..1, [0xc0, 0x00, 0x03]);
    let cc_table = vec![ExceptionTableEntry {
        start_pc: 0,
        end_pc: 16,
        handler_pc: 16,
        catch_type: 0,
    }];
    assert_eq!(
        first_unsupported_precise_frame_site(&with_checkcast, with_checkcast.len(), &cc_table,),
        None,
        "checkcast publishes via emit_post_invoke_exception_check and must be admitted"
    );
}

/// `getfield`/`putfield` inside a protected range no longer withhold
/// coverage (2026-08-02). The exclusion outlived its cause: both top-level
/// field arms grew a precise null trap after it was written — see
/// `precise_frame_publishing_opcode`'s own doc for which commit did which.
///
/// This is a static-admission test. The behavioural acceptance tests are
/// `probes/Rbc6FieldProbe.java` (a handler reading a non-parameter local
/// after a field NPE) and `probes/SyncBlockFieldProbe.java` (javac's
/// `synchronized` cleanup handler releasing the monitor on the way out) —
/// re-run THOSE, not this, before touching the list again.
#[cfg(target_arch = "x86_64")]
#[test]
fn protected_field_access_is_precise_exception_covered() {
    use cratonvm_reader::attribute::ExceptionTableEntry;

    let code = vec![
        0x2a, // 0: aload_0
        0xb4, 0x00, 0x01, // 1: getfield #1
        0x57, // 4: pop
        0x2a, // 5: aload_0
        0x03, // 6: iconst_0
        0xb5, 0x00, 0x02, // 7: putfield #2
        0xb1, // 10: return
        0x4c, // 11: astore_1
        0x2b, // 12: aload_1
        0xbf, // 13: athrow
    ];
    let table = vec![ExceptionTableEntry {
        start_pc: 0,
        end_pc: 11,
        handler_pc: 11,
        catch_type: 0,
    }];
    assert!(precise_exception_frame_sites_supported(
        &code,
        code.len(),
        &table,
    ));
}

/// RG.7 — `System.arraycopy` is routed through the native registry as an
/// interpreter-level intrinsic. We verify the invokestatic dispatch path
/// is scanner-compatible; the actual copy is performed by the native.
#[test]
fn rg7_jit_accepts_arraycopy_invoke_site() {
    // aconst_null, iconst_0, aconst_null, iconst_0, iconst_0,
    // invokestatic #1 (placeholder CP index — scanner only checks opcode shape),
    // ireturn
    let code = vec![0x01, 0x03, 0x01, 0x03, 0x03, 0xb8, 0x00, 0x01, 0x03, 0xac];
    assert!(
        is_jit_compatible(&code, code.len(), "()I"),
        "invokestatic for System.arraycopy must scan OK"
    );
}

/// RG.8 — A method with a loop (back-edge via goto backward) must still
/// scan as JIT-compatible; the compiler emits an oop-map safepoint at
/// each back-edge when lowering to machine code.
#[test]
fn rg8_jit_accepts_loop_with_backedge() {
    // iconst_0, istore_1 (0x3c)              ; i = 0
    // iload_1 (0x1b), iconst_5 (0x08), if_icmpge (0xa2) +10 (forward to ireturn)
    // iinc local=1 by 1 (0x84)                ; i++
    // goto -8 (0xa7) backward to iload_1     ; back-edge
    // iconst_0, ireturn
    // Offsets are relative to the branch opcode's pc.
    let code = vec![
        0x03, 0x3c, // iconst_0, istore_1
        0x1b, 0x08, 0xa2, 0x00, 0x0a, // iload_1, iconst_5, if_icmpge +10
        0x84, 0x01, 0x01, // iinc 1, 1
        0xa7, 0xff, 0xf8, // goto -8
        0x03, 0xac, // iconst_0, ireturn
    ];
    assert!(
        is_jit_compatible(&code, code.len(), "()I"),
        "loop with back-edge must be JIT-compatible (safepoint injected by compiler)"
    );
}

/// RG.9 — `tiered.rs` carries the tier-up manager; verify the module
/// exists and exposes the expected hot-counter threshold so the
/// interpreter-to-JIT transition (OSR entry) is active at runtime.
#[test]
fn rg9_tiered_manager_exists() {
    // Spot-check: module is reachable from the jit crate root and the
    // default policy produces a usable manager.
    let _manager = crate::tiered::TieredCompilationManager::with_default_policy();
}

/// RG.10 — The interpreter uses typed value-stack slots with automatic
/// reference-int coercion. We cannot touch the putfield/putstatic blocks
/// per project constraints, so this test pins the reader's ability to
/// decode those opcodes — any regression at the reader layer would show
/// here first.
#[test]
fn rg10_reader_decodes_putfield_putstatic() {
    use cratonvm_reader::instruction::Instruction;
    // putfield #1
    let (inst, next) = Instruction::decode(&[0xb5, 0x00, 0x01], 0).unwrap();
    assert_eq!(next, 3);
    assert!(matches!(inst, Instruction::Putfield(1)));
    // putstatic #2
    let (inst, next) = Instruction::decode(&[0xb3, 0x00, 0x02], 0).unwrap();
    assert_eq!(next, 3);
    assert!(matches!(inst, Instruction::Putstatic(2)));
}

/// RG.11 — The `wide` prefix (0xc4) extends the following index to u16.
/// Reader produces an `Iload(u16)` (or Lload/Fload/Dload/Aload/Istore/...
/// variants) with the widened index. Method with >255 locals must work.
#[test]
fn rg11_wide_prefix_decodes_u16_index() {
    use cratonvm_reader::instruction::Instruction;
    // wide iload 300 → 0xc4 0x15 0x01 0x2c
    let (inst, next) = Instruction::decode(&[0xc4, 0x15, 0x01, 0x2c], 0).unwrap();
    assert_eq!(next, 4);
    match inst {
        Instruction::Iload(idx) => assert_eq!(idx, 300),
        other => panic!("expected Iload(300), got {other:?}"),
    }
    // wide iinc index=500 const=2 → 0xc4 0x84 0x01 0xf4 0x00 0x02
    let (inst, next) = Instruction::decode(&[0xc4, 0x84, 0x01, 0xf4, 0x00, 0x02], 0).unwrap();
    assert_eq!(next, 6);
    match inst {
        Instruction::Iinc { index, constant } => {
            assert_eq!(index, 500);
            assert_eq!(constant, 2);
        }
        other => panic!("expected Iinc, got {other:?}"),
    }
}

/// RG.12 — The `invokedynamic` opcode (0xba) is decoded by the reader
/// with its 4-operand shape (cp index + 2 reserved zero bytes) and
/// routed to `Instruction::Invokedynamic(u16)`. The interpreter then
/// delegates to the real bootstrap (LambdaMetafactory, etc.) in
/// `vm/src/runtime/invokedynamic.rs`.
#[test]
fn rg12_reader_decodes_invokedynamic() {
    use cratonvm_reader::instruction::Instruction;
    // invokedynamic #42, 0, 0 → 0xba 0x00 0x2a 0x00 0x00
    let (inst, next) = Instruction::decode(&[0xba, 0x00, 0x2a, 0x00, 0x00], 0).unwrap();
    assert_eq!(next, 5);
    match inst {
        Instruction::Invokedynamic(idx) => assert_eq!(idx, 42),
        other => panic!("expected Invokedynamic(42), got {other:?}"),
    }
}

/// RG.3 extra — JIT accepts all long/double return types so lcmp/dcmp can
/// appear in methods returning any numeric primitive.
#[test]
fn rg3_jit_accepts_long_and_double_returns() {
    assert!(is_jit_compatible(&lreturn_tail(vec![]), 2, "()J"));
    assert!(is_jit_compatible(&freturn_tail(vec![]), 2, "()F"));
    assert!(is_jit_compatible(&dreturn_tail(vec![]), 2, "()D"));
}

/// Scan-result sanity: an empty-body `public void foo() {}` must scan.
#[test]
fn rg_scan_sanity_void_return() {
    let code = vec![0xb1]; // return (void)
    let r = jit_scan(&code, code.len(), "()V");
    assert!(r.is_some(), "void return must scan");
}

// ── JIT code-cache cap tests ────────────────────────────────────
//
// These avoid mutating `CRATONVM_JIT_CODE_CACHE_MAX_MB`: `jit_code_cache_cap_bytes`
// caches its value in a process-wide `OnceLock`, so an env-var test would be
// order-dependent and could poison the cache for the rest of the suite. We
// exercise the observable behaviour through `COMMITTED_JIT_CODE_BYTES` and the
// public accessors instead.

#[test]
fn code_cache_cap_default_is_nonzero_and_below_disable_sentinel() {
    let cap = jit_code_cache_cap_bytes();
    // Whatever the env says, a sane cap is either a real byte bound or the
    // explicit "disabled" sentinel — never an accidental 0 that would refuse
    // all compilation.
    assert!(cap > 0, "cap must never be zero");
    // The compiled-in default is a generous, finite bound.
    assert_eq!(DEFAULT_JIT_CODE_CACHE_CAP_BYTES, 256 * 1024 * 1024);
}

#[test]
fn code_cache_not_at_capacity_when_committed_is_small() {
    // In the test process essentially no JIT code is committed, so with any
    // realistic cap (or the disabled sentinel) we must be under capacity and
    // therefore still willing to compile.
    let cap = jit_code_cache_cap_bytes();
    if cap == usize::MAX {
        // Cap disabled in this environment: at-capacity is unconditionally false.
        assert!(!jit_code_cache_at_capacity());
        return;
    }
    let committed = COMMITTED_JIT_CODE_BYTES.load(std::sync::atomic::Ordering::Relaxed);
    // Sanity: the test process hasn't committed a quarter-gig of code.
    assert!(committed < cap, "unexpectedly large committed code in test");
    assert!(!jit_code_cache_at_capacity());
}

#[test]
fn code_cache_committed_counter_tracks_buffer_allocation() {
    // A before/after DELTA is not a sound observable here: the counter is
    // process-global and every other test that drops an `ExecutableBuffer`
    // decrements it, so `after` can legitimately sit below `before + 4096`.
    // What does hold at every instant is that the ledger is exact — `new`
    // adds `capacity`, `Drop` subtracts it — so while our buffer is live
    // the total cannot be below our own contribution. Same reasoning as
    // `test_clear_all_releases_committed_executable_bytes`.
    let buf = ExecutableBuffer::new(4096).expect("alloc failed");
    let after = COMMITTED_JIT_CODE_BYTES.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        after >= 4096,
        "committed ledger {after} is below this test's own live mapping (4096 bytes)"
    );
    // Drop now returns the executable mapping. Exact post-drop accounting
    // is covered by the reclamation tests because this suite runs other
    // buffer-allocation tests concurrently.
    drop(buf);
}

#[test]
fn code_cache_cap_refusals_accessor_is_monotonic() {
    // The accessor reads the global refusal counter; it never decreases.
    let a = jit_code_cache_cap_refusals();
    let b = jit_code_cache_cap_refusals();
    assert!(b >= a);
}
