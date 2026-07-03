# `ConcurrencyThrottleInterceptorTests` TIMEOUT — stale JIT-scan cache served at the blocked-path root deposit (FIXED)

Status: **FIXED** on branch `fix/throttle-jit-concurrency-hang` (2026-07-02).
Family: **Family A — GC root coverage under JIT** (see `docs/known-issues/README.md`);
this closes the *parked-thread deposit* member. The register-invisibility residual
(A4) and the walker-containment layers are unchanged.

## Symptom

`org.springframework.aop.interceptor.ConcurrencyThrottleInterceptorTests`
(spring-aop, jit-real mode) TIMEOUTs 100% reproducibly: `multipleThreadsWithLimit`
(100 threads × 1000 proxied `getName()` calls through a
`ConcurrencyThrottleInterceptor`, plus 10 exception-throwing threads) wedges
permanently a few minutes in. Log fills with the known
"`kind=Object but array_length=512 (num_slots=5, class_id=1197)`" /
"non-moving sweep: stopping walk / RE-SYNCED" corruption family, then goes
silent; all ~100 threads park forever. `--stack-dump-on-timeout` shows:

- 69 threads in `ConditionObject.await` ← `ConcurrencyThrottleSupport.onLimitReached`,
- 2-3 threads blocked in `ReentrantLock.lock`/`unlock` (`beforeAccess`/`afterAccess`),
- no thread inside the target invocation, main in `Thread.join` — i.e. the
  throttle's `ReentrantLock`/condition state is permanently broken.

One-shot Java failures observed at the wedge point (each sufficient to leak the
lock or a throttle permit forever, wedging every other thread):

- `ERROR gen_heap::get_field: out-of-bounds field read dropped … num_slots=0
  class_id=1197 class_name=java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionNode
  real_field_count=Some(5)` — a **live ConditionNode read through a stale young
  address whose slot the sweep had zeroed**;
- under `CRATONVM_MOVING_YOUNG=1` the pure form:
  `NoSuchMethodError "java/lang/Object.isReleasable()Z"` at
  `ConditionObject.await @pc=110` (stale node reads as `class_id=0`) and
  `IllegalMonitorStateException` at `ReentrantLock.unlock` ←
  `beforeAccess` (count already incremented, unlock failed → lock held forever);
- `Stale pointer detected in invokevirtual receiver (all-zero header)` on a
  `ReflectiveMethodInvocation` → `AbstractMethodError` from `proceed()`.

## Root cause

`NativeContextImpl::deposit_root_snapshot` (`vm/src/vm/vm_exec.rs`) — the
GC-authoritative root publish a thread performs right before parking
(`LockSupport.park`/`Unsafe.park`/`Object.wait`/`Thread.join`) — called
`scan_active_jit_frames` **without first calling
`invalidate_scan_cache_for_gc()`**. The per-thread `JIT_SCAN_CACHE` is keyed by
`(boundary-generation, chain-len, collection-count)`, and none of those change
when:

- compiled code allocates via the **inline TLAB fast path** (no helper, no
  boundary bump), or
- only **void** natives run after the last object-returning native filled the
  cache (`update_root_snapshot` refills only on object-returning calls; `park`,
  `unpark`, `setCurrentBlocker` are void).

So a JIT-compiled `AQS$ConditionObject.await` frame could inline-allocate its
`ConditionNode`, spill it, park — and the deposit would publish the **cached,
pre-node** root set. The collector marks a parked thread ONLY from this
deposited snapshot, and the non-moving young sweep's **selective promotion pins
by root value**: the node (marked via the heap chain, aged by repeated parks,
but absent from every snapshot) was **evacuated to old gen and its young slot
zeroed** while the parked thread's JIT frame still held the young address. On
wake: zeroed-header reads (`num_slots=0`), broken condition-queue signalling,
stray writes through stale refs into reused slots (the `array_length=512`
header-clobber family), and finally one failed `unlock`/leaked permit that
wedges all 110 threads. The safepoint-arrival publish
(`interpreter.rs safepoint_check`) and the initiator's `collect_roots`
already invalidated the cache before their authoritative scans —
`invalidate_scan_cache_for_gc()`'s own doc names "a parked thread's pre-STW
publish" as a required call site; the deposit was the one publish that didn't.

Evidence that nailed it (all on a frozen dev binary, real suite classpath):

| Config | corruption warns | outcome |
|---|---|---|
| default JIT | 100s + wedge | TIMEOUT every run (4/4) |
| `--nojit` | 0 | **OK in 19.7 min** (pure interpreter slowness, no bug) |
| `--Xmx 8g` | still corrupts | TIMEOUT (young GC still runs) |
| `CRATONVM_MOVING_YOUNG=1` | 0 header-corruption, but stale-node NSME + IMSE | TIMEOUT (same staleness, moving form) |
| `CRATONVM_NO_SELECTIVE_PROMOTE=1` | **0** | no wedge (still slow — promotion is the mover) |
| `CRATONVM_NO_JIT_SCAN_CACHE=1` | **0** | no corruption (cache is the stale source) |
| **fix (deposit invalidate)** | ~99 contained warns | **OK, 3/3 tests pass, clean exit** |

## Fix

`vm/src/vm/vm_exec.rs` `deposit_root_snapshot`: call
`crate::jit::conservative_roots::invalidate_scan_cache_for_gc()` first, so the
blocked-path deposit always performs a fresh JIT-frame scan (one TLS increment
per park — negligible against the park syscall itself).

## Residual (tracked elsewhere, unchanged by this fix)

- The run still logs contained "inconsistent header / RE-SYNC" warnings: the
  register-invisibility / running-thread transient reclaim family (A4 /
  DoHead Layer 1) is still open; the 2026-07-02 walker hardening contains it
  (no fatal manifestation in this workload's verification runs).
- `--nojit` remains ~20 min for this class (~170× HotSpot's 6.8 s) — an
  interpreter-throughput limitation, not a hang: it passes and exits cleanly.
  The class stays TIMEOUT in nojit suite lanes with the default 180/600 s
  budgets.
- jit-real wall time is ~5 min (corruption-containment overhead + heavy lock
  churn); above the 180 s single-class re-run budget, so suite lanes may still
  record TIMEOUT for the *class* while the VM-level hang itself is gone.

## Verification

- jit-real: `KRun ConcurrencyThrottleInterceptorTests` → `found=3 succ=3
  fail=0 status=OK`, clean process exit (repeated runs; see branch log).
- nojit-real: same → `status=OK`, clean exit.
- spring-aop module spot-check (66 classes) via `apps/spring-suite-runner`
  against the fixed binary: no regressions vs baseline.
