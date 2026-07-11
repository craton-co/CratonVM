# LICM aaload hoist: unguarded preheader load executes accesses the program skips — FIXED

**Found:** 2026-07-11, by code inspection while fixing the per-array BCE bug
(`jit-bce-multi-array-oob-store-FIXED.md` — same loop-header preheader).
**Fixed:** 2026-07-11, `fix/licm-aaload-hoist-guard-20260711`.
**Severity: HIGH** — deterministic VM crash (EXCEPTION_ACCESS_VIOLATION) or a
garbage row pointer fed to the loop body, from *correct* Java programs.

## The bug

The x86-64 single-pass JIT's LICM hoists any loop-invariant
`aload X; iload Y; aaload` (an `Object[][]`/`int[][]` row pointer) from
ANYWHERE in the loop body into the loop preheader, emitted as a raw
`MOV RAX, [RAX + RCX*8 + HEADER_SIZE]` (`emit_ref_aload_regs`) — no null
check, no bounds check. The preheader executes unconditionally on every loop
entry, so the hoisted load runs even when:

- the in-body access is behind a conditional the program skips
  (`for(..){ if (m != null) s += m[j][i]; }` with `m == null`), or
- the loop is zero-trip (`n == 0`), or
- the access would throw in-loop (`j >= m.length`: in-loop that's a proper
  AIOOBE; hoisted it was an unchecked spine read).

## Repro (in-tree)

`test_classes/LicmHoistRepro.java` — four shapes, warmed 60k calls so each
tier-compiles, then probed with bad inputs. HotSpot reference:

```
condNull OK 0
zeroTrip OK 0
uncondOob THROWN java.lang.ArrayIndexOutOfBoundsException
condOob OK 0
```

Pre-fix dev (48c2d92d4): the first probe (`condNull(null, 1, 8)`) dies with
`EXCEPTION_ACCESS_VIOLATION reading 0x30` — rax=0 (null `m`), rcx=1 (`j`),
`0x30 = 0 + 1*8 + HEADER_SIZE`: the hoisted raw aaload dereferencing null.
`uncondOob` would also return garbage instead of AIOOBE. A/B kill-switch:
`CRATONVM_DISABLE_AALOAD_LICM=1`.

## The fix (jit/src/x64.rs)

1. **Guard the hoisted load.** Each preheader hoist now emits
   `TEST RAX,RAX; JZ deopt` (null) and
   `MOV R10D,[RAX+len]; CMP ECX,R10D; JAE deopt` (unsigned bounds — negative
   `Y` caught as huge) before `emit_ref_aload_regs`, routed to the reason-2
   deopt stub at the loop-header bci with a deopt snapshot (deduped against
   the speculative-BCE guard block's snapshot, which is emitted FIRST in the
   preheader — ordering preserved from the BCE fix). On guard failure the
   interpreter re-runs the loop with real per-access semantics: it throws
   NPE/AIOOBE only if the access is actually reached, returns normally for
   the skipped-conditional and zero-trip shapes.

2. **De-spec.** Repeated guard failures at a header cross the per-bci
   de-spec threshold (4, `vm/src/runtime/interpreter.rs`), and the
   `despec_contains` filter on `hoist_info` (added in
   `compile_with_param_slots`, BEFORE `Compiler::new` pairs
   `hoist_offsets` by index) drops the hoist on recompile — the in-loop
   aaload then compiles with its normal checks and no further deopts.

The detector (`find_loop_hoists` / `match_invariant_aaload`) is deliberately
unchanged — conditional-body sequences are still hoisted, made sound by the
emission-side guards. `x64::tests::test_find_loop_hoists_conditional_body_still_detected`
pins that contract.

## Verification

- `LicmHoistRepro` matches HotSpot exactly post-fix (was: VM crash).
- `BoundsDeopt2`/`BoundsDeopt3` (BCE fix repros, same preheader) still pass.
- Full `cratonvm-jit` suite green (898 lib tests).
- Disasm (`CRATONVM_DBG_JIT_DISASM`) confirms the hoist still fires on the
  happy path: guards + one hoisted load in the preheader, in-loop reads from
  the spill slot.
- Perf (interleaved A/B, affinity-pinned): amortized case (inner loop 4096
  iterations) — no measurable delta. Adversarial case (16-iteration inner
  loop, i.e. one guard per 16 iterations) — ~9% (best-of-3: 176.3ms → 192.7ms,
  `LicmHoistBench 16 400000`). The guard is the price of not crashing; a
  provable-safety carve-out (skip the guard when `Y < X.length` is
  statically established, BCE-provenance style) is a possible follow-up if a
  real workload shows the worst-case shape.
