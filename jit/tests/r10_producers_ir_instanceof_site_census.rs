// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 10, wave 9 — the OPTIMIZING tier's `instanceof` arm files
//! each outcome in its own census bucket.
//!
//! Closes
//! `docs/internal/fixed-bugs/r10-producers-ir-tier-instanceof-arm-still-has-no-census-FIXED-20260922.md`,
//! which was itself the residual left open when wave 8 gave the SINGLE-PASS arm
//! its five-bucket census
//! (`jit/tests/r10_producers_instanceof_site_census.rs`). That page's argument,
//! in one sentence: the tier with the census was the tier a hot method never
//! runs, because `ir_lower.rs`'s `Op::InstanceOf` fast path (round 9 wave 9) is
//! the door a C2 body reaches, and the `irl9` change exists precisely because
//! that supersession cost 550 ms where the single-pass body took 30.
//!
//! # Why this file exists beside `r9w9_irl9_ir_instanceof_fast_path.rs`
//!
//! That file asserts the arm ANSWERS correctly and counts HELPER CALLS at run
//! time. This one asserts which COMPILE-TIME bucket each site lands in. They
//! are different claims about different quantities, and the second is the one
//! `CHECKCAST_INLINE_SITES_IR`'s own doc says is needed: "the intrinsic
//! answering the same values as the native it replaced proves nothing about
//! whether it actually ran."
//!
//! # Four buckets, not five, and that is the finding
//!
//! The page warned in as many words: **do not assume the five buckets
//! transfer.** The single-pass arm's `refused-untrusted-operand` is fed by
//! `operand_is_trusted_oop`, a test over that backend's `stack_oop_marks`
//! abstract-stack marks. The optimizing tier has no such test and can have
//! none — its operand is an SSA value whose reference-ness is a property of the
//! IR — so there is no population for that bucket to count, and a bucket with
//! that name fed by some other condition would be worse than a missing one.
//! `instanceof_ir_inline_sites()` is therefore a four-tuple beside
//! `instanceof_inline_sites()`'s five, and step 6 pins that the two are not the
//! same counters read twice.
//!
//! The fourth bucket that DOES transfer transfers with a different population,
//! which steps 4 and 5 pin together: `refused-no-target-id` on this door can
//! only ever mean "the resolved class id is the unbakeable `0`", because an
//! unresolved site is refused upstream by `lib.rs`'s `instanceof_info`
//! admission gate and never becomes an `Op::InstanceOf` node at all. Since
//! `ClassStore::next_id` is `self.classes.len()`, id `0` belongs to the first
//! class the VM loads, so that is a live population rather than a tripwire.
//!
//! # Deltas, one `#[test]`, absolute values never asserted
//!
//! These are process-wide counters, so every assertion is on a DELTA across one
//! compile. Exactly ONE `#[test]` in this binary for the reason
//! `r10_producers_instanceof_site_census.rs` gives: two would run concurrently
//! and interleave their deltas.

use cratonvm_jit::{
    try_compile_request, CachedBytecodeMethod, CompileRequest, JitNewSite, JitRuntimeHelpers,
};
use cratonvm_types::ClassId;
use std::sync::Arc;

/// `(prim_array, class_id, final_miss, refused_no_target_id)` — the optimizing
/// tier's four.
type IrCensus = (u64, u64, u64, u64);

fn ir_census() -> IrCensus {
    cratonvm_jit::instanceof_ir_inline_sites()
}

fn ir_delta(before: IrCensus, after: IrCensus) -> IrCensus {
    (
        after.0 - before.0,
        after.1 - before.1,
        after.2 - before.2,
        after.3 - before.3,
    )
}

/// Stub helpers. Nothing here executes the compiled code — this test reads
/// counters, not answers — so every helper may be the panicking stub.
fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("r10 producers IR census test invoked an unwired runtime helper");
    }
    unsafe extern "C" fn self_guard(_vm_ptr: i64) -> i64 {
        0
    }
    unsafe extern "C" fn native_stack_floor() -> i64 {
        0
    }
    unsafe extern "C" fn record_throw_bci(_bci: i64) {}
    unsafe extern "C" fn deopt_unserviceable_stub(
        _vm: i64,
        _info: i64,
        _args: i64,
        _n: i64,
    ) -> i64 {
        i64::MIN
    }
    let s = stub as *const () as usize;
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
        self_call_stack_guard: self_guard as *const () as usize,
        local_handler_lookup: 0,
        native_stack_floor_fn: native_stack_floor as *const () as usize,
        ldc_string: s,
        set_throw_bci: record_throw_bci as *const () as usize,
        service_callee_deopt: deopt_unserviceable_stub as *const () as usize,
        ..Default::default()
    }
}

/// `static boolean f(Object o) { return o instanceof <cp 1>; }`.
///
/// Shape and padding copied from `r9w9_irl9_ir_instanceof_fast_path.rs`'s
/// `instanceof_method`, which is the door this tier's `instanceof` arm is
/// reached through in the only other test that reaches it.
fn instanceof_method(name: &str) -> CachedBytecodeMethod {
    let code: Vec<u8> = vec![0x2a, 0xc1, 0x00, 0x01, 0xac, 0x00, 0x00];
    CachedBytecodeMethod {
        declaring_class_id: ClassId::new(1),
        class_name: Arc::from("R10ProducersIrCensus"),
        method_name: Arc::from(name),
        method_descriptor: Arc::from("(Ljava/lang/Object;)Z"),
        source_file: None,
        code: Arc::from(code.as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 8,
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
    }
}

/// Compile one `instanceof <target>` and say whether the OPTIMIZING tier took
/// it.
///
/// `target_id` is `None` to leave the site UNRESOLVED, which is what `lib.rs`'s
/// `instanceof_info` admission gate refuses: the site gets no row, the IR
/// builder's `0xc1` arm plants an `UnresolvedTypeCheck` uncommon trap, and the
/// body goes to single-pass. So an unresolved target does NOT reach this tier's
/// census at all — a fact worth pinning rather than assuming, which is what
/// step 5 does.
fn compile_ir_maybe(
    method: &str,
    target: &'static str,
    target_id: Option<u32>,
    helpers: &JitRuntimeHelpers,
) -> bool {
    // Routing, not policy: see `ir_vs_singlepass.rs::routing_not_policy`.
    cratonvm_jit::ir_evidence::force_accept_always_for_this_process();
    let cm = instanceof_method(method);
    let names = move |cp: u16| (cp == 1).then(|| target.to_string());
    let loaded = move |cp: u16| {
        target_id
            .filter(|_| cp == 1)
            .map(|class_id| JitNewSite::Resolved {
                class_id,
                num_fields: 0,
                has_prim_init: false,
                has_finalizer: false,
            })
    };
    let mut req = CompileRequest::new(&cm, helpers);
    req.cp_class_name_resolver = Some(&names);
    req.cp_new_resolver = Some(&loaded);
    req.optimize = true;
    let compiled = try_compile_request(&req).expect("instanceof must compile");
    compiled.used_ir_backend
}

/// [`compile_ir_maybe`], asserting the optimizing tier took it.
fn compile_ir(method: &str, target: &'static str, target_id: u32, helpers: &JitRuntimeHelpers) {
    assert!(
        compile_ir_maybe(method, target, Some(target_id), helpers),
        "{method}: the optimizing tier must compile this body, or the census \
         this test reads was fed by the single-pass arm and every assertion \
         about it is about the wrong door"
    );
}

#[test]
fn each_ir_instanceof_outcome_moves_exactly_its_own_bucket() {
    if !cratonvm_jit::x64::checkcast_inline_enabled() {
        // `CRATONVM_JIT_CHECKCAST_INLINE=0` gates the arm, census included.
        // Returning early rather than asserting all-zero deltas is the honest
        // thing: an all-zero census under that flag is documented behaviour,
        // not a finding, and it is what the printed line says in its own text.
        return;
    }
    let h = helpers();

    // ---- 1. the class-id compare, against a `final` boot class -------------
    //
    // Both buckets move: `class_id` because the compare was emitted, and
    // `final_miss` because `java/lang/String` is on
    // `ir_instanceof_miss_is_final_boot_class`'s list, so the arm also emitted
    // the definite-miss inline `0`. `final_miss` is a SUBSET of `class_id`,
    // never a fifth disjoint outcome — this pair with case 2 is what pins that.
    const STRING_ID: u32 = 0x000A_2101;
    let before = ir_census();
    compile_ir("fStr", "java/lang/String", STRING_ID, &h);
    let d = ir_delta(before, ir_census());
    assert_eq!(
        (d.0, d.1, d.3),
        (0, 1, 0),
        "a resolved, non-array target is exactly one optimizing class-id site \
         and nothing else: got {d:?}"
    );
    if cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_INSTANCEOF_FINAL_MISS") {
        assert_eq!(
            d.2, 1,
            "`java/lang/String` is a final boot class, so the definite-miss stub \
             was emitted on the door a hot method reaches, and its engagement \
             must be counted there — this counter is the only evidence that a \
             DEFAULT-ON substitution decided by a hard-coded name list ever fires \
             in the optimizing tier: got {d:?}"
        );
    } else {
        assert_eq!(
            d.2, 0,
            "with CRATONVM_JIT_INSTANCEOF_FINAL_MISS=0 the stub is not emitted, \
             so its counter must not move: got {d:?}"
        );
    }

    // ---- 2. a class-id site whose target is NOT final ----------------------
    //
    // `class_id` moves and `final_miss` does not. Without this case the
    // `final_miss` assertion above cannot tell "counted the substitution" from
    // "counted every class-id site twice".
    let before = ir_census();
    compile_ir("fUser", "com/example/r10/Open", 0x000A_2102, &h);
    let d = ir_delta(before, ir_census());
    assert_eq!(
        d,
        (0, 1, 0, 0),
        "a target that is neither a primitive array nor provably final is one \
         class-id site with no definite-miss stub: got {d:?}"
    );

    // ---- 3. the 1-D primitive-array arm ------------------------------------
    //
    // A disjoint bucket, not a variant of the class-id one: it proves its
    // answer from the header's KIND_TAGS byte and needs no class id at all,
    // which is the whole point — a primitive array's header class id is 0 and
    // could never have matched.
    let before = ir_census();
    compile_ir("fIntArr", "[I", 0x000A_2103, &h);
    let d = ir_delta(before, ir_census());
    assert_eq!(
        d,
        (1, 0, 0, 0),
        "`[I` is answered from the KIND_TAGS byte, so it is a prim-array site \
         and must not be counted as a class-id one: got {d:?}"
    );

    // ---- 4. refused: no USABLE target id ------------------------------------
    //
    // The denominator, and the bucket whose reachable population on this door
    // is not the one its name first suggests. Both arms derive their target as
    // `typecheck_target_for_site(..).filter(|&id| id != 0)`, so the refusal has
    // two sub-causes: the site had no id at all, or its id is `0`. On THIS door
    // only the second is reachable — the first is refused upstream by `lib.rs`'s
    // `instanceof_info` admission gate and never becomes an `Op::InstanceOf`
    // node (step 5 pins that). And `0` is not a hypothetical sentinel:
    // `ClassStore::next_id` is `self.classes.len()`, so the FIRST class the VM
    // loads owns id `0` and `x instanceof <that class>` takes this arm on a real
    // workload.
    //
    // Without this bucket a zero on the three inline buckets could not be told
    // from an `instanceof`-free workload, which is the denominator the parent
    // page asked for by the name `INSTANCEOF_HELPER_SITES`.
    let before = ir_census();
    compile_ir("fIdZero", "java/lang/Number", 0, &h);
    let d = ir_delta(before, ir_census());
    assert_eq!(
        d,
        (0, 0, 0, 1),
        "a target whose class id is the unbakeable `0` takes the bare \
         jit_instanceof call and is the refused bucket, not any inline one: \
         got {d:?}"
    );

    // ---- 5. an UNRESOLVED site never reaches this door at all ---------------
    //
    // Pinned rather than assumed, because it is what makes step 4's comment
    // true and because it is the one place this door's census differs in
    // POPULATION, not just in bucket count, from the single-pass one. The
    // single-pass arm meets unresolved sites and files them under
    // `refused-no-target-id`; here `lib.rs`'s admission gate omits the site
    // from `instanceof_info`, the IR builder's `0xc1` arm plants an
    // `UnresolvedTypeCheck` uncommon trap, and the body goes to single-pass —
    // so this census never sees it.
    //
    // If this ever starts returning `true`, step 4's second sub-cause is no
    // longer the only reachable one and the bucket's doc comment needs the
    // first one back.
    assert!(
        !compile_ir_maybe("fUnresolved", "java/lang/Number", None, &h),
        "an `instanceof` whose target is not loaded at compile time must not \
         reach the optimizing tier: `lib.rs`'s instanceof_info gate admits only \
         resolved sites, because the not-yet-loaded resolution path can run a \
         user classloader"
    );

    // ---- 6. the two doors are counted APART --------------------------------
    //
    // The assertion the parent page's whole argument rests on. If the
    // optimizing tier's sites were filed into `instanceof_inline_sites()`'s
    // buckets, the printed line would read as a single-pass census that moved
    // on a workload whose single-pass arm compiled nothing — the exact
    // mislabelling `CHECKCAST_INLINE_SITES_IR` exists to prevent for
    // `checkcast`.
    //
    // In this `#[test]` rather than its own, because both halves read
    // process-wide counters and two `#[test]`s in one binary interleave.
    let sp_before = cratonvm_jit::instanceof_inline_sites();
    let ir_before = ir_census();
    compile_ir("fApart", "java/lang/String", 0x000A_2201, &h);
    let sp_after = cratonvm_jit::instanceof_inline_sites();
    let ir_after = ir_census();
    assert_eq!(
        sp_before, sp_after,
        "an optimizing-tier compile must not move ANY single-pass bucket: the \
         two doors are independent emitters and the printed line invites the \
         reader to compare them, which is meaningless if one door's sites land \
         in the other's buckets"
    );
    assert_ne!(
        ir_before, ir_after,
        "...and it must move an optimizing bucket, or this test would pass with \
         the census removed entirely"
    );
}
