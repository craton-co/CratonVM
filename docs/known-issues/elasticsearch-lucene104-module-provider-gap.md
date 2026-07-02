# Elasticsearch Lucene104 module-provider discovery gap

Status: open

Date observed: 2026-07-02

## Summary

Elasticsearch JUnit classes that initialize Lucene fail under CratonVM because
Lucene cannot discover the current `Lucene104` codec provider. HotSpot passes
the same classes with the same classpath.

The CratonVM failure is:

```text
java.lang.IllegalArgumentException: An SPI class of type org.apache.lucene.codecs.Codec
with name 'Lucene104' does not exist.
The current classpath supports the following names:
[Lucene80, Lucene84, Lucene86, Lucene87, Lucene90, Lucene91, Lucene92, Lucene94, Lucene95, SimpleText]
```

The provider is present in the Lucene 10.4 core jar module descriptor:

```text
provides org.apache.lucene.codecs.Codec with org.apache.lucene.codecs.lucene104.Lucene104Codec
```

This points at a CratonVM service-provider discovery gap around Lucene's
`module-info.class` provider declarations. It is not a missing classpath entry:
HotSpot uses the same `build\craton-testcp.txt` file and finds the provider.

## Repro

Build a unique CratonVM binary from `dev`:

```powershell
cargo build --release -p cratonvm-cli
Copy-Item target\release\cratonvm.exe target\release\cratonvm-elasticsearch-suite-20260702.exe -Force
```

Run the Elasticsearch runner:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category others -Jit on -Start 1 -Count 10 -Parallel 4 -TimeoutSec 60 `
  -RunName es-bug-sweep-jiton-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-suite-runner-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-suite-runner-20260702\target\release\cratonvm-elasticsearch-suite-20260702.exe
```

HotSpot baseline for the same slice:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm hotspot -Category others -Jit on -Start 1 -Count 10 -Parallel 4 -TimeoutSec 60 `
  -RunName es-bug-sweep-hotspot-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-suite-runner-20260702\apps\elasticsearch-suite-runner\.suite
```

## Results

Full suite `all[1..2701]` on 2026-07-02 with `-TimeoutSec 300`:

| VM | Result |
|---|---:|
| CratonVM JIT-on | 17 PASS, 2684 FAIL, 0 HANG, 0 CRASH |
| HotSpot JIT-on | 2514 PASS, 169 FAIL, 2 HANG, 16 CRASH |

The full-suite scan found 2394 CratonVM failures whose logs contain the
Lucene104 provider error. Of those, 2279 are CratonVM-only failures where the
same class passed under HotSpot. The remaining 115 overlap HotSpot baseline
failures, hangs, or crashes and should not be counted as CratonVM-only.

Representative full-suite row:

```text
index=26
module=libs/core
class=org.elasticsearch.common.unit.TimeValueTests
CratonVM=FAIL, 21.499s
HotSpot=PASS, 9.851s
```

Slice `others[1..10]`:

| VM | Result |
|---|---:|
| CratonVM JIT-on | 3 PASS, 7 FAIL |
| HotSpot JIT-on | 10 PASS |

CratonVM-specific failures in this slice:

```text
org.elasticsearch.cli.terminal.internal.EcsJsonUtilsTests
org.elasticsearch.cli.terminal.internal.TerminalPrintStreamTests
org.elasticsearch.cli.terminal.JsonTerminalTests
org.elasticsearch.cli.terminal.TerminalTests
org.elasticsearch.common.CharArraysTests
org.elasticsearch.common.collect.TupleTests
org.elasticsearch.common.unit.TimeValueTests
```

Slice `others[11..30]`:

| VM | Result |
|---|---:|
| CratonVM JIT-on | 20 FAIL |
| HotSpot JIT-on | 17 PASS, 3 FAIL |

The 3 HotSpot failures are baseline/environment failures and should be excluded
from CratonVM bug counts:

```text
org.elasticsearch.core.StringsTests
org.elasticsearch.entitlement.bootstrap.HardcodedEntitlementsTests
org.elasticsearch.entitlement.initialization.DynamicInstrumentationTests
```

The remaining CratonVM-only failures in the slice show the same Lucene104
provider error.

JIT-off is also affected: `others[1..10]` produced 2 PASS and 8 FAIL. The seven
Lucene provider failures match the JIT-on failures, so this bug is not
JIT-specific. The extra no-JIT-only failure was
`org.elasticsearch.client.RestClientSingleHostIntegTests::testManyAsyncRequests`
with no useful suppressed stack in the JUnitCore output; keep it separate from
this Lucene provider issue.

## Evidence paths

```text
apps\elasticsearch-suite-runner\.suite\results\es-bug-sweep-jiton-20260702\others-jit\results.tsv
apps\elasticsearch-suite-runner\.suite\results\es-bug-sweep-hotspot-20260702\hotspot-jit\results.tsv
apps\elasticsearch-suite-runner\.suite\results\es-bug-sweep-jiton-20260702-b\others-jit\results.tsv
apps\elasticsearch-suite-runner\.suite\results\es-bug-sweep-hotspot-20260702-b\hotspot-jit\results.tsv
apps\elasticsearch-suite-runner\.suite\results\es-bug-sweep-nojit-20260702\others-nojit\results.tsv
```

Representative CratonVM log:

```text
apps\elasticsearch-suite-runner\.suite\results\es-bug-sweep-jiton-20260702\others-jit\logs\libs_core.org.elasticsearch.common.unit.TimeValueTests.out.log
```

HotSpot baseline copies:

```text
apps\elasticsearch-suite-runner\.suite\baseline\hotspot-baseline-es-bug-sweep-hotspot-20260702.tsv
apps\elasticsearch-suite-runner\.suite\baseline\hotspot-baseline-es-bug-sweep-hotspot-20260702-b.tsv
apps\elasticsearch-suite-runner\.suite\baseline\hotspot-baseline-latest.tsv
```

Full-suite evidence:

```text
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\baseline\hotspot-baseline-es-full-hotspot-20260702.tsv
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-jiton-20260702\all-jit\logs\libs_core.org.elasticsearch.common.unit.TimeValueTests.out.log
```

## Notes

Elasticsearch native access logs a `LoaderHelper.findPlatformLibDir` NPE under
both HotSpot and CratonVM in this fixture. HotSpot continues and passes the
affected tests, so that warning is not the primary CratonVM-only failure.

The current blocker happens later during Lucene codec initialization:
`org.apache.lucene.codecs.Codec$Holder.<clinit>` cannot resolve `Lucene104`.
