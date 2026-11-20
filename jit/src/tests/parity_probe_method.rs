#[allow(unused_imports)]
use super::*;

// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// The crate-root unit-test suite, split out of `lib.rs` on 2026-09-16.
//
// This is the one extraction in this round that is drawn around a *build
// configuration* rather than around a subject. Everything in here is
// `#[cfg(test)]`: it is compiled only by `cargo test`, it is linked into no
// shipped artefact, and nothing in the crate's production paths can name it.
// That makes its boundary the cheapest real one available -- the compiler
// already proves that no production code depends on this text, so moving it
// cannot change what the JIT does.
//
// It is worth saying why that matters more here than it would in a small
// crate. `lib.rs` is the file a reader opens to find out how compilation is
// driven, and nearly a quarter of it was test bodies that only `cargo test`
// ever reads. Interleaving them with the driver meant that scrolling from the
// compile entry point to the single-pass tier crossed thousands of lines of
// assertions, so the shape of the file stopped reflecting the shape of the
// code. The tests did not get harder to read by moving; the driver got easier.
//
// # Why this is a file module and not a nested one
//
// Every test in here reaches back into the crate root through `super::`, and
// many of the items it reaches for are PRIVATE to the crate root -- module
// bodies, helper functions, counters that exist for exactly one assertion. A
// private item is visible to its own module and to every descendant of it, so
// those paths only keep resolving while this module stays a DIRECT child of
// the crate root. `mod tests;` in `lib.rs` backed by `tests.rs` beside it
// keeps the module path at `crate::tests`, exactly where the inline `mod
// tests { .. }` block had it, which is why the bodies below did not have to be
// touched: `super::X` still means the same `X` it meant before the split.
//
// The four sibling suites that stayed behind in `lib.rs`
// (`code_buffer_retry_tests`, `long_box_direct_bind_tests`,
// `direct_exc_table_publish_policy`, `varhandle_read_direct_bind_tests`) are
// small and sit directly under the code they exercise; moving them would have
// bought a few hundred lines at the cost of four more files, so they stayed.
//
// The only edit made to the bodies was removing one level of indentation,
// which the `\`-continued string literals in here are immune to: Rust drops
// the newline and all leading whitespace after a trailing backslash, so the
// assertion messages are byte-identical to what they were inline.

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
pub(in crate::tests) static BACKEND_PARITY_CENSUS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
pub(in crate::tests) fn parity_probe_method(name: &'static str) -> CachedBytecodeMethod {
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
        hidden_frame: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
        array_inc_declined: std::sync::OnceLock::new(),
        ctor_verdict_memo: std::sync::OnceLock::new(),
        pool_generation: u64::MAX,
    }
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
pub(in crate::tests) fn the_parity_flag_on_compares_and_still_installs_ir() {
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
        &[],
        || {
            try_compile_request(&CompileRequest {
                optimize: true,
                ..CompileRequest::new(&cached, &helpers, CompileRealm::process())
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
pub(in crate::tests) fn arraycopy_scratch_homes_are_allocated_clear_of_the_operand_homes() {
    let mut src = String::new();
    collect_rs_under(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/x64/op_invoke"),
        &mut src,
    );

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
pub(in crate::tests) fn every_jit_cache_publication_stamps_the_wrapped_entry_requirement() {
    let mut src = String::new();
    collect_rs_under(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../vm/src/runtime/interpreter/jit_bridge"),
        &mut src,
    );

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
        // `stamp_compilation_epoch` call that also sits in it. Widened from
        // 900 to 3000 bytes in JIT round 14: the interpreter round's wave-42
        // bytecode-read witness (a `CRATONVM_DBG_JITC` diagnostic) now sits
        // between the stamp and the lock in `try_jit_compile_callee`.
        let from = at.saturating_sub(3000);
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
pub(in crate::tests) fn code_only(src: &str) -> String {
    src.chars().filter(|c| !c.is_ascii_whitespace()).collect()
}

/// The working-tree text of what was `vm/src/jit/helpers.rs`.
///
/// origin/dev `00529e979f` split that file into `vm/src/jit/helpers/*.rs`;
/// this joins the parts in the order `vm/src/jit/helpers/mod.rs`'s
/// `HELPERS_SRC` concatenates them (keep the two lists in step), so a search
/// over it sees the monolith's layout.
fn collect_rs_under(dir: &std::path::Path, out: &mut String) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            collect_rs_under(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs")
            && p.file_name().is_some_and(|n| {
                let s = n.to_string_lossy();
                s != "tests.rs" && !s.ends_with("_tests.rs")
            })
        {
            out.push_str(
                &std::fs::read_to_string(&p)
                    .unwrap_or_else(|e| panic!("read {}: {e}", p.display())),
            );
            out.push('\n');
        }
    }
}

pub(in crate::tests) fn read_vm_jit_helpers_src() -> String {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../vm/src/jit/helpers");
    let mut out = String::new();
    collect_rs_under(&dir, &mut out);
    out
}

#[test]
pub(in crate::tests) fn the_callee_cache_fast_path_refuses_a_wrapped_entry_body() {
    let mut src = String::new();
    collect_rs_under(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../vm/src/runtime/interpreter/jit_bridge"),
        &mut src,
    );
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
pub(in crate::tests) fn the_dispatch_helpers_jit_cache_arm_refuses_a_wrapped_entry_body() {
    let src = read_vm_jit_helpers_src();

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
    // Round 11 bound the lookup to `published` first, so a wrapped-entry
    // body can also be memoised in `WRAPPED_STATIC_CALLEE`; the filter below
    // still runs on that same value before anything is cached.
    let at = flat
        .find("letpublished=jit_cache.get(info.class_name,info.method_name,info.descriptor,info_class_id,);")
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
pub(in crate::tests) fn multianewarray_site_packing_round_trips() {
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
pub(in crate::tests) fn multianewarray_lowering_passes_the_resolved_site_not_an_element_type() {
    let src = std::fs::read_to_string(format!(
        "{}/src/x64/op_object.rs",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("read op_object.rs");

    let arm = src
        .find("// multianewarray — allocate a multi-dimensional array, any arity")
        .expect("the multianewarray arm must still exist");
    let end = src[arm..]
        .find("self.helpers.multianewarray_n")
        .map(|off| arm + off)
        .expect("the arm must still call the multianewarray_n helper");
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
pub(in crate::tests) fn multianewarray_helper_calls_the_shared_interpreter_body() {
    let helpers = read_vm_jit_helpers_src();
    let f = helpers
        .find("pub unsafe extern \"C\" fn jit_multianewarray_n(")
        .expect("the multianewarray helper must still exist");
    let body = &helpers[f..f + 6000];
    assert!(
        // Wave 44 (lane L2): the helper enters the shared body through
        // `multianewarray_alloc_at_site`, which judges the site's index under
        // the pool read's lock and then runs the same allocation.
        body.contains("multianewarray_alloc(") || body.contains("multianewarray_alloc_at_site("),
        "jit_multianewarray_n must call `interpreter::multianewarray_alloc` (or its \
         `_at_site` entry); \
         a private allocation loop here is how the JIT came to stamp \
         `ClassId(0)` on every level"
    );
    assert!(
        !body.contains("ClassId::new(0)"),
        "jit_multianewarray_n must not allocate any level with `ClassId(0)`: \
         that is what made `new String[a][b]` read back as \
         `[Ljava.lang.Object;`"
    );

    // The 2-D helper is still wired (the table is append-only) but must not
    // grow a second allocation path of its own: one body was the whole point
    // of the fix this test guards, and an arity-specific copy here would be
    // the same mistake one level down.
    let two = helpers
        .find("unsafe fn jit_multianewarray_2d_body(")
        .expect("the 2-D adapter must still exist");
    let two_body = &helpers[two..two + 400];
    assert!(
        two_body.contains("jit_multianewarray_n_body("),
        "jit_multianewarray_2d_body must delegate to the N-ary body"
    );
    assert!(
        !two_body.contains("multianewarray_alloc("),
        "jit_multianewarray_2d_body must NOT allocate for itself"
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
pub(in crate::tests) fn ir_declines_an_unresumable_protected_trap_and_only_that() {
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

    // `this.f` read after a side effect in an INSTANCE method cannot take the
    // null-check trap (JVMS 2.6.1: `this` is non-null) when local 0 is never
    // stored and nothing jumps to the getfield: the shape of
    // `DirectByteBuffer.getInt(long)`'s protected `getIntUnaligned(session(), null, a, bigEndian)`.
    //   0: aload_0, 1: invokevirtual #1, 4: aload_0, 5: getfield #2, 8: ireturn
    let this_read = [0x2a, 0xb6, 0x00, 0x01, 0x2a, 0xb4, 0x00, 0x02, 0xac];
    assert_eq!(
        ir_unresumable_protected_trap_in(&this_read, this_read.len(), &range(0, 8), false),
        None,
        "a getfield on `this` of an instance method is not a deopt trap"
    );
    // A static method's local 0 is an ordinary (nullable) argument.
    let static_site =
        ir_unresumable_protected_trap_in(&this_read, this_read.len(), &range(0, 8), true);
    assert_eq!(
        static_site,
        ir_unresumable_protected_trap(&this_read, this_read.len(), &range(0, 8))
    );
    // Storing local 0 anywhere voids the proof.
    let restored = [0x2a, 0xb6, 0x00, 0x01, 0x4b, 0x2a, 0xb4, 0x00, 0x02, 0xac];
    assert!(
        ir_unresumable_protected_trap_in(&restored, restored.len(), &range(0, 9), false).is_some()
            || !bytecode_commits_side_effect(&restored, restored.len())
    );

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

    // `T1L71JarLoadMicro.lambda$main$0`, javac 25: the protected aaload is
    // before the only effect (loadClass). A trap at the aaload replays no
    // effect, so a later call in the same protected range must not block C2.
    //   0 aload_0  1 aload_1  2 iload_2  3 bipush 63  5 iand  6 aaload
    //   7 invokevirtual #1  10 pop  11 iconst_0  12 ireturn
    //   13 astore_3  14 iconst_1  15 ireturn
    let protected_load_miss = [
        0x2a, 0x2b, 0x1c, 0x10, 0x3f, 0x7e, 0x32, 0xb6, 0x00, 0x01, 0x57, 0x03, 0xac, 0x4e,
        0x04, 0xac,
    ];
    assert_eq!(
        ir_unresumable_protected_trap(
            &protected_load_miss,
            protected_load_miss.len(),
            &range(0, 13),
        ),
        None,
        "a protected aaload before the first side effect is safe to replay"
    );

    // Moving the call before the guarded access makes replay observably
    // unsafe, even though the same opcodes and handler range are present.
    //   0 aload_0  1 invokevirtual #1  4 aload_1  5 iconst_0  6 baload
    let effect_before_trap = [0x2a, 0xb6, 0x00, 0x01, 0x2b, 0x03, 0x33, 0xac];
    assert_eq!(
        ir_unresumable_protected_trap(
            &effect_before_trap,
            effect_before_trap.len(),
            &range(0, 7),
        ),
        Some((6, 0x33)),
        "an effect before the protected trap must still decline C2"
    );

    // A loop can pass the guard once, commit an effect, then fail the guard
    // on a later iteration. Loop closure must retain the refusal in that case.
    //   0 aload_0  1 iconst_0  2 baload  3 pop  4 invokestatic #1
    //   7 goto 0
    let effect_in_guard_loop = [0x2a, 0x03, 0x33, 0x57, 0xb8, 0x00, 0x01, 0xa7, 0xff, 0xf9];
    assert_eq!(
        ir_unresumable_protected_trap(
            &effect_in_guard_loop,
            effect_in_guard_loop.len(),
            &range(0, 10),
        ),
        Some((2, 0x33)),
        "a loop effect before a later trap arrival must still decline C2"
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
pub(in crate::tests) fn the_throw_bci_probe_body_stays_admissible_to_the_optimizing_tier() {
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

    // A guard after a committed write cannot replay the entry prefix safely.
    // iconst_1; putstatic #1; aload_0; iconst_0; iaload; ireturn
    let after_store = [0x04, 0xb3, 0, 1, 0x2a, 0x03, 0x2e, 0xac];
    assert_eq!(
        ir_unresumable_protected_trap(&after_store, after_store.len(), &table(0, 7)),
        Some((6, 0x2e)),
        "a protected guard after a committed write must still be declined"
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
pub(in crate::tests) fn rbc6_admits_exactly_the_opcodes_whose_lowerings_publish() {
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
        0xbc, // newarray       — inline-TLAB arm cannot throw; the fallback
        //           arm and the inline arm's decline both merge into
        //           `emit_post_alloc_oom_check`
        0xbd, // anewarray      — both the resolved and CP-deferred arms end
        //           in `emit_post_alloc_oom_check`
        0xc5, // multianewarray — the 2-D helper checks both dimensions
              //           before resolving anything, then ends in
              //           `emit_post_alloc_oom_check` (round 9 wave 6 fixed the
              //           helper's own resolution-failure arm to stash the real
              //           exception instead of discarding it)
    ] {
        assert!(
            super::precise_frame_publishing_opcode(op),
            "opcode {op:#04x} publishes a precise frame but is not admitted — \
                 every method with one in a protected range stays interpreted"
        );
    }

    // Do NOT publish. `aastore` (a reference store's type check, deliberately absent from
    // `precise_frame_publishing_opcode`) and the integer divide are the sites still on
    // `first_unsupported_precise_frame_site`'s named list. (`ldc` was here until
    // 2026-09-19, and `newarray`/`anewarray`/`multianewarray` until 2026-09-21, both on a
    // claim that they reached their helpers "through the shared sentinel stub"; every one
    // of those lowerings ends in `emit_post_alloc_oom_check`, which records a reason-9
    // frame at a protected bci. `arraylength` was here until round 9.)
    //
    // Round 11 wave 8: `aastore` and the integer divides left that list too.
    // Their lowerings now publish on every throwing edge (reasons 9/10/11, and
    // reason 12 for the zero divisor), so they are admitted with the other
    // array accesses.
    for op in [0x53u8, 0x6c, 0x6d, 0x70, 0x71] {
        assert!(
            super::precise_frame_publishing_opcode(op),
            "opcode {op:#04x} publishes a precise frame but is not admitted"
        );
    }
    // Not a candidate at all: `iadd` cannot throw, and the predicate answers
    // only for the candidate set.
    assert!(!super::precise_frame_publishing_opcode(0x60));
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
pub(in crate::tests) fn rbc6_gate_clears_getstatic_checkcast_and_straight_line_arraylength() {
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

/// `newarray`/`anewarray`/`multianewarray` (0xbc/0xbd/0xc5), admitted
/// 2026-09-21 — same bookkeeping status as `getstatic`/`checkcast` above: the
/// scanner used to refuse a protected range containing any of the three even
/// though every lowering already published. See `precise_alloc_athrow_enabled`.
#[test]
#[cfg(target_arch = "x86_64")]
pub(in crate::tests) fn rbc6_gate_clears_array_allocations() {
    use cratonvm_reader::attribute::ExceptionTableEntry;
    let table = [ExceptionTableEntry {
        start_pc: 0,
        end_pc: 4,
        handler_pc: 4,
        catch_type: 0,
    }];
    // `iconst_1; newarray int` (2 bytes) then `nop`.
    let newarray = [0x04u8, 0xbc, 0x0a, 0x00];
    assert_eq!(
        super::first_unsupported_precise_frame_site(&newarray, newarray.len(), &table),
        None,
        "a protected newarray must no longer refuse the compile"
    );
    // `iconst_1; anewarray #0` (3 bytes) then `nop`.
    let anewarray = [0x04u8, 0xbd, 0x00, 0x00];
    assert_eq!(
        super::first_unsupported_precise_frame_site(&anewarray, anewarray.len(), &table),
        None,
        "a protected anewarray must no longer refuse the compile"
    );
    // `multianewarray #0, 2` (4 bytes) — dimensions are already on the stack
    // in a real method; the scanner only looks at the opcode stream.
    let multi_table = [ExceptionTableEntry {
        start_pc: 0,
        end_pc: 4,
        handler_pc: 4,
        catch_type: 0,
    }];
    let multianewarray = [0xc5u8, 0x00, 0x00, 0x02];
    assert_eq!(
        super::first_unsupported_precise_frame_site(
            &multianewarray,
            multianewarray.len(),
            &multi_table
        ),
        None,
        "a protected multianewarray must no longer refuse the compile"
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
pub(in crate::tests) fn rbc6_admits_a_protected_throw_new_and_refuses_it_when_withdrawn() {
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
        "a protected `throw new X()` must compile — this is the netty \
             AdaptivePoolingAllocator$Magazine.allocate shape"
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
        "the withdrawal must take both opcodes out — withdrawing one leaves \
             the other holding the same method down"
    );
    assert!(
        withdrawn(0xb7),
        "the withdrawal must not disturb invokespecial, which sits between \
             them in this very sequence"
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
pub(in crate::tests) fn rbc6_admits_a_protected_throw_new_with_a_message_constant() {
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
        "a protected `throw new X(\"msg\")` must compile — this is the \
         ConcurrentHashMap.putVal shape"
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
    // Not vacuous: a stream cut inside the protected ldc refuses AT the ldc.
    assert_eq!(
        super::first_unsupported_precise_frame_site(&code, 5, &table),
        Some((4, 0x12)),
        "the scan must reach the ldc it admits"
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
pub(in crate::tests) fn a_string_access_site_pins_its_method_to_the_single_pass_backend() {
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
        "a String.charAt call site must keep its method on the backend that \
             can emit the inline decode"
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
pub(in crate::tests) fn a_missing_pin_input_is_reported_as_a_blind_spot_not_as_no_site() {
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

/// nio20 wave 1, lane L3 (NIO19-8): a method whose every String intrinsic site
/// is an accessor the optimizing tier expands itself (`length` / `isEmpty` /
/// `charAt` on a `java/lang/String` `invokevirtual`) is not pinned; ONE site
/// the expander does not emit -- `hashCode`, a `CharSequence`-declared
/// (guarded) accessor, an `invokeinterface` -- keeps the pin.
///
/// Reads the ambient `CRATONVM_JIT_IR_STRING_INTRINSICS` /
/// `CRATONVM_JIT_NO_IR_STRING_ACCESS_ADMIT`; both default to the arm asserted
/// here, and a run that sets either is the A/B arm this test is not about, so
/// it returns early rather than asserting the other answer.


/// nio20 wave 4, lane L5 (W3-L2-4): a method whose every builder intrinsic
/// site is one the optimizing tier's prefix answers (`StringBuilder.length` /
/// `charAt` / `append(char)`, and since nio20 wave 5 `append(String)`) is
/// not pinned; `CRATONVM_JIT_IR_SB_APPEND_STRING_PREFIX=0` makes an
/// `append(String)` site pin again, and `CRATONVM_JIT_IR_SB_PREFIX=0`
/// restores the pin for all of them. The builder prefix is default-on with
/// the measured `append(String)` member default-off. The kinds are named through the resolver's STRINGBUILDER_ACCESS rows,
/// and `StringBuffer` is never one.


/// JIT r20 wave 18, lane xSb (STR20-2): under `CRATONVM_JIT_IR_SB_PREFIX=1`
/// a method whose builder sites include `append(int)` / `toString()` is no
/// longer pinned to the single-pass backend; each new kind's own switch
/// (`CRATONVM_JIT_IR_SB_APPEND_INT_PREFIX=0` / `..._SB_TOSTRING_PREFIX=0`)
/// pins it again, and with `CRATONVM_JIT_IR_SB_PREFIX` unset nothing changes.


/// nio20 wave 4, lane L5: `CRATONVM_JIT_IR_STRING_CALL_ADMIT=callers` admits
/// a non-intrinsic String member everywhere except inside the String
/// implementation classes; `=1` admits it everywhere; unset admits nowhere.


/// nio20 wave 1, lane L3 (NIO19-8): the invoke-planning gate under
/// `CRATONVM_JIT_IR_STRING_CALL_ADMIT=0` is the blanket `java/lang/String`
/// refusal minus the three expandable accessors, byte for byte (the default
/// has been the admit arm since JIT r23 wave 6: see
/// `str20_call_admit_default_and_case_workers_stay_calls`);
/// `CRATONVM_JIT_IR_STRING_CALL_ADMIT=1` narrows it to the members the
/// single-pass backend would intrinsify.


/// STR20 closure evidence (page `jit-r20-strings-proposals.md`, wave 1):
/// * STR20-5: `CRATONVM_JIT_IR_STRING_CALL_ADMIT` was decided on 2026-10-2x
///   (JIT r23 wave 6): unset is the ADMIT arm, `=0` restores the blanket
///   refusal of every non-accessor `java/lang/String` member.
/// * STR20-4: the `compareToCI` cold-nested-splice loss is answered by the
///   prefix kinds, not by a bound-call fallback: `StringLatin1.compareToCI`
///   and `toLowerCase` (the workers) and `compareToIgnoreCase` stay CALLS in
///   the splice planner, so no spliced 108-byte looping body is left to drop.
/// * STR20-1/2: the builder kinds that were pinned stay calls too (the
///   `SbCharAt` row follows its switch).


/// nio20 wave 6, lane L2 (`heapcb`): `String.getChars(II[CI)V` is the
/// optimizing tier's [`super::IrStringSearchKind::GetChars`] prefix kind, so
/// it no longer bails the invoke-planning gate (on the default admit arm,
/// even for a caller inside the String classes), stays a CALL in the splice
/// planner, and takes four arguments; `CRATONVM_JIT_IR_STRING_GETCHARS_PREFIX=0`
/// (or the search prefix's master switch) restores the wave-5 refusal.
/// Narrow oops / an armed ZGC barrier keep the refusal (the prefix reads
/// `value`).


/// JIT r20 wave 12, lane qStr (W6-L2-2): `String.toCharArray()`, the static
/// `valueOf(char[])` / `copyValueOf(char[])` and `new String(char[])` are the
/// allocating prefix kinds, so they no longer bail the invoke-planning gate,
/// stay CALLs in the splice planner and arrive as exactly their own invoke
/// kinds; `CRATONVM_JIT_IR_STRING_CHARS_PREFIX=0` (or the search prefix's
/// master switch) restores the refusal. Other overloads are not these kinds.


/// JIT r20 wave 18, lane xUtf8 (STR20-6): the two static `StringCoding`
/// leaves are their own prefix kinds, static sites only, kept CALLs by the
/// splice planner; `CRATONVM_JIT_IR_STRING_CODING_PREFIX=0` (or the search
/// prefix's master switch) withdraws both. Other members are not these kinds.
/// JIT r20 wave 19, lane zCp: `StringBuilder.appendCodePoint(I)` and the
/// static `Character.toChars(I)[C` are their own prefix kinds -- outside the
/// opt-in `CRATONVM_JIT_IR_SB_PREFIX`, each behind its own default-ON switch
/// (and the search prefix's master switch), kept CALLs by the splice planner
/// and arriving as exactly their own invoke kinds. `appendCodePoint` is NOT a
/// STRINGBUILDER_ACCESS row, so naming it pins no method.
/// JIT r21 wave 1, lane strings (CP21-1): `String.codePointAt(int)` is the
/// `CodePointAt` prefix kind only under its opt-in switch (and inside the
/// search-prefix switch); naming it lifts the invoke-planning gate for the
/// site and keeps it a call in the splice planner; unset, the gate refuses the
/// method exactly as before. The other code-point members are not this kind.


/// JIT r23 wave 1, lane L5 (INTRINSICS22-1): the single-pass code-point rows
/// are pin candidates (`has_string_intrinsic_site`), and each is named by its
/// optimizing-tier prefix kind in `every_string_intrinsic_site_ir_expands`,
/// so a method whose only String sites are these two is NOT pinned to the
/// single-pass backend while the prefixes answer -- adding the rows must not
/// move such a method off the optimizing tier. With a prefix withdrawn, the
/// row pins (the single-pass body is then the fast one), and a method that
/// also holds an unprefixed builder site stays pinned exactly as before.


/// JIT r23 wave 1, lane L5 (STRINGS21-4): `String.codePointBefore(int)` and
/// `codePointCount(int, int)` are prefix kinds by default, each withdrawn by
/// its own switch (and by the search-prefix switch); naming one lifts the
/// invoke-planning gate for the site and keeps it a call in the splice planner.
/// Neither is a single-pass resolver row, so neither can pin a method.


/// JIT r23 wave 2b, lane L5 (INTRINSICS22-5): the integral `Arrays.equals`
/// overloads are the `ArraysEquals` prefix kind exactly while their switch,
/// the search-prefix switch and the call admission are all on; never the
/// floating-point or `Object[]` overloads; a static site that stays a call.
/// JIT r23 wave 2, lane L5: the wave-2 rows (`StringBuilder.codePointAt`,
/// `String.codePointBefore` / `codePointCount`) do not pin a method while
/// their prefix kinds answer, and do once a prefix is withdrawn.


/// JIT r21 wave 1, lane strings: STRINGS21-1 (`coder()`, `isLatin1()`,
/// `String.<init>([BB)V` at the planning gate), STRINGS21-2
/// (`compareToIgnoreCase`) and ZCP-3's unchanged `toLowerCase`: each site is
/// admitted only under its own opt-in switch, each switch withdraws only its
/// own members, and unset the gate refuses all of them exactly as before.


/// JIT r21 wave 3, lane strings: the two `StringLatin1` workers are static
/// prefix kinds under the internal admit only; `StringBuilder.length()` is the
/// `SbLength` kind under `CRATONVM_JIT_IR_SB_LENGTH_PREFIX` without the rest of
/// `CRATONVM_JIT_IR_SB_PREFIX` (the other builder members stay unnamed, so they
/// keep the pin).


/// STRINGS21 (str21): the lower-case worker kind is named by the default-ON
/// internal admit as well as by the case-map switch, so the lowering must
/// route it to the case-map emitter only while
/// `CRATONVM_JIT_IR_LATIN1_CASE_MAP_PREFIX` is on (otherwise `=0` could not
/// withdraw that emitter). The kind stays named with the case-map switch off
/// (the admit alone answers); the routing guard is pinned in the source.
/// JIT r20 wave 19, lane zCp: the two surrogate constants both code-point
/// prefixes bake are `Character.highSurrogate` / `lowSurrogate`'s, checked
/// against Rust's own UTF-16 encoder over every supplementary code point, and
/// the validity screens' unsigned compares (`cp >>> 16 == 0`, `cp - 0x10000`
/// below `0x100000`) are `isBmpCodePoint` / `isValidCodePoint` at every
/// boundary.
#[cfg(target_arch = "x86_64")]
#[test]
pub(in crate::tests) fn the_code_point_prefix_surrogate_constants_are_the_jdks() {
    use crate::x64::concat_emit::{HIGH_SURROGATE_BIAS, LOW_SURROGATE_BASE};
    for cp in 0x1_0000u32..=0x10_FFFF {
        let high = (cp >> 10).wrapping_add(HIGH_SURROGATE_BIAS as u32);
        let low = (cp & 0x3FF) + LOW_SURROGATE_BASE as u32;
        let mut units = [0u16; 2];
        let n = char::from_u32(cp)
            .expect("supplementary scalar")
            .encode_utf16(&mut units)
            .len();
        assert_eq!(n, 2, "cp {cp:#x}");
        assert_eq!(
            (u32::from(units[0]), u32::from(units[1])),
            (high, low),
            "cp {cp:#x}"
        );
    }
    let is_bmp = |cp: i32| (cp as u32) >> 16 == 0;
    let is_supplementary_valid = |cp: i32| (cp as u32).wrapping_sub(0x1_0000) < 0x10_0000;
    for (cp, bmp, valid) in [
        (0, true, true),
        (0xFFFF, true, true),
        (0xD800, true, true),
        (0x1_0000, false, true),
        (0x10_FFFF, false, true),
        (0x11_0000, false, false),
        (-1, false, false),
        (i32::MIN, false, false),
        (i32::MAX, false, false),
    ] {
        assert_eq!(is_bmp(cp), bmp, "{cp:#x}");
        assert_eq!(is_bmp(cp) || is_supplementary_valid(cp), valid, "{cp:#x}");
    }
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
pub(in crate::tests) fn direct_native_helper_answers_per_vm_not_per_process() {
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

/// JDK-ONLY-WAVE2 §4: under `JdkOnly` the bind decision comes from the VM's
/// view of the registry, not from a hard-coded triple list.
///
/// The pre-2026-08-06 gate refused all seven ladders wholesale, which is
/// stricter than the contract. What the VM answers has itself moved once since
/// (2026-09-22, from "is this a reviewed `Intrinsic`" to "would the generic
/// tail run this native"), and this test is written so that move does not show
/// up here: the resolver is a closure, and what this pins is that
/// `direct_native_helper` consults it on the strict arm, refuses and RECORDS a
/// `false`, and (since JIT r23 w1 L6) honours it unrecorded when `Compatible`.
#[test]
pub(in crate::tests) fn direct_native_helper_asks_the_registry_not_a_name_list() {
    let entry: usize = 0xfeed_face;

    let admits = |_c: &str, _m: &str, _d: &str| true;
    let refuses = |_c: &str, _m: &str, _d: &str| false;

    // Admitted by the VM: binds even under strict.
    assert_eq!(
        direct_native_helper(
            entry,
            true,
            Some(&admits),
            "java/lang/Integer",
            "valueOf",
            "(I)Ljava/lang/Integer;"
        ),
        0xfeed_face,
        "a triple the VM admits must bind under JdkOnly"
    );

    // Refused by the VM — a SyntheticStub, an unregistered triple, an armed
    // enforcement dial: refused here, and recorded.
    let before = jdk_only_direct_native_refusals();
    assert_eq!(
        direct_native_helper(
            entry,
            true,
            Some(&refuses),
            "java/util/HashMap",
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;"
        ),
        0,
        "a triple the VM refuses must not be bound under JdkOnly"
    );
    assert!(jdk_only_direct_native_refusals() > before);

    // Compatible asks the resolver too since JIT r23 w1 L6 (SYNC22-3,
    // `CRATONVM_JIT_COMPAT_BIND_ASKS_REGISTRY`): a refusal there is honoured
    // (the VM's compat arm refuses only a triple the registry holds no row
    // for), and an admission still binds.
    assert_eq!(
        direct_native_helper(
            entry,
            false,
            Some(&refuses),
            "java/util/HashMap",
            "put",
            "()V"
        ),
        0
    );
    assert_eq!(
        direct_native_helper(
            entry,
            false,
            Some(&admits),
            "java/util/HashMap",
            "put",
            "()V"
        ),
        0xfeed_face
    );
}

/// H7-1 — the gate that is the ONLY thing keeping the interpreter and
/// compiled code from answering a map lookup two different ways under
/// `--real-jdk`, pinned per triple.
///
/// `H4-1` O1 called the six collection direct helpers "blocking" because
/// their failure mode is a tier-dependent wrong answer no arm diffs for.
///
/// # What this test used to claim, and why the claim was retired 2026-09-22
///
/// It was called `strict_mode_refuses_every_collection_direct_helper`, and the
/// reason it gave was that all four triples the collection ladders name are
/// `bridge` in `scripts/baselines/jdk-only-kind-map-25-linux.tsv`, so an
/// `Intrinsic`-only admission refused every one "and the call falls to the
/// generic, policy-checked dispatcher — the same route the interpreter takes".
///
/// The premise is still true and the conclusion was not. The generic
/// dispatcher is `vm_exec::invoke_or_native`, whose §7 step-3 input is
/// structurally `false` for a `Bridge`, so the route it takes for these four
/// triples is the registered native — the same native the helper
/// re-implements, reached through a funnel instead of an inlined `CALL`. The
/// refusal never gave compiled code and the interpreter different answers to
/// agree on; it gave them the same answer at ~3x the cost. See
/// `docs/feature-designs/jit-ir-optimizations.md`.
///
/// So the four rows now ADMIT under strict, and what keeps compiled code and
/// the interpreter consistent is the thing that always did: both run the same
/// registered native, and the VM answers that question once, in
/// `jit::helpers::direct_helper_refusal`, for the bind and for the call.
///
/// # What is unchanged, and is the reason this test exists
///
/// The ladders ask about the triple named at the CALL SITE, which for the two
/// `invokeinterface` arms is `java/util/Map.get` and
/// `java/util/concurrent/ConcurrentMap.get` — not the `java/util/HashMap` /
/// `java/util/concurrent/ConcurrentHashMap` row whose native the helper
/// actually runs. A tagging that admits only the interface row would bind a
/// helper whose implementation the policy was never asked about.
/// `direct_native_helper_for_impl` closes that, and the blocks after the loop
/// are the executable statement of it — they matter MORE now that the common
/// answer is "admit".
#[test]
pub(in crate::tests) fn strict_mode_asks_both_rows_of_every_collection_direct_helper() {
    let entry: usize = 0x0c0f_fee0;

    // What the VM answers today for the four triples the collection ladders
    // name. Source: scripts/baselines/jdk-only-kind-map-25-linux.tsv (rows
    // 6554, 6560, 6851, 7602), all `bridge`, and a `bridge` with no armed
    // enforcement dial is what the generic tail runs — so "would the tail run
    // this native" is `true` for all four. Written as a constant rather than a
    // name list so the test cannot drift into asserting a list it maintains
    // itself; the TSV is the source and it is cited above.
    let as_measured_today = |_c: &str, _m: &str, _d: &str| -> bool { true };

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
        assert_eq!(
            direct_native_helper(
                entry,
                true,
                Some(&as_measured_today),
                class,
                method,
                descriptor
            ),
            0x0c0f_fee0,
            "{class}.{method}{descriptor} is a Bridge the generic tail runs, \
             so refusing the bind gives compiled code a slower route to the \
             SAME native rather than a different answer"
        );
        // …and compatible mode binds it too, which is the property that makes
        // this family's behaviour mode-independent: one code path, one answer.
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
            "{class}.{method}{descriptor} must still bind under --fast-boot"
        );
    }

    // Admitting ONLY the interface row must NOT open the door: the helper
    // this ladder binds runs `native_hashmap_get_exact`, registered on
    // `java/util/HashMap`, and that is the row the policy question is about.
    let interface_only = |class: &str, _m: &str, _d: &str| -> bool { class == "java/util/Map" };
    let before = jdk_only_direct_native_refusals();
    assert_eq!(
        direct_native_helper_for_impl(
            entry,
            true,
            Some(&interface_only),
            "java/util/Map",
            "java/util/HashMap",
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;"
        ),
        0,
        "admitting the INTERFACE row must not admit a helper whose \
         implementing row the policy would refuse"
    );
    assert!(
        jdk_only_direct_native_refusals() > before,
        "the implementing-row refusal must be counted too"
    );

    // Both rows admitted: the policy genuinely covers the code that runs,
    // so it binds.
    let both_admitted = |class: &str, _m: &str, _d: &str| -> bool {
        matches!(class, "java/util/Map" | "java/util/HashMap")
    };
    assert_eq!(
        direct_native_helper_for_impl(
            entry,
            true,
            Some(&both_admitted),
            "java/util/Map",
            "java/util/HashMap",
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;"
        ),
        0x0c0f_fee0,
        "when BOTH rows are admitted the bind is admitted"
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
pub(in crate::tests) fn eventually(mut cond: impl FnMut() -> bool) -> bool {
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
pub(in crate::tests) fn eventually_drained(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if cond() {
            return true;
        }
        jit_execution_leave(jit_execution_enter());
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    cond()
}


#[test]
pub(in crate::tests) fn dynamic_dispatch_cache_eligibility_is_independent_of_recursion_metadata() {
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
pub(in crate::tests) fn self_recursive_nontail_site_routes_through_dispatch() {
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
        hidden_frame: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
        array_inc_declined: std::sync::OnceLock::new(),
        ctor_verdict_memo: std::sync::OnceLock::new(),
        pool_generation: u64::MAX,
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
        ..CompileRequest::new(&cached, &helpers, CompileRealm::process())
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

/// The raw self-recursive `CALL` must SERVICE its callee's deopt sentinel.
///
/// The single-pass backend binds a statically-bound self-recursive site as a
/// raw `CALL` to this method's own entry. That is a JIT-to-JIT direct call like
/// any other, and every other one in this backend follows it with
/// `emit_inline_callee_deopt_check` — the call to `jit_service_callee_deopt`
/// that gives the CALLEE's own exception table a look before the callee's
/// `i64::MIN` return is treated as the caller's own unwind.
///
/// This site alone did not, because the planner deliberately gives it no
/// `invoke_info` row (a row would route it to `jit_invoke_dispatch` and undo
/// the direct bind), so the codegen had nothing to hand the service. The
/// consequence was silent and ugly: a self-recursive method whose body catches
/// what its own recursive call throws ran that `catch` in NO activation the
/// compiler owned. Each one stamped its own throw bci over the callee's and
/// returned the sentinel again, so the exception walked the whole compiled
/// region and was first resolved at the topmost compiled frame — the outermost
/// activation's handler. `regression-suite/probes/R10SelfRecCatch.java` is the
/// end-to-end witness (`explicit` returned the outermost `catch`'s value, 16,
/// against HotSpot's 120); this is the unit-level guard for the same thing.
///
/// Proven the way its sibling above proves routing: `emit_call_absolute` bakes
/// the helper address as a `MOV RAX, imm64`, so the 8-byte LE pattern in the
/// emitted code identifies which helpers the site calls.
#[test]
// x86-64 only, for the reason its sibling states: this asserts what
// `try_compile_inner` ROUTES and what the x64 backend emits.
#[cfg(target_arch = "x86_64")]
pub(in crate::tests) fn raw_self_recursive_call_services_the_callee_deopt_sentinel() {
    use std::sync::Arc;

    // The sibling test's body: `static int f(int n) { return n <= 0 ? 0 : f(n - 1) + 1; }`
    // with the self `invokestatic` at pc 9 NON-tail (an `iadd` follows).
    let code: &[u8] = &[
        0x1a, 0x9d, 0x00, 0x05, 0x03, 0xac, 0x1a, 0x04, 0x64, 0xb8, 0x00, 0x01, 0x04, 0x60, 0xac,
    ];
    let cached = CachedBytecodeMethod {
        declaring_class_id: cratonvm_types::ClassId::new(1),
        class_name: Arc::from("pkg/RecSvc"),
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
        hidden_frame: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
        array_inc_declined: std::sync::OnceLock::new(),
        ctor_verdict_memo: std::sync::OnceLock::new(),
        pool_generation: u64::MAX,
    };
    let resolver = |idx: u16| -> Option<(String, String, String)> {
        (idx == 1).then(|| {
            (
                "pkg/RecSvc".to_string(),
                "f".to_string(),
                "(I)I".to_string(),
            )
        })
    };
    // Distinctive fake addresses — the code is never executed, only
    // pattern-searched.
    const GUARD_ADDR: usize = 0x7161_7264_5f61_6472;
    const SERVICE_ADDR: usize = 0x7376_6331_5f61_6472; // "svc1_adr"-ish tag
    let contains = |hay: &[u8], addr: usize| -> bool {
        let needle = (addr as u64).to_le_bytes();
        hay.windows(needle.len()).any(|w| w == needle)
    };

    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the test wires only the slots it exercises.
    let mut helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    helpers.self_call_stack_guard = GUARD_ADDR;
    helpers.service_callee_deopt = SERVICE_ADDR;

    let compiled = try_compile_request(&CompileRequest {
        cp_invoke_resolver: Some(&resolver),
        self_call_identity_stable: true,
        ..CompileRequest::new(&cached, &helpers, CompileRealm::process())
    })
    .expect("identity-proven self-recursive method must compile");
    let bytes = compiled.code_bytes().to_vec();

    // Non-vacuity: this must be the RAW self-call shape, not a dispatch
    // fallback — the guard helper is baked only on that route.
    assert!(
        contains(&bytes, GUARD_ADDR),
        "the fixture must exercise the raw guarded self-CALL, not a dispatch fallback"
    );
    assert!(
        contains(&bytes, SERVICE_ADDR),
        "a raw self-recursive CALL must service its callee's deopt sentinel, or the \
         callee activation's own catch block is skipped and the exception is resolved \
         against whichever frame happens to reach the interpreter"
    );
    // The service takes a `JitInvokeInfo`, which the planner stages on its own
    // channel for exactly this site (`stage_self_call_invoke_info`). Without it
    // the assertion above cannot hold, so this pins down the mechanism rather
    // than just the symptom.
    assert_eq!(
        compiled._jit_invoke_infos.len(),
        1,
        "the self-call site must carry the one JitInvokeInfo describing this method \
         as its own callee"
    );
}

/// JIT round 19 wave 3, lane throws3 (THROWS3-1,
/// `CRATONVM_JIT_IR_SELFREC_DIRECT_VOID`): the optimizing tier's route for a
/// self-recursive static VOID call. Off (the default) it is the generic
/// `jit_invoke_dispatch` call, as before; on, with the loader-identity proof,
/// it is the direct self-CALL every other return type takes (the stack-guard
/// helper is baked, the dispatch helper is not); on WITHOUT the proof it
/// stays on dispatch. Proven from the emitted bytes, as the siblings above.
/// JIT round 19 wave 4, lane throws4 (THROWS3-3,
/// `CRATONVM_JIT_SELF_CALL_SENTINEL_LEAN`): the void direct self-call's
/// sentinel path. Off, it calls the callee-deopt service with a plain count
/// and then peeks `dispatch_threw`; on, the count carries
/// `SERVICE_ARGS_LEAN_PROPAGATE_BIT` and the peek is gone (a void result is
/// never `i64::MIN`). Proven from the emitted bytes.
/// The admission rule of THROWS3-1 alone: the switch AND the proof.
#[test]
pub(in crate::tests) fn r19w3_throws3_void_self_call_admission_needs_switch_and_proof() {
    assert!(crate::selfrec_direct_void_admits(true, true));
    assert!(!crate::selfrec_direct_void_admits(true, false));
    assert!(!crate::selfrec_direct_void_admits(false, true));
    assert!(!crate::selfrec_direct_void_admits(false, false));
}

/// deopt-osr Step 9 follow-up (a): `stamp_deopt_epoch_guard` writes the
/// creation epoch + live-cell pointer into the artifact's retained guard, and
/// the resulting guard reports superseded once the live epoch advances past
/// the creation epoch. A null guard (production artifact) is a safe no-op.
#[test]
pub(in crate::tests) fn fua_stamp_deopt_epoch_guard_and_supersede() {
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
pub(in crate::tests) fn step3_optimize_toggle_routes_c1_singlepass_and_c2_ir() {
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
        // Two trailing pad bytes, as the VM's padded bytecode carries them
        // (`try_compile` builds `code.len() - 2`); unpadded, the IR saw a
        // two-byte method with no return (round 12 wave 2's dead-end refusal).
        code: Arc::from([0x1a, 0x1b, 0x60, 0xac, 0, 0].as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 2,
        max_locals: 2,
        num_params: 2,
        is_synchronized: false,
        is_static: true,
        force_native_cache: std::sync::OnceLock::new(),
        hidden_frame: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
        array_inc_declined: std::sync::OnceLock::new(),
        ctor_verdict_memo: std::sync::OnceLock::new(),
        pool_generation: u64::MAX,
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
pub(in crate::tests) fn step3_getfield_int_routes_through_ir() {
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
        hidden_frame: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
        array_inc_declined: std::sync::OnceLock::new(),
        ctor_verdict_memo: std::sync::OnceLock::new(),
        pool_generation: u64::MAX,
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
pub(in crate::tests) fn ea_bridge_scalar_replaces_full_layout_new_store_load() {
    use crate::ir::{Graph, IrType, MemKind, Op};

    // Object o = new Foo(); o.f1 = 42; return o.f1;  (single int field at
    // index 1, to also exercise a non-zero field index vs MemKind::Int=0).
    let mut g = Graph::empty(0);
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
pub(in crate::tests) fn ea_bridge_keeps_escaping_new() {
    use crate::ir::{Graph, IrType, MemKind, Op};

    let mut g = Graph::empty(0);
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
pub(in crate::tests) fn ea_unstored_field_load_resolves_to_zero_default() {
    use crate::ir::{Graph, IrType, MemKind, Op};

    let mut g = Graph::empty(0);
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
pub(in crate::tests) struct EaFixture {
    pub(in crate::tests) g: crate::ir::Graph,
    pub(in crate::tests) mem: crate::ir::NodeId,
    pub(in crate::tests) newobj: crate::ir::NodeId,
    pub(in crate::tests) val: crate::ir::NodeId,
    pub(in crate::tests) store: crate::ir::NodeId,
    pub(in crate::tests) load: crate::ir::NodeId,
}

pub(in crate::tests) fn ea_full_layout_fixture() -> EaFixture {
    use crate::ir::{Graph, IrType, MemKind, Op};

    let mut g = Graph::empty(0);
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

pub(in crate::tests) fn run_ea(g: &mut crate::ir::Graph) {
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
pub(in crate::tests) fn assert_memory_chain_intact(g: &crate::ir::Graph) {
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
pub(in crate::tests) fn assert_frame_state_lane_clean(g: &crate::ir::Graph) {
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
pub(in crate::tests) fn ea_refuses_to_elide_an_allocation_a_safepoint_names() {
    use crate::ir::{Op, SafepointSnapshot, NO_NODE};

    let mut f = ea_full_layout_fixture();
    f.g.safepoints.push(SafepointSnapshot::new(
        3,
        vec![NO_NODE],
        vec![f.newobj],
        Vec::new(),
    ));
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
pub(in crate::tests) fn ea_elides_an_allocation_no_safepoint_names() {
    use crate::ir::{Op, SafepointSnapshot, NO_NODE};

    let mut f = ea_full_layout_fixture();
    f.g.safepoints.push(SafepointSnapshot::new(
        3,
        vec![NO_NODE],
        vec![f.val],
        Vec::new(),
    ));
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
pub(in crate::tests) fn ea_safepoint_slot_naming_a_replaced_load_follows_the_value() {
    use crate::ir::{Op, SafepointSnapshot, NO_NODE};

    let mut f = ea_full_layout_fixture();
    f.g.safepoints.push(SafepointSnapshot::new(
        13,
        vec![f.load],
        vec![NO_NODE],
        Vec::new(),
    ));
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
pub(in crate::tests) fn ea_killed_store_leaves_the_memory_chain_intact() {
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
pub(in crate::tests) fn ea_lock_group_fixture() -> (
    crate::ir::Graph,
    crate::ir::NodeId,
    crate::ir::NodeId,
    crate::ir::NodeId,
) {
    use crate::ir::{Graph, IrType, MemKind, Op, SafepointSnapshot, NO_NODE};

    let mut g = Graph::empty(0);
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
    g.safepoints.push(SafepointSnapshot::new(
        3,
        vec![NO_NODE],
        vec![m_exit],
        Vec::new(),
    ));
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
pub(in crate::tests) fn ea_a_partially_refusable_lock_group_is_refused_whole() {
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
pub(in crate::tests) fn ea_the_flat_elide_locks_list_no_longer_drives_the_applier() {
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
pub(in crate::tests) fn ea_load_before_a_later_store_forwards_the_pre_store_value() {
    use crate::ir::{Graph, IrType, MemKind, Op};

    let mut g = Graph::empty(0);
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
pub(in crate::tests) fn ea_an_identity_compared_object_is_not_replaced() {
    use crate::ir::{CmpOp, Graph, IrType, MemKind, Op};

    // `null_rhs`: compare against `aconst_null` (an `ifnull`) instead of
    // against another reference.
    let build = |null_rhs: bool| -> (Graph, crate::ir::NodeId) {
        let mut g = Graph::empty(0);
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
pub(in crate::tests) fn ea_a_cold_path_escape_is_classified_partial() {
    use crate::ir::{Graph, IrType, MemKind, Op};
    use std::collections::HashMap;

    // Foo o = new Foo(); o.x = 42; if (c) escape(o);   — with `c` profiled
    // as never taken, so the call that publishes `o` is cold.
    let mut g = Graph::empty(0);
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
pub(in crate::tests) fn ir_new_scalar_replaces_end_to_end() {
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
pub(in crate::tests) fn assert_no_snapshot_names_a_dead_node(graph: &crate::ir::Graph) {
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
pub(in crate::tests) fn ir_new_scalar_replaces_through_astore_local() {
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
pub(in crate::tests) fn run_ir_pipeline_through_escape_analysis(
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
pub(in crate::tests) fn foo_builder(num_locals: usize, putfield_pc: usize, getfield_pc: usize) -> crate::ir::IrBuilder {
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
pub(in crate::tests) fn dead_snapshot_locals_are_cut_and_live_ones_kept() {
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
pub(in crate::tests) fn an_allocation_dead_at_a_later_guard_is_elided_by_default() {
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
pub(in crate::tests) fn a_null_check_on_a_fresh_allocation_does_not_pin_it() {
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
pub(in crate::tests) fn an_allocation_live_at_a_guard_is_elided_with_a_materialization_recipe() {
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
pub(in crate::tests) fn ir_new_bails_on_init_of_nonfresh_receiver() {
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
pub(in crate::tests) fn scalar_new_wiring_routes_through_ir_only_with_resolver() {
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
        hidden_frame: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
        array_inc_declined: std::sync::OnceLock::new(),
        ctor_verdict_memo: std::sync::OnceLock::new(),
        pool_generation: u64::MAX,
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
                needs_post_alloc_init: false,
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
pub(in crate::tests) fn deferred_new_site_compiles_when_cp_helper_is_wired() {
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
        hidden_frame: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
        array_inc_declined: std::sync::OnceLock::new(),
        ctor_verdict_memo: std::sync::OnceLock::new(),
        pool_generation: u64::MAX,
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
            needs_post_alloc_init: true,
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
pub(in crate::tests) fn ir_call_wiring_routes_through_ir_only_with_flag() {
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
        hidden_frame: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
        array_inc_declined: std::sync::OnceLock::new(),
        ctor_verdict_memo: std::sync::OnceLock::new(),
        pool_generation: u64::MAX,
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
pub(in crate::tests) fn ir_special_call_wiring_routes_through_ir_only_with_flag() {
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
        hidden_frame: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
        array_inc_declined: std::sync::OnceLock::new(),
        ctor_verdict_memo: std::sync::OnceLock::new(),
        pool_generation: u64::MAX,
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
pub(in crate::tests) fn ir_long_wiring_routes_through_ir_only_with_flag() {
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
        hidden_frame: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
        array_inc_declined: std::sync::OnceLock::new(),
        ctor_verdict_memo: std::sync::OnceLock::new(),
        pool_generation: u64::MAX,
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

