# RandomizedContext.current() returned null instead of throwing — FIXED

Status: FIXED (worktree `fix/es-vector-codec-exception-cause-object-20260710`)

## Regression

Dev commit `4978c5d5c` ("Speed up Elasticsearch sliced IVF native paths")
rewrote `com.carrotsearch.randomizedtesting.RandomizedContext.current()` /
`.context(Thread)` as a CratonVM native (`native_randomized_context_current`
/ `native_randomized_context_context`, backed by
`randomized_context_for_thread` in `native-builtins/src/lib.rs`) for
performance. Before this commit `current()`/`context()` ran as real JDK
bytecode.

Real-JDK `RandomizedContext.context(Thread)` (decompiled from
`randomizedtesting-runner-2.8.2.jar`) never returns `null`: if the thread's
`ThreadGroup` chain has no registered context, it throws
`IllegalStateException` with a message ending "... static test class
initializers are not permitted to access random contexts." The new native
reimplementation instead returned `Ok(Some(Value::Object(None)))` (a plain
`null`) in all three "not found" branches.

## Symptom

`org.apache.lucene.tests.codecs.asserting.AssertingCodec`'s constructor
relies on the documented throwing behaviour:

```java
Class<?> targetClass;
try {
    targetClass = RandomizedContext.current().getTargetClass();
} catch (IllegalStateException e) {
    targetClass = null;
}
```

(bytecode-verified: an exception-table entry from `RandomizedContext.current()`
through `getTargetClass()` catching `java.lang.IllegalStateException` only).
With `current()` returning `null` instead of throwing, `.getTargetClass()`
dispatches on a null receiver -> `NullPointerException`, a different
exception type the `catch (IllegalStateException)` block does not match.
The NPE propagates out of `AssertingCodec.<init>`, out of the enclosing
static initializer, and becomes an uncaught `ExceptionInInitializerError` —
which happens whenever a test class's own `<clinit>` builds a
Lucene-test-framework `AssertingCodec` (extremely common; e.g. any
`org.apache.lucene.tests.util.TestUtil.alwaysKnnVectorsFormat(...)` call in
a static field initializer), which is BEFORE the `RandomizedRunner` has
pushed a context for the running thread — a documented, intentional,
tolerated case.

Verified deterministic: 3/3 failing runs on the regressed binary, 3/3
passing runs on the pre-regression binary
(`cratonvm-es-full-local-20260710-083851.exe`, commit `21c8755ad`), same
seed (`B17AC9D3E1F2A0C4`), same test class
(`org.elasticsearch.index.codec.vectors.ES815BitFlatVectorFormatTests`).
Blocked essentially the entire family in
`ES-FAIL-FAMILY-20260710-vector-codec-exception-cause-object.md` from being
re-tested at all (class never finished loading).

## Fix

`native-builtins/src/lib.rs`, `randomized_context_for_thread`: the three
"no context found" branches (terminated thread with no `ThreadGroup`,
`RandomizedContext.contexts` static field unavailable, and walking the
`ThreadGroup` parent chain to `null` without a match) now throw
`RuntimeError::IllegalStateException` with a message mirroring the real
bytecode's, instead of returning `Ok(Some(Value::Object(None)))`. Added
`randomized_no_context_error`/`randomized_thread_name` helpers. All internal
callers (`native_randomized_context_current`, `native_randomized_context_context`,
`native_randomized_test_get_context`, `native_randomized_test_get_random`,
`native_randomized_test_random_float`) already propagate `MethodCallResult`
errors via `?`/direct return, so the exception now surfaces correctly to
Java catch blocks without any other call-site changes.

## Verification

- `ES815BitFlatVectorFormatTests`: 3/3 `ExceptionInInitializerError` on
  regressed binary -> 5/5 clean runs (reaching `testRandom`) on the fixed
  binary.
- `ES93HnswBFloat16VectorsFormatTests`, `ES93FlatBFloat16VectorFormatTests`:
  both now load and run past `<clinit>` on the fixed binary (see
  `ES-FAIL-FAMILY-20260710-vector-codec-exception-cause-object.md` for their
  individual pass/fail status — this fix only restores their ability to run,
  it does not change `ES93FlatBFloat16VectorFormatTests`'s separate,
  still-open `testMultiClose` residual).
