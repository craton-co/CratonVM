# `RTreeRangeGc` reddens on the default collector — the gap is old, the *coverage* is new

> **2026-08-22 — this vector carries TWO defects, and this record describes one.**
> The generational column of the table below is a **different** defect from the
> default column's: on generational the first failure is `subMap(k, k)`
> returning an **empty view** (`0 entries, expected 200`) on its FIRST walk,
> not a stale address decoding as another object. Different operation, different
> symptom. Fixing either will not green the vector.
> See `jdk-only/WORKER-1-NOTE-2-…-20260822.md` §4c.

## What is failing

`regression-suite` vector `RTreeRangeGc`, which the harness runs with
`--Xmx 64m` (`class_cv_args` in `harness-guard.sh`), on the **default**
collector:

```text
RTreeRangeGc   FAIL  rc=1: unclassified: ERROR cratonvm::gc::guard: …
```

```text
ERROR cratonvm::gc::guard: …and this is where that address stood in the OWNING
thread's own GC bookkeeping. obj="0x16e0a184d18" site="checkcast"
in_published_snapshot=true published_roots=893 last_publish_at_collection=1
collections_now=2 last_publish_pc=17 holder=<not found in frames>
in_blocked_region=false frames=2 top_frame=RTreeRangeGc.checkMap pc=44
```

`stdout` is empty, so the vector publishes no check count and the cross-VM diff
compares two empty strings — which is why harness guards **G2 and G3 also fire**
on it. Those two are consequences of the rc=1, not separate defects.

> **CORRECTION (2026-08-22): "deterministic" is true of the ISOLATED repro and
> NOT of the suite.** This paragraph originally read "Deterministic: 3/3 with the
> merged binary, 0/3 without." The isolated claim still holds — 3/3 on r12, r13
> and r14, 0/3 on r5/r10/r11 — but a single `r14` corpus run reddened it in
> `SUITE=all` and **passed it in `SUITE=core`**, same binary, same flags,
> minutes apart:
>
> ```text
> r14   --jdk-only   106 / 107   RLangPackages                  (RTreeRangeGc PASSED)
> r14   SUITE=all    105 / 107   RLangPackages  RTreeRangeGc
> r14   SUITE=core    66 /  67   RLangPackages                  (RTreeRangeGc PASSED)
> ```
>
> **RE-CORRECTED 2026-08-22, later: one outlier in eight, not “intermittent”.**
> Four corpus runs on this host, two compatible arms each:
>
> ```text
> r13  all FAIL   core FAIL
> r14  all FAIL   core PASS   <- the single outlier, and the one this note was written from
> r15  all FAIL   core FAIL
> r16  all FAIL   core FAIL
> ```
>
> Seven of eight compatible observations FAIL and every strict one PASSES, so
> the honest reading is **deterministic and mode-dependent, with a rare
> flake** — not “intermittent”, which is what one outlier looked like at the
> time. `WORKER-5` reached the same conclusion from 12 ABBA-interleaved runs
> on their own binary (strict PASS x6, compatible FAIL x6) and their framing
> is the one to keep: **“flaky” is a property of a BINARY AND A HOST, not of a
> vector.** Both of us called it a flake first, from different binaries.
>
> The practical rule is unchanged: judge before/after on the isolated
> `--Xmx 64m` arm, three runs, not on one corpus cell — a single green corpus
> run does NOT show the gap closed. That is the arm the binary table below was
> measured on.
>
> The `--jdk-only` PASS is a separate matter and is NOT the flake: it reproduces
> 2/2 deliberately, and the mechanism is in the mode section further down.

## The gap is NOT new, and it is not the merge's

Four binaries spanning two days, two collectors, same host, same fixture:

| binary | built | generational | default (ZGC) |
| --- | --- | --- | --- |
| `cratonvm-r5`  | 2026-08-20 21:57 | **FAIL** | pass |
| `cratonvm-r10` | 2026-08-21 12:57 | **FAIL** | pass |
| `cratonvm-r11` | 2026-08-21 18:42 | **FAIL** | pass |
| `cratonvm-r12` | 2026-08-21 22:22 (dev merged in) | **FAIL** | **FAIL** |

The generational column never changed. The defect predates every commit on
`claude/jdk-only-mode-handoff-09b48c` and predates the 219 dev commits merged
into it. **Only the default column moved.**

## The mechanism: the green was vacuous, not earned

The gap fires whenever the collector actually **relocates** the object. The
generational collector always relocates, so it has always been red. ZGC
*refused* to compact whenever the JIT was warm — the defect fixed on dev by
`4e8e8afe5` ("ZGC refused to compact whenever the JIT was warm, and threw
OutOfMemoryError on a 97%-free heap"). A collector that never moves anything
cannot expose a stale-address bug, so the vector passed **for the wrong
reason**.

Two controls establish this without building pristine dev:

* **The knob alone does not explain it.** `CRATONVM_ZGC_RELOCATE=1` on `r11`
  still **passes** — the old code's refusal was not overridable by the flag,
  which is exactly what `4e8e8afe5` describes. On `r12`, `=0` passes and `=1`
  fails.
* **Take the JIT warmth away and the OLD binaries go red too.** If the refusal
  was gated on JIT warmth, then `--nojit` should relocate and fail on the old
  binaries. Predicted before running, then measured:

  ```text
  r11 --nojit  default-GC  rc=1   (no PASS line)
  r10 --nojit  default-GC  rc=1   (no PASS line)
  ```

So the ordering is: the root-collection gap has been there all along; ZGC's
refusal to compact hid it on the default collector; dev fixed the refusal; the
gap is now visible where the corpus actually looks.

**`RTreeRangeGc`'s green has been vacuous since ZGC became the default on
2026-08-10.** For eleven days this vector asserted 14,014 checks against a
collector that never moved an object, which is the one condition it exists to
stress. This is the same species as
`a-gc-coverage-verdict-that-is-never-computed-reads-as-proven` — a gate whose
subject was switched off underneath it reads as a pass.

## What the defect itself is

A root **collection** gap, not a mark or sweep one: the snapshot the collector
marks the thread from did not contain a slot the thread's own frames hold.

* `site="checkcast"`, `top_frame=RTreeRangeGc.checkMap pc=44`, `frames=2`
* `holder=<not found in frames>` — the guard could not attribute the address to
  any frame slot it knows about
* `--nojit` reproduces, so this is **not** JIT root scanning.
* `-XX:+UseG1GC` **passes**, so the affected path is the young/relocating one
  the other two collectors share and G1 does not take here.

**CORRECTION (2026-08-22).** This bullet originally continued "It is the
interpreter frame root set." That was too broad, and the mode arm says so:

```text
--Xmx 64m                 rc=1  2/2   (no PASS line)
--Xmx 64m --jdk-only      rc=0  2/2   PASS RTreeRangeGc (14014 checks)
```

> **REFUTED 2026-08-22 — strict mode is not clean, it is LESS SENSITIVE.**
> The two-row arm below was measured at one heap size. Squeeze it and
> `--jdk-only` goes red: **`--Xmx 48m` FAIL 2/2, `--Xmx 32m` FAIL 2/2**, and
> `--Xmx 64m` itself flaked to FAIL on one of two runs. So the `pass` is a
> threshold, not immunity, and **every inference in the rest of this section
> rests on it**. The unrooted reference cannot be attributed to the substituted
> collection natives on this evidence, because the gap survives their removal.
> Measured on a pristine `origin/dev` build at `bf85389bb`; see
> `jdk-only/WORKER-1-NOTE-2-rtreerangegc-range-views-and-a-repro-that-disagrees-20260822.md`.

**Strict mode is CLEAN.** `--jdk-only` drops the substituted collection natives
and runs the JDK's own `TreeMap`/`TreeSet` bytecode, and the gap goes with them.
So the unrooted reference is held by CratonVM's own collection substitution
across a relocating collection, not by interpreter frames in general — which is
why the corpus's `--jdk-only` arm stayed green at 106/107 while `SUITE=all` and
`SUITE=core` went red. That also makes this the SIXTH Compatible-mode defect
this week that strict mode does not have, and an argument for the mode rather
than a cost of it.

`RTreeRangeGc.checkMap` walks TreeMap/TreeSet range views. This area has prior
form: `b2e13e441 fix(collections): TreeMap/TreeSet snapshot walks dereferenced
relocated ObjectRefs` is the same shape — a snapshot walk holding an address
across a relocation.

### The obvious candidate is DISPROVED (2026-08-22)

WORKER-4 landed `420d5117a fix(collections): TreeMap.root is a real red-black
tree of real TreeMap$Entry` and `83d3c296e fix(collections): TreeSet.m is a real
backing TreeMap` — changes to exactly the substitution layer the mode arm
localises this gap to. Predicted that they might close it, then measured:

```text
r13 (before WORKER-4)  --Xmx 64m   rc=1  3/3
r14 (after  WORKER-4)  --Xmx 64m   rc=1  3/3
```

**Unchanged.** So the unrooted reference is NOT the map's root/entry structure,
which is now real on both counts. That leaves the range-VIEW machinery
(`checkMap` walks `subMap`/`headMap`/`tailMap` and `navigableKeySet`) as the
remaining suspect, and it is the same object the prior fix in this area named:
a snapshot walk holding an address across a relocation. Whoever picks this up
starts there, and does not re-try the backing store.

## Why this was landed rather than held

The merge introduces no defect; it inherits dev's newly-honest collector. Dev's
own tip carries `4e8e8afe5` and the same pre-existing gap, so this vector is
red on dev independently of this branch — merging 163 commits changes nothing
about that. Suppressing the vector, or pinning it to `-XX:+UseG1GC` to recover
a green board, would restore exactly the vacuous pass documented above.

**Not verified:** pristine `origin/dev` was not built and run (a ~57-minute
LTO build). The claim that dev is already red rests on the table above plus the
two controls, not on a direct observation of dev's own binary. That is the one
loose end here, and it is cheap for anyone who already has a dev build.

## Reproduce

```bash
cratonvm --java-home "$JDK" --Xmx 64m -cp regression-suite/build RTreeRangeGc
```

Expect `rc=1` and no `PASS` line. Then `CRATONVM_ZGC_RELOCATE=0` on the same
binary to watch it pass, and `-XX:+UseG1GC` for the other passing arm.
