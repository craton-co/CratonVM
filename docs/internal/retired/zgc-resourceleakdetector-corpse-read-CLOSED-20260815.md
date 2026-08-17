# `ResourceLeakDetectorTest` — ZGC reads a `DefaultResourceLeak` the slide moved away

**Status: CLOSED and RETIRED — 2026-08-15 (later the same day).** The defect
and all three residuals this page left behind are measured and settled; see
"Closing out the residuals" at the foot of the page for what each of them
turned out to be. One of them found a VM-wide defect in soft-reference
clearing, which is fixed here; another found a still-open crash that has its
own page. Everything above that section is the record as written when the fix
landed, unedited apart from this banner.

**Status when written: FIXED** (the ZGC-only crash) — 2026-08-15. This page
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
| compaction is not the trigger | **no**, it is | 10 interleaved reps each: `RELOCATE=0` **0/10** crash, compaction on **6/10** |
| the object died and this is a lifetime bug | **no** | `survivor_still_registered=true` |
| JIT frames are the only holder | **no** | `--nojit` reduces but does not remove |
| ZGC's heap-internal `ReferenceProcessor` holds it | **no** — it is EMPTY | `ZgcRealHeap::discover_reference` has no caller in the VM |
| the VM-level `ref_processor` is never remapped | **no** | `ref_proc.update_after_gc` runs at the end of `process_references_after_gc` |
| some collection path skips that remap | **no** | all six `collect_garbage_with_finalizers` sites call it; the four bare `collect_garbage` sites are tests |

## The crash is INTERMITTENT, and the corpse read is not fatal by itself

Thirty runs, one binary, interleaved, only the environment varied:

| arm | SIGSEGV |
|---|---|
| ZGC, compaction on | **6/10** |
| ZGC, `CRATONVM_ZGC_RELOCATE=0` | **0/10** |
| ZGC + `CRATONVM_DBG_ROOT_SOURCE=1` | 2/10 |

Two things follow that the earlier record got wrong.

**It is not 0/15.** This page's parent recorded the class as crashing on every
run. On current `dev` it crashes about 60% of the time; the other 40% complete
and report `found=3 ok=0 failed=3` (the separate, GC-independent
`NoSuchMethodError` on `DefaultResource.close`). Whether the two fixes landed
today moved that rate is **not established** — the pre-fix binaries had to be
discarded for provenance, so there is no clean before-arm to compare against.

**The attribution flag is a variable, not a neutral observer.** It builds a
`Vec` of every root each scan, and the crash rate falls from 6/10 to 2/10 with
it on. Anything measured with it on is measuring a different program.

**A corpse read does not imply a crash.** Runs that completed normally logged
up to five of them. So the stale write lands somewhere harmless most times and
somewhere fatal sometimes — consistent with "whatever now occupies the vacated
address" being sometimes an object with a field 0 and sometimes not.

**The reads are strikingly uniform.** Across every run that logged one, all 35:

```
class=io/netty/util/ResourceLeakDetector$DefaultResourceLeak
size=176   index=0   cycles_ago=0   survivor_still_registered=true
```

One class, one field, one size, always the cycle immediately after the slide,
survivor always alive. That is a single code path, not a scattering of stale
pointers — which is worth more than any single sample.

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

## What the root-source lever answered, and why it was not enough

`CRATONVM_DBG_ROOT_SOURCE=1` was the named next step on the parent page. It is
now wired through to the collector (`gc_quiescence::install_root_source_hook`),
so a corpse line carries `root_source=`. On the repro it says:

```
root_source="<none: not handed to the marker as a root>"
```

That rules out all 26 named root sources as the direct holder. It is weaker
than it looks: an object reachable through the Java object graph is
legitimately not a direct root, so `<none>` is the *expected* answer for one.
The lever answers "who rooted this object"; the question was "who kept a copy
of its old address". Related, not the same. Keep the instrument — it is cheap
and it did close off a whole inventory — but it was not what named the holder.

## What DID name the holder

Three cheap facts in sequence, each one flag:

1. **`op="set"`, 7 of 7.** `check_field_index` is shared by `get_field` and
   `set_field`; tagging it showed every stale access is a **write** to field 0.
   Somebody is clearing a referent through a pre-move address.
2. **`CRATONVM_DBG_JIT_NAMES=1`** named the faulting method:
   `io/netty/util/ResourceLeakDetectorTest$LeakAwareResource.close()Z`, and the
   dump reports `guarded compiled frames live process-wide: YES (quiescence
   depth=101)`.
3. `LeakAwareResource.close()` calls through to `DefaultResourceLeak`, whose
   `close()` calls `WeakReference.clear()` — which nulls **field 0**. The write
   the corpse ledger sees and the frame the crash dump names are the same
   operation.

**The holder is the JIT frame.** A compiled frame can keep an object pointer in
a register or a spill slot; the collector can neither find nor rewrite those.
`gen_heap` says so at its own divert — "cannot be relocated (raw register/spill
slots can't be rewritten)" — and diverts to a non-moving sweep;
`gc_quiescence::is_active()` is the flag both it and G1 read.

**ZGC read it zero times.** 9 call sites in `gen_heap`, 6 in `g1`, none in
`zgc.rs`. It consumed only `pinned_jit_roots_snapshot()`, which holds what a
conservative STACK scan recovered — a pointer that never left a register is not
in it, so its page is not withheld and the object slides.

Fixed 2026-08-15: `relocate_stw` now declines the cycle when a compiled frame
is live, with `relocation_skipped_jit` exported so the cost is a number. That
cost is real and unmeasured — on this collector compaction is also
defragmentation, so a permanently JIT-busy process defragments less. **Someone
should measure that on a JIT-heavy workload before calling it settled.**

## Verification

Same binary, same runner, one class per VM:

| arm | runs | outcome |
|---|---|---|
| ZGC, before the fix | 10 | **6 SIGSEGV**, corpse reads in most survivors |
| ZGC, after the fix | 10 | **0 SIGSEGV, 0 corpse reads**, all `found=3 started=3 ok=2 failed=1` |
| G1, after the fix | 2 | `found=3 started=3 ok=2 failed=1` |

ZGC and G1 now agree exactly, and the residual `failed=1` is GC-independent —
it reproduces identically on both collectors and belongs to whoever owns that
test, not to the collector.

**One correction to this morning's entry on the parent page.** It recorded G1
as `0 ok / 3 failed` with a `NoSuchMethodError` on
`DefaultResource.close(Ljava/lang/Object;)Z`. On the current binary G1 gives
`2 ok / 1 failed`, which is what the page originally recorded before that
update. The earlier reading came from a different build; treat that row as
binary-dependent and re-measure rather than quoting it.

**What is NOT verified:** the cost. The fix declines relocation while a
compiled frame is live, and this workload runs at quiescence depth ~100, so it
may now compact rarely or never. `relocation_skipped_jit` is printed beside
`compaction_cycles` in the shutdown summary precisely so that shows up, but
nobody has yet run a JIT-heavy workload long enough to say whether the
fragmentation cost matters. That is the open question this fix creates.

## Not closed: a second, JIT-independent crash

`--nojit` still crashes 2/10, in a completely different place — the collector
faulting inside its own rewrite pass. Written up separately in
`zgc-rewrite-pass-walks-off-a-reference-array-20260815.md`. Both defects vanish
under `CRATONVM_ZGC_RELOCATE=0`, so both belong to compaction.

| arm | SIGSEGV |
|---|---|
| ZGC, JIT on | 6/10 |
| ZGC, `--nojit` | 2/10 |
| ZGC, `RELOCATE=0` | 0/10 |

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

---

# Closing out the residuals (2026-08-15, later the same day)

This page left three things open. All three are now measured. Two of them
turned out to be about something other than what the page thought.

## 1. The cost of declining relocation while a compiled frame is live

The page's own words: *"nobody has yet run a JIT-heavy workload long enough to
say whether the fragmentation cost matters. That is the open question this fix
creates."*

**Run.** `docs/known-issues/repros/frag-churn/FragProbe.java`: 4e6 allocations
of 64 B..8 KiB through one hot compiled method, a 512-entry rolling live
window, a 512 MiB heap, then a count of the 4 MiB contiguous blocks the heap
can still hand out. The allocation loop lives in its own method so it tiers up
— a loop in `main()` measures the interpreter.

| arm | `compaction_cycles` | `relocation_skipped_jit` | worst largest-free ‰ | 4 MiB blocks |
|---|---|---|---|---|
| ZGC, JIT on | 4 | **64** | 138 | 72 |
| ZGC, `CRATONVM_ZGC_RELOCATE=0` | 0 | 0 | **239** | **124** |

**The decline fires on 64 of 68 cycles, and costs nothing.** The prediction that
a JIT-saturated run compacts rarely is confirmed exactly. The fear attached to
it is not: the arm that relocates *never* ends with a **larger** worst-case
largest free block and satisfies nearly twice as many large contiguous
requests. Compaction is not what buys contiguity on this collector, so
deferring it is not what loses it. Four interleaved JIT reps returned an
identical 72 blocks and ~2.4 s churn, so this is a reading and not a sample.

One workload is not a proof, which is why the numbers are now in the source
beside the guard (`relocate_stw`) rather than only here — the next person
argues with a measurement instead of re-deriving the fear.

## 2. "Whether the VM-level pass compensates is not established here"

It is now, and the answer is **yes for Weak and Phantom, no for Soft** — and the
soft half was broken on **every collector**, not just ZGC.

`weakref_null_referents_pre_gc` writes null into the referent slot of every
active Weak and Phantom reference before any collector runs, so a marker that
traces slot 0 as a strong edge — which every CratonVM marker does — reads a
null. That is what made ZGC's permanently-empty skip set harmless.

Soft references were not in that pass, and `ReferenceProcessor::process_soft_refs`
opens with `if is_marked(entry.referent) { continue; }`. A soft referent is
always marked *through its own `SoftReference`*, so that check always won and
the LRU policy underneath it was unreachable. **Soft references behaved exactly
like strong ones.** Measured against HotSpot with
`docs/known-issues/repros/reference-semantics/RefProbe.java` in a 64 MiB heap:

| arm | HotSpot | CratonVM before | CratonVM after |
|---|---|---|---|
| weak clears | yes | yes | yes |
| phantom enqueues | yes | yes | yes |
| soft retained while roomy | yes | yes | yes |
| soft cleared once tight | yes | **no** | **yes** |
| soft cleared rather than OOME | yes | **no — `OutOfMemoryError`** | **yes** |

Identical on ZGC, G1 and the default generational heap, before and after.

**The fix moves the policy decision in front of the mark**, which is the only
place it can go given a marker that traces referents strongly:
`ReferenceProcessor::condemn_idle_soft_refs` applies the LRU rule during the
pre-collection pass and only its condemned set gets nulled, so an entry the
policy wants to keep is still traced strongly and still retained. Survivors
(the referent was strongly reachable after all) are restored by the same
post-GC loop that restores weak and phantom ones.

Two things were needed beyond that, both found by the probe:

* **the clock.** `process_references_after_gc` passes `0` for "now", which the
  processor reads as "use the last value a mutator handed
  `touch_soft_reference`" — i.e. the moment of the most recent
  `SoftReference.get()`. The idle window of the reference that made that call
  is then zero, forever, and a program looping on its own soft-referenced cache
  is exactly the program that keeps re-stamping it. The pre-collection pass is
  ordinary VM code and passes a real `SystemTime` reading.
* **the last-ditch rule.** `java.lang.ref` guarantees every softly-reachable
  object is released before the VM throws `OutOfMemoryError`, and that is a
  different rule from the LRU policy, not a limiting case of it — see the
  clock argument above. `condemn_all_soft_refs`, armed by
  `last_ditch_reclaim` on the allocation-failure ladder, is that rule. It is
  the step between "G1's forced full mark cycle" and "throw".

ZGC's own `ref_processor` stays inert, and its field doc now says so outright,
with the measurement above as the reason that is not a hole. It is left in
place because several tests in `zgc.rs` are the only thing that exercises a
reference processor in isolation.

## 3. The residual `failed=1`

This page and its parent both recorded it as "GC-independent and belongs to
whoever owns that test". Half right.

It is GC-independent — 15 interleaved runs, ZGC with the JIT, ZGC `--nojit` and
G1, five reps each, every one `found=3 started=3 ok=2 failed=1`. But it does
not belong to the test's owner: the failing test is **`testConcurrentUsage`,
which HotSpot passes.** HotSpot fails the other two (`testLeakBrokenHint`,
`testLeakSetupHints`) and completes the whole class in 2.8 s; CratonVM passes
those two and blows `testConcurrentUsage`'s 60 s `@Timeout` at 61.3–65.3 s on
every arm and every collector. So the two VMs fail *disjoint* sets, and quoting
`ok=2 failed=1` as "better than HotSpot's `ok=1 failed=2`" would have been
exactly backwards.

That is a throughput gap on a 50-thread allocation workload, not a collector
defect, and it has its own page:
`docs/known-issues/netty/resourceleakdetector-concurrentusage-timeout-20260815.md`.

## What is still open, and is not this page

* `zgc-rewrite-pass-walks-off-a-reference-array-20260815.md` — the `--nojit`
  half. Still reproducing; see that page for the current numbers.
* ZGC and the generational heap throw `OutOfMemoryError` on an allocation-churn
  workload that G1 and HotSpot both survive — found while measuring residual 1,
  written up in the `zgc-nojit-allocation-churn-oome` page -- root-caused
  and FIXED on 2026-08-16 (the slide discarded the whole low free list,
  including the holes it had not written into), and retired with it.
