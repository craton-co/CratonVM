# GC round 2026-09-20 — marking, roots, references and lifecycle: proposed work

Scope of the review this came out of: `gc/src/{satb, concurrent_mark, reference,
gc_quiescence, gc_metrics, class_unloading, external_roots, shadow_stack,
pinned, collector, gc}.rs` and their tests. Each item below is sized, has a
named first step, and says what would make it fail.

Ordered by expected value, highest first.

---

## 1. Give `deactivate_and_drain` its precondition in the type system, and make the loom model real

> **DONE 2026-09-21.** All three parts landed — the token, a real loom model,
> and `.github/workflows/loom-nightly.yml` running it (first run green, 3
> tests, 10.69 s, no counterexample). The one part of the shape below that was
> NOT taken is the `#[cfg(loom)]` import-alias layer: driving the real
> `SatbQueue` is blocked by four compile errors recorded in
> `gc/tests/loom_satb.rs`'s header, so the replica is kept and the lane is what
> guards it. Record:
> `docs/internal/gc/mark-satb-drain-happens-before-20260920-RETIRED-20260921.md`.

**Why.** `docs/internal/gc/mark-satb-drain-happens-before-20260920-RETIRED-20260921.md`: the
final SATB drain has no happens-before edge against a mutator that checked the
gate before the `ACTIVE -> DRAINING` CAS. It is sound only because the remark
pause is stop-the-world — a property of the caller that nothing in the
signature expresses. Meanwhile the one instrument that is supposed to check the
FSM (`gc/tests/loom_satb.rs`) models a *different* state machine, is in no CI
lane, and would fail if run.

**Size.** Small + small + a nightly lane.

**Shape.**
- `pub fn deactivate_and_drain(&self, _stw: &StopTheWorldToken) -> Vec<usize>`.
  `StopTheWorldToken` already exists (`gc/src/collector.rs`) and `g1.rs` already
  threads one through `start_concurrent_mark`/`remark`, so the idiom is in
  place. `ConcurrentMarker::abort_cycle` has no token and should not pretend to
  produce a snapshot: split it to `deactivate_and_discard`, which flips the gate
  and drops what it drains, matching what the abort path already does with the
  return value (`let _ = ...`).
- `#[cfg(loom)]` import aliases in `gc/src/satb.rs` so the test can drive the
  REAL `SatbQueue` and the replica can be deleted. `gc/build.rs` already
  declares the `loom` cfg.

**First step.** Change the signature and fix the two call sites in
`gc/src/concurrent_mark.rs`; the compiler finds everything else. Do this before
the loom work — a model of a contract nobody has written down is what produced
the current state.

**What would make it fail.** A third caller outside a safepoint that cannot get
a token. `rg 'deactivate_and_drain'` finds two today, both in
`concurrent_mark.rs`, so this is a two-line change or it is a design problem
that has been hiding.

---

## 2. A termination protocol for the mark closure that is not "the queue looked empty"

**Why.** `ConcurrentMarker::concurrent_mark` and `drain_closure` both terminate
on `while let Some(p) = self.queue.pop()` returning `None`. For the generational
collector that is currently safe *only* because exactly one thread drives them
(`vm/src/runtime/interpreter/gc_and_alloc.rs::maybe_concurrent_gc`). The moment
a second worker is added — which is the obvious next optimisation, the queue is
already sharded and `MarkQueue` is already `Send + Sync` and documented as
"called by marker thread(s)" — the condition becomes wrong: worker A can pop the
last pointer and still be inside `scan_object` (which pushes its children) while
worker B sees every shard empty and returns. The closure is then incomplete, the
bitmap is not final, and `concurrent_sweep` frees a live object.

**Size.** Medium. The standard fix is small; proving it is the work.

**Shape.** An `AtomicUsize active_scanners` beside the queue: `pop` increments
before returning `Some`, the scanner decrements after `scan_object` returns, and
termination is `queue.is_empty() && active_scanners == 0` re-checked after a
short backoff. This is the shape G1's own `ConcurrentMarkController` will need
too, so build it once in `MarkQueue` rather than in each driver.

**First step.** Add the counter and the `terminated()` predicate to `MarkQueue`
with a unit test that fails today: two threads, one of which sleeps inside a
scan callback, asserting the second does not observe termination. Do this
**before** anyone parallelises the drain — the test is the deliverable, the
parallel driver is the follow-up.

**What would make it not worth doing.** If the generational concurrent
old-gen cycle is going to be retired in favour of G1/ZGC marking, this is dead
work. Decide that first; it is a ten-minute question and a week of difference.

---

## 3. Make `MARK_QUEUE_SHARD_CAP` overflow observable, and price the fallback

**Why.** A dropped push sets `overflowed`, and the compensation is a full
`old_gen.walk_objects()` rescan — up to eight of them, then a
mark-everything-and-give-up pass (`drain_closure`, `rescan_passes > 8`). That is
an O(heap) to O(8 × heap) STW cost that **no counter reports**: nothing in
`gc_metrics` records that it happened, so a workload whose remark went from 4 ms
to 4 s has no line to point at. G1 has exactly this counter for its own
worklist (`g1_degraded::MARK_WORKLIST_OVERFLOW`); the generational marker has
none.

**Size.** Small.

**Shape.** Two counters in `gc_metrics` — `mark_queue_overflows` and
`mark_rescan_passes` — bumped from `drain_closure`, printed unconditionally in
`collector_decision_report` so a zero is citable. Then the interesting question
becomes answerable: is `1 << 20` per shard ever hit on a real corpus, or is the
whole fallback path unreachable code with a test?

**First step.** Add the two counters and run the H2 and Spring lanes with
`--verbose:gc`. If both read zero everywhere, the follow-up is to *lower* the
cap deliberately in a probe and confirm the fallback is even correct — it has
never executed.

---

## 4. Segregate the reference lists so per-collection cost tracks live references

**Why.** The module header states the cost honestly: "Per-collection cost is
**O(registered references)**, not O(live references) … A `WeakHashMap` with a
million live entries still costs a million-entry scan per collection." Phases
2-4 are flat `for entry in &mut self.weak_refs` scans that visit every already-
cleared and already-enqueued entry to skip it. `mark_manually_enqueued` is worse:
a linear scan across four lists per `Reference.enqueue()` call, and pgjdbc /
H2's `CloseWatcher` call it per connection.

**Size.** Medium, and it is a pure data-structure change — no semantics move.

**Shape.** Split each list into `active` and `retired`. An entry moves to
`retired` the moment it is flagged `cleared`/`enqueued` *and* its once-only
emissions are done (`clear_emitted`, `action_emitted`). The phases scan `active`
only; `remove_collected`/`update_after_gc` walk both. Add an
`addr -> (list, index)` index for `mark_manually_enqueued`, mirroring the
`soft_ref_addr_index` that already exists for `touch_soft_reference` and was
added for exactly this reason.

**First step.** Instrument before restructuring: add
`ReferenceProcessingStats::entries_visited` and `entries_skipped_retired`, and
read them on the Hibernate and H2 lanes. The sketch in
`refs-metaspace-unloading.md` has been open long enough that the number matters
more than the design.

**What would make it fail.** The `soft_refs` position indices are load-bearing
(`soft_ref_lru_index` keys `(timestamp, idx)`), so soft is the hard list. Do
weak/phantom/cleaner first; they have no positional index at all.

---

## 5. A paired scan/remap audit that is a test, not a review

**Why.** Every root family needs a scan and a write-back applying the
collection's pointer map, and the pairing is currently maintained by reading two
files side by side (`vm/src/memory/roots.rs::collect_roots` against
`vm/src/memory/gc.rs::update_all_roots`). `external_roots.rs` is the one place
where the pairing is structural — a provider registers `scan` and `remap`
together in one value, and `register_external_root_provider` panics on a partial
re-registration. That design should be the rule, not the exception.

**Size.** Small for the test, large for the migration. Do the test.

**Shape.** A test fixture that (a) registers a synthetic root provider holding
one address, (b) runs a moving collection, (c) asserts the provider's address
was rewritten. Then one such case per family that can be expressed from inside
`gc/`: shadow stack, JNI pin set (`pinned.rs` — already has
`a_token_releases_the_pin_after_the_object_moved`), watched referents, the
conservative-JIT pin registry.

**First step.** Write down the families and their current status — the audit
table at the end of this round's report is that list. Anything marked "scanned,
not remapped" is a bug; anything marked "remapped, not scanned" is dead code.

---

## 6. Bound `ORPHANED_SATB_BUFFERS` and the token map, and say what bounds them

**Why.** Two process-global tables grow on paths nothing prunes:

- `satb.rs::ORPHANED_SATB_BUFFERS` — a dead thread's buffer is reaped only when
  a collector drains *its* queue. A bucket belonging to a queue that has since
  been dropped is never taken, so the orphan is retained for the life of the
  process. One live queue in a production VM makes this a non-issue today and a
  leak in any multi-heap embedding, which is the case the queue-id scoping was
  added for in the first place.
- `pinned.rs::tokens()` — a `PinToken` entry is removed only by `unpin_token`. A
  caller that takes a token and releases by address (`unpin(addr)`) leaks the
  token row, and `update_after_gc` then walks it on every moving collection.

**Size.** Small.

**Shape.** For the orphans: reap a buffer whose every bucket names a queue id no
longer allocated — which needs a live-queue id set, so the cheaper version is to
reap on a generation counter (an orphan not drained in N cycles is dropped, with
a `warn!` naming the count). For the tokens: make `unpin` take the token map's
reverse index into account, or make `pin_tokened` the only tokened entry point
and assert the two counts agree in `update_after_gc`.

**First step.** A `debug_assert` in `update_after_gc` that `tokens().len() <=
table().len()`, and the same for the orphan list against a high-water mark. If
it never fires on the corpus, downgrade both to a counter and move on — an
unbounded table nobody grows is a documentation problem, not a leak.

---

## 7. Decide `class_unloading.rs` and `metaspace.rs`

**Why.** 3,043 lines with no caller, correctly labelled as such in three places
(`gc/src/lib.rs`, each module header, and a twenty-line comment in `g1.rs` that
exists solely to answer the grep). `lib.rs` says the question "gets asked once
per collector" and asks the reader not to re-derive it a fourth time. This
review is the fourth time.

**Size.** Trivial either way. The cost is the decision, not the edit.

**Shape.** Either delete both and keep the answer in
`docs/architecture/class-loader-unloading.md` (which already documents the real
transaction), or wire `ClassUnloader` behind the driver in
`vm/src/memory/gc.rs::unload_dead_class_metadata` so the bookkeeping has one
home. The module header warns the tables "previously had no bound at all and
would have violated the 'unload invalidation or hard bound' rule the moment they
saw traffic" — which is the argument for deleting it rather than keeping a
tested-but-unexercised second implementation.

**First step.** Ask whoever owns the metaspace approximation in
`vm/src/vm/vm_init.rs` whether `-XX:MaxMetaspaceSize` is ever going to bound
anything. If no, delete both files in one commit.
