# ES HANG — `Thread.join()` never returns for a dead RandomizedRunner worker (Windows host)

Status: OPEN

Discovered 2026-07-19 while doing full end-to-end verification of the
IVFKnn stale-precise-root-mirror fix (see
[`ES-HANG-20260709-...-3ff8aa1c4b.md`](ES-HANG-20260709-server-org-elasticsearch-search-vectors-diversifyingchildrenivfknnfloatslicedvectorquerytests-3ff8aa1c4b.md)).
**Not caused by, or related to, that fix** — confirmed to reproduce
identically on a clean, unmodified `origin/dev` build. This is a
separate, pre-existing CratonVM defect, first observed on this Windows
host; the doc it was found alongside has historically only been
verified on Linux (Azure), which may explain why this has not been
caught before.

## Symptom

Any ES `JUnitCore` direct-invocation repro that goes through
`com.carrotsearch.randomizedtesting.RandomizedRunner` (i.e. essentially
every real ES test class) hangs indefinitely. A `--stack-dump-on-timeout`
dump shows:

```
tid=0 name="main" ... blocked=?  (in Thread.join())
  depth=15 class=java/lang/Thread method=join desc=()V
  depth=16 class=java/lang/Thread method=join desc=(J)V
  ... com/carrotsearch/randomizedtesting/RandomizedRunner.runSuite
```

and the worker thread it is joining shows:

```
name="...-seed#[B17AC9D3E1F2A0C4]-worker" alive=false daemon=false blocked=true roots=1 top=<no-frame-trace>
```

i.e. the worker thread has already finished (`alive=false`) but the
main thread's `Thread.join()` never wakes up — a lost-wakeup /
notification-ordering bug in CratonVM's `Thread.join()` implementation
or in the thread-death → registry-update → notify sequence.

## Reproduction

Any of the following, run directly (bypassing the suite runner), hang
identically:

```powershell
$JDK = "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
$ES  = "C:\craton\CratonVM\apps\elasticsearch"
$CP  = Get-Content "$ES\..\..\server\build\craton-testcp.txt" -Raw
& $EXE --java-home $JDK --stack-dump-on-timeout 90 --Xmx 2g `
  -Dtests.seed=B17AC9D3E1F2A0C4 -Des.path.home=$ES `
  -Dtests.testfeatures.enabled=true -Dtests.security.manager=false -Dtests.asserts=false `
  -Dtests.timeoutSuite=580000! -Dtests.method=testSlicesSparseWithFilter `
  <standard ES --add-opens set, see run-elasticsearch-suite.ps1 Get-EsJavaArgs> `
  -cp $CP org.junit.runner.JUnitCore `
  org.elasticsearch.search.vectors.DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests
```

- `testSlicesSparseWithFilter` on `DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests`: hangs, 2/2 runs.
- `testRandomWithFilter` on the **sibling class** `IVFKnnFloatVectorQueryTests`: hangs, 1/1 run.
- Same hang with `--nojit` — **not JIT-related**.
- Same hang on a from-scratch `origin/dev` build (`C:\craton\CratonVM\target\release\cratonvm.exe`,
  no IVFKnn-session changes at all) — confirms this is not something this
  session's GC/JIT fixes introduced.
- The identical repro under real HotSpot (`java.exe`, same JDK, same
  classpath, same args minus the CratonVM-specific flags) completes
  cleanly: `OK (1 test)`, ~2.2s. This rules out the ES test fixture /
  checkout state — the hang is CratonVM-specific.

## Not yet investigated

- The exact `Thread.join()` / thread-death-notification code path (likely
  `vm/src/vm.rs`, `vm/src/threading/*`, `vm/src/native/jni.rs`'s foreign-thread
  teardown, or wherever the worker's `alive` flag flips and the joiner is
  notified).
- Whether this is Windows-specific (untested on Linux this session — the
  original doc's own verification history was Linux/Azure-only, so this
  may simply never have been exercised on Windows before).
- Whether this is specific to this JDK build (25.0.3.9-hotspot) or this
  particular `com.carrotsearch.randomizedtesting` worker-thread naming/
  lifecycle shape (`...-seed#[...]-worker`).
- Whether recent `dev` commits (unrelated to this session) changed
  `Thread.join()`/thread-registry code in a way that could explain this
  newly-surfaced hang, or whether it is much older and simply never
  exercised via a full ES `JUnitCore` run on Windows before.

## Impact

Blocks full end-to-end `JUnitCore`-based verification of ES test classes
on this Windows host — including the final confirmation step for the
IVFKnn stale-precise-root-mirror fix and any future ES suite work done
here. The underlying GC/JIT correctness fixes in the sibling doc were
still validated thoroughly via non-ES-suite means (full `cratonvm-gc`/
`cratonvm-jit`/`cratonvm-vm` `--lib` suites, JIT differential suites, and
the `bench/BenchSuite.java` micro-benchmarks bt10–bt18/arith/matrix, all
correct-checksum) — this hang specifically blocks only the ES-suite-shaped
verification path.
