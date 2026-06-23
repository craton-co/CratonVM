# 09 — StreamsUtil: `onClose` / auto-close handler propagation wrong

**Status:** open (java.util.stream pipeline semantics)
**Affected:** StreamsUtilTest (`testAutoClosingOfClosingStreamFlatMap`,
`testMultipleClosingHandlersOnClosingStream`, `testAutoClosingOfClosingUsingConcat`,
`testAutoClosingOfClosingUsingIterator`, `testLimitOnClosingStream`)
**Symptoms:** `AssertionError: expected:<1> but was:<3>` and `expected:<1> but was:<0>`
— a close handler fires the wrong number of times (3× or 0× instead of once).

## Analysis
`StreamsUtil.closing(stream)` wraps a stream so terminal ops auto-`close()` it, running
the registered `onClose` handlers exactly once. CratonVM's `java.util.stream` pipeline
mis-handles close-handler composition:
- `was:<3>` → a handler runs multiple times (close called repeatedly, or
  `Streams.composeWithExceptions` chaining duplicated).
- `was:<0>` → close not propagated at all (flatMap inner stream, `Stream.concat`, and
  `.iterator()` paths don't trigger the source's onClose).

These are pipeline `close()`/`onClose` semantics — likely real `AbstractPipeline`
bytecode running in the interpreter producing wrong control flow, or a native stream
intercept that drops/duplicates close handlers. Distinct from the crypto issues.

## Isolation result (apps/probe/kcstream/StreamClose.java)
A minimal probe shows that plain `Stream.of(..).onClose(h).forEach(..)` does **not**
auto-close on **either** VM (correct JDK behavior — `forEach` is not try-with-resources;
keycloak's `StreamsUtil.closing()` adds the close-on-terminal via a custom spliterator).
The **only** CV-vs-HotSpot divergence the probe found is:

```
T2  Stream.of("v").flatMap(v -> Stream.of(1,2,3).onClose(h)).forEach(..)
    HotSpot : inner onClose fires (true)
    CratonVM: inner onClose does NOT fire (false)
```

## True root cause
`native-builtins/src/streams.rs:129` is explicit: **"`onClose(Runnable)` returns `this`
(we don't track close handlers)."** CratonVM's native stream layer registers `onClose`
as a no-op (`native_stream_return_this`) and `close()` does nothing — and these natives
**shadow the real `java.util.stream` bytecode even in real-JDK mode**. So no registered
close handler ever runs (the probe's T2 differs from HotSpot only because the JDK's
flatMap would have run the inner stream's real handler — which CratonVM never recorded).

This is therefore not a one-line fix but a **stream-feature addition**: track the
`Runnable` close handlers per pipeline stage, compose them across `flatMap`/`concat`/
intermediate ops, and run them on `close()` / terminal-op completion for `closing()`-style
streams. Given streams are pervasive and the natives are load-bearing, this carries real
regression risk and was scoped out of this pass. Fix locus: `native-builtins/src/streams.rs`
(`onClose`, `close`, flatMap/concat handler composition).
