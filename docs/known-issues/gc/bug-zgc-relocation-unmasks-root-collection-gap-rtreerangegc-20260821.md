# `RTreeRangeGc` reddens on the default collector — the gap is old, the *coverage* is new

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

Deterministic: 3/3 with the merged binary, 0/3 without. `stdout` is empty, so
the vector publishes no check count and the cross-VM diff compares two empty
strings — which is why harness guards **G2 and G3 also fire** on it. Those two
are consequences of the rc=1, not separate defects.

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
* `--nojit` reproduces, so this is **not** JIT root scanning. It is the
  interpreter frame root set.
* `-XX:+UseG1GC` **passes**, so the affected path is the young/relocating one
  the other two collectors share and G1 does not take here.

`RTreeRangeGc.checkMap` walks TreeMap/TreeSet range views. This area has prior
form: `b2e13e441 fix(collections): TreeMap/TreeSet snapshot walks dereferenced
relocated ObjectRefs` is the same shape — a snapshot walk holding an address
across a relocation.

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
