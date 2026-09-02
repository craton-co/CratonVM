# WORKER-4-NOTE-2 — `RTreeRangeGc`: the standalone reproduction, the assertion, and an A/B that clears WORKER 4

**Status: COMPANION to `WORKER-1-NOTE-1`, which owns this vector.** MEASURED
2026-08-22, Linux (Azure host 2), Temurin 25.0.4+7.

`WORKER-1-NOTE-1` establishes the shape and this note does not restate it: the
vector is **two defects wearing one name** — a ~25% flake under `--jdk-only`
and a DETERMINISTIC failure in compatible mode — and the arm boundary sits
exactly between them. Read that first.

This note adds three things it does not have, all of which WORKER 4 needed in
order to land, and none of which should have to be re-derived.

## 1. It reproduces WITHOUT the harness — but only with the launcher flag

The vector passes when run plainly, in both modes:

```text
cratonvm --jdk-only  … -cp build RTreeRangeGc     rc=0, PASS (14014 checks)
cratonvm --real-jdk  … -cp build RTreeRangeGc     rc=0, PASS (14014 checks)
```

**`harness-guard.sh::class_cv_args` gives it `--Xmx 64m`, and that flag is the
whole reproduction:**

```bash
cratonvm --real-jdk --Xmx 64m --java-home "$JAVA_HOME" -cp build RTreeRangeGc
```

`run.sh` says so in as many words — *"Two members — `RPriorityQueueGc` and
`RTreeRangeGc` — are INERT without the CratonVM-only launcher flags
`class_cv_args()` supplies for them. Scheduling either one without that hook
manufactures a green gate."* **A plain run of this vector is a green gate**, so
"cannot reproduce outside the suite" is the wrong conclusion to draw from one,
and a single-vector `ONLY=` run through `run.sh` (which does supply the flag) is
not the only way in. Bisecting this needs a one-process command, and that is it.

## 2. The assertion

```text
AssertionError: RTreeRangeGc: headMap(k,false): 0 entries, expected 300
```

A `TreeMap` range view answering **0** where 300 entries are live. Not an
ordering difference and not a tolerance — a view that lost its whole backing,
at a 64 MB heap. Worth having in the record because the harness line truncates
to `Exception in thread "main" java/lang/AssertionError:` and shows none of it.

## 3. It is not WORKER 4's, established by an interleaved A/B

`base` is the handoff tip built with NO WORKER 4 commits; `r1` is the same
worktree with this lane's `java.io` changes on top. Ten runs, interleaved, one
process each, `--real-jdk --Xmx 64m` (`[ABBA]`):

```text
base:1 r1:1 base:1 r1:1 base:1 r1:1 base:1 r1:1 base:1 r1:0
```

**base 5/5 red, r1 4/5 red** — the same failure at the same rate on both sides.
The single `r1` pass is the flake, not the signal. Whole-suite arms agree:
`106/107` and `66/67` on both binaries, with this vector as the only failure.

**No attribution is offered here.** An earlier draft of this note guessed at the
commit range from what had recently landed nearby; that is exactly the inference
`WORKER-1-NOTE-1` §2 shows going wrong on this vector, and it is removed rather
than softened.

## 4. A narrower reading of trap 8, which cost this lane a contaminated run

The first appearance of the failure in this lane came with two
`HARNESS FAULT — MAIN CLASS NOT FOUND` entries beside it — the brief's own
trap-8 signature, *"total redness that INCLUDES the harness guard is an
ENVIRONMENT failure, not a defect."* It was, and the environment was this lane:
a single-vector control was launched through `run.sh` while a full sweep was
still running **in the same worktree**, and the second run recompiled
`regression-suite/build` underneath the first.

H0 PID-scoped `.guard-tmp` on 2026-08-21 and the brief now reads *"FIXED — you
may now sweep concurrently."* That is true **across worktrees**.
`regression-suite/build` is still one directory per worktree, so two `run.sh` in
the SAME worktree still collide — a narrower rule than the sentence reads, and
the one that bit here. Every number above was re-measured afterwards with one
sweep at a time.
