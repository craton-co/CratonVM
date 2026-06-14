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

The precompiled case isolates the matcher inner loop (`Pattern$Node.match` per
character): ~0.9 ms to match a 58-char string vs ~1.5 µs on HotSpot. A ~600× gap on
a pre-compiled matcher is far beyond CratonVM's typical interpreter overhead
(~10–50×), pointing at a regex-engine-specific inefficiency in the
`Pattern`/`Matcher` match loop — not just general interpretation cost.

## Status / fix direction
Open — this is a **performance optimization**, not a one-line fix:
- Most direct: get CratonVM's JIT to cover the `java.util.regex.Pattern$*.match`
  hot loop. These methods run per-class during the archive build; if they stay
  interpreted, the archive build is bound by interpreter speed. Investigate why the
  JIT isn't compiling them (cold per-call Matchers, threshold, or regex methods
  excluded from JIT).
- Or profile the `Matcher.match` interpreted path for a CratonVM-specific O(n²) /
  per-char allocation issue (the 600× gap suggests one exists).

Until then, the CratonVM Arquillian client cannot build the JUnit-5 deployment
archive in reasonable time. (The no-container per-class suite is unaffected — it
never builds a real deployment.)
