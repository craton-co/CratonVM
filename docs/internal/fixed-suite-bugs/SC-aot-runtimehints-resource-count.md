# AOT RuntimeHints resource-pattern count mismatch — Stream.distinct() ignores element equals()

> **STATUS: RESOLVED (archived).** The single high-confidence root cause — `Stream.distinct()`
> (`native_stream_distinct`) deduping via shallow `values_equal` instead of the element's real
> Java `equals()` — is fixed on `dev` (`029f2c87`, merge `cf269fc5`). `native_stream_distinct`
> now dedups via `list_element_matches` (cheap structural check → seen element's `equals`),
> matching `List.contains`/`indexOf` and `Collectors.groupingBy`. This resolves the 3 count-mismatch
> tests (`RuntimeHintsWriterTests$ResourceHintsTests.registerExactMatch` /
> `registerPatternWithIncludesAndExcludes`, `FileNativeConfigurationWriterTests.resourceConfig`).
> Verified vs HotSpot (JDK 25) with `test_classes/DistinctEquals` (pre-fix dev over-counts:
> record 4 vs 3, value-class 5 vs 3; post-fix 5/5, String/Integer dedup unchanged).
>
> **Residual answered 2026-07-29:** all 5 writer cases (`reflectionConfig`, `resourceConfig`,
> `jniConfig`, `serializationConfig`, `proxyConfig`) failed identically on HotSpot and CratonVM
> when the suite runner loaded Spring from its versioned snapshot JAR. The manifest made
> `SpringVersion.getVersion()` non-null, so the writer emitted a top-level `comment` rejected by
> the `NON_EXTENSIBLE` assertions. Prepending the owning module's main output, as Gradle does,
> makes the class 7/7 on both VMs. This is a fixture-classpath issue, not a second VM bug; see
> `spring/CRATONVM-SPRING-AOT-RUNNER-CLASSPATH-BATCH-ISOLATION-20260729-FIXED.md`.

## Symptom
Three Spring AOT nativex tests fail with resource glob count mismatches:
- `FileNativeConfigurationWriterTests.resourceConfig`: "resources[]: Expected 5 values but got 8"
- `RuntimeHintsWriterTests$ResourceHintsTests.registerExactMatch`: "Expected 5 values but got 8"
- `RuntimeHintsWriterTests$ResourceHintsTests.registerPatternWithIncludesAndExcludes`: "Expected 7 values but got 8"

CratonVM emits extra (duplicate) resource glob entries that HotSpot collapses.

## Affected tests
- `org.springframework.aot.nativex.FileNativeConfigurationWriterTests.resourceConfig`
- `org.springframework.aot.nativex.RuntimeHintsWriterTests$ResourceHintsTests.registerExactMatch`
- `org.springframework.aot.nativex.RuntimeHintsWriterTests$ResourceHintsTests.registerPatternWithIncludesAndExcludes`

Listed for assessment but NOT this root cause: `FileNativeConfigurationWriterTests.{reflectionConfig, jniConfig, serializationConfig, proxyConfig}` (see "More than one cause?" below).

## Root cause (single, high-confidence)
Spring builds the `resources` list in `ResourceHintsAttributes.resources()`:
```
hint.resourcePatternHints()
    .map(ResourcePatternHints::getIncludes).flatMap(List::stream).distinct()   // <-- dedup
    .sorted(RESOURCE_PATTERN_HINT_COMPARATOR).map(this::toAttributes).toList()
```
(`spring-core/.../aot/nativex/ResourceHintsAttributes.java:54-57`)

Each `registerPattern(...)` call creates its own `ResourcePatternHints`, and `ResourcePatternHints.Builder.expandToIncludeDirectories` expands a pattern into all its parent-directory hints (`.../aot/hint/ResourcePatternHints.java:87-116`). So:
- `registerExactMatch`: `com/example/test.properties` -> [`/`,`com`,`com/example`,`com/example/test.properties`]; `com/example/another.properties` -> [`/`,`com`,`com/example`,`com/example/another.properties`]. 8 raw hints; the 3 shared prefixes (`/`,`com`,`com/example`) must collapse via `distinct()` -> **5**.
- `registerPatternWithIncludesAndExcludes`: two wildcard patterns share only `/` -> 8 raw, distinct -> **7**.

`distinct()` dedups using `ResourcePatternHint.equals()/hashCode()`, which compare the String `pattern` field (+ reachableType) вЂ” see `.../aot/hint/ResourcePatternHint.java:94-103`. Distinct *instances* with the same pattern are therefore equal.

**CratonVM divergence:** `Stream.distinct()` is served by the synthetic native `native_stream_distinct` (`native-collections/src/lib.rs:10488`, registered at `lib.rs:9644-9649`):
```rust
for elem in &elements {
    let dup = unique.iter().any(|u| values_equal(ctx, u, elem));
    if !dup { unique.push(*elem); }
}
```
`values_equal` (`native-collections/src/lib.rs:974-1007`) only handles: pointer identity, `String` value-equality, enum constants, and unboxed primitives вЂ” and explicitly returns `false` for any other two distinct object instances. It never invokes the element's Java `equals()`/`hashCode()`. `ResourcePatternHint` is a plain final class (not String, not enum), so two `ResourcePatternHint("/", null)` from the two registrations are seen as non-equal -> no dedup -> **8** entries.

This is the *exact same gap* already fixed elsewhere and documented inline in the same file:
- `group_key_equal` (`lib.rs:1023`) вЂ” added real `equals` for `Collectors.groupingBy`/`toMap` keys.
- `list_element_matches` (`lib.rs:1058`) вЂ” added real `equals` for `List.contains`/`indexOf`.

`native_stream_distinct` was simply never updated to the same standard.

## Reproduction sketch
```java
import java.util.*; import java.util.stream.*;
public class DistinctEquals {
  record K(String p) {}                         // equals/hashCode by field
  public static void main(String[] a){
    List<K> in = List.of(new K("/"), new K("com"), new K("/"), new K("x"));
    System.out.println(in.stream().distinct().count()); // HotSpot 3, CratonVM 4
  }
}
```
Run: `cratonvm --java-home <jdk25> -cp . DistinctEquals`
- HotSpot: `3` (the two `K("/")` collapse).
- CratonVM (bug): `4`.

The original Spring failure is the same effect: `registerExactMatch` expects 5 globs, gets 8.

## Suspected subsystem
`native-collections` synthetic Stream pipeline вЂ” `native_stream_distinct` + `values_equal` (`native-collections/src/lib.rs`).

## Severity / Confidence
- Severity: **medium** вЂ” produces semantically wrong `Stream.distinct()` output (over-counts) for *any* user/value class with a custom `equals` that is not String/enum/primitive. Correctness bug affecting AOT metadata generation and, more broadly, any distinct() over value objects.
- Confidence: **high** вЂ” the data flow is fully traced from the test through Spring to the exact CratonVM function, and the divergence (no real-equals dispatch) is explicit in `values_equal` and called out in the file's own doc comments.

## Recommendation
**Fix** (contained, low-risk, well-precedented). In `native_stream_distinct` (`native-collections/src/lib.rs:10488`), replace the `values_equal`-only dedup with a comparison that first does the cheap structural check and then falls back to the element's real Java `equals()` for distinct object pairs вЂ” i.e. reuse/parallel `list_element_matches` (`lib.rs:1058`) or `group_key_equal` (`lib.rs:1023`). Pattern:
```rust
let dup = unique.iter().any(|u| list_element_matches(ctx, u, elem)); // needs &mut ctx
```
Note `list_element_matches`/`group_key_equal` take `&mut dyn NativeContext` (they call `invoke_virtual`); `native_stream_distinct` already has `&mut`. The existing O(n^2) loop stays semantically correct since Spring re-sorts after distinct, so output ordering is irrelevant вЂ” only the dedup count matters.

## More than one root cause?
There were two independent effects: the VM's `Stream.distinct()` equality bug
caused the original three resource-count mismatches, while the later five
`FileNativeConfigurationWriterTests` failures were the HotSpot-identical
versioned-JAR `comment` fixture effect. The suspected case-insensitive field
ordering divergence did not occur. The corrected owning-module classpath makes
the entire writer class 7/7 in JIT and `--nojit`.

## Open questions
- **Answered:** the failures were the versioned-JAR `comment` fixture effect and matched HotSpot exactly; the corrected runner is 7/7 on both VMs.
- Are there other terminal/intermediate stream natives (`Collectors.toSet`, `Set`-collectors) that likewise dedup via `values_equal` and would mis-handle value classes? Worth a sweep when applying the fix.
