# HIB-CV-12 — `LongStream.mapToObj` dispatches to the abstract interface method → `AbstractMethodError`

**Severity:** Low/Medium — fails any code path that calls `LongStream.mapToObj(...)`.
**Status:** ✅ FIXED (worktree `fix/hibernate-full-suite`, `native-collections/src/lib.rs` `register_long_stream_natives`/`register_double_stream_natives`). Registered `mapToObj` in the real-JDK-active path (the synthetic-jdk-only `register_phase56_stream_extras` is compiled out under `--java-home`). Verified `LeakingStatementCachingTest` ok=1.
**Mode:** Interpreter (JIT-off census) — not a JIT bug.
**HotSpot:** not affected.

## Symptom

```
java.lang.AbstractMethodError: method java/util/stream/LongStream.mapToObj
  (Ljava/util/function/LongFunction;)Ljava/util/stream/Stream; has no Code attribute
   at …SessionFactoryScopeImpl.inTransaction(SessionFactoryExtension.java:371)
```

Witness: `org.hibernate.orm.test.batch.LeakingStatementCachingTest` (its teardown path uses a
`LongStream.mapToObj`).

## Root cause (mechanism)

`java.util.stream.LongStream.mapToObj(LongFunction)` is an **abstract interface method** (no `Code`
attribute); the concrete implementation lives on `java.util.stream.LongPipeline`. CratonVM's virtual
dispatch on the concrete `LongStream` instance resolved to the **abstract** interface method instead
of `LongPipeline.mapToObj`, so invoking it throws `AbstractMethodError: … has no Code attribute`.

This is the `LongStream`/`IntStream`/`DoubleStream` primitive-stream analogue of CratonVM's
synthetic-stream handling: a primitive-stream operation is not routed to the real pipeline override
(or the synthetic primitive-stream object lacks a working `mapToObj`), so the call lands on the
declaring interface's bodiless method.

## Suspected area / next step

CratonVM's stream natives / primitive-stream dispatch (`native-builtins` streams). Ensure
`LongStream.mapToObj` (and `IntStream.mapToObj`, `DoubleStream.mapToObj`) resolve to the concrete
`LongPipeline`/`IntPipeline`/`DoublePipeline` implementation (or a registered native), not the
abstract interface method. Minimal repro: `LongStream.range(0,3).mapToObj(Long::valueOf).count()`.
