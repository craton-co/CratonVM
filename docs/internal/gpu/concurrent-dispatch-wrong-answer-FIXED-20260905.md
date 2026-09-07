# Concurrent GPU dispatch returns a wrong answer, intermittently

## Status

**Found and FIXED 2026-09-05 on real hardware.** Two independent races,
both in `input_cache`, both of the same family: state guarding a
mutex-protected structure was mutated outside that mutex. The second was
only visible once the first was fixed.

Four Java threads dispatching through one `OffloadCache` intermittently
produced a wrong checksum under `--gpu`: a *different* wrong value nearly
every time, which is a race rather than a miscompute.

Found by `bench-gpu/runtime-stress.sh`'s `concurrent` scenario, which the
weekly `gpu-selfhosted.yml` job runs but `bench-gpu/ci-gate.sh` does not.
All five gates in `ci-gate.sh` were green in the same session.

## The cause: a lost filter bit between two writers of `ADDR_FILTER`

`input_cache` keeps a 64-bit membership filter, `ADDR_FILTER`, so that
`invalidate` — which runs on **every primitive array store in the VM** —
can return after one relaxed load for the overwhelming majority of arrays
that were never marshalled to the device. Its soundness rests on one
invariant:

> every array in the cache has its bit set in the filter.

False positives are fine (a wasted lock and a failed lookup). A false
*negative* is a wrong answer, because `invalidate` skips an array that
really is cached, and the host's write then never evicts the device
mirror.

`insert` OR'd its bit **outside** the cache mutex and then took the lock
to add the map entry. But `rebuild_filter` *stores* a mask recomputed
from the whole table, under the lock. So:

```
thread A: insert(X)              thread B: invalidate(Y)
  ADDR_FILTER |= bit(X)
                                   lock
                                   table.remove(Y)
                                   rebuild_filter()   <- X is not in the
                                     table yet, so this STORES a mask
                                     with bit(X) CLEARED
                                   unlock
  lock
  table.insert(X, entry)           <- X is now cached with its
  unlock                              filter bit clear
```

From that point every `invalidate(X)` takes the fast path and returns.
`GpuRuntimeStress.concurrent` mutates its input between rounds
(`in[i] += 1`), so the very next `scale(in, out)` computes from a stale
device copy.

`insert` was the **only** one of the six writers of `ADDR_FILTER` that
did not hold the mutex; `invalidate`, `clear_all`, `drain_compiled_writes`,
`disable_for_jit_array_writer` and `remap_and_sweep` all already rebuilt
under it.

### The fix

Take the lock first, so the OR and the map insert cannot be interleaved
by a concurrent `rebuild_filter`. Three functional lines in
`vm/src/runtime/offload.rs`. The *order* within the lock is unchanged and
still matters — bit first, then the entry — because `invalidate` reads
the filter without the lock and must never see an entry whose bit is
still clear.

## What this was NOT, and how that was established

The first version of this page guessed at "relocation re-keying under
concurrency". **That was wrong**, and cheap measurements refuted it
before any code was read:

* **Not GC.** `gpu_residency_census::exit_summary` prints only when
  `collections != 0`, and it printed nothing on any collector, on
  failing runs included. **Zero collections occurred.** An 8 GB heap did
  not change the rate either (11/20 vs 9/20).
* **Not device-side ordering.** `CRATONVM_GPU_DISPATCH_STREAMS=1` puts
  every dispatch on one stream in strict order — still 9/20. Chunking off
  (`CRATONVM_GPU_CHUNKS=1`): 10/20.
* **Not the dispatch memo, the transfer path, or the wait latch.**
  `DISPATCH_MEMO=0` 11/20, `NO_ZEROCOPY=1` 8/20, `WAIT_LATCH=0` 9/20.
* **Not the device buffer pool.** `CRATONVM_GPU_DEVICE_POOL=0` made it
  *worse* — 16/30 against 2/20 — which is the opposite of what pool
  aliasing predicts. Buffer reuse serialises dispatches and was masking
  the race.
* **Not the `cuda-bridge` layer.** Both `#[ignore]`d driver integration
  tests pass on this box: `concurrent_dispatch_it` (2) and
  `stream_ordering_it` (3).

The collector split (below) is a consequence of the allocator laying
arrays out at different addresses, not of relocation: with 64 filter bits
and `(addr >> 3) & 63`, which arrays collide — and therefore how often a
`rebuild_filter` races an `insert` — is address-dependent.

## Measured

RTX 2060 (sm_75), CUDA 13.3, driver 610.88, Windows 11, JDK 25.0.3.
Fixture `test_classes/gpu/GpuRuntimeStress.java` scenario 1, `n=65536`,
4 threads, 8 rounds. HotSpot reference `concurrent=-6512721874358955904`.
The scenario is deterministic by construction: each thread writes only
`results[id]`, all threads are joined before the combine, and the combine
walks the array in index order.

Before and after are two builds from the same worktree differing **only**
by the fix, so there is no cross-binary confound. The `after` column here
is after the FIRST fix only — the Generational row is what the second race
below accounts for, and the final numbers are under "Verification".

| arm | before | after 1st fix |
| --- | ---: | ---: |
| default (ZGC, pool off) | 12 / 30 | **0 / 30** |
| `-XX:+UseZGC`, pool off | 7 / 20 | **0 / 40** |
| `-XX:+UseGenerationalGC`, pool off | 13 / 20 | 4 / 205 (see below) |
| `-XX:+UseG1GC`, pool off | 0 / 40 | **0 / 40** |
| `--gpu` default config | 2 / 20 | — |

Controls, before the fix: `--nojit` 0/20 and JIT-without-device 0/20, so
the defect was always in the offload path rather than host-side threading.

## The gate that let it through, and what changed

`runtime-stress.sh` ran each scenario **once**. Against a 10% race that
is a gate which passes 9 times in 10 — this defect was caught only
because the first run of the day happened to be unlucky. Demonstrated
directly: run the *old* binary against the *new* script and the
single-shot `concurrent` check PASSES while the repeat arm fails.

The script now re-runs the concurrent scenario `REPEATS` times (default
5) under `CRATONVM_GPU_DEVICE_POOL=0`. The flag is not a workaround — it
is the sensitive configuration, taking the rate from 10% to 53%, and a
gate should use the sensitive one. At 53% per run, five repeats miss a
regression of that size about 2% of the time; one run missed it 47% of
the time.

## The second race: the compiled-store drain cleared its flags too early

Fixing the filter bit left a residual — ~2% under `-XX:+UseGenerationalGC`
with the JIT on, and 0/100 under `--nojit`. That comparison was suggestive
and nothing more (at 2%, 100 clean runs happen 13% of the time by chance),
so it was not treated as a diagnosis.

The mechanism was confirmed with the tree's own kill switch,
`CRATONVM_JIT_GPU_ARRAY_BARRIER=0`, which un-arms the compiled-store
barrier and restores `offload_jit_gate`'s refuse-to-compile behaviour.
**Alternating arms, one binary, 300 runs each**, Generational, pool off:

| arm | before | after |
| --- | ---: | ---: |
| barrier armed (default) | **12 / 300** | **0 / 300** |
| `CRATONVM_JIT_GPU_ARRAY_BARRIER=0` | 0 / 300 | 0 / 300 |

Eight distinct wrong values among the twelve. A 0/300 result if the true
rate were 4% has probability ~5e-6, so the arms are genuinely different.
The barrier is not dormant in this scenario either — the census reports 6
array writers admitted behind it, 27 drains and 28 buckets evicted.

### What was wrong

`drain_compiled_writes` cleared the `DIRTY` bytes with `swap(0)` and only
*then* took the cache mutex to perform the eviction those flags
authorised:

```
thread A: drain                thread B: get_i32(X)
  swap DIRTY[b] -> 0
                                 drain: DIRTY all clear, returns early
                                 lock; reads X   <- STALE: the eviction
                                   A owes has not happened yet
  lock; evict bucket b
```

The getters compounded it a second way: each did
`drain_compiled_writes(); let g = map().lock();` — two separate
acquisitions, so even a correctly ordered drain released the lock before
the lookup reacquired it, leaving a gap a compiled store could land in.

### The fix

`drain_locked(&mut tables)` does the read-clear **and** the eviction with
the mutex already held, and the seven getters now take one lock across
drain-then-lookup. `drain_compiled_writes` keeps its unlocked
`DIRTY.iter().any(...)` pre-filter, which stays sound: it can only produce
a false *positive* (a wasted lock), never a false negative.
`remap_and_sweep` drains under its own lock too, still before the re-key.

`vm/src/memory/addr_keyed.rs`'s census caught the new `&mut` parameter as
a fourth `ObjectRef`-table declaration in the file and had to be
re-audited — correctly, and worth noting as a guard that works: it is
still one table, borrowed, so the existing remap+sweep disposition
covers it.

## Verification

Post-fix, `GpuRuntimeStress` scenario 1, `n=65536`, pool off, 100 runs per
collector: **0/100 on Generational, ZGC and G1**. Plus the 300+300
alternating A/B above.

Whole battery on the final binary: `ci-gate.sh` 5/5, `runtime-stress.sh`
(including the new repeat arm), `marshal-stress.sh`, `residency-gc.sh`
(all three collectors), `jit-writer-stale.sh`, both `cuda-bridge` driver
suites, and `cargo test -p cratonvm-vm --features gpu-offload --lib`
(2699 passed).

## The shape worth remembering

Both bugs are the same mistake in two places: a cheap lock-free
side-channel guarding an expensive locked structure, updated outside that
structure's lock.

* `ADDR_FILTER` — a bit OR'd in before the lock, erased by a concurrent
  rebuild-and-store.
* `DIRTY` — flags cleared before the lock, so the eviction they
  authorised had not happened when the next reader looked.

Neither is visible single-threaded, neither needs a GC, and neither
changes an answer that any single-threaded test checks. When a fast path
exists to let callers skip a lock, the state that fast path reads is part
of the locked invariant and has to be maintained under the same lock.

---

# Residuals, closed 2026-09-06

Both races on this page were fixed and verified on 2026-09-05. What was
left open was the GATE — this page's own section "The gate that let it
through" ends by describing a change to `runtime-stress.sh` without
saying whether anything actually runs it.

## The gate gap is covered, and it is covered by the job this page named

The page opens with

> Found by `bench-gpu/runtime-stress.sh`'s `concurrent` scenario, which
> the weekly `gpu-selfhosted.yml` job runs but `bench-gpu/ci-gate.sh`
> does not.

which reads as a coverage hole. It is not one. Read on its own,
`ci-gate.sh` does not cover concurrent dispatch — but `ci-gate.sh` is one
step of `gpu-selfhosted.yml`, and the SAME job runs `runtime-stress.sh`
four steps later, with no `continue-on-error` on either. A failure in the
repeat arm fails the job exactly as a failed gate would. Moving the
scenario into `ci-gate.sh` would buy nothing and would cost the gate
script its "no scenario here takes minutes" property.

What the weekly job runs, in order, all failure-propagating:
`ci-gate.sh`, `runtime-stress.sh` (including the `REPEATS=5` concurrent
arm under `CRATONVM_GPU_DEVICE_POOL=0`), `marshal-stress.sh`,
`residency-gc.sh`, `jit-writer-stale.sh`, and both `cuda-bridge` driver
suites.

## Re-verified on 2026-09-06

Same hardware — RTX 2060 (sm_75), CUDA 13.3, Windows 11, JDK 25.0.3 —
on a binary carrying an unrelated change to `offload_jit_gate` (see
[the compiled-caller gate page](compiled-caller-gate-refused-ldc-kernels-FIXED-20260905.md)),
which is worth stating because that change arms the compiled dispatch
helper on every `--gpu` run rather than only once a caller scan has found
a kernel. If anything were going to disturb the concurrent path, a change
that makes more sites consult the hook would be it.

```
=== vm offload runtime stress (n=65536) ===
PASS concurrent
PASS cache_coherence
PASS readonly_inputs
PASS aliasing
PASS repeat_submit
PASS deopt_then_continue
PASS bulk_writes
PASS concurrent x5 (pool off, the sensitive configuration)
ALL RUNTIME STRESS SCENARIOS PASSED
```

`marshal-stress.sh` passed with all six kernels engaged, `ci-gate.sh` is
6/6 (it gained a gate), `jit-writer-stale.sh` is 5/5, and
`cargo test -p cratonvm-vm --features gpu-offload --lib` is 2717 passed.

`residency-gc.sh` was RED, and not for anything on this page: a stale native
argument snapshot across a Java re-entry, fixed the same day — see
[native-arg-snapshot-stale-across-java-reentry-FIXED-20260906.md](../fixed-bugs/native-arg-snapshot-stale-across-java-reentry-FIXED-20260906.md).
It needed neither the GPU nor this page's subsystem: plain Generational with
the JIT on reproduces it with no `--gpu` anywhere.

## The shape, restated because it earned it

Both bugs were one mistake in two places: a cheap lock-free side-channel
guarding an expensive locked structure, updated outside that structure's
lock. `ADDR_FILTER` had a bit OR'd in before the lock and erased by a
concurrent rebuild-and-store; `DIRTY` had flags cleared before the lock,
so the eviction they authorised had not happened when the next reader
looked.

Worth noting alongside: the compiled-caller gate page's 2026-09-06
residual is a third variation on the same theme in the same subsystem —
a cheap registry consulted to decide a per-site memo, where the registry
could not yet hold the answer and the memo was taken anyway. Not a race
that time, but the same trade of a cheap side-channel for the expensive
truth, and the same failure mode: the fast path answered before the slow
path could have.
