// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 10, wave 9 — the per-method publication census, the last of
//! the four producers `vm::jit::code_cache_lifecycle`'s process report pulls.
//!
//! `docs/internal/retired/r10-report-seven-code-cache-fields-still-have-no-producer-20260921-RETIRED-20260922.md`
//! left the compilation group — `methods_compiled`, `recompilations`,
//! `max_versions_for_one_method` — suppressed after wave 8 fed the other three,
//! because nothing in `cratonvm-jit` counted publishes per method and the one
//! counter that looked like it did (`tiered::CompilationStats::
//! nominate_to_first_body`) is a lower bound behind a second `if`. Wiring THAT
//! would have restored `installs / lower_bound`, which is the impossible reading
//! the whole story is about, wearing a producer.
//!
//! `cratonvm_jit::jit_compilation_census()` is the honest producer: one
//! publication count per method key, filled at `JitCache::put` and
//! `JitCache::put_osr` and gated on the same `ExecutableBuffer::mark_published`
//! call that bumps `installed_bodies`. This file pins its four claims:
//!
//! 1. **A first publication makes a method, a second makes a recompilation.**
//!    The distinction is the whole point of the group — `versions_per_method`
//!    answers "is this workload thrashing the compiler?" — and a census that
//!    counted either event as the other would print a plausible number.
//! 2. **An OSR body is a version of the SAME method**, not a second method.
//!    `installed_bodies` counts it, so a census that gave it its own key would
//!    put `versions_per_method` below the truth by exactly the number of
//!    OSR-compiled methods.
//! 3. **The table does not forget a withdrawn method.** A class unload followed
//!    by a recompile is a RECOMPILATION, which is what the model
//!    (`CodeCacheLifecycle::install`, whose own `versions` map never forgets)
//!    counts and therefore what the field is named for. A census keyed on the
//!    live cache would have called it a new method and quietly deflated the
//!    ratio on exactly the workloads that stress the code cache most.
//! 4. **`methods_compiled + recompilations == installed_bodies`**, exactly.
//!    That identity is what puts `versions_per_method` at or above `1.0` by
//!    construction rather than by hope, and it is the single assertion that
//!    fails if the census and the install counter ever stop describing the same
//!    population.
//!
//! **One `#[test]`, deliberately**, for the reason the sibling
//! `r10_producers_code_cache_retire_reasons.rs` gives: every counter read here
//! is process-wide. Unlike that file this one asserts ABSOLUTE counts, which it
//! may because a test binary is its own process and nothing else in this one
//! publishes a body.

use cratonvm_jit::x64::compile_with_param_slots;
use cratonvm_jit::{JitCache, JitCodeReclamationStats};
use cratonvm_jit_api::JitRuntimeHelpers;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

fn stats() -> JitCodeReclamationStats {
    cratonvm_jit::jit_code_reclamation_stats()
}

/// Stub helpers: nothing in this file executes compiled code, so every helper
/// is a panicking address that proves as much if one is ever entered.
fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("r10 census test invoked a runtime helper");
    }
    let s = stub as *const () as usize;
    unsafe extern "C" fn record_throw_bci(_bci: i64) {}
    unsafe extern "C" fn deopt_unserviceable_stub(
        _vm: i64,
        _info: i64,
        _args: i64,
        _n: i64,
    ) -> i64 {
        i64::MIN
    }
    JitRuntimeHelpers {
        newarray: s,
        new_object: s,
        anewarray_object: s,
        baload: s,
        bastore: s,
        iaload: s,
        iastore: s,
        aaload: s,
        aastore: s,
        multianewarray_2d: s,
        arraylength: s,
        getfield: s,
        putfield_int: s,
        putfield_long: s,
        putfield_float: s,
        putfield_double: s,
        putfield_object: s,
        getstatic: s,
        putstatic_int: s,
        putstatic_long: s,
        putstatic_float: s,
        putstatic_double: s,
        putstatic_object: s,
        checkcast: s,
        instanceof_check: s,
        throw_aioobe: s,
        throw_arithmetic: s,
        invoke_dispatch: s,
        invoke_virtual_mic: s,
        lambda_int_to_double: s,
        write_barrier: s,
        satb_pre_write_barrier: s,
        uncommon_trap: s,
        math_fma_double: s,
        math_fma_float: s,
        tlab_end_offset_in_thread: 8,
        throw_exception: s,
        jit_npe_with_action: s,
        dispatch_threw: s,
        jit_frem: s,
        jit_drem: s,
        ldc_string: s,
        set_throw_bci: record_throw_bci as *const () as usize,
        service_callee_deopt: deopt_unserviceable_stub as *const () as usize,
        ..Default::default()
    }
}

/// One publishable body: `int f(Object o) { return o instanceof Object; }`.
///
/// The body's BEHAVIOUR is irrelevant here — nothing calls it — so this reuses
/// the one hand-built shape the tree already publishes successfully
/// (`r9w5_typecheck5_instanceof_final_miss.rs`'s `instanceof_method`), argument
/// for argument and comment for comment, rather than inventing a smaller one and
/// guessing at the three unnamed leading counts. A 39-positional-argument call
/// is not the place to economise.
fn body(helpers: &JitRuntimeHelpers) -> cratonvm_jit::CompiledMethod {
    let (name_ptr, name_len) = cratonvm_jit::intern_typecheck_target("java/lang/Object", Some(1));
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xc1, 0x00, 0x01, // 1: instanceof #1
        0xac, // 4: ireturn
        0, 0,
    ];
    compile_with_param_slots(
        // Not a door: hand-built bytecode with no method identity to admit.
        &cratonvm_jit::compile_gate::CompileAdmission::for_backend_test(),
        &code,
        5,
        1,
        1,
        false,
        Vec::new(),                         // multianewarray_info
        Vec::new(),                         // field_info
        vec![(1usize, name_ptr, name_len)], // typecheck_info
        Vec::new(),                         // static_field_info
        Vec::new(),                         // new_info
        Vec::new(),                         // new_deferred_info
        Vec::new(),                         // anewarray_info
        Vec::new(),                         // anewarray_deferred_info
        Vec::new(),                         // invoke_info
        Vec::new(),                         // direct_calls
        Vec::new(),                         // mic_slots
        Vec::new(),                         // pic_slots
        Vec::new(),                         // ldc_info
        Vec::new(),                         // ldc_string_info
        Vec::new(),                         // ldc_class_info
        Vec::new(),                         // ldc2w_info
        Default::default(),                 // ldc_fp_pcs
        HashMap::new(),
        HashMap::new(),
        helpers,
        HashSet::new(),
        HashMap::new(),
        HashMap::new(), // inline_guard_variants
        None,           // string_layout
        &[],
        0,
        0b1,                                    // param_oop_mask
        Vec::new(),                             // compact_field_info
        "R10Producers.f:(Ljava/lang/Object;)I", // non-empty method key
        None,                                   // despec
        Vec::new(),                             // indy_info
        None,                                   // elidable_init_pcs
        cratonvm_jit::x64::BackendRequest::default(),
    )
    .expect("the fixture body must compile")
}

/// The census, or a failure naming the one thing `None` means.
///
/// `jit_compilation_census()` answers `None` only when its bounded per-method
/// table filled up and stopped being exact — 2^17 distinct methods, which a test
/// binary that publishes five bodies cannot reach. So `None` here is a defect in
/// the bound or in the latch, not a legitimate suppression, and the `expect`
/// says which.
fn census() -> cratonvm_jit::JitCompilationCensus {
    cratonvm_jit::jit_compilation_census().expect(
        "the census only answers None once its 2^17-method table is full; this \
         binary publishes five bodies, so a None here means the truncation latch \
         is being set by something other than the bound",
    )
}

#[test]
fn the_compilation_census_counts_publications_per_method() {
    let h = helpers();
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("R10Census");
    let f: Arc<str> = Arc::from("f");
    let g: Arc<str> = Arc::from("g");
    let descriptor: Arc<str> = Arc::from("(Ljava/lang/Object;)I");
    let cid = cratonvm_types::ClassId::new(0x0010_7209);

    let put = |name: &Arc<str>| {
        cache.put(
            Arc::clone(&class),
            Arc::clone(name),
            Arc::clone(&descriptor),
            cid,
            body(&h),
        );
    };

    // Nothing has been published in this process yet, which is what lets every
    // assertion below be an absolute count rather than a delta.
    let start = census();
    assert_eq!(
        (
            start.methods_compiled,
            start.recompilations,
            start.max_versions_for_one_method
        ),
        (0, 0, 0),
        "a process that has published nothing has compiled no methods — and \
         note that this zero is NOT the suppressed zero the report used to \
         print: `installs` is zero beside it"
    );
    assert_eq!(stats().installed_bodies, 0);

    // ---- 1. the first publication is a METHOD, the second a RECOMPILATION ---
    put(&f);
    let c = census();
    assert_eq!(
        (c.methods_compiled, c.recompilations, c.max_versions_for_one_method),
        (1, 0, 1),
        "one method, compiled once, is one version and no recompilation"
    );

    put(&f);
    let c = census();
    assert_eq!(
        (c.methods_compiled, c.recompilations, c.max_versions_for_one_method),
        (1, 1, 2),
        "republishing the SAME key is a recompilation, not a second method — \
         which is the distinction `versions_per_method` exists to report"
    );

    // ---- 2. a different method is a different row --------------------------
    put(&g);
    let c = census();
    assert_eq!(
        (c.methods_compiled, c.recompilations, c.max_versions_for_one_method),
        (2, 1, 2),
        "a second key is a second method, and it does not move the max"
    );

    // ---- 3. an OSR body is a version of the same method --------------------
    //
    // `put_osr` writes a different MAP (`osr_methods`), so a census keyed on the
    // map rather than on the method would have made this a third method. It
    // must not: `installed_bodies` counts this publication, so splitting it out
    // would put the denominator above the truth and the ratio below it.
    let mut osr = body(&h);
    osr.compiled_via_osr = true;
    cache.put_osr(
        Arc::clone(&class),
        Arc::clone(&f),
        Arc::clone(&descriptor),
        cid,
        osr,
    );
    assert_eq!(
        stats().installed_bodies,
        4,
        "the OSR body must actually publish, or the claim below measures nothing"
    );
    let c = census();
    assert_eq!(
        (c.methods_compiled, c.recompilations, c.max_versions_for_one_method),
        (2, 2, 3),
        "an OSR body is a third version of `f`, not a third method"
    );

    // ---- 4. the table does not forget a withdrawn method -------------------
    //
    // The model's own `versions` map never forgets, and `recompilations` is
    // named for that definition. A census that dropped a method when its last
    // body was withdrawn would call the recompile below a NEW method, which
    // deflates `versions_per_method` on precisely the workloads — class
    // unloading, redefinition, cache-pressure eviction — where compilation
    // churn is the thing an operator is looking for.
    let evicted = cache.invalidate_unloaded_class(cid, &class);
    assert_eq!(evicted, 3, "both of `f`'s bodies and `g`'s are the class's");
    let c = census();
    assert_eq!(
        (c.methods_compiled, c.recompilations, c.max_versions_for_one_method),
        (2, 2, 3),
        "withdrawing bodies compiles nothing and must move no census figure"
    );

    put(&f);
    let c = census();
    assert_eq!(
        (c.methods_compiled, c.recompilations, c.max_versions_for_one_method),
        (2, 3, 4),
        "recompiling after an unload is a RECOMPILATION — the method was \
         compiled before, and the census remembers that whether or not the \
         cache still holds the body"
    );

    // ---- 5. the identity the whole group rests on --------------------------
    //
    // Every publication is either a method's first or a recompilation, and both
    // sides of this are counted from the same `mark_published` call — the census
    // is bumped only when that call reports it performed the install accounting.
    // Single-threaded here, so the usual one-event snapshot skew cannot occur
    // and the identity is exact.
    //
    // This is what makes `versions_per_method = installs / methods_compiled` at
    // or above `1.0` by construction. A failure means the census and
    // `installed_bodies` have stopped describing the same population, which is
    // how the report would come to print a mean below one version per method —
    // the reading three waves of work exist to make impossible.
    let s = stats();
    let c = census();
    assert_eq!(
        c.methods_compiled + c.recompilations,
        s.installed_bodies,
        "census {c:?} must account for every published body: {s:?}"
    );
    assert!(
        c.methods_compiled <= s.installed_bodies,
        "the denominator can never exceed the numerator: {c:?} {s:?}"
    );
    assert!(
        c.max_versions_for_one_method >= 1,
        "something was published, so some method reached version 1: {c:?}"
    );
}
