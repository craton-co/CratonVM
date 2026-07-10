# `StackWalker$Option` `$VALUES` is null during `StackWalker.<clinit>` — new dominant ES-suite blocker after the EnumSet fix

**Status:** OPEN. **Severity:** high — newly discovered 2026-07-09 while
verifying the `EnumSet.of()`/`allOf()` fix (see
`docs/internal/fixed-suite-bugs/enumset-synthetic-surface-drop-realmode-FIXED.md`).
Not a regression from that fix — it was **masked** by the EnumSet bug
before (`EnumSet.noneOf()` never reached real bytecode / real
`getEnumConstantsShared()` in real-JDK mode, so this gap never triggered)
and is now the new dominant blocker for the same Elasticsearch suite the
EnumSet bug used to block.

## Reproduction

Any ES test class whose bootstrap reaches `org.apache.lucene.tests.util.LuceneTestCase`
hits this — e.g.:

```
<EXE> --java-home <realjdk> -Xmx2g -cp <es-suite-classpath> \
  org.junit.runner.JUnitCore org.elasticsearch.index.mapper.UpdateMappingTests
```

fails immediately with:

```
[CLINIT-TRACE] ... at org/apache/lucene/internal/tests/TestSecrets.ensureCaller (TestSecrets.java:141)
[CLINIT-TRACE] ... at java/lang/StackWalker.<clinit> (StackWalker.java:322) bci=2
[CLINIT-TRACE] ... at java/util/EnumSet.noneOf (EnumSet.java:115) bci=32

WARN native_class_get_enum_constants: $VALUES is null/non-object for class=java/lang/StackWalker$Option
WARN <clinit> failed — wrapping in ExceptionInInitializerError class=java/lang/StackWalker cause=java/lang/ClassCastException class java.lang.StackWalker$Option not an enum
```

`java.lang.StackWalker`'s own `<clinit>` calls `EnumSet.noneOf(Option.class)`
(building an empty default-options set) as essentially its first action.
Real `EnumSet.noneOf` → `getUniverse` → `SharedSecrets.getJavaLangAccess()
.getEnumConstantsShared(Option.class)` → our
`native_class_get_enum_constants` (`native-builtins/src/lang_class.rs`).
That function does call `ctx.ensure_class_initialized_with_class_id(class_id)`
for `Option` before reading its `$VALUES` static field — but by the time
it reads the field, `$VALUES` is still null/non-object, so it falls into
the `values_val` mismatch branch, returns `null`, and real `noneOf()`
throws `ClassCastException` (`universe == null` branch), which propagates
out of `StackWalker.<clinit>` as `ExceptionInInitializerError`.

## Root-cause hypothesis (not investigated further this session)

`StackWalker$Option` is a nested enum whose `<clinit>` is being forced
*from within* `StackWalker`'s own `<clinit>` (i.e. `Option`'s
initialization is nested inside its enclosing class's initialization,
same thread). Two candidate explanations, neither confirmed:

1. `ensure_class_initialized_with_class_id` for `Option` returns before
   `Option`'s `<clinit>` (which populates `$VALUES`) has actually
   completed — e.g. if the VM's class-init-in-progress tracking is scoped
   too coarsely (per-thread rather than per-thread-per-class), a nested
   initialization of a *different* class than the one currently
   initializing on this thread could be incorrectly short-circuited as
   "already in progress" and skipped/no-op'd.
2. Something about `Option` being a `static enum` nested inside an
   interface/abstract-class host (`StackWalker`) trips a different
   class-loading code path than top-level or simply-nested enums — the
   `EnumSetProbe`-style standalone repro (top-level `enum Color`) and
   Log4j's `StandardLevel` (also apparently top-level or simple nested,
   now fixed) both work fine post-EnumSet-fix; only this
   during-enclosing-clinit case fails.

## Impact

Confirmed to block at least 2 sampled ES test classes
(`org.elasticsearch.index.mapper.UpdateMappingTests`,
`org.elasticsearch.index.query.CombineIntervalsSourceProviderTests`) via
`LuceneTestCase.<clinit>` → `TestSecrets.ensureCaller` →
`StackWalker.<clinit>`. Given `LuceneTestCase` is presumably the base (or
near-base) class for the overwhelming majority of ES/Lucene test classes,
this is likely close to universal for the suite, same blast radius as the
EnumSet bug it replaces. **Does NOT block the Tomcat
`TestWsRemoteEndpointImplServerDeadlock` repro** — confirmed via a full
run log showing zero `StackWalker`/`EnumSet` warnings — so it did not
block the websocket close-delay investigation.

## Suggested next step

Add a temporary debug print in `native_class_get_enum_constants`
(`native-builtins/src/lang_class.rs`) right after
`ensure_class_initialized_with_class_id(class_id)` returns, showing the
`Result` and immediately re-reading `$VALUES` at that point, to confirm
whether `Option`'s `<clinit>` actually ran (and if so, whether it wrote
`$VALUES` to a *different* field index than what
`static_field_index_by_name` resolves — a possible field-index mismatch
like the one found in the original EnumSet bridge — vs. whether
`ensure_class_initialized_with_class_id` returned without truly running
`Option`'s `<clinit>` at all (re-entrancy/short-circuit bug in class-init
tracking, in `vm/src/vm/vm_init.rs` or `classloading/src/class_manager.rs`).
