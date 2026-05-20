# Round 7 — Concurrency & Lock Primitive Audit

Workspace audit of Mutex/RwLock/atomic/channel usage.

---

## CRIT-1 — `set_field_volatile` is not atomic (torn JMM `volatile long/double`)
**Files:** `gc/src/gen_heap.rs:735-751`, `gc/src/heap.rs:486-500`, `gc/src/g1.rs:2088-2099`.
Two `SeqCst` fences around a plain multi-word `Value` write enforce ordering, not atomicity. Comment at `gen_heap.rs:730-734` admits torn reads; JLS §17.7 mandates atomicity for `volatile long`/`double`. Concurrent writers can stitch one variant-tag with another's payload → UB.
**Fix:** Seq-lock or `AtomicU128` for `J`/`D` slots; `AtomicPtr` + `Acquire/Release` for refs (drop both fences).

## CRIT-2 — `ProfileStore::get_or_insert_borrowed` inverted lock order
**File:** `jit/src/profile.rs:362-389`.
Read path: `name_index.read()` → drop → `methods.write()` → `name_index.write()` (while still holding `methods.write()`, line 384). The forward path takes them L→R, the slow path R→L → classic AB/BA deadlock under contention. Neither lock is in `docs/lock-order.md`.
**Fix:** Enforce `methods > name_index`; drop `methods.write()` before taking `name_index.write()`. Add both to lock-order table.

## CRIT-3 — `ProfileStore::snapshot_all` holds outer read while locking per-entry mutex
**File:** `jit/src/profile.rs:502-517`.
`methods.read()` held across `slot.lock()` on every entry. Any code path that holds `slot.lock()` then promotes/inserts (needing `methods.write()`) deadlocks against this.
**Fix:** Clone Arcs under the read guard, drop, then lock each: `let arcs: Vec<_> = read.values().cloned().collect(); drop(read);`.

---

## HIGH-1 — `SharedResolutionState` uses `std::sync::RwLock` on dispatch hot path
**File:** `vm/src/runtime/lockfree_resolve.rs:19,288-296,322-396`.
Std RwLock = pthread_rwlock + poisoning overhead. Every other workspace lock is `parking_lot`.
**Fix:** Switch all four `RwLock`s to `parking_lot::RwLock`; drop `.unwrap()`.

## HIGH-2 — JIT cache write held across `flight_recorder.lock()`
**File:** `vm/src/runtime/interpreter.rs:2059-2090`.
`shared.jit_cache.write()` (2059) is alive when the JFR event is emitted at 2072 — global JIT writer serialises JFR formatting for every other compiler thread.
**Fix:** `drop(jit_cache);` immediately after `put()`, before the JFR block.

## HIGH-3 — `SatbQueue` uses one global `Mutex<Vec<usize>>` for all mutators
**File:** `gc/src/satb.rs:150-207`.
Every mutator's 256-entry buffer flush serialises on one Mutex. Concurrent marking ≠ concurrent SATB enqueue.
**Fix:** Replace with `crossbeam::queue::SegQueue<Vec<usize>>` (lock-free MPSC; marker is the single consumer, mutators push whole batches).

## HIGH-4 — `VH_META_TABLE` still `std::sync::Mutex` despite round-4/5 directives
**File:** `native-builtins/src/lang_invoke.rs:154-184`.
`vh_meta_get` fires on every VarHandle `get`/`set`/`compareAndSet` and serialises all VarHandle traffic. Both `docs/round4-native-builtins.md:48` and `docs/round5-native-builtins.md:23` flagged this — not actioned.
**Fix:** `parking_lot::RwLock<FxHashMap<usize, Arc<VarHandleMeta>>>`.

## HIGH-5 — `socket_channel::connect` spawns one OS thread per call
**File:** `native-io/src/socket_channel.rs:657-671`.
Fallback path does `std::thread::spawn` per connect. Microservice connect-storm path. `native-io/src/async_socket.rs:235-249` shows the worker-pool pattern.
**Fix:** `OnceLock<crossbeam::Sender<ConnectJob>>` connect-pool sized to `available_parallelism().min(32)`.

## HIGH-6 — `dispatch_trace.rs` claims "Lock-free" but uses `std::sync::Mutex<Vec<Slot>>`
**File:** `vm/src/dispatch_trace.rs:10,28-32,63,107,131`.
Comment line 1 lies — `CRATONVM_DBG_LETSGO=1` serialises *all* interpreter dispatch through one Mutex.
**Fix:** `[AtomicU64; SLOTS]` epoch + per-slot `UnsafeCell<Slot>` written by the thread that wins `SEQ.fetch_add(1) & (SLOTS-1)`.

---

## MED-1 — `TieredCompilationManager` double-locks per invocation
**File:** `jit/src/tiered.rs:298-355`.
Hot path takes `methods.lock()` (338) + `policy.lock()` (349) on every profiled invocation; `policy` is read-only after VM init.
**Fix:** `ArcSwap<CompilationPolicy>` so the policy read is lock-free.

## MED-2 — `MarkQueue::push_batch` re-locks per pointer
**File:** `gc/src/concurrent_mark.rs:185-189`.
Calls `push()` per element → fresh shard lock per ptr. Typical 256-entry batch ≈ 256 lock acquires when ~8 (one per shard) suffice.
**Fix:** Bucket batch by `shard_for`, then for each non-empty shard acquire once and `extend()`.

## MED-3 — `gpu_pinned_refs` `Mutex<HashSet>` on GC root-scan path
**File:** `gc/src/heap.rs:109,162`.
JNI pin/unpin and GC root scan serialise on one Mutex.
**Fix:** `dashmap::DashSet<ObjectRef>` — sharded lock-free set.

---

## Process notes

- **lock-order.md gaps:** `jit_cache`, `vtable_manager`, `name_index`+`methods` (ProfileStore), `osr_trampoline_cache`, `tiered.rs` triple lock — none documented.
- **Channels:** `std::sync::mpsc` still in `vm/src/debug/*` and `native-io/src/lib.rs:12209-12266` — prefer `crossbeam_channel`. JFR `SpscEventRing` correctly lock-free post wave-1.
- **Atomics:** No naked `.load/.store` without ordering. `SeqCst` elsewhere is conservative-correct; CRIT-1 is wrong about atomicity, not ordering.
- **Thread pools:** no global pool; each subsystem spawns its own. Consider `rayon` for parallel-mark.
