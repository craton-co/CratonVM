# A static leaf call in a loop costs about an interpreted call when the optimizing tier is refused

Status: OPEN
Area: JIT calls (single-pass tier: `invokestatic` splice / direct bind / dispatch; C2 acceptance gate)
Severity: MEDIUM (performance, 60-250x on the shape; no wrong answer)
Found by: round 14 wave 7 lane mega (reading the orchestrator's recorded battery, not a new run)

## What is wrong

`C:\craton\jitr12-probes\src\R12Mega4OneSite.java` phases `static24` (a loop calling the 20-byte
`static int leaf(int x, int k)` once per iteration) and `nocall24` (the same arithmetic inline) are
meant to price one compiled static call. In every arm but one they are equal. Under
`CRATONVM_C2_ACCEPT=never` (single-pass bodies only) they are not, on every binary this round
recorded (`C:\craton\jitr14-probes\res-*-c2never.txt`, pairs "total ms / best-of-5 ms", 2^21 calls
per rep):

| binary | osr-static24 | ent-static24 | osr-nocall24 | ent-nocall24 |
|---|---|---|---|---|
| base | 6121 / 1156 | 1829 / 341 | 27 / 4 | 26 / 5 |
| w2a | 13488 / 2625 | 3159 / 589 | 76 / 12 | 70 / 12 |
| w3a | 6966 / 1222 | 1811 / 339 | 31 / 5 | 34 / 6 |

Best rep: ~580 ns per call in the OSR shape, ~160 ns in the method-entry shape. For scale, the same
file's `mega24` (a MEGAMORPHIC virtual call per iteration) runs in 47-81 ms best in that arm, i.e.
a static leaf call is 5-25x slower than a 24-receiver megamorphic call, and the interpreter arm
(`res-*-osr0.txt`, OSR off) prices an interpreted static call at ~850 ns. The default arm
(`res-*-def.txt`) shows 20/3 ms for all four cells, so the shape is fine whenever the caller's
optimizing body is accepted (it splices `leaf`).

Why it matters outside the diagnostic arm: the default policy is `evidence`
(`jit/src/ir_evidence.rs` `accept_policy`), which REFUSES optimizing bodies that produce no
evidence ("`fib` is pure arithmetic, so it produces no evidence at all"). Any caller the gate
refuses stays single-pass, and if the cause below is general, every static helper call it makes
from a loop pays this price.

## Hypotheses (by reading; not settled)

1. The single-pass caller does not splice `leaf` (it should: 20 bytes, under `MaxInlineSize`, and
   C14W3-1 / OD-1 price in-loop static sites hot), AND its call is not a direct bind to a compiled
   `leaf`, so every call goes through `jit_invoke_dispatch` to the by-name tail and an interpreted
   `leaf`. ~580 ns is interpreter-call cost, not compiled-call cost.
2. `leaf` itself never gets a usable body in this arm: if its tier-up goes to the optimizing tier
   first (a tiny pure leaf) and the gate refuses it under `never`, is a single-pass body compiled in
   its place, or is the method left interpreted? The default arm hides this: `leaf` produces no
   evidence either, but there the CALLER's accepted optimizing body splices it.

## How to confirm / locate

On any Windows binary: `CRATONVM_C2_ACCEPT=never CRATONVM_DBG=mic-prof CRATONVM_DBG_JITC=1` on
`R12Mega4OneSite`. In `[DISP_CENSUS]`, `kind_static` near 2^21 x 10 reps x 2 shapes and
`out_tail` (or `out_static_bc`) of the same order means hypothesis 1 (every call is a helper
round trip); `out_dcache` of that order means a compiled `leaf` reached through the helper's cache
(still a Rust round trip per call). The JITC lines say whether `R12Mega4OneSite.leaf(II)I` was ever
compiled single-pass, and whether `osrStatic24` / `entStatic24` report an `osr-splice-planned` /
inline tally for the `leaf` site. Compare with `CRATONVM_C2_ACCEPT=evidence` (default) on a copy
of the probe whose callers are pure arithmetic (no array loads), so the gate refuses the callers
too: if `static24` blows up there as well, the defect is in the default configuration.

## Proposed fix

Depends on which hypothesis holds: (1) find why the single-pass planner refuses the site
(`jit/src/lib.rs` single-pass inline planner / `jit_bridge.rs` `resolve_inline_site`) or why the
static site is not direct-bound to the compiled callee; (2) a refused optimizing body must leave a
single-pass body behind (`jit_bridge.rs` tier-up path around the acceptance gate). Neither is in
lane mega's files.
