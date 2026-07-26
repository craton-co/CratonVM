# Keycloak JUnit 5: synthetic AnonymousObject IntStream.anyMatch missing

Status: FIXED (2026-07-04, branch `fix/keycloak-junit-anonymousobject-anymatch-20260704`)

Date observed: 2026-07-03

## Summary

After the PreviewFeatures native fix, 64 `tests/base` rows failed during JUnit
discovery with a CratonVM synthetic class method-resolution error:

```text
NoSuchMethodError method="cratonvm/synthetic/AnonymousObject$1.anyMatch(Ljava/util/function/IntPredicate;)Z [class not found on any classpath entry - synthetic stub, add the missing jar]"
caller="org/junit/platform/commons/util/StringUtils.containsWhitespace(Ljava/lang/String;)Z @pc=18"
```

The runner recorded these as `FAIL` because JUnit exits normally with discovery
issues rather than CratonVM aborting the process.

## Evidence

Run:

```text
craton-azure-nonpassed-dev-20260703-previewfeatures-fixed-01
```

Count:

```text
64 FAIL rows
```

Representative log:

```text
/home/victor/wt-keycloak-previewfeatures-suite-20260703-01/apps/keycloak-suite-runner/.suite/results/craton-azure-nonpassed-dev-20260703-previewfeatures-fixed-01/others-jit/logs/tests_base.org.keycloak.tests.actions.RequiredActionUpdateProfileTest.err.log
```

Excerpt:

```text
NoSuchMethodError method="cratonvm/synthetic/AnonymousObject$1.anyMatch(Ljava/util/function/IntPredicate;)Z [class not found on any classpath entry - synthetic stub, add the missing jar]"
caller="org/junit/platform/commons/util/StringUtils.containsWhitespace(Ljava/lang/String;)Z @pc=18"
[cratonvm] System.exit(1) called - process terminating
```

## Root cause (CONFIRMED, not just IntStream.anyMatch)

`java/lang/String.chars()` (and `codePoints()`, which delegates to the same
native) is implemented by `native_string_chars` in
`../../../../native-builtins/src/lang_string.rs`. It built the returned IntStream object
with:

```rust
let stream = ctx.alloc_object(ClassId::new(0), 1);
```

`ClassId::new(0)` is the "class resolution failed" sentinel. `alloc_object`'s
defense-in-depth guard (`../../../../vm/src/vm/vm_exec.rs`, `fn alloc_object`) treats any
`ClassId::new(0)` allocation with `num_fields > 0` as an undersized-`Object`
hazard and silently substitutes a generic, shared
`cratonvm/synthetic/AnonymousObject$1` placeholder class instead of the
intended `java/util/stream/IntStream` interface stamp — this is the *correct*
behavior for genuinely-anonymous internal bookkeeping objects (HashMap nodes,
view backings, etc., see the cache at `shared.anon_class_cache`), but here it
was masking a real bug: `native_string_chars` never actually resolved/stamped
the object as `IntStream` at all.

Every other stream native in this codebase (see
`native-collections/src/lib.rs::register_int_stream_natives` — `anyMatch`,
`allMatch`, `noneMatch`, `count`, `filter`, `map`, `mapToObj`, `boxed`, `max`,
`toArray`, ...) is registered keyed to the literal class name
`java/util/stream/IntStream`. Because the object returned by `chars()` was
stamped as `AnonymousObject$1` instead, **every** IntStream method call on a
`chars()`/`codePoints()` result failed with `NoSuchMethodError` — not just
`anyMatch`. The `is_synthetic_stub` flag that produces the "add the missing
jar" hint is shared by both the "genuinely missing classpath jar" case and the
"generic `ensure_synthetic_class` placeholder" case, which is why the message
was misleading.

Confirmed empirically with `CRATONVM_DBG_ANONALLOC=1`: the allocation stack for
`"a b".chars().anyMatch(...)` bottoms out directly at `native_string_chars`
(no intermediate Java frames — natives don't push interpreter frames), ruling
out the initial hypothesis (a missing `StreamSupport.intStream(Supplier,int,
boolean)` lazy-overload registration causing a fallthrough to real
`IntPipeline$Head` construction). `String.chars()` is fully native-intercepted
in default real-JDK mode via `register_wrapper_natives`
(`../../../../native-builtins/src/lang_math.rs`), so it never reaches that bytecode path.

## Fix

`native-builtins/src/lang_string.rs::native_string_chars` now allocates via
the same helper every other stream-producing native uses:

```rust
let stream = alloc_concurrent_synthetic(ctx, "java/util/stream/IntStream", 1);
```

This resolves/initializes the real `java/util/stream/IntStream` interface
class and stamps the object with it, so the existing `IntStream`-keyed native
registrations dispatch correctly. `codePoints()` shares the same
implementation and is fixed identically.

## Verification

Minimal probe mirroring the real JUnit method
(`org.junit.platform.commons.util.StringUtils.containsWhitespace`) plus a
broader IntStream surface (`count`, `filter`, `anyMatch`, `allMatch`,
`noneMatch`, `mapToObj().collect(joining())`, `max`, `boxed`, `toArray`,
`codePoints()`), compared against real HotSpot (JDK 25):

- Baseline (pre-fix) binary: `NoSuchMethodError` on `AnonymousObject$1.count()J`
  (and every other IntStream op) — confirms the bug was not limited to
  `anyMatch`.
- Fixed binary: output matches HotSpot exactly across every op; no
  `AnonymousObject` allocations for this path under `CRATONVM_DBG_ANONALLOC=1`.

The actual failing keycloak `tests/base` module sources were not present in
the local checkout (`../../../../apps/keycloak-26.6.3`) to re-run the original 64 tests
directly; verification relied on a faithful standalone reproduction of the
exact JUnit code path instead (recommended in this doc's original "Next
steps").

## Note for future work

`ClassId::new(0)` passed directly to `alloc_object`/`ctx.alloc_object` (rather
than through `alloc_concurrent_synthetic`/`ensure_class_initialized`) is a
legitimate, intentional idiom for objects that are *purely internal*
bookkeeping (never dispatched to by declared Java class/interface name — e.g.
HashMap linked nodes, ConcurrentHashMap segments). It is a bug whenever the
resulting object is later used as the receiver of a virtual/interface method
call keyed by a specific class name (as with any public API return value).
A handful of other `ctx.alloc_object(ClassId::new(0), N)` call sites exist in
`native-builtins`/`native-collections` (Field/Module mirrors in
`lang_class.rs`, `MethodHandles.Lookup` in `classloader.rs`, HTTP
request/response objects) that were not audited here — worth a systematic
pass if similar `NoSuchMethodError`-on-`AnonymousObject$N` reports recur for
other public API surfaces.
