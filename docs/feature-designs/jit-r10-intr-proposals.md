# JIT round 10, lane `intr` — proposals

Findings from a read-only-plus-doc-fix audit of the intrinsic catalogue,
direct-helper table, and SIMD emission owned by this lane
(`jit/src/intrinsic_catalogue.rs`, `jit/src/runtime_lowering.rs`,
`jit/src/direct_helpers.rs`, `jit/src/x64/simd.rs`,
`jit/src/x64/simd_analysis.rs`, `jit/src/x64/vec_emit.rs`,
`jit/src/x64/cpu_features.rs`). Two directions worth doing on a future round;
neither is a bug fix, both are net-new mechanism.

## 1. Retire or finish `JitIntrinsic::ScopedMemoryGet*Unaligned`

**What:** four enum variants (`ScopedMemoryGetShortUnaligned` /
`GetCharUnaligned` / `GetIntUnaligned` / `GetLongUnaligned`) have existed since
this file's SCOPED_MEMORY_UNALIGNED region was written, are never registered
by any matcher, and have no emitter arm — see
`docs/known-issues/jit/r10-intr-scoped-memory-unaligned-dead-variants-20260920.md`
for the full trace. The feature they were meant to provide is already served,
completely separately, by `direct_helpers::ScopedMemoryOp` +
`DirectHelperTable`.

**Mechanism to retire them:** a lane that owns `intrinsic_catalogue.rs` alone
for a full round (no concurrent editor of files matching on `JitIntrinsic`
variants) deletes the four variants and their region markers, runs a full
build, and confirms nothing outside this file named them (this audit found
none, but a build is the only real confirmation). Cost: near zero — the
variant set shift is safe because every consumer recomputes `.as_entry()` from
the enum rather than hard-coding a discriminant, per the type's own
documented invariant.

**Mechanism to finish them instead:** would duplicate
`direct_helpers::ScopedMemoryOp`, which already has the ABI checklist
(`DIRECT_HELPER_FN_SIGS`, the win64/sysv stack-argument census, the
hand-written vs. marshalled path distinction) this new work would need to
rebuild from scratch. Not recommended — there is no benefit to a second
mechanism for the same four JDK methods, and the `Put*` half was never even
started on this side, so "finishing" is actually "building the majority of a
new feature."

**Recommendation:** retire. Low cost, removes stale enum surface, and the
`known-issues` page this round leaves behind gives the next lane the exact
diff to make.

## 2. Wire `VecPlan`/`emit_vector_loop` into `x64.rs`, with an AVX-vs-AVX2 split

**What:** `jit/src/x64/simd_analysis.rs`'s `admit_vectorization` gate and
`jit/src/x64/vec_emit.rs`'s `emit_vector_loop` are a complete, well-tested
general loop-vectorization pipeline — guards, alignment, tail strategy,
register-pool allocation, a reduction epilogue — that nothing calls yet
(`emit_vector_loop`'s own doc: "deliberately not wired into `x64.rs`: the call
site is added separately"). The production SIMD path today
(`jit/src/x64/simd.rs`) is a much narrower hand-written set of whole-loop
replacements (int-array sum, element-wise, matrix dot, bulk byte fill/sieve),
each recognising one exact bytecode shape.

**Gap found in the unwired pipeline that the wiring lane should close as it
goes:** `VectorIsa::detect()` offers `sse41()`/`sse2()` (128-bit) targets when
the host lacks AVX2, but `emit_vector_loop` refuses unconditionally without
AVX2 (`HostVectorSupport` only tracks one bit). Every encoding
`emit_vector_loop` writes today is VEX; 128-bit VEX-encoded integer
instructions (`VPADDD xmm`, `VMOVDQU xmm`, etc.) are legitimately AVX
instructions — they do not need AVX2, only the 256-bit (`VEX.256`) integer
forms do. See
`docs/known-issues/jit/r10-intr-vecplan-non-avx2-isa-branch-is-unreachable-20260920.md`.

**Mechanism:**
1. Add `has_avx()` to `cpu_features.rs` (CPUID.1:ECX bit 28 + OSXSAVE +
   `XGETBV`, i.e. the AVX half of the existing `detect_avx2` check without the
   leaf-7 AVX2 bit) and an `avx: bool` field alongside `avx2` on
   `HostVectorSupport`.
2. Give `emit_vector_loop` two encoding modes selected by `plan.width_bytes`:
   32 still requires `host.avx2`, 16 requires `host.avx` (which every AVX2
   host also has, so no regression for the existing width).
3. Wire one `x64.rs` call site: after the existing hand-written detectors in
   `simd.rs` decline a loop (they are narrower and should stay first, since
   they also drive `single_pass_only`'s IR-tier veto), try
   `admit_vectorization` / `emit_vector_loop` for the general case.
4. Decide, and write down, how the two pipelines' de-specialization vetoes
   compose — `single_pass_only.rs`'s `has_avx2()` veto currently assumes the
   only live vector transforms need AVX2; a 128-bit AVX-only plan would need
   its own veto predicate.

**Cost:** this is a multi-day feature, not a bug fix — `cpu_features.rs` is
small and low-risk, but step 3/4 touches `x64.rs` and `single_pass_only.rs`,
neither owned by this lane, and needs a real differential-testing pass (the
`ir_vs_singlepass` / `differential` test suites already in `jit/tests/` are
the right harness) before it can be trusted on real workloads. The payoff is
vectorizing element-wise/reduction loops on the (now rare, but not zero)
AVX-without-AVX2 hosts, and — more valuably — giving the general gate a
second, narrower width to fall back to instead of an all-or-nothing AVX2
requirement, which is useful once AVX-512 masked tails are considered (a
three-tier width ladder is the natural extension point;
`VectorIsa::masked_tail` already exists as a field for exactly that, unused).

## 3. (Smaller) Give `HostVectorSupport`/`VecEmitPolicy` a real production
   caller for `simd_sum_forms_enabled`-style measurement

Not a code change, a process note: `simd_sum_forms_enabled()`
(`simd_analysis.rs`) and `VecEmitPolicy::from_flags` (`vec_emit.rs`) read the
*same* environment key (`super::vec_emit::VECTORIZE_FLAG`) with different
default-on/off rules, documented as deliberate ("this answer does not depend
on it") because the second reader has no production caller yet. Once proposal
2 lands and `VecEmitPolicy` gets a real caller, this divergence needs a single
paragraph explaining why the same flag means two different defaults in two
places, or the two should be unified — worth flagging for whoever does the
wiring so it is not rediscovered as a surprise.
