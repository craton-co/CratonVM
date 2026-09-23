// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 10, wave 8, lane `producers` — the code-cache producers
//! that `vm::jit::code_cache_lifecycle`'s process report pulls.
//!
//! Three of the seven fields
//! `docs/internal/retired/r10-report-seven-code-cache-fields-still-have-no-producer-20260921-RETIRED-20260922.md`
//! left suppressed now have one, all three inside `cratonvm-jit` because the
//! crate graph forbids anything else (`cratonvm-vm` depends on `cratonvm-jit`
//! with no edge back, so the report PULLS). This file pins the producers where
//! they are produced. The consumer side — the flags, the `Display` arms, the
//! `.max(live)` clamp — is pinned by `vm/src/jit/code_cache_lifecycle.rs`'s own
//! test module, which can see them; this binary cannot, and asserting a report's
//! text from here would only re-test the pull.
//!
//! What is pinned here:
//!
//! 1. **`peak_live_bytes` is actually fed.** A publish moves it off zero. That
//!    is the whole claim, and it is the one a suppression-to-feed change can get
//!    wrong silently: a `fetch_max` placed where `installed_bytes` has not yet
//!    been bumped, or behind a branch a real publish does not take, leaves the
//!    field reading exactly what it read while it was suppressed.
//! 2. **`withdrawn_by_reason` files each withdrawal under the reason its ENTRY
//!    POINT chose**, and the reasons are distinguishable. This is the assertion
//!    with the most riding on it: the report's "retirement by reason" line exists
//!    so an operator can tell cache-pressure eviction from class unloading, and a
//!    threading change that funnelled everything into one bucket would print a
//!    plausible histogram that answers nothing.
//! 3. **The histogram sums to the total.** `retire_withdrawn_body` bumps
//!    `withdrawn_bodies` and one bucket in the same call, so a bucket that stops
//!    being bumped shows up here rather than as a report line whose columns
//!    quietly stop adding up.
//! 4. **`jit_oldest_retirement_age()`'s contract**, including that it does not
//!    deadlock against the queue lock it takes, and that it separates "the queue
//!    is empty" (`None`) from "queued by the most recent withdrawal" (`Some(0)`).
//!
//! **One `#[test]`, deliberately.** Every counter read here is process-wide, so
//! two concurrent tests in one binary would interleave their deltas. Every
//! assertion is on a delta or on an invariant, never on an absolute count.
//!
//! **Read, not executed by its author.** Lane `producers` may not build or run
//! anything; the orchestrator owns this file's first run.

use cratonvm_jit::x64::compile_with_param_slots;
use cratonvm_jit::{retire_reason, JitCache, JitCodeReclamationStats};
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
        panic!("r10 producers retire-reason test invoked a runtime helper");
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

/// `withdrawn_by_reason` deltas, bucket by bucket.
fn reason_delta(
    before: &JitCodeReclamationStats,
    after: &JitCodeReclamationStats,
) -> [u64; retire_reason::COUNT] {
    let mut out = [0u64; retire_reason::COUNT];
    for (i, o) in out.iter_mut().enumerate() {
        *o = after.withdrawn_by_reason[i] - before.withdrawn_by_reason[i];
    }
    out
}

/// One withdrawal in `code`, and nothing anywhere else.
fn exactly_one(delta: [u64; retire_reason::COUNT], code: usize) -> bool {
    delta
        .iter()
        .enumerate()
        .all(|(i, &n)| if i == code { n == 1 } else { n == 0 })
}

#[test]
fn the_code_cache_producers_the_process_report_pulls_are_real() {
    let h = helpers();

    // ---- the numbering, before anything depends on it ----------------------
    //
    // `vm/src/jit/code_cache_lifecycle.rs` carries a `const _` assertion that
    // this numbering and its own agree, which is what makes the report's
    // POSITIONAL copy of the histogram safe. That assertion lives in the other
    // crate; this one pins the producer side's own shape, so a renumbering here
    // fails in `cratonvm-jit`'s own suite as well as in the VM's compile.
    assert_eq!(retire_reason::COUNT, 6);
    let codes = [
        retire_reason::SUPERSEDED,
        retire_reason::INVALIDATED,
        retire_reason::DEOPTIMIZED,
        retire_reason::CACHE_PRESSURE,
        retire_reason::CLASS_UNLOADED,
        retire_reason::SHUTDOWN,
    ];
    for (i, c) in codes.iter().enumerate() {
        assert_eq!(
            *c, i,
            "retire_reason codes must be 0..COUNT in declaration order"
        );
    }

    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("R10Producers");
    let method: Arc<str> = Arc::from("f");
    let descriptor: Arc<str> = Arc::from("(Ljava/lang/Object;)I");
    let cid = cratonvm_types::ClassId::new(0x0010_7208);

    // ---- 1. a publish feeds the high-water mark -----------------------------
    let before = stats();
    cache.put(
        Arc::clone(&class),
        Arc::clone(&method),
        Arc::clone(&descriptor),
        cid,
        body(&h),
    );
    let after = stats();
    assert_eq!(
        after.installed_bodies - before.installed_bodies,
        1,
        "the fixture body must actually publish, or nothing below measures anything"
    );
    let live = after.installed_bytes.saturating_sub(after.reclaimed_bytes);
    assert!(
        live > 0,
        "a published body leaves mapped bytes behind: {after:?}"
    );
    assert!(
        after.peak_live_bytes > 0,
        "THE producer assertion: `mark_published` is followed by a `fetch_max` on \
         the live figure, so a publish cannot leave the high-water mark at the \
         zero it read while the field was suppressed. peak={} live={live}",
        after.peak_live_bytes,
    );
    // The peak is a LOWER bound by construction (two independent relaxed loads,
    // saturating subtraction), so it may legitimately sit below `live` by up to
    // one body under concurrency — which is why the VM side clamps with
    // `.max(live)`. Nothing else runs in this binary, so here it must be exact,
    // and asserting the exact equality is what would catch a `fetch_max` reading
    // the wrong pair of counters (e.g. `installed - installed`, which is 0, or
    // `installed` alone, which is too high the moment anything is reclaimed).
    assert_eq!(
        after.peak_live_bytes, live,
        "single-threaded, the sample is exact: peak={} live={live}",
        after.peak_live_bytes,
    );

    // ---- 2. SUPERSEDED: a second publish of the same key --------------------
    let before = stats();
    cache.put(
        Arc::clone(&class),
        Arc::clone(&method),
        Arc::clone(&descriptor),
        cid,
        body(&h),
    );
    let after = stats();
    assert_eq!(
        after.withdrawn_bodies - before.withdrawn_bodies,
        1,
        "the displaced body is one withdrawal"
    );
    let d = reason_delta(&before, &after);
    assert!(
        exactly_one(d, retire_reason::SUPERSEDED),
        "a republication of the same key is SUPERSEDED and nothing else: {d:?}"
    );

    // ---- 3. CLASS_UNLOADED: distinguishable from INVALIDATED ----------------
    //
    // The load-bearing case. `docs/feature-designs/jit-r10-report-proposals.md`
    // doubted this was separable, on the grounds that `invalidate_matching`
    // "serves both assumption invalidation and class unloading" and that folding
    // them would defeat the one reading the report exists for. It is separable,
    // because the distinction lives one level up in which NAMED entry point was
    // called, and this asserts that it survives the threading.
    let before = stats();
    let evicted = cache.invalidate_unloaded_class(cid, &class);
    let after = stats();
    assert_eq!(
        evicted, 1,
        "the one live body is owned by the unloaded class"
    );
    let d = reason_delta(&before, &after);
    assert!(
        exactly_one(d, retire_reason::CLASS_UNLOADED),
        "`invalidate_unloaded_class` must NOT land in the INVALIDATED bucket — \
         telling eviction from class unloading is the reading this histogram \
         exists for: {d:?}"
    );

    // ---- 4. INVALIDATED: `clear_all` is redefinition, not shutdown ----------
    //
    // The proposal mapped `clear_all` to `SHUTDOWN`. Its two production callers
    // are `vm/src/vm/vm_exec.rs`'s `redefineClass` handler and
    // `vm/src/vm/vm_init.rs`'s `jit_invalidate_adapter`, both of which invalidate
    // a RUNNING VM's cache, so `INVALIDATED` is the honest code and this pins it.
    //
    // Last, because `clear_all` raises this cache's install barrier: a body
    // compiled before it can never be published afterwards.
    cache.put(
        Arc::clone(&class),
        Arc::clone(&method),
        Arc::clone(&descriptor),
        cid,
        body(&h),
    );
    let before = stats();
    let flushed = cache.clear_all();
    let after = stats();
    assert_eq!(flushed, 1, "one body was in the cache to flush");
    let d = reason_delta(&before, &after);
    assert!(
        exactly_one(d, retire_reason::INVALIDATED),
        "`clear_all` is a redefinition-class invalidation of a running VM, not a \
         shutdown: {d:?}"
    );

    // ---- 5. the histogram sums to the total --------------------------------
    //
    // `retire_withdrawn_body` bumps `withdrawn_bodies` and one bucket in the same
    // call, so this identity holds by construction and its failure means a
    // withdrawal site was added that passes a code outside `retire_reason` (which
    // is counted in the total and dropped from the histogram by design, so that
    // it cannot panic a compile thread). Single-threaded here, so the usual
    // relaxed-snapshot skew of one event cannot occur.
    let s = stats();
    assert_eq!(
        s.withdrawn_by_reason.iter().sum::<u64>(),
        s.withdrawn_bodies,
        "every withdrawal must carry a reason: {s:?}"
    );
    // SHUTDOWN and DEOPTIMIZED have no site reachable from this test:
    // `DEOPTIMIZED` comes only from `JitCache::remove` and `SHUTDOWN` from
    // nothing at all. Asserted as zero to catch a future edit that quietly
    // retargets one of the four codes above onto them.
    assert_eq!(
        s.withdrawn_by_reason[retire_reason::SHUTDOWN],
        0,
        "SHUTDOWN is a reserved code with no production site: {s:?}"
    );

    // ---- 6. the deferral age accessor's contract ----------------------------
    //
    // Called twice in a row, which is the cheap half of "it does not deadlock
    // against the retirement-queue lock it takes" — the hazard worth a pin here,
    // because the process report calls it while building a snapshot and a
    // self-deadlock would hang a shutdown dump rather than fail it.
    //
    // The VALUE is not asserted. Every withdrawal above went through
    // `defer_jit_owner`, which releases immediately when no JIT execution is in
    // flight — the case in this binary — so the queue is expected to be empty and
    // the honest answer `None`. What is asserted is that `None` and `Some(0)` stay
    // distinguishable, since an age of 0 (queued by the most recent withdrawal)
    // is a real reading and not an absence.
    let first = cratonvm_jit::jit_oldest_retirement_age();
    let second = cratonvm_jit::jit_oldest_retirement_age();
    if cratonvm_jit::jit_retirement_queue_len() == 0 {
        assert_eq!(
            first, None,
            "an empty retirement queue has no oldest owner, and must answer None \
             rather than 0 — 0 means 'queued by the most recent withdrawal'"
        );
        assert_eq!(second, None, "and must keep answering None");
    } else {
        // Non-empty: some body is still retained. The age is then whatever the
        // generation counter says; only its presence is a property of the
        // accessor, so only that is asserted.
        assert!(
            first.is_some(),
            "a non-empty queue has an oldest owner and therefore an age"
        );
        assert!(second.is_some());
    }
}
