// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 10, wave 8, lane `producers` — the single-pass
//! `instanceof` arm's site census counts the right bucket for each outcome.
//!
//! Closes
//! `docs/internal/fixed-bugs/r10-readers-instanceof-arm-has-no-site-census-FIXED-20260922.md`,
//! whose finding was that `jit/src/x64/op_object.rs` handles `checkcast` and
//! `instanceof` a hundred lines apart with the same inline fast-path structure,
//! that the `checkcast` arm feeds four counters split by cause, that those four
//! are PRINTED, and that the `instanceof` arm fed nothing at all — so a reader
//! of `[cratonvm] JIT checkcast inline sites:` saw a cause-by-cause account of
//! one type check and a checkcast-shaped hole where its twin should be.
//!
//! # Why this test compiles four methods instead of asserting a total
//!
//! Because the value of the census is the SPLIT, and a split is only pinned by
//! driving each arm separately. A single "sites counted" total that moves would
//! be satisfied by a census that files every outcome in one bucket, which is
//! exactly the shape the page argued against: the two refusals want opposite
//! fixes (an unresolved target id is transient and a recompile clears it; an
//! untrusted operand is structural), so a combined count that moves identifies
//! neither.
//!
//! Every assertion is on a DELTA across one compile, never on an absolute
//! value, because these are process-wide counters. That also makes the test
//! independent of how many `instanceof` sites anything else in this binary
//! compiled — and this file is one `#[test]` for the same reason
//! `r9w5_typecheck5_instanceof_final_miss.rs` is: two `#[test]`s in one binary
//! run concurrently and would interleave their deltas.
//!
//! # What it deliberately does NOT assert
//!
//! That any bucket is non-zero at process start, or that the four buckets sum
//! to anything in particular across the whole process. The arm is gated by
//! `checkcast_inline_enabled()`, so under `CRATONVM_JIT_CHECKCAST_INLINE=0`
//! nothing is counted at all and this test returns early — the same
//! early-return every other test of these arms uses, and the reason the
//! `[cratonvm]` line that prints the census says so in its own text.
//!
//! **Read, not executed by its author.** Lane `producers` may not build or run
//! anything; this file was written against the source of
//! `jit/src/x64/op_object.rs` and `jit/src/lib.rs`, and the orchestrator owns
//! its first run.

use cratonvm_jit::x64::compile_with_param_slots;
use cratonvm_jit_api::JitRuntimeHelpers;
use std::collections::{HashMap, HashSet};

/// `(prim_array, class_id, final_miss, no_target_id, untrusted)`.
type Census = (u64, u64, u64, u64, u64);

fn census() -> Census {
    cratonvm_jit::instanceof_inline_sites()
}

/// `after - before`, bucket by bucket.
fn delta(before: Census, after: Census) -> Census {
    (
        after.0 - before.0,
        after.1 - before.1,
        after.2 - before.2,
        after.3 - before.3,
        after.4 - before.4,
    )
}

/// Stub helpers. `instanceof_check` is a real address because the arm bakes a
/// call to it on every path; nothing here executes the compiled code, so the
/// helper is never entered.
fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("r10 producers census test invoked an unwired runtime helper");
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

/// Compile `int f(Object o) { return o instanceof <name>; }`.
///
/// `target` is the resolved class id to intern for the site, or `None` to leave
/// the site unresolved — which is what drives the `no_target_id` bucket, since
/// `typecheck_target_for_site` then has nothing to answer with. `method_key`
/// empty is what drives the `untrusted` bucket: the arm's
/// `operand_is_trusted_oop` requires a non-empty key, and the census's `else`
/// arm is reached when a target id EXISTS and the operand is not trusted.
///
/// Shape copied from `r9w5_typecheck5_instanceof_final_miss.rs`'s
/// `instanceof_method`, which is the only door in the tree that reaches the
/// inline typecheck guard from a hand-built body.
fn compile_instanceof(
    helpers: &JitRuntimeHelpers,
    name: &str,
    target: Option<u32>,
    method_key: &str,
) -> bool {
    let (name_ptr, name_len) = cratonvm_jit::intern_typecheck_target(name, target);
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xc1, 0x00, 0x01, // 1: instanceof #1
        0xac, // 4: ireturn
        0, 0,
    ];
    // The argument list is reproduced from
    // `r9w5_typecheck5_instanceof_final_miss.rs`'s `instanceof_method` WITH its
    // per-argument comments, deliberately. This call takes 39 positional
    // arguments, thirteen of which are consecutive `Vec::new()`s; dropping the
    // comments to shorten it is how one gets miscounted, and a miscount here is
    // a type error at best and the wrong table at worst.
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
        0b1,        // param_oop_mask: the parameter is a reference
        Vec::new(), // compact_field_info
        method_key, // empty => the operand is NOT a trusted oop
        None,       // despec
        Vec::new(), // indy_info
        None,       // elidable_init_pcs
        cratonvm_jit::x64::BackendRequest::default(),
    )
    .is_some()
}

#[test]
fn each_instanceof_outcome_moves_exactly_its_own_bucket() {
    if !cratonvm_jit::x64::checkcast_inline_enabled() {
        // `CRATONVM_JIT_CHECKCAST_INLINE=0` gates the whole arm, census
        // included. Returning early rather than asserting all-zero deltas is
        // the honest thing: an all-zero census under that flag is the
        // documented behaviour, not a finding.
        return;
    }
    let h = helpers();
    const TRUSTED_KEY: &str = "R10Producers.f:(Ljava/lang/Object;)I";

    // ---- 1. the inline class-id compare, against a `final` boot class -------
    //
    // Both buckets move: `class_id` because the compare was emitted, and
    // `final_miss` because `java/lang/String` is on
    // `instanceof_miss_is_final_boot_class`'s nine-name list, so the arm also
    // emitted the definite-miss inline `0`. `final_miss` is a SUBSET of
    // `class_id`, never a fifth disjoint outcome — this pair is what pins that.
    let before = census();
    assert!(compile_instanceof(
        &h,
        "java/lang/String",
        Some(0x0005_7101),
        TRUSTED_KEY
    ));
    let d = delta(before, census());
    assert_eq!(
        (d.0, d.1, d.3, d.4),
        (0, 1, 0, 0),
        "a resolved, trusted, non-array target is exactly one class-id site and \
         nothing else: got {d:?}"
    );
    if cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_INSTANCEOF_FINAL_MISS") {
        assert_eq!(
            d.2, 1,
            "`java/lang/String` is a final boot class, so the definite-miss stub \
             was emitted and its engagement must be counted — this counter is the \
             only evidence that a DEFAULT-ON substitution decided by a hard-coded \
             name list ever fires: got {d:?}"
        );
    } else {
        assert_eq!(
            d.2, 0,
            "with CRATONVM_JIT_INSTANCEOF_FINAL_MISS=0 the stub is not emitted, so \
             its counter must not move: got {d:?}"
        );
    }

    // ---- 2. a class-id site whose target is NOT final -----------------------
    //
    // `class_id` moves and `final_miss` does not. Without this case the
    // `final_miss` assertion above cannot distinguish "counted the
    // substitution" from "counted every class-id site twice".
    let before = census();
    assert!(compile_instanceof(
        &h,
        "java/util/ArrayList",
        Some(0x0005_7102),
        TRUSTED_KEY
    ));
    let d = delta(before, census());
    assert_eq!(
        d,
        (0, 1, 0, 0, 0),
        "`java/util/ArrayList` is neither a primitive array nor a final boot \
         class, and was not interned as a proven-final user class, so it is one \
         class-id site with no definite-miss stub: got {d:?}"
    );

    // ---- 3. the 1-D primitive-array arm ------------------------------------
    //
    // A disjoint bucket, not a variant of the class-id one: it proves its
    // answer from the header's KIND_TAGS byte and needs no class id at all,
    // which is the whole point (a primitive array's header class id is 0 and
    // could never have matched).
    let before = census();
    assert!(compile_instanceof(&h, "[B", Some(0x0005_7103), TRUSTED_KEY));
    let d = delta(before, census());
    assert_eq!(
        d,
        (1, 0, 0, 0, 0),
        "`[B` is answered from the KIND_TAGS byte, so it is a prim-array site and \
         must not be counted as a class-id one: got {d:?}"
    );

    // ---- 4. refused, no resolvable target id -------------------------------
    //
    // Interned with no class id, so `typecheck_target_for_site` answers `None`
    // and the arm falls through to the bare `jit_instanceof` call. Transient by
    // nature — the site's `CONSTANT_Class` was not loaded when this body was
    // compiled — which is why it is counted apart from the untrusted refusal.
    let before = census();
    assert!(compile_instanceof(
        &h,
        "java/lang/Number",
        None,
        TRUSTED_KEY
    ));
    let d = delta(before, census());
    assert_eq!(
        d,
        (0, 0, 0, 1, 0),
        "an unresolved target is the no-target-id refusal, not the untrusted one: \
         got {d:?}"
    );

    // ---- 5. refused, untrusted operand --------------------------------------
    //
    // An empty `method_key` makes `operand_is_trusted_oop` false while the
    // target id still resolves, which is the arm's `else`: the structural
    // refusal, the one a recompile does NOT fix. That this reaches a DIFFERENT
    // bucket from case 4 is the single most load-bearing assertion in this
    // file, because the page's argument for splitting by cause rests on the two
    // wanting opposite fixes.
    let before = census();
    assert!(compile_instanceof(
        &h,
        "java/lang/Thread",
        Some(0x0005_7104),
        ""
    ));
    let d = delta(before, census());
    assert_eq!(
        d,
        (0, 0, 0, 0, 1),
        "an untrusted operand with a resolved target is the untrusted refusal: \
         got {d:?}"
    );

    // ---- the subset invariant, process-wide ---------------------------------
    //
    // `final_miss <= class_id` must hold at all times, because every
    // definite-miss stub is an extension of a class-id site. A refactor that
    // made `final_miss` exclusive — the obvious "tidy-up" — would break this,
    // and would silently change what `class-id=` means in the printed line
    // from "inline compares emitted" to "inline compares without the
    // extension", which is not a quantity anyone asked for.
    let c = census();
    assert!(
        c.2 <= c.1,
        "final_miss is a subset of class_id and cannot exceed it: {c:?}"
    );
}
