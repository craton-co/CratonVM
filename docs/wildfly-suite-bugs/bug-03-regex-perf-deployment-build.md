# Bug 03 — `java.util.regex` is 50–600× slower than HotSpot (Gap C: deploy-phase "hang")

**Severity:** Medium (performance, CratonVM-only). Not a crash or deadlock — the
operation makes progress but is so slow it never finishes in practice. This is the
**Gap C** blocker for running the WildFly Arquillian client under CratonVM.

## Symptom
Running the WildFly Arquillian client under CratonVM (KRun + remote container, after
[bug-02](bug-02-zipfile-entries-null.md) fixed the ShrinkWrap NPE) never completes:
a 600 s run prints `BEGIN` then nothing. A `--stack-dump-on-timeout 150` dump shows
the main thread **executing** (frames change between dumps — not blocked on I/O),
deep in deployment-archive building:

```
DeploymentGenerator.loadAuxiliaryArchives
 JUnitJupiterDeploymentAppender.buildArchive
  ContainerBase.addPackages / addPackage
   URLPackageScanner.scanPackage / handleArchiveByFile / foundClass
    AssetUtil.getFullPathForClassResource
     java.util.regex.Matcher.replaceAll -> find -> Pattern$Start.match -> Pattern$BmpCharProperty.match
```

ShrinkWrap calls `AssetUtil.getFullPathForClassResource` (a regex `replaceAll`)
**once per class** while packaging the JUnit-5 + Arquillian container archive
(hundreds–thousands of classes). Under CratonVM each call is slow enough that the
whole archive build takes minutes-to-never.

## Quantification — `RegexBench` (2000 iterations, 58-char input)
[`RegexBench.java`](../../wildfly-suite/repro/RegexBench.java):

| | HotSpot 25 | CratonVM | Slowdown |
|--|-----------|----------|----------|
| `String.replaceAll("[.]","/")` ×2000 | 54 ms | 2691 ms | **~50×** |
| precompiled `Matcher.replaceAll` ×2000 | 3 ms | 1830 ms | **~600×** |

## Deeper root cause — not regex-specific: native-bridged char accessors
Further benchmarks (warm, JIT on) localise it to **per-char VM→native boundary
crossings**, not regex or allocation:

| benchmark (warm) | HotSpot | CratonVM | slowdown |
|------------------|---------|----------|----------|
| precompiled `Matcher.replaceAll` ×20000 | 27 ms | 10 398 ms | ~385× |
| same, JIT **off** (`--nojit`) | — | 14 228 ms | (JIT helps only ~27%) |
| `String.replace(char,char)` ×5000 (native) | 7 ms | 2 705 ms | ~386× |
| **`charAt` loop, NO allocation** (11.6 M calls) | 17 ms | **18 081 ms** | **~1063×** |
| `substring` (allocation) ×200000 | 10 ms | 501 ms | ~50× |

The decisive one: a tight `charAt`/`length` loop **with no allocation** is ~1063×
slower, while an allocation-heavy `substring` loop is only ~50×. So the cost is
**not** allocation/GC and **not** regex-engine-specific — it is the per-call cost of
the hot String accessors. CratonVM's JIT has OSR (1000-backedge) and an invocation
threshold (2000), but `String.charAt`/`length` are **native-bridged** (they appear on
the JIT skip-list / are serviced by Rust natives), so even a JIT/OSR-compiled loop
must cross the VM→native boundary on **every** `charAt` (~1.5 µs) instead of HotSpot's
intrinsified direct char-array read (~1.5 ns). The `java.util.regex.Pattern$Node.match`
loop calls `charAt` per character per position, so it inherits the same ~400–1000×
penalty; ShrinkWrap calls it per class.

## Status / fix direction
Open — a **JIT/intrinsics performance project**, not a one-line fix:
- **Primary:** add JIT intrinsics for the hot String/char accessors
  (`String.charAt`, `length`, `coder`/`value` access, `charSequence` reads) so
  compiled code touches the String's backing array directly instead of calling the
  native each iteration. This is what makes char-by-char loops (regex, path
  manipulation, `replace`) fast on HotSpot.
- Secondary: ensure the `Pattern$*.match` methods are JIT-compiled (not skip-listed)
  once charAt is intrinsified, so the match loop runs as native code.

This is a sensitive area (the JIT carries a curated skip-list and threshold tuning),
so it needs a focused change with full suite re-test — deferred, not rushed here.

Until then the CratonVM Arquillian client cannot build the JUnit-5 deployment archive
in reasonable time. (The no-container per-class suite is unaffected — it never builds
a real deployment, so it never hits this hot path.)
