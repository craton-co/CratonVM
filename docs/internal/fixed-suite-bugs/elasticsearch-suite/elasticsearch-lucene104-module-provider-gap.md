# Elasticsearch Lucene104 provider initialization gap

Status: fixed

Date observed: 2026-07-02

Date fixed: 2026-07-02

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

This initially looked like a CratonVM service-provider discovery gap around
Lucene's `module-info.class` provider declarations. The direct probe showed the
JPMS `provides` entry was discoverable, but Lucene's `NamedSPILoader` discarded
`Lucene104Codec` because the provider constructor failed while initializing
Lucene's vectorization stack.

The CratonVM-only blocker was missing JDK 25 native coverage reached by that
constructor:

```text
com/sun/management/internal/Flag.initialize()V
jdk/internal/vm/vector/VectorSupport.registerNatives()I
jdk/internal/vm/vector/VectorSupport.getCPUFeatures()Ljava/lang/String;
jdk/internal/vm/vector/VectorSupport.getMaxLaneCount(Ljava/lang/Class;)I
```

The fix adds the JDK 25 `Flag` management native surface, registers the
`VectorSupport` ACC_NATIVE methods in the real-JDK essential native path, and
pins JPMS module `provides` discovery with a ServiceLoader regression test.

## Verification

Focused tests:

```powershell
cargo test -p cratonvm-native-builtins register_essential_includes_jdk25_vector_support_natives
cargo test -p cratonvm-native-builtins service_loader::tests::discover_providers_includes_jpms_module_provides_entries
cargo test -p cratonvm-native-builtins vector_api_tests::test_vector_support_jdk25_natives_registered
cargo test -p cratonvm-native-builtins --features experimental-jmx jmx::jmx_tests::test_jdk25_internal_flag_natives_registered
```

Runtime probe:

```powershell
cargo build --release -p cratonvm-cli
Copy-Item target\release\cratonvm.exe target\release\cratonvm-lucene104-module-provider-20260702.exe -Force
$env:CRATONVM_STRICT_SWALLOWS='1'
.\target\release\cratonvm-lucene104-module-provider-20260702.exe --java-home 'C:\Program Files\Java\jdk-25' -cp 'target\lucene104-probe;C:\Users\Victor\.gradle\caches\modules-2\files-2.1\org.apache.lucene\lucene-core\10.4.0\7493bc763cd5e91f2a8f7722c2f90d8ce15c6319\lucene-core-10.4.0.jar' Lucene104Probe
```

The probe exits 0 and prints:

```text
Lucene104
```

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

The pre-fix slice runs showed the Lucene104 provider gap clearly.

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

## Current verification

The later full-suite run `es-current-full-jiton-20260702` was executed after
the fix with:

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

That run no longer contains the `Lucene104` provider-missing failure signature.
Current Elasticsearch failures are tracked as separate open issues in
`../../../known-issues`.

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

## Notes

Elasticsearch native access logs a `LoaderHelper.findPlatformLibDir` NPE under
both HotSpot and CratonVM in this fixture. HotSpot continues and passes the
affected tests, so that warning is not the primary CratonVM-only failure.

The original visible failure happened during Lucene codec initialization:
`org.apache.lucene.codecs.Codec$Holder.<clinit>` could not resolve `Lucene104`
after `NamedSPILoader` discarded the provider whose constructor had failed.
