# `ResourceLeakDetectorTest` — ZGC reads a `DefaultResourceLeak` the slide moved away

**Status: OPEN.** Still SIGSEGVs under ZGC on `dev` @ `a7f891a5a`+. This page
records what is now *measured* rather than inferred, two defects fixed along
the way (neither of which closes this one), and one instrument that finally
made the crash say something.

Parent: the ZGC-only SIGSEGV cluster
(`zgc-specific-sigsegv-cluster-20260814.md`), of which this is the last open
member — 7 of 8 closed on 2026-08-15.

## What the crash is, exactly

```
zgc corpse read: this address was vacated by the LAST slide
  read_addr=2733782942912  vacated_base=2733782942912  interior_offset=0
  moved_to=2733782371072   size=176   cycles_ago=0     index=0
  class=io/netty/util/ResourceLeakDetector$DefaultResourceLeak
  survivor_still_registered=true
```

then

```
EXCEPTION_ACCESS_VIOLATION (SIGSEGV), faulting read at 0x0F
  at io/netty/util/ResourceLeakDetectorTest$1.run
  at io/netty/util/ResourceLeakDetector$DefaultResourceLeak.<init>
```

Read as a sentence: **something is still holding the pre-move address of a
`DefaultResourceLeak`, and it touches field 0 (the referent) one cycle after
the slide moved that object.** `DefaultResourceLeak extends WeakReference`,
which is why this class and not the other 656 in the suite.

`survivor_still_registered=true` is the load-bearing field. The object is alive
and well at `moved_to`. So this is a **missing remap, not a lifetime bug** — a
distinction that was unavailable before and rules out a whole family of
hypotheses.

## Why this took an instrument

`compact_low_to` **zeroes the span it vacates**, deliberately, so a
conservative scan cannot resurrect a corpse. A stale pointer therefore resolves
to a well-formed ALL-ZERO object — `class_id=0`, `num_slots=0`, no fields — and
the only diagnostic it can produce is `zgc real: field index OOB index=N
num_slots=0`, which identifies nothing. Every identifying byte is destroyed by
the same memset that makes the bug survivable. Three prior investigations
reached "something holds a stale address" and stopped there.

`CRATONVM_DBG_ZGC_CORPSE=1` keeps a ledger of what each slide vacated —
destination, class id, size, and which cycle — and reports it when a read lands
on a zeroed span, together with `root_source=` from the root-attribution
registry. The ledger accumulates across cycles rather than keeping only the
last, because "read in the same cycle" and "cached ten collections ago" produce
the identical warning and are different bugs.

Backtrace capture is wired in and is **useless in this tree**: fat LTO plus
`debug = "line-tables-only"` renders every frame `<unknown>`. The VM's own
crash dump supplies Java frames instead. Worth knowing before reaching for
`Backtrace::force_capture` in a release build here.

## Hypotheses eliminated, by measurement

| hypothesis | verdict | evidence |
|---|---|---|
| the rewrite pass misses a reference slot | **no** | `missed_rewrites=0` (slide verifier) |
| stale words alias live objects | **no** | `aliasing_a_survivor=0` |
| compaction is not the trigger | **no**, it is | `CRATONVM_ZGC_RELOCATE=0` → 5/5 clean; on → 0/15 |
| the object died and this is a lifetime bug | **no** | `survivor_still_registered=true` |
| JIT frames are the only holder | **no** | `--nojit` reduces but does not remove |
| ZGC's heap-internal `ReferenceProcessor` holds it | **no** — it is EMPTY | `ZgcRealHeap::discover_reference` has no caller in the VM |
| the VM-level `ref_processor` is never remapped | **no** | `ref_proc.update_after_gc` runs at the end of `process_references_after_gc` |
| some collection path skips that remap | **no** | all six `collect_garbage_with_finalizers` sites call it; the four bare `collect_garbage` sites are tests |

## Two defects found and fixed on the way — neither closes this

**1. ZGC's heap-internal reference-processor tables were never remapped.**
`ref_processor` stores `reference_obj` / `referent` / `queue_addr` as raw
addresses; `relocate_and_compact`'s rewrite cannot see collector-side tables,
and nothing called `ReferenceProcessor::update_after_gc`. The comment said why:
"(Non-moving: addresses are stable...)" — true when written, false since
2026-08-13. Measured 84 of 106 entries stale per collection on a four-page
fixture.

**Correctly fixed, and inert in production.** `ZgcRealHeap::discover_reference`
is `pub` and has **no caller anywhere in the VM** — every registration goes to
`shared.mem.ref_processor` instead. So the table it repairs is empty in a real
run. Stating this plainly because the first commit of the pair overstated it.

That inertness is itself worth a look by someone: with the heap processor
empty, `reference_object_addresses()` returns nothing, so ZGC's weak-referent
**skip set is always empty** and the marker traces referents as strong edges.
Whether the VM-level pass compensates is not established here.

**2. `resurrected_finalizers` reported pre-slide addresses.** Filled during the
remark, several phases before the slide, never rewritten, under a comment
reading `// non-moving: address unchanged`. The runtime enqueues these on the
`FinalizerThread`; G1 documents the contract as "the POST-copy addresses".
Measured 73 of 88 at a dead address per collection. **This one is on a live
path** (`collect_garbage_with_finalizers` is what the VM calls) and stands on
its own merits.

## Where to look next

The holder is not either reference processor and not the Java slot graph. What
is left is a **collector-side or runtime-side structure that stores a
`Reference` address and outlives the cycle**. The corpse line's new
`root_source=` field answers this directly on the next run: a named source
means that source's remap half is incomplete; `<none>` means the holder never
hands the address to the marker at all, which is a different and narrower
search.

Note the family pattern before starting. Five defects so far, every one code
that was correct while the collector never moved an object:

1. `pin_critical_region` pinned nothing under ZGC
2. the reference-processing guard tested the pre-move address
3. `prune_dead` ran with pre-slide addresses
4. the heap-internal reference-processor tables (inert)
5. `resurrected_finalizers`

1–3 are within-cycle and were found by their crashes. 4–5 are cross-cycle,
found by asking **which structure outlives the cycle that filled it** — which
is the question to keep asking here.

## Measurement traps recorded

* **The `field index OOB` warning count is not a severity metric.** A run that
  dies early logs fewer warnings, so comparing counts between arms measures how
  long each survived. Use completion rate over 15+ reps; 5 cannot separate 0/5
  from 2/5 on this class.
* **A relocation test must keep page occupancy under 25%.** The selector's
  `max_live_occupancy` is 0.25, so a fixture with 40% live pages declines every
  page, moves nothing, and passes for a reason unrelated to the fix.
* **Do not measure non-vacuity from the thing under test.** Deriving "did
  anything move" from a list the bug leaves un-remapped yields zero, and the
  test then fails on its own vacuity guard — which reads as a broken fixture
  rather than as the defect. Measure from the pointer map.
* **Do not edit sources while a release build runs.** Two binaries in this
  session had uncertain provenance for exactly that reason and had to be
  discarded and rebuilt; an A/B on them would have been unreadable either way.

## Repro

```bash
cd apps/netty-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_DBG_ZGC_CORPSE=1 CRATONVM_DBG_ROOT_SOURCE=1 \
  <cratonvm.exe> --java-home "<jdk-25>" --Xmx 1500m -XX:+UseZGC @common.args \
  -Dcraton.batch=1 CratonRunner io.netty.util.ResourceLeakDetectorTest
```

`CRATONVM_ZGC_RELOCATE=0` is the control arm and is clean.
