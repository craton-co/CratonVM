# Interpreter timings on the Windows dev box halve when the main thread runs on an E-core (not a class-count slowdown)

Status: OPEN, external (host CPU scheduling). Nothing in CratonVM to fix; the gap is in how we measure.

## The report

The i89-L4 close (`docs/internal/fixed-bugs/interpreter-i89-L4-method-finder-per-class-and-per-handle-cost-CLOSED.md`,
"Not this page") recorded that with 25,000-30,000 classes defined, a plain interpreted static
call, an allocation and a `MethodType` intern hit ran 1.5-2.5x slower than with 15,000 classes
(`--jdk-only --nojit`, no collection in the run). It also noted that the effect was bimodal: one
15,000-class run was slow too.

## The measured cause: the Windows scheduler, not the class count

The Windows dev box has a hybrid Intel CPU (`Genuine Intel(R) 0000`, 24 cores / 32 threads).
`GetLogicalProcessorInformationEx` reports it like this:

* logical processors 0-15 are 8 hyperthreaded P-cores (EfficiencyClass 1);
* logical processors 16-31 are 16 E-cores (EfficiencyClass 0).

An unpinned process lands its `main-vm` thread on either kind of core, and the scheduler can
move it there mid-run. On an E-core every interpreted operation costs about 1.8x (2.8x for the
`MethodType` intern), which matches the "slow" numbers exactly.

### Probe

`tools/probes/interp/L4/L4W111ClassCountSlowdown.java`:

1. `java L4W111ClassCountSlowdown gen 30000 blob30k.bin`, on HotSpot, writes 30,000 ClassFile-API
   classes to one file. Each class has a static int field, a static `int m()`, an instance field
   and a constructor.
2. `cratonvm --jdk-only --nojit L4W111ClassCountSlowdown run blob30k.bin <k> init` runs these steps:
   * it times three loops, taking the median of 5 reps each:
     * 2M calls to `static int s(int)`;
     * 1M allocations of `new Box(i)`;
     * 200k calls to `MethodType.methodType(int.class, String.class)`;
   * it defines the first k classes in a user loader and initializes each through `m()`;
   * it times the same loops again and prints the after/before ratio.

   So each process compares itself against its own baseline.

Every run here had `[GC] zgc-real: collections=0`. Release build of dev `2c274d3af7` (LTO off,
16 codegen units), Windows.

### Unpinned, 3 interleaved reps per N (ns/op after the define, and the after/before ratio)

| N | rep 1 | rep 2 | rep 3 |
|---:|---|---|---|
| 1,000 | call 211 (**2.05**), alloc 373 (1.97), MT 2743 (2.48) | 115 (1.01) | 94 (1.00) |
| 5,000 | 118 (1.15) | 93 (0.99) | 106 (1.06) |
| 10,000 | 109 (0.97) | 131 (1.36) | 116 (0.95) |
| 15,000 | 96 (1.04) | 96 (1.00) | 117 (0.94) |
| 20,000 | 112 (1.10) | 95 (0.99) | 96 (0.91) |
| 25,000 | 102 (1.01) | 100 (0.98) | 95 (0.84) |
| 30,000 | 144 (1.38), alloc 347 (1.97) | 110 (1.10) | 126 (1.07), alloc 291 (1.36) |

A separate unpinned 30,000-class run gave 215 / 424 / 3236 ns (ratios 2.30 / 2.30 / 3.24).

Two things in this table rule the class count out:

* the largest slowdown in the sweep is at **1,000** classes;
* both 25,000-class reps in the second and third rounds are flat.

The slow state is a host state: it comes and goes between runs, and it also flips inside a run.

### Pinned (`ProcessorAffinity`), static call / alloc / MethodType in ns/op after the define

| affinity | N | call | alloc | MT | ratios |
|---|---:|---:|---:|---:|---|
| P-cores `0xFFFF`, 3 reps | 1,000 | 104 / 106 / 92 | 194 / 193 / 164 | 977 / 1223 / 869 | 0.93-1.17 |
| | 15,000 | 102 / 111 / 90 | 185 / 199 / 163 | 982 / 1046 / 877 | 1.01-1.12 |
| | 25,000 | 118 / 91 / 92 | 196 / 164 / 164 | 1022 / 880 / 879 | 0.85-1.09 |
| | 30,000 | 103 / 102 / 91 | 179 / 203 / 163 | 1018 / 1007 / 883 | 0.68-1.07 |
| E-cores `0xFFFF0000` | 1,000 | 171 (before 162) | 327 (279) | 2539 (2500) | 1.02-1.17 |
| | 30,000 | 166 (before 162) | 324 (276) | 2498 (2515) | 0.99-1.17 |

What the pinned runs show:

* **On P-cores, 30,000 defined classes cost nothing measurable** over 1,000. The per-N spread
  (about ±10%) is the box's ordinary run-to-run noise.
* **On E-cores, every N is uniformly slow, and slow by the reported amount.**

Alloc's after/before ratio of 1.05-1.17 shows up at every N and on both core kinds, so it is not
a class-count effect either. The "after" allocation loop runs on a heap that is 200+ MB fuller.

The RIP sampler was not needed. An affinity A/B gives the cause directly. And because the E-core
slowdown is spread over every instruction, a profile of the slow state has the same shape as a
profile of the fast one.

### Not part of this gap

`defineMs` (define plus a reflective `m()` call per class) is about 0.5 ms a class on P-cores,
and linear in N: 0.55 ms at 1k, 0.42-0.53 ms at 30k. Define-only is 0.06 ms a class.

## What would close this

Nothing in the VM. HotSpot shows the same core-kind dependence on this box. HotSpot 25.0.3
`-Xint` running the same probe at N=1,000, two reps per affinity:

| HotSpot `-Xint` | call | alloc | MT |
|---|---:|---:|---:|
| P-cores | 12.0-12.2 ns | 21-22 ns | 587-592 ns |
| E-cores | 19.6-25.4 ns | 39-40 ns | 1223-1240 ns |

That is 1.6x, 1.8x and 2.1x slower on E-cores. This page stays open as a measurement rule for
this machine.

* Pin interpreter and microbenchmark timings to the P-cores:

  ```bash
  cmd //c "start /b /wait /affinity FFFF cratonvm.exe <args>"
  ```

  From PowerShell, set `(Get-Process -Id <pid>).ProcessorAffinity = 0xFFFF` instead.
* Or, when comparing arms unpinned, interleave them and take medians. A single slow rep is more
  likely an E-core placement than a regression.
* Before calling an observation a VM-wide slowdown, re-run it pinned. Any "it got slower after
  X" seen within one unpinned process is suspect, because the scheduler moves threads between
  core kinds mid-run.

## Wave 135 (interpreter round i7, lane L6)

CLOSED (external known gap; nothing in the VM should act on it). The page stays in
`docs/known-issues/` as the measurement rule for hybrid-CPU hosts, which is what that directory
holds: a gap the project can do nothing about.

Question asked this wave: should the VM itself act on core kinds, for example with a documented
affinity knob (`SetThreadAffinityMask` / `SetThreadSelectedCpuSets` on Windows,
`sched_setaffinity` on Linux) or a "prefer performance cores" hint
(`SetThreadInformation(ThreadPowerThrottling)` / `PROCESS_POWER_THROTTLING_EXECUTION_SPEED`)?
No, for four reasons:

1. **HotSpot does not.** HotSpot 25 has no flag that pins or steers Java threads by core kind
   (`-XX:+UseNUMA` concerns memory placement, not hybrid cores), and the table above shows HotSpot `-Xint` slowed by 1.6-2.1x on E-cores
   just as this VM is. Both modes target HotSpot's behaviour; a VM that ran faster on this host
   only because it pinned itself would mislead every A/B against HotSpot.
2. **It would cost real workloads.** Pinning every Java and VM thread to the 8 P-cores (16
   logical processors) halves the logical processors a 32-thread application, the GC workers and
   the compiler threads can use. What helps a single-threaded microbenchmark hurts a server.
3. **Embedding.** The VM runs inside other processes (`libcratonvm`, `cratonvm-embed`). Affinity
   and power-throttling state are per process or per OS thread and outlive the VM's own threads;
   a VM that set them would change its host's scheduling. That is the process-global state
   AGENTS.md rules out.
4. **The OS tools already do it, outside the VM.** `start /affinity FFFF` (Windows), `taskset`
   / `numactl` (Linux), or `ProcessorAffinity` from PowerShell pin the whole process for a
   measurement without any VM code. The bench scripts already use them.

What remains is the measurement rule in the section above (pin, or interleave and take
medians). Nothing to build, no switch.
