# ES BUG — `EmbeddedImplClassLoader`-loaded classes (jar-in-jar `IMPL-JARS/` bundling) throw `NoClassDefFoundError` under CratonVM

Status: OPEN — dominant root cause behind most FAIL rows in the 20260716 tmp-fix rerun

## Discovery context

Found 2026-07-16 while triaging the first batch of genuine (non-infra)
failures from the `es-tmpfix-rerun-20260716` 8-shard rerun (see
[ES-RUN-20260716-tmpdir-fix-in-progress.md](ES-RUN-20260716-tmpdir-fix-in-progress.md)
for the overall run status), after fixing the `libvec.so` fixture recurrence
documented in
[ES-FIXTURE-20260716-libvec-so-wrong-version-recurrence-FIXED.md](../../internal/elasticsearch-suite/ES-FIXTURE-20260716-libvec-so-wrong-version-recurrence-FIXED.md).
With that fixture fixed, a dedupe pass on the remaining FAIL rows' `note`
column showed `java.lang.NoClassDefFoundError` (various inner classes) as
the single largest remaining signature across `libs/core`, `libs/x-content`,
`server`, and other modules — a much larger set of classes than any one
feature area would suggest, since the trigger is Elasticsearch's low-level
content-type bootstrap rather than anything specific to the failing test's
own subject matter.

## Repro

Worktree `/data/wt-es-tmpfix-rerun-20260716` (Azure host
`victor@20.83.144.174`), binary
`/data/data/target-es-tmpfix-rerun-20260716/release/cratonvm-es-tmpfix-rerun-20260716`,
built off `dev` at `3b62bf53`. Fixture:
`/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch`
(with the `libvec.so` fix from the sibling doc applied).

```bash
ES=/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch
CP=$(tr -d '\r' < "$ES/libs/core/build/craton-testcp.txt" | paste -sd: -)
"$EXE" --java-home /usr/lib/jvm/java-21-openjdk-amd64 --Xmx 2g \
  -Djava.io.tmpdir=<workdir>/tmp -Dtests.seed=B17AC9D3E1F2A0C4 -Des.path.home="$ES" \
  <standard ES test JVM args, see run-elasticsearch-suite.ps1 Get-EsJavaArgs> \
  -cp "$CP" org.junit.runner.JUnitCore org.elasticsearch.common.CharArraysTests
```

**Deterministic, CratonVM only:**

```
NOTE: All tests run in this JVM: [CharArraysTests]
EE
Time: 1.889
There were 2 failures:
1) org.elasticsearch.common.CharArraysTests
java.lang.NoClassDefFoundError: com/fasterxml/jackson/core/util/JsonRecyclerPools$ThreadLocalPool
2) org.elasticsearch.common.CharArraysTests
java.lang.NoClassDefFoundError: org/elasticsearch/xcontent/XContentType

FAILURES!!!
Tests run: 0, Failures: 2
```

`Tests run: 0` — the failure happens during class/suite initialization,
before any `@Test` method executes; `CharArraysTests` itself does not touch
Jackson or `XContentType` directly. Both frames are the standard
JUnitCore failure-summary line with no `at ...` trace beneath them in
CratonVM's output (see Open questions).

**Confirmed NOT a classpath problem:** the exact same computed `-cp` string,
run against real HotSpot (`/usr/lib/jvm/java-21-openjdk-amd64/bin/java`,
identical seed, identical flags) with the identical `craton-testcp.txt`,
passes cleanly:

```
NOTE: All tests run in this JVM: [CharArraysTests]
....
Time: 1.018
OK (4 tests)
```

**Confirmed the class genuinely exists on the classpath**, just not at a
top-level jar path:

```
$ jar tf libs/x-content/build/distributions/elasticsearch-x-content-9.5.0-SNAPSHOT.jar | grep JsonRecyclerPools
IMPL-JARS/x-content/jackson-core-2.17.2.jar/com/fasterxml/jackson/core/util/JsonRecyclerPools$ThreadLocalPool.class
IMPL-JARS/x-content/jackson-core-2.17.2.jar/com/fasterxml/jackson/core/util/JsonRecyclerPools.class
```

## Analysis (source-level, not yet fully root-caused)

`org.elasticsearch.xcontent.XContentType`'s static init path pulls in
Jackson's `JsonFactory`/`JsonRecyclerPools`, which Elasticsearch does not
ship as a normal top-level classpath jar. Instead `elasticsearch-x-content-*.jar`
bundles Jackson (and its own impl jars) *inside itself* under an
`IMPL-JARS/<module>/<jar-name>.jar/...` entry prefix, and loads them at
runtime through a custom loader:
`org.elasticsearch.core.internal.provider.EmbeddedImplClassLoader`
(`libs/core/build/classes/java/main/org/elasticsearch/core/internal/provider/EmbeddedImplClassLoader.class`
in this checkout — this is exactly the mechanism ES's own
`EmbeddedImplClassLoaderTests`, `EmbeddedModulePathTests`,
`InMemoryModuleFinderTests`, and `ProviderLocatorTests` unit-test, all four
of which are *also* present in the FAIL set for the same reason).

`javap -p -c` on that class shows it:

- extends `SecureClassLoader` (not a plain `URLClassLoader`);
- reads nested-jar entry bytes directly via `InputStream.readAllBytes()`
  (no extraction to a temp directory — this is unrelated to the tmpdir
  fixture issue in the sibling docs);
- implements **both** the standard single-arg
  `findClass(String name)` **and** the module-aware two-arg
  `findClass(String moduleName, String name)` (the latter is the
  `ClassLoader` method the JDK's own module-system machinery calls when
  resolving a class through a `Module`/`ModuleLayer`, not something typical
  application classloaders override);
- calls `Class.getModule()`/`Module.getName()` on the results, consistent
  with defining each nested jar's classes into its own dynamically
  constructed named `Module` rather than the unnamed module.

This combination — `defineClass` into a synthetic per-jar `Module`,
resolved via the two-arg module-aware `findClass` — is a materially
different code path from ordinary flat-classpath or single-jar
classloading. The working hypothesis is that CratonVM's classloading
support does not fully honor this path (either the two-arg `findClass`
dispatch, or the module-membership bookkeeping `Class.getModule()` depends
on afterward), causing the JVM's normal class-resolution fallback to
report `NoClassDefFoundError` for a class the custom loader could have
supplied.

**Open questions / not yet established:**

- Exactly which JDK-internal call site invokes `findClass(moduleName,
  name)` for this lookup, and whether CratonVM's classloader dispatch
  reaches `EmbeddedImplClassLoader`'s override at all versus silently
  falling through to a different (failing) lookup path.
- Why CratonVM's JUnit failure output has no stack trace frames beneath
  the exception header for this specific failure (`There were 2
  failures:` followed immediately by the exception line, nothing else) —
  worth checking separately whether this is CratonVM losing/truncating the
  trace for this exception, or JUnit legitimately having nothing to print
  because the failure happened during static class construction before any
  frame of interest was captured. If it's the former, that is itself a
  second, smaller CratonVM defect (stack traces should not go missing) and
  should be split into its own doc once confirmed.
- Whether `gen_heap::get_field: out-of-bounds field read dropped` WARNs
  seen for `java/lang/foreign/DowncallHandle` (`class_id=1495`,
  `index=18`, `num_slots=5`) and `java/lang/foreign/Arena`
  (`class_id=1515`) immediately before this failure in full-class runs are
  related, coincidental (same general FFM-heavy ES bootstrap sequence
  triggering both an unrelated pre-existing OOB-guard warning and this
  classloading bug back to back), or actually causal. Not established
  either way — flagged for whoever picks this up, not asserted as a
  connection.

## Impact

Any ES test class whose static initialization path reaches
`XContentType` (i.e. almost anything that touches ES's JSON/YAML/SMILE/CBOR
content-type registry — a very large fraction of the suite, though not
literally every class: simple non-XContent classes like
`TruncatedOutputStreamTests`/`VersionCheckingStreamOutputTests` passed
cleanly in the same smoke batch) fails during suite/class init with `Tests
run: 0` rather than exercising the actual test logic. This is very likely
the single largest remaining contributor to the FAIL count in the
2026-07-16 rerun once the `libvec.so` fixture issue is excluded — see the
run's totals doc for the exact proportion once the full rerun completes.

## Next steps for whoever picks this up

1. Instrument (temporarily, env-var gated per this codebase's convention)
   CratonVM's classloader dispatch to log every call into the two-arg
   `findClass(moduleName, name)` path and confirm whether
   `EmbeddedImplClassLoader`'s override is reached at all for this repro,
   or whether resolution is failing earlier/elsewhere.
2. If the two-arg `findClass` override is never invoked, that pins the bug
   as a JDK module-aware classloading dispatch gap in CratonVM's
   `ClassLoader` implementation — compare against how CratonVM handles the
   single-arg `findClass` (which does work broadly) to find the missing
   dispatch branch.
3. If it *is* invoked but still fails, capture what `Class.getModule()`
   returns for classes it defines, and whether CratonVM's `Module`
   bookkeeping (name/reads/exports) is consistent enough for the
   downstream lookup to succeed.
4. Re-run `org.elasticsearch.common.CharArraysTests` (seed
   `B17AC9D3E1F2A0C4`) plus the four `EmbeddedImplClassLoader`-family unit
   tests listed above after any fix attempt, and diff against HotSpot's
   clean pass as the acceptance bar. Given the blast radius, also re-run a
   broad `others` slice to measure the FAIL-count drop.
