# Binary Trees depth-18 second half-gap closure (2026-07-30)

Status: **partial fix, real but below goal.** After reverting a regression
found mid-validation (see "History" below), the final candidate is a
genuine, reproducible **~14.5% wall-time improvement** over `origin/dev`,
a **16.1% gap reduction** toward Temurin JDK 25 — short of the 50% goal
stated below.

## Goal and acceptance

This follow-up targets the current `dev` regression in CratonBench
`CratonBench bintrees`, whose fixed workload is the depth-18 Binary Trees
case. The result must retain checksum `68332206` and reduce the excess over
Temurin JDK 25 by at least 50%:

```text
gap reduction =
  1 - (candidate median - HotSpot median)
        / (baseline median - HotSpot median)
```

**Not met.** Final measured gap reduction is 16.1% (below).

## Final acceptance table

Azure EPYC host, `-Xmx8g`, `taskset -c 13`, `nice -n 10`, host load
0.97-1.74 throughout, 10 interleaved rounds (order rotated per round so no
arm systematically leads), checksum `68332206` verified on every one of the
30 runs:

| arm | binary / commit | median ms | min-max ms | non-moving-GC fallbacks | peak RSS |
| --- | --- | --- | --- | --- | --- |
| baseline | `origin/dev` @ `fcc723007` | 1789.5 | 1783-1815 | 1 | ~2.37 GB |
| candidate | this branch @ `3a5d58d1d` | 1529.5 | 1522-1544 | 1 | ~2.37 GB |
| Temurin JDK 25 (reference) | n/a | 177 | 174-178 | n/a | ~210 MB |

```text
gap reduction = 1 - (1529.5 - 177) / (1789.5 - 177) = 1 - 1352.5/1612.5 = 16.1%
```

Wall-time improvement: `(1789.5 - 1529.5) / 1789.5` = **14.5% faster** than
`origin/dev`.

## Root causes

Two independent costs dominated the pre-fix large-heap run:

1. `-Xmx8g` eagerly sized each young semispace to 2 GiB. Binary Trees touched
   hundreds of thousands of demand-zero pages before useful allocation work,
   producing about 2.3 GiB peak RSS and several seconds of kernel CPU on the
   Azure EPYC host.
2. The JIT redundantly zeroed every new object's body and GC header even
   though all TLAB refill backends already return zero-filled allocation
   ranges. The recursive `itemCheck` kernel also paid a general safepoint root
   shadow/spill sequence around its exact self-call despite having no
   allocation, arbitrary call, or backward-edge path.

## Fix (final)

- x64 object allocation relies on the refill invariant and elides redundant
  zero stores for the body, identity-hash, forwarding, and mark fields. Header
  class/kind/shape publication and cursor-last visibility are unchanged (the
  fields that fixed a real historical corruption incident are still written
  unconditionally — see `jit/src/x64.rs` around `OBJECT_KIND_OFFSET`).
  `CRATONVM_NO_JIT_TLAB_ZERO_ELISION=1` restores the conservative stores.
- A deliberately narrow proof recognizes allocation-free exact self-recursive
  kernels. It rejects allocation, arbitrary calls, unresolved fields, and
  backward branches; only the loader-stable exact-self target that the call
  planner already proved may remain. Such a kernel omits the prologue poll and
  the hot self-call root shadow/spill. Its cold stack-overflow guard still
  spills conservatively and therefore forces the moving collector fallback if
  collection occurs there. `CRATONVM_JIT_GC_INERT_SELFREC=0` disables this
  optimization.
- G1 TLAB refill explicitly zeroes reused allocation ranges (`gc/src/g1.rs`),
  matching the invariant `gc/src/gen_heap.rs`'s generational collector
  (bt18's actual default GC) already unconditionally provides. Verified true
  independently of any historical incident narrative — see "History" below.

**Dropped**: the 512 MiB initial young-semispace cap (`gc/src/gen_heap.rs`).
It measured as a net regression — see "History".

## History: the cap was tried, measured as a regression, and reverted

An earlier version of this candidate additionally capped the initial young
semispace at 512 MiB (rationale: `-Xmx8g` was eagerly committing/faulting a
~2 GiB semispace). A first noisy round of Azure measurements (uncontrolled
host load, single-sample and small-N comparisons) suggested this was a net
win. A clean, controlled, 10-round interleaved re-measurement (this session)
showed the opposite: **candidate-with-cap was ~12-13% SLOWER than plain
`origin/dev`** (2033ms vs 1805ms median), not faster.

Isolating each mechanism independently (`CRATONVM_JIT_GC_INERT_SELFREC=0`,
`CRATONVM_NO_JIT_TLAB_ZERO_ELISION=1`) showed neither the self-recursion
optimization nor zero-elision was responsible — both are neutral-to-mildly-
positive alone, and disabling either left the fallback count and wall time
essentially unchanged. The **512 MiB cap itself was the cause**: shrinking
the young semispace from `-Xmx8g`'s uncapped ~2 GiB down to 512 MiB forces
~6x more young GC cycles for this workload, and *every young GC that fires
in this benchmark already falls back to the non-moving sweep* — on baseline
too, not just the candidate (confirmed: running baseline at `-Xmx2g`, where
its own uncapped semispace is also 512 MiB, produces the identical 6
fallbacks). That "moving young essentially never actually moves for this
workload" behavior is a **pre-existing, unrelated `gc_quiescence` defect**
(fallback reasons observed: `missing-exact-rbp`,
`innermost-rbp-belongs-to-unguarded-callee`) — the cap didn't introduce it,
it just turned a benchmark that used to GC once (cheap) into one that GCs
six times, each paying the expensive non-moving-sweep cost instead of a
cheap copy. The RSS win was real (~807 MB vs ~2.37 GB peak) but not worth
that wall-clock cost, and CratonBench's regression gate
(`regression-suite/perf/run-cratonbench-gate.sh`) is time-based, not
memory-based, so the cap was reverted (commit `3a5d58d1d`) and
`gc/src/gen_heap.rs` is back to its original uncapped `Xmx / 4` sizing. The
earlier session's empirical cap-size sweep (256/384/512/768/1024/2048 MiB)
had only ever compared cap sizes against **each other**, never against a
clean, load-controlled measurement of true uncapped `origin/dev` — that's
why it didn't surface the regression.

## Remaining gap (open follow-up, not done here)

The 16.1% gap reduction is real but well short of the 50% goal. The biggest
remaining lever, per the investigation above, is **not** allocation
publication cost but the pre-existing defect where moving-young almost never
actually moves for this workload (constant fallback to non-moving sweep
regardless of cap). If that were fixed, a smaller young semispace would
likely become a net win again (more, but cheap, collections) and could be
revisited. That is a materially larger GC investigation, out of scope here.
Secondary remaining costs: non-moving mark/sweep work and template-JIT frame
traffic. This change intentionally does not attempt escape analysis, scalar
replacement, or a general optimizing compiler comparable to HotSpot C2.

## Validation

- Every JIT benchmark execution produced checksum `68332206` on both baseline
  and candidate, across all measurement rounds (10 + 6 + 6 + 3 + 10 = 35 runs).
- `cargo test -p cratonvm-gc --lib -- --test-threads=1`: 873 passed, 0 failed
  (final state, commit `3a5d58d1d`, includes the reverted-cap test rewrite).
- `cargo test -p cratonvm-jit --lib -- --test-threads=1` (Windows,
  post-`origin/dev`-merge): one crash,
  `x64::tests::cooperative_poll_runs_in_a_pure_compiled_method`
  (STATUS_ACCESS_VIOLATION) — **verified pre-existing on a clean, unmodified
  `origin/dev` checkout** (commit `4d8a39a39`), unrelated to this branch.
  Tracked separately (spawned as its own follow-up), not a gate on this doc.
- The changed diff passes `git diff --check`.
