# Profile-guided inlining

C2-review P1 — "Add direct-call and type-profile inlining" / "Profile-guided
inlining". Consolidated from `docs/known-issues/c2/pgo-01-call-site-evidence-gap.md`
and `pgo-02-guarded-inlining.md` (both retired 2026-08-03) plus the design
doc this document supersedes (`docs/jit/profile-guided-inlining.md`, written
2026-07-31 before either lane shipped). One doc for the whole feature going
forward — states the policy, what evidence feeds it, what the backend can
now emit, the deopt-safety argument, and what is still open.

**Status 2026-08-03: PGO-01 and PGO-02's first increments are both shipped.**
Interpreted `invokestatic`/`invokespecial` feed `MethodProfile::call_sites`
(PGO-01). A monomorphic `invokevirtual`/`invokeinterface` site can be
speculatively inlined behind a receiver class-id guard, falling back to
normal dispatch on a miss — never a deopt — when
`CRATONVM_JIT_GUARDED_VIRTUAL_INLINE` is set (PGO-02, default-OFF, unsoaked).
Both verified end to end through real JIT compiles, not just unit tests of
the policy in isolation. `pgo-02`'s own "first increment" scope (monomorphic
only, no deopt) is complete; Bimorphic guards, a full deopt-capable
speculation, and the metrics/JFR wiring in §6 remain open — see §8.

---

## 1. What existed before either lane

`try_compile_inner`'s invoke loop asked exactly one question per candidate:

```rust
inline_site_expansion_cost_tiered(&site, site_hot)
    .filter(|cost| *cost <= inline_budget_remaining)
```

That is the HotSpot three-tier size model (`MaxTrivialSize` 6 / `MaxInlineSize`
35 / `FreqInlineSize` 325, with expansion ceilings 64 / 512 and a per-method
budget of 750 cold / 2000 hot). It is a good size model. It was also the whole
policy: no depth accounting, no recursion accounting, no receiver-type profile,
no record of why a candidate was refused, and no dependency beyond the callee's
own class name. The candidate set was `invokestatic` and `invokespecial`
only — virtual and interface sites were never considered.

Separately, `MethodProfile::record_call_site`/`record_call_site_borrowed`
(`jit/src/profile.rs`) existed with **zero callers anywhere in `vm/` or
`jit/`** — `CallSiteEvidence::Direct` was unreachable outside tests, and
there was no per-call-site hotness evidence for `invokestatic`/
`invokespecial` at all (the receiver-type profile covers virtual/interface
only). The inlining policy below was reading a data source a real run never
populated for that whole class of call sites.

## 2. The policy

`plan_inline(&InlineRequest) -> InlinePlan` is a pure function. It resolves
nothing, allocates no code and mutates nothing; it takes evidence the caller
already has and returns a verdict, a price, and a dependency list.

### Budgets and limits

| Knob | Value | Source |
|---|---|---|
| `MAX_TRIVIAL_INLINE_SIZE` | 6 | pre-existing (`MaxTrivialSize`) |
| `MAX_INLINE_SIZE_COLD` | 35 | pre-existing (`MaxInlineSize`) |
| `MAX_INLINE_BYTECODE_SIZE` | 325 | pre-existing (`FreqInlineSize`) |
| `MAX_INLINE_EXPANSION_COST` / `_HOT` | 64 / 512 | pre-existing |
| `MAX_INLINE_BUDGET` / `_HOT` | 750 / 2000 | pre-existing |
| `INLINE_MAX_DEPTH` | 9 | `MaxInlineLevel` |
| `INLINE_MAX_RECURSIVE_DEPTH` | 1 | `MaxRecursiveInlineLevel` |
| `INLINE_MIN_SPECULATION_OBSERVATIONS` | 250 | half `INLINE_HOT_SITE_OBSERVATIONS` |
| `INLINE_MONOMORPHIC_SHARE_PCT` | 90 | |
| `INLINE_BIMORPHIC_SHARE_PCT` | 92 | |
| `INLINE_MEGAMORPHIC_TYPE_CEILING` | 8 | twice the PIC's four ways |

`INLINE_MAX_RECURSIVE_DEPTH` counts **ancestors on the inline stack**, not
sibling copies. A leaf called five times from one caller is five independent
sites, each paying the per-method budget; that is not recursion and must keep
inlining.

### Receiver-shape classification

`classify_receiver_shape` reads `profile::MethodProfile::receivers` — the map
the interpreter feeds via `ProfileStore::record_receiver_borrowed`, for
`invokevirtual` and `invokeinterface`. Ranking is descending count, ties
broken by ascending class id (`FxHashMap` iteration order is not
deterministic, and two compiles of the same profile must produce the same
artifact).

- `Unprofiled` — no observations. Distinct from "many types", the same way
  `CallSiteEvidence::None` is distinct from a count of zero.
- `Cold` — under 250 observations. A site executed twice with one receiver is
  not monomorphic, it is unproven.
- `Monomorphic` — top type ≥ 90%. A tail is allowed: the guard routes it to
  dispatch.
- `Bimorphic` — top two ≥ 92% combined. Higher bar because the site pays two
  guards.
- `Megamorphic` — more than 8 distinct types (a dispatch hub, refused however
  dominant its top type looks), or no sufficiently dominant type. The two are
  reported apart (`megamorphic` vs `receiver-not-dominant`).

### Call-site evidence for static/special sites — PGO-01

`invokestatic`/`invokespecial` have no receiver to profile, so
`classify_receiver_shape` doesn't apply to them; `MethodProfile::call_sites`
(execution count keyed by bci, kind-agnostic) is what tells a hot static call
from a cold one inside the same method. As of 2026-08-03 it is fed from
exactly four places in `vm/src/runtime/interpreter/invoke.rs`:

- `execute_invokestatic` and `execute_invokestatic_cached` — every
  `invokestatic`, slow-path and cached-fast-path.
- `execute_invoke_kind` and `execute_invokevirtual_cached`, both gated on
  `is_special` — every `invokespecial`. These two functions are ALSO the
  dispatch path for `invokevirtual`/`invokeinterface`, deliberately NOT
  recorded there (already covered by `receivers`; recording both would
  double-count one call site against two evidence sources).

Recording is placed once per function, right after resolution succeeds and
past every early give-up/eviction/cache-miss return — not scattered across
every one of the many downstream successful-dispatch branches (native, JIT,
bytecode-push, lambda proxy…) each of these functions has. This is
explicitly a heuristic hotness signal for the inliner, not a correctness
input: the placement can over-count by one in a narrow redefinition-eviction
race, an acceptable trade against hunting down every branch in a
500–2000-line function and risking missing one — undercounting, not
overcounting, was the failure mode found in production (the recorder had
zero callers for an entire wave).

Verified with `vm/tests/pgo01_call_site_evidence.rs` — positive controls
(static-call loop, constructor-call loop) and negative controls
(virtual-only method, profiling disabled), all against a real interpreted
run, not just `profile.rs`'s own unit tests (which call `record_call_site`
directly and could never have caught the zero-callers gap in the first
place).

**Trap found building that test, worth keeping**: a same-class **private
instance method** call does **not** compile to `invokespecial` on a modern
(JDK 11+) javac — confirmed with `javap`, it's `invokevirtual` (JEP 181,
nestmate access control). A guaranteed-`invokespecial` Java fixture needs
`new Foo()`'s `<init>` call or an explicit `super.foo()`, not a private
method call — the pre-11 JVMS intuition is stale.

### Verdicts

- `DirectBind` — statically bound callee (`invokestatic`/`invokespecial`). One
  possible target, no guard, no speculation.
- `Monomorphic { guard_class_id }` — splice the body behind
  `CMP DWORD [recv+0], guard_class_id`, miss edge falls through to normal
  dispatch. **Backend now emits this — PGO-02, see §5.**
  `INLINE_MONOMORPHIC_SHARE_PCT` (top type ≥ 90%) is the "narrowest
  speculation that is worth anything" the pgo-02 doc asked for as the first
  increment; that is the ONLY verdict the backend emits today.
- `Bimorphic { guard_class_ids }` — two guarded bodies in descending profile
  order, second miss edge falls through to dispatch. **Backend does not emit
  this yet** — see §8. `plan_inline` still admits it (the policy is
  unchanged); the invoke loop in `try_compile_inner` deliberately withholds
  the side effects (`inline_sites`/dependency recording) from a Bimorphic
  verdict specifically because there is nowhere to emit it, so an admitted-
  but-unactionable plan never silently claims a splice that never happens.
- `Refuse(InlineRefusal)` — thirteen named reasons, each with a stable
  `category()` string for metrics keys and log greps.

### Exception-path policy

Three rules, in precedence order:

1. **Precise exception frames ⇒ nothing inlines.** Mirrors the unconditional
   `inline_sites.clear()` `try_compile_inner` already performs.
2. **A speculative site inside a protected range is refused.** The guard's
   miss edge is a new control-flow edge in the middle of a `try` block, and
   the inlined body publishes no exceptional frame of its own.
3. **A statically bound site inside a protected range is unchanged** — it
   inlines exactly as it did before this policy existed.

### Fail-closed rules

No speculative inline is admitted without **both**:

- a receiver class-id guard the backend can actually emit
  (`InlineBackendCaps::guarded_inline_body_at_virtual_sites`), and
- a recorded invalidation dependency naming the speculated receiver class
  (needs a working `receiver_class_namer`).

Either missing ⇒ `GuardNotEmittable` or `NoInvalidationDependency`. Both are
refusals, never warnings.

## 3. Deopt safety with no caller scopes

`deopt::FrameState::caller` exists but **no production site populates it** —
every construction in `ir_lower.rs` and `x64.rs` passes `caller: None`. Inlined
scopes are therefore not representable in deopt metadata. An inline that
needed to describe "we are inside callee C, called from caller M at bci N"
would publish a frame naming M with C's bci — a silent wrong answer.

**Both PGO-01 and PGO-02 rely on the absence of deopt points inside inlined
bodies, not on caller scopes.** The argument, verified against the source and
unchanged by PGO-02 landing:

- `x64::try_emit_inline` (the shared splice machinery both the pre-existing
  invokespecial DirectBind path and PGO-02's new guarded-virtual path call)
  contains no `snapshot_pre_intrinsic_call`, no `build_and_record_deopt_point`
  and no `deopt_stubs` push in the callee-replay body. Every callee operation
  it cannot emit without one is a `return false`, and the wrapper rolls back
  the buffer, the operand stack, the oop marks, the spill offset and all
  seven deferred patch-lists, then falls through to a real call.
- The guard PGO-02 adds in front of `try_emit_inline` is a plain
  `CMP`/conditional-jump pair with **no deopt stub of its own** — a mismatch
  falls through to the pre-existing normal-dispatch code (MIC/PIC/
  `jit_invoke_dispatch`), never traps. That is the literal reason the
  "falls back to the existing dispatch on mismatch — not to a deopt" framing
  in the original ask exists: a falling-through guard needs no frame at all,
  sidestepping the caller-scope gap entirely rather than requiring it to be
  fixed first.

Consequence, stated plainly: an inlined body (static/special DirectBind or a
guarded virtual/interface Monomorphic splice) is entered and left within one
frame described as the caller's own, and no deopt point inside it is
described at all. There is no metadata a missing caller scope could make
wrong.

**Hard precondition for any future work.** If the emitter learns to publish a
deopt point inside an inlined callee — an inlined bounds check, an inlined
null check, an inlined guard that traps rather than falling through — this
policy must refuse that shape until `FrameState::caller` is populated by the
producer.

## 4. Dependencies and how invalidation reaches the code

The channel is the existing one. `InlinePlan::invalidation_triples()` flattens
to `(class, method, descriptor)` triples, written into
`CompiledMethod::inlined_methods` (deduplicated).

Two dependency kinds:

- `InlinedCallee` — the spliced body. Recorded for every admitted plan whose
  side effects are actually applied (DirectBind, and Monomorphic since
  PGO-02 — NOT Bimorphic, per §2/§8).
- `SpeculatedReceiver` — the class a guarded site speculated on. Recorded for
  every guard id of every actionable speculative plan.

Reach, in the VM: class define / redefine / unload all funnel into
`JitCache::invalidate_matching` via `vm/src/vm/vm_init.rs`'s `load_class` and
the class-manager's redefinition/unload paths, which retargets inline caches,
republishes shard snapshots, and hands the executable allocations to the
epoch/quiescence reclaim path.

### Known coarseness — asserted, not assumed

The VM walks the loaded class and its **direct** superclass only. A
dependency on `A` is therefore *not* reached when `C extends B extends A` is
loaded. This is survivable because **correctness does not rest on the
dependency**: an exact receiver class-id guard is not a CHA assumption — a
`C` receiver fails the `CMP` and takes the dispatch path regardless. The
dependency is a *retirement* obligation only — without it, the caller keeps
paying a guard that now always misses, with no event to trigger a recompile.
`speculative_inline_records_a_dependency_a_class_load_invalidates` asserts
the gap explicitly.

### The stronger channel that exists but is not wired

`deopt::InvalidationManager` already models this properly —
`CompilationAssumption::StableType { bci, expected_class }` is exactly a
guarded receiver speculation — but is not usable from `try_compile_inner`
today: the manager lives behind `vm/src/vm/realms/jit_realm.rs`'s mutex and
is not threaded into the compiler, and `InvalidationManager::on_class_loaded`
has no reverse index for `StableType`. Still open — see §8.

## 5. What the backend can do — updated 2026-08-03

**`invokestatic`/`invokespecial` DirectBind splicing** existed before either
lane (`x64.rs`'s `invokestatic` and `invokespecial`-only arms consult
`inline_sites`).

**Guarded Monomorphic virtual/interface splicing is now real (PGO-02).**
`jit/src/x64.rs`'s `0xb6 | 0xb7 | 0xb9` arm, between the pre-existing
invokespecial-only inline check and the intrinsic-ladder/direct-call/
dispatch-helper block, now:

1. For `op == 0xb6 || op == 0xb9` with an entry in BOTH `self.inline_sites`
   and the new `self.inline_guard_class_ids` map (populated together, only
   for an admitted `Monomorphic` verdict — see §2), peeks the receiver
   (deepest of `callee_num_args` operands) into a register **without
   popping** it.
2. Emits `TEST reg,reg` (null → miss) then `CMP DWORD [reg+0], guard_class_id`
   (class mismatch → miss) — the identical encoding the pre-existing String/
   CRC32 intrinsic guards already use.
3. On the guard-pass path, calls the SAME `try_emit_inline(pc)` the
   invokespecial DirectBind arm already uses. On success, snapshots-then-
   restores the compiler's symbolic operand-stack/oop-mark/spill state to
   its pre-guard values (the emitted BYTES for the spliced body are kept;
   only the compile-time model is rewound) so the immediately-following,
   UNCHANGED normal-dispatch code — which runs unconditionally as this arm's
   only Rust-level continuation — pops the same receiver+args positions and
   pushes a canonically-shaped result regardless of which machine-code path
   a given execution actually takes at runtime. Both paths converge on the
   same `push_from_rax`/`push_from_rax_as_xmm0` convention by construction,
   which is what makes this reconciliation sound rather than merely
   convenient.
4. On a `try_emit_inline` failure (rare — buffer overflow, an unsupported
   callee construct), rewinds the guard bytes too (same 7-deferred-patch-list
   checkpoint set `try_emit_inline` itself uses), so the fall-through is
   byte-identical to never having attempted the guard.
5. A guard-pass-but-splice-fails miss, and every ordinary guard miss, lands
   at the exact start of the pre-existing normal-dispatch code (MIC/PIC/
   `jit_invoke_dispatch`) — unmodified. A guard hit jumps PAST that code
   entirely.

`InlineBackendCaps::single_pass_x64()`'s `guarded_inline_body_at_virtual_sites`
is now `class_id_name_resolver.is_some()` at the `plan_inline` call site
(`jit/src/lib.rs`), and that resolver is `Some` only when
`CRATONVM_JIT_GUARDED_VIRTUAL_INLINE` is set — the flag is the ONLY thing
gating the whole feature; the codegen above costs one `HashMap` probe per
virtual/interface site when off (both maps are always empty then).

**Bimorphic virtual/interface splicing is NOT emitted.** `plan_inline` can
still admit a Bimorphic verdict (untouched policy), but `try_compile_inner`
withholds `inline_sites`/dependency recording for one (see §2) and the
backend has no two-guard lowering. See §8.

**`InlineBackendCaps::unrestricted()`** remains what it was: a caps value no
production caller passes, kept so the policy's speculative arm stays
testable ahead of backend support (now partially — Monomorphic only — caught
up to it).

## 6. Verification

`vm/tests/pgo02_guarded_virtual_inline.rs` +
`vm/tests/resources/cratonvm/PgoGuardedVirtualInline.java`, driven by
REPEATED INVOCATIONS from the test (not an internal Java loop — invocation-
count tiering is the code path this lane touches; an internal loop would
need OSR, a different, untouched compile path):

- **Guard-hit** (`callA`, monomorphic-A the whole run): correct results past
  the compile threshold, AND `CompiledMethod::inline_tally.speculative_sites
  >= 1` fetched via `shared.jit.jit_cache` — proof the new code path actually
  ran, not just that results happened to be correct (which normal dispatch
  alone would also produce).
- **Guard-miss** (`callCurrent`): builds a monomorphic-A profile (baking an
  A-guarded compile), then switches the receiver to three OTHER concrete
  classes without recompiling — every one of those calls must dispatch to
  the ACTUAL runtime class, not silently run A's already-inlined body. This
  is the single most safety-critical check given the risk profile.
- **Exception through a compiled, guard-eligible call site** (`callThrowerCaught`):
  the callee throws, caught internally — verifies control flow through a
  compiled call site is unchanged. (An *uncaught* exception propagating out
  of an inlined frame is a documented gap, not tested — see §8; it needs
  deopt-metadata machinery this increment does not touch, per §3.)
- **Polymorphic** (`callPoly`, 4 types, none dominant): must never crash or
  mis-dispatch regardless of whether `plan_inline` classifies it Megamorphic
  or ReceiverNotDominant.

Regression evidence (2026-08-03, flag off AND on for every suite):
`interpreter_tests.rs` 924/924, jit crate's full suite (lib + `differential`
+ every `intrinsic_*` + `ir_vs_singlepass`) 2013/2013, full `cratonvm-vm`
test binary set 2378/2379 lib + all integration tests (the one flaky failure,
a JNI thread-identity-census timing test, reproduces identically in isolation
with zero relation to this change — confirmed before and after). One cluster
of 4 pre-existing failures in `jdk_only_config.rs` (JDK-only mode violation
reporting/JSON serialization, an unrelated subsystem) reproduces byte-
identically with the new flag on OR off — not caused by this change, not
investigated further here.

## 7. Metrics wiring — NOT done this pass

`CompiledMethod::inline_tally` (an `InlineDecisionTally`) is published on
every artifact and already carries `candidates` / `inlined_sites` /
`speculative_sites` / `inlined_bytecodes` / `expansion_cost` /
`observed_calls_inlined` / `refusals`. Nothing in `jit/src/metrics.rs`
(`CompilationReport`, `CompileRecorder::installed`, `to_json`) harvests it
yet — still the exact gap the original design doc described, unchanged by
either PGO-01 or PGO-02. A real follow-up, not attempted here; the
verification in §6 reads the tally directly from `CompiledMethod` instead of
through the metrics/JFR surface.

## 8. To reconcile — what's left open

1. **Bimorphic guarded splicing.** The policy already produces the verdict;
   the backend has no two-guard lowering. Same architecture as §5's
   Monomorphic emitter, doubled: two `CMP`+miss-chain pairs, two candidate
   `try_emit_inline` bodies, one shared miss/dispatch tail.
2. **A guard that deopts instead of falling through**, once `FrameState::caller`
   is populated (§3's hard precondition) — the pgo-02 doc's own deliberately
   deferred "only after that, and only if the metadata is shown total"
   next step. Nothing here started that work.
3. **`StableType` in `InvalidationManager`** has no reverse index and no
   `on_class_loaded` query. Wiring it would close the grandchild gap in §4
   and give speculations a retirement path that does not depend on the
   coarse direct-superclass-only name scan.
4. **The metrics/JFR harvest in §7.**
5. **The tally undercounts candidates once the budget is exhausted**, because
   the invoke loop still short-circuits on `inline_budget_remaining > 0`
   before calling the resolver.
6. **`jit/src/pgo.rs` remains unwired.** Nothing here reads it. If it is ever
   given a recorder, `ReceiverTypeProfile::is_monomorphic` and
   `InliningPolicy::should_inline` should be deleted in favour of
   `classify_receiver_shape` / `plan_inline` rather than kept as a second,
   divergent policy.
7. **An uncaught exception propagating out of a guard-hit inlined frame** is
   untested (§6) — the same deopt-metadata gap as item 2 blocks a real test
   of it, since there is no frame to describe the inlined scope if execution
   needs to resume the interpreter mid-callee.
