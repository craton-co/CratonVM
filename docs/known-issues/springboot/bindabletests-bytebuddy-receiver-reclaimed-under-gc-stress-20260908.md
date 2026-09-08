# The young sweep reclaims a live `TypeDescription$Generic$OfNonGenericType$ForLoadedType`, and Mockito reports it as its own limitation

| | |
|---|---|
| **Status** | OPEN, filed 2026-09-08. **Deterministic** — 2 of 2, same victim class, adjacent sweep cycle and free sequence. Not root-caused. |
| **Scope** | `--XX:UseGc Generational`, JIT on, `CRATONVM_DBG_GC_STRESS=4194304`. Passes without the stress interval. |
| **Reproducer** | `org.springframework.boot.context.properties.bind.BindableTests`, ~890 s; 26 of its 27 tests pass |
| **Victim** | `net/bytebuddy/description/type/TypeDescription$Generic$OfNonGenericType$ForLoadedType` — **named by the instrument, not inferred** |
| **Instrument** | the ALWAYS-ON invoke-dispatch reclaim guard (fired unarmed), plus `CRATONVM_DBG_SWEEP_ZERO=1` for the class name |

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
  obj="0x11c5ee9b988"
  original_class=net/bytebuddy/description/type/TypeDescription$Generic$OfNonGenericType$ForLoadedType
  original_kind=0  sweep_cycle=6322  gc_reason=0 gc_initiator=0 threads_blocked=0
```

Two runs, and they agree to within the noise you would expect of an allocation
sequence:

| run | sweep_cycle | free_seq | victim class | verdict |
|---|---:|---:|---|---|
| 4-class parallel, `+GC_VERIFY_RSET` | 6353 | 16948 | (not armed for the name) | FAIL, 828 s |
| single class, `+DBG_SWEEP_ZERO` | 6322 | 16909 | `…OfNonGenericType$ForLoadedType` | FAIL, 890 s |

## Why this is not the instrument

The sibling guard on this same family was just found to have a false-positive
mode — `CRATONVM_DBG_DEADRECV` consulted reclamation rings that are never pruned
when the allocator re-serves a span, so every re-allocated address answered
(`gpuresidencygc-generational-jit-reclaims-a-live-object-FIXED-20260908.md`).
That makes "which consumer fired" the first question to ask of any report in
this family, and this is a different one.

It fires at an interpreter INVOKE only when **both**:

1. the receiver's header reads `class_id == 0` — an actually zeroed object, read
   out of memory rather than remembered; and
2. `reclaimed_hole_at(addr)` finds the address on the arena's free list **right
   now** — a live view of the allocator, not a ring.

A re-allocated address fails both: it would carry its new owner's class id and
would not be on the free list. The second report is a third independent
witness: `CRATONVM_DBG_SWEEP_ZERO`'s ring recorded that *this* address was
zeroed by *that* sweep cycle, carrying the class it held at the time — which is
a real ByteBuddy type, not `java/lang/Object`.

## The failure it causes

```text
MockitoException: Mockito cannot mock this class: interface java.lang.annotation.Annotation.
Underlying exception : java.lang.IllegalArgumentException: Could not create type
  BindableTests.withAnnotationsShouldSetAnnotations(BindableTests.java:137)
```

Mockito's message is a statement about ITS OWN capabilities, which is how this
was first triaged. The reclaimed receiver is a `TypeDescription$Generic`, and
`asGenericType()` on it is exactly what "Could not create type" is thrown out
of. **Read the Mockito text as a symptom, not a diagnosis.** A suite triaging
this class as "an inline-mock limitation" would close a live reclamation.

## What was ruled out before filing

* **Not the nine natives fixed the same day**
  (`natives-hold-a-stale-reference-across-a-park-FIXED-20260908.md`). Those are
  `java.util.concurrent` wrappers holding a reference across a park or an
  allocation. This receiver reaches an invoke through ByteBuddy's type
  description and none of those natives is on the path; the report survives all
  nine fixes.
* **Not a missed write barrier.** The 4-class run emitted **16,860**
  `[rset-verify]` reports with `missing=0` in every one — the card table
  delivered every old-to-young edge it was asked for.
* **Not the off-grid sweep anchor.** Two OTHER classes in the same run reported
  `young sweep: sweep anchor(s) are NOT on the object grid` (`off_grid=1
  anchors=2`, eight times each) and both PASSED; this class reported
  `off_grid=0`. The two signals do not co-occur. (That counter is separately
  worth chasing — its own comment says it is "exactly zero on a sound anchor
  list", and it is not.)
* **Not the default configuration.** Without `CRATONVM_DBG_GC_STRESS` this class
  passes; the stress interval is what makes a collection land in the window.

## Repro

```powershell
$env:CRATONVM_DBG_GC_STRESS = '4194304'
$env:CRATONVM_DBG_SWEEP_ZERO = '1'      # names the victim class on the hit
run-spring-boot-suite.ps1 -ClassList <core/spring-boot BindableTests> -Parallel 1 `
  -TimeoutSec 900 -Vm craton -CratonArgs @('--XX:UseGc','Generational')
```

`CRATONVM_GC_VERIFY_RSET` is NOT needed and costs a full old-generation walk per
collection; the first observation carried it only because that run was auditing
the remembered set.

## Next

1. **Find who holds it.** `root_coverage="NEVER-LOOKED"` and `xt_passes=0` mean
   the cross-thread take-over never ran, which on a class with no peer threads
   is the correct and uninformative answer — it says nothing about the
   COLLECTING thread's own frames. The question is what roots a
   `TypeDescription$Generic$OfNonGenericType$ForLoadedType` at that instant:
   a ByteBuddy cache, a JIT spill slot, or a native holding it across an
   allocation.
2. **`CRATONVM_DBG_FULLSTACK_SCAN=1` is the decisive arm** and is one run.
   It scans the ENTIRE native stack conservatively rather than the JIT chain's
   own `[scanner_sp, entry_sp)` bands. If the report disappears the missed root
   WAS on the stack but outside those bounds (a range bug); if it survives, the
   reference is not on the stack at all and the next place to look is a side
   table.
3. `--nojit` will read clean and that will mean nothing: with no JIT frame the
   collector takes the MOVING path, which frees nothing into a free list, so
   this consumer's second condition can never hold. The same trap retired the
   GPU page above; do not run it as a control.
