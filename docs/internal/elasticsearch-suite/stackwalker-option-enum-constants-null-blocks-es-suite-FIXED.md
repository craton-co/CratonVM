# `StackWalker$Option` `$VALUES` is null during `StackWalker.<clinit>` - FIXED

**Status:** FIXED on 2026-07-10. **Severity while open:** high.

Resolved by completing the native `java/lang/StackWalker$Option.<clinit>`
implementation in `native-builtins/src/stack_walker.rs`: it now publishes the
three option singletons both as static enum constants and as the generated
`$VALUES` reference array that real `EnumSet.noneOf`, `Class.getEnumConstants`,
and `StackWalker.Option.values()` require.

## Original Reproduction

Any ES test class whose bootstrap reaches `org.apache.lucene.tests.util.LuceneTestCase`
hit this, for example:

```
<EXE> --java-home <realjdk> -Xmx2g -cp <es-suite-classpath> \
  org.junit.runner.JUnitCore org.elasticsearch.index.mapper.UpdateMappingTests
```

Previously it failed immediately with:

```
[CLINIT-TRACE] ... at org/apache/lucene/internal/tests/TestSecrets.ensureCaller (TestSecrets.java:141)
[CLINIT-TRACE] ... at java/lang/StackWalker.<clinit> (StackWalker.java:322) bci=2
[CLINIT-TRACE] ... at java/util/EnumSet.noneOf (EnumSet.java:115) bci=32

WARN native_class_get_enum_constants: $VALUES is null/non-object for class=java/lang/StackWalker$Option
WARN <clinit> failed - wrapping in ExceptionInInitializerError class=java/lang/StackWalker cause=java/lang/ClassCastException class java.lang.StackWalker$Option not an enum
```

`java.lang.StackWalker`'s own `<clinit>` calls `EnumSet.noneOf(Option.class)`
as essentially its first action. Real `EnumSet.noneOf` asks
`Class.getEnumConstantsShared()` for the enum universe; CratonVM initialized
`StackWalker$Option` but left its generated `$VALUES` field null, so the real
JDK enum path treated the class as not-an-enum.

## Root Cause

The failure was not generic class-init reentrancy. `StackWalker$Option.<clinit>`
is registered as a CratonVM native boot-path initializer in
`native-builtins/src/stack_walker.rs`. That native initialized
`RETAIN_CLASS_REFERENCE`, `SHOW_HIDDEN_FRAMES`, and `SHOW_REFLECT_FRAMES`, but
it did not populate the compiler-generated `$VALUES` field.

The fix stores the three allocated option objects in order, creates a reference
array of component type `StackWalker$Option`, fills it with those same objects,
and writes it to `$VALUES` (plus the older `ENUM$VALUES` spelling as a harmless
compatibility fallback).

## Impact After Fix

The original StackWalker blocker is gone for the sampled Elasticsearch rows.
Both `UpdateMappingTests` and `CombineIntervalsSourceProviderTests` now advance
past Lucene `TestSecrets.ensureCaller` / `StackWalker.<clinit>` with no
`StackWalker$Option` `$VALUES is null/non-object` warning and no
`class java.lang.StackWalker$Option not an enum` failure.

Those rows still fail later on a separate Elasticsearch-suite family:
`org/elasticsearch/xcontent/XContentBuilder.<clinit>` wraps
`java.lang.ClassCastException: java.util.function.Function$Identity cannot be
cast to java.util.function.Function`. Keep the class-level ES crash docs open
for that downstream issue.

## Verification

- Built branch binary:
  `/data/data/bin/cratonvm-es-suite-stackwalker-option-enum-20260710-021000-r1`.
- Scratch `StackWalkerEnumProbe`: `Class.getEnumConstants`, `EnumSet.noneOf`,
  and `StackWalker.getInstance(empty)` pass.
- Scratch `StackWalkerEnumProbe2`: direct `StackWalker.Option.values()` returns
  length `3`; `Class.getEnumConstants()` returns length `3`.
- `cargo test -p cratonvm-native-builtins option_clinit_populates_enum_values_array -- --nocapture`: PASS.
- ES harness `others` row adjusted to `Start 1659 Count 1`
  (`UpdateMappingTests`): reaches test execution and fails later in
  `XContentBuilder`, with no StackWalker enum warning.
- ES harness `others` row adjusted to `Start 1680 Count 1`
  (`CombineIntervalsSourceProviderTests`): same downstream `XContentBuilder`
  failure, with no StackWalker enum warning.
