use super::*;
use crate::annotations::{GpuExcludeAttrs, GpuKernelAttrs};
use crate::test_support::load_method;

/// Build a `MethodAnnotations` carrying a `GpuKernelAttrs` with the
/// requested admission hint and all other fields at their `Default`
/// values. Used by the loosening tests below.
///
/// This relies on `GpuKernelAttrs: Default` (owned by Item 3 — see
/// the Phase-1 spec §2.3). If that derive is dropped, this helper
/// is the single place to update.
fn annotate(admit: AdmissionHint) -> MethodAnnotations {
    MethodAnnotations {
        gpu_kernel: Some(GpuKernelAttrs {
            admit: admit.into(),
            ..GpuKernelAttrs::default()
        }),
        gpu_exclude: None,
    }
}

fn track_reduction_ops(bytes: &[u8]) -> ReductionDataflow {
    let mut tracker = ReductionDataflow::default();
    let mut pc = 0usize;
    let mut prev_op = None;
    while pc < bytes.len() {
        tracker.observe(bytes, pc, prev_op);
        prev_op = Some(bytes[pc]);
        pc += instruction_size(bytes, pc).expect("test bytecode must be well-shaped");
    }
    tracker
}

#[test]
fn eligible_vector_add_is_eligible() {
    let method = load_method("EligibleVectorAdd", "vectorAdd", "([I[I[I)V");
    match analyze(&method) {
        OffloadVerdict::Eligible(sig) => {
            assert_eq!(
                sig.param_kinds,
                vec![
                    ParamKind::I32Array,
                    ParamKind::I32Array,
                    ParamKind::I32Array
                ]
            );
            assert_eq!(sig.return_kind, ParamKind::Void);
            // A void-returning per-element map (`out[i] = a[i] + b[i]`)
            // must NOT be flagged as a reduction. If it is, lowering
            // emits an atomic accumulate into a single scalar slot and
            // the dispatcher silently falls back to the CPU instead of
            // launching the per-element kernel.
            assert!(
                !sig.is_reduction,
                "void array-writing map misclassified as a reduction"
            );
        }
        v => panic!("expected Eligible, got {v:?}"),
    }
}

#[test]
fn eligible_saxpy_is_eligible() {
    let method = load_method("EligibleSaxpy", "saxpy", "(F[F[F[F)V");
    match analyze(&method) {
        OffloadVerdict::Eligible(sig) => {
            assert_eq!(
                sig.param_kinds,
                vec![
                    ParamKind::F32,
                    ParamKind::F32Array,
                    ParamKind::F32Array,
                    ParamKind::F32Array
                ]
            );
            assert_eq!(sig.return_kind, ParamKind::Void);
            // saxpy (`out[i] = a*x[i] + y[i]`) is a map, not a reduction.
            assert!(!sig.is_reduction, "saxpy map misclassified as a reduction");
        }
        v => panic!("expected Eligible, got {v:?}"),
    }
}

#[test]
fn eligible_dot_product_returns_long() {
    let method = load_method("EligibleDotProduct", "dot", "([I[I)J");
    match analyze(&method) {
        OffloadVerdict::Eligible(sig) => {
            assert_eq!(sig.return_kind, ParamKind::I64);
            // A genuine dot-product / sum reduction (scalar return,
            // accumulates, no array store) must STILL be recognised so
            // lowering emits the atomic-accumulate path. Guards against
            // the `!body_has_array_store` / scalar-return gate
            // over-tightening and dropping real reductions.
            assert!(
                sig.is_reduction,
                "genuine scalar-return dot-product reduction no longer recognised"
            );
        }
        v => panic!("expected Eligible, got {v:?}"),
    }
}

#[test]
fn reject_float_remainder_before_lowering() {
    let frem = load_method("FloatRemainder", "fremScalar", "(FF)F");
    assert_eq!(
        analyze(&frem),
        OffloadVerdict::Rejected(Reason::FloatRemainder)
    );

    let drem = load_method("FloatRemainder", "dremScalar", "(DD)D");
    assert_eq!(
        analyze(&drem),
        OffloadVerdict::Rejected(Reason::FloatRemainder)
    );
}

// ─── AUDIT 2026-07-11: frem/drem gated admission ────────────────
//
// `frem`/`drem` now have a real (if precision-bounded) PTX
// lowering — see `lowering::emit::Emitter::frem_f32`/`drem_f64`.
// `Strict` (the default, exercised above) must keep rejecting them
// unconditionally; `AdmissionHint::AllowDivByZero` is the opt-in
// that admits them (see `classify`'s `0x72 | 0x73` arm and
// `Reason::FloatRemainder` for why that hint was reused instead of
// a dedicated one).

#[test]
fn frem_admitted_under_allow_div_by_zero_hint() {
    let method = load_method("FloatRemainder", "fremScalar", "(FF)F");
    let loose = annotate(AdmissionHint::AllowDivByZero);
    match analyze_with_annotations(&method, &loose) {
        OffloadVerdict::Eligible(sig) => {
            assert_eq!(sig.param_kinds, vec![ParamKind::F32, ParamKind::F32]);
            assert_eq!(sig.return_kind, ParamKind::F32);
            assert!(
                sig.allow_div_by_zero,
                "AllowDivByZero must reach lowering through KernelSignature \
                 even when it is admitting frem, not an integer div/rem"
            );
        }
        v => panic!("expected Eligible under AllowDivByZero, got {v:?}"),
    }
}

#[test]
fn drem_admitted_under_allow_div_by_zero_hint() {
    let method = load_method("FloatRemainder", "dremScalar", "(DD)D");
    let loose = annotate(AdmissionHint::AllowDivByZero);
    match analyze_with_annotations(&method, &loose) {
        OffloadVerdict::Eligible(sig) => {
            assert_eq!(sig.param_kinds, vec![ParamKind::F64, ParamKind::F64]);
            assert_eq!(sig.return_kind, ParamKind::F64);
        }
        v => panic!("expected Eligible under AllowDivByZero, got {v:?}"),
    }
}

/// Other `AdmissionHint` loosenings must NOT accidentally admit
/// `frem`/`drem` — only `AllowDivByZero` does. Pins the `matches!`
/// guard in `classify` against a future accidental broadening
/// (e.g. someone generalising the guard to "any non-Strict hint").
#[test]
fn frem_still_rejected_under_unrelated_hints() {
    let method = load_method("FloatRemainder", "fremScalar", "(FF)F");
    for hint in [
        AdmissionHint::AllowAllocation,
        AdmissionHint::AllowIntrinsicCalls,
    ] {
        assert_eq!(
            analyze_with_annotations(&method, &annotate(hint)),
            OffloadVerdict::Rejected(Reason::FloatRemainder),
            "hint {hint:?} must not admit frem — only AllowDivByZero does"
        );
    }
}

/// `EligibleFrem.frem([F[F)V` (`out[i] = in[i] % 3.7f` in the
/// canonical loop) needs BOTH loosenings at once: the pool-aware
/// `ldc` resolution (for the `3.7f` literal, which has no `fconst`
/// short form) and `AllowDivByZero` (for the `frem` opcode). Under
/// `Strict` — even with a constant pool available — it must still
/// reject on the `frem`, proving the fixture is "only Eligible
/// under the gating" as intended, not incidentally eligible via
/// the `ldc` loosening alone.
#[test]
fn eligible_frem_fixture_rejected_under_strict_even_with_pool() {
    let (method, cp) = load_method_with_pool("EligibleFrem", "frem", "([F[F)V");
    assert_eq!(
        analyze_with_pool(&method, &cp),
        OffloadVerdict::Rejected(Reason::FloatRemainder)
    );
}

#[test]
fn eligible_frem_fixture_is_eligible_with_pool_and_allow_div_by_zero() {
    let (method, cp) = load_method_with_pool("EligibleFrem", "frem", "([F[F)V");
    let loose = annotate(AdmissionHint::AllowDivByZero);
    match analyze_with_annotations_and_pool(&method, &loose, &cp) {
        OffloadVerdict::Eligible(sig) => {
            assert_eq!(
                sig.param_kinds,
                vec![ParamKind::F32Array, ParamKind::F32Array]
            );
            assert_eq!(sig.return_kind, ParamKind::Void);
            assert!(sig.allow_div_by_zero);
        }
        v => panic!("expected Eligible under AllowDivByZero + pool, got {v:?}"),
    }
}

// AUDIT 2026-07-11: `lcmp`/`fcmpl`/`fcmpg`/`dcmpl`/`dcmpg` moved from
// an unconditional `Reject(Reason::Compare)` to unconditional `Ok` —
// see `classify`'s `0x94..=0x98` arm and `Reason::Compare`'s doc
// comment for the full rationale (a real, precision-free PTX
// lowering now exists; the opcodes are no longer rejected at
// analyze time, only a following non-canonical `if*` branch still
// rejects, one layer later, in `lowering/emit.rs`).
#[test]
fn compare_opcodes_are_admitted_by_analyzer() {
    for op in 0x94..=0x98 {
        match classify(op, AdmissionFlags::STRICT) {
            OpClass::Ok => {}
            OpClass::Reject(r) => {
                panic!("opcode 0x{op:02x} unexpectedly rejected with {r:?}")
            }
        }
    }
}

/// AUDIT 2026-09-21: this used `RejectAllocation.build(I)[I`,
/// which returns `[I` and is therefore now rejected on its return
/// type before the body is scanned at all — it could no longer
/// pin the allocation band. `RejectScratchAllocation.sumSquares`
/// allocates the same `iload_0; newarray int` shape and returns a
/// scalar, so allocation is the only thing left to reject it.
/// The ordering itself is pinned by
/// [`array_return_rejected_before_the_body_is_scanned`].
#[test]
fn reject_allocation() {
    let method = load_method("RejectScratchAllocation", "sumSquares", "(I)I");
    assert_eq!(
        analyze(&method),
        OffloadVerdict::Rejected(Reason::Allocation)
    );
}

/// AUDIT 2026-09-21: a primitive ARRAY return kind is rejected, and
/// rejected from the descriptor — before the bytecode scan, next to
/// the parameter-kind check it belongs with.
///
/// `RejectArrayReturn.doubleInPlace` is the shape that made this
/// necessary: it allocates NOTHING (the output buffer is the
/// parameter, handed straight back), so no hint and no annotation
/// is involved, `Strict` admitted it, and `lowering/emit.rs` then
/// refused the `areturn` with "opcode 0xb0 not implemented in
/// lowering". See [`Reason::UnsupportedReturnType`].
///
/// `RejectAllocation.build` pins the ORDER: it trips both rules —
/// `newarray` in the body and `[I` in the descriptor — and the
/// descriptor wins, because the return type is the more
/// fundamental blocker and rewriting to avoid the allocation
/// alone would not make it offloadable.
#[test]
fn array_return_rejected_before_the_body_is_scanned() {
    let no_allocation = load_method("RejectArrayReturn", "doubleInPlace", "([I)[I");
    assert_eq!(
        analyze(&no_allocation),
        OffloadVerdict::Rejected(Reason::UnsupportedReturnType),
        "an array return needs no allocation to be unlowerable"
    );

    let also_allocates = load_method("RejectAllocation", "build", "(I)[I");
    assert_eq!(
        analyze(&also_allocates),
        OffloadVerdict::Rejected(Reason::UnsupportedReturnType),
        "the descriptor check runs before the body scan"
    );
}

#[test]
fn reject_invoke() {
    let method = load_method("RejectInvoke", "outer", "([I)I");
    assert_eq!(analyze(&method), OffloadVerdict::Rejected(Reason::Invoke));
}

#[test]
fn reject_synchronized() {
    let method = load_method("RejectSynchronized", "addAll", "([I)I");
    assert_eq!(
        analyze(&method),
        OffloadVerdict::Rejected(Reason::Synchronized)
    );
}

#[test]
fn reject_ref_array() {
    let method = load_method("RejectRefArray", "sum", "([Ljava/lang/Integer;)I");
    assert_eq!(
        analyze(&method),
        OffloadVerdict::Rejected(Reason::UnsupportedParamType)
    );
}

#[test]
fn reject_switch() {
    let method = load_method("RejectSwitch", "pick", "(I)I");
    assert_eq!(analyze(&method), OffloadVerdict::Rejected(Reason::Switch));
}

#[test]
fn reject_throw() {
    let method = load_method("RejectThrow", "maybeThrow", "(I)V");
    assert_eq!(analyze(&method), OffloadVerdict::Rejected(Reason::Throw));
}

#[test]
fn reject_field_access() {
    let method = load_method("RejectFieldAccess", "read", "()I");
    assert_eq!(
        analyze(&method),
        OffloadVerdict::Rejected(Reason::FieldAccess)
    );
}

// ─── Phase 9 #2 — non-static receiver-access pattern ────────────

/// `NonStaticScale.scaleInPlace(I)V` is a non-static method whose
/// body reads `this.data` (primitive-array field) via the
/// `aload_0; getfield <data-cp>` pattern. The lowering emitter has
/// no `getfield` arm and never binds `this`, so this shape can never
/// lower; the analyzer must agree and reject it up front (rather than
/// admit it and waste an analyze→lower round-trip). See the
/// `NonStaticReceiverMisuse` handling in `scan_bytecode`.
#[test]
fn reject_non_static_this_field_pattern() {
    let method = load_method("NonStaticScale", "scaleInPlace", "(I)V");
    assert_eq!(
        analyze(&method),
        OffloadVerdict::Rejected(Reason::NonStaticReceiverMisuse),
        "non-static this-field methods must be rejected — analyzer and \
         emitter must agree, and the emitter cannot lower them",
    );
}

/// Sanity check: static methods that still use `aload_0` (to
/// load their first array parameter) keep working — Phase 9 #2
/// must not break the static path.
#[test]
fn static_methods_still_eligible_after_non_static_relax() {
    let method = load_method("EligibleVectorAdd", "vectorAdd", "([I[I[I)V");
    match analyze(&method) {
        OffloadVerdict::Eligible(sig) => {
            assert!(
                sig.this_field_cps.is_empty(),
                "static methods should have no this_field_cps, got {:?}",
                sig.this_field_cps,
            );
        }
        v => panic!("expected Eligible, got {v:?}"),
    }
}

#[test]
fn reject_type_check() {
    let method = load_method("RejectTypeCheck", "isIntArray", "([I)I");
    assert_eq!(
        analyze(&method),
        OffloadVerdict::Rejected(Reason::TypeCheck)
    );
}

// ─── annotation-driven loosening (Phase 1, §2.4) ────────────────

/// AUDIT 2026-09-21: `AllowAllocation` must NOT admit `newarray`.
///
/// This test is the inverse of the one it replaces
/// (`admit_allocation_loosens_primitive_newarray`), which pinned
/// the loosening that produced the analyzer/emitter disagreement:
/// `RejectScratchAllocation.sumSquares(I)I` opens with `iload_0;
/// newarray int`, the canonical §2.4 pattern, and the emitter has
/// no arm for 0xBC. Both hints must now reach the same verdict, so
/// the method never gets lowered and never gets blacklisted. See
/// [`Reason::Allocation`].
#[test]
fn allow_allocation_hint_does_not_admit_newarray() {
    let method = load_method("RejectScratchAllocation", "sumSquares", "(I)I");

    for hint in [AdmissionHint::Strict, AdmissionHint::AllowAllocation] {
        assert_eq!(
            analyze_with_annotations(&method, &annotate(hint)),
            OffloadVerdict::Rejected(Reason::Allocation),
            "{hint:?} must reject newarray as Allocation"
        );
    }
}

/// The `ALLOW_ALLOCATION` annotation surface still parses and
/// still reaches the analyzer — it is *accepted and inert*, not
/// removed, so already-annotated sources keep compiling. Pins the
/// bit as set-but-unread: if a future round wires the lowering it
/// has a flag to read, and until then nothing silently depends on
/// it. See `docs/gpu/annotations.md`.
#[test]
fn allow_allocation_hint_is_still_parsed_into_flags() {
    let flags = AdmissionFlags::from(AdmissionHint::AllowAllocation);
    assert!(flags.allocation, "the hint must still set its bit");
    assert!(
        !flags.unguarded_integer_division && !flags.approximate_float_remainder,
        "AllowAllocation must not set any other loosening bit"
    );
}

// ─── AUDIT 2026-07-11: intrinsic-table follow-up ─────────────────
//
// The tests below replace the old `admit_intrinsic_loosens_math_sqrt`
// PHASE1-GUESS placeholder (it asserted only "the verdict changed",
// using the unrelated `RejectInvoke` fixture, because no real
// `Math.sqrt` fixture and no CP-aware invoke resolution existed
// yet). `EligibleMathKernel.java` now gives us real
// `Math.sqrt`/`abs`/`fma`/`pow` callsites, and
// `classify_invokestatic`/`resolve_math_intrinsic` actually resolve
// the constant-pool callee instead of admitting every
// `invokestatic` blindly.

/// `sqrtAbsFma` calls `Math.sqrt`/`Math.abs`/`Math.fma` — all three
/// are in the curated table (see `resolve_math_intrinsic`) — so
/// under the pool-aware entry point with `AllowIntrinsicCalls` it
/// must be admitted `Eligible`; under `Strict`, even with the same
/// pool available, it must still reject with `Reason::Invoke`.
#[test]
fn admit_intrinsic_calls_admits_real_math_kernel() {
    let (method, cp) =
        load_method_with_pool("EligibleMathKernel", "sqrtAbsFma", "([F[F[FFFF)V");

    let strict = annotate(AdmissionHint::Strict);
    assert_eq!(
        analyze_with_annotations_and_pool(&method, &strict, &cp),
        OffloadVerdict::Rejected(Reason::Invoke),
        "Strict must still reject Math.sqrt/abs/fma calls"
    );

    let loose = annotate(AdmissionHint::AllowIntrinsicCalls);
    match analyze_with_annotations_and_pool(&method, &loose, &cp) {
        OffloadVerdict::Eligible(sig) => {
            assert_eq!(
                sig.param_kinds,
                vec![
                    ParamKind::F32Array,
                    ParamKind::F32Array,
                    ParamKind::F32Array,
                    ParamKind::F32,
                    ParamKind::F32,
                    ParamKind::F32,
                ]
            );
            assert_eq!(sig.return_kind, ParamKind::Void);
        }
        v => panic!("expected Eligible under AllowIntrinsicCalls + pool, got {v:?}"),
    }
}

/// The CP-free entry point (`analyze_with_annotations`, no constant
/// pool) can never resolve an `invokestatic` target, so even under
/// `AllowIntrinsicCalls` it must keep rejecting with
/// `Reason::Invoke`. This is the precise behavioural fix over the
/// old PHASE1-GUESS code, which admitted ANY `invokestatic` here
/// regardless of whether a pool was available to prove it safe.
/// Pins that the CP-free path never regresses back to blind
/// admission.
#[test]
fn admit_intrinsic_calls_without_pool_still_rejects_invoke() {
    let method = load_method("EligibleMathKernel", "sqrtAbsFma", "([F[F[FFFF)V");
    let loose = annotate(AdmissionHint::AllowIntrinsicCalls);
    assert_eq!(
        analyze_with_annotations(&method, &loose),
        OffloadVerdict::Rejected(Reason::Invoke),
        "AllowIntrinsicCalls with no constant pool must not blindly admit invokestatic"
    );
}

/// `Math.pow` is deliberately NOT in the curated intrinsic table
/// (PTX's `.approx` transcendentals don't meet Java's
/// relative-error contract — see `MathIntrinsic`'s doc comment).
/// Even under `AllowIntrinsicCalls` WITH a constant pool available,
/// a callsite that resolves to `Math.pow` must still reject with
/// `Reason::Invoke` — proving the hint now discriminates by callee
/// instead of admitting every `invokestatic`.
#[test]
fn admit_intrinsic_calls_still_rejects_math_pow() {
    let (method, cp) = load_method_with_pool("EligibleMathKernel", "powRejected", "([D[D)V");
    let loose = annotate(AdmissionHint::AllowIntrinsicCalls);
    assert_eq!(
        analyze_with_annotations_and_pool(&method, &loose, &cp),
        OffloadVerdict::Rejected(Reason::Invoke),
        "Math.pow must not be admitted — it is not in the curated intrinsic table"
    );
}

/// White-box coverage of [`resolve_math_intrinsic`] itself — every
/// table entry, both `java/lang/Math` and `java/lang/StrictMath`.
#[test]
fn resolve_math_intrinsic_covers_the_curated_table() {
    use MathIntrinsic::*;
    let cases: &[(&str, &str, &str, MathIntrinsic)] = &[
        ("java/lang/Math", "sqrt", "(D)D", SqrtF64),
        ("java/lang/StrictMath", "sqrt", "(D)D", SqrtF64),
        ("java/lang/Math", "abs", "(I)I", AbsI32),
        ("java/lang/StrictMath", "abs", "(I)I", AbsI32),
        ("java/lang/Math", "abs", "(J)J", AbsI64),
        ("java/lang/Math", "abs", "(F)F", AbsF32),
        ("java/lang/StrictMath", "abs", "(F)F", AbsF32),
        ("java/lang/Math", "abs", "(D)D", AbsF64),
        ("java/lang/Math", "min", "(II)I", MinI32),
        ("java/lang/Math", "max", "(II)I", MaxI32),
        ("java/lang/Math", "min", "(JJ)J", MinI64),
        ("java/lang/Math", "max", "(JJ)J", MaxI64),
        ("java/lang/Math", "min", "(FF)F", MinF32),
        ("java/lang/Math", "max", "(FF)F", MaxF32),
        ("java/lang/Math", "min", "(DD)D", MinF64),
        ("java/lang/Math", "max", "(DD)D", MaxF64),
        ("java/lang/Math", "fma", "(FFF)F", FmaF32),
        ("java/lang/StrictMath", "fma", "(FFF)F", FmaF32),
        ("java/lang/Math", "fma", "(DDD)D", FmaF64),
        ("java/lang/StrictMath", "fma", "(DDD)D", FmaF64),
    ];
    for (class_name, name, desc, expected) in cases {
        assert_eq!(
            resolve_math_intrinsic(class_name, name, desc),
            Some(*expected),
            "expected {class_name}.{name}{desc} to resolve to {expected:?}"
        );
    }
}

/// The deliberately-excluded transcendentals/unknown classes/
/// mismatched descriptors must all resolve to `None`.
#[test]
fn resolve_math_intrinsic_excludes_transcendentals_and_unknown_classes() {
    for name in ["sin", "cos", "tan", "exp", "log", "log10", "pow", "cbrt"] {
        assert_eq!(
            resolve_math_intrinsic("java/lang/Math", name, "(D)D"),
            None,
            "{name}(D)D must not be in the curated intrinsic table"
        );
    }
    assert_eq!(
        resolve_math_intrinsic("java/lang/Math", "pow", "(DD)D"),
        None,
        "pow must not be in the curated intrinsic table under any descriptor"
    );
    // Wrong class entirely.
    assert_eq!(
        resolve_math_intrinsic("com/example/Math", "sqrt", "(D)D"),
        None
    );
    // Right class/name, mismatched descriptor — must not fuzzy-match.
    assert_eq!(
        resolve_math_intrinsic("java/lang/Math", "sqrt", "(F)F"),
        None
    );
    assert_eq!(
        resolve_math_intrinsic("java/lang/Math", "min", "(DI)D"),
        None
    );
}

#[test]
fn gpu_exclude_overrides_kernel_for_direct_api_users() {
    let method = load_method("EligibleVectorAdd", "vectorAdd", "([I[I[I)V");
    let annotations = MethodAnnotations {
        gpu_kernel: Some(GpuKernelAttrs::default()),
        gpu_exclude: Some(GpuExcludeAttrs {
            reason: "test opt-out".to_string(),
        }),
    };

    assert_eq!(
        analyze_with_annotations(&method, &annotations),
        OffloadVerdict::Rejected(Reason::GpuExcluded)
    );
}

#[test]
fn allow_div_by_zero_is_carried_to_signature() {
    let method = load_method("EligibleStraightLine", "constReturn", "()I");

    match analyze(&method) {
        OffloadVerdict::Eligible(sig) => assert!(
            !sig.allow_div_by_zero,
            "strict/default analysis must keep divisor-zero guards enabled"
        ),
        v => panic!("expected strict Eligible, got {v:?}"),
    }

    let loose = annotate(AdmissionHint::AllowDivByZero);
    match analyze_with_annotations(&method, &loose) {
        OffloadVerdict::Eligible(sig) => assert!(
            sig.allow_div_by_zero,
            "AllowDivByZero must reach lowering through KernelSignature"
        ),
        v => panic!("expected AllowDivByZero Eligible, got {v:?}"),
    }
}

/// A method that is eligible under the existing `analyze` path
/// must remain eligible when called through
/// `analyze_with_annotations` with the default (no-annotations)
/// `MethodAnnotations` — i.e. the new entry-point introduces zero
/// behavioural drift for un-annotated callers.
#[test]
fn strict_default_unchanged() {
    let method = load_method("EligibleVectorAdd", "vectorAdd", "([I[I[I)V");

    let baseline = analyze(&method);
    let with_default = analyze_with_annotations(&method, &MethodAnnotations::default());

    assert_eq!(
        baseline, with_default,
        "MethodAnnotations::default() must not change any verdict"
    );
    assert!(
        matches!(baseline, OffloadVerdict::Eligible(_)),
        "control: vectorAdd must be Eligible without annotations"
    );
}

// ─── AUDIT C31 follow-up (2026-07-11): ldc/ldc_w/ldc2_w numeric
// ─── literal admission ──────────────────────────────────────────
//
// Before this fix, ANY int literal outside sipush range
// (`|c| > 32767`) or ANY long/float/double literal made an
// otherwise-perfect element-wise kernel ineligible — the single
// most common real-world eligibility killer. `analyze_with_pool` /
// `analyze_with_annotations_and_pool` now resolve the CP entry and
// admit `ldc`/`ldc_w` of `Integer`/`Float`, and `ldc2_w` of
// `Long`/`Double`. The CP-free `analyze` / `analyze_with_annotations`
// entry points are unchanged: they still reject every ldc form,
// since they have no constant pool to resolve against.

#[test]
fn ldc_int_literal_is_eligible_with_pool() {
    let (method, cp) = load_method_with_pool("EligibleLdcInt", "scale", "([I[I)V");
    match analyze_with_pool(&method, &cp) {
        OffloadVerdict::Eligible(sig) => {
            assert_eq!(
                sig.param_kinds,
                vec![ParamKind::I32Array, ParamKind::I32Array]
            );
            assert_eq!(sig.return_kind, ParamKind::Void);
        }
        v => panic!("expected Eligible, got {v:?}"),
    }
}

#[test]
fn ldc_int_literal_still_rejected_without_pool() {
    // Same fixture, but through the CP-free entry point: it cannot
    // resolve the `ldc` target, so it must keep rejecting exactly
    // like it did before this audit.
    let method = load_method("EligibleLdcInt", "scale", "([I[I)V");
    assert_eq!(
        analyze(&method),
        OffloadVerdict::Rejected(Reason::LoadConstant)
    );
}

#[test]
fn ldc2_w_long_literal_is_eligible_with_pool() {
    let (method, cp) = load_method_with_pool("EligibleLdcLong", "mix", "([J[J)V");
    match analyze_with_pool(&method, &cp) {
        OffloadVerdict::Eligible(sig) => {
            assert_eq!(
                sig.param_kinds,
                vec![ParamKind::I64Array, ParamKind::I64Array]
            );
            assert_eq!(sig.return_kind, ParamKind::Void);
        }
        v => panic!("expected Eligible, got {v:?}"),
    }
}

#[test]
fn ldc_float_literal_is_eligible_with_pool() {
    let (method, cp) = load_method_with_pool("EligibleLdcFloat", "fma", "([F[F)V");
    match analyze_with_pool(&method, &cp) {
        OffloadVerdict::Eligible(sig) => {
            assert_eq!(
                sig.param_kinds,
                vec![ParamKind::F32Array, ParamKind::F32Array]
            );
            assert_eq!(sig.return_kind, ParamKind::Void);
        }
        v => panic!("expected Eligible, got {v:?}"),
    }
}

#[test]
fn ldc2_w_double_literal_is_eligible_with_pool() {
    let (method, cp) = load_method_with_pool("EligibleLdcDouble", "fma", "([D[D)V");
    match analyze_with_pool(&method, &cp) {
        OffloadVerdict::Eligible(sig) => {
            assert_eq!(
                sig.param_kinds,
                vec![ParamKind::F64Array, ParamKind::F64Array]
            );
            assert_eq!(sig.return_kind, ParamKind::Void);
        }
        v => panic!("expected Eligible, got {v:?}"),
    }
}

/// A `String` constant-pool entry must never be admitted — not even
/// once the pool is available — because a `java.lang.String` has no
/// GPU-representable immediate form. `RejectLdcString.noop()`'s body
/// is exactly `ldc #<String>; astore_0; return`, so the `ldc` is the
/// very first instruction the scan sees: whichever reason fires, it
/// is unambiguously the ldc classification and not, say, the
/// `astore` of a reference-typed local.
#[test]
fn ldc_string_is_rejected_even_with_pool() {
    let (method, cp) = load_method_with_pool("RejectLdcString", "noop", "()V");
    assert_eq!(
        analyze_with_pool(&method, &cp),
        OffloadVerdict::Rejected(Reason::LoadConstant),
        "a String CP entry must stay rejected even when the pool is available"
    );
    // And the CP-free path rejects it too, for the same reason
    // (just less precisely — it can't tell WHAT the entry is).
    assert_eq!(
        analyze(&method),
        OffloadVerdict::Rejected(Reason::LoadConstant)
    );
}

// ─── reduction dataflow helpers (review jit-cuda-review.md §1 MED) ──
//
// These exercise the slot-decoding helpers that back the
// accumulator-feeds-return proof directly, without needing a Java
// fixture (the `test_classes/gpu/` sources live outside this crate's
// owned-file scope). The end-to-end behaviour — a genuine
// `return acc` reduction stays `is_reduction`, a `return a[0]`
// shape-match does not — is covered by `eligible_dot_product_returns_long`
// plus a future `ReductionFalsePositive` fixture noted in the review.

#[test]
fn load_slot_decodes_short_and_wide_forms() {
    // iload_0..=iload_3 (0x1A..=0x1D).
    assert_eq!(load_slot(&[0x1A], 0), Some(0));
    assert_eq!(load_slot(&[0x1D], 0), Some(3));
    // lload_2 (0x20), fload_1 (0x23), dload_3 (0x29).
    assert_eq!(load_slot(&[0x20], 0), Some(2));
    assert_eq!(load_slot(&[0x23], 0), Some(1));
    assert_eq!(load_slot(&[0x29], 0), Some(3));
    // Two-byte `iload <index>` (0x15) / `dload <index>` (0x18).
    assert_eq!(load_slot(&[0x15, 7], 0), Some(7));
    assert_eq!(load_slot(&[0x18, 42], 0), Some(42));
    // Non-load opcodes and reference loads decode to None — an
    // `aload` (0x2A / 0x19) must NOT be mistaken for a scalar
    // accumulator load.
    assert_eq!(load_slot(&[0x2A], 0), None); // aload_0
    assert_eq!(load_slot(&[0x19, 0], 0), None); // aload <index>
    assert_eq!(load_slot(&[0x60], 0), None); // iadd
                                             // Truncated two-byte operand decodes to None rather than panicking.
    assert_eq!(load_slot(&[0x15], 0), None);
}

#[test]
fn store_slot_decodes_short_and_wide_forms() {
    // istore_0..=istore_3 (0x3B..=0x3E).
    assert_eq!(store_slot(&[0x3B], 0), Some(0));
    assert_eq!(store_slot(&[0x3E], 0), Some(3));
    // lstore_1 (0x40), fstore_2 (0x45), dstore_0 (0x47).
    assert_eq!(store_slot(&[0x40], 0), Some(1));
    assert_eq!(store_slot(&[0x45], 0), Some(2));
    assert_eq!(store_slot(&[0x47], 0), Some(0));
    // Two-byte `istore <index>` (0x36).
    assert_eq!(store_slot(&[0x36, 9], 0), Some(9));
    // astore (reference store) must not decode as a scalar store.
    assert_eq!(store_slot(&[0x4B], 0), None); // astore_0
    assert_eq!(store_slot(&[0x3A, 0], 0), None); // astore <index>
    assert_eq!(store_slot(&[0x36], 0), None); // truncated -> None
}

#[test]
fn is_load_op_excludes_reference_loads() {
    assert!(is_load_op(0x15)); // iload
    assert!(is_load_op(0x1A)); // iload_0
    assert!(is_load_op(0x29)); // dload_3
                               // aload family is intentionally excluded.
    assert!(!is_load_op(0x19)); // aload
    assert!(!is_load_op(0x2A)); // aload_0
    assert!(!is_load_op(0x2D)); // aload_3
    assert!(!is_load_op(0xAC)); // ireturn
}

#[test]
fn reduction_tracker_keeps_genuine_accumulation() {
    // lload_2; lload_0; ladd; lstore_2; lload_2; lreturn
    let tracker = track_reduction_ops(&[0x20, 0x1E, 0x61, 0x41, 0x20, 0xAD]);
    assert!(
        tracker.accumulator_feeds_return(),
        "load/add/store of the returned slot must still prove a reduction"
    );
}

#[test]
fn reduction_tracker_clears_candidates_on_pop() {
    // lload_2; pop2; lload_0; lload_1; ladd; lstore_2; lload_2; lreturn
    //
    // Without clearing the loaded-slot window at pop2, the later
    // unrelated ladd would inherit stale slot 2 and falsely prove
    // `lstore_2` as an accumulator update.
    let tracker = track_reduction_ops(&[0x20, 0x58, 0x1E, 0x1F, 0x61, 0x41, 0x20, 0xAD]);
    assert!(
        !tracker.accumulator_feeds_return(),
        "pop2 must clear stale reduction candidates"
    );
}

#[test]
fn reduction_tracker_clears_proven_candidate_on_late_pop() {
    // lload_2; lload_0; lload_1; ladd; lstore_2; pop2; lload_2; lreturn
    //
    // The lstore_2 initially looks like an accumulator update because
    // slot 2 was loaded earlier, but the following pop2 proves that
    // old value was still on the stack and did not feed the ladd.
    let tracker = track_reduction_ops(&[0x20, 0x1E, 0x1F, 0x61, 0x41, 0x58, 0x20, 0xAD]);
    assert!(
        !tracker.accumulator_feeds_return(),
        "late pop2 must clear a falsely proven accumulator candidate"
    );
}

#[test]
fn reduction_tracker_drops_overwritten_accumulator_slot() {
    // lload_2; lload_0; ladd; lstore_2; lconst_0; lstore_2; lload_2; lreturn
    let tracker = track_reduction_ops(&[0x20, 0x1E, 0x61, 0x41, 0x09, 0x41, 0x20, 0xAD]);
    assert!(
        !tracker.accumulator_feeds_return(),
        "a non-accumulating store must invalidate the earlier accumulator proof"
    );
}
