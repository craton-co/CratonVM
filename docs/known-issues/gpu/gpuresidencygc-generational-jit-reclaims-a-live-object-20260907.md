# `GpuResidencyGc` fails 6/6 on Generational+JIT — the young sweep frees a live object, and the sweep never looked at its coverage

| | |
|---|---|
| **Status** | OPEN, filed 2026-09-07. **Deterministic** — 6/6, ~26 s, single-threaded, no broker, no port, no GPU, no load dependency. |
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
`springboot/generational-young-sweep-frees-an-interpreter-held-object-20260906.md`,
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
