# ✅ FIXED — `AccurateMathTest` / `PSquarePercentileTest`: not a hang, and the JIT-mode half was three missing stack opcodes

## Status
**RESOLVED 2026-08-17** on branch `fix/commonsmath-math-divide-overflow-20260816`.

Filed originally as: *`AccurateMathTest` / `PSquarePercentileTest` exceed even a
10x timeout — confirmed CPU-bound interpreter throughput cliff, not a hang.*

Both classes **pass**, in every mode, and the JIT-mode cost of each fell by
**2.6x**:

| class | HotSpot | CVM `--nojit` | CVM +JIT, as filed | CVM +JIT, now |
|---|---|---|---|---|
| `AccurateMathTest` (70 tests) | 26s | 2997s | 194s | **74–137s** |
| `PSquarePercentileTest` (47 tests) | 6s | 3445s | 1000s | **389–504s** |

Every arm reports the same test count as HotSpot (70 and 47) with zero failures,
so none of these is a `started=0` green.

**On the ranges.** This is a shared host — other sessions build and run VMs on
it — so end-to-end wall clock moves with contention: the same binary and class
measured 74s and 137s in two runs hours apart. The controlled number is the
interleaved phase A/B further down (2.66x, 3844 vs 3849 ms across rounds), and
the load-independent one is `hot_but_stuck_in_interpreter`, which is **0** for
both classes where it was 2 and 1. Treat the table as orders of magnitude, not
as a benchmark.

## First: the HANG classification was wrong, and the doc suspected as much

The original report declined to call this a deadlock and was right. Given a
large enough budget **every arm terminates with the correct result**, including
pure interpretation: `AccurateMathTest` needs 2997s under `--nojit` and
`PSquarePercentileTest` 3445s. The 180s per-class cap, and the 1800s "10x"
rerun, were simply below the cost of the work.

That alone retires the HANG label. The rest of this doc is about the part the
original investigation could not see.

## The doc's own hypothesis was refuted

The report identified `Quantile.withDefaults().with(HF6)…evaluate()` over a
990,000-element array as the likely cost — "a comparator-based sort is a
call-dense operation" — and explicitly *cleared* `PSquarePercentile.increment`,
having measured 2,000 increments at 122ms and reasoned that even 990,000 would
"only take roughly a minute".

Timing the `test5Percentile` workload phase by phase (`probes/PSquarePhaseProbe`,
n = 990,000, the suite's real size) says the opposite:

| phase | HotSpot | CratonVM +JIT (as filed) |
|---|---|---|
| `randomTestData` (boxing 990k Doubles) | 19 ms | 421 ms |
| **`increment()` × 990,000** | **99 ms** | **10,358 ms** |
| unbox to `double[]` | 3 ms | 73 ms |
| **`Quantile…evaluate()`** | **15 ms** | **66 ms** |
| TOTAL | 138 ms | 10,919 ms |

`Quantile.evaluate` is **0.6% of the run** and only 3.7x HotSpot — it was never
the problem. (It also isn't comparator-based: the `double[]` overload selects on
primitives.) `increment()` is **95%** of it, at **103x** HotSpot.

**Why the original probe pointed the wrong way:** 2,000 iterations is far too
few to separate CratonVM's start-up from steady state, so almost all of that
122ms was class loading. Dividing it by 2,000 produced a per-iteration figure
that looked plausible, extrapolated to "about a minute", and cleared the one
call that actually mattered. The real steady-state cost is 10.3 µs/iteration —
about 170x what that estimate implied.

## Root cause of the JIT-mode gap: three missing operand-stack opcodes

`CRATONVM_DBG=jit-method-stats` named it in one run:

```
hot_but_stuck_in_interpreter=2 (of which ineligible-by-policy=2, compile-failures=0)
  299995  ineligible-by-policy  PSquarePercentile$Markers.findCellAndUpdateMinMax(D)I
            reason=singlepass-codegen(pc=25,op=0x58)
  299995  ineligible-by-policy  PSquarePercentile$Markers.adjustHeightsOfMarkers()V
            reason=singlepass-codegen(pc=12,op=0x58)
```

`0x58` is **`pop2`**, and it had no arm in the single-pass codegen's dispatch
loop at all — so any method containing one refused to compile and stayed
interpreted for the life of the process. Both of these are called **once per
`increment()`**, which is exactly why the class's whole cost sat in that phase.

`pop2` is not an exotic shape. **javac emits it whenever a call returning `long`
or `double` is used as a statement** — here, discarding `estimate(int)` and a
synthetic `access$502` setter.

Fixing it moved the bail rather than ending it, which is the expected behaviour
of a walk that stops at the first unhandled opcode, so the two it exposed were
taken as well:

* **`dup2_x1` (0x5d)** — also no arm. javac emits it for
  `return this.field = value;` on a `long`/`double` field, i.e. every synthetic
  outer-class setter of a `double`. `PSquarePercentile$Marker.access$502` is one.
* **`dup2` after a store** — the width oracle `dup2_top_cat2` classifies the top
  of stack by looking at the instruction that produced it, and returned "unknown"
  whenever that instruction was a *store* (a store consumes, it does not
  produce). This is javac's chained assignment `a = b = c = 0.0`, and it is why
  `AccurateMath.tanQ` compiled its first `dup2` (following `dconst_0`) and
  refused the next two.

### What each fix is allowed to assume

The operand model holds **one entry per value, not per JVM slot**, so a
category-2 value is a single entry. That makes the admission arguments short:

* `pop2` form 2 pops once, form 1 pops twice; `dup2_top_cat2` already answers
  that width question.
* `dup2_x1` — only FORM-2 is admitted, and the top's width settles it alone: a
  *verified* `dup2_x1` with a category-2 top cannot be FORM-1 (all category-1),
  and JVMS requires its value2 be category-1 or it would have had to be
  `dup2_x2`. In this model FORM-2 is then structurally identical to `dup_x1`.
  FORM-1 needs a different rotate and has no local witness, so it stays
  interpreted — the same conservatism `dup_x2` already applies.
* the store rule needs the **pair** `dup`/`dup2` *then* store. After
  `<t>store`, what survives on top is the original the dup copied, and the
  store's own width names it. Skipping any store and looking further back would
  be unsound: `iload_0; dload_1; dstore_3` leaves an **int** on top behind a
  category-2 store.

## Result

`hot_but_stuck_in_interpreter` is **0** for both classes, and the phase that
owned the cost fell by 2.66x — interleaved, two rounds, same host:

| | `increment()` × 990,000 | TOTAL |
|---|---|---|
| before | 10,358 / 10,210 ms | 10,919 / 10,775 ms |
| after | **3,844 / 3,849 ms** | **4,403 / 4,395 ms** |
| HotSpot | 101 / 102 ms | 141 / 147 ms |

## What is NOT claimed

**This does not close the interpreter-vs-HotSpot gap, and was never going to.**
`PSquarePercentileTest` is still ~65-85x HotSpot and `AccurateMathTest` ~3-5x. What
changed is that the hot methods now *compile at all*; what remains is the
ordinary cost of this JIT against C2 on call-dense numeric code, which is a
separate and much larger subject.

`--nojit` is still slow — 2997s and 3445s. That mode has no fix here and needs
none: it is a diagnostic mode, and the practical answer for suite sweeps is the
one the original doc reached, below.

## For the suite harness

The original doc's practical recommendation stands and is now quantified:
**these classes were never hanging, and the harness was measuring `--nojit`.**
With the JIT on, `AccurateMathTest` finishes in 74-137s — inside even the
original 180s cap — and `PSquarePercentileTest` in 389-504s, which needs a
raised per-class budget but nothing like the 30-minute one. A timeout-bounded sweep that runs
`--nojit` should either drop the forced-interpreter flag or price its classes
against interpreter cost, not against HotSpot's.

## The transferable part

**A small-N probe cannot clear a suspect.** The 2,000-iteration `increment()`
measurement was start-up wearing a per-iteration costume, and it sent the
investigation to a phase that turned out to be 0.6% of the run. Scale a probe
until the constant is a minority of it, or report it as an upper bound only.

**Ask the JIT which methods it refused before profiling why code is slow.**
`CRATONVM_DBG=jit-method-stats` named two methods, their invocation counts and
the exact refusing opcode in a single run — the phase timings said *where* the
time went, but not *why*, and no amount of further timing would have said
"0x58 has no arm".

**Clearing one opcode moves the bail.** All three of these fixes were needed to
get either class to zero stuck methods, and the second and third only became
visible once the first landed. A single-pass walk that stops at the first
unhandled opcode will always under-report how much is missing.
