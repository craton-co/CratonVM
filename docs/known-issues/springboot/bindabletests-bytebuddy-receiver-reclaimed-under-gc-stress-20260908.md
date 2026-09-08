# The young sweep reclaims a live `TypeDescription$Generic$OfNonGenericType$ForLoadedType` under GC stress — and it is NOT what fails the test

| | |
|---|---|
| **Status** | OPEN, filed 2026-09-08. **Pre-existing** — reproduces identically on the 2026-09-07 `dev` tip, 5 of 6 runs. Not root-caused. |
| **Scope** | `--XX:UseGc Generational`, JIT on, `CRATONVM_DBG_GC_STRESS=4194304`. Passes without the stress interval. |
| **Reproducer** | `org.springframework.boot.context.properties.bind.BindableTests`, 520-890 s; 26 of its 27 tests pass |
| **Victim** | `net/bytebuddy/description/type/TypeDescription$Generic$OfNonGenericType$ForLoadedType` — **named by the instrument** |
| **Instrument** | the ALWAYS-ON invoke-dispatch reclaim guard (fires unarmed), plus `CRATONVM_DBG_SWEEP_ZERO=1` for the class name |

## The report

```text
ERROR cratonvm::gc::guard: receiver is inside a YOUNG span the non-moving sweep
zeroed and returned to the free list.
  obj="0x11c5ee9b988"  site="invoke dispatch"  actual_class_id=0
  target_class=java/lang/Object.asGenericType()Lnet/bytebuddy/description/type/TypeDescription$Generic;
  freed_span="0x11c5ee9b988+0x28"  interior_off=0
  sweep_cycle=6322  free_seq=16909
  root_coverage="NEVER-LOOKED"  xt_passes=0 xt_taken_over=0 xt_unclassified=0

ERROR cratonvm::gc::guard: …and it was RECLAIMED BY THE YOUNG SWEEP while still
reachable. The original class names the root-coverage gap.
  original_class=net/bytebuddy/description/type/TypeDescription$Generic$OfNonGenericType$ForLoadedType
```

## It is PRE-EXISTING, and the sweep cycle proves it

| binary | runs | reclaim seen | victim class | sweep_cycle | test |
|---|---:|---:|---|---|---|
| `dev` tip of 2026-09-07 20:29 (**pre-fix**) | 1 | **1** | `…OfNonGenericType$ForLoadedType` | **6353** | FAIL |
| the nine park/allocation fixes, pre-merge | 2 | 2 | same | 6353 / 6322 | FAIL |
| those fixes merged with `dev` of 2026-09-08 | 3 | 2 | same | — | FAIL |

**5 of 6 runs**, across three binaries.

Same victim class at the same point in the allocation sequence on a binary built
the day before any of this work. Nothing in
`natives-hold-a-stale-reference-across-a-park-FIXED-20260908.md` caused it and
nothing there cured it.

## The Mockito failure is NOT this — that inference was wrong

The first revision of this page said the class's failure —

```text
MockitoException: Mockito cannot mock this class: interface java.lang.annotation.Annotation.
Underlying exception : java.lang.IllegalArgumentException: Could not create type
  BindableTests.withAnnotationsShouldSetAnnotations(BindableTests.java:137)
```

— "is this receiver and not the Mockito limitation the message names", because
`asGenericType()` on a `TypeDescription$Generic` is what "Could not create type"
comes out of, and the two co-occurred twice.

**A third run refutes it.** On the merged tree the test failed with the same
Mockito message and the guard reported **nothing at all**: `guard=0`, no
invoke-dispatch line, no sweep-zero record. The failure happens with and without
a reclaim, so the reclaim is not its cause. Two co-occurrences were a
coincidence of a deterministic workload, not evidence.

That leaves **two independent things in this class**, and they should be chased
separately:

1. **the reclaim** — real, pre-existing, intermittent (5 of 6 runs);
2. **the Mockito failure under GC stress** — also pre-existing, and so far
   unexplained; the class passes without `CRATONVM_DBG_GC_STRESS`.

## Why the reclaim itself is not an instrument artefact

The sibling guard on this family WAS found to have a false-positive mode —
`CRATONVM_DBG_DEADRECV` consulted reclamation rings that are never pruned when
the allocator re-serves a span
(`gpuresidencygc-generational-jit-reclaims-a-live-object-FIXED-20260908.md`).
So "which consumer fired" is the first question to ask of any report here, and
this is a different one. It fires at an interpreter INVOKE only when **both**:

1. the receiver's header reads `class_id == 0` — an actually zeroed object, read
   out of memory rather than remembered; and
2. `reclaimed_hole_at(addr)` finds the address on the arena's free list **right
   now** — a live view of the allocator, not a ring.

A re-allocated address fails both. The sweep-zero record is a third witness: its
ring recorded that *this* address was zeroed by *that* cycle, carrying the class
it held at the time.

## What was ruled out

* **Not a missed write barrier.** The run this came from emitted **16,860**
  `[rset-verify]` reports with `missing=0` in every one.
* **Not the off-grid sweep anchor.** Two OTHER classes in the same suite run
  reported `young sweep: sweep anchor(s) are NOT on the object grid`
  (`off_grid=1 anchors=2`, eight times each) and both PASSED; this class reports
  `off_grid=0`. (That counter is separately worth chasing — its own comment says
  it is "exactly zero on a sound anchor list", and it is not.)
* **Not the default configuration.** Without the stress interval this class
  passes.

## Repro

```powershell
$env:CRATONVM_DBG_GC_STRESS  = '4194304'
$env:CRATONVM_DBG_SWEEP_ZERO = '1'      # names the victim class on the hit
run-spring-boot-suite.ps1 -ClassList <core/spring-boot BindableTests> -Parallel 1 `
  -TimeoutSec 1800 -Vm craton -CratonArgs @('--XX:UseGc','Generational')
```

`CRATONVM_GC_VERIFY_RSET` is NOT needed and costs a full old-generation walk per
collection.

## The most specific lead: three predicates for one question

Three places ask "is the NON-MOVING sweep the collector that will run?", and
they do not agree:

| site | predicate |
|---|---|
| `gen_heap.rs` `has_conservative_roots` — **the collector's own choice** | `is_active() \|\| unregistered_jit_frame_on_stack()` |
| `roots.rs` `conditional_loader_metadata` (Generational) | `is_active() \|\| unregistered_jit_frame_on_stack() \|\| major_gc_requested()` |
| `roots.rs` `conservative_locals_enabled` | **`is_active()` only** |

The third is the hardening that conservatively probes frame slots for
lost-tag object references, and its own doc gives the reason it is safe: "the
only mode in which a false-positive root is harmless (nothing is relocated)".
That is equally true on the A5 unregistered-frame path — which is *why* the
collector forces the non-moving sweep there — yet the hardening does not engage
on it. So on that path the sweep frees on `GC_FLAG_MARKED` while the pass that
exists to widen its root set is off.

**This is a lead, not a fix, and widening the predicate naively is wrong.**
`unregistered_jit_frame_on_stack()` for the CURRENT cycle is only known after
`scan_active_jit_frames`, which runs in section 14 of `collect_roots` — after
the frame scan in section 1 that would consume it. Reading it where
`conservative_locals` is computed today yields the PREVIOUS cycle's answer, and
enabling the probe on a cycle that then takes the MOVING path is unsound by the
same comment ("a pointer-shaped `long` rooted here would be relocated and
corrupted"). Making this correct means reordering the root scan, which wants a
reproduction that pins the mechanism first.

## Next

1. **Establish whether the victim is a lost-tag operand at all.** If it is, the
   predicate above is the answer; if it is not, that lead is dead and the
   question is what else holds a `TypeDescription$Generic`.
2. `CRATONVM_DBG_FULLSTACK_SCAN=1` — scans the ENTIRE native stack
   conservatively rather than the JIT chain's own bands. If the report
   disappears the missed root WAS on the stack but outside those bounds; if it
   survives, the reference is not on the stack at all. One run, and it was
   started and abandoned for time here.
3. **Do not run `--nojit` as a control.** With no JIT frame the collector takes
   the MOVING path, which frees nothing into a free list, so this guard's second
   condition can never hold and the arm reads clean whatever the truth is. That
   exact false control kept the GPU page above open for a day.
4. Separately: bisect the Mockito failure, which is now known not to be this.
