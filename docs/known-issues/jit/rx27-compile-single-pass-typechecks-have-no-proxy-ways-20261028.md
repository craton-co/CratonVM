# rx27 compile: single-pass `instanceof` / `checkcast` send every lambda proxy to the helper

Status: OPEN (implemented opt-in in wave 3, pending build and A/B; see the last section). Area: JIT type checks, single-pass tier (`jit/src/x64/op_object.rs`
`emit_instanceof_site` and the `0xc0` arm). Severity: perf only, size unmeasured. Filed 2026-10-28 by round
rx27, lane compile, by reading `df86fe90ba` (a proposal: it changes no code).

## What is true

A lambda proxy's class id is `>= 0x8000_0000` (`LAMBDA_PROXY_ID_BASE`). It is in no supers display and never
equals a resolved target id, so the single-pass `instanceof` arm sends it to `jit_instanceof`
(`CMP DWORD [RAX], 0 ; JL helper` ahead of the final-miss answer) and the `checkcast` arm to `jit_checkcast`, on
every execution. The helper memoizes a proxy's `true` per class id, so each call is the call plus the memo
probe, not a full resolve.

The optimizing tier answers the same receivers inline from per-target cells: round 25's
`CRATONVM_JIT_IR_INSTANCEOF_PROXY_WAYS` (default ON) and round rx27's opt-in
`CRATONVM_JIT_IR_PROXY_TWO_HIT_WAYS` / `CRATONVM_JIT_IR_CHECKCAST_PROXY_WAYS`. A body that stays single-pass in
steady state (pinned, refused by the IR tier, or still warming) gets none of that.

## How to see it on the host

`CRATONVM_DBG_TYPECHECK_FILTER=<target substring>` prints one census line per helper call; since rx27 a proxy
receiver reads `reason=lambda-proxy`. With the IR switches on, lines that remain in steady state come from a
single-pass site, or from an IR site whose ways were evicted. Map the caller with `CRATONVM_JIT_PERF_MAP=1` and a
RIP sample if the count is large; for the regex rows, start with
`CRATONVM_DBG_TYPECHECK_FILTER=CharPredicate ... R24RegexSplitRows <row> 1 3`.

## What would close it

Either the census shows no steady-state single-pass proxy calls on the rows that matter (then retire this page
as not worth it), or the single-pass arms get the same cell: an artifact-owned `TypecheckSeenCell` per site
(the single-pass backend has no per-site side table for type checks today; `_jit_typecheck_seen_cells` is filled
only by the IR tier), a hit-way compare in front of the `JL helper` edge, and the post-call fill. The soundness
argument is the r25 one on `ir_lower::ir_instanceof_proxy_ways_enabled` and does not depend on the tier.

## Wave 3 (2026-10-28, lane compile; pending build): `CRATONVM_JIT_SP_INSTANCEOF_PROXY_WAYS` (opt-in)

`jit/src/x64/op_object.rs` `emit_instanceof_site`. After the exact-class compare, a negative class id is
answered from the site's cell: a hit way gives `1`; the miss way gives `0` while the duplicate-name latch is down
(test at run time); anything else goes to the helper. The second hit way follows
`CRATONVM_JIT_IR_PROXY_TWO_HIT_WAYS`. The cell is a `TypecheckSeenCell`, one per interned target per body
(`Compiler::sp_proxy_cells`), moved onto `CompiledMethod::_jit_typecheck_seen_cells` at finalize
(`x64/driver.rs`).

The fill needs the receiver's class after the call, but the popped receiver slot is no root across the call (a
moving collector would not update it). So the class id, or 0 for an array, is read before the call into a fresh
non-reference operand word (`push_stack`, the "carrier"). The carrier leaves the simulated stack before the
post-call exception check, so a frame snapshot filed there is the one the arm-less code files. After the call
the word is read back to fill the way, with the optimizing tier's bytes (`ir_lower::proxy_hit_way_fill_bytes`).
A class id never changes for an object, so no reference is needed after the call. An array never fills a way,
even one whose header carries a proxy id as its component.

The `checkcast` arm is not done: its helper returns the reference, but a single-pass cast of a proxy is rare
(javac drops `(T) lambda` casts, and a cast dominated by `instanceof` is the optimizing tier's case).

- Engagement: `CRATONVM_DBG_JITC=1` prints `sp instanceof proxy ways: <method> pc= target= cell= hit_ways=`.
  Then `CRATONVM_DBG_TYPECHECK_FILTER=<target>` shows the `reason=lambda-proxy` census lines dropping to one per
  proxy class per site.
- Test: x64 `rx27_single_pass_instanceof_proxy_ways_answer_seen_proxies_inline` (compiled and executed
  against a counting helper; the switch off is the positive control).
- A/B: the `pwsp` arm on the residuals page. Facts: `RX27ProxyTypecheckFacts` and `R25W6ProxyWaysLoaders` with
  the switch, in both modes, also with `CRATONVM_C2_SUPERSEDE=0` so the single-pass bodies are the ones that run.
- Retire when a host census shows the arm engaging and the facts equal HotSpot. It is a closure of a tier gap,
  not a row win, so the 15 % flip rule decides only its default, not this page.
