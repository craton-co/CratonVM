# WITHDRAWN — "JIT-only zero-length array" was not a JIT bug, and `TestMemoryEstimator` is not a cratonvm failure

## Status
**WITHDRAWN 2026-08-02.** Filed 2026-08-01 as
`docs/known-issues/jit-zero-length-array-20260801.md`, HIGH, "deterministic,
JIT-only, reproducible in under a second". Every load-bearing claim on that page
is retracted below, each against a measurement. It is retired here rather than
deleted because three of its *corrections* were right and are still needed, and
because the way it went wrong is the interesting part.

## What it claimed

```
java.lang.ArrayIndexOutOfBoundsException: Index 0 out of bounds for length 0
    at ExactEstimatorProbe.testPageEstimator(ExactEstimatorProbe.java:84)
```

| arm | claimed |
| --- | --- |
| HotSpot jdk-25 ×3 | clean |
| cratonvm **JIT** ×3 | **fails 3/3, deterministic** |
| cratonvm **`--nojit`** ×3 | clean |

and that this was the real blocker on lifting the `org/h2/` JIT ban.

## 1. It does not reproduce — including on the exact tree it was filed from

`ExactEstimatorProbe`, unchanged, from `docs/internal/repros/h2-insert-scale-20260731/`:

| binary | configuration | rounds | AIOOBE |
| --- | --- | --- | --- |
| `dev` @ `750a95f8e3` | baseline JIT | 3 | 0 |
| `dev` @ `750a95f8e3` | 10 further JIT-opt arms (scalar-replacement off, BCE off, range/spec BCE off, inline-TLAB off, inline-new off, aaload-LICM off, arith-LICM off, LICM off, OSR dead-locals off, OSR off) | 3 each | 0 |
| `dev` @ `750a95f8e3` | JIT, 50 rounds ×2 | 100 | 0 |
| `dev` @ `750a95f8e3` | `CRATONVM_NO_MOVING_YOUNG=1`, 50 rounds | 50 | 0 |
| `dev` @ `750a95f8e3` | `--Xmx 64m`, 50 rounds | 50 | 0 |
| **`8d837f1244`** — the tree the report was written from | JIT, 3 rounds ×3 | 9 | **0** |
| **`8d837f1244`**, 12 concurrent VMs ×20 rounds ×3 waves, `--Xmx 96m` | JIT, under self-inflicted load | **720** | **0** |
| `dev` + this session's diagnostic, same 12-way stress | JIT | **720** | **0** |

**≈1 690 clean rounds, 0 events**, and the control arm is the very commit whose
binary produced the original 3/3. The last two rows exist because the original
session ran at host load 70-85 and load was the leading remaining explanation;
12-way self-inflicted concurrency does not bring it back either.

The obvious explanation — "a GC fix closed it in between" — is **wrong, and was
checked rather than assumed**: `8d837f1244` does **not** contain
`9484a11fd8` (*close the young non-moving sweep's silent premature-reclamation
paths*, on dev 2026-08-01 20:15 UTC), yet it is clean.

## 2. `TestMemoryEstimator` matches HotSpot, at HotSpot's own failure rate

The real class, 25 runs per arm, interleaved:

| arm | pass | AssertionError | crash (NPE / AIOOBE) |
| --- | --- | --- | --- |
| cratonvm `8d837f1244` | 23 | 2 | **0** |
| cratonvm `dev` @ `750a95f8e3` | 23 | 2 | **0** |
| **HotSpot jdk-25** | 22 | **3** | 0 |

The failures are the same marginal statistics in all three arms —
`err=0.1213…0.1305` against a `< 0.12` bound, `pct=8` against `<= 7`. The test
seeds an **unseeded** `java.util.Random`; it is flaky by construction, at
roughly 8-12% on HotSpot itself.

So `TestMemoryEstimator` is not a cratonvm failure at all, and the NPE the
original page reported at `TestMemoryEstimator.java:77` did not recur in 50
cratonvm runs.

## 3. There is no `org/h2/` JIT package ban — so there was nothing to block

Measured with `CRATONVM_DBG_JIT_COMPILED=1` on `750a95f8e3`:

| configuration | `org/h2/…` methods JIT-compiled |
| --- | --- |
| default build | **27** |
| `CRATONVM_JIT_ALLOW_PACKAGES=org/h2/` | **26** |

The flag is a no-op. `org/h2/` compiles by default; what remains are
method-level entries, not a package ban. Two consequences:

* the whole "this is the blocker on lifting the ban, whose lift condition is
  *once `TestMemoryEstimator` passes lifted*" framing is void. For the record it
  passes lifted anyway: **15/15**, 0 crashes;
* the retired insert page's *"lifting the ban made the insert path ~9% worse"*
  was a **null A/B** — the same configuration measured twice — and is withdrawn
  along with it.

## What the original page got RIGHT, and is still needed

Its four corrections to the skip-list's account of `TestMemoryEstimator` all
survive, and are now backed by n=25 instead of n=3:

* it is **not** ban-linked (it behaves identically banned and lifted);
* the **average is fine** — `err` passes its own bound; the failing assertion is
  the sampling percentage;
* **HotSpot fails it too**, intermittently — now measured at 3/25;
* it is a **statistical** test on an unseeded `Random`, not a correctness one.

Also still true, and the reason the AIOOBE looked structural: cratonvm's
exception **type and line attribution** were wrong on the H2 class (reported
`NullPointerException` at `:77`; the probe's own `printStackTrace` said
`ArrayIndexOutOfBoundsException` at `:84`). That mis-attribution is unexplained
and worth its own investigation — it is what sent the original session hunting a
null `Integer` that never existed.

## How it went wrong

Three failing runs, no negative control, and a host at load 70-85 running
several 25-thread H2 workloads and two cargo builds concurrently. Three
consecutive failures at an ~8% flake rate is p≈5e-4, so those runs were
probably *real* — but "real once, under that load" is not "deterministic", and
the page asserted the stronger claim from the same three runs. A length-0 array
is also the array face of the all-zero header the collector leaves over a
reclaimed span, which is a live open defect
(`../../../known-issues/h2/bug-h2-blocked-frame-classid0-dispatch-miss.md` and
its sibling) whose rate is exactly the kind that rises with load.

Rule this adds to the pile: **N=3 buys "it happened", never "it is
deterministic"** — and a 250-round negative on the *same commit* is what it
would have taken to say either way.

## Related

* `bug-h2-testmultithread-concurrent-insert-throughput-RESOLVED-20260801.md` —
  the investigation this fell out of; its ban A/B is withdrawn above.
* `../../../known-issues/h2/bug-h2-blocked-frame-classid0-dispatch-miss.md` —
  the all-zero-header defect that is real, reproduced, and still open.
