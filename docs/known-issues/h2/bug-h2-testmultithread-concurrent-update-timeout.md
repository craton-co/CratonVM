# `TestMultiThread` is ~100x HotSpot, and H2's own timeouts trip somewhere different every run

## Status
**OPEN, re-diagnosed 2026-08-02.** The previous title of this page —
"`testConcurrentUpdate` times out" — named one run's symptom as if it were the
defect. It is not. On a quiet host the class **passes**; it is just slow enough
that H2's internal timeouts (a 10 s `LOCK_TIMEOUT`, a 5 min `job.get`) fall over
in a different place on each run.

Three runs of the same class on the same binary (`dev` @ `750a95f8e3`), same
host, same hour:

| run | wall | CPU (user) | outcome |
| --- | --- | --- | --- |
| 2026-08-02 00:03, load 15 | **727 s** | 1343 s | **PASS** (rc=0) |
| 2026-08-02 00:46, load 6, `DBG_REMAP_TRACE` on | 348 s | 808 s | FAIL — `LOCK_TIMEOUT` after 10 000 ms in `testConcurrentUpdate2` (`TestMultiThread.java:414`) |
| 2026-08-01, load 70-85 | 784 s | 392 s | FAIL — `TimeoutException` from `job.get(5, MINUTES)` in `testConcurrentUpdate` |

HotSpot jdk-25 on the same host, same minute as the first row: **real 7.26 s,
user 15.18 s, rc=0**.

**≈100x wall, ≈88x CPU.** That is the bug. Everything else on this page is a
consequence of it.

## Severity
**MEDIUM** for the test class; the throughput number underneath it is the
interesting part.

## The UPDATE path has a real contention component — the INSERT path did not

`docs/internal/repros/h2-insert-scale-20260731/H2UpdateScaleProbe.java` models
`testConcurrentUpdate` exactly (same `NUMBER(18,0)` PK schema, same 10 000-row
`MERGE` seed, same `UPDATE account SET balance=? WHERE id=?` + `commit` inner
loop, same `LOCK_TIMEOUT=10000`) with thread and update counts as parameters.

CPU time (`/usr/bin/time`, user+sys), with a 0-update run of the identical shape
subtracted so VM startup and H2 class loading do not contaminate it; arms
round-robin, reps 2-3 taken at load 7-14:

| shape | HotSpot CPU-ms/update | cratonvm CPU-ms/update | ratio |
| --- | --- | --- | --- |
| 4 threads × 200 | 0.41 | 3.5 – 3.8 | ~9x |
| 25 threads × 200 | 0.15 – 0.17 | 7.2 – 7.8 | ~45x |
| 25 threads × 1000 | 0.122 | 7.84 | **64x** |

Read the direction, not just the ratio: **cratonvm's CPU per update doubles
from 4 to 25 threads (3.6 → 7.8) while HotSpot's falls (0.41 → 0.15).** CPU
time cannot be inflated by descheduling on a shared box, so that is genuine
contention, not host oversubscription.

That is the one thing the insert investigation did *not* find. Its finding —
flat CPU/row across 1/2/4/8 threads, a roughly constant ~25-30x — still holds
for INSERT and does **not** transfer here. The two workloads differ in exactly
the way you would expect to matter: UPDATE takes row locks and writes MVCC
versions; INSERT of a fresh PK does not.

Setup cost, single-threaded (10 000 `MERGE` + VM start + H2 class load):
HotSpot ~2.2 CPU-s, cratonvm ~23-24 CPU-s — a flat ~20 s tax on every H2 run.

## Where the CPU goes

`perf record -F 199 -g`, 25 threads × 1000 updates, 27 K samples, aggregated by
symbol (the per-thread default view splits every symbol 25 ways and hides all
of this):

| cluster | share | symbols |
| --- | --- | --- |
| Rust-side allocation | 5.9% | `_mi_page_malloc_zero` |
| **dispatch + JIT precedence** | **~12%** | `invoke_on_class_shared_inner` 2.42, `execute_invokevirtual_cached` 1.83, `InvokeCache::get` 1.45, `try_jit_compile_callee` 1.38, `jit_invoke_virtual_mic` 1.18, `force_native_over_real_jdk_bytecode` 1.06, `virtual_dispatch_target_cached` 1.02, `find_method_recursive` 1.01, `jit_method_calls_native_shadowed` 0.95 |
| **GC conservative root scan** | **6.6%** | `native_stack_has_jit_frame` 2.76, `scan_one_frame` 2.01, `is_object_address` 1.87 |
| **native-method registry lookup** | **6.0%** | `slot_for_exact` 2.12, `__memcmp_evex_movbe` 2.79, `hashbrown …search` 1.13 |
| **ClassManager lock** | **5.2%** | `RawRwLock::lock_shared_slow` **1.47**, `OrderedPlRwLock::read` 1.41, `load_class_concurrent` 1.40, `drop_in_place<…ClassManager guard>` 0.88 |
| interpreter | 3.0% | `execute_frame_from_index` |

Two of those are the ones that should scale with thread count and are the
candidates for the 4→25 doubling:

* **`lock_shared_slow` is the CONTENDED acquire path of a `parking_lot` rwlock**
  — an uncontended read never reaches it. A second profile with
  `--call-graph=dwarf` names the caller:

  ```
   1.54%  cratonvm_types::lock_order::OrderedPlRwLock<T>::read
          |
           --0.78%--cratonvm_vm::vm::vm_exec::invoke_on_class_shared_inner (inlined)
  ```

  so this is not only class loading warming up — **the invoke slow path itself
  takes a process-wide shared read on the `ClassManager` lock per call**, and
  with 25 threads that read starts hitting `lock_shared_slow`. The other
  callers in the same band are `resolve_field_ref_loader_aware` (1.51%) and
  `load_class_concurrent` (1.44%, still 1.4% in *steady state* hundreds of
  seconds after warm-up, which is its own question).
  This is the most concrete scaling target the profile produces: a per-call
  global rwlock read is exactly the shape whose cost per operation grows with
  thread count, which is what the CPU-time table above measures.
* **the conservative root scan is per-thread-stack work done per collection**,
  so its cost grows with (threads × collections).

The native-registry cluster is the same one the insert profile named (~5.6%
there); it is a constant factor, not a scaling one.

## The `CloneNotSupportedException` residual DOES reproduce — in the real class

The retired insert page recorded this residual as *"not reproduced, 0 of 8"*.
That was measured on a standalone 25-thread INSERT probe. Running the real class
on `dev` @ `5a18a9db1c`, ten runs:

```
ExecutionException: org.h2.jdbc.JdbcSQLNonTransientException:
  General error: "java.lang.CloneNotSupportedException"; SQL statement: COMMIT
    at org/h2/test/db/TestMultiThread.testConcurrentUpdate(TestMultiThread.java:382)
    at java/util/concurrent/FutureTask.get(FutureTask.java:207)
```

| outcome | runs | time |
| --- | --- | --- |
| pass | 8 | ~360 s each |
| **`CloneNotSupportedException` on `COMMIT`** | **2** | **77 s and 112 s** |

**2 in 10, both in `testConcurrentUpdate`, both on `COMMIT`, both inside two
minutes** — a method and a workload the INSERT probe never exercised. The probe
negative was a true negative *about the probe*, and was recorded as such;
quoting it as "the residual does not reproduce" would have been wrong. Same
lesson as the rest of this file: run the real class.

This is now the best handle on this family anyone has had: a **correctness**
failure at 20 % in under two minutes, in a named method on a named statement —
against the ~1-in-16 dispatch-miss face and the 1-event-per-3-worker-hours
old-gen face. **Start here.**

Worth noting what a `CloneNotSupportedException` IS on this VM: `Object.clone()`
throws it when the receiver's class is not `Cloneable`, and a receiver whose
header has been zeroed resolves to `java.lang.Object`, which is not `Cloneable`.
So this may be a third face of
`bug-h2-classid0-stale-address-family.md` rather than a clone bug —
exactly the shape of the already-fixed
`../../internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testtemptables-clonenotsupportedexception-thread-clone-frame-FIXED.md`.
Not established; `reclaim_guard` is **not** wired into the clone path, so the
occurrence above produced no verdict. Wiring it there is the cheap next step.

## What is already ruled out (do not redo)

* **There is no `org/h2/` JIT package ban to lift.** Measured 2026-08-02 with
  `CRATONVM_DBG_JIT_COMPILED=1`: **27** `org/h2/…` methods JIT-compile on the
  default build, **26** with `CRATONVM_JIT_ALLOW_PACKAGES=org/h2/`. The flag is
  a no-op for this workload. The retired insert page's "lifting the ban made it
  ~9% worse" was therefore a **null A/B** — two identical configurations — and
  is withdrawn.
* **Not heap pressure** (`--Xmx` 1g/2g/4g/8g: no trend), **not the young-GC
  livelock**, **not the STW cross-thread takeover**
  (`CRATONVM_XT_PEER_DEADLINE_MS` 1/20/200: no effect), **not the JIT-root path**
  (`--nojit` scales identically) — all measured on the insert half.
* **`jit_activation`'s global `Mutex` is gone** (per-thread tables since
  2026-07-31).
* **`Math.random()` is not a contention point** — it is a thread-local `Cell`
  seed (`native-builtins/src/lang_math.rs`), not a shared `Random`. Worth
  recording because `testConcurrentUpdate` calls it twice per update and a
  shared LCG would have been the obvious suspect.

## Measurement discipline this host requires

1. **Never quote a debug-build ratio.** ~5-10x slower than release on its own.
2. **Never quote a multi-threaded wall-clock number.** 16 cores shared with
   15-40 sessions. The three rows in the table above are the same class on the
   same binary at 348 / 727 / 784 s. Use CPU time, round-robin the arms,
   min-of-N, record `uptime` beside every number.
3. **Aggregate `perf report` by symbol** (`--sort symbol`). The default groups
   by command, which on a 25-thread run divides every symbol by 25 and puts
   nothing above 2.3%. And use `--call-graph=dwarf` — the `fp` graphs on this
   binary resolve almost nothing above the leaf, which is what left the first
   pass unable to name who takes the contended lock.
4. **An empty stdout is not a pass.** H2's `TestBase` reports some failures on
   stderr and the VM exits 1; check the exit code, not the output.

## Reproducing

```bash
cd <fresh writable dir>          # H2 writes ./data
<cratonvm> --java-home /home/victor/jdk25 --Xmx 1g \
  -c "<h2>/target/classes:<h2>/target/test-classes:$(cat <h2>/craton-testcp.txt)" \
  org.h2.test.db.TestMultiThread
```

For the UPDATE path alone, without the 12-minute class:

```bash
javac -cp <h2>/target/classes -d probe H2UpdateScaleProbe.java
<cratonvm> --java-home <jdk25> --Xmx 1g -c "<h2>/target/classes:probe" \
  -Dprobe.dir=./h2updb H2UpdateScaleProbe <threads> <updates> 10000
```

## Related

* `bug-h2-classid0-stale-address-family.md` — a silent memory-safety
  defect found in this class. Split out; it is not the cause of the slowness.
* `../../internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testmultithread-concurrent-insert-throughput-RESOLVED-20260801.md`
  — the insert half, with the flat-scaling measurement and the 1-thread profile.
