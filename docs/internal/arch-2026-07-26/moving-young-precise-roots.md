# Moving young generation via precise, rewritable JIT roots

Slug: `moving-young-precise-roots` · Wave: arch-2026-07-26
Basis: merged `dev` @ `6495a191c` (this worktree was branched from `origin/main`
`e4e4053bb` and the whole analysis was re-derived after merging).

---

## VERDICT (read this first)

**Moving-young is NOT on by default after this change, and turning it on would
corrupt the heap today.** That is not a precaution — it is measured, on
unmodified `dev`, in `docs/known-issues/moving-young-gen-drops-jit-held-oops.md`:

| Configuration (bt18, `-Xmx8g`, correct = `68332206`) | Checksum |
|---|---|
| default (non-moving young) | `68332206` ✅ |
| `CRATONVM_MOVING_YOUNG=1` | `68310826` / `68310832` ❌ varies per run |
| `CRATONVM_MOVING_YOUNG=1` + `CRATONVM_ALLOW_MOVING_YOUNG=1` | `68029454` / `68029436` ❌ worse |
| `CRATONVM_MOVING_YOUNG=1` + `CRATONVM_DISABLE_JIT=1` | `68332206` ✅ |

So there are **two** blockers, and the second is the important one:

1. **Structural** — `jit/src/x64.rs:2455 moving_young_enabled()` still parses
   `CRATONVM_MOVING_YOUNG` from the environment itself instead of reading
   `cratonvm_types::flags().gc.moving_young`. Until it does, flipping
   `flags::DEFAULT_MOVING_YOUNG` would make the COLLECTOR relocate while the
   CODEGEN emits no rewritable root map. One line; exact patch below.
2. **Substantive** — even with the flag on, JIT root coverage under relocation
   is incomplete somewhere, and bt18 loses ~0.03% of its nodes per run. This
   session closed two specific holes that are strong candidates for that loss
   (see "The prediction" below), but **it could not be tested** (this wave's
   agents must not build), so the known-issue must be re-measured before anyone
   claims it fixed.

Everything else needed for the flip has landed, and after the one-line codegen
change the flip itself is a single constant in a file this session owns.

### The exact codegen patch (next wave — `jit/src/x64.rs` is not owned here)

```rust
// jit/src/x64.rs:2455 — replace the whole body
#[inline]
pub fn moving_young_enabled() -> bool {
    cratonvm_types::flags().gc.moving_young
}
```

`cratonvm-jit` already depends on `cratonvm-types`, and `flags()` latches from
the environment on first use, so this is behaviour-preserving *today*
(`flags().gc.moving_young` currently evaluates to
`present("CRATONVM_MOVING_YOUNG")`, byte-for-byte what the `OnceLock` did) while
making the future flip a one-constant change.

### Then, and only then, the flip

`types/src/flags.rs`:

```rust
pub const DEFAULT_MOVING_YOUNG: bool = true;   // was false
```

That single edit moves codegen, root gathering and the collector together.
`CRATONVM_NO_MOVING_YOUNG` is already wired as the opt-OUT. No test needs
editing: `empty_source_matches_all_documented_defaults` now asserts against the
constant rather than against `false`, deliberately, so this test cannot become
the thing that blocks the flip.

**Acceptance criteria before flipping** (all of them):

- `docs/known-issues/moving-young-gen-drops-jit-held-oops.md` re-measured and
  CLOSED: bt18 `= 68332206` on every run, with the JIT enabled, at `-Xmx8g`
  *and* at a small heap where minor GC actually fires.
- bt16 `14985902`, bt14 `3222190`.
- App gauntlet with `gc_quiescence::moving_young_coverage_fallback_count()` and
  `moving_young_cycle_count()` both observed — the second must be non-zero, or
  moving-young is "on" and doing nothing (which is exactly the state this whole
  item was created to end).

---

## What was actually wrong

Four defects. Three are closed; the fourth is the measured corruption above.

### 1. The advertised copying collector was switched off by a second, undocumented opt-in

`ARCHITECTURE.md` advertises a "generational semi-space collector (Cheney moving
young gen)". `gen_heap::collect_garbage_inner` contained:

```rust
let fail_closed_non_moving =
    crate::gc_quiescence::is_active() && !gc_flags().allow_moving_young;
...
let divert_non_moving = fail_closed_non_moving || ...;
```

`gc_quiescence::is_active()` is true whenever **any** thread holds a live JIT
frame, and the JIT tier-up threshold is 500 invocations — so in any steady-state
workload this is permanently true and the collector runs
`run_non_moving_young_cycle` with `tracing::debug!("… compaction deferred")`.

The part the brief understated: **that term never consulted
`moving_young_requested`.** So `CRATONVM_MOVING_YOUNG=1` — the documented way to
enable the feature, and the recipe in `default-moving-young-gen.md`'s "FINISHED"
validation table — could not on its own run a moving cycle under a live JIT
frame, which is the only case the feature exists for. You also had to set
`CRATONVM_ALLOW_MOVING_YOUNG=1`, a flag that appears in the tree only in prose,
one of those places calling it "diagnostic-only". A plausible reason the feature
was "implemented" three times and remained a gap.

`CRATONVM_ALLOW_MOVING_YOUNG` is **deleted**: the field is gone from
`types::GcFlags`, the term is gone from `collect_garbage_inner`, and the
parameter is gone from `next_young_gc_is_guaranteed_non_moving`. A flag whose
only job is to permit correct behaviour is not a safety mechanism.

What remains diverting, and both are real:

1. `has_conservative_roots && !moving_young` — with no rewritable map a live JIT
   frame's roots are conservative (a stack word that merely looks like a pointer
   may be an `i64`): markable, never rewritable. The original NEW-1.5 rule.
2. `honor_promotion_oom_risk` — the genuine safety fallback: with both
   generations ~full the moving path can `process::abort()` on promotion
   failure, so that cycle takes the abort-free sweep. Kept exactly as-is.

### 2. The gate was three independent parses in three crates

```
jit/src/x64.rs:2455                    env::var_os("CRATONVM_MOVING_YOUNG")  — CODEGEN
vm/src/jit/conservative_roots.rs:393   env::var_os("CRATONVM_MOVING_YOUNG")  — ROOT GATHERING
gc/src/gc_quiescence.rs:106            gc_flags().moving_young               — COLLECTOR
```

`cratonvm-gc` has no dependency on `cratonvm-jit` (only a dev-dependency), so
the three could not consult each other; they agreed only because all three
resolved to the same variable with the same semantics. That is why "flip the
default" was never a safe one-line change, and it is why `types/src/flags.rs`
existing does **not** by itself solve the problem — the codegen never migrated
to it.

**Now:** the effective gate is a conjunction of two questions answered in
different crates, evaluated once in
`conservative_roots::moving_young_enabled()`:

- *Can* we move? — `cratonvm_jit::x64::moving_young_enabled()`. Only the codegen
  can emit the shadow push/reload and the per-safepoint
  `moving_young_coverage_complete` bit.
- *Should* we move? — `cratonvm_types::flags().gc.moving_young`.

The AND is fail-safe in both directions and closes a live gap: because `x64`
still parses the raw variable, without it a user setting
`CRATONVM_NO_MOVING_YOUNG` alongside a stale `CRATONVM_MOVING_YOUNG` would be
silently ignored on the codegen side. The result is then published into the GC
crate (`gc_quiescence::publish_moving_young_enabled`) from `collect_roots`,
which is on the path of every collection — so the collector can never relocate
against a gate the codegen disagrees with. Before the first publish the GC falls
back to `gc_flags().moving_young`, reachable only in a process with no JIT at
all, where moving is unconditionally safe.

Tests: `conservative_roots::tests::{moving_young_gate_is_a_single_decision_across_all_three_layers,
codegen_gate_vetoes_moving_young_regardless_of_config}`,
`gc_quiescence::tests::published_gate_wins_over_the_flags_default`.

### 3. The register-spill claim in the brief is stale — verified on the merged tree

The brief cites `jit/src/x64.rs:2650` ("the register spill … only ran when the
SEPARATE `CRATONVM_JIT_SAFEPOINT_REG_SPILL` env var was ALSO set — off by
default, so the documented protection never actually happened"). That text is
a **historical note attached to the fix**, not a live defect. On dev,
`x64.rs:2646-2662` is `precise_reg_spill_disabled()`, an opt-OUT
(`CRATONVM_NO_PRECISE_REG_SPILL`), and `x64.rs:8697-8699`:

```rust
let precise_implies_reg_spill = precise_maps && !precise_reg_spill_disabled();
let safepoint_reg_spill     = safepoint_reg_spill_enabled() || precise_implies_reg_spill;
let safepoint_reg_spill_all = safepoint_reg_spill_all()     || precise_implies_reg_spill;
```

with `precise_maps = precise_jit_maps_enabled() || moving_young_enabled()` and
`precise_jit_maps_enabled()` default-ON since 2026-07-07. The full-GPR safepoint
spill runs by default.

It is also not the mechanism moving-young needs. A spilled copy in a frame slot
makes a register-resident oop **visible** to a conservative scan; it does not
make the register **rewritable** — the code resumes from the register, not the
spill slot. The rewritable channel is the shadow stack (push before the
GC-capable call → GC rewrites the shadow slots → reload after the call restores
the registers), which `moving_young_enabled()` implies (`x64.rs:2436`).

### 4. The measured corruption (still open)

See the VERDICT table. What the known-issue's own matrix establishes: the bug is
in the interaction with JIT frames (clean with `CRATONVM_DISABLE_JIT=1`), it
needs real GC pressure (depth 14 clean, depth 18 not), and it is *proportional
to how much relocation actually happens* (adding `ALLOW_MOVING_YOUNG` makes more
cycles move and drives the checksum further off).

---

## Proof obligations: audited, and what was closed

| # | Obligation | Site | Before | After |
|---|---|---|---|---|
| 1 | Registered JIT entry has precise metadata | `conservative_roots::refresh_…_for_current_thread` | checked | checked + reason code |
| 2 | Frame published its exact RBP | same | checked | checked + reason code |
| 3 | Active safepoint's map is `moving_young_coverage_complete` | same → `moving_young_frame_coverage_complete` | checked | checked + reason code |
| 4 | Parent (RBP-chain) frames' maps complete | same | checked | checked + reason code |
| 5 | ≤64 locals / PC reached by the oop dataflow | `x64::moving_young_safepoint_coverage_complete` → the map bit → #3 | checked | unchanged |
| 6 | Active OSR artifact proves rewritable shadow coverage | `roots.rs` + `moving_young_osr_shadow_fallback_needed` | checked | checked + reason code |
| 7 | **No unregistered JIT frame on the native stack (A5)** | `conservative_roots` ×2 | **SKIPPED under moving-young** | **CLOSED** |
| 8 | **No peer thread's JIT frames unaccounted for** | — | **not checked at all** | **CLOSED (conservatively)** |
| 9 | OS-frozen peer scanned conservatively | `xt_root_scan` takeover pass | marked, but only at one call site in `interpreter.rs:812` | **also asserted at the source** |
| 10 | Blocked peer's helper window scanned conservatively | `xt_root_scan` helper-window pass | same | **also asserted at the source** |
| 11 | Per-cycle verdict reset between collections | `begin_moving_young_coverage_cycle` | called from `interpreter.rs` ×3 | unchanged — ordering verified: `begin` → deposits → STW/takeover → `collect_roots` |

### #7 — unregistered JIT frame, and why it is the prime suspect for the corruption

Both A5 probe sites were guarded by `if !moving_young_enabled() && …`, on the
argument that "in precise moving-young mode every actual compiled transition is
registered by JitEntryGuard and checked above". That is an assertion, not a
proof, and it is load-bearing in the fatal direction.

A JIT frame live **without** a `JitEntryGuard` is not in `JIT_ENTRY_CHAIN`, so
the per-frame loop never examines it; it publishes no shadow homes and no
safepoint id, so nothing can rewrite its register/spill slots. With the probe
skipped, `refresh_moving_young_coverage_for_current_thread` returned `true`,
`roots.rs` took the precise-only branch — which *also* suppresses the
conservative backstop that would at least have MARKED the frame's oops — and
`gen_heap` relocated.

**The prediction.** Trace the old `CRATONVM_MOVING_YOUNG=1` (no `ALLOW`) path
for bt18 on a cycle where `is_active()` happens to be false but the compiled
`main` frame is on the stack:

- `fail_closed_non_moving = is_active() && !allow` → `false`
- `has_conservative_roots = is_active() || unregistered_jit_frame_on_stack()` —
  but the flag was never set, because the probe that sets it was skipped → `false`
- `divert_non_moving` → `false` → **moving cycle runs**
- the unregistered frame's oops are neither published nor conservatively marked
  → a few live nodes lost, checksum slightly low, varying with GC timing

That matches every feature of the observed symptom: the ~0.03% shortfall, the
run-to-run variation, the dependence on GC pressure, cleanliness under
`CRATONVM_DISABLE_JIT=1`, and the fact that `ALLOW_MOVING_YOUNG=1` (more moving
cycles) makes it strictly worse. It is also a documented, previously-observed
condition in this exact benchmark —
`docs/internal/fixed-suite-bugs/app-jvm-bugs/gc-stress-bintrees-main-args-unregistered-jit-frame-FIXED.md`.

**Closed:** both probes now run regardless of the gate. A hit sets the A5
thread-local flag *and* `mark_moving_young_coverage_incomplete_because(
UNREGISTERED_JIT_FRAME)`. Independently, `gen_heap` now folds
`unregistered_jit_frame_on_stack()` directly into `coverage_incomplete`, so the
moving branch cannot bypass it even if a future root-scan caller forgets the
mark. Regression test:
`gen_heap::tests::non_moving_sweep_when_unregistered_jit_frame_on_stack`, which
now runs its assertions with moving-young both off and on.

This is a **prediction, not a result.** It must be checked by re-running the
known-issue's repro. If bt18 still under-counts, obligation #8 and the
`CRATONVM_MOVING_YOUNG_VERIFY` gate (`gen_heap::moving_young_dangling_verify_enabled`,
`gc_flags().moving_young_verify`) are the next instruments.

The original skip was motivated by **utility**: the probe is a raw-word scan,
not a frame walk, so a cached function pointer or a JIT helper argument that
points inside generated code reads as a return PC and fabricates a frame. The
comment records that this "permanently disabled precise reclamation". That cost
is real and is now paid again — over-detection costs compaction, under-detection
costs the heap. The right fix is in the probe, not the skip; see "Remaining
work".

### #8 — cross-thread coverage

`refresh_moving_young_coverage_for_current_thread` is per-thread. Nothing gave
the *collection* a positive proof for peers.

The pieces that exist: a cooperatively-parked peer publishes its shadow values
into its `root_snapshot` (`interpreter.rs:3442`) and remaps its own shadow stack
on resume (`interpreter.rs:3813` plus `remap_active_jit_frames`); an OS-frozen or
helper-window peer is scanned conservatively and the cycle is marked incomplete
(`interpreter.rs:812`). What is missing is a proof at the moment the
**initiator** decides to relocate: a peer whose deposit is stale, or that
entered JIT after its last deposit, is unrepresented — and its JIT registers and
frame slots are not rewritable by this collection.

**Closed conservatively.** New
`conservative_roots::refresh_moving_young_coverage_for_collection()` =
per-thread refresh **+** "if `GLOBAL_JIT_DEPTH > current_thread_jit_depth()`,
this cycle is unproven". `roots.rs` — the one collection-authoritative call site
— now calls it; the per-thread variant stays in use at each mutator's deposit,
where per-thread scope is the right question.

Consequence, stated plainly: **once moving-young is on, it will engage only on
cycles where the initiator is the sole thread in compiled code.** That is
over-strict. It is also the honest state of the proof; making it precise needs a
cross-thread coverage handshake (see "Remaining work"). Over-diverting costs
compaction, under-diverting costs the heap.

### #9 / #10 — the xt passes' inherited soundness argument

`xt_root_scan.rs`'s module doc justified conservative peer scanning with: "A
frozen in-JIT peer keeps its JIT-entry guard live, so
`gc_quiescence::is_active()` stays `true` for the whole collection and the heap
performs a **non-moving** sweep." That inference held only while `is_active()`
unconditionally forced non-moving. Under moving-young it is exactly inverted:
`is_active()` is the condition moving-young runs *through*.

`interpreter.rs:812` does mark the cycle when
`taken.count() > 0 || helper_windows > 0`, so the obligation was discharged —
but at one call site, by inference, in a different file from the scan. Both
passes (Windows and Linux arms) now assert it themselves at the point a peer's
conservative roots are contributed, and the module doc states the obligation
directly instead of deriving it.

---

## The loud diagnostic

`record_moving_young_coverage_fallback()` now emits `tracing::warn!` **on by
default**, rate-limited (every occurrence up to 8, then powers of two), naming
the specific unproven obligation via a reason code
(`gc_quiescence::incomplete_reason`, 10 variants with labels).
`gc_flags().moving_young_fallbacks` no longer decides *whether* anything is
reported — it now only asks for every occurrence instead of the rate-limited
subset — and the `eprintln!` it used to gate is gone.

Counters:

- `moving_young_coverage_fallback_count()` — pre-existing, now reachable in an
  ordinary log.
- `moving_young_cycle_count()` — **new.** Incremented when a young collection
  actually ran the Cheney copy *while a JIT frame was live*. The pair answers
  "is the young generation actually a copying collector right now?" at runtime
  instead of by reading the collector source. It is also the metric that would
  have caught defect #1 immediately: it would have read zero throughout the
  2026-07-01 validation that declared the feature working.

This directly implements the known-issue's own third suggested next step
("consider making the failure loud instead of silent while coverage is
incomplete").

---

## Tests

`types/src/flags.rs`
- `empty_source_matches_all_documented_defaults` — **updated** to assert
  `moving_young == DEFAULT_MOVING_YOUNG` rather than `== false`, so the flip
  does not have to edit a test to happen.
- `presence_parser_treats_zero_as_set` — kept, with a note explaining that
  `moving_young` must retain presence semantics for its compatibility opt-in
  precisely because `jit/src/x64.rs` still parses the same variable that way.
- `moving_young_is_an_opt_out_with_a_compatibility_opt_in` — **new**: opt-out
  beats opt-in, and the opt-in alone is sufficient (no second flag).

`gc/src/gc_quiescence.rs`
- `published_gate_wins_over_the_flags_default`, `coverage_cycle_resets_verdict_and_reason`,
  `every_incomplete_reason_has_a_label` — new.

`gc/src/gen_heap.rs`
- `non_moving_sweep_when_jit_active` — **updated, not deleted.** Still asserts
  the NEW-1.5 contract verbatim; now states its narrowed precondition
  explicitly and pins the gate off.
- `non_moving_sweep_when_unregistered_jit_frame_on_stack` — **updated,** now
  loops over `moving_young ∈ {false, true}` and asserts the sweep is selected in
  *both*. Obligation #7's regression test.
- `moving_young_copies_with_live_jit_frame_and_proven_coverage` — **new, the
  headline assertion:** live JIT frame + healthy generations + proven coverage ⇒
  `objects_copied == 2`, root address changed, cycle counted, no fallback
  recorded. Unreachable under the old code at any single env-var setting.
- `moving_young_falls_back_and_counts_when_coverage_is_unproven`,
  `moving_young_falls_back_on_forced_non_moving_jit_roots` — the safety net.
- `young_gc_trigger_preserves_moving_headroom_but_fills_non_moving_space` —
  updated for the removed parameter.

`vm/src/jit/conservative_roots.rs`
- `moving_young_gate_is_a_single_decision_across_all_three_layers`,
  `codegen_gate_vetoes_moving_young_regardless_of_config`,
  `collection_coverage_refresh_is_inert_when_moving_young_is_off`,
  `peer_detection_ignores_this_threads_own_jit_frames` — new.

Test isolation: `gc_quiescence`'s gate, per-cycle verdict, reason and counters
are thread-local under `cfg(test)` (process-global otherwise), mirroring the
existing `TEST_JIT_ACTIVE_DEPTH` treatment, so parallel unit tests cannot divert
or miscount each other's deliberate collections.

**Not built or run** — this wave's agents are forbidden from building (nine
concurrent `cargo build`s OOM the host). The orchestrator builds after merging.

---

## Remaining work, in priority order

1. **Codegen reads the typed config** — `jit/src/x64.rs:2455`, exact patch at
   the top. Behaviour-preserving today; it is what makes the flip a
   one-constant change.

2. **Re-measure `docs/known-issues/moving-young-gen-drops-jit-held-oops.md`.**
   The repro is in that file. If obligation #7 was the cause, bt18 should now
   read `68332206` with `CRATONVM_MOVING_YOUNG=1` and the JIT on — at the cost
   of moving-young engaging rarely (watch `moving_young_cycle_count()`; if it is
   zero, the checksum being right proves nothing). Add that repro to the
   regression suite as the objective pass criterion the known-issue asks for.

3. **Un-blunt obligation #7 so moving-young can actually engage.** Two
   independent routes, either sufficient:

   a. *Register the entry transition* — make an unregistered JIT frame
      structurally impossible rather than detected. Every current entry goes
      through `JitEntryGuard::enter_with_compiled` in `vm/src/jit/helpers.rs`
      and `vm/src/runtime/interpreter.rs`; the A5 doc names `Vm::invoke` →
      compiled app `main` as the case that escapes. Audit `vm/src/vm/vm_exec.rs`
      (it has a shadow-stack fold-in, implying a JIT-adjacent path) and give it
      a guard. The probe then becomes a cheap assertion rather than the gating
      check.

   b. *Make the probe precise* — `native_stack_has_jit_frame` treats any stack
      word landing in a JIT code range as a return PC. Validate that it is one:
      a genuine return address is preceded by a `call`. Decoding the x86-64
      forms backwards (`E8 rel32`; `FF /2` as `FF D0-D7` / `FF 10-17` /
      `FF 50 disp8` / `FF 90 disp32` / `FF 15 rip+disp32` / `FF 14 25 abs32`,
      each with optional `REX`) must **fail closed** — treat "cannot decode" and
      "cannot read `ra-7`" as a hit. Deliberately not attempted here: an
      untestable-without-building change in the one place where being wrong is
      silent corruption.

4. **Cross-thread coverage handshake** (obligation #8). Each peer, at its
   root-snapshot deposit, publishes its shadow values (already done) plus a
   per-thread coverage verdict and a deposit sequence number; the initiator
   accepts a peer as proven iff its verdict is complete **and** its deposit is
   from the current cycle **and** it has not entered JIT since. `gc_quiescence`
   already has the `PINNED_JIT_ROOTS_BY_THREAD` per-thread-registry pattern to
   copy. Frozen / helper-window peers stay unproven by construction.

5. **Then update the stale status lines.**
   `docs/feature-designs/default-moving-young-gen.md` still reads "IMPLEMENTED
   behind `CRATONVM_MOVING_YOUNG` (default-off), validated correct". Its
   2026-07-01 "FINISHED" validation table should be treated as suspect on two
   independent grounds: the known-issue contradicts it empirically, and unless
   those runs also set `CRATONVM_ALLOW_MOVING_YOUNG` the moving cycles they
   report cannot have come from the JIT-active path at all.

---

## Files considered and not changed, with reasons

- `jit/src/x64.rs` — not owned. Holds blocker #1. Exact patch specified above;
  everything on the consuming side is ready for it.
- `gc/src/young_mark.rs` (owned) — parallel young **marking**
  (`YoungMarkBits`, `drain_parallel`, `zero_spans_parallel`). It serves the
  non-moving sweep's mark phase and is orthogonal to the moving/non-moving
  decision: nothing in it reads or influences the moving-young gate, and the
  Cheney path does not use it. No change was warranted. Worth noting for the
  flip, though: if moving-young becomes the default, this module's work
  disappears from the common path, so its parallelism tuning
  (`CRATONVM_GC_PAR_THREADS`, `CRATONVM_GC_PAR_MIN_BYTES`) stops being a
  throughput lever and the profile shifts to the copy.
- `gc/src/safepoint.rs` (owned) — GPU/GC coordination, `gpu-offload` feature
  only. Unrelated.
- `gc/src/shadow_stack.rs` (owned) — the rewritable-root data structure itself.
  Correct as-is; the gap was never in `push` / `for_each_value` / `remap`.
- `vm/src/jit/skip_list.rs` (owned) — the JIT-ban skip list. Unrelated.
