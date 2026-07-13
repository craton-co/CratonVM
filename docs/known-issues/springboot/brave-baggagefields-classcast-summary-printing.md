# `BraveAutoConfigurationTests`: `ClassCastException: brave.internal.baggage.BaggageFields cannot be cast to java.lang.String` during JUnit summary printing

**Status: OPEN, small scope (1 class). NOT a memory-safety issue** — no
native crash, no heap-corruption guard hits. Uncaught Java-level exception.

Found while verifying the [`OnClassCondition` NPE-cast-to-`String[]`
fix](../../internal/springboot/onclasscondition-npe-cast-string-array-cluster-FIXED.md).
`BraveAutoConfigurationTests` used to `FAIL` normally (26 tests, 2 failed,
~230s) on unmodified `dev`. With that fix applied (which corrects
`@ConditionalOnClass` evaluation, changing which beans/conditions match for
this class), the run instead dies after ~30-70s before printing any test
results:

```
WARN cratonvm_vm::vm::vm_util: <clinit> failed — wrapping in ExceptionInInitializerError
  class=sun/util/cldr/CLDRBaseLocaleDataMetaInfo
  cause=java/lang/ClassCastException brave.internal.baggage.BaggageFields cannot be cast to java.lang.String
  ...
Caused by: java/lang/ClassCastException: brave.internal.baggage.BaggageFields cannot be cast to java.lang.String
	at SbRunner.main(SbRunner.java:39)
	at org/junit/platform/launcher/listeners/MutableTestExecutionSummary.printTo(MutableTestExecutionSummary.java:156)
	at java/io/PrintWriter.printf(PrintWriter.java:870)
	at java/io/PrintWriter.format(PrintWriter.java:973)
	at java/util/Formatter.format(Formatter.java:2761)
	at java/util/Formatter$FormatSpecifier.print(Formatter.java:3155)
	...
	at java/text/DecimalFormatSymbols.getInstance(DecimalFormatSymbols.java:181)
	at sun/util/locale/provider/LocaleProviderAdapter.getAdapter(LocaleProviderAdapter.java:251)
	...
	at sun/util/cldr/CLDRLocaleProviderAdapter.<clinit>(CLDRLocaleProviderAdapter.java:54)
	at sun/util/cldr/CLDRBaseLocaleDataMetaInfo.<clinit>(CLDRBaseLocaleDataMetaInfo.java:19)
```

`SbRunner.main`'s call to `MutableTestExecutionSummary.printTo` triggers a
`Formatter`/`DecimalFormatSymbols`/CLDR `Locale` bootstrap chain
(formatting the test-count summary line), and somewhere in there a live
`brave.internal.baggage.BaggageFields` object ends up where a `String` is
expected — a genuine type-confusion bug, but NOT the same class as the
heap-corruption defect the sibling fix resolved (no `gen_heap::` guard
messages precede it; the process doesn't crash, it just propagates an
uncaught exception out of `main`).

## Hypothesis (untested)

The `@ConditionalOnClass` fix likely changes Brave's bean-creation counts
(previously-masked conditions now evaluate correctly), which changes the
JUnit summary's actual content (different pass/fail/container counts) enough
to reach a `Formatter`/CLDR code path this class never exercised before.
Since `BaggageFields` is a Brave-internal tracing-context class, this smells
like a **stale receiver / wrong-object** bug somewhere in the CLDR
`Locale`-bootstrap machinery unrelated to annotations — possibly a
cache/registry keyed incorrectly, or a `Formatter` varargs argument slot
reused across an unrelated native call. Not yet investigated further.

## Repro

```powershell
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -ClassList <TSV row: module/spring-boot-micrometer-tracing-brave	org.springframework.boot.micrometer.tracing.brave.autoconfigure.BraveAutoConfigurationTests> `
  -Start 1 -Count 1 -Exe <cratonvm exe from dev, post onclasscondition-fix merge>
```
