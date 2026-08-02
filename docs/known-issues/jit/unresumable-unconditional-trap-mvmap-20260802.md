# `InternalError: refusing side-effecting replay` — a compiled method carries an unconditional trap it cannot resume from

## Status
**OPEN, REGRESSION.** Bisected 2026-08-02 to `ebf1cf3131`
(*Merge perf/ir-code-buffer-estimate-20260801: measure the optimizing tier's
code-buffer estimate*). `org.h2.test.db.TestMultiThread` died in **under 3
seconds** in 7 of 8 runs on dev tip; twelve hours earlier the same class ran for
6-12 minutes and passed 5 of 6.

Very likely an **exposure, not a new defect** — see *What actually changed*.

## Severity
**HIGH.** A hard `java.lang.InternalError` out of a JIT dispatch, on a workload
that has no JIT-visible fault of its own. Any method whose compiled body carries
an unconditional trap and whose artifact publishes `can_deopt_resume == false`
is a landmine that fires on its first compiled call past a side effect.

## Symptom

```
Caused by: java/lang/InternalError: JIT dispatch into
  org/h2/mvstore/MVMap.evaluateMemoryForKey(Ljava/lang/Object;)I failed:
  internal error: precise deoptimization unavailable for
  org/h2/mvstore/MVMap.evaluateMemoryForKey(Ljava/lang/Object;)I at bci 123
  (can_deopt_resume=false (no deopt points, or an elided monitor),
   stashed key "", inline callers 0, reason UnreachedCode);
  refusing side-effecting replay
    at org/h2/mvstore/MVMap.operate(MVMap.java:1897)
```

H2 wraps it as `JdbcSQLNonTransientException: GeneralError`, so the visible
failure is a plain SQL error and the InternalError is three `Caused by` levels
down. Which test method dies varies with timing —
`testConcurrentLobAdd:133`, `testConcurrentAlter:154` — because the trap fires
wherever the method first warms up.

Denying the named method just moves it to its twin:

| `CRATONVM_JIT_DENY` | method that then fails |
| --- | --- |
| *(none)* | `MVMap.evaluateMemoryForKey(Ljava/lang/Object;)I` |
| `MVMap.evaluateMemoryForKey` | `MVMap.evaluateMemoryForValue(Ljava/lang/Object;)I` |

so this is a **shape**, not one method.

## Bisect

Six runs per commit, `--Xmx 1g`, `timeout 100`; "fast fail" = exited non-zero in
under 100 s, "survived" = still running at 100 s.

| commit | fast fail | survived |
| --- | --- | --- |
| `750a95f8e3` (12 h earlier) | **0** | 6 |
| `ebf1cf3131` *merge perf/ir-code-buffer-estimate* | **4** | 2 |
| `103d8328f6` | **6** | 0 |
| `6cf267a21a` | 2 | 1 |
| `5a18a9db1c` (dev tip) | 7 | 1 |

Between `750a95f8e3` and `ebf1cf3131` there is one non-docs commit — that merge.

## What actually changed, and why this is probably an exposure

`ebf1cf3131` improves the **optimizing tier's code-buffer estimate**. Before it,
methods routinely bailed with

```
WARN JIT compile bailed: code buffer estimate too small; method stays interpreted
     method="org/h2/…" code_len=… capacity=… wanted=…
```

`MVMap.evaluateMemoryFor{Key,Value}` are one-line delegations
(`return keyType.getMemory(key);`) that plausibly sat behind that bail. Sizing
the buffer correctly lets them compile — and the artifact they compile into is
the broken thing. The estimate fix is almost certainly right; what it uncovered
is not.

## The defect: two gates asking different questions

Both live in `jit/src/x64.rs`.

**At publish** (~25570) the artifact's resume capability is:

```rust
cm.can_deopt_resume = !cm.deopt_points.is_empty() && !compiler.has_elided_monitor;
```

**At the trap emission site** (~22315) the compiler already tries to refuse to
compile such a method — its own comment says so: *"if the snapshot just built
cannot be materialised back into an interpreter frame, the method is guaranteed
to fail on its first compiled call … Compiling such a method is strictly worse
than interpreting it, so bail the whole compile."* But the predicate is:

```rust
let unresumable_trap = self
    .deopt_points
    .last()
    .is_some_and(|p| !crate::deopt::frame_state_is_resumable(&p.frame_state));
```

That asks **one** of the two questions `can_deopt_resume` asks, and gets the
other one backwards:

* `is_some_and` on an **empty** `deopt_points` yields `false` → *no bail* — yet
  an empty `deopt_points` is precisely the first disjunct that makes
  `can_deopt_resume` false. The runtime message says which one fired here:
  `can_deopt_resume=false (no deopt points, or an elided monitor)`;
* `has_elided_monitor` is not consulted at all.

So a method can pass the compile-time guard and still publish an artifact that
carries an unconditional `UnreachedCode` trap it can never resume from. The
runtime then has only the re-run-from-entry fallback, which it correctly refuses
once side effects have happened — `bci 123` in a method reached through
`MVMap.operate` is well past that point.

This is the codebase's recurring two-gates-two-questions shape; compare
`docs/known-issues/jit/` on the relocation gates.

**Whoever fixes this should decide where.** The narrow fix is to make the
emission-site guard mirror `can_deopt_resume` exactly (bail when
`deopt_points.is_empty()` or `has_elided_monitor`). The complete one is at
publish, where both `can_deopt_resume` and `has_indy_trap` are already computed
a few lines apart: an artifact with `has_indy_trap && !can_deopt_resume` is
strictly worse than no artifact and should not be published. **Do not apply the
publish-side rule blind** — `deopt_points` is empty on the default path unless
`deopt_real_enabled()`, so the naive form would refuse every trap-carrying
artifact, including the many whose re-run-from-entry fallback works fine. The
distinguishing condition is whether a side effect can precede the trap bci.

## Reproducing

```bash
cd <fresh writable dir>          # H2 writes ./data
<cratonvm> --java-home /home/victor/jdk25 --Xmx 1g \
  -c "<h2>/target/classes:<h2>/target/test-classes:$(cat <h2>/craton-testcp.txt)" \
  org.h2.test.db.TestMultiThread
```

~75 % of runs fail in under 3 seconds. A run that gets past ~10 s will run for
minutes — the signal is entirely in the first few seconds, which makes this a
cheap bisect target (`timeout 100`, six runs per commit).

Workaround for anyone who needs the class meanwhile — **verified 0 fast
failures in 4 runs on dev tip, against 7 of 8 unflagged**:

```bash
CRATONVM_JIT_DENY=MVMap.evaluateMemoryForKey,MVMap.evaluateMemoryForValue
```

That both methods together are sufficient is also the confirmation that the
diagnosis is complete for this class: nothing else in it carries the shape.

## Related

* `../h2/bug-h2-testmultithread-concurrent-update-timeout.md` — the same class's
  throughput problem, which this now hides: the class cannot reach
  `testConcurrentUpdate` at all on an affected binary.
* `sigsegv-in-unmapped-code-buffer-20260801.md`, `jit-ir-tier-code-buffer-*` —
  the code-buffer sizing work this fell out of.
