# keycloak #16 — `java.util.stream` native shim: ~~`onClose`/`close` no-ops~~ (Part A ✅ FIXED) + eager intermediate ops (Part B OPEN)

**Status:** **Part A (close handlers) ✅ FIXED** (2026-06-18, branch `fix/keycloak-16-stream-close`,
`native-collections/src/lib.rs`). **Part B (laziness/short-circuit) remains OPEN** (architectural).

**Part A fix (verified):** synthetic streams now carry their `onClose` `Runnable`s in a new field
(`STREAM_FIELD_CLOSE_HANDLERS`); `close()` runs them once; intermediate ops propagate handlers
(`make_derived_stream`); `flatMap` closes each mapped inner stream and `concat` merges both inputs'
handlers (JDK contract). `onClose`/`close` registered to win over the `streams.rs` no-op stubs.
Verified by the pure-JDK 6-way `repros/keycloak-16-stream-onclose/StreamClose2.java` (all PASS;
matches HotSpot) and no regression in a full stream-ops sanity vs HotSpot. The 6 close-handler tests
(`testAutoClosingOfClosingStream*`, `testMultipleClosingHandlersOnClosingStream`) should now pass;
the 2 laziness tests still fail (Part B).

**Affected (CV-only, HotSpot passes):**
- `org.keycloak.utils.StreamsUtilTest` (server-spi-private) — was 8/8 fail; **Part A fix clears the 6
  close-handler tests**, leaving the 2 Part-B laziness tests (`testLimitOnClosingStream`,
  `testSortedInsideOfFlatMapShouldRespectTerminalOperation`).

## Symptom
```
java.lang.AssertionError: expected:<1> but was:<3>
java.lang.AssertionError: expected:<1> but was:<0>
```

## Root cause (confirmed; `--nojit` identical → native-shim, not JIT)
CratonVM does **not** run the real JDK stream pipeline. It intercepts
`java.util.stream.Stream` with an **eager, materialize-everything** native shim in
`native-collections/src/lib.rs` (`register_stream_natives`, ~line 8610). A synthetic
Stream is a flat `Object[]` of elements (one field, `STREAM_FIELD_ELEMENTS = 0`).
Two independent defects:

### Defect A — `onClose`/`close` are no-ops (6 tests)
- `onClose(Runnable)` → `native_stream_return_this` (`native-builtins/src/streams.rs:156`)
  returns `this` and **discards** the Runnable.
- `close()` → `close_noop` (`native-collections/src/lib.rs` ~8732) is a hard no-op.

So registered close handlers are never stored or run. Keycloak's `ClosingStream.forEach`
correctly calls `delegate.close()`, but the native `close()` does nothing.
Standalone (`scratch/streams/S2.java`): HotSpot `after close=true`; CV `after close=false`.

Breaks: `testAutoClosingOfClosingStream{,Outer,FlatMap,UsingIterator,UsingConcat}` and
`testMultipleClosingHandlersOnClosingStream`.

### Defect B — eager intermediate ops break short-circuit ordering (2 tests)
`native_stream_peek` (~9415), `map`/`filter`/`flatMap` fully materialize at
construction; `native_stream_limit` (~9385) then `.take(n)` on the already-built
list. Real JDK is lazy: `limit(1)` pulls only 1 element through `peek`, so the
counter is 1; CV peeks all elements first.
Standalone (`scratch/streams/S1.java`): HotSpot `peek-count=1`; CV `peek-count=3`.

Breaks: `testLimitOnClosingStream`, `testSortedInsideOfFlatMapShouldRespectTerminalOperation`.

## Fix direction
**Part A (recommended; closes 6 tests, low risk, no stubs).** Give the synthetic
stream a close-handler field and run handlers on `close()`:
- `STREAM_NUM_FIELDS: 1 → 2`, add `STREAM_FIELD_CLOSE_HANDLERS` (`Object[]` of
  `Runnable`, or null);
- propagate handlers across every intermediate op (`filter/map/flatMap/sorted/
  distinct/limit/skip/peek/mapToInt|Long|Double`) — copy `src`'s handler array onto
  the result stream;
- implement `onClose(Runnable)` (append to the handler array) and `close()` (invoke
  each `Runnable.run()`, then clear → run-once), registered in
  `register_stream_natives` so last-registration-wins **overrides** the streams.rs
  no-op (registration order in `vm/src/vm/vm_init.rs`: `register_builtins` then
  `register_collections_natives`). Mirror the same for the `IntStream`/`LongStream`/
  `DoubleStream` variants used by `testMultipleClosingHandlers` (`mapToInt`).

**Part B (architectural; the remaining 2 tests).** The `limit`/`peek` ordering
cannot be fixed within the materialize-everything model. Options: make the shim lazy
(represent a stream as source + ordered pending-op list, drive elements only on a
terminal op and stop early once `limit` is satisfied), **or** route
`java/util/stream/Stream` to real JDK `ReferencePipeline` bytecode behind a gate
(mirroring `CRATONVM_REAL_AQS` / `CRATONVM_REAL_NET_SOCKETS`). Substantial, with real
regression risk (streams are pervasive). Note `testSortedInsideOfFlatMap` only
asserts the JDK-24+ `prepareSorted` sub-case, which passes once laziness is correct.

## Repro
`scratch/streams/S1.java` (peek/limit laziness), `S2.java` (onClose/close):
```
& "C:/Program Files/Java/jdk-25/bin/java.exe" S1     # HotSpot
& cratonvm-kcfull.exe --java-home <jdk> -cp . S1     # CV (add --nojit to confirm native-shim)
```
Full test: `powershell -File apps/keycloak/repro-kc.ps1 org.keycloak.utils.StreamsUtilTest`
