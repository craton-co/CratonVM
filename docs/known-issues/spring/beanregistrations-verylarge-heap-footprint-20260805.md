# `BeanRegistrationsAotContributionTests` — the 10001-definition test exhausts the heap in javac

| | |
|---|---|
| **Status** | **OPEN.** 13 of 14 tests match HotSpot. `applyToWithVeryLargeBeanDefinitionsCreatesSeparateSourceFiles` does not. |
| **Scope** | `org.springframework.beans.factory.aot.BeanRegistrationsAotContributionTests` (spring-framework, `spring-beans`). |
| **Measured** | 2026-08-02 and 2026-08-05, Azure host `20.83.144.174`, real JDK 25, branch `fix/spring-4tests-20260802`, binaries `/data/data/wt-spr4-20260802/localbin/cvm-spr4-{base,poll,gcscan}.bin`. |
| **Not** | Not the quadratic dirty-card scan fixed on this same branch (`gc/src/old_gen.rs`) — that is real and removes 73.7% of CPU here, but this test still fails with it in. |

## Symptom

The class runs to completion on HotSpot in ~10–16 s, 14/14. On CratonVM the
first 13 tests pass; the 14th builds a contribution for **10001** bean
definitions, generates three source files, and compiles them with the
in-process `TestCompiler`. That compile dies:

```
org.springframework.core.test.tools.CompilationException: Unable to compile source
```

with an **empty** `Errors:` section, because `JavacTaskImpl.call()` returned
`false` without reporting a diagnostic. javac's own crash report is in the
run log and names the cause:

```
An exception has occurred in the compiler (25.0.3). ...
java.lang.OutOfMemoryError: Java heap space (alloc_array length 1048320)
```

Read the `CompilationException` message alone and you get "Unable to compile
source" with no errors listed, which reads like a codegen bug. It is not — grep
the log for `OutOfMemoryError`.

## The gap is footprint, not the heap cap

HotSpot does not merely have a bigger default heap here; it needs *far less*
heap than CratonVM can finish in:

| | heap | result |
|---|---|---|
| HotSpot | `-Xmx512m` | **passes, 13 s** |
| HotSpot | `-Xmx1g` / `-Xmx2g` | passes, 13 s / 15 s |
| CratonVM | 4 GiB (its own ergonomic default) | **OOM in javac after ~2324 s** |
| CratonVM | `--Xmx 8g` | still running at the 1800 s cap, no result |

So raising `MAX_ERGONOMIC_HEAP` (`vm-cli/src/main.rs`, capped at 4 GiB because
the generational heap eagerly commits its arenas) would not close this: 8 GiB
does not finish either. This is **not** the "CratonVM's default heap is capped
at 4 GiB while HotSpot's is an uncapped RAM/4 = 8.4 GiB" story.

`-Xlog:gc` on HotSpot at `-Xmx512m` puts its live set after a young collection
at **~180 MB** (`326M->180M(352M)` near the end of the run).

**The corresponding CratonVM number is NOT yet measured — do not quote one.**
An earlier revision of this doc claimed CratonVM's live set was "1.2 GB and
still climbing, roughly 8x". That was wrong, and the way it was wrong is worth
recording:

* `jcmd GC.heap_info`'s young "used" is `young_from_used()` — the from-space
  **bump-allocation cursor**, i.e. everything allocated since the last young
  collection, live or not. It is not a live-set figure.
* `jcmd GC.class_histogram` (`SharedVm::class_histogram`, `vm/src/vm/vm_init.rs`)
  calls `heap.walk_objects()` with **no preceding collection and no liveness
  mark** — it histograms every object in the heap, garbage included. HotSpot's
  `GC.class_histogram` reports live objects.

So the comparison was CratonVM-allocated against HotSpot-live, which proves
nothing. Getting a real number needs a forced collection immediately before the
walk, or an instrument that marks. See
[[verify-what-the-instrument-measures-before-believing-it]] — this is that
lesson, paid for again.


## Heap accounting: nothing is promoted

Two `GC.heap_info` samples ~10 minutes apart on the `--Xmx 8g` run:

```
Young Generation: 1.2 GB / 2.0 GB (58.9% used)      Young: 1.4 GB / 2.0 GB (70.0% used)
Old   Generation: 13.1 MB / 4.0 GB (0.3% used)      Old:   13.1 MB / 4.0 GB (0.3% used)
```

Old gen is **frozen at 13.1 MB** across many collections. That figure comes
from `old_gen_stats()`, which is a real used-bytes accounting rather than a
cursor, so the flatness is meaningful — but "nothing is promoted" is one
reading and "promoted then collected by a major GC" is another, and these two
samples cannot tell them apart. The young column beside it is the allocation
cursor (above) and should not be read as growth of live data.

Eliminations, so the next person does not redo them:

* **Not a general promotion defect.** `PromoProbe` (200 MB of long-lived
  `byte[]` plus heavy short-lived churn, `--Xmx 2g`) behaves correctly:
  `CRATONVM_DBG_YOUNG_TRIGGER=1` shows `live` flat at ~131 MB, `non_moving=false`,
  churn reclaimed. The pathology is specific to this workload.
* **Not the non-moving-sweep livelock** of
  `young-gc-trigger-livelock-under-nonmoving-sweep`: only 8 `[moving-young]
  fallback` events fire in a 2324 s run (the counter is rate-limited to
  `n <= 8` then powers of two, so a 9th–15th are possible but no 16th), i.e.
  almost every young collection took the *moving* path, which does promote.
* **`--nojit` is inconclusive** — RSS was lower (2.8 GB vs 4.4 GB at comparable
  elapsed time, which is suggestive of conservative JIT roots retaining
  garbage) but the run did not finish inside 900 s either, so it neither
  confirms nor rules that out. This is the most promising next thread.

## What the profile says

`perf record -F 199 -g` against the pre-fix binary during this test put
`cratonvm_gc::old_gen::OldGen::scan_region_filtered` at **73.66% of all CPU
samples** — a per-object linear scan of the dirty-card range list, i.e.
O(old-gen objects x dirty ranges). That is fixed on this branch and the symbol
no longer appears in the profile at all.

After the fix the profile is **flat** — top entry 5.6%, spread across
`NativeMethodRegistry::slot_for_exact` (+ its `memcmp`),
`force_native_over_real_jdk_bytecode`, `jit_method_calls_native_shadowed` and
`resolve_id_with_descriptor_quirks`, i.e. ~15–25% in by-name "is this method
natively shadowed?" lookups. Per
`profile-before-calling-it-the-interpreter-throughput-wall`, a flat profile is
where the general interpreter gap starts and a single-bug hunt stops paying.

Note the CPU fix does not change the outcome: it makes the test reach the same
OOM sooner. Removing 73.7% of the CPU cannot fix a memory-footprint failure.

## Reproduce

Single test, ~2300 s to the OOM on a quiet host:

```bash
cd /data/data/wt-springsuite8b-20260726/apps/spring-framework/spring-beans
CP="<runner-dir>:$(tr -d '\r' < build/cratonvm-testcp.txt)"
printf -- '-cp\n%s\n' "$CP" > /tmp/af.txt
<cratonvm> --java-home /home/victor/jdk25 \
  --add-opens=java.base/java.lang=ALL-UNNAMED \
  --add-opens=java.base/java.util=ALL-UNNAMED \
  -Djava.awt.headless=true @/tmp/af.txt \
  KRunM org.springframework.beans.factory.aot.BeanRegistrationsAotContributionTests \
        applyToWithVeryLargeBeanDefinitionsCreatesSeparateSourceFiles
```

`KRunM` is `apps/spring-suite-runner/KRunM.java` (one test method, own launcher
pass). The 1001-definition sibling
`applyToWithLargeBeanDefinitionsCreatesSlices` is a ~200 s proxy that *passes*,
useful for A/B work; note its old gen is small enough that the dirty-card
quadratic never bites, so it is **not** a proxy for the GC fix (an alternating
2-binary A/B there is within noise: base 221/221/183 s, fixed 229/204 s).

## Measuring here at all

This host is shared. Two things invalidated whole measurement rounds:

* **Load.** Other sessions took the 16-core box to load average 36; absolute
  times moved ~2x. Always A/B two binaries *alternately* in one script rather
  than comparing against a number from an earlier round.
* **The shared Gradle cache.** `build/cratonvm-testcp.txt` is a July-27
  snapshot of Gradle's `sourceSets.test.runtimeClasspath`, pinned to exact jar
  paths. On 2026-08-05 another session re-resolved dependencies, evicting the
  pinned versions (e.g. `junit-vintage-engine` 6.1.0 -> 6.1.1/6.1.2): **54 of
  254** spring-test entries vanished. The symptom is *not* a classpath error —
  it is `found=0 status=EMPTY`, or worse, a plausible-looking assertion failure
  (the vintage engine silently missing shifts Spring's AOT `TestContextNNN`
  numbering and `AotIntegrationTests#endToEndTests` fails on the diff). Check
  it before believing any result:

```bash
for p in $(tr -d '\r' < build/cratonvm-testcp.txt | tr ':' '\n'); do [ -e "$p" ] || echo "MISSING: $p"; done
```

  Repair by regenerating the dumps (needs network):
  `./gradlew --no-daemon -I ../spring-suite-runner/dump-testcp.init.gradle :spring-test:dumpTestCp`
