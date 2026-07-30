# The optimizing IR (C2) tier is disabled by default — `moving_young` turned it off

**Status:** 🔴 **OPEN.** Not a regression in the IR pipeline itself; a
consequence of an unrelated default flip that nothing flagged, because the test
suite that would have caught it could not compile at the time.

## The finding

`try_compile_inner` (jit/src/lib.rs) admits the optimizing IR pipeline only
when:

```rust
if optimize
    && !x64::moving_young_enabled()
    && ir::ir_compatible(&scan)
    …
```

and `types/src/flags.rs` now says:

```rust
pub const DEFAULT_MOVING_YOUNG: bool = true;
```

So on a default run `moving_young_enabled()` is `true`, the gate is `false`, and
**every compile falls through to the single-pass (C1) backend. The C2/IR tier
never runs.**

The gate is legitimate and should not simply be deleted — its comment states
the reason: *"IR lowering has no exact-RBP or safepoint-map publication. A
mapless IR frame can be live when the moving young collector runs, but cannot
prove or rewrite its roots."* Under a relocating young generation that is a
correctness requirement, not a tuning knob.

What is not legitimate is that the two landed independently and nothing
reported the interaction.

## Evidence

`tests::step3_optimize_toggle_routes_c1_singlepass_and_c2_ir` calls the very
same `try_compile` entry production uses, with `optimize = true`, and counts IR
lowerings through the `IR_LOWER_COMPILES` thread-local:

```
assertion `left == right` failed: optimize=true (C2) must route `add` through the IR pipeline
  left: 0
 right: 1
```

`left: 0` — with default flags, an `optimize = true` compile produces **no** IR
body. Setting `CRATONVM_NO_MOVING_YOUNG=1` and changing nothing else flips it to
`1` and the test passes. The same single flag turns **24 of the 25** failing
`cargo test -p cratonvm-jit` assertions green:

| target | default flags | `CRATONVM_NO_MOVING_YOUNG=1` |
|---|---|---|
| lib | 1043 passed / 13 failed | 1054 passed / 2 failed |
| `tests/ir_vs_singlepass.rs` | 77 passed / 12 failed | 89 passed / 0 failed |

## Why nobody noticed

Three things had to line up:

1. `DEFAULT_MOVING_YOUNG` flipped to `true` (`67de5400a`), disabling the gate.
2. `service_callee_deopt` / `set_throw_bci` were added to `JitRuntimeHelpers`
   without updating the ten `jit/tests` tables, so **`cargo test -p
   cratonvm-jit` stopped compiling at all** — 11 × `E0063`. Fixed in
   `4d8a39a39`.
3. Two SIGSEGVs then killed the lib binary at test ~828 and
   `ir_vs_singlepass.rs` partway through. Fixed in `fcc723007`.

So from the moment the flip landed, the tests that assert "C2 routes through
IR" could not run to report it.

## Consequences to check

This is the plausible common cause behind several open throughput documents,
and they should be re-read with it in mind rather than treated as independent:

* `docs/known-issues/tomcat/30-…-OPEN.md` — "1531/1642 hot methods never
  compile" and the whole hot-loop tiering story.
* `project_hib_actionqueue_graph_default_jit_tiering_blocker` — "1362/1463 hot
  methods never compile".
* The `wire-tiered-manager` work, whose try/catch C2 admission fix was recorded
  as *"routing proven, no speedup yet"* — consistent with routing that reaches a
  tier which is then globally switched off.

Note this does **not** mean methods stop compiling: the single-pass C1 backend
still compiles them. It means the optimizing tier contributes nothing, so every
C2-only optimization (the IR optimizer, its inline caches, its direct-call
lowering) is inert by default.

## What a fix would involve

1. **Give IR the map contract the gate demands** — exact-RBP + a complete
   rewritable root map at every GC-capable safepoint, i.e. the same protocol the
   single-pass backend already publishes. Then the gate can be dropped honestly.
2. **Or** scope the gate: if `precise_jit_maps_enabled() || moving_young_enabled()`
   already forces the precise-map machinery on (jit/src/x64.rs does exactly this
   for the single-pass path), establish whether IR bodies can join it rather
   than being excluded wholesale.
3. Until then, **make the interaction loud**: a compile-time or startup
   diagnostic saying "optimizing tier disabled: moving_young" would have turned
   this into a one-line observation instead of a cross-suite archaeology
   exercise.

Do **not** "fix" it by flipping `DEFAULT_MOVING_YOUNG` back — that trades a
throughput ceiling for a GC-correctness hazard, which is the wrong direction and
reverses a deliberate architecture decision
(`docs/internal/arch-2026-07-26/moving-young-precise-roots.md`).

## Update 2026-07-30 — item 3 landed; the trade is now real, not theoretical

`moving_young_disables_optimizing_tier()` (jit/src/lib.rs) replaces the bare
`!x64::moving_young_enabled()` term at the admission site. It returns the same
answer and, the first time it vetoes, emits one `warn` naming the cause, the
consequence, and the opt-out. Observed on a Tomcat `TestTomcat` run:

```
WARN cratonvm_jit: [jit] optimizing (C2/IR) tier DISABLED: the moving young
generation is active and IR lowering publishes no exact-RBP or per-safepoint
oop map, ... `CRATONVM_NO_MOVING_YOUNG=1` restores the optimizing tier and
gives up compaction.
```

**What changed underneath it.** Until 2026-07-30 this document described a trade
that was not actually being made: the moving young generation was
default-*requested* but could never *engage* under JIT, so a process paid the
optimizing tier for compaction it never received. Three defects caused that
(see `docs/internal/default-moving-young-enabled-20260730.md`) and all are fixed —
`BinTreesClassic 18` at `-Xmx512m` now runs 25 real Cheney young cycles with
zero coverage fallbacks. The cost recorded here is now buying something, so the
comparison a reader should make is three-way, not two-way:

| `-Xmx512m` bt18 | young collector | optimizing tier | result |
|---|---|---|---|
| default | moving, 25 cycles | off | 4,220 ms |
| `CRATONVM_MOVING_YOUNG_NO_JIT=1` (the pre-fix behaviour) | non-moving, on the moving-young heap policy | off | 4,295 ms |
| `CRATONVM_NO_MOVING_YOUNG=1` | non-moving | **on** | 4,279 ms |

Medians of five interleaved rounds. The three are indistinguishable, so on this
workload the optimizing tier is currently worth nothing measurable either —
which is itself a reason to price item 1 before assuming it is.

**Item 2 is more tractable than it looks, and for a specific reason — but it is
still not the answer.** An IR-lowered `CompiledMethod` publishes no `oop_maps`
and no `sp_id_slot_off`, so `conservative_roots::moving_young_frame_coverage_complete`
returns `false` for any live IR frame *by construction*: the per-cycle proof
already fails closed on exactly the condition the gate exists to prevent.
Admitting IR would therefore not be unsound — it would mean every cycle with a
live IR frame diverts to the non-moving sweep. Since IR compiles the hottest
methods, that is close to "moving-young never engages again", which is why it
is **not** done here. C2 and a relocating young generation are mutually
exclusive in practice until IR supplies the map contract (item 1); choosing
between them is a policy decision with measurable stakes on both sides, and the
warning above now makes the choice visible instead of silent.

## Test-side follow-up already landed

The 24 affected tests exercise IR *lowering*, not the deployment policy, so they
now pin it explicitly via `x64::set_moving_young_override(Some(false))` (a
thread-local, never set in production) instead of silently depending on the
ambient default. That restores their coverage and, more importantly, makes the
dependency visible at each call site — the absence of which is what let this go
unseen.
