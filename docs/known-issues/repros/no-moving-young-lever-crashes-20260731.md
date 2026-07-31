# `CRATONVM_NO_MOVING_YOUNG=1` crashes — the diagnostic lever half the JIT docs measure with

> **SUPERSEDED 2026-07-31** by
> [`../jit-no-moving-young-opt-out-unpublishes-roots.md`](../jit-no-moving-young-opt-out-unpublishes-roots.md),
> which root-causes this lane from a Linux Hibernate repro found the same day.
> Two independent faults, not one: (1) the flag also withdrew shadow-stack root
> publication — both sides read `flags().jit.shadow_stack ||
> moving_young_enabled()` — now **FIXED**, the collector term is gone from
> both; (2) with publication restored the lane takes a one-byte-off control
> transfer into the safepoint register-spill run (SIGILL) — still **OPEN**, and
> neutralised by `CRATONVM_NO_PRECISE_REG_SPILL=1`.
>
> The isolation below stands and adds to it: the raw JIT-to-JIT direct-call
> gate was ruled out here independently.

**Status:** 🔴 **OPEN**, found 2026-07-31 while re-deriving
[tomcat/32](../../internal/fixed-suite-bugs/tomcat/32-doc04-residual-perf-assertions-CLOSED.md).

Not a default-configuration defect — nothing ships with this set. It matters
because `CRATONVM_NO_MOVING_YOUNG=1` is the **standard A/B lever** for anything
moving-young-related, named in `moving_young_disables_optimizing_tier`'s own
warning text and used to produce the cost tables in
[the retired moving-young gate doc](../../internal/jit-optimizing-tier-moving-young-gate-RETIRED-20260731.md)
and in tomcat/32's 07-30 revision. While it crashes, **none of those tables can
be reproduced or extended**, and any new measurement that reaches for it will
look like an unrelated failure.

## Repro — about ten seconds

```
set CRATONVM_REAL_NET_SOCKETS=1
set CRATONVM_REAL_AQS=1
set CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
set CRATONVM_ROOTSNAP_CACHE=1
set CRATONVM_NO_MOVING_YOUNG=1
cratonvm.exe -Xmx2g -cp <probes-out> DateSymbolsProbe 1000
```

`apps/tomcat-suite-runner/probes/DateSymbolsProbe.java`. Exit code
`-1073741819` (0xC0000005, access violation), partway through round 0 — after
the `A DFS.getInstance` line, before `B`.

## Isolation

Same binary (`33281d948` + the tomcat/32 branch), same host, 3 runs each:

| configuration | result |
|---|---|
| default | **clean 3/3** |
| `CRATONVM_NO_MOVING_YOUNG=1` | **crash 3/3** |
| `CRATONVM_NO_MOVING_YOUNG=1` + `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` | **crash 3/3** |

**So it is not the raw JIT-to-JIT direct-call edge**, which is the obvious
suspect: `moving_young_enabled()` going false is exactly what opens that gate,
and that gate has a documented reclaimed-root SIGSEGV of its own
(`BasicErrorControllerIntegrationTests`, 14/14). Closing the gate explicitly
does not help here, so this is a second, distinct fault in the
non-moving-young configuration.

Crash face, for matching:

```
fault pc is in NO live registered code buffer
ShadowStack @ R10+0x1B8: top=0x0 end=0x0000000112E237DB base=0xFFFFFFFF1DE02E63
```

`base` reading as `0xFFFFFFFF...` and `top` as zero on a live shadow-stack
window is the first thing to chase. Symbolize with `CRATONVM_SYMBOLIZE` against
the same binary, per the crash report's own instruction.

## Why it is worth fixing rather than working around

The lever's whole purpose is attribution. Per
[reference: an inert lever is not an elimination], a diagnostic flag that fails
loudly is recoverable; the danger is the other direction — a run that dies is
easy to misread as "the workload is broken under this configuration", which is
precisely the conclusion the flag exists to test. Two documents currently carry
tables that cannot be re-derived until this is fixed.
