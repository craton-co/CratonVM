# Concurrent GPU dispatch returns a wrong answer, intermittently

## Status

**Found 2026-09-05 on real hardware. Dominant cause found and fixed the
same day; a much rarer residual remains open — see the last section.**

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
by the fix, so there is no cross-binary confound.

| arm | before | after |
| --- | ---: | ---: |
| default (ZGC, pool off) | 12 / 30 | **0 / 30** |
| `-XX:+UseZGC`, pool off | 7 / 20 | **0 / 40** |
| `-XX:+UseGenerationalGC`, pool off | 13 / 20 | 4 / 205 |
| `-XX:+UseG1GC`, pool off | 0 / 40 | **0 / 40** |
| `--gpu` default config | 2 / 20 | — |

Controls, before the fix: `--nojit` 0/20 and JIT-without-device 0/20, so
the defect was always in the offload path rather than host-side threading.

Whole battery on the fixed binary: `ci-gate.sh` 5/5, `runtime-stress.sh`,
`marshal-stress.sh`, `residency-gc.sh` (all three collectors),
`jit-writer-stale.sh`, both `cuda-bridge` integration suites, and
`cargo test -p cratonvm-vm --features gpu-offload --lib -- offload`
(40 passed).

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

## Still open: a ~2% residual under Generational, with the JIT on

The fix does not take Generational to zero. On the fixed binary,
`-XX:+UseGenerationalGC` with the pool off:

| arm | mismatches |
| --- | ---: |
| JIT on | 4 / 205 (~2%) |
| `--nojit` | 0 / 100 |

Two things to keep in mind about that comparison. It is **suggestive, not
conclusive**: at a 2% rate, 100 clean `--nojit` runs would happen about
13% of the time by chance even if the JIT were irrelevant. And the
failing runs again show **no collections**, so whatever this is, it is
not relocation either.

The obvious next suspect is the other half of the eviction machinery, the
one `--nojit` removes: the compiled-store barrier's `DIRTY` byte array.
`drain_compiled_writes` reads and clears those bytes **outside** the
cache mutex and only then takes the lock, so a compiled store that sets
its bucket between the drain's `swap` and a concurrent `get_*` could have
its eviction missed. That is the same family of bug as the one fixed
here, one level down, and it has not been confirmed — nothing in that
path has been instrumented yet.

Reproducing it wants a much larger sample than anything above: at 2%,
telling 0 from 2 apart needs several hundred runs per arm.
