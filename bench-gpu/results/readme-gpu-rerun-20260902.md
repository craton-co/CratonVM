# Re-run of the README GPU rows, and the three GPU apps — 2026-09-02

RTX 2060 (sm_75), CUDA 13.3, Windows 11. CratonVM binary: dev at
`0206834e4` plus the dispatch phase-table instrument (a handful of
`Instant::now()` per dispatch, ~0.1 us on a 73 us dispatch — negligible
at these kernel sizes, but stated because it is not a pristine dev
binary). HotSpot is Adoptium 25.0.3.9. TornadoVM is 4.0.1-jdk25-ptx,
the same build the README's numbers came from.

**The host was not quiet.** The ray-tracer harness's own CPU control
flagged every round as loaded (HotSpot best 1122-1214 ms against a
1093.5 ms limit; the README's figure for that row is 837 ms). Another
session was compiling for part of this. Read the arms against each
other within a row, not against the README's absolute numbers.

## The four array kernels (N = 2^24, best of 3 rounds)

| Row | HotSpot | TornadoVM | CratonVM | README said (craton) | checksum vs HotSpot |
|---|---|---|---|---|---|
| int div-chain (48 divs/elem) | 1911 ms | 27 ms | **8 ms** | 11 ms | match |
| double div-chain (64 divs/elem) | 2126 ms | 135 ms | **82 ms** | 95 ms | match (craton) |
| 128 multiply-adds/elem | 1608 ms | 34 ms | **8 ms** | 8 ms | match |
| dot-product reduction | 1589 ms | fails | **2 ms** | 12 ms | match |

Every CratonVM checksum is bit-identical to HotSpot's.

Two rows moved, both explicable:

* **dot reduction 12 ms -> 2 ms.** This is the warp-fold landed earlier
  today (one `red.global.add` per warp instead of per thread); the
  before/after was 23 ms -> 2 ms on the same bench.
* **int div-chain 11 ms -> 8 ms.** Not attributable to a specific
  change; the host was loaded, and this row is small enough that a
  couple of milliseconds is within the day-to-day spread.

### One methodology trap, recorded because it produced a fake regression

`GpuCompute` times ONE call with no warm-up. Run that way the 128
multiply-add row read **67 ms** for CratonVM — 8x the README's 8 ms, and
worse than TornadoVM — because the single timed call carries the whole
first-call compile (analyze, lower, `ptxas`, module load). Its TornadoVM
twin `TornadoGpuCompute` DOES warm up (`plan.execute()` once before the
timer), so the two arms were not measuring the same thing.

`bench-gpu/GpuComputeWarm.java` is that row's warm harness: same kernel,
best of 5. It reads 8 ms, exactly the README's number. The README's row
says "warm"; `GpuCompute` alone cannot produce a warm number.

### TornadoVM notes

* `TornadoDotBench` throws `ArrayIndexOutOfBoundsException: Index 256 out
  of bounds for length 256` at 2^24, which is why the README's
  TornadoVM cell for that row reads "unimplemented". Unchanged.
* On the double div-chain, TornadoVM's checksum
  (`5.9583712290819384E7`) does NOT match HotSpot's
  (`5.92801002867254E7`); CratonVM's does. Worth knowing when reading
  that row as a speed comparison.
* The `tornado` launcher needs Python3 on PATH. `/c/craton/tornadovm/py3shim`
  exists for this; pass `PYTHON3_DIR=/c/craton/tornadovm/py3shim` to
  `run-raytracer-interleaved.sh` or its TornadoVM arm silently reports
  nothing and the script divides by zero.

## Ray tracer proxy kernel, 7680x4320 (3 interleaved rounds)

| round | HotSpot control | CratonVM | TornadoVM | verdict |
|---|---|---|---|---|
| 1 | 1213.97 ms | 15.95 ms | 26.90 ms | craton 1.687x |
| 2 | 1122.02 ms | 13.85 ms | 25.36 ms | craton 1.831x |
| 3 | 1128.79 ms | 15.50 ms | 24.87 ms | craton 1.604x |

Paired mean: craton 15.10 ms, tornado 25.71 ms — craton faster in 3/3
rounds, 41.3% on the mean. The README's row is 12.29 vs 24.29 (2.0x) on
a quiet host; every round here was flagged CPU-loaded, which inflates
the host-side share of both arms and compresses the ratio. Direction and
ordering hold.

This is the reduced proxy (`bench-gpu/RayTracerKernel.java`), which is
what the README's row has always been — see the note there about the
full app.

## The three apps

### GPULlama3 (`apps/GPULlama3.java`) — runs, and the TornadoVM arm is WRONG

The app's own `llama-tornado` launcher is its native GPU mode, so that —
not HotSpot alone — is the arm to compare against:

| arm | tok/s | output |
|---|---|---|
| HotSpot CPU | 10.52 | correct |
| TornadoVM GPU (PTX 4.0.1) | 16.62 / 19.26 (F16), 20.65 (Q8_0) | **garbage** |
| CratonVM GPU | 35.71 / 35.40 | correct, identical to HotSpot |

**TornadoVM does not produce the right tokens on this box**, for either
model. Its F16 run emits `wed wed wed !7 powers! ... redeemed ...` and
its Q8_0 run emits `nonetheless nevertheless nevertheless ...`. These
are plain ASCII in the captured bytes, so it is not console encoding —
it is the wrong answer. HotSpot and CratonVM agree with each other
character for character.

So the honest reading of the tok/s column is that only two of the three
arms are computing the model. A speed comparison against an arm that is
not producing the right output is not a speed comparison; CratonVM's
2.1x over TornadoVM's number should be read as "TornadoVM is broken
here", not as a benchmark result, until that is explained.

Greedy decode, temperature 0, 48 tokens, Llama-3.2-1B-Instruct-F16 for
every arm. The text is the correctness oracle here, which is what makes
the TornadoVM result readable at all. CratonVM reports
`captured 453 launches as one graph at pos=1` — the explicit graph
capture path is what this app uses.

An earlier pair of rounds on a busier host read 27.14 and 32.08 tok/s;
the spread between runs is larger than most changes, so single-round
numbers from this app are not comparable across sessions.

`compare-hotspot-vs-craton.sh` sends the HotSpot arm's stderr to
`/dev/null`, and the app prints its metrics there — so that script shows
a tok/s line for CratonVM only. The HotSpot figure above was taken by
running that arm directly with stderr kept.

### kfusion-tornadovm (`apps/kfusion-tornadovm`) — runs, very slow

`kfusion.java.Benchmark conf/bm-traj2-local.settings` (the pure-CPU
mirror; the TornadoVM path needs a TornadoVM runtime CratonVM does not
provide).

HotSpot completes frames in 0.52-1.05 s each. Under CratonVM the run
printed its header and no frame row within 10 minutes. This is the
already-documented
`bug-kfusion-tornadovm-cpu-path-superlinear-slowdown-oom-20260824.md`;
a smoke log from 2026-08-29 in the app directory shows 56-96 s per
frame, dominated by `integration`.

### TornadoVM-Ray-Tracer (`apps/TornadoVM-Ray-Tracer`) — not built here

Not a CratonVM problem, and worth stating precisely because this app is
reported to have run on this machine before.

What is on disk: the checkout has NO `target/` and no jar; there is no
`tornadovm-ray-tracer*.jar` anywhere under `C:/craton`; and the local
Maven repository entry
`~/.m2/repository/tornado/tornado-api/0.14-dev/` contains only
`tornado-api-0.14-dev.jar.lastUpdated` and `.pom.lastUpdated` — failed
resolution markers dated 2026-08-22, refreshed by today's attempt. A
successful build would have left the jar there and a jar in `target/`.

Its `pom.xml` pins
`tornado:tornado-api:jar:0.14-dev`, which is no longer published in
either the upstream `universityOfManchester-graal` repository or Maven
Central, and its sources use the removed 0.14 API (`TaskSchedule`,
`TornadoDriver`, `TornadoRuntimeCI`) which TornadoVM 4.x replaced with
`TaskGraph` / `TornadoExecutionPlan`. Building it against the installed
4.0.1 SDK would mean porting the app, not configuring it.

`tornado-api-4.0.1-jdk25.jar` ships `TaskGraph` and
`ImmutableTaskGraph` and no `TaskSchedule`, `TornadoDriver` or
`TornadoRuntimeCI`, so pointing the pom at the installed SDK does not
compile these sources either — porting them is the work.

The README's ray-tracer row never used this app: it uses the reduced
proxy kernel measured above, for the reason stated there (the real
kernel needs dynamic-length scene loops neither engine's analyzer
admits). `bench-tornado/RayTracerTornado.java` — the TornadoVM side of
that proxy — DOES run, and is the ray tracer that produced the README's
rows and today's re-run. If this app ran here a week ago, it was from
somewhere this box no longer holds, or it was that proxy.
