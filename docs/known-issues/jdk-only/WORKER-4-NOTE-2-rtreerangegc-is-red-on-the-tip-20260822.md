# WORKER-4-NOTE-2 — `RTreeRangeGc` is RED on the handoff tip: `headMap(k,false)` answers 0 entries where 300 are live

**MEASURED 2026-08-22, Linux (Azure host 2), Temurin 25.0.4+7.** Filed by
WORKER 4, whose subject is `java.io` and who has no claim on this surface. It is
here because this lane hit it while landing, established whose it is, and should
not be the only party that knows.

## The failure

```text
SUITE=all (COMPATIBLE mode)   RTreeRangeGc  FAIL
  AssertionError: RTreeRangeGc: headMap(k,false): 0 entries, expected 300
```

`--jdk-only` is GREEN. Only the compatible arms (`SUITE=all`, `SUITE=core`)
fail.

## Reproducing it WITHOUT the harness

The vector passes when run plainly, which is why it is easy to dismiss:

```text
cratonvm --jdk-only  … RTreeRangeGc     rc=0, PASS (14014 checks)
cratonvm --real-jdk  … RTreeRangeGc     rc=0, PASS (14014 checks)
```

**It needs the launcher flag `harness-guard.sh::class_cv_args` gives it**, which
that file documents in as many words — *"Two members — `RPriorityQueueGc` and
`RTreeRangeGc` — are INERT without the CratonVM-only launcher flags
`class_cv_args()` supplies for them. Scheduling either one without that hook
manufactures a green gate."* For this vector the flag is `--Xmx 64m`:

```bash
cratonvm --real-jdk --Xmx 64m --java-home "$JAVA_HOME" -cp build RTreeRangeGc
```

That is the whole reproduction. **A plain run of this vector is a green gate**,
and this note exists partly so the next person does not conclude "cannot
reproduce" from one.

## Whose it is: an interleaved A/B, ten runs

`base` is the handoff tip `d235cced9` built with NO WORKER 4 commits; `r1` is
the same worktree with this lane's `java.io` changes on top. Runs interleaved,
one process each (`[ABBA]`):

```text
base:1 r1:1 base:1 r1:1 base:1 r1:1 base:1 r1:1 base:1 r1:0
```

**base 5/5 red, r1 4/5 red.** The same failure at the same rate on both sides,
so it is not this lane's, and the single `r1` pass is the flake rather than the
signal — `[0/12 does not close a 1-in-10 bug]`, read in the other direction.

## Why this is worth a note rather than a shrug

`headMap(k, false)` returning **0** entries where 300 are live is not a
tolerance failure or an ordering difference. It is a view that lost its whole
backing, under memory pressure, in COMPATIBLE mode only.

That is the surface the commits between `11634cd44` and `381a14036` rebuilt:
*"`TreeMap.root` is a real red-black tree of real `TreeMap$Entry`"* and
*"`TreeSet.m` is a real backing `TreeMap`"*. A real node graph is a graph the
COLLECTOR must now trace, and `--Xmx 64m` is precisely the configuration that
makes it collect. `[refuse2move=vacuous green]` names the neighbouring hazard —
a collector that declines to move turns GC vectors green — and this is its
mirror: a collector that DOES move, over a graph that just became real.

**Two things a lane picking this up should not have to re-derive:**

1. `--jdk-only` being green proves nothing here. The strict arm refuses a large
   part of the collection surface, so it is not exercising the same object
   graph.
2. Run it at `--Xmx 64m` and interleave against a pristine build. A single run
   of either binary can come back either way.

## What this lane did NOT do

Diagnose it. `native-collections` is WORKER 2's file and the GC roots are
somebody else's again; this note carries the reproduction, the A/B and the
assertion, and stops there.

## A trap this lane walked into while establishing the above

The FIRST time this failure appeared it came with two `HARNESS FAULT — MAIN
CLASS NOT FOUND` entries beside it, and that shape is the brief's own trap 8:
*"Total redness that INCLUDES the harness guard is an ENVIRONMENT failure, not a
defect."* It was — **and the environment was this lane**. A single-vector control
was launched through `run.sh` while a full sweep was still running in the same
worktree, and the second run recompiled `regression-suite/build` underneath the
first.

H0 PID-scoped `.guard-tmp` on 2026-08-21 and the brief now says concurrent
sweeps are safe. They are safe **across worktrees**. `regression-suite/build` is
still one directory per worktree, so two `run.sh` in the SAME worktree still
collide — which is a narrower rule than "you may now sweep concurrently" reads,
and worth stating that way.

Every number above was re-measured after that, with one sweep at a time.
