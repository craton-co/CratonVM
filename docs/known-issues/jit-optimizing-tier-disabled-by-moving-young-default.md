# The optimizing IR (C2) tier is disabled by default — `moving_young` turned it off

**Status:** 🟢 **FIXED 2026-07-30** by scoping the gate (option 2 below).
The tier runs on default flags again. Original report retained below.

## Fix

The gate asked the wrong question. It read `!x64::moving_young_enabled()`, but
what it protects against is a mapless IR frame being live while the collector
**relocates** — and relocation cannot observe a compiled frame at all today:
`conservative_roots::refresh_moving_young_coverage_for_current_thread` vetoes
moving-young process-wide as soon as `jit_code_range_count() != 0`, and
`memory::roots::collect_roots` runs that refresh on the path of every
collection. So a relocating cycle happens only while the process holds no
compiled code. The gate was disabling the optimizing tier in exchange for a
hazard that could not occur.

`types::flags::JIT_PUBLISHES_RELOCATION_CONTRACT` (`false`) now names that fact,
and `x64::moving_young_relocates_compiled_frames()` = `moving_young_enabled() &&
JIT_PUBLISHES_RELOCATION_CONTRACT` is what the two relocation-safety admission
gates read — this one and `direct_jit_callee_calls_enabled`. The runtime veto
reads the same constant, so a future change cannot lift the veto while leaving
a gate disarmed. That is option 2 ("scope the gate") from the list below, done
in a way that keeps option 1 the eventual answer.

Map-publication machinery is deliberately untouched: `shadow_stack_maps_enabled`
and `collect_live_oop_homes` still key on `moving_young`, so the single-pass
backend keeps emitting and exercising the protocol a future flip depends on.
Scoping those too was implemented, measured as no-change, and reverted — see
the note in `shadow_stack_maps_enabled`.

### Verification (default flags; these previously required `CRATONVM_NO_MOVING_YOUNG=1`)

| target | before | after |
|---|---|---|
| `cratonvm-jit --lib` | 1043 passed / 13 failed | **1058 / 0** |
| `jit tests/ir_vs_singlepass.rs` | 77 passed / 12 failed | **89 / 0** |
| `cratonvm-jit` all targets | — | **every target 0 failed** |
| `cratonvm-gc --lib` | — | **873 / 0** |
| `cratonvm-vm --lib` | — | 2300 / 1 (pre-existing `/tmp` Unix-socket test on Windows) |

`BinTreesClassic 18` returns the HotSpot checksum `68332206` at both `-Xmx2g`
and `-Xmx512m`, and `[GC] moving_young: cycles=0` with every fallback still
`jit-relocation-contract-unproven` — i.e. the collector's proof is unchanged and
relocation remains vetoed. The fix removes a JIT self-handicap; it does not
weaken the GC.

Real-workload effect, Hibernate on default flags:

| class | before | after | HotSpot |
|---|---:|---:|---:|
| `ASTParserLoadingTest` | 376 s | **~190 s** | 21 s |
| `OracleInlineMutationStrategyIdTest` | 322 s | **188 s** | 41 s |
| `jpa.lock.LockTest` | 34.5 s | **18.5 s** | 10 s |

The first two now fit inside the suite's 300 s per-class cap. The
`type.temporal` pair still exceeds it on default flags and still passes under
`CRATONVM_NO_MOVING_YOUNG=1`, so a residual moving-young cost remains outside
these two gates — `flush_scratch_registers` at every safepoint
(`emit_pre_safepoint_spill_impl`) and the `can_elide_self_call_register_spill`
proof are the untested candidates. Not yet measured: the box had nine other
sessions' VMs running.

---

## Original report

**Status at filing:** 🔴 OPEN. Not a regression in the IR pipeline itself; a
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

## Test-side follow-up already landed

The 24 affected tests exercise IR *lowering*, not the deployment policy, so they
now pin it explicitly via `x64::set_moving_young_override(Some(false))` (a
thread-local, never set in production) instead of silently depending on the
ambient default. That restores their coverage and, more importantly, makes the
dependency visible at each call site — the absence of which is what let this go
unseen.
