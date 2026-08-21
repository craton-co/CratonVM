# The ntru unpinned-JIT-reference failure now reproduces on GENERATIONAL

## What is failing

`org.bouncycastle.pqc.math.ntru.test.PolynomialTest` under
`-XX:+UseGenerationalGC`, on **pristine `dev`**, deterministically:

```text
gen-devpure-1  FAIL  IllegalFormatConversionException: d != java.lang.Object
gen-devpure-2  FAIL  IllegalFormatConversionException: d != java.lang.Object
gen-devpure-3  FAIL  IllegalFormatConversionException: d != java.lang.Object
```

That is the signature of `bug-g1-evacuates-live-jit-reference-20260819.md` — a
live reference the collector's root set did not contain, read back stale after
the object moved — but on a **different collector** and a different test method
(`testSqToBytes` in one run, `testS3FromBytes` in the G1 case).

## This is not the G1 branch's doing

Measured with the branch that fixed the G1 case applied, and without it, three
runs each, same host, same fixture:

| build | generational |
|---|---|
| pristine `dev@cae49a85c` | FAIL 3/3 |
| `dev@cae49a85c` + the G1 root-coverage branch | FAIL 3/3 |

Identical. The G1 fixes neither cause nor address it. (Under G1 on the same
tips: pristine FAIL 2/2, branch PASS 2/2 — that half is fixed and landed.)

## The regression window, stated honestly

The window is **`684f37e14..cae49a85c`**, and it is wider than it first looked.

What was actually measured passing under generational was a binary built from
`684f37e14` (my branch point, pristine) — `PASS`, with the collector reporting
`incomplete=true` on all 8 collections, so it never took the precise-only
suppression at all. **Pristine `dev@8d5e26cf9` was never run under
generational**, so the tempting narrower claim ("it broke in dev's last 41
commits") is not supported by anything measured. Do not repeat it without
bisecting.

That leaves roughly 240 commits in the window. Bisecting it is the obvious next
step and has not been done.

## What the failing run shows

The collector is repeatedly *declining* to move, which is the safe direction,
and failing anyway:

```text
[moving-young] fallback #3: reason=unregistered-jit-frame-on-stack
[moving-young] fallback #4: reason=compiled-frame-oop-not-published
[moving-young] fallback #5..#8: reason=compiled-frame-oop-not-published
```

`compiled-frame-oop-not-published` is `incomplete_reason::UNPUBLISHED_FRAME_OOP`.
It is **not** new and not from the G1 branch — it is present in both `684f37e14`
and `dev`, introduced by `ba2c417af` ("the frame-band verifier must not pass
vacuously").

The tension worth chasing: a non-moving sweep does not relocate anything, so a
stale JIT slot should be impossible on those cycles. Either a cycle that did
*not* fall back is the one that corrupts, or the damage is not a relocation at
all. That distinction is the first thing to establish — the fallback log is
evidence about the cycles that were safe, not about the one that was not.

Also present, immediately before the failure:

```text
JIT compile bailed: code buffer estimate too small; retrying at the measured size
  method="…/PolynomialTest.testSqToBytes:()V" code_len=341 capacity=67552 wanted=67957
```

Whether a recompile of the very method that then fails is coincidence or
mechanism is unknown; it is recorded because it is one line above the failure,
not because there is a story for it.

## Repro

```bash
cd /data/cratonvm/apps/bc-java
cratonvm --java-home /data/toolchain/jdk-25 -XX:+UseGenerationalGC --Xmx 1g \
    -Dbc.test.data.home=/data/cratonvm/apps/bc-test-data \
    -Dtest.java.version.prefix=25 \
    -c "$(cat /data/bcjca-classpath.txt)" \
    junit.textui.TestRunner org.bouncycastle.pqc.math.ntru.test.PolynomialTest
```

Deterministic, ~2 minutes. `CRATONVM_DBG_JIT_ROOTSCAN=1` prints one line per
collection (`precise_only` / `incomplete` / `scan_added`), which is what
distinguishes "the scan was skipped" from "the scan ran and found nothing" — the
distinction that took the G1 case three wrong hypotheses to get right.

## Not claimed

* Not that it is the same root cause as the G1 bug. Same *signature*, different
  collector, different protection mechanism (generational protects by not moving,
  G1 by pinning). Treat the shared symptom as a lead, not an identity.
* Not that any particular commit introduced it. The window is ~240 commits and
  nothing has been bisected.
