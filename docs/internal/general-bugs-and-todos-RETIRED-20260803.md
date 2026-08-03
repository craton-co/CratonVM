# "General Bugs & TODOs" — retired

**Status: ✅ RETIRED 2026-08-03.** All five items are closed. Three were
already fixed in the tree and are re-verified here with the evidence the item
asked for; two carried live residuals, which are fixed on
`fix/general-bugs-todo-20260803`.

The original list is preserved verbatim under "The original list" below,
because three of its five items describe the code as it was *before* a fix
that had already landed — and one of those descriptions points at the wrong
file. Keeping the text makes the drift readable instead of confusing.

## Verdicts

| # | item | verdict |
|---|---|---|
| 1 | GC: incorrect "moving young" logic | **already fixed** (arch-2026-07-26); the flag and its term are deleted, a regression test pins the inverse, benchmarks re-run below |
| 2 | Perf: layout registry lookup hotspot | **two live defects found and fixed** — one a silent correctness bug, one the quadratic scan the item describes |
| 3 | Concurrency: unsafe `ObjectRef` sharing | **already audited**; the one gap the audit itself flagged as unenforced is now enforced |
| 4 | Logic: monitor notify bug | **the described mechanism was already correct; a real residual found and fixed** — `Object.wait()` had no prompt interrupt wake |
| 5 | Interop: incorrect synthetic-JDK handling | **already fixed**; usage text and two tests added |

---

## Item 1 — GC "moving young" logic

**Claim.** `Heap::collect_young_if_threshold()` in `gc/src/heap.rs` guards
moving GC with `self.is_active() && !self.allow_moving_young`, made
unconditional by mistake; removing the `!allow_moving_young` clause restores
the semispace copy collector.

**Verdict: already fixed, and the diagnosis was right.** The term existed and
was exactly as harmful as described. It was deleted — along with the
`CRATONVM_ALLOW_MOVING_YOUNG` flag that fed it — by the arch-2026-07-26
`moving-young-precise-roots` work.

Two corrections to the report, both worth recording because they cost time to
re-derive:

* The code is in `gc/src/gen_heap.rs::collect_garbage_inner`. Neither
  `Heap::collect_young_if_threshold` nor `fail_closed_non_moving` exists in
  `gc/src/heap.rs` — nor anywhere else; the only names a text search finds
  today are in a commented-out copy of the exact expression (`let
  fail_closed_non_moving = is_active() && !gc_flags().allow_moving_young;`)
  that `gen_heap.rs` keeps as the historical record. The item's "TODO in
  gc/src/heap.rs" is not there either; that file's TODOs are all NUMA
  multi-arena work.
* The fix was **not** "remove or alter the `!allow_moving_young` clause" so
  that the flag permits moving. Both the term *and the flag* were deleted. The
  reasoning is in the source and is the right one: a flag whose only job is to
  permit correct behaviour is not a safety mechanism, and the old shape meant
  `CRATONVM_MOVING_YOUNG=1` alone could never run a moving cycle under a live
  JIT frame — which is the only case the feature exists for.

Two diversions survive, and both are real: conservative (unrewritable) JIT
roots when moving-young is not in effect, and the promotion-OOM fallback when
both generations are ~90% full.

**Regression guard.** `gen_heap.rs`'s
`moving_young_copies_with_live_jit_frame_and_proven_coverage` asserts the
inverse directly: with a live JIT frame, healthy generations and a proven
coverage map, `objects_copied` must be non-zero and the root must be rewritten.
Its own doc comment records that the test was *unreachable at any single env
setting* under the old code. Nothing further was needed here.

**Benchmarks.** See "Measurements" below.

**Not this item's defect, still open.** `JIT_PUBLISHES_RELOCATION_CONTRACT`
is `false`, so a runtime veto still forces the non-moving sweep once compiled
code exists. That is a deliberate, separately documented project state with
its own flip checklist (`types/src/flags.rs`, and
`docs/internal/jit-optimizing-tier-moving-young-gate-RETIRED-20260731.md`),
not a residual of the guard bug.

---

## Item 2 — layout registry lookup hotspot

**Claim.** Object layouts are resolved by a linear search of the class
hierarchy on every lookup; cache `ClassId -> layout` in a map.

**Verdict: the specific cache already exists — and looking for the *rest* of
the described shape found two live defects.**

The `ClassId -> layout` lookup itself is already O(1) and heavily optimised:
`types/src/field_layout.rs` keeps a dense `Vec` indexed by `ClassId`, a
version registry behind `parking_lot::RwLock<FxHashMap<_>>`, and two
generation-validated thread-local working sets. Its own comments record the
`perf record` that drove that work (`BinTreesClassic d=18`: 37.8% of samples
in `class_layout_for_fields`, 4.8% more in SipHash underneath it). Nothing to
do there.

What was *not* fixed is the place that genuinely still scanned the hierarchy.

### 2a. `recompute_subclass_layouts` skipped subclasses after any class unload

`ClassManager::recompute_subclass_layouts(changed_id)` propagates a layout
change to every transitive subclass. It answered "which classes are
descendants?" with:

```rust
let class_count = self.class_store.len();
for idx in 0..class_count { /* ... is_subclass_of(changed_id) ... */ }
```

`ClassStore::len()` is the **live** class count. The ClassId upper bound is
`slot_count()`, and its own doc says so — ids are never reused, so an unloaded
class leaves a tombstone and `len() < slot_count()` forever after.

One unloaded class was therefore enough to make the loop stop a slot short and
silently skip the highest-id subclasses. Those are the most recently loaded
ones — i.e. exactly the application classes that extend the JDK stub being
upgraded. A skipped descendant keeps a `first_field_index` computed against
the pre-upgrade parent, so its own fields overlap the parent's and
`getfield`/`putfield` resolve past the object's slot count: the
"out-of-bounds field read ... undersized object layout" this function exists
to prevent.

Proven, not inferred —
`a_subclass_above_the_live_class_count_still_gets_its_layout_recomputed` was
run against a copy of the tree with the old loop restored:

```
test class_manager::tests::a_subclass_above_the_live_class_count_still_gets_its_layout_recomputed ... FAILED
assertion `left == right` failed: Child's own field must start after Parent's two fields
  left: 1
 right: 2
```

Two more sites made the same `len()`-as-id-bound mistake and are fixed the
same way: `validate_native_coverage` (`vm/src/vm/vm_object.rs`, under-reported
the native census after an unload) and the vtable catch-up pass in
`vm_init.rs`.

### 2b. …and it was quadratic

The same loop probed **every** class with `is_subclass_of`, which walks a
superclass chain: `O(classes x depth)` per call, once per synthetic-stub
upgrade whose layout shifted. On a framework-scale run that is thousands of
upgrades against tens of thousands of classes.

`ClassStore` now carries a direct-subclass adjacency index, maintained by
`add`, the new `set_superclass`, and `remove`. `descendants_of` walks it
breadth-first and yields exactly the transitive subclasses, parents before
children — the topological order the recompute needs, and the one the old
ascending-id scan provided. Cost drops to `O(descendants)`.

`upgrade_synthetic_class` re-parents through `set_superclass` rather than
writing `class.superclass` through `get_mut`: a stub minted under one
superclass whose real bytecode names another has to *move* its edge, and a raw
field write would leave the propagation blind to it.
`re_parenting_a_class_moves_its_edge_in_the_subclass_index` pins that, and
`the_subclass_index_agrees_with_a_full_hierarchy_scan` pins the index against
the brute-force answer it replaced (including topological order, and with a
tombstone in the store).

Three bootstrap sites in `vm_init` were doing exactly that raw write
(`Enumeration$Impl`, `Comparator$Native`, the unmodifiable-view wrappers) and
are routed through the new `ClassManager::set_superclass`. Inert in practice —
the new parent is `java/lang/Object`, whose layout never grows — but they were
written that way for no reason other than that it is the obvious way to write
the line, which is why the fix is paired with a gate:
`superclass_is_only_ever_written_through_set_superclass` walks the workspace
and fails on any raw write outside `class.rs`. There is no failure at the point
of the mistake, so a comment would not have held.

### 2c. `unregister_class_layout` swept the whole version map

Dropping one class's entries used `retain` over the entire
`(class_id, field_count) -> layout` map — `O(all registered versions)` per
unloaded class, so tearing down a class loader was quadratic in the number of
loaded classes. That is the same "linear scan where an index belongs" shape
the registry above was rewritten to remove. A reverse index
(`class_id -> its field counts`) makes it `O(that class's versions)`.

---

## Item 3 — unsafe `ObjectRef` sharing

**Claim.** `ObjectRef` is `unsafe impl Send/Sync` on an outdated
single-threaded-mutator assumption. Audit cross-thread uses; if safe sharing
is not guaranteed, remove the impls; at minimum document why they are sound.

**Verdict: already done, and done more thoroughly than the item asks.** The
audit is `docs/threading/objectref-concurrency-contract.md` (401 lines: the
real threading model, what mutates the pointee and under what lock, whether
the pointee can move under a live copy on another thread, a per-operation
contract table, and the soundness derivation). The `unsafe impl`s stay, and
the SAFETY note above them was rewritten to be *narrower* than the argument it
replaced — it asserts only that the value (a single `NonNull<u8>`) is `Copy`
with no `Drop`, no interior mutability and no ownership semantics, and
explicitly stops asserting anything about the pointee.

The item's premise that the impls rested on a single-threaded mutator was
correct about the *old* note, which is exactly why it was replaced: that
coupling tied two auto-trait impls to the whole GC design, so they read as
unsound the moment the scheduler changed. They were not. The contract doc's §8
also records two claims in the old note that were simply false (plain field
access takes no lock; the JIT's `jit_putfield_*` helpers do write slots
outside the GC).

**What was left.** §7 of that doc lists nine invariants resting on convention
rather than types. One of them is a gap this item's "audit all uses" reading
covers directly, §7.3: `Hash`/`Eq` are address-based, so every
`HashMap<ObjectRef, _>` needs a GC disposition — and *"Nothing enumerates the
tables that need it."*

That is now enumerated and enforced. `vm/src/memory/addr_keyed.rs` carries a
census of every address-keyed table in the workspace with the disposition its
own source states, and a test walks the tree and fails when a declaration
appears that is not listed, or when an audited file's declaration count
changes.

The audit behind the list — all nine are accounted for, and none was found
broken:

| table | disposition |
|---|---|
| `gpu_pinned_refs` | pinned; membership is what forbids relocation |
| `class_mirrors_reverse` | rebuilt inside the collection by `vm/src/memory/gc.rs` |
| `offload::input_cache` | remapped + swept via `addr_keyed::remap_and_sweep` |
| `inet_addr_side_table`, `ds_side_table` | scanned + remapped (`gc_scan_inet_addr_roots` / `gc_update_inet_addr_refs`, and the re10 pair) |
| `synthetic_locale_data`, `locale_data` | scanned + remapped (`gc_scan_locale_roots`) |
| `class_data_store` | tolerated under a stated condition: effectively write-only; the source names the re-key-by-identity-hash migration required before a reader is added |
| `EQE_PENDING` | tolerated under a stated condition: values are identity-hash-rooted, and a stale key only splits one EQE's tasks across two buckets that are both drained unconditionally |

No live defect. The gap was that nothing would have noticed the tenth.

The census walks **directories**, not a list of file names: a gate keyed on
file names goes quietly fail-open exactly when the code it guards is
reorganised. It also fails loudly when it cannot find the workspace root, when
the walk turns up implausibly few files, or when the pattern matches nothing,
so it cannot pass vacuously.

---

## Item 4 — monitor notify

**Claim.** `wait()` may hang if a thread is interrupted right before `notify`;
`notify()` should use `Condvar::notify_one()` under the same lock `wait()`
holds, `wait()` should re-check its condition in a loop, and a test should
cover "A waits with a timeout, B notifies".

**Verdict: every mechanism the item names was already in place — and looking
for the *symptom* found a real residual.**

Already correct in `vm/src/threading/monitor.rs`:

* `notify` / `notify_all` take the monitor state lock and signal
  `wait_condvar` while holding it, which is the pairing the item asks for.
* `wait` loops on a predicate and returns on a signal, so a caller re-checks.
* The lost-wakeup case the item's title points at is handled explicitly: a
  thread that is both notified and interrupted in the same slice forwards the
  notification to another waiter before breaking out, so the single
  notification is not consumed by the `InterruptedException` throw.
* JLS §17.2.1 entry-time check and clear-on-throw are both in
  `vm_exec.rs::monitor_wait`.

**The residual.** `Thread.interrupt()` only sets a flag. `Monitor::wait`
therefore observes an interrupt no sooner than its next 5 ms poll slice, and
an *untimed* wait had nothing but that poll to end it. `Thread.interrupt0`
already unparks a target blocked in `LockSupport.park` for exactly this
reason, and its comment says so in as many words: *"Without this the target
only notices at the next 5 ms interrupt poll; an explicit unpark makes the
wakeup prompt and matches the JVM contract."* `Object.wait()` was the one
blocking primitive left without that.

Fixed by using the monitor the registry already records for JMX
(`set_jmx_waiting_monitor`, written just before the park and taken just after)
to identify what to wake. No new side table. The wake:

* is a `notify_all`, because the interrupted thread is not identifiable from
  the interrupter, and a `notify_one` reaching the wrong waiter would leave
  the interrupt unserviced for another slice *and* consume a slot;
* consumes no pending notification — condvars bank no permits — so it cannot
  swallow a `notify()`. `an_interrupt_wake_does_not_swallow_a_later_notify`
  pins this;
* never inflates: `wait()` inflates, so an object with no monitor has no
  waiters, and inflating on an interrupt would put a heavyweight monitor on
  every object an interrupted thread happened to hold.
  `an_interrupt_wake_never_inflates_an_untouched_object` pins this.

**The test the item asks for** is
`a_timed_waiter_returns_as_soon_as_it_is_notified` — A waits with a 5 s
timeout, B notifies after 30 ms, and the assertion is on elapsed time, not on
returning. The failure it guards is not a hang but a silent one: the timed
branch once ignored the condvar's verdict and always slept out the full
timeout, so `Thread.join(millis)` and `awaitTermination` "worked" while
burning the whole duration after the event they waited for.
`an_untimed_waiter_is_released_by_an_interrupt_wake` covers the untimed half:
it asserts the wake is actually delivered (the monitor is inflated, so
`wake_waiters_for_interrupt` must return `true`) and that the waiter leaves
reporting the interrupt. It deliberately does **not** assert a latency bound —
the 5 ms poll would satisfy one on its own, so a timing assertion here would
pass with the fix reverted and be worse than none.

**What was deliberately not changed.** The 5 ms poll stays. `thread_interrupt`
is today the only production writer of the interrupt flag (every other
`set_interrupted` call in the tree is a test), so raising the interval would
be safe *today* — but the poll is the safety net for any future path that sets
the flag without going through the wake, and shortening the gap between "flag
set" and "waiter notices" was never the poll's cost. The wake is the fix; the
poll is the backstop.

---

## Item 5 — synthetic-JDK handling

**Claim.** `--synthetic-jdk` on a binary built without the `synthetic-jdk`
feature silently fails to find classes; the launcher's check "may be
incomplete". Ensure a clear error, update the usage docs, and ensure
`--real-jdk` correctly overrides synthetic mode.

**Verdict: already fixed on both halves; the usage-doc half was the only thing
missing.**

`resolve_jdk_mode` in `vm-cli/src/main.rs` is the single authority. It rejects
the flag pair explicitly (not just via clap's `conflicts_with`, so an argv
preprocessing change cannot quietly make one win), then validates the selected
mode: `require_synthetic_jdk()` for synthetic, `require_real_jdk()` for real.
Neither falls back to the other library — a run whose standard library was
chosen by the host is neither reproducible nor reportable. The error text
names the feature, what the state would be if the launch proceeded, and both
fixes.

`--real-jdk` overriding is structural rather than a preference: there are two
fixed constants, `LAUNCHER_DEFAULT_JDK_MODE = Real` and
`EMBEDDED_DEFAULT_JDK_MODE = Synthetic`, neither derived from a Cargo feature
or from host probing, and the only way to change either is an explicit flag or
`VmConfig::with_jdk_mode`. The second launcher entry point (`libcratonvm`)
calls `require_synthetic_jdk` too, and `-Xinternalversion` reports
`jdk.mode.synthetic_compiled_in` so the question is answerable before a run.

Added here: `--help` now states the build requirement and points at that
`-Xinternalversion` key (the rejection message alone is not documentation — it
only appears after the run has already failed), plus
`the_usage_text_states_the_synthetic_jdk_build_requirement` and
`real_jdk_flag_selects_real_mode_regardless_of_the_synthetic_feature`.

---

## Measurements

All on the shared Azure Linux host (16 cores), against JDK 25.0.3+9. The host
carries other people's builds; every comparison below is **interleaved in both
orders within each round**, which is the only thing that makes a shared host's
numbers comparable at all. Spreads are quoted, not hidden — see the caveat at
the end.

### Item 2 — the descendant walk

`cargo test --release -p cratonvm-classloading --lib -- --ignored --nocapture
descendant_walk_scaling`, reproducible on any machine:

```
descendant walk over 20000 classes x 2000 upgrades
  index : 571.495µs (10000 descendants visited)
  scan  : 63.324098ms (40010000 classes probed)
  ratio : 110.8x
```

The shape is the real one — a JDK stub being upgraded has a handful of
descendants among tens of thousands of loaded classes — so the old scan paid
the whole class count on every upgrade to produce a five-element answer. The
item asked for "~50% cut in layout lookup time"; this part of it is 110x, and
the `ClassId -> layout` lookup the item actually named was already O(1) before
this work started.

### Item 1 — BinaryTrees and Sieve

`bench/CratonBench.java`, phases `bintrees` (the classic depth-18 binary-trees
kernel) and `sieve` (100,000-limit sieve x 20,000 reps), isolated-process, 3
rounds x 2 orders x 2 binaries = 6 samples per cell. Checksums were identical
in **every** cell (`68332206` / `9592`), which is the half of this that is not
noise-sensitive.

Median ms, moving-young ON (the shipped default) vs OFF
(`CRATONVM_GC=-moving-young`):

| phase | OFF | ON | ON/OFF |
|---|---:|---:|---:|
| bintrees | ~2020 | ~3600–4950 | **~2.1x** |
| sieve | ~2580–2920 | ~2680–3070 | ~1.1x |

**This is the documented state, not a regression.**
`docs/moving-young-throughput.md` measured bt18 at 1363 ms default vs 2905 ms
moving-young — **2.1x** — after replacing the `young_object_starts` hash set
with a bitmap, and recorded that as the copying collector's accepted cost. The
retest lands on the same 2.1x, on both binaries, with matching checksums.

What the item's bug actually did was different in kind: the deleted
`!allow_moving_young` term made *every* JIT-active cycle take the non-moving
sweep no matter what the flag said, so the copying collector could not be
selected at all. That is gone. What is left is the copying collector costing
what a copying collector costs — and it is the collector that completes bt18
at `-Xmx512m`, which the default sweep cannot run at all.

### No regression from this branch

`fix2` (this branch) was first compared against a binary built from
`origin/dev` **before** the 11 commits this branch later merged — including
`fix(jit): give the operand stack a type model, closing the OSR veto`, which
lands directly on the recursion-and-allocation loop `bintrees` is. That
comparison is not attributable and is not reported. The control below is
`origin/dev` HEAD built from the same tree, so the only difference is this
branch.

**bintrees, control vs branch, 6 rounds x 2 orders x 2 moving-young arms**
(48 samples). Checksums identical throughout.

| arm | median | mean | min |
|---|---:|---:|---:|
| control, moving-young off | 3707 | 4193 | 2522 |
| branch, moving-young off | 4095 | 4042 | 2272 |
| control, moving-young on | 4383 | 4539 | 2511 |
| branch, moving-young on | 4971 | 4739 | 3416 |

**Inconclusive, and reported as such.** The branch is nominally slower by
median in both arms and nominally *faster* by mean and by min in the OFF arm —
the signs disagree between statistics and between arms. The control's own
spread is 2511–8089 ms, a 3.2x range on one binary in one arm, which is larger
than every difference in the table. Nothing here is resolvable at this host's
noise level, and the ON/OFF ratio is not resolvable in this run either (it
reads ~1.15x here against the ~2.1x the quieter earlier run and
`moving-young-throughput.md` both give) — which is the clearest single
statement of how noisy the run was.

**So the question was asked somewhere it can actually be answered.** `bintrees`
loads about thirty classes, unloads none and interrupts no thread, so nothing
this branch changes is even reachable in its hot loop. What the branch *does*
touch on a per-class basis is VM boot: one adjacency insert per
`ClassStore::add`, one reverse-index insert per first
`register_class_layout` of a `(class, field-count)` pair. A boot-dominated
workload (`Hello.main`, wall clock, one warm-up per binary, 15 iterations x 2
orders = 30 samples each, interleaved):

| | median | min |
|---|---:|---:|
| control | 785 ms | 351 ms |
| branch | 765 ms | 399 ms |

Indistinguishable — 2.5% apart by median, in the branch's favour, and the
opposite sign by min. That is the expected result: ~400 boot classes x one
hash insert is microseconds against a ~780 ms boot.


### Caveat on the timings

`bintrees` ranged 2368–8089 ms across samples of the *same* binary in the
*same* arm, on a host running other people's cargo builds at load 10–27. The
ON/OFF ratio is reported because it is large, consistent in sign across every
round and both orders of the quieter run, and independently documented at the
same 2.1x by `moving-young-throughput.md` — three things the control-vs-branch
comparison has none of. Any difference of a size comparable to the spread
itself is **unmeasured** on this host, not measured-and-small; that is why the
branch's cost was re-asked on the boot path, where the change is actually
reachable, instead of being declared absent from a bintrees table.

---

## The original list

> **[GC] Incorrect "moving young" logic:** Symptom: Certain GC benchmarks fall
> back to a slow non-moving young-gen collection even when moving GC is
> enabled. Root cause: In `gc/src/heap.rs`, the guard for using moving GC was
> inadvertently made unconditional. Fix: Change the guard logic so that if
> `CRATONVM_MOVING_YOUNG=1`, the semispace copy collector is used for young
> gen. For example, in `Heap::collect_young_if_threshold()`, replace the logic
> `if self.is_active() && !self.allow_moving_young { ... }` with a check that
> allows moving when permitted (remove or alter the `!self.allow_moving_young`
> clause). Retest `BinaryTrees` and `Sieve` benchmarks to ensure the fix
> eliminates the regression. (TODO in gc/src/heap.rs at lines around where
> `fail_closed_non_moving` is set.)
>
> **[Performance] Layout registry lookup hotspot:** Symptom: Profile shows a
> large fraction of runtime spent resolving object layouts by scanning class
> hierarchy. Root cause: The code does a linear search each time a class layout
> is needed. Fix: Modify `classloading::LayoutRegistry` (or similar) to cache
> mappings from `ClassId` to layout index. For example, add a
> `HashMap<ClassId, LayoutId>` updated whenever a new layout is registered. In
> the lookup function, first check the map. This eliminates repetitive linear
> scans. Measure impact to confirm ~50% cut in layout lookup time.
>
> **[Concurrency] Unsafe ObjectRef sharing:** Symptom: `ObjectRef` is marked
> `unsafe impl Send/Sync` despite potential multithreaded mutation. Root cause:
> Historical assumption of single-threaded mutator is outdated. Fix: Audit all
> uses of `ObjectRef` across threads. If indeed Java object references are
> passed between threads, ensure correctness by adding necessary memory fences
> or marking fields `volatile` in JNI terms. If safe sharing is not guaranteed,
> remove the unsafe impl and use explicit `Arc<Mutex<Object>>` when needed. At
> minimum, add comments documenting why `Send/Sync` is (or is not) safe under
> the one-thread-per-Java-thread model. (Edit `types/src/value.rs` around the
> `unsafe impl` of `ObjectRef`.)
>
> **[Logic] Monitor notify bug:** Symptom: `wait()` may hang if a thread is
> interrupted right before `notify`. Root cause: The `monitor` code may miss a
> notification or double-wait. Fix: In `vm/src/threading/monitor.rs`, ensure
> `notify()` uses `Condvar::notify_one()` under the same lock that `wait()` is
> doing `wait()`. Check that `wait()` rechecks the condition in a loop. If
> necessary, restructure to use Rust's `Condvar` correctly (lock guard + loop
> on predicate). Add tests where thread A waits with a timeout and thread B
> notifies to verify no deadlock.
>
> **[Interop] Incorrect synthetic JDK handling:** Symptom: Running with
> `--synthetic-jdk` on a binary built without `synthetic-jdk` feature silently
> fails to find classes. Root cause: The launcher in `vm-cli` rejects synthetic
> mode only by fatal error, but the check may be incomplete. Fix: In
> `vm-cli/main.rs`, make sure that if `--synthetic-jdk` is passed without the
> `synthetic-jdk` feature, the program exits with a clear error. Update the
> usage docs accordingly. Conversely, ensure `--real-jdk` correctly overrides
> synthetic mode. (Check `VmConfig::for_launcher()` in `vm/src/config.rs`.)
