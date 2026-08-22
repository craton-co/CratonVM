# The ntru unpinned-JIT-reference failure now reproduces on GENERATIONAL

## What is failing

`org.bouncycastle.pqc.math.ntru.test.PolynomialTest` under
`-XX:+UseGenerationalGC`, on **pristine `dev`**:

```text
gen-devpure-1  FAIL  IllegalFormatConversionException: d != java.lang.Object
gen-devpure-2  FAIL  IllegalFormatConversionException: d != java.lang.Object
gen-devpure-3  FAIL  IllegalFormatConversionException: d != java.lang.Object
```

> **CORRECTION (2026-08-21, later the same day).** This section originally said
> "deterministically". **It is not deterministic — it is intermittent**, and
> that word was wrong. An attempted bisect ran the same commit `cae49a85c` as
> its BAD endpoint and got **PASS 2/2**, and one interior commit reported
> `run1=PASS run2=SIG` in a single step. Three consecutive failures were real
> but did not establish determinism.
>
> The likeliest reading is host load: the 3/3 runs happened while a 53-class
> gate and other jobs were saturating the box; the bisect endpoints ran on a
> quiet one. That matches this host's recorded behaviour, where load flips
> PASS/FAIL and not only timings.
>
> Everything below that depends on the failure being reliable — including the
> regression *window* — is weaker than it reads. See the bisect section.

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

That leaves roughly 240 commits in the window.

## The bisect was attempted and is VOID

`git bisect run` over `684f37e14..cae49a85c`, probe = build + run, matching the
exception signature rather than the exit code, two passes required for "good":

```text
BAD  end (cae49a85c)  exit=0   <- the endpoint should have been BAD
GOOD end (684f37e14)  exit=0
78606e45d  BAD  (run1=PASS run2=SIG - flaky)
…                                   walked to 819ad679a
```

**The BAD endpoint passed, so the run proves nothing** and `819ad679a` is not a
result. With an intermittent failure a two-run pass cannot establish "good" —
any commit can pass twice by chance — so every GOOD verdict in that trace is
unsound, and the bisect walked a tree of unreliable answers to a confident
conclusion. Do not cite it.

## The rate, measured — and it IS a regression

Ten runs per endpoint, interleaved so host load lands on both equally:

```text
bad  cae49a85c   SIG=7  PASS=3     ~70% per run
good 684f37e14   SIG=0  PASS=10    0/10, never reproduced
```

Two things follow.

**The regression is real.** The good end is clean in ten runs, so this is not a
long-standing intermittent defect that was always present — something inside
`684f37e14..cae49a85c` introduced it. That was the question that decided whether
bisecting is meaningful at all.

**The void bisect is explained arithmetically, not vaguely.** At 70% per run,
two consecutive passes occur 0.3² = **9%** of the time. The first probe required
exactly two passes to call a commit good, so it carried a 9% chance of
mislabelling *any* bad commit — and it spent that on the endpoint check. Nothing
about the host or the tooling was wrong; the probe was simply under-powered for
the rate, and the rate had not been measured.

The re-run uses **six** consecutive clean passes for GOOD (0.3⁶ ≈ 0.07% per
step, ~0.6% over eight steps) and exits on the first signature, so bad commits
stay cheap and only genuinely good ones pay the full six.

A GOOD verdict still means "did not reproduce in six", not proof: if the failure
rate collapses near the introduction point, six passes buys less than that
arithmetic suggests. Whatever commit the search names should be confirmed by
re-running it and its parent directly, rather than trusting the walk.

## The second bisect completed, and its answer did not survive confirmation

The repetition-aware run named `e40c176d8` — a **merge** — and marked both its
parents GOOD, which would have made this a two-clean-branches-interact defect.
Confirmation, 15 reps per arm, all three interleaved:

```text
merge  e40c176d8   SIG=0  PASS=15
p1     0fbb1df8a   SIG=0  PASS=15
p2     927350e53   SIG=0  PASS=15
```

**45 runs, zero reproductions.** The commit the bisect called BAD does not
reproduce at all. So this search is void too, and `e40c176d8` is not the answer
any more than `819ad679a` was.

Note what the BAD verdict rested on: the probe saw the signature **once**, on
run 5 of 6. That was a real observation, not a bug in the probe — and it is not
reproducible fifteen runs later.

## What the evidence actually supports now

Collecting every measurement of this failure, in the order taken:

| commit | result | when |
|---|---|---|
| `cae49a85c` | SIG 3/3 | during a saturating 53-class gate |
| `cae49a85c` | SIG 0/2 | bisect #1 endpoint, quiet box |
| `cae49a85c` | **SIG 7/10** | **interleaved against `684f37e14`** |
| `684f37e14` | **SIG 0/10** | **same interleave** |
| `e40c176d8` | SIG 1/5 | bisect #2 |
| `e40c176d8` | SIG 0/15 | confirmation, interleaved |
| `0fbb1df8a`, `927350e53` | SIG 0/15 each | same interleave |

The rate tracks **when the runs happened** at least as strongly as **which
commit** was built. That is the property that makes an ordinary bisect
unusable here: an absolute per-commit verdict is measuring the box as much as
the code.

The one measurement that controls for it is the interleaved endpoint pair, and
**it reproduced exactly**:

```text
run 1   bad cae49a85c  SIG=7 PASS=3      good 684f37e14  SIG=0 PASS=10
run 2   bad cae49a85c  SIG=7 PASS=3      good 684f37e14  SIG=0 PASS=10
```

Twenty runs at the good end with zero failures against twenty at the bad end
with fourteen. **The regression is confirmed.** An interleaved A/B is a reliable
instrument here even though an absolute verdict is not — the alternation cancels
whatever the environment is contributing.

### The rate is not uniform across the window, and that matters

`e40c176d8` showed the signature once in six runs, then zero in fifteen. If good
commits never fail (`684f37e14` is 0/20), a single signature there cannot be
noise — it means `e40c176d8` is already bad, but at a **much lower rate** than
`cae49a85c`'s 70%. One in twenty-one is consistent with roughly 5%.

So the rate appears to *rise* across the window rather than switch on. That has a
sharp consequence: **"the first bad commit" may not be a well-formed question
here.** Either several commits each widen the race, or one introduces it and
later ones amplify it. A binary search assumes a step function and there may not
be one.

It also prices the search honestly. Detecting a 5% rate with confidence needs on
the order of 60 reps per step, not 6 or 15 — and near the introduction point
that is exactly the rate a bisect would face.

## What a workable method looks like

If the interleaved result does reproduce, a bisect is still possible but each
step must be an **interleaved A/B against a fixed reference build**, not an
absolute verdict: run candidate and reference alternately in the same window and
compare their rates. That costs roughly twice as much per step and needs enough
reps to separate two rates rather than to observe one event — but it is the only
form that survives an environment-sensitive failure.

Absolute-verdict bisects have now been attempted twice, at 2 and 6 repetitions,
and both produced confident answers that confirmation destroyed. A third of that
KIND would do the same — but an interleaved A/B bisect is a different instrument,
and the endpoint pair reproducing twice is evidence it works.

The cost is the open question, not the validity: with the rate falling toward
~5% near the introduction point, each step needs enough reps to separate 5% from
0%, and that is ~60 runs per arm per step rather than 6. Before paying that,
the thing to try is removing the variance instead of measuring around it — see
whether any knob (heap size, collection frequency, CPU contention) drives the
rate toward 100%, which would make every later question cheaper.

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

**Intermittent** — budget many repetitions, not one. ~2 minutes per run. `CRATONVM_DBG_JIT_ROOTSCAN=1` prints one line per
collection (`precise_only` / `incomplete` / `scan_added`), which is what
distinguishes "the scan was skipped" from "the scan ran and found nothing" — the
distinction that took the G1 case three wrong hypotheses to get right.

## Not claimed

* Not that it is the same root cause as the G1 bug. Same *signature*, different
  collector, different protection mechanism (generational protects by not moving,
  G1 by pinning). Treat the shared symptom as a lead, not an identity.
* Not that any particular commit introduced it. The window is ~240 commits and
  nothing has been bisected.
