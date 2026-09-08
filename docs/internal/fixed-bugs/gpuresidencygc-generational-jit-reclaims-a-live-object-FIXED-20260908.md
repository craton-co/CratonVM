# ✅ RESOLVED — `GpuResidencyGc` under Generational+JIT: the young sweep frees nothing, the INSTRUMENT reported re-allocated addresses

> **RESOLVED 2026-09-08.** See the final addendum: with a re-allocation screen
> in `deadrecv_check` the 8 hits go to 0, the armed arm passes, and its wall
> time collapses from 163-251 s to the unarmed arm's 4 s. The page is kept
> whole — including the title claim it turned out not to support — because the
> two earlier addenda are the record of how it got there.

Original title: `GpuResidencyGc` fails 6/6 on Generational+JIT — the young sweep frees a live object, and the sweep never looked at its coverage

| | |
|---|---|
| **Status** | RESOLVED 2026-09-08 (filed 2026-09-07). **Deterministic** — 6/6, ~26 s, single-threaded, no broker, no port, no GPU, no load dependency. |
| **Scope** | `--XX:UseGc Generational` **with the JIT on**. `--nojit` passes 6/6 with answers byte-identical to HotSpot; HotSpot passes 2/2. |
| **Symptom** | `java.util.ConcurrentModificationException` at `GpuResidencyGc.churn:76` — in a probe with **no threads** |
| **Instrument** | `CRATONVM_DBG_DEADRECV=1` reports 8 reclaimed-receiver hits per run, every run |

## Repro

```bash
javac -d /tmp/cls test_classes/gpu/GpuResidencyGc.java
CRATONVM_DBG_DEADRECV=1 $CVM --java-home $JDK --Xmx 2g \
    --XX:UseGc Generational -cp /tmp/cls GpuResidencyGc 0 1024 800
# rc=1, ConcurrentModificationException, 6/6.  Add --nojit: rc=0, 6/6.
```

This is by a wide margin the cheapest handle on the Generational reclaim
family. Everything else in it needs a Kafka broker, a free port and a
CONTENDED host (see
`generational-young-sweep-frees-an-interpreter-held-object-FIXED-20260908.md`,
whose rate runs 0/4 on a quiet box and 3/3 at load 20-35). This one is
deterministic on an idle machine in 26 seconds.

## What the guard says

`CRATONVM_DBG_DEADRECV` asks the always-on reclamation rings before the first
dereference. Eight hits per run, on `class_id_of_object` and
`identity_hash_code`, all with the same verdict:

```text
receiver is inside a YOUNG span the non-moving sweep zeroed and returned to
the free list. The span is coalesced, so `freed_span` bounds the victim
rather than naming it.
  obj=0x7ef6440170e8  site=class_id_of_object  actual_class_id=0
  target_class=java/lang/Object  freed_span="0x7ef6440170e8+0xe0"
  interior_off=0  sweep_cycle=0  free_seq=48
  root_coverage="NEVER-LOOKED"  xt_passes=0  xt_taken_over=0  xt_unclassified=0
```

**`root_coverage="NEVER-LOOKED"` on all 48 reports.** The guard's own text says
`INCOMPLETE` would mean the sweep freed on `GC_FLAG_MARKED` while a running
peer's JIT frames were in no root set. `NEVER-LOOKED` is weaker than that: the
sweep did not perform the cross-thread coverage check at all, and `xt_passes=0`
agrees. On a single-threaded probe there are no peers to classify — so the
question is what this sweep believed about its OWN thread's JIT frames.

`interior_off=0` on the reports means the stale reference is at the span's
BASE, not into the middle of a coalesced block.

## Why this is not the retired page

`internal/fixed-bugs/native-arg-snapshot-stale-across-java-reentry-FIXED-20260906.md`
retired this exact reproducer (`GpuResidencyGc 0 1024 800` under Generational)
and states its answers are byte-identical to HotSpot. **The SIGSEGV that page
fixed is indeed gone** — this is a `ConcurrentModificationException` now, not a
crash. But the probe still FAILS 6/6 with the JIT on, so its reproducer no
longer demonstrates what that page claims. Either the arm was re-measured with
`--nojit` (which does pass), or something regressed after it.

Its own fix is separately ruled out for the springboot page's defect: that one
survives `14d9a50f4` 6/8 on a tip containing it.

## Relationship to the springboot page — UNPROVEN, do not merge them

Both are "the Generational young sweep frees a live object". They differ on
the one axis that matters:

| | this page | springboot page |
|---|---|---|
| JIT | **required** (`--nojit` passes 6/6) | **not** required (`--nojit` is the arm that crashes) |
| determinism | 6/6 on an idle host | 0/4 idle, 3/3 at load 20-35 |
| face | `ConcurrentModificationException` | SIGSEGV on a decommitted span |

A single root cause would have to explain both, and nothing measured here does
yet. Treat them as two entries in one family until an arm links them.

## Next

* Name the victim. `freed_span` bounds it but the span is coalesced;
  `interior_off=0` says the reference is at the base, so the a2dbg allocation
  ring (`lookup_at` / `history_at`) should be able to say what was allocated
  there last.
* Ask why `root_coverage` is `NEVER-LOOKED` with `xt_passes=0` on a sweep that
  is about to free a JIT-reachable object. That is a question about the sweep's
  own preconditions, and it is answerable without any of the workloads above.
* `CRATONVM_DBG_SWEEP_ZERO` is blind here for the reason the springboot page
  records: it rings the young sweep but its consumer only fires at an
  interpreter INVOKE on an all-zero header.

## Addendum 2026-09-07: the symptom is the instrument, the defect is two objects

Re-measured against `005ceb951` on an idle-ish host (load ~2-8), round-robin
interleaved so neither arm owns a stretch of the machine.

### The `ConcurrentModificationException` is produced by `CRATONVM_DBG_DEADRECV`

| arm | runs | rc=1 | CME |
|---|---|---|---|
| `GpuResidencyGc 0 1024 800`, Generational, **no flag** | 5 | **0** | 0 |
| the same **plus `CRATONVM_DBG_DEADRECV=1`** | 7 | **7** | 7 |

The repro block at the top of this page carries `CRATONVM_DBG_DEADRECV=1`, so
every one of its 6/6 was armed and the unarmed arm was never run. That flag's
own doc comment says why this happens:

> Opt-in and **behaviour-changing**: a hit returns 0, which
> `identity_hash_code` otherwise never does (C28).

`GpuResidencyGc.churn` keys a hash structure on identity; an
`identity_hash_code` of 0 puts the entry in the wrong bucket, and the next
traversal raises `ConcurrentModificationException` at the line this page
names. **The pass/fail oracle was manufactured by the diagnostic.** The
`--nojit` "rc=0, 6/6" comparison inherits the same problem.

This does NOT retract the page. It moves the evidence: the guard's own hit
reports, which are read from the always-on reclamation rings BEFORE any
dereference, are independent of the return-0 behaviour and are what the rest
of this addendum uses as the metric.

### "8 hits" is 8 DEREFERENCES of 2 objects

Stable across 5 armed runs:

```
reports=8   distinct objs=2   free_seq=[48, 336]   sweep_cycle=[0]
```

Seven of the eight name **one address** (`…170e8`, `freed_span=<base>+0xe0`,
`free_seq=48`) at `class_id_of_object` / `identity_hash_code`; the eighth is a
single other address with `free_seq=336`. The guard reports per dereference,
not per victim, so "eight reclaimed-receiver hits" reads as a population and is
a report count. **Two objects are being freed, both in the FIRST sweep cycle.**

`interior_off` is also not uniformly 0 as stated above: the `free_seq=336`
record has `interior_off=4974048` into a `0x643900` span. Only the
seven-times-dereferenced victim sits at its span base.

### `root_coverage="NEVER-LOOKED"` cannot answer the question this page asks it

The "Next" item asking why coverage is `NEVER-LOOKED` "on a sweep that is about
to free a JIT-reachable object" is chasing a counter that structurally cannot
speak to it. `xt::take_over_pass` skips `self_tid`:

```rust
for tid in list_thread_tids() {
    if tid == self_tid || taken.contains(tid) { continue; }
```

so `xt_passes` / `xt_taken_over` / `xt_unclassified` describe **peers only**. On
a probe with no threads there are none, and `NEVER-LOOKED` is the correct and
uninformative answer. `stw_takeover_should_scan`'s own comment says the gating
hint "is a single process-global depth and cannot distinguish 'a peer is in JIT'
from 'I am'". Whatever covers the COLLECTING thread's frames, it is not this.

### Mechanisms ruled out — 7 ablations, all `hits=8`

Each run armed, one variable changed, deterministic metric:

| lever | hits |
|---|---|
| baseline | 8 |
| `CRATONVM_GC_SWEEP_ANCHOR_STRIDE=2^40` (collapses the anchor list below the `len() > 2` gate, forcing the sequential sweep) | 8 |
| `CRATONVM_DBG_NO_JIT_ROOT_SCAN=1` (disables the conservative JIT frame scan **entirely**) | 8 |
| `CRATONVM_JIT_A5_RESIDUE_FILTER=0` | 8 |
| `CRATONVM_JIT_UNREG_ACCEPT_RESIDUE=1` | 8 |
| `CRATONVM_GC_NO_TLAB_SKIP=1` | 8 |
| `CRATONVM_NO_JIT_INLINE_TLAB_NEW=1` | 8 |
| `CRATONVM_GC_CONDITIONAL_TLAB_SKIP_PUBLISH=1` | 8 |
| **`--nojit`** | **0** |

The positive control is the third row and it is the informative one:
**turning the conservative JIT root scan off changes nothing**, while removing
the JIT removes the defect. So the victims are not objects the JIT root scan
fails to find — that scan's presence or absence is irrelevant to them. The
off-grid sweep-anchor guard (`gen_heap.rs:13122`, "a chunk beginning here can
parse phantom objects and reclaim every live object they subsume") was the most
promising candidate on paper and is refuted by row 2: the walk shape does not
matter, which is consistent with the two objects being genuinely UNMARKED
rather than mis-parsed.

Also checked: the precise-oop-map suppression, which does drop the conservative
roots it gathered when the proof holds (`memory/roots.rs:1415-1439`), is
**opt-in** behind `CRATONVM_GC_PRECISE_ONLY_ROOTS=1` and off by default here —
so `scan_active_jit_frames` runs on every default young collection and no
suppression is in play.

### Where that leaves it

Two objects, freed in `sweep_cycle=0` at `free_seq` 48 and ~336, deterministic,
present only with the JIT on, and not attributable to the root scan, the TLAB
skip spans, the anchor list, or the precise-only suppression. Naming them is
still the first move — the page's own suggestion — but for a sharper reason
than before: with exactly two victims and a stable `free_seq`, the a2dbg
allocation ring should identify them outright rather than bounding them.

## Addendum 2026-09-08: RESOLVED — every one of the 8 hits was a RE-ALLOCATED address

This page's last standing piece of evidence was the guard's own hit reports,
which the previous addendum kept precisely because they are read from the
always-on reclamation rings BEFORE any dereference and so survive the
return-0 artefact. They do not survive the next question.

**Neither ring is pruned when the allocator hands a freed span back out.**
`record_young_span_freed` appends one entry per coalesced span and nothing ever
removes it, so every object later allocated inside that span answers
`young_freed_lookup` for the rest of the process. The rings remember what was
freed, not what is dead. `deadrecv_check` now asks `is_object_address` first —
the arena's object-start bitmap, which records "a base this arena handed out and
has NOT freed" — and only consults the rings for an address that is not a
current allocation.

Measured on this page's own command, Windows 11, `--Xmx 2g`:

| arm | rc | guard hits | wall |
|---|---:|---:|---:|
| **before**, `Generational`+JIT, `DEADRECV=1` | 1 (CME) | **8, 8, 8** | 163 / 185 / 251 s |
| **before**, `Generational` `--nojit`, `DEADRECV=1` | 0 | 0, 0 | 428 / 475 s |
| **before**, `Generational`+JIT, unarmed | 0 | 0 | 4 / 4 / 4 s |
| **after the screen**, `Generational`+JIT, `DEADRECV=1` | **0** | **0, 0, 0** | **4 / 4 / 4 s** |
| **after the screen**, `Generational`+JIT, unarmed | 0 | 0 | 3 / 4 / 4 s |

The armed arm is now indistinguishable from the unarmed one, in verdict, in
hit count and in wall time. There is no remaining evidence on this page that
anything live was reclaimed, so **this page is retired.**

### Two things worth carrying forward

**The `--nojit` split was never about the JIT.** Only the NON-MOVING sweep calls
`record_young_span_freed`; the moving Cheney cycle resets from-space wholesale
and rings nothing. Whether a JIT frame is live is exactly what selects the
non-moving sweep. So "8 hits with the JIT, 0 with `--nojit`" is a statement
about WHICH COLLECTOR RAN, and it would read the same on a VM with no defect at
all. That is why the seven ablations in the previous addendum all read 8: every
one of them left the collector choice alone, and the positive control (`--nojit`)
changed the collector rather than the coverage.

**The guard changed the workload it measured, by a factor of 40-60.** The
unarmed run finishes in 4 s and the armed one took 163-251 s, because
`deadrecv_check` scanned both rings linearly on every `identity_hash_code` and
`class_id_of_object` — and the old-gen ring is 2^20 entries. The young ring's own
header comment says the gated, mutex-backed forensics it replaced were rejected
for exactly this reason ("The instrument changed the thing it measured, which is
a documented cause of this family's 'reproduces on plain runs, never on
instrumented ones' history"). The write side is O(1) and lock-free as designed;
the READ side was not, and nothing said so. With the screen the ring scan is
reached only for an address that is not a live allocation, and the armed cost is
back to the unarmed cost.

**What was NOT wrong with this page.** Filing it was right, the previous
addendum's retraction of the CME oracle was right, and its refusal to merge with
the springboot page was right — those two really were different things, and the
springboot one really was a defect (nine natives, fixed in
`natives-hold-a-stale-reference-across-a-park-FIXED-20260908.md`). What this page
was missing is that its surviving metric had a false-positive mode nobody had
asked about.
