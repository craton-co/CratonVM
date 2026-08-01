# Profile-guided inlining

C2-review P1 — "Add direct-call and type-profile inlining" / "Profile-guided
inlining".

This document states the inlining policy that now lives in `jit/src/lib.rs`
(`plan_inline` and the types around it), the deopt-safety argument it rests on,
the dependencies it records, and the two things it needs from files this change
does not own.

---

## 1. What existed before

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
own class name.

The candidate set was `invokestatic` and `invokespecial` only. Virtual and
interface sites were never considered — the profile-guided half of the task did
not exist at all.

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
| `INLINE_MAX_DEPTH` | 9 | new (`MaxInlineLevel`) |
| `INLINE_MAX_RECURSIVE_DEPTH` | 1 | new (`MaxRecursiveInlineLevel`) |
| `INLINE_MIN_SPECULATION_OBSERVATIONS` | 250 | new — half `INLINE_HOT_SITE_OBSERVATIONS` |
| `INLINE_MONOMORPHIC_SHARE_PCT` | 90 | new |
| `INLINE_BIMORPHIC_SHARE_PCT` | 92 | new |
| `INLINE_MEGAMORPHIC_TYPE_CEILING` | 8 | new — twice the PIC's four ways |

`INLINE_MAX_RECURSIVE_DEPTH` counts **ancestors on the inline stack**, not
sibling copies. A leaf called five times from one caller is five independent
sites, each paying the per-method budget; that is not recursion and must keep
inlining. Conflating the two would have silently disabled inlining of repeated
calls, which is most of the win.

### Receiver-shape classification

`classify_receiver_shape` reads `profile::MethodProfile::receivers` — the map
the interpreter really feeds, via `ProfileStore::record_receiver_borrowed`, for
`invokevirtual` and `invokeinterface`. (`jit/src/pgo.rs` is a design sketch with
no recorder; nothing here reads it.)

Ranking is descending count, ties broken by ascending class id. `FxHashMap`
iteration order is not deterministic, and two compiles of the same profile must
produce the same artifact.

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
  reported apart (`megamorphic` vs `receiver-not-dominant`) because they call
  for different responses: never speculate here, vs. wait for a longer profile.

### Verdicts

- `DirectBind` — statically bound callee (`invokestatic`/`invokespecial`). One
  possible target, no guard, no speculation.
- `Monomorphic { guard_class_id }` — splice the body behind
  `CMP DWORD [recv+0], guard_class_id`, miss edge falls through to normal
  dispatch.
- `Bimorphic { guard_class_ids }` — two guarded bodies in descending profile
  order, second miss edge falls through to dispatch.
- `Refuse(InlineRefusal)` — thirteen named reasons, each with a stable
  `category()` string for metrics keys and log greps.

### Exception-path policy

Three rules, in precedence order:

1. **Precise exception frames ⇒ nothing inlines.** Mirrors the unconditional
   `inline_sites.clear()` that `try_compile_inner` already performs. Stated in
   the policy as well so the two cannot drift apart silently.
2. **A speculative site inside a protected range is refused.** The guard's miss
   edge is a new control-flow edge in the middle of a `try` block, and the
   inlined body publishes no exceptional frame of its own.
3. **A statically bound site inside a protected range is unchanged.** It inlines
   exactly as it did before this policy existed, because the lowering it asks
   for is the one the backend has always emitted there. Widening or narrowing
   this would be a behaviour change dressed as a policy.

### Fail-closed rules

No speculative inline is admitted without **both**:

- a receiver class-id guard the backend can actually emit
  (`InlineBackendCaps::guarded_inline_body_at_virtual_sites`), and
- a recorded invalidation dependency naming the speculated receiver class.

Either missing ⇒ `GuardNotEmittable` or `NoInvalidationDependency`. Both are
refusals, never warnings.

## 3. Deopt safety with no caller scopes

`deopt::FrameState::caller` exists but **no production site populates it** —
every construction in `ir_lower.rs` and `x64.rs` passes `caller: None`; only
`deopt.rs`'s own tests build a chain. Inlined scopes are therefore not
representable in deopt metadata. An inline that needed to describe "we are
inside callee C, called from caller M at bci N" would publish a frame naming M
with C's bci: a silent wrong answer of exactly the class this branch closed with
`apply_ea_to_ir` and the safepoint-named scalar-replacement fix.

**This policy relies on the absence of deopt points inside inlined bodies, not
on caller scopes.** The argument, verified against the source:

- `x64::try_emit_inline_body` (`jit/src/x64.rs:9772-11056`) contains no
  `snapshot_pre_intrinsic_call`, no `build_and_record_deopt_point` and no
  `deopt_stubs` push. Every callee operation it cannot emit without one is a
  `return false`, and `try_emit_inline` (x64.rs:9689) rolls back the buffer, the
  operand stack, the oop marks, the spill offset and all seven deferred
  patch-lists, then falls through to a real call.
- The one construct in the callee replay that can publish a frame is
  `emit_post_invoke_exception_check`, reached for an inlined `<clinit>` helper.
  It builds a reason-9 `DeoptimizationPoint` only when
  `self.precise_exception_frames` is set — and under that flag
  `try_compile_inner` has already cleared every inline site, so the path is
  unreachable. `InlineRefusal::PreciseExceptionFrames` restates the same
  interlock in the policy.
- The callee replay never advances `dbg_last_pc`, so even the unreachable path
  would key on the caller's invoke bci rather than a callee bci.

Consequence, stated plainly: an inlined body is entered and left within one
frame that is described as the caller's own, and no deopt point inside it is
described at all. There is no metadata a missing caller scope could make wrong.

**Hard precondition for any future work.** If the emitter learns to publish a
deopt point inside an inlined callee — an inlined bounds check, an inlined
null check, an inlined guard that traps rather than falling through — this
policy must refuse that shape until `FrameState::caller` is populated by the
producer. The guarded-virtual lowering sketched in §5 is deliberately specified
with a **fall-through** miss edge, not a trap, for exactly this reason: a
falling-through guard needs no frame.

## 4. Dependencies and how invalidation reaches the code

The channel is the existing one. `InlinePlan::invalidation_triples()` flattens
to `(class, method, descriptor)` triples, which the wiring writes into
`CompiledMethod::inlined_methods` (deduplicated — the scans are `.any()`
predicates that run on every class define, so a repeated triple is pure cost).

Two dependency kinds:

- `InlinedCallee` — the spliced body. Recorded for every admitted plan.
- `SpeculatedReceiver` — the class a guarded site speculated on. Recorded for
  every guard id of every speculative plan.

Reach, in the VM:

| Event | Call | Matches |
|---|---|---|
| class define | `JitCache::invalidate_for_class_change(name)` and again for the direct superclass (`vm/src/vm/vm_init.rs`, `load_class`) | any triple naming either class |
| class redefine / structural change | `JitCache::invalidate_for_class(name)` | any triple naming the class |
| class unload | `JitCache::invalidate_unloaded_class(class_id, name)` | owner, or any triple naming the class |

All three funnel into `JitCache::invalidate_matching`, which collects the
matching entry pointers, retargets inline caches, republishes the shard
snapshots and hands the executable allocations to the epoch/quiescence reclaim
path. So a retired body stops being reachable at the next lookup and its memory
is reclaimed once no active frame can still be executing it.

### Known coarseness — asserted, not assumed

The VM walks the loaded class and its **direct** superclass only. A dependency
on `A` is therefore *not* reached when `C extends B extends A` is loaded.

This is survivable because **correctness does not rest on the dependency**. An
exact receiver class-id guard is not a CHA assumption: a `C` receiver fails the
`CMP` and takes the dispatch path, which resolves the real target. The
dependency is a *retirement* obligation — without it, the caller keeps paying a
guard that now always misses, with no event to trigger a recompile against the
new profile.

`speculative_inline_records_a_dependency_a_class_load_invalidates` asserts the
gap explicitly, so it stays a documented cost rather than becoming a surprise.

### The stronger channel that exists but is not wired

`deopt::InvalidationManager` already models this properly:
`CompilationAssumption::StableType { bci, expected_class }` is exactly a guarded
receiver speculation. It is not usable from `try_compile_inner` today for two
reasons, both out of scope for this change:

- the manager lives behind `vm/src/vm/realms/jit_realm.rs`'s mutex and is not
  threaded into the compiler; and
- `InvalidationManager::on_class_loaded` consults only `leaf_class_index` and
  `class_dependencies` — there is no reverse index for `StableType`, so
  registering one would record an assumption nothing ever queries.

Wiring `StableType` (index + query + registration at install) is the clean
long-term fix and would close the grandchild gap. See §6.

## 5. What the backend cannot do yet

`InlineBackendCaps::single_pass_x64()` reports
`guarded_inline_body_at_virtual_sites: false`, and that is why every speculative
plan is refused in production today. The facts behind that flag, verified
against `jit/src/x64.rs`:

- `jit/src/x64.rs:17274` — the `invokestatic` arm consults `inline_sites`.
- `jit/src/x64.rs:19463` — the `0xb6 | 0xb7 | 0xb9` arm consults `inline_sites`
  **only** when `op == 0xb7`; its own comment reads "invokespecial only —
  virtual/interface not eligible".
- `jit/src/x64.rs:20716` — the plain direct-call path in that same arm emits
  `emit_call_absolute(callee_entry)` with **no receiver test at all**. The
  `CMP DWORD [recv+0], guard_class_id` compare exists only inside the String
  (x64.rs:19597) and CRC32 (x64.rs:20543) intrinsic ladders.

So a speculative plan has nowhere to be emitted, and pushing a `direct_calls`
entry at a virtual pc would produce an **unguarded** call to a profile-chosen
target — a wrong-target bug for every other receiver. Refusing is the only
correct answer until the backend edit below lands.

### Required backend edit (out of scope for this change)

`jit/src/x64.rs:19462-19468`, currently:

```rust
// Check for inline site (invokespecial only — virtual/interface not eligible)
if op == 0xb7 && self.inline_sites.contains_key(&pc) {
    if self.try_emit_inline(pc) {
        pc += 3;
        continue;
    }
}
```

needs a virtual/interface arm that, for `op == 0xb6 || op == 0xb9` with a
planned site carrying guard class ids:

1. loads the receiver (deepest operand, `callee_params + 1` down) into a
   register **without popping**;
2. emits `TEST reg,reg` + `JZ miss` and `CMP DWORD [reg+0], guard_class_id` +
   `JNE miss` per guard id;
3. calls `try_emit_inline(pc)` on the hit path, rolling back the whole guard
   sequence if it bails;
4. emits `JMP done` and lands `miss` on the **existing** dispatch fall-through
   (MIC/PIC or `jit_invoke_dispatch`) — not a deopt trap, per §3;
5. leaves `pc_is_protected(pc)` sites alone, matching
   `InlineRefusal::SpeculationInsideProtectedRange`.

Carrying the guard ids to the backend needs one of: a new field on `InlineSite`
(blocked — `vm/src/runtime/interpreter/invoke.rs:19312` constructs it with an
exhaustive struct literal, so adding a field breaks the `vm` crate), or a
separate `HashMap<usize, Vec<u32>>` threaded into `x64::compile_with_param_slots`
alongside `inline_sites`. The second is the smaller change and does not touch
the VM crate.

Once that lands, flip
`InlineBackendCaps::single_pass_x64().guarded_inline_body_at_virtual_sites` to
`true`, thread a class-id → class-name resolver into `try_compile_inner` so
`receiver_class_namer` is non-`None`, and widen the invoke-loop condition from
`invoke_kind == 3 || invoke_kind == 1` to include `0 | 2`. Nothing else in the
policy changes: `production_caps_refuse_every_speculative_site` is the test that
will fail loudly if the caps are flipped before the emitter exists.

## 6. Metrics wiring

`CompiledMethod::inline_tally` (an `InlineDecisionTally`) is published on every
artifact and carries:

| field | meaning |
|---|---|
| `candidates` | sites considered |
| `inlined_sites` | sites inlined |
| `speculative_sites` | of those, guarded receiver speculations |
| `inlined_bytecodes` | callee bytecodes spliced — the review's "inlined bytecodes" |
| `expansion_cost` | budget spent |
| `observed_calls_inlined` | summed profile executions of the inlined sites — the review's "call count" |
| `refusals` | histogram by `InlineRefusal::category()` |

The virtual-site arm of the invoke loop additionally classifies every
`invokevirtual`/`invokeinterface` receiver profile and tallies the refusal
without paying for a callee resolution, gated on `metrics::enabled()` so the
measurement does not tax the compile path it measures. `refusal_count(
"guard-not-emittable")` on a real workload is therefore a direct measurement of
how much the missing backend lowering in §5 is costing.

`deopts`, `compile milliseconds` and `code bytes` are already recorded:
`CompilationReport::deopt_points`, `::phases[..].wall_ns` / `::total_wall_ns`,
`::code_bytes`.

### Exact edit needed in `jit/src/metrics.rs` (not owned by this change)

1. Add to `CompilationReport` (next to `code_bytes`, around metrics.rs:507):

   ```rust
   /// Sites the inliner considered. See `crate::InlineDecisionTally`.
   pub inline_candidates: Measured<u32>,
   /// Sites inlined.
   pub inlined_sites: Measured<u32>,
   /// Of those, guarded receiver-type speculations.
   pub speculative_inlined_sites: Measured<u32>,
   /// Callee bytecodes spliced into this body.
   pub inlined_bytecodes: Measured<u32>,
   /// Profile-observed executions of the inlined call sites.
   pub inlined_call_count: Measured<u64>,
   /// Refusals by `InlineRefusal::category()`.
   pub inline_refusals: Vec<(String, u32)>,
   ```

2. Initialise them in `CompilationReport::new` (metrics.rs:534) as
   `Measured::NotMeasured` / `Vec::new()`.

3. Harvest them in `CompileRecorder::installed` (metrics.rs:1005), which already
   receives `&crate::CompiledMethod` and reads only public accessors:

   ```rust
   let tally = cm.inline_tally.clone();
   // ... inside the existing `self.with_report(|r| { ... })`:
   r.inline_candidates = Measured::Value(tally.candidates);
   r.inlined_sites = Measured::Value(tally.inlined_sites);
   r.speculative_inlined_sites = Measured::Value(tally.speculative_sites);
   r.inlined_bytecodes = Measured::Value(tally.inlined_bytecodes);
   r.inlined_call_count = Measured::Value(tally.observed_calls_inlined);
   r.inline_refusals = tally
       .refusals
       .iter()
       .map(|(c, n)| ((*c).to_string(), *n))
       .collect();
   ```

4. Emit them in `CompilationReport::to_json` (metrics.rs:577) alongside the
   existing numeric fields, with the refusal histogram as a nested object.

No other file needs to change for the metrics side.

## 7. To reconcile

1. **The backend lowering in §5** is the difference between a policy that plans
   guarded devirtualisation and one that delivers it. Until it lands the
   measured effect of this change on virtual sites is zero by construction, and
   `refusal_count("guard-not-emittable")` says exactly how large the missed
   opportunity is.
2. **`StableType` in `InvalidationManager`** has no reverse index and no
   `on_class_loaded` query (`jit/src/deopt.rs:979`). Wiring it would close the
   grandchild gap in §4 and give speculations a retirement path that does not
   depend on the coarse name scan.
3. **`needs_heap` under precise exception frames.** Previously a site was
   admitted (setting `needs_heap` from `site.needs_heap`) and then cleared;
   now it is refused up front, so such a method no longer claims a VM context
   it does not use. `CompiledMethod::needs_context` is a queried flag, so
   callers adapt — but it is a real, if strictly tightening, behaviour change
   and is called out here rather than left to be discovered.
4. **The tally undercounts candidates once the budget is exhausted**, because
   the invoke loop still short-circuits on `inline_budget_remaining > 0` before
   calling the resolver. That guard is worth keeping (it avoids a resolver call
   per site), but it means `candidates` is "sites the resolver was asked
   about", not "sites that existed".
5. **`jit/src/pgo.rs` remains unwired.** Nothing here reads it. If it is ever
   given a recorder, `ReceiverTypeProfile::is_monomorphic` and
   `InliningPolicy::should_inline` should be deleted in favour of
   `classify_receiver_shape` / `plan_inline` rather than kept as a second,
   divergent policy.
