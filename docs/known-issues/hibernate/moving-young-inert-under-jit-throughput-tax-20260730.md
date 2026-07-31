# Moving young gen is inert under the JIT but still charged for — Hibernate temporal HANGs

**Status: OPEN — superseded in part; see the correction immediately below.**

> **CORRECTION 2026-07-30 (later same day).** The dominant mechanism is not the
> root-map emission this document originally blamed. `moving_young_enabled()`
> also gates the **entire optimizing C2/IR tier**: `try_compile_inner`
> (`jit/src/lib.rs`) admits the IR pipeline only when
> `!x64::moving_young_enabled()`, so with `DEFAULT_MOVING_YOUNG = true` every
> compile falls through to the single-pass C1 backend and C2 never runs. That
> was found independently and is tracked on dev as
> `docs/known-issues/jit-optimizing-tier-disabled-by-moving-young-default.md`,
> which is the authority for this interaction. The emission cost described below
> is real but secondary, and the "Options" section here is superseded by that
> document — in particular it **rejects** flipping `DEFAULT_MOVING_YOUNG` back.
> Fresh post-ban-retirement numbers:
> `docs/internal/repros/hib-five-20260730/RESULTS-ban-lift-20260730.md`.

## Claim

With `DEFAULT_MOVING_YOUNG = true` (current `dev`), a JIT-enabled Hibernate
workload **never runs a single moving collection**, yet every compiled method
still pays the full cost of making one possible. On the allocation-heavy
`type.temporal` classes that tax is the difference between a timeout and a pass.

## Evidence — the collector never moves

`org.hibernate.orm.test.type.temporal.ZonedDateTimeTest`, default config,
`-Xmx2g`. The fallback log is rate-limited to powers of two and still reached:

```
[moving-young] fallback #256: reason=compiled-frame-oop-not-published — a live JIT
frame could not prove a complete rewritable root map, so this young collection runs
the NON-MOVING sweep (no compaction, free-list allocation).
```

At least 256 consecutive young collections were diverted. Reason histogram over
the logged sample:

| reason | n |
|---|---:|
| `compiled-frame-oop-not-published` | 6 |
| `unregistered-jit-frame-on-stack` | 5 |
| `missing-exact-rbp` | 1 |
| `compiled-frame-band-unbounded` | 1 |

Four *different* obligations fail, so this is not one narrow gap.

This is independently corroborated by the moving-young owner branch's own
acceptance data (`codex/moving-young-default-20260730-019fb305`,
`docs/internal/default-moving-young-enabled-20260730.md`): on `BinTreesClassic`
its JIT lane records **`cycles=0, coverage_fallbacks=64`** — "requested moving
young on all 64 pressure cycles but safely diverted them". Only its `--nojit`
lane executed real moving collections. Two unrelated workloads, same result:
**under the JIT, coverage never completes.**

## Evidence — the cost is real

`OffsetDateTimeTest`, same binary, quiet host, one variable changed:

| Config | Result | Wall |
|---|---|---|
| default (`moving_young` on) | **TIMEOUT** | >900 s |
| `CRATONVM_NO_MOVING_YOUNG=1` | **PASS**, `failed=0` | 757 s |
| `CRATONVM_NO_MOVING_YOUNG=1` + `CRATONVM_JIT_VIRTUAL_TIERUP=1` | FAIL (OOM in ByteBuddy) | 717 s |

HotSpot reference: 51 s, with **identical** `ok`/`aborted`/`failed` counts —
so correctness is already at parity and only throughput is at issue.

`ZonedDateTimeTest` behaves the same: TIMEOUT >900 s by default; completes with
`found=608 started=608 ok=404 failed=0 aborted=204` once moving-young is off.

## Why the flag costs anything when both paths sweep

Both configurations end in the *same* `run_non_moving_young_cycle`
(`gc/src/gen_heap.rs`): with coverage incomplete,
`divert_for_incomplete_moving_coverage` forces it; with the feature off,
`has_conservative_roots && !moving_young` forces it. The collector choice is
therefore **identical** — the cost is entirely on the *emission* side:

- `x64::moving_young_enabled()` makes the JIT publish "a **complete** rewritable
  precise root map at **every** GC-capable safepoint — EVERY live oop (operand
  stack entries in registers AND frame slots, and every oop local in its
  register or canonical frame slot)" (`jit/src/x64.rs`).
- It also forces `shadow_stack_maps_enabled()` on, adding shadow push/reload
  emission to every compiled method — a path whose own doc comment says it is
  **partial** and "must stay default-off … Do not enable in production".

So the workload pays for a complete rewritable root map at every safepoint and
then never relocates a single object.

## Additional witnesses (2026-07-31, full 4548-class suite)

Same `org.hibernate.orm.test.type.temporal` package, same shape, milder outcome — these two
don't hit the full 300s wrapper (no HANG) but lose a large fraction of their parameterized
iterations to abort within the wrapper, consistent with the same per-safepoint emission tax
applied to a slightly less allocation-heavy workload:

| Class | found | ok | aborted | ms |
|---|---:|---:|---:|---:|
| `InstantTests` | 204 | 112 | 92 (45%) | 107949 |
| `LocalDateTimeTest` | 162 | 90 | 72 (44%) | 113732 |

Consistent with `ZonedDateTimeTest`'s own `ok=404 aborted=204` (33%) split reported above even
in the *mitigated* (`CRATONVM_NO_MOVING_YOUNG=1`) configuration — some fraction of aborts in this
whole class family look inherent to the per-test time budget under real JIT/GC timing, not solely
attributable to the moving-young tax, but these two are the same mechanism family and don't need
a separate doc.

## Not a universal tax

`ASTParserLoadingTest` reports `[GC] moving_young: cycles=0
coverage_fallbacks=0` and takes 407 s either way. Its ~19x gap to HotSpot (21 s)
is the separate interpreted-code problem (`org/hibernate/` and `org/h2/` are
both blanket-banned in `vm/src/jit/skip_list.rs`), not this one. The moving-young
tax specifically hits allocation-heavy classes that actually trigger young GCs
under live JIT frames.

## Options

1. **Fix coverage** so moving-young actually moves under the JIT. The principled
   fix; it is the outstanding `arch-2026-07-26` follow-up and is large. Four
   distinct obligations currently fail.
2. **Flip `DEFAULT_MOVING_YOUNG` to `false`.** Recovers the throughput today.
   Directly contradicts `codex/moving-young-default-20260730-019fb305`, which is
   actively hardening default-ON.
3. **Adaptive one-way disable** — keep default-ON, but stop requesting moving
   young once the process has proven coverage unachievable (N consecutive
   fallbacks, zero cycles). Better than either default, but to recover the
   *emission* cost it must reach codegen, and `shadow_stack_maps_enabled()` is
   read by both the emission side and the root-scan side, which the code
   requires to agree. Flipping it mid-run risks the scanner ignoring shadow
   entries that older frames still push — i.e. lost roots. Needs real design
   work, not a quick gate.

Option 1 or 3 is the right engineering; option 2 is what the measurements
support today. The choice spans two branches and is not a local call.

## Reproduce

```
.hibtake/run-hib.ps1 -Classes @('org.hibernate.orm.test.type.temporal.OffsetDateTimeTest') `
  -Label probe -Runtime craton -TimeoutSec 900 -EnvVars @{ CRATONVM_NO_MOVING_YOUNG = '1' }
```

`CRATONVM_GC_STATS=1` prints `[GC] moving_young: cycles=N coverage_fallbacks=M`
plus the per-reason histogram at shutdown — the direct way to ask "is the young
generation actually copying?". Note a class that ends in `System.exit` never
prints it.
