# JIT round 14 wave 3, lane fixup: proposals

Status: PROPOSALS (for triage; not work items until queued)
Found by: round 14 wave 3 lane fixup

Ranked by expected benefit over cost.

## FX-1. Blame the splice for ANY lowering refusal inside a spliced range (generalise RV3-1)

Benefit: medium. RV3-1 turns one lowering refusal (a synchronized-splice window) into a per-site
rebuild. Every other `latch_bailout` in `ir_lower.rs` still costs the whole method even when the
node that refused has a combined-buffer `bytecode_pc` inside a relocated body (a refused recipe, an
unsupported node shape, a deopt point the lowerer cannot describe). The W5-2 machinery
(`ir_splice_rebuild_site`, `IR_SPLICE_REBUILD_MAX`) already maps such a pc to its top-level site.
Cost: small -- record the refusing node's `bytecode_pc` in `latch_bailout` (first latch wins, the
same `LOWER_BAIL_REASON` cell RV3-1 extended) and read it in `ir_tier_attempt` where RV3-1 does.
Risk: low; a wrong blame costs one extra build (the retry refuses again), never a wrong body.
First step: a census under `CRATONVM_DBG_JITC=1` of lowering refusals whose node pc is `>= code_len`
across the probe battery, to size the win before wiring it (switch
`CRATONVM_JIT_IR_LOWER_SPLICE_BLAME`, default off until the census says otherwise).

## FX-2. Let a compile-id reservation pump the code grace when the free list's head lags

Benefit: low-medium (memory; robustness). Since RV3-2 a released compile id is reissued only after
the CODE grace passes its stamp. That grace advances on drains and on the grace-lag pump, both of
which key on the INLINE-CACHE grace lagging. In a process whose inline-cache grace is kept current
by miss-handler stamps and cooperative stops, and that retires nothing for a while, the code grace
can sit behind, and every compile then takes a fresh id (a new 32 KiB chunk per 4096 compiles).
Cost: small -- in `reserve_compile_id`, when the head's stamp is not code-graced but is
inline-cache-graced, run the per-thread evidence once (outside the allocator lock, rate-limited like
`jit_ic_grace_catch_up`) and note the code grace. Risk: low (one registry walk per throttled
compile). First step: count `reserve` calls that took a fresh id while the free list was non-empty
(a field on `RECLAMATION`, surfaced in the code-cache report) to see whether it happens at all.

## FX-3. Lock-free answers for the other per-`loadClass` class-manager reads

Benefit: low-medium on class-loading-heavy startups (Spring, Tomcat). RV3-4 removes one read lock
from `cl_load_class_base_delegation_rooted`; the same path takes `class_manager.read()` for the
loaded-class lookup and the loader-constraint checks. `ClassRealm` already carries three lock-free
mirrors (`published_supers`, `type_maps`, `java_util_classes`).
Cost: medium -- a census first. Risk: medium (any mirror must be published after the manager's own
state, as `bootstrap_append_seen` is). First step: count `class_manager.read()` acquisitions per
`loadClass` on the Tomcat fixture with a debug counter, then pick the top one.
