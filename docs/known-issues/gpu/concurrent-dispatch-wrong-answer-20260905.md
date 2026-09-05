# Concurrent GPU dispatch returns a wrong answer, intermittently

## Status

**Found 2026-09-05 on real hardware, not fixed, not previously recorded.**
Four Java threads dispatching through one `OffloadCache` intermittently
produce a wrong checksum under `--gpu`. The answer is wrong, not slow, and
it is a *different* wrong value nearly every time — a race, not a
miscompute.

Found by `bench-gpu/runtime-stress.sh`'s `concurrent` scenario, which is
part of `.github/workflows/gpu-selfhosted.yml` but is **not** part of
`bench-gpu/ci-gate.sh`. Nothing in `ci-gate.sh` reaches this shape, which
is why a full green gate battery sat beside it.

## Rate, and the controls

RTX 2060 (sm_75), CUDA 13.3, driver 610.88, Windows 11, JDK 25.0.3,
`cratonvm-cli --features gpu-driver` at `b82da0607`. Fixture
`test_classes/gpu/GpuRuntimeStress.java` scenario 1, `n=65536`, 4 threads,
8 rounds each. Reference is HotSpot on the same fixture:
`concurrent=-6512721874358955904`.

| arm | mismatches |
| --- | ---: |
| `--gpu` (default: ZGC, device pool on) | **2 / 20** |
| `--nojit`, no device | 0 / 20 |
| default JIT, no device | 0 / 20 |

Forty control runs clean across two host-side configurations, so this is
the offload path and not a host-side threading defect in the fixture or
the VM. The script's own doctrine — "the GPU disagrees with HotSpot is
also what a host-side defect looks like" — is satisfied before the device
arm is read.

The scenario is deterministic by construction: each thread writes only
`results[id]`, all threads are joined before the combine, and the combine
walks the array in index order. Thread scheduling cannot change the
answer.

## The amplifier: turning the device pool OFF makes it much worse

`CRATONVM_GPU_DEVICE_POOL=0`, same fixture, same reference:

| arm | mismatches |
| --- | ---: |
| `--gpu` pool ON | 2 / 20 (10%) |
| `--gpu` pool OFF | **16 / 30 (53%)** |

This is the opposite of what a pool-aliasing hypothesis predicts, and it
rules that hypothesis out: a pool handing the same device buffer to two
threads would get *better*, not worse, when the pool is disabled. Buffer
reuse was **masking** the race — with pooling off, every dispatch takes a
fresh `cuMemAlloc` and the threads interleave more.

Use `CRATONVM_GPU_DEVICE_POOL=0` when working on this. 53% is a workable
repro rate; 10% is not.

## It splits by collector

With the pool-off amplifier:

| collector | mismatches |
| --- | ---: |
| `-XX:+UseGenerationalGC` | 13 / 20 |
| `-XX:+UseZGC` (the default) | 7 / 20 |
| `-XX:+UseG1GC` | **0 / 40** |

Both collectors that relocate young objects on every cycle fail; G1 did
not reproduce in 40 runs. That points at the interaction between
relocation and the input-residency cache under concurrency — the cache is
keyed by raw heap address (`FxHashMap<ObjectRef, Entry>` behind a
`Mutex`), so every relocation has to re-key its entries, and the
concurrent case has four threads' arrays moving while dispatches are in
flight.

**G1's zero is "did not reproduce", not "immune."** A pass on one
collector can be a masked failure; G1 relocated fewer cached entries than
Generational in the single-threaded `residency-gc.sh` run (8 vs 15), so
it may simply be moving these arrays less often in this window.

## What is already ruled out

* **The `cuda-bridge` layer.** Both `#[ignore]`d driver integration tests
  pass on this box: `concurrent_dispatch_it` (`one_context_many_threads`,
  `many_contexts_many_threads`) and `stream_ordering_it` (all three). The
  race is above the bridge, in `vm/src/runtime/offload.rs`.
* **The device buffer pool**, per the amplifier above.
* **Single-threaded residency across relocation.** `residency-gc.sh`
  passes on all three collectors, with a non-vacuous `re-keyed > 0`
  census on each. So relocation re-keying is correct when one thread does
  it; this defect needs the concurrency too.

## Worth checking first

`OffloadCacheRegistry::get_or_create` hands one `Arc<OffloadCache>` to
every dispatching thread — one input-residency cache, one kernel map, one
dispatch memo, one submission table, one chunk-stream pool, shared by all
four. The residency cache's map is `Mutex`-guarded, so the question is
not whether the map is torn but whether the *window* between a lookup and
the launch that uses the looked-up device buffer is protected against a
concurrent relocation re-keying that entry. A lookup that returns a
device pointer, then a GC that moves the array and re-keys, then a launch
against the stale pointer, is the shape that fits every observation here:
intermittent, different-wrong-value, worse with more allocation, absent
under the one collector that moves these arrays least, and invisible
single-threaded.

## Repro

```bash
# needs a --features gpu-driver binary; the fixture is gitignored and
# runtime-stress.sh compiles it on demand
CV=target-gpu/release/cratonvm.exe
JDK=<jdk25>
TG=test_classes/gpu

REF=$("$JDK/bin/java" -cp "$TG" GpuRuntimeStress 1 65536 | grep '^concurrent=')
bad=0
for i in $(seq 1 30); do
  g=$(CRATONVM_GPU_DEVICE_POOL=0 "$CV" --java-home "$JDK" -cp "$TG" \
        --gpu GpuRuntimeStress 1 65536 2>/dev/null | grep '^concurrent=')
  [ "$g" != "$REF" ] && { bad=$((bad+1)); echo "run $i: $g"; }
done
echo "$bad / 30 mismatched"
```

Expect roughly half the runs to mismatch, each with a different value.
Add `-XX:+UseG1GC` and expect zero.
