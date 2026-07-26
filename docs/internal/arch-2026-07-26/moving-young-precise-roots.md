# Moving young generation via precise, rewritable JIT roots

Slug: `moving-young-precise-roots` · Wave: arch-2026-07-26

---

## VERDICT (read this first)

**Moving-young is NOT on by default after this change, and it must not be
turned on from the files this session owned.** There is exactly one remaining
blocker, it is a single constant in a file this session did not own, and
everything else needed to flip it has landed.

**The blocker:**

> `jit/src/x64.rs:2420` — `pub fn moving_young_enabled()`, whose body is
> `*G.get_or_init(|| std::env::var_os("CRATONVM_MOVING_YOUNG").is_some())`.

This is the **codegen** gate. It decides whether the JIT emits the shadow-stack
push/reload sequences at all (`x64.rs:2401 shadow_stack_maps_enabled`), whether
`collect_live_oop_homes` publishes the *complete* home set rather than only the
register-invisible operand oops (`x64.rs:10080`), and whether
`OopMapEntry::moving_young_coverage_complete` can ever be `true`
(`x64.rs:10173`, `x64.rs:10531`). Nothing outside that file can cause a
rewritable root map to be emitted. A collector that relocates while this is
`false` relocates objects whose only home is a JIT register or frame slot that
nothing will ever rewrite — guaranteed heap corruption, not a risk.

**The exact change required (next wave, owner of `jit/src/x64.rs`):**

```rust
// jit/src/x64.rs:2420
pub fn moving_young_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    // Default-ON. Opt OUT with CRATONVM_NO_MOVING_YOUNG. The historical
    // CRATONVM_MOVING_YOUNG opt-in is accepted as a compatibility no-op.
    *G.get_or_init(|| std::env::var_os("CRATONVM_NO_MOVING_YOUNG").is_none())
}
```

That is the whole flip. It must match
`gc/src/gc_quiescence.rs::moving_young_env_seed()` exactly (same two env vars,
same precedence), because the seed is what a `cratonvm-gc`-only process and the
window before the VM's first publish observe. Also flip
`gc/src/gc_quiescence.rs::DEFAULT_MOVING_YOUNG` to `true` in the same commit so
those two windows agree.

Nothing else needs to change. In particular the collector and the root gatherer
no longer read the env var at all — see "One gate" below.

**Before flipping, the two validation gates that were always specified for this
feature still apply** (`docs/feature-designs/default-moving-young-gen.md`
step 6): the binarytrees checksum invariant (`bt18 == 68332206`, bt16
`14985902`, bt14 `3222190`) at *small* heaps where minor GC actually fires, and
an app-gauntlet run with `moving_young_coverage_fallback_count()` observed. The
new warn-level fallback diagnostic (below) makes the second one readable from an
ordinary log instead of requiring a special build.

---

## What was actually wrong

The brief's diagnosis was right in direction and understated in degree. Three
separate defects, all now closed except the codegen constant.

### 1. The advertised copying collector was switched off by a second, undocumented opt-in

`ARCHITECTURE.md` advertises a "generational semi-space collector (Cheney moving
young gen)". `gen_heap.rs` contained:

```rust
let fail_closed_non_moving = crate::gc_quiescence::is_active()
    && std::env::var_os("CRATONVM_ALLOW_MOVING_YOUNG").is_none();
...
let divert_non_moving = fail_closed_non_moving || ... ;
```

`gc_quiescence::is_active()` is true whenever **any** thread holds a live JIT
frame (`conservative_roots.rs` `JitEntryGuard::enter` → `gc_quiescence::enter`).
The JIT tier-up threshold is 500 invocations, so in any steady-state workload
this is permanently true. Result: `run_non_moving_young_cycle` — free-list
allocation, in-place survivors, `tracing::debug!("… compaction deferred")`.

The severity beyond the brief: **that term ignored `CRATONVM_MOVING_YOUNG`
entirely.** `moving_young_requested` was not part of `fail_closed_non_moving`.
So setting `CRATONVM_MOVING_YOUNG=1` — the documented way to enable the
feature, and the recipe in `default-moving-young-gen.md`'s "FINISHED"
validation table — could **never** run a moving cycle under a live JIT frame,
which is the only case the feature exists for. You also had to set
`CRATONVM_ALLOW_MOVING_YOUNG=1`, which appears in exactly two places in the
tree, both of them prose (`binarytrees-half-gap-20260718.md:83`, and a
`stream-arraylist…` doc that calls it "diagnostic-only"). The feature has been
"implemented" three times; a plausible reason it kept being a gap is that the
validation runs that declared it working were the only runs that ever set the
second variable.

`CRATONVM_ALLOW_MOVING_YOUNG` is now **deleted**, both from
`collect_garbage_inner` and from `next_young_gc_is_guaranteed_non_moving`'s
parameter list. A flag whose only job is to permit correct behaviour is not a
safety mechanism.

### 2. The gate was three independent env reads in three crates

```
jit/src/x64.rs:2420                            moving_young_enabled()  — CODEGEN
vm/src/jit/conservative_roots.rs:374           moving_young_enabled()  — ROOT GATHERING
gc/src/gc_quiescence.rs:105                    moving_young_enabled()  — COLLECTOR
```

`cratonvm-gc` has no dependency on `cratonvm-jit` (only a dev-dependency), so
the three could not consult each other; they agreed only because all three
parsed the same variable the same way. This is why "flip the default" was never
a safe one-line change: flipping the collector without the codegen relocates
against a map that was never emitted; flipping the root gatherer without the
codegen suppresses the conservative backstop (`roots.rs:512`) with nothing
precise replacing it.

**Now:** the codegen side is the single source of truth.
`conservative_roots::moving_young_enabled()` returns
`cratonvm_jit::x64::moving_young_enabled()` verbatim and, on the way through,
calls `gc_quiescence::publish_moving_young_enabled(on)`. `collect_roots` is on
the path of every collection and re-publishes, so the collector can never decide
to relocate against a gate the codegen disagrees with. The GC's own value is a
published tri-state (`unpublished` / off / on) that falls back to a **fail-safe**
seed before the first publish. A disagreeing re-publish logs a `warn`.

Unit test: `conservative_roots::tests::
moving_young_gate_is_a_single_decision_across_all_three_layers`.

### 3. The register-spill claim in the brief is stale; the real gaps were elsewhere

The brief cites `jit/src/x64.rs:2650` ("the register spill … only ran when the
SEPARATE `CRATONVM_JIT_SAFEPOINT_REG_SPILL` env var was ALSO set — off by
default, so the documented protection never actually happened"). That comment
describes a bug that **has since been fixed**; it reads as a live defect but is
a historical note. `x64.rs:8643-8645`:

```rust
let precise_implies_reg_spill = precise_maps && !precise_reg_spill_disabled();
let safepoint_reg_spill     = safepoint_reg_spill_enabled() || precise_implies_reg_spill;
let safepoint_reg_spill_all = safepoint_reg_spill_all()     || precise_implies_reg_spill;
```

`precise_maps` is `precise_jit_maps_enabled() || moving_young_enabled()`, and
`precise_jit_maps_enabled()` is default-ON since 2026-07-07 (`x64.rs:2077`).
So the full-GPR safepoint spill *does* run by default today.

That is also why it is not the mechanism moving-young needs. A spilled copy in a
frame slot makes a register-resident oop **visible** to a conservative scan; it
does not make the register **rewritable** — the code resumes from the register,
not the spill slot. The rewritable channel is the shadow stack (push before the
GC-capable call, GC rewrites the shadow slots, reload after the call restores
the registers), and `moving_young_enabled()` implies it (`x64.rs:2401`). So the
codegen constant really is the only lever.

---

## Proof obligations: audited, and what was closed

Every site that can declare moving-young unsafe for a cycle, and its state
after this session.

| # | Obligation | Site | Before | After |
|---|---|---|---|---|
| 1 | Registered JIT entry has precise metadata | `conservative_roots.rs` `refresh_…_for_current_thread` | checked | checked + reason code |
| 2 | Frame published its exact RBP | same | checked | checked + reason code |
| 3 | Active safepoint's oop map is `moving_young_coverage_complete` | same, via `moving_young_frame_coverage_complete` | checked | checked + reason code |
| 4 | Parent (RBP-chain) frames' maps complete | same | checked | checked + reason code |
| 5 | Method has ≤64 locals / the PC was reached by the oop dataflow | `x64.rs::moving_young_safepoint_coverage_complete` → the map bit → #3 | checked | unchanged |
| 6 | Active OSR artifact proves rewritable shadow coverage | `roots.rs:502` + `moving_young_osr_shadow_fallback_needed` | checked | checked + reason code |
| 7 | **No unregistered JIT frame on the native stack (A5)** | `conservative_roots.rs` ×2 | **SKIPPED under moving-young** | **CLOSED** |
| 8 | **No peer thread's JIT frames unaccounted for** | — | **not checked at all** | **CLOSED (conservatively)** |
| 9 | OS-frozen peer scanned conservatively | `xt_root_scan.rs` takeover pass | marked, but only at one call site in `interpreter.rs:812` | **also asserted at the source** |
| 10 | Blocked peer's helper window scanned conservatively | `xt_root_scan.rs` helper-window pass | same | **also asserted at the source** |
| 11 | Per-cycle verdict is reset between collections | `begin_moving_young_coverage_cycle` | called from `interpreter.rs` ×3 | unchanged (verified correct ordering: `begin` → deposits → STW/takeover → `collect_roots`) |

### #7 — unregistered JIT frame (the dangerous one)

Both A5 probe sites were guarded by `if !moving_young_enabled() && …`:

- `conservative_roots.rs` `refresh_moving_young_coverage_for_current_thread`
- `conservative_roots.rs` `scan_active_jit_frames`

with the rationale "In precise moving-young mode every actual compiled
transition is registered by JitEntryGuard and checked above." That is an
assertion, not a proof, and it is load-bearing in the fatal direction:

- A JIT frame live **without** a `JitEntryGuard` is not in `JIT_ENTRY_CHAIN`, so
  the per-frame loop never examines it. It publishes no shadow homes and no
  safepoint id, so nothing can rewrite its register/spill slots.
- With the probe skipped, `complete` stayed `true`, `roots.rs` took the
  precise-only branch (which *also* suppresses the conservative backstop that
  would at least have marked the frame's oops), and `gen_heap` relocated. That
  is the `main`-compiled bintrees corruption, re-armed.
- The condition is known reachable — the entire A5 fix exists because it was
  observed (`Vm::invoke` → compiled app `main`, live while a clinit or
  interpreted callee triggers a GC).

**Closed:** both probes now run regardless of the gate. A hit sets the A5
thread-local flag *and* `mark_moving_young_coverage_incomplete_because(
UNREGISTERED_JIT_FRAME)`. Independently, `gen_heap` now folds
`unregistered_jit_frame_on_stack()` directly into `coverage_incomplete`, so the
moving branch cannot bypass it even if a future caller forgets the mark.

The original skip was motivated by **utility**: the probe is a raw-word scan,
not a frame walk, so a cached function pointer or a JIT helper argument that
points inside generated code reads as a return PC and fabricates a frame. The
comment records that this "permanently disabled precise reclamation". That cost
is real and is now paid again — but over-detection costs compaction, while
under-detection costs the heap. See "Remaining work" for the right fix.

### #8 — cross-thread coverage

`refresh_moving_young_coverage_for_current_thread` is, as its name says,
per-thread. Nothing gave the *collection* a positive proof for peers.

The pieces that exist: a cooperatively-parked peer publishes its shadow values
into its `root_snapshot` (`interpreter.rs:3442`) and remaps its own shadow stack
on resume (`interpreter.rs:3813`, plus `remap_active_jit_frames`); an OS-frozen
or helper-window peer is scanned conservatively and the cycle is marked
incomplete (`interpreter.rs:812`). What is missing is a proof at the moment the
**initiator** decides to relocate: a peer whose deposit is stale, or that
entered JIT after its last deposit, is simply unrepresented — and its JIT
registers and frame slots are not rewritable by this collection.

**Closed conservatively.** New
`conservative_roots::refresh_moving_young_coverage_for_collection()` =
per-thread refresh **+** "if any peer holds live JIT frames
(`GLOBAL_JIT_DEPTH > current_thread_jit_depth()`), this cycle is unproven".
`roots.rs` — the one collection-authoritative call site — now calls it; the
per-thread variant stays in use at each mutator's root-snapshot deposit, where
per-thread scope is the right question.

Consequence, stated plainly: **once the codegen constant flips, moving-young
will engage only on cycles where the initiator is the sole thread in compiled
code.** That is over-strict. It is also the honest state of the proof. Making it
precise needs a cross-thread coverage handshake (see "Remaining work").

### #9 / #10 — the xt passes' inherited soundness argument

`xt_root_scan.rs`'s module doc justified conservative peer scanning with:

> A frozen in-JIT peer keeps its JIT-entry guard live, so
> `gc_quiescence::is_active()` stays `true` for the whole collection and the
> heap performs a **non-moving** sweep.

That inference held only while `is_active()` unconditionally forced non-moving.
Under moving-young it is exactly inverted: `is_active()` is the condition
moving-young runs *through*. `interpreter.rs:812` does mark the cycle when
`taken.count() > 0 || helper_windows > 0`, so the obligation was discharged —
but at one call site, by inference, in a different file from the scan.

The passes now assert it themselves (`mark_moving_young_coverage_incomplete_
because(XT_TAKEOVER / XT_HELPER_WINDOW)` at the point where a peer's
conservative roots are actually contributed), and the module doc states the
obligation directly instead of deriving it. Belt and braces, and it survives a
future caller that forgets.

---

## The loud diagnostic

`record_moving_young_coverage_fallback()` now emits `tracing::warn!` **on by
default**, rate-limited (every occurrence up to 8, then powers of two), naming
the specific unproven obligation via a new reason code
(`gc_quiescence::incomplete_reason`, 10 variants with labels). The old
`CRATONVM_MOVING_YOUNG_FALLBACKS` env gate around an `eprintln!` is gone.

Added counters:

- `gc_quiescence::moving_young_coverage_fallback_count()` — pre-existing, now
  actually reachable in a log.
- `gc_quiescence::moving_young_cycle_count()` — **new**. Incremented when a
  young collection actually ran the Cheney copy *while a JIT frame was live*.
  The pair answers "is the young generation actually a copying collector right
  now?" at runtime instead of by reading the collector source.

The failure mode this exists to prevent is precise: the previous signal for "the
copying collector silently stopped copying" was a `tracing::debug!` line reading
`… — compaction deferred.` and an env-gated counter nobody set.

---

## Tests

`gc/src/gc_quiescence.rs`
- `published_moving_young_gate_wins_over_the_local_seed`
- `coverage_cycle_resets_verdict_and_reason`
- `every_incomplete_reason_has_a_label`

`gc/src/gen_heap.rs`
- `non_moving_sweep_when_jit_active` — **updated, not deleted.** Still asserts
  the NEW-1.5 contract verbatim; now states its (narrowed) precondition
  explicitly, that moving-young is not in effect.
- `non_moving_sweep_when_unregistered_jit_frame_on_stack` — **updated,** now
  loops over `moving_young ∈ {false, true}` and asserts the sweep is selected in
  *both*. This is obligation #7's regression test.
- `moving_young_copies_with_live_jit_frame_and_proven_coverage` — **new, and the
  headline assertion:** live JIT frame + healthy generations + proven coverage ⇒
  `objects_copied == 2` and the root address changed. Under the old code this
  was unreachable at any env-var setting short of also setting
  `CRATONVM_ALLOW_MOVING_YOUNG`.
- `moving_young_falls_back_and_counts_when_coverage_is_unproven` — the mandatory
  safety net, plus the counter.
- `moving_young_falls_back_on_forced_non_moving_jit_roots`
- `young_gc_trigger_preserves_moving_headroom_but_fills_non_moving_space` —
  updated for the removed parameter.

`vm/src/jit/conservative_roots.rs`
- `moving_young_gate_is_a_single_decision_across_all_three_layers`
- `collection_coverage_refresh_is_inert_when_moving_young_is_off`
- `peer_jit_frames_present` predicate coverage

Note on test isolation: `gc_quiescence`'s moving-young gate and per-cycle
coverage verdict are thread-local under `cfg(test)` (process-global otherwise),
mirroring the existing `TEST_JIT_ACTIVE_DEPTH` treatment, so parallel unit tests
cannot divert each other's deliberate collections.

**Not built or run** — this wave's agents are forbidden from building
(nine concurrent `cargo build`s OOM the host). The orchestrator builds after
merging.

---

## Remaining work, in priority order

1. **The codegen constant** (`jit/src/x64.rs:2420`) — the flip itself; exact
   patch at the top of this document. Not owned by this session.

2. **Un-blunt obligation #7 so moving-young can actually engage.** Two
   independent routes, either sufficient:

   a. *Register the entry transition.* Make an unregistered JIT frame
      structurally impossible rather than detected. Every current entry goes
      through `JitEntryGuard::enter_with_compiled` — `vm/src/jit/helpers.rs`
      (1223, 7038) and `vm/src/runtime/interpreter.rs` (6692, 22826, 32911,
      36627, 36649, 37111, 37133). The A5 doc names `Vm::invoke` → compiled app
      `main` as the case that escapes; audit `vm/src/vm/vm_exec.rs` (see the
      shadow-stack fold-in at `vm_exec.rs:2269`, which implies a JIT-adjacent
      path there) and give it a guard. With that proven, the probe becomes a
      cheap assertion rather than the gating check.

   b. *Make the probe precise.* `native_stack_has_jit_frame`
      (`conservative_roots.rs:917`) treats any stack word landing in a JIT code
      range as a return PC. Validate that it is one: a genuine return address is
      preceded by a `call`. Decoding the x86-64 `call` forms backwards
      (`E8 rel32`; `FF /2` in its `FF D0-D7` / `FF 10-17` / `FF 50 disp8` /
      `FF 90 disp32` / `FF 15 rip+disp32` / `FF 14 25 abs32` variants, each with
      optional `REX`) is fiddly and must **fail closed** (treat "cannot decode"
      and "cannot read `ra-7`" as a hit). Deliberately not attempted here: it is
      an untestable-without-building change in the one place where being wrong
      is silent corruption.

3. **Cross-thread coverage handshake** (obligation #8). Today the initiator
   assumes nothing about peers. The shape that would work: each peer, at its
   root-snapshot deposit, publishes *both* its shadow values (already done) and
   a per-thread coverage verdict + a deposit sequence number; the initiator
   accepts a peer as proven iff its verdict is complete **and** its deposit is
   from the current cycle **and** it has not entered JIT since. `gc_quiescence`
   would carry the per-thread registry (it already has the
   `PINNED_JIT_ROOTS_BY_THREAD` pattern to copy). Frozen / helper-window peers
   stay unproven by construction — they are scanned conservatively.

4. **Then re-run the validation gates** in
   `docs/feature-designs/default-moving-young-gen.md` step 6 and update its
   status line, which still reads "IMPLEMENTED behind `CRATONVM_MOVING_YOUNG`
   (default-off)". Its "FINISHED" validation table (2026-07-01) should be
   treated as suspect until re-run, for the reason in defect #1: unless those
   runs also set `CRATONVM_ALLOW_MOVING_YOUNG`, the moving cycles they report
   cannot have come from the JIT-active path.

---

## Files not touched, and why

- `jit/src/x64.rs` — not owned. Contains the blocker. Exact patch specified
  above; everything on the consuming side is ready for it.
- `gc/src/young_mark.rs`, `types/src/flags.rs` — **these files do not exist** in
  this tree (nor does any `flags.rs` anywhere, nor a `gc_flags()` accessor
  function; `gc_flags` is only the `u8` field on `ObjectHeader`). The brief's
  line references (`types/src/flags.rs:619`/`:1657`, `gen_heap.rs:3770`) do not
  correspond to this checkout. Creating either file would require editing
  `gc/src/lib.rs` / `types/src/lib.rs`, which this session did not own. The
  flag work landed in `gc/src/gc_quiescence.rs` instead, which is where the
  moving-young gate actually lives.
- `gc/src/safepoint.rs` — GPU/GC coordination (`gpu-offload` feature only);
  unrelated to this item.
- `gc/src/shadow_stack.rs` — the rewritable-root data structure itself. Correct
  as-is; the gap was never in `push`/`for_each_value`/`remap`.
- `vm/src/jit/skip_list.rs` — the JIT-ban skip list; unrelated.
