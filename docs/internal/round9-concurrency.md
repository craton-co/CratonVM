# Round-9 Concurrency Audit

Workspace-wide, focus on round-8 changes. **CRIT**=race/UB,
**HIGH**=perf/correctness, **MED**=doc/cleanup.

---

## CRIT-1 — CHM segment resize lock still uses `std::sync` + poison dance
`native-collections/src/lib.rs:145-163, 14591-92, 1373-74`

Round-8 migrated `synthetic_field_store` / `class_atomic_side_store` to
parking_lot but missed the equally hot `chm_seg_resize_locks`: outer
`std::sync::Mutex<HashMap<usize, Arc<std::sync::RwLock<()>>>>` and inner
`std::sync::RwLock<()>` — both with `unwrap_or_else(|e| e.into_inner())`.
Every `chm_seg_get` lock-acquires the outer Mutex to clone an Arc, then
read-locks the inner. **Fix:** parking_lot for both; ideally
`DashMap<usize, Arc<RwLock<()>>>` so the outer mutex stops serialising.

## CRIT-2 — `chm_seg_lock_for` keyed by raw heap ptr; stale after GC
`native-collections/src/lib.rs:156-163`

Segments are `ObjectRef`s. After G1 / genHeap relocation the old ptr
becomes stale; a new segment can reuse the address and silently share
a resize lock with an unrelated CHM, serialising readers across two
maps. Old entries are never reclaimed. **Fix:** key by identity-hash
or embed the lock in a VM-side header field with GC-tied lifetime.

## CRIT-3 — `AtomicOperations::compare_and_set_int/long` operate on a stack-local atom
`vm/src/threading/varhandle.rs:348-373, 409-435`

`let atom = AtomicI32::new(current); atom.compare_exchange(...)` — the
CAS targets a fresh local atomic, never publishes. Only tests call them
today, but the helpers are `pub` and Java-named — a future caller will
be silently wrong. **Fix:** delete, or accept `&AtomicI32` from the
heap slot.

## HIGH-1 — `marking_complete` published with Relaxed; mixed-GC trigger races
`gc/src/g1.rs:907, 1628, 1822`

`store(true, Relaxed)` runs after concurrent-mark drains SATB and
updates RSets. `needs_mixed_gc()` Relaxed-loads it; no happens-before
edge to the preceding marking writes, so a mutator can trigger mixed
GC observing stale RSet state. The clear-store is symmetric. **Fix:**
Release store + Acquire load (free on x86-64).

## HIGH-2 — xnio_conduits saturates SeqCst on per-byte hot path
`native-builtins/src/xnio_conduits.rs:188-1320` (~30 sites)

`buffered_bytes` `fetch_add`, plus `read_ready` / `write_ready` /
`*_suspended` / `shutdown` / `eof` `store`/`load` all use SeqCst. None
need cross-variable total order. On x86 SeqCst stores emit `mfence`.
**Fix:** Release/Acquire for the flags; Relaxed `fetch_add` for the
counter (drained under another lock).

## HIGH-3 — `volatile_stripe_lock` no longer enforces JMM total order across volatiles
`gc/src/g1.rs:2225-2250`, `gc/src/collector.rs:50-64`

Round-8 dropped the bracketing `fence(SeqCst)`. Mutex pair gives
happens-before only on the **same stripe**. JLS §17.4.5 requires all
volatile actions to be **totally ordered** — two volatile writes on
different stripes now have no JMM-required global order (observable as
IRIW). **Fix:** single `fence(SeqCst)` inside the guarded region (one
fence, not two) — preserves total order at half the cost.

## HIGH-4 — `class_init_waiters` value still `std::sync::Mutex<bool>` + `Condvar`
`vm/src/vm/vm_init.rs:150-154, 502, 524`

Outer is parking_lot; inner mutex/condvar are std. Every `<clinit>`
wait does `pair.0.lock().unwrap()` + `cvar.wait_timeout(...).unwrap()`
— two `Result` unwraps and pthread/SRW overhead. **Fix:** parking_lot
Mutex+Condvar throughout. Same for `class_loading_locks`.

## MED-1 — `lockfree_resolve` doc rot persists in different shape
`vm/src/runtime/lockfree_resolve.rs:20-27`

Round-8 patched "four → three" but added a "fourth was folded into a
sibling struct" claim that never happened. Just say "three
`parking_lot::RwLock` maps below". The struct fields at 303-311 are
the source of truth.

## MED-2 — `lookup_define::class_data_store` uses std Mutex with poison passthrough
`native-builtins/src/lookup_define.rs:529-555`

`std::sync::Mutex<HashMap<u32, ObjectRef>>` + `unwrap_or_else(|p|
p.into_inner())`. Round-8 migrated peer side-tables; this one missed.
Hot during `MethodHandles.classData` / LambdaForm bootstrap. **Fix:**
parking_lot::Mutex<FxHashMap<...>>.

## MED-3 — `jca::cipher::CIPHER_TABLE` + `securerandom` use std RwLock with poison
`native-builtins/src/jca/cipher.rs:125-136`, `native-builtins/src/securerandom.rs:53`

`static RwLock<Option<FxHashMap<...>>>` with poison passthrough on
every Cipher op (init / update / doFinal). Crypto fast path. **Fix:**
`OnceLock<parking_lot::RwLock<FxHashMap>>` — drops the `Option` and
the poison dance.
