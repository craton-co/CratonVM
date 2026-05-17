# Round-8 Concurrency Audit

Workspace-wide, focus on round-7 changes. **CRIT**=race/UB,
**HIGH**=perf/correctness, **MED**=doc/cleanup.

---

## CRIT-1 — `synthetic_field_store` / `class_atomic_side_store` still single std::sync::Mutex+HashMap
`native-builtins/src/lib.rs:12152-58, 12219-28, 12168, 12175, 12188`

Round-7 sharded 4 sibling globals (`UnsafeShardedMap`, 32 parking_lot
shards) but missed these two. They sit on `Unsafe.compareAndSet` lazy-
init for `Class$Atomic.{reflectionData,annotationType,annotationData}`
— every `Class.getDeclaredMethods` traverses them. The comment block
literally says "all four globals" — it's now wrong. **Fix:** migrate
both to `UnsafeShardedMap`; drop the `unwrap_or_else(|e|
e.into_inner())` poison dance.

## CRIT-2 — `get_or_insert_borrowed` collision-counter re-probe not atomic with first probe
`jit/src/profile.rs:430-436`

The "miss vs collision" disambiguation re-acquires `name_index.read()`
to call `contains_key`. Latent: today the map is insert-only so the
two probes agree. If eviction is ever added the counter under-counts
silently and the diagnostic becomes meaningless. **Fix:** return a
3-state `enum { Hit, Collide, Miss }` from the first probe.

## HIGH-1 — `shard_for_current_thread` rebuilds SipHash on every SATB flush
`gc/src/satb.rs:172-182`

`flush` constructs a fresh `DefaultHasher` (SipHash-1-3, ~30 ns) and
re-hashes `ThreadId` on every drain. Defeats the 16-shard split under
high flush rates. **Fix:** `thread_local! { static SHARD: usize = … }`
initialised once per thread.

## HIGH-2 — `volatile_stripe_lock` `fence(SeqCst)` pair redundant under the held Mutex
`gc/src/g1.rs:2148-51, 2158-61`

The stripe `Mutex::lock()` already imposes Acquire on entry / Release
on drop — that IS the JMM happens-before edge. The fence pair was left
over from the pre-striping single-mutex design. Costs one `mfence` per
volatile field op on x86, `dmb sy` on ARM. **Fix:** delete the four
fence calls.

## HIGH-3 — `connect_pool` mpsc is unbounded — no backpressure under connect storms
`native-io/src/socket_channel.rs:163-186`

Worker count caps at 32 but `mpsc::channel` is unbounded. A burst of
100 k async-connects pins workers on 5 s DNS timeouts and grows the
queue to ~6 MB of `ConnectJob`s. Comment claims "bounded by available
parallelism" but only the *worker count* is. **Fix:**
`mpsc::sync_channel(workers * 8)`.

## HIGH-4 — `schedule_wakeup` spawns one OS thread per virtual-thread sleep
`vm/src/threading/virtual_threads.rs:798-812`

Every VT sleep allocates a Condvar + spawns an OS thread. 10 k VTs
each calling `Thread.sleep(1)` spawns 10 k OS threads — defeats the
lightweight-VT premise. **Fix:** single timer-wheel thread; each
scheduled wakeup ~80 B.

## HIGH-5 — `vh_meta_update_field_index` stale-Arc race window undocumented
`native-builtins/src/lang_invoke.rs:191-201`

Clone-mutate-reinsert means readers holding a pre-update
`Arc<VarHandleMeta>` keep observing the stale `field_index`. The
inline comment says this is intentional but never explains why callers
tolerate the stale value. If a CAS races with `update_field_index` it
may target the wrong offset. **Fix:** document the invariant
("`field_index` updates happen pre-publication during class init only")
or gate readers behind a generation counter.

## MED-1 — `SharedResolutionState` comment says "four RwLocks", code has three
`vm/src/runtime/lockfree_resolve.rs:20-25, 299-308`

Struct has 3 RwLocks (`global_methods`, `global_fields`,
`promoted_invokes`). Doc rot. **Fix:** s/four/three/.

## MED-2 — `tcp_next_id` SeqCst over-specified
`native-io/src/socket_channel.rs:127-129`

Monotonic id counter consumed under a downstream lock. Relaxed
suffices; saves an `mfence` per registration. **Fix:**
`Ordering::Relaxed`.

## MED-3 — Hot JCA/securerandom/jvmti side-tables still `std::sync::{Mutex,RwLock}`
`native-builtins/src/securerandom.rs:176`,
`native-builtins/src/jca/cipher.rs:78`,
`vm/src/runtime/jvmti.rs:2474`,
`vm/src/vm/vm_init.rs:491, 513, 633` (Condvar paths), plus ~10 cold
sites in `native-builtins/src/{aot,classloader_real,deprecated_*,…}`.
None need poisoning. **Fix:** sweep to `parking_lot`; round-7 wave-2
missed them.

---

## Audits B/C/D/E summary

- **std::sync locks remaining (production):** ~15 sites (above + cold
  wildfly admin).
- **SeqCst:** most correct (Java-volatile semantics in
  `vm/src/threading/varhandle.rs`). Misuses in HIGH-2, MED-2.
- **Relaxed:** all counters/diagnostics/advisory gates.
  `PROFILING_ENABLED` (`jit/src/profile.rs:49,55`) Relaxed is fine — a
  stale read costs one sample.
- **Channels:** only HIGH-3 is a real risk. JDWP `mpsc` is cold.
- **Thread spawns:** HIGH-4 is the only hot-path spawn.

## Lock-free angles

- **ProfileStore `name_index`:** `evmap`-style left-right map would
  drop the `RwLock` on the read path (single writer, many readers).
- **JFR SPSC:** hazard pointers on `consumer_busy` would let multiple
  drainers proceed instead of skipping — ~2× drain throughput.
- **Round-7 migrations:** ProfileStore two-phase snapshot,
  SpscEventRing ordering (x86 TSO and ARM weak), SatbQueue full-shard
  drain, parking_lot migrations in `dispatch_trace`/`VH_META_TABLE`/
  `lockfree_resolve` all clean. CRIT-1, HIGH-1, HIGH-2, HIGH-3 are the
  highest-value follow-ups.
