# `QuartzEndpointWebIntegrationTests` issues ~24,000 HTTP requests where `--nojit` issues ~22 — a JIT-only spin loop, OOM-killed as a side effect

**Status: OPEN, reproduced 2026-08-18 on `dev` `24a5d4528` (Azure Linux). Not
fixed. The root cause is NOT isolated, but it is now correctly framed: this is a
poll loop that never observes its condition under the JIT, not a memory leak.
Four hypotheses tested and refuted, three JIT kill switches tried and cleared —
all recorded below so nobody re-runs them.**

Found re-measuring the four classes on
retired/moving-young-fallback-four-springboot-classes-RETIRED-20260818.md. That
page records this class as green under the shipped default. It is not, on Linux,
on current dev — and the failure is not that page's mechanism: there are **zero**
`[moving-young]` fallback lines in the run.

## What it actually is

The class runs its request against the Quartz actuator endpoint over and over,
forever. Counted by instrumenting the throwable/frame capture
(`CRATONVM_DBG_STTRACE=1`) and tallying `QuartzEndpoint.triggerQuartzJob`
frames:

| arm | `triggerQuartzJob` frames | window | outcome |
|---|---:|---|---|
| `--nojit` — **the whole passing run** | **66** | ~110 s | ✓45/45 |
| JIT on | **47,362** | 22 s | OOM-killed |

23,681 separate captured traces carry exactly **three** `triggerQuartzJob`
frames (`QuartzEndpointWebExtension:97` → `QuartzEndpoint:227` →
`QuartzEndpoint:231`) and none carry more, so this is **not** runaway recursion —
it is ~24,000 *separate, complete HTTP requests* through a three-deep call nest
that should run about 45 times.

The logs are quiet: 4 `IllegalStateException`s in the whole run and no repeated
exception. The requests are not failing and being retried by an error handler;
something is polling for a condition it never sees. `--nojit` sees it.

**The 22 GB is a symptom, not the bug.** Each iteration captures a stack trace
(up to 240 frames), so RSS climbs ~350 MB/s until the kernel OOM-killer takes
the process — 21.9 GB unconstrained, ~24 s under an 8 GB cgroup cap. That is why
`--Xmx` has no effect (the Java heap reads ~1 MB throughout) and why all three
collectors behave identically. Stop the loop and the memory goes with it.

**Run it under a cap.** This is a shared box and an unconstrained arm will OOM
other people's work:
`systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=0 …`

## Measured

One class per process, `--Xmx 2g`, real JDK 25 backend, the three load-bearing
suite env vars, RSS sampled every 2 s.

| arm | peak RSS | outcome |
|---|---:|---|
| HotSpot 25 | **385 MB** | ✓45/45 in 8 s |
| CratonVM, ZGC (shipped default) | 7,867 MB | OOM-killed @ ~24 s |
| CratonVM, `--XX:UseGc G1` | 7,491 MB | OOM-killed @ ~24 s |
| CratonVM, `-XX:+UseGenerationalGC` | 7,998 MB | OOM-killed @ ~24 s |
| CratonVM, `--nojit` | 2,741 MB | **✓45/45** in ~110 s |

Collector-independent, so not a GC bug. `--nojit` clears it, so it is the JIT —
which is the same thesis the retired page reached for these classes by a
different route.

## The shape to look for

A spin/poll loop whose condition is written by one thread and read by another:
the request runs on `http-nio-auto-N`, the assertion waits on `main`. The
classic compiled-code failure for that shape is a **non-volatile field read
hoisted out of the loop** (or otherwise cached in a register), so the waiter
never observes the writer's store. That is consistent with every observation
here — JIT-only, collector-independent, no exception, unbounded iterations —
but it is **not confirmed**, and the loop that spins has not been identified in
the Java source yet.

The first thing the next pass should do is find the loop: run under `--nojit`
with a request counter, diff the two arms' iteration counts per test method to
name which of the 45 tests spins, then read that test.

## Refuted — do not re-run these

Each hypothesis was tested with a standalone probe reproducing the suspected
shape in isolation. All stay flat, so none is the mechanism.

1. **"The throwable stack-trace side table is never pruned."** The natural
   suspect once the trace-capture symbols showed up in the profile: the frames
   are parked in a VM-wide identity-hash-keyed store that only a collection
   prunes, and Throwables are tiny so the heap-occupancy trigger never fires.
   `ThrowNoGcProbe` — 50,000 throws at depth 200 with **no** `System.gc()` —
   holds flat at **317 MB**. The store is swept.
2. **"Classloader churn leaks metadata."** `LoaderChurnProbe` — 240 throwaway
   `URLClassLoader`s over the same 186 real jars — grows 464 → 508 MB, ~0.2 MB
   per loader. Real, small, two orders of magnitude short.
3. **"A JIT recompile loop."** `CRATONVM_DBG_JITC=1` over 30 s: **1,358 compiles
   of 1,280 distinct methods**, most-recompiled method 25 times. Ordinary
   warm-up volume.
4. **"Per-thread VM state is retained at thread exit."** Real but **bounded**:
   `ThreadOnlyProbe` shows ~85 KB per exited-and-joined thread (HotSpot flat at
   48 MB over 2,000 threads, CratonVM 268 → 435 MB) — but it **plateaus**, with
   18,000 threads levelling in a 500-800 MB band and falling back. Worth its own
   look; not 5 GB.

Three JIT kill switches were tried and none clears it (all still spin past a
200 s cap): `CRATONVM_DISABLE_AALOAD_LICM`, `CRATONVM_DISABLE_ARITH_LICM`,
`CRATONVM_JIT_GETFIELD_HELPER`.

Two dead ends on instrumentation, recorded to save the next person the time:
`heaptrack` cannot see this — the binary uses **mimalloc** as its global
allocator and mimalloc goes to `mmap` directly, so libc-malloc interception
records nothing. And a `perf record -e page-faults` profile attributes 21.6% to
mimalloc internals on the request thread with **no resolvable Rust caller**;
frame-pointer unwinding does not reach past them. Sampling says where allocation
happens, which was the wrong question anyway.

## Reproduction

```bash
# fixture: apps/spring-boot, built test classes + per-module cratonvm-test-cp.txt
# /data/sb4.sh and /data/sbrss.sh on the Azure Linux box wrap the exact launch
# run-spring-boot-suite.ps1 uses (three env vars, --stack-dump-on-timeout 0,
# --add-opens=java.base/java.net).

MEMCAP=8G /data/sb4.sh <cratonvm> module/spring-boot-quartz \
  org.springframework.boot.quartz.actuate.endpoint.QuartzEndpointWebIntegrationTests q 400

# the passing control
MEMCAP=8G /data/sb4.sh <cratonvm> module/spring-boot-quartz <same class> q 400 --nojit

# count the loop rather than watching the memory — this is the load-bearing number
CRATONVM_DBG_STTRACE=1 <cratonvm> … 2>&1 | grep -c 'QuartzEndpoint.triggerQuartzJob'
```

## Related

- retired/moving-young-fallback-four-springboot-classes-RETIRED-20260818.md —
  the page this was found from. Same class, different mechanism: that one is a
  `[moving-young]` fallback spiral under Generational, and this run logs none.
  Its thesis that the JIT is what breaks these classes survives.
- `docs/known-issues/gc/generational-young-relocation-nulls-live-string-references-20260818.md`
  — the other finding from the same re-measurement.
