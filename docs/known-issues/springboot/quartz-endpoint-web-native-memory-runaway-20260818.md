# `QuartzEndpointWebIntegrationTests` grows to 22 GB of NATIVE memory and is OOM-killed — every collector, `--Xmx` has no effect

**Status: OPEN, reproduced 2026-08-18 on `dev` `24a5d4528` (Azure Linux). Not
fixed, root cause NOT isolated. Three plausible hypotheses were tested and all
three are REFUTED — they are recorded below so nobody re-runs them.**

Found re-measuring the four classes on
retired/moving-young-fallback-four-springboot-classes-RETIRED-20260818.md. That
page records this class as green under the shipped default. It is not, on Linux,
on current dev — and the failure is not that page's mechanism: there are **zero**
`[moving-young]` fallback lines in the run.

## The failure

The process grows ~350 MB/s of resident memory until the kernel OOM-killer takes
it. Unconstrained, it reached **21.9 GB RSS** (`total-vm:27 GB`) and killed the
run at ~80 s on a 31 GB shared host; under an 8 GB cgroup cap it dies at ~24 s.

```
Out of memory: Killed process ... (cvm-myoung-base) total-vm:27355160kB,
  anon-rss:21939268kB ... oom_score_adj:0
```

**Run it under a cap.** This is a shared box and an unconstrained arm will
OOM other people's work:
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

Four facts that constrain any explanation:

1. **Collector-independent.** All three backends explode identically, so this is
   not a GC-algorithm bug and not the retired page's mechanism.
2. **It is not the Java heap.** `--Xmx 512m` and `--Xmx 4g` both reach ~7.5 GB —
   the flag makes no difference at all — and the program's own
   `totalMemory()-freeMemory()` reads **~1 MB** throughout. `smaps` puts the
   growth in `[anon:mimalloc]` (982 MB → 1,474 MB → 5,186 MB across three
   samples); JIT code is 16 MB of it and flat.
3. **The JIT is the accelerator, not the cause.** `--nojit` completes, but still
   peaks at 2.7 GB against HotSpot's 385 MB, so there is growth proportional to
   work done with the JIT merely doing that work ~6x faster.
4. **The workload is an exception storm.** `CRATONVM_DBG_STTRACE=1` counts
   **25,650 throwable captures in 22 s** (25,197 distinct), traces up to 240
   frames, all through
   `QuartzEndpoint.triggerQuartzJob` → Mockito's `MockMethodInterceptor.doIntercept`.
   `perf record` agrees: on the request thread, `find_method_index_memoized`
   (18%), `entry_from_frame` (9%), `Vec<StackTraceEntry>::clone` (4%) and
   `capture_throwable_trace` (4%) are the top symbols.

## Three hypotheses, all REFUTED — do not re-run these

Each was tested with a standalone probe that reproduces the suspected shape in
isolation. All three stay flat, so none of them is the mechanism.

1. **"The throwable stack-trace side table is never pruned."** The natural
   suspect: `capture_throwable_trace` parks the frames in a VM-wide
   identity-hash-keyed store that only a collection prunes, and the Throwable
   objects are tiny so the heap-occupancy trigger never fires.
   `ThrowChurnProbe` / `ThrowNoGcProbe` — 50,000 throws at depth 200 with **no**
   `System.gc()` — hold flat at **317 MB** while the Java heap reads 8 MB.
   The store is swept.
2. **"Classloader churn leaks metadata."** The class restarts a Spring/Tomcat
   context per test, each with a fresh webapp classloader.
   `LoaderChurnProbe` — 240 throwaway `URLClassLoader`s over the same 186 real
   jars, loading classes through each — grows 464 → 508 MB, i.e. ~0.2 MB per
   loader. Real, small, and two orders of magnitude short.
3. **"A JIT recompile loop."** `CRATONVM_DBG_JITC=1` over 30 s: **1,358
   compiles of 1,280 distinct methods**. That is ordinary warm-up volume, not a
   loop; the most-recompiled method is compiled 25 times.

A fourth was measured and **bounded**: thread churn does retain ~85 KB per
exited-and-joined thread (`ThreadOnlyProbe`: HotSpot flat at 48 MB over 2,000
threads, CratonVM 268 → 435 MB), but it **plateaus** — 18,000 threads levels off
in a 500-800 MB band and falls back. Worth its own look, but it is not 5 GB.

## Where to look next

The page-fault profile (`perf record -e page-faults -g --call-graph fp`) puts
21.6% on the request thread inside mimalloc with no resolvable Rust caller, and
~28% across JIT compilation (`ir_lower::emit_prologue`,
`x64::driver::compile_with_param_slots`, `emit_hashed_vtable_stub`,
`emit_oop_map_for_safepoint`) plus 6.8% in `ZipArchive::new` /
`ClassPath::load_jar_data_at_depth` — a jar central directory re-parsed per
class load.

The unresolved 21.6% is the one that matters and frame-pointer unwinding does
not reach past mimalloc's internals. The next step is a heap profiler rather
than another sampling profile: build with `MIMALLOC_SHOW_STATS`, or link
`heaptrack`/`bytehound`, and attribute *retained* bytes rather than page faults.
Sampling says where allocation happens; only a heap profile says what is kept.

## Reproduction

```bash
# fixture: apps/spring-boot, built test classes + per-module cratonvm-test-cp.txt
# /data/sb4.sh and /data/sbrss.sh on the Azure Linux box wrap the exact launch
# run-spring-boot-suite.ps1 uses (three env vars, --stack-dump-on-timeout 0,
# --add-opens=java.base/java.net).

MEMCAP=8G /data/sb4.sh <cratonvm> module/spring-boot-quartz \
  org.springframework.boot.quartz.actuate.endpoint.QuartzEndpointWebIntegrationTests q 400

# with the RSS trace
/data/sbrss.sh <cratonvm> module/spring-boot-quartz <same class> q 300

# the passing control
MEMCAP=8G /data/sb4.sh <cratonvm> module/spring-boot-quartz <same class> q 400 --nojit
```

## Related

- retired/moving-young-fallback-four-springboot-classes-RETIRED-20260818.md —
  the page this was found from. Same class, different mechanism: that one is a
  `[moving-young]` fallback spiral under Generational, and this run logs none.
- `docs/known-issues/gc/generational-young-relocation-nulls-live-string-references-20260818.md`
  — the other finding from the same re-measurement.
