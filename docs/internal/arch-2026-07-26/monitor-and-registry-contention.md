# Monitor and thread-registry contention

**Slug:** `monitor-and-registry-contention`
**Date:** 2026-07-26
**Basis:** merged `dev` @ `6495a191c` into the worktree branch (the worktree was
originally cut from `e4e4053bb` = `origin/main`; the merge was clean, and the two
files this work owns had drifted by only 4 and 5 lines respectively).
**Files changed (owned):** `vm/src/threading/monitor.rs`,
`vm/src/threading/thread_registry.rs`.
**Files inspected, not changed:** `types/src/heap_types.rs`,
`types/src/lock_order.rs`, `gc/src/collector.rs`, `gc/src/{gc,g1,gen_heap,region}.rs`,
`vm/src/vm/realms/thread_realm.rs`, `vm/src/threading/{jmm,varhandle}.rs`.

---

## 1. The premise

CratonVM spawns **one real OS thread per Java thread** (`thread_start` in
`vm/src/vm/vm_exec.rs` uses `std::thread::Builder`; ARCHITECTURE.md "Key Design
Decision #6" states it explicitly). Comments elsewhere in the tree that describe
Java execution as single-OS-threaded under cooperative scheduling are stale.
Global locks on synchronization fast paths are therefore genuine scalability
ceilings.

Two sat directly on those paths:

* **`MonitorTable`** serialized *every* inflated-monitor operation in the VM on
  one mutex — `monitors: OrderedMutex<FxHashMap<usize, Arc<Monitor>>>` — plus a
  second global map, `cas_locks`, beside it. Finding the monitor for object *X*
  took a process-wide lock regardless of which object, which thread, or whether
  anything was contending *X*.
* **`ThreadRegistry`** was a global `Mutex<FxHashMap<ThreadId, ThreadEntry>>`
  at **L5**, i.e. acquired while higher-level locks are held.

## 2. What is now off the global lock

### 2.1 Inflated monitors: mark word, not table

The encoding needed for this **already existed** — no `heap_types.rs` change was
required (see §6). `types/src/heap_types.rs` defines `MARK_INFLATED = 0b10` with
`INFLATED_PTR_MASK = !0b11`, the constructor `ObjectHeader::make_inflated(ptr)`
and the accessor `ObjectHeader::inflated_monitor(mark) -> *mut ()`. The mark word
already *stored* the monitor's address; nothing *read* it. Every lookup went
through the global map, and the pointer was dead weight.

Now the pointer is the lookup. These all resolve through a mark-word load plus
that monitor's own mutex, with **no table lock and no atomic refcount traffic**:

| Operation | Before | After |
|---|---|---|
| `enter` / `enter_or_contend` (INFLATED) | global mutex + hash probe | mark-word load → `Monitor::try_enter` |
| `exit` (INFLATED) | global mutex + hash probe | mark-word load → `Monitor::exit` |
| `wait` / `notify` / `notify_all` | global mutex + hash probe | `ensure_inflated` returns straight from the mark word |
| `holds` (`Thread.holdsLock`) | global mutex + hash probe | mark-word load |
| `current_owner`, `entry_count` | global mutex + hash probe | mark-word load |
| `jfr_enter_recorded` | global mutex + hash probe | mark-word load |

The thin-lock paths (uncontended and re-entrant single-thread) were already
allocation-free and table-free; they are unchanged.

Only the *contended* arm clones an `Arc`, because the caller is about to park on
the monitor and must own a reference across the park. The uncontended arm borrows.

### 2.2 The residual table is a sharded enumeration index

`MonitorTable` is no longer how a monitor is found. It is a 64-way sharded index
(fibonacci-hashed on the object address) whose only consumers are cold:

* `release_monitors_held_by` — thread-death monitor release;
* `remap_after_gc` / `prune_dead` — GC re-key and prune;
* `with_cas_lock` — per-object CAS locks, which have **no** mark-word home and so
  are genuinely address-keyed (also sharded now);
* the `ThreadId > u32::MAX` legacy path, which cannot be represented in a thin
  lock and so has no mark-word home either;
* diagnostics (`Debug`, `CRATONVM_DBG_MONEXIT` forensics).

Inflation touches exactly one shard, once per object, ever.

### 2.3 A whole failure class removed

Because the mark word is now authoritative and the index is derived, the
"`INFLATED` mark word but no registry entry" state is no longer a contradiction.
The old code treated it as an invariant violation and, before the hardening that
made it a hard error, *recovered* by synthesising a **second** `Monitor` for the
same object — orphaning every thread parked on the first (the audit-finding-1(b)
tripwire; the gdb-captured "5 waiters, none woken" MTChurn pile-up). That branch,
its rate-limited warning, its `IllegalStateException`, and the six
`.expect("registry/mark-word desync")` panic sites are all gone. A missing index
entry is now repaired from the mark word (`index_repair_if_absent`).

### 2.4 Thread registry

* `threads` is an **`RwLock`**. Of ~50 accessors, 14 mutate; the other ~40 only
  *find* an entry and then operate on the atomics and per-entry locks inside it
  (`is_alive`, `get_park_state`, `is_blocked`, `java_block_state`,
  `frame_trace_of`, the JMX readers, and every O(N) safepoint census the
  collector runs). Those now proceed concurrently.
* **`take_async_exception` takes no registry lock at all** after a thread's first
  safepoint. It runs on every safepoint poll of every thread and is always a
  self-lookup — the hottest "look up my own entry" path there is. Its
  `Arc<AtomicUsize>` slot is created once at registration and never replaced (no
  setter exists; `post_async_exception` writes through the same allocation), so
  it is cached in a thread-local.

Note that the *other* per-thread hot state — `park_state`, `interrupted`,
`root_snapshot`, `frame_trace`, `gc_block_state` — is already `Arc`-shared with
the owning `JvmThread` (that is what `set_root_snapshot` / `set_park_state` /
… exist for). A thread reaching its own copies of those **never went through the
registry to begin with**, so no handle was needed for them.

## 3. Lifetime / reclamation argument

An `INFLATED` mark word **owns one strong `Arc<Monitor>` reference**, leaked into
it by the publishing CAS (`MonitorTable::publish_inflated` clones, CASes, and on
success `mem::forget`s the clone, recording the fact in `Monitor::mark_ref`). So
for as long as a mark word reads `INFLATED`, the strong count of the monitor it
names is ≥ 1 by construction — which is exactly what makes borrowing (`&Monitor`)
or upgrading (`Arc::increment_strong_count` + `Arc::from_raw`) through that raw
pointer sound. That reference is released at exactly **one** site:
`MonitorCleanup::prune_dead`, which the collector calls with an *exact* set of
just-swept addresses. A thread can only load an object's mark word while holding
a live reference to that object, so a swept object's mark word has no possible
reader and the release cannot race a load; and any thread parked in `block_enter`
or `wait` reached there through an `Arc<Monitor>` it holds for the whole park, so
the release is never the last drop under a waiter. `Monitor::mark_ref` is cleared
with a `swap`, making a second release a no-op rather than a double free.
Relocation is free: a moving collector byte-copies the header, so the pointer
travels with the object and stays valid (monitors live in the Rust heap, never
the Java heap), and no GC or header-walking code interprets the mark word as an
object reference — `gc/src/{gc,g1,gen_heap,region}.rs` all copy it verbatim.

### 3.1 The hazard this change had to close (and did)

`remap_after_gc` previously reclaimed monitors whose address was **absent from
the GC forwarding map**, behind the default-off `CRATONVM_RECLAIM_DEAD_MONITORS`
flag. That signal means *dead* only for a whole-heap collector; for a partial one
(G1 young/mixed, generational minor GC) `pointer_map` lists only *moved* objects,
so a live old-gen / non-CSet survivor is routinely absent. Dropping such an entry
left a **live** survivor's `INFLATED` mark word pointing at a freed `Monitor`.
The old code survived this only because nothing ever dereferenced that pointer —
lookups went through the map, which is precisely why the failure surfaced as the
second-monitor/orphaned-waiter bug in §2.3 rather than as a crash.

Making the mark word the lookup would have turned that latent bug into a real
use-after-free. **So the branch is removed**, along with its env flag. Nothing is
lost: the flag was default-off and had therefore never run — one more instance of
the repo's documented "capability lands behind a `CRATONVM_*` var defaulting to
off and never executes" pattern. Sound reclamation needs an exact dead set, which
is what `prune_dead` already receives; that path is unconditional, on by default,
and now releases the mark-word reference too, so it *genuinely frees* where
before it only dropped one of two references.

Consequence to be honest about: under the **moving** collectors a dead object's
monitor is now retained until the process exits, because only the ZGC backend
calls `prune_dead`. That is a bounded leak (one `Monitor` per object that was ever
contended or `wait()`ed on) and is strictly preferable to a dangling mark word.
Closing it is the cross-file follow-up in §7.

## 4. Lock hierarchy

**The hierarchy is unchanged. No level was added, removed, or renumbered, and no
edit to `types/src/lock_order.rs` is required for this work to be correct.**

Verified against the merged tree: `types/src/lock_order.rs` is the authoritative
definition on `dev` (it moved down from `vm` so the `gc` crate could name L8);
`vm/src/runtime/lock_order.rs` is now a `pub use` re-export shim. On `dev` the
wired levels are **L10 `class_manager`** (`OrderedPlRwLock`), **L7
`ref_processor`** (`OrderedPlMutex`) and **L6 `monitors`** — so the claim relayed
mid-task that "L10 is explicitly NOT runtime-enforced, only L6 is wired" describes
the pre-`dev` state and is out of date. L9 is reserved with no lock instance; L8
`heap` is unblocked but not yet wrapped.

What changed within L6:

* **Wrapper family:** the two `MonitorTable` maps moved from the std-backed
  `OrderedMutex` to the `parking_lot`-backed `OrderedPlMutex`. Same level, same
  enforcement, no poison plumbing. Both remain L6.
* **Cardinality:** L6 is now 128 lock instances (64 monitor shards + 64 cas-lock
  shards) rather than 2. The descending rule forbids acquiring a lock at a level
  *≤* the minimum held, so equal-level nesting is already a violation the checker
  catches. Every multi-shard walk — `release_monitors_held_by_except`,
  `remap_after_gc`, `prune_dead`, `indexed_monitor_count` — locks exactly one
  shard at a time. `remap_after_gc` additionally cannot refill in place (re-keying
  can move an entry across shards), so it drains shard-by-shard into a local
  `Vec` and then inserts shard-by-shard; it runs under STW, so the intermediate
  state is unobservable, and even if it were, the mark word is what locking
  consults.
* **Coverage, honestly:** the checker now *observes fewer* L6 acquisitions,
  because `monitorenter` / `monitorexit` genuinely no longer take an L6 lock.
  This **relaxes** a constraint rather than introducing one: code holding an
  L5/L4/L1 lock can now perform an inflated `monitorenter` without inverting the
  hierarchy. `Monitor::state` is an untracked `parking_lot::Mutex` and always was,
  so entering a monitor was already invisible to the checker either way.

`ThreadRegistry::threads` is L5 by documentation and is still not wrapped in an
ordered wrapper, exactly as before — changing `Mutex` → `RwLock` does not change
its level or its (absent) enforcement.

**Documentation drift worth fixing by whoever owns the file** (specified, not
made — I do not own `types/src/lock_order.rs`): its "Runtime enforcement status"
section says *"L6 `monitors` — both `MonitorTable` maps … are `OrderedMutex`"*.
That should read *"…are sharded `OrderedPlMutex` arrays (64 shards each); no path
holds two shards at once."* This is a comment-accuracy fix only; nothing depends
on it.

## 5. Deadlock-safety review of the `RwLock` conversion

`parking_lot::RwLock` reads are **not** recursion-safe, so nesting two
acquisitions on one thread would deadlock. Every `self.threads` site was checked:
each accessor either scopes its guard to a block and drops it before calling out,
or holds it for a self-contained walk. The specific patterns confirmed safe:

* `join()` scopes the write guard, then calls `mark_dead` / `is_alive`;
* `find_thread_id_by_thread_obj_tid_checked` `drop(threads)` before touching
  `java_tid_to_id` (a different mutex anyway);
* `recover_stale_mirror` drops `former_mirror_addrs` before `java_thread_obj`;
* `collect_all_root_snapshots` / `alive_count_and_os_tids` /
  `alive_count_blocked_and_os_tids` take `…read().len()` as a *statement*
  temporary that drops before the `_inner` call re-acquires;
* `set_os_tid_current`'s three `cfg` branches each scope their own guard.

The discipline is recorded in a doc comment on the field so a future edit does not
quietly reintroduce a nested acquisition.

### 5.1 The auto-trait bound this raises

Worth knowing because it is invisible at the edit site: `parking_lot::Mutex<T>`
is `Sync` when `T: Send`, but `RwLock<T>` is `Sync` only when `T: Send + Sync`.
So `ThreadEntry` must now be **`Sync`**, which it previously did not have to be,
for `ThreadRegistry` to stay `Sync` — and it must, because
`vm/src/vm/realms/thread_realm.rs` holds one by value inside the shared VM.

Every field was checked and the bound holds, but two of them hold it for
non-obvious reasons: the bare (unlocked) `java_thread_obj: Option<ObjectRef>`
depends on `ObjectRef`'s `unsafe impl Sync` (`types/src/value.rs`), and
`join_handle` depends on `std::thread::JoinHandle<T>` being unconditionally
`Sync`. If either ever changes, the fix is to put those two fields behind their
own locks — not to revert the `RwLock`. This is recorded on the field itself.

Collector-thread safety: STW root scanning iterates the registry from a thread
that is not a registered Java thread. It only ever *reads*, so it now takes a
shared lock and no longer excludes other readers; nothing about it requires
registration. The `take_async_exception` thread-local cache is only consulted by
the owning thread (it is documented as never cross-thread), and the collector does
not use it.

## 6. `heap_types.rs`: **no edit needed**

Re-verified on the merged tree. `types/src/heap_types.rs` already provides
everything this design requires:

* `MARK_INFLATED = 0b10`, `MARK_STATE_MASK = 0b11`, `INFLATED_PTR_MASK = !0b11`;
* `ObjectHeader::make_inflated(monitor_ptr: usize) -> u64` — asserts the low 2
  bits are clear and that the pointer is a plausible user-space address;
* `ObjectHeader::inflated_monitor(mark: u64) -> *mut ()`;
* `MARK_WORD_OFFSET = 32` inside a 40-byte `HEADER_SIZE`, pinned by a
  compile-time `offset_of!` assertion.

(Note: `MARK_WORD_OFFSET` is **32**, not the 24 quoted in the task brief;
`HEADER_SIZE` is 40. The brief's offset is stale — the constant and its
compile-time assertion agree on 32.)

The only new requirement this work imposes is that `Monitor` be ≥ 4-byte aligned,
which is now stated rather than assumed: `Monitor` carries `#[repr(align(8))]`.
Nothing in `heap_types.rs` needs to change, now or for the follow-ups in §7.

## 7. Follow-ups (specified, not done — outside file ownership)

1. **Exact dead set for the moving collectors.** Give `Heap`, `GenerationalHeap`
   and `G1Collector` a way to hand `MonitorCleanup` an exact swept-address set
   (or call the existing `prune_dead` alongside `remap_after_gc`). Touches
   `gc/src/collector.rs` and the `remap_after_gc` call sites in `gc/src/heap.rs`,
   `gc/src/g1.rs`, `gc/src/gen_heap.rs`. This is the *only* thing standing between
   the current bounded retention (§3.1) and full monitor reclamation under every
   collector; the VM-side predicate is already written and pinned by tests
   (`Arc::strong_count(m) == m.structural_refs() && m.is_idle()`).
2. **`types/src/lock_order.rs` comment refresh** — the exact wording is in §4.
3. **Thin locks held by dead threads.** `release_monitors_held_by` walks inflated
   monitors only, so an *uncontended thin lock* still held by a dead thread is not
   swept. This is pre-existing and benign (nothing was contending it, so nothing is
   blocked), and the contention-point recovery in `vm_exec::monitor_enter_blocking`
   handles the case where someone later does contend it. Recorded because the
   mark-word design makes a heap-walking sweep newly feasible if it ever matters.
4. **Deeper `ThreadEntry` handles.** The registry map could hold
   `Arc<ThreadEntry>` with interior mutability on its ~9 mutable fields, letting
   *every* self-lookup skip the map rather than just `take_async_exception`. Not
   done: it is a ~60-site mechanical refactor with real borrow-lifetime traps
   (`let g = entry.root_snapshot().lock();` does not compile as a one-liner), and
   this wave forbids building. The `RwLock` conversion captures most of the win at
   a fraction of the risk. If it is taken up, do it as its own change so it can be
   compiled and tested in isolation.

## 8. Not touched, and why

`vm/src/threading/jmm.rs` and `vm/src/threading/varhandle.rs` are owned by this
task and do contain global `Mutex<HashMap<…>>` state (`JmmManager`'s clock and
race-detection maps; `VarHandleTable`). **Both modules are dead code**: they are
declared in `vm/src/threading/mod.rs` and have **no callers anywhere in the
workspace** — the VM's real `VarHandle` support lives in the interpreter and
`native-builtins`, not here. Their locks are therefore not a contention source,
and rewriting unused code would add merge risk for zero measurable benefit. Left
alone deliberately; flagging them as dead-code candidates is a separate call.

## 9. Test coverage added

`vm/src/threading/monitor.rs`:

* `inflated_lock_unlock_never_touches_the_shared_index` — the headline property,
  proven rather than asserted: the test **holds the L6 shard guard for the
  object's key** while a second thread completes a full lock / re-entrant lock /
  unlock cycle plus `holds` / `current_owner` / `entry_count`. If any of them
  regressed to probing the table, the worker would block and the test's
  `recv_timeout` would fail it (the guard is released before the assert so a
  regression fails rather than hangs).
* `uncontended_fast_path_never_allocates_or_indexes` — 1000 cycles, index stays
  empty, mark word never inflates.
* `recursive_entry_survives_thin_lock_overflow_into_inflation` — 256 nested
  acquires stay thin; the 257th inflates and carries the count; 257 exits unwind
  exactly; the 258th is an IMSE, not a wrap.
* `contended_handoff_between_two_threads_on_an_inflated_monitor` — asserts the
  contender actually observed contention, that the handoff completes, and that
  **no second monitor was created** (the §2.3 failure shape).
* `wait_notify_round_trip_on_a_mark_word_reachable_monitor` — asserts the mark
  word is byte-identical afterwards, i.e. no republish.
* `mark_word_owns_one_strong_ref_and_release_is_idempotent` — pins the refcount
  arithmetic (index + mark word + handle = 3) and that a double release does not
  double-decrement.
* `a_parked_waiter_keeps_its_monitor_unreclaimable_and_alive` — a real parked
  waiter, asserting `strong_count > structural_refs` while parked, that the
  reclaim predicate refuses it, and that it becomes reclaimable only after the
  waiter leaves.
* `a_lost_index_entry_is_repaired_not_re_inflated` — regression guard for §2.3.
* `shard_selection_is_in_bounds_and_spreads_consecutive_addresses`.

`vm/src/threading/thread_registry.rs`:

* `registry_reads_do_not_exclude_each_other` — main thread holds a read guard;
  8 worker threads must all complete registry reads. Deadlocks under the old
  `Mutex`; passes under the `RwLock`.
* `registry_writes_still_exclude_readers` — the converse, so the conversion is
  not silently degenerate.
* `cached_self_async_slot_is_the_registrys_slot` — the cached `Arc` must *be* the
  map's `Arc`, or a cross-thread `Thread.stop` would never be observed.
* `self_slot_cache_does_not_leak_between_registries` — pins the reason the cache
  key is a monotonic registry id and not an address.
* `cached_slot_survives_entry_reaping` — cache stays sound after the terminated-
  entry purge removes the map entry.

**Not run.** This wave forbids `cargo build` / `test` (nine concurrent builds
would OOM the host); the orchestrator builds after merging.
