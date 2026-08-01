# The JIT's pending exception was an unrooted heap reference in TLS

**Status:** fixed (scan half needs one line in `memory/roots.rs` — see
"Outstanding" below). Census of the surrounding directories: complete, no other
offenders.

## The bug

`JitSignals::exception` (`vm/src/jit/helpers.rs`, the `JIT_SIGNALS`
`thread_local!`) was a `Cell<Option<ObjectRef>>`. Every JIT helper that raises a
Java throwable parks it there and returns the `i64::MIN` deopt sentinel; the
interpreter drains it after the compiled frame unwinds and routes it through the
method's exception table.

For the whole of that window the throwable was a **live heap reference that no
root provider knew about**:

* it was never pushed by `memory/roots.rs::collect_roots`, so a collection could
  reclaim it;
* it was never rewritten by `memory/gc.rs::update_all_roots`, so a *moving*
  collection left the interpreter's drain reading a from-space address.

A thread-local cannot be fixed by adding a root source. `VM_ROOT_SOURCES`
(`vm/src/memory/native_roots.rs`) callbacks run on the **collecting** thread, and
a collecting thread cannot reach a parked peer's TLS. The VM-agnostic
`register_native_root_source` registry that used to offer a second door was
deleted in `64e6d61b4` for a related reason. The only mechanism that reaches
per-thread state is a `JvmThread` field — which is exactly why
`native_pending_return` (`gc.rs` §-native-return) and `pending_async_exception`
(`roots.rs` §10 / `gc.rs` §10) are fields and not thread-locals.

### Verified, not assumed

Re-checked by reading, at the cited lines:

| claim | evidence |
| --- | --- |
| the cell is a live `ObjectRef` in TLS with no root provider | `vm/src/jit/helpers.rs:303` (`JIT_SIGNALS`), field `exception: Cell<Option<ObjectRef>>` at the `JitSignals` struct |
| `register_native_root_source` is gone | no definition or caller in the tree; `vm/src/memory/native_roots.rs:50-58` documents the deletion |
| `VM_ROOT_SOURCES` has no JIT row | the 25-row table at `vm/src/memory/native_roots.rs:302-351` |
| the collector reaches `pending_async_exception` because it is a **field** | scan `vm/src/memory/roots.rs:534-540`, remap `vm/src/memory/gc.rs:897-911` |
| both halves are per-**current**-thread | `collect_roots(shared: &SharedVm, thread: &JvmThread)` (`roots.rs:65`), `update_all_roots(shared, thread: &mut JvmThread, ..)` (`gc.rs:365`) |

### The window is not theoretical

`jit_alloc_oom` (`vm/src/jit/helpers.rs`) constructs an `OutOfMemoryError`,
stashes it, and returns the null sentinel. It is called *because the heap is
exhausted*, so a collection between the stash and the drain is not a
possibility — it is the expected next event. The singleton-OOME fallback in the
same function had the same exposure.

The other two named windows both **re-execute the callee in the interpreter**
while the exception is in hand:

* `route_implicit_exc_through_callee` — drains the signals, then either resumes
  the callee at its handler or bails to the interpreter;
* `handle_compiled_callee_deopt_sentinel` — same shape for the MIC/PIC direct-call
  arm, and re-stashes on the no-handler path.

Both funnel through `try_run_callee_handler`, which is where the second half of
the fix went (below).

## What changed

1. **`vm/src/threading/jvm_thread.rs`** — new field, next to its sibling:

   ```rust
   pub jit_pending_exception: Option<ObjectRef>,
   ```

   initialised to `None` in `JvmThread::new`. The scalar signals (`athrow_bci`,
   `aioobe`, `arithmetic`, `npe`, `npe_action`, `deopt`) stay in `JIT_SIGNALS` —
   the collector has no interest in them, and keeping them there preserves the
   one-TLS-access drain that consolidation was built for. `JitSignals` now
   carries a "do not add a heap reference to this struct" note.

2. **`vm/src/jit/helpers.rs`** — the stash/drain API takes the thread:

   | before | after |
   | --- | --- |
   | `set_jit_pending_exception(exc)` | `set_jit_pending_exception(thread, exc)` |
   | `set_jit_pending_exception_with_bci(exc, bci)` | `set_jit_pending_exception_with_bci(thread, exc, bci)` |
   | `stash_jit_pending_exception(exc)` | `stash_jit_pending_exception(thread, exc)` |
   | `take_jit_pending_exception()` | `take_jit_pending_exception(thread)` |
   | `take_all_jit_signals()` | `take_all_jit_signals(thread)` |

   The thread had to become explicit rather than being resolved from the
   `JIT_THREAD` TLS pointer, because `restore_jit_thread` clears that pointer
   **before** the interpreter's drain runs (`invoke.rs`, the
   `restore_jit_thread(saved_jit_thread); take_all_jit_signals()` pair) — a
   pointer-resolved drain would read a null thread and silently strand every
   JIT-raised exception.

   Two readers keep the pointer-resolved form, and only because they are
   non-mutating peeks whose call sites cannot name a thread:
   `jit_pending_exception_is_set()` and the `extern "C"` `jit_dispatch_threw()`.
   Both take a null-pointer early-out.

   `jit_throw_exception` (the `athrow` lowering, `extern "C"`, no thread
   argument) acquires the thread itself. That is safe by construction:
   `compiler.emitted_athrow` forces `has_dispatch` (`jit/src/x64.rs`, the
   `has_dispatch` computation), and `has_dispatch` is precisely what makes
   `execute_jit_call` install `JIT_THREAD`. The impossible branch carries a
   `debug_assert!` rather than a silent drop.

3. **`vm/src/memory/gc.rs`** — the remap half. §10 is now
   `remap_thread_object_slots(thread, pointer_map)`, a small factored function
   covering `java_thread_obj`, `pending_async_exception` and
   `jit_pending_exception` together, so the set is visible in one place and
   unit-testable without a VM (same shape as the existing `remap_handle_slots`).

4. **`vm/src/jit/helpers.rs::try_run_callee_handler`** — closes the *other* half
   of the named windows. Both re-execution paths drain the throwable out of the
   thread slot into a Rust local before re-entering the interpreter, so moving
   the field alone does not protect it there. The local is now pinned in
   `thread.native_pin_roots` (scanned *and* remapped) across
   `resolve_callee_cached` — which can load the callee's class, hence allocate —
   and across `run_jit_callee_handler`, and re-read from the pin slot afterwards
   so a relocation is picked up instead of a from-space address being handed to
   the interpreter.

### Tests

* `vm/src/memory/gc.rs`
  * `moving_gc_rewrites_every_per_thread_object_slot` — all three §10 slots follow
    a move.
  * `per_thread_object_slot_remap_leaves_unmoved_and_empty_slots_alone` — a
    non-empty map that does not mention the object must not perturb it.
  * `jit_pending_exception_survives_and_follows_a_moving_collection` — the real
    shape: allocate a stand-in throwable, stash it on the thread, run an actual
    `collect_garbage` that relocates it, remap, and assert the drained reference
    is the post-move address **and** that the object's field is still readable.
    Asserts the object actually moved first, so the test cannot pass vacuously.
* `vm/src/jit/helpers.rs`
  * `a_stashed_jit_exception_is_reachable_through_the_thread_it_belongs_to` —
    the stash lands in the `JvmThread` slot (not TLS), and draining a *different*
    thread does not steal it.
  * `take_jit_pending_exception_consumes_the_thread_slot`.
  * `the_athrow_bci_still_pairs_with_the_thread_resident_exception` — the scalar
    that stayed in TLS still travels with the thread-resident throwable and
    resets on drain.
  * `the_pending_exception_peek_is_null_safe_without_a_jit_thread`.

## Census — thread-local heap references in `vm/src/jit/`, `vm/src/threading/`, `vm/src/memory/gc.rs`

Every `thread_local!` in the three directories. "Heap ref?" means: does it hold
an `ObjectRef`, a `Value`, or a raw address into the Java heap?

| holder | what it holds | heap ref? | exposure window | scanned? | remapped? | action |
| --- | --- | --- | --- | --- | --- | --- |
| `helpers.rs` `JIT_SIGNALS.exception` | pending Java throwable | **yes** | JIT helper stash → compiled unwind → interpreter drain; includes a guaranteed GC in the OOM case | no → **yes** | no → **yes** | **moved to `JvmThread::jit_pending_exception`** |
| `helpers.rs` `JIT_SIGNALS` (`athrow_bci`, `aioobe`, `arithmetic`, `npe`, `npe_action`, `deopt`) | scalars (bci, index/length, flags) | no | — | n/a | n/a | stays in TLS; struct doc now forbids adding a reference |
| `helpers.rs` `JIT_THREAD` | `*mut JvmThread` | no (VM-side struct, not heap) | JIT call duration | n/a | n/a | none |
| `helpers.rs` `CURRENT_JIT_CALLEE` | `String` (diagnostic method name) | no | — | n/a | n/a | none |
| `helpers.rs` `JIT_THREAD_BORROWED` | `bool`, debug only | no | — | n/a | n/a | none |
| `helpers.rs` `VIRTUAL_TARGET_CACHE` | `(JitSiteKey, class id) -> Rc<str> + flags` | no | — | n/a | n/a | none |
| `helpers.rs` `DISPATCH_MEMO_CLASS_IDENTITY` | `(epoch, bool)` | no | — | n/a | n/a | none |
| `helpers.rs` `DISPATCH_CACHE` / `DISPATCH_COUNTER` / `VIRTUAL_DISPATCH_CACHE` | compiled entry addresses + owning `Arc<CompiledMethod>` | no (code cache, not heap) | — | n/a | n/a | none |
| `helpers.rs` `OBJECT_NATIVE_DISPATCH_CACHE` / `INTEGER_NATIVE_DISPATCH_CACHE` | native callbacks, class ids, `NativeMethodId` | no | — | n/a | n/a | none |
| `helpers.rs` `INTEGER_WRAPPER_CLASS_CACHE` / `MATCHER_CLASS_CACHE` / `HASHMAP_CLASS_CACHE` / `CONCURRENT_HASHMAP_CLASS_CACHE` | `(vm_identity, class id)` | no | — | n/a | n/a | none |
| `helpers.rs` `INTEGER_ALLOC_SLOTS` | `(vm key, class id, slot count)` | no | — | n/a | n/a | none |
| `helpers.rs` `JIT_TYPECHECK_TARGET_CACHE` / `JIT_TYPECHECK_ANSWER_CACHE` / `JIT_SUBTYPE_POSITIVE_CACHE` | class ids + pointers into the immutable JIT string table | no | — | n/a | n/a | none |
| `helpers.rs` `JIT_DISPATCH_DEPTH`, `DISPATCH_CACHE_SUPERSEDE_EPOCH`, `DISPATCH_CACHE_JIT_GENERATION`, `JIT_SELF_CALL_STACK_FLOOR` | counters / epochs / a native stack address | no | — | n/a | n/a | none |
| `conservative_roots.rs` `JIT_SCAN_CACHE` | `Vec<ObjectRef>` — cached conservative roots | **yes, but a cache** | between two `scan_active_jit_frames` calls | n/a | n/a | **none — correctly keyed, not rooted.** Keyed on `(boundary gen, chain len, heap_id, collection_count)`; any collection invalidates it, and it is disabled outright under `CRATONVM_DBG_FORCE_MOVING` / `CRATONVM_SHADOW_STACK`. This is the "address-keyed cache ≠ root provider" shape: sweep the dead key, do not root it. Rooting it would be actively wrong — it would resurrect garbage the previous scan happened to see. |
| `conservative_roots.rs` `JIT_ENTRY_CHAIN` | native stack pointers + `*const CompiledMethod` | no | — | n/a | n/a | none |
| `conservative_roots.rs` `TOP_RBP`, `UNREG_JIT_VERIFIED_LO`, `CACHED_HIGH`, `JIT_BOUNDARY_GEN`, `RANGE_SNAPSHOT` | native stack addresses, JIT code ranges, counters | no | — | n/a | n/a | none |
| `threading/monitor.rs` `CAS_LOCK_CACHE` | `object_key: usize` — a raw heap address used as a **key**, plus a non-owning `*const Mutex<()>` | address only, never dereferenced as an object | one `Unsafe.compareAndSet*` | n/a | n/a | **none — epoch-invalidated.** `remap_after_gc` / `prune_dead` bump `cas_lock_epoch` under the STW token before any mutator resumes, so an entry never outlives the collection that could invalidate it. Same posture as `JIT_SCAN_CACHE`. |
| `threading/event_loop.rs` `CURRENT_EVENT_LOOP` | `Weak<EventLoop>` (a Rust struct) | no | — | n/a | n/a | none |
| `threading/thread_registry.rs` `CACHED` (`self_async_slot`) | `(registry id, ThreadId, Arc<AtomicUsize>)` | it caches an `Arc` to the slot, not a reference; the *slot* does hold a raw `ObjectRef` address | cross-thread `Thread.stop0` → target's next safepoint | **yes** (`collect_all_root_snapshots` pushes a non-zero slot) | **yes** (the `pointer_map` store in `update_after_gc`) | none — already closed, with tests `b1_async_exception_slot_is_a_root` / `b1_async_exception_slot_is_remapped`. This is the shape the JIT signal should have had. |
| `threading/thread_state.rs` `SELF_CELL`, `PENDING_THREAD_ID` | census cell handle, `u64` | no | — | n/a | n/a | none |
| `threading/virtual_threads.rs` (test) | test-only | no | — | n/a | n/a | none |

`vm/src/memory/gc.rs` declares no `thread_local!` of its own. `vm/src/jit/xt_root_scan.rs`,
`alloc_class_cache.rs`, `code_cache_lifecycle.rs` and `skip_list.rs` hold only
counters, `OnceLock`ed env-var reads and code-cache bookkeeping.

## Outstanding

### 1. The scan half needs one line in `memory/roots.rs` (not this lane's file)

`gc.rs` owns only the *remap*. The matching scan lives in
`vm/src/memory/roots.rs` §10 and must gain a third push, or the object can still
be **collected** while stashed (the remap then faithfully rewrites a reference to
a reclaimed slot, which is worse than either failure alone):

```rust
    if let Some(ref obj_ref) = thread.pending_async_exception {
        roots.push(*obj_ref);
    }
    // The JIT's out-of-band pending throwable — paired with the remap in
    // `gc.rs::remap_thread_object_slots`. See
    // `docs/known-issues/jit-signals-root-gap.md`.
    if let Some(ref obj_ref) = thread.jit_pending_exception {
        roots.push(*obj_ref);
    }
```

Until that lands the fix is half-wired, which is the exact defect this document
describes. It is one line because the storage is now in the right place; it was
impossible before.

### 2. Peer threads

Both halves take a single `&JvmThread` — the collecting thread's. A **parked
peer's** `jit_pending_exception` is no more visible than its
`pending_async_exception` is. This is a pre-existing property of the whole §10
family, not a regression, and it is strictly better than the previous state: a
field can be reached by a future cross-thread scan (the `root_snapshot` /
`jit/xt_root_scan.rs` machinery), a thread-local never can.

### 3. `jit/src/deopt.rs` — the same defect, outside these directories

`LAST_DEOPT` and `LAST_EXCEPTIONAL` (`jit/src/deopt.rs:1626` and `:1648`) are
`thread_local! RefCell<Option<ReconstructedFrame>>`. A `ReconstructedFrame`'s
slots include `FrameValue::Object(u64)` — **raw heap addresses**
(`jit/src/deopt.rs:141`). They are published by a deopt stub and consumed by the
interpreter's materialisation, and `route_implicit_exc_through_callee` runs a
whole interpreter re-execution between the two. Neither scanned nor remapped, for
the same structural reason. Same fix shape (move the stash onto `JvmThread`), but
`jit/` is a different crate and a different lane's file set.

### 4. `get_current_thread`'s declared signature (found while wiring the ABI checks)

Unrelated to roots; recorded here because it is the one row the new signature
checks could not cover. `jit-api` declares
`HelperFnGetCurrentThread = unsafe extern "C" fn() -> *mut c_void`, but
`build_helpers` installs `jit_get_current_thread`, which returns
`*mut JvmThread`. ABI-identical, but fn-pointer types are invariant in their
return type, so the slot cannot be bound to its own alias and no cast expresses
it. Fixing it is a one-word change to that `helper_fn_slots!` row in
`jit-api/src/helpers_abi.rs`.

## Rule of thumb

A `thread_local!` is invisible to the collector. If a value in one can be a heap
reference and can survive a safepoint, it belongs on `JvmThread`, with a push in
`roots.rs` and a rewrite in `gc.rs` — **both**, in the same change. An
address-keyed *cache* is the one exception, and it must then be invalidated by
`collection_count` or an STW-bumped epoch, never rooted.
