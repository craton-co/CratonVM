# Weak and soft references to finalizer-reachable objects are kept, where HotSpot clears them

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`, wave d7, Linux release): Generational half FIXED and measured; the page stays open for G1 and ZGC only.** `GenR4WeakToFinalizableProbe -Xmx256m -XX:+UseGenerationalGC` prints HotSpot's `weak-to-finalizable-cleared: ok`, `weak-to-finalizer-reachable-child-cleared: ok`, `PASS` 3/3 in the default mode (`cd_weakfin_1..3`) and 3/3 in `--compatible` (`cd_weakfin_compat_1..3`); the battery's `weak_to_final` is SAME too. What is left is G1's Phase 3.5 and ZGC's `mark_is_set` recording of the resurrection closure (`VmHeap::take_resurrection_closure`'s G1 and ZGC arms, step 2 of `docs/internal/gc-common-round-20260923/handoff-w7d-collectors-report-the-resurrection-closure.md`), out of this round's scope. Retire when the same probe prints those lines on `-XX:+UseG1GC` and `-XX:+UseZGC` in both modes. The two documented Generational depth-1 residues belong to `gcd-d5r-proposal-true-root-seed-beyond-requested-non-promoting-majors-20260928.md`.

> **STATUS (2026-09-28, gcd d5/r): Generational = one probe run from
> retirement; what is left there is two documented depth-1 residues, not a
> code gap; G1 / ZGC halves out of this round's scope (unchanged).**
>
> * **Generational, re-read on d916d1c40.** `VmHeap::take_resurrection_closure`'s
>   Generational arm returns `GenerationalHeap::take_resurrection_closure`,
>   fed by all three resurrection rounds (the non-moving young sweep's drain,
>   the moving Phase 2.5b, `OldGenFinalizerPass`). d4/n's true-root seed
>   (requested majors, default on) does not change what is recorded: the old
>   pass still records every object its resurrection round pops, and the
>   young sweep that runs first records the young closure, as before. d5/r's
>   watched-set seed (`gcd-d4j-...-young-nepotism` page) keeps a watched
>   young object's old referents strongly, exactly as the legacy seed did,
>   so it changes no verdict of this page either.
> * **What remains on Generational, by design (depth-1 behaviour, as the
>   w36-d block below says):** a YOUNG object reached only from a dead OLD
>   finalizable (the young sweep treats every old object as live, so it is
>   not in any resurrection round), and objects a pinned in-place young
>   cycle keeps. Both need the young phase to know which old objects are
>   dead, which only a whole-heap mark has; tracked by
>   `gcd-d5r-proposal-true-root-seed-beyond-requested-non-promoting-majors-20260928.md`,
>   not by this page.
> * **The run that retires the Generational half** (both modes, 3/3):
>   ```
>   java -XX:+UseSerialGC -cp tools/bench GenR4WeakToFinalizableProbe
>   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4WeakToFinalizableProbe
>   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m --compatible -cp tools/bench GenR4WeakToFinalizableProbe
>   ```
>   Expected stdout, all three: `weak-to-finalizable-cleared: ok`,
>   `weak-to-finalizer-reachable-child-cleared: ok`, `PASS` (exit 0). A
>   `FAIL 1` on the child line is the depth-2 recording missing on the
>   `System.gc()` path (the non-moving sweep's drain); a `FAIL 2` is a
>   depth-1 regression.
> * **Then:** move the Generational half out (record it on the G1 / ZGC
>   owners' pages or split this page); the page itself retires only with
>   G1's Phase 3.5 and ZGC's `mark_is_set` recording (step 2 of
>   `docs/internal/gc-common-round-20260923/handoff-w7d-collectors-report-the-resurrection-closure.md`),
>   out of scope this round.
>
> **STATUS (2026-09-26, gc-common w36-d, superseded above): depth 1 FIXED. Depth 2 is FIXED on
> Generational (awaiting the probe run) and OPEN on G1 and ZGC,** owned by
> those two collectors. No VM edit is needed.
>
> **Depth 2 on Generational (w36-d).** `VmHeap::take_resurrection_closure`'s
> Generational arm now returns `GenerationalHeap::take_resurrection_closure`
> (`gc/src/gen_heap.rs` ~10075). That is a per-heap set, cleared at the entry
> of every collection and filled by all three Generational resurrection
> rounds, at PRE-collection addresses:
>
> * the non-moving young sweep's drain, which is the `System.gc()` path the
>   probe takes: every object its `drain_parallel` scans (~17195);
> * the moving Phase 2.5b: the `pointer_map` keys whose destination was
>   copied or promoted after the resurrection seeds (~14521);
> * the old generation's `OldGenFinalizerPass`, whose round was added by the
>   same wave: every object popped in the resurrection round.
>
> Each round starts with the strong closure's worklist drained, and every
> push site marks before it pushes. So the set holds only objects the strong
> closure had NOT marked, which is the soundness condition. The set is a
> SUBSET of the depth-2 closure: a young object reached only from an OLD
> dead finalizable, and objects a pinned in-place young cycle keeps, are not
> recorded, and those keep the depth-1 behaviour. Tests:
> `w36d_major_gc_resurrects_a_dead_old_finalizable_and_its_referent` and
> `w36d_collect_garbage_with_finalizers_reports_a_dead_promoted_finalizable`
> (the closure names the finalizable and its referent, and never a strongly
> reachable object).
> **Orchestrator:** `tools/bench/GenR4WeakToFinalizableProbe.java` must print
> `PASS` on Generational in both modes. The concurrent cycle's retained
> finalizables (see the old-gen finalizer page) are not recorded; that is
> depth 1 behaviour for them.
>
> **Still open (G1, ZGC; the exact edits are step 2 of
> `docs/internal/gc-common-round-20260923/handoff-w7d-collectors-report-the-resurrection-closure.md`).**
>
> * G1 records the from-space address of every object its Phase 3.5 drain
>   (`G1Collector::resurrect_dead_finalizers` and the parallel twins)
>   evacuates.
> * ZGC records every object its `mark_is_set` resurrection pass marks, at the
>   pre-relocation address.
> * Each then replaces its `Vec::new()` arm in `VmHeap::take_resurrection_closure`
>   (`gc/src/vm_heap.rs` ~2935).
>
> Earlier record (w18-a and before) follows.
>
> **Depth 1 (a weak/soft ref to the finalizable object itself): fixed.**
> `ReferenceProcessor::note_resurrected_finalizables` (`gc/src/reference.rs`,
> line 1851; w6-d, `90cd743d6`) treats the objects a collection marked only
> to run their `finalize()` as unmarked for soft and weak processing in the
> next round; final and phantom processing still see them marked. It is
> wired in `run_collection_pause` through
> `note_resurrected_finalizables_for_reference_processing`
> (`vm/src/runtime/interpreter/gc_and_alloc.rs`, line 2048; applied in
> `8c1f5b96c`). The VM computes the pre-collection set from `fin_roots`, the
> pointer map and `dead_finalizers`. Test:
> `reference::tests::w6d_a_weak_ref_to_a_resurrected_finalizable_is_cleared`
> (W6D-4).
>
> **Depth 2 (a weak ref to an object reachable only through a finalizable):
> open.** The VM cannot compute this set: a closure traced from the
> resurrected objects also reaches strongly reachable objects. The collector
> must report what its resurrection drain marked that its strong closure had
> not. The common half is landed (w18-a, `f56b0b9f7`): the dispatcher
> `VmHeap::take_resurrection_closure()` (`gc/src/vm_heap.rs`, line 2931) and
> its consumer in `note_resurrected_finalizables_for_reference_processing`,
> tested with an injected closure (`w18a_resurrection_closure_tests`, line
> 2108) and by `reference::tests::w7d_the_resurrection_closure_clears_weak_refs_at_depth_two`.
> Until w36-d every backend's arm returned `Vec::new()`. What remained, per
> `docs/internal/gc-common-round-20260923/handoff-w7d-collectors-report-the-resurrection-closure.md`,
> was to record the set where each drain marks and return it from that
> backend's arm: G1 Phase 3.5; Generational Phase 2.5b plus the old-gen pass
> (see `../../internal/gc-common-round-20260923/common-d-generational-old-gen-finalizables-are-never-finalized-RETIRED-20260928.md`;
> both were done by w36-d); ZGC's `mark_is_set` pass.
>
> **Measured** with `tools/bench/GenR4WeakToFinalizableProbe.java`
> (`orchestrator-w6-verification.md`, `-w7-verification.md`): HotSpot and G1
> `PASS`; Generational and ZGC `FAIL 1` (`weak-to-finalizable-cleared` ok,
> `weak-to-finalizer-reachable-child-cleared` fails). G1 passes because its
> `System.gc()` concurrent cycle's remark processes references before its
> resurrection (observed with `CRATONVM_DBG_WATCHREF=1`; the ordering is
> inferred from the code). No later verification doc re-runs the probe.
>
> The `finalizer_live` terms in `soft_is_live` / `weak_is_live`
> (`reference.rs` lines 855 and 985) and the V18 tests pinning them are still
> in place; dropping them alone changes nothing on the post-GC paths, because
> each collector has already marked the resurrected closure when
> `process_references_after_gc` asks. The depth-1 fix changes `--compatible`
> output only for a program that holds a weak or soft reference to an object
> with a `finalize()` override, and the new answer is HotSpot's.

*Status: **OPEN** (depth 2). Filed 2026-09-23, gc-common round wave 1, lane D.*

## Evidence

`gc/src/reference.rs::process_references_with_finalizer_trace` folds the
finalizer-reachable closure (`finalizer_live`) into the liveness predicate of
phase 1 (soft) and phase 2 (weak):

```rust
let soft_is_live = |addr| strongly_marked(addr) || finalizer_live.contains(&addr);
...
let weak_is_live = |addr| strongly_marked(addr) || finalizer_live.contains(&addr) || soft_live.contains(&addr);
```

The "SECURITY FIX (V18)" comment justifies it as "HotSpot avoids this by
treating the finalizer-reachable set as live during weak/soft processing". That
is the wrong way round. HotSpot processes Soft, Weak, **then** Final, then
Phantom, and an object reachable only through a `FinalReference` is not
strongly reachable when weak references are processed: its weak references are
cleared *before* it is finalized. That asymmetry is the documented reason
`PhantomReference` exists (the `java.lang.ref` package doc: phantom references
are enqueued only after finalization; weak references are cleared "at that
time", before). Pinned by `v18_weak_ref_to_finalizable_object_not_cleared`
and `v18_transitive_closure_protects_weak_ref_with_tracer`.

A second, collector-side source (wave 2): every post-GC path runs
`collect_garbage_with_finalizers(.., finalizable_roots)`, and each backend
MARKS a dead finalizable and its whole subgraph INSIDE the collection. By the
time `process_references_after_gc` runs, `is_marked(referent)` is already
`true` for a finalizer-reachable referent. That is why the fix is the
collectors' resurrection-closure report above, not a `reference.rs` edit.

## Failure scenario (HotSpot diff)

```java
class F { protected void finalize() { resurrected = this; } }
WeakReference<F> w = new WeakReference<>(new F());
System.gc(); System.runFinalization(); System.gc();
System.out.println(w.get() == null);
```

HotSpot: `true` (cleared before `finalize()` ran, and resurrection does not
un-clear it). This depth-1 shape is fixed. The open shape is the same with the
weak reference pointing at an object held only by `F`'s fields: CratonVM keeps
it on Generational and ZGC. Same shape for a `WeakHashMap` whose key is
reachable only from a finalizable. The phantom half (`phantom_is_live`
includes `finalizer_live`) is correct and must stay.

## Confirmation

```
rg -n 'VmHeap::.*=> Vec::new\(\)' gc/src/vm_heap.rs     # take_resurrection_closure arms
cratonvm -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4WeakToFinalizableProbe   # and G1, ZGC
```

## Retire when

`GenR4WeakToFinalizableProbe` prints `PASS` on all three collectors in both
modes.

## See also

The generational round filed the same defect independently and retired it as
a duplicate of this page:
`docs/internal/gc/gengc-r4-mark-weak-refs-honour-finalizer-reachability-RETIRED-20260923.md`.
