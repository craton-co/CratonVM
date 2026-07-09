# JettyClientHttpRequestFactoryTests HttpExchange null snapshot - FIXED

## Status

FIXED on 2026-07-08 in `native-collections/src/lib.rs`.

## Symptom

`org.springframework.http.client.JettyClientHttpRequestFactoryTests` failed during
`HttpClient.stop()` cleanup with:

```text
java.lang.NullPointerException: Cannot invoke "org.eclipse.jetty.client.transport.HttpExchange.getRequest()" because "exchange" is null
```

The failing methods were `headersAfterExecute()`, `status()`, `echo()`,
`multipleWrites()`, and `queryParameters()`. The thrown frame was
`org.eclipse.jetty.client.transport.HttpDestination.abortExchanges()`, line 539,
while iterating `new ArrayList<>(exchanges)`.

This retired the stale open-bug note at
`docs/known-issues/jetty-clienthttprequestfactory-httpexchange-getrequest-npe.md`.
The symptom was related to, but distinct from, the earlier Jetty
`InputStreamResponseListener$Input` fallback-dispatch stack overflow and the
separate NIO SocketChannel/selector residual.

## Root Cause

Jetty 12.1.10 stores destination exchanges in
`org.eclipse.jetty.util.BlockingArrayQueue`, a real `AbstractList` /
`BlockingQueue`. CratonVM's generic collection copier had an ungated
`java/util/Arrays$ArrayList` heuristic: it resolved the `Arrays$ArrayList.a`
field slot and treated any array found at that slot as the collection backing.

For `BlockingArrayQueue`, that slot aliases `_indexes:int[]`, not the real
`_elements:Object[]`. Copying those primitive int slots into the new
`ArrayList`'s `Object[]` snapshot produced null entries, so Jetty later read a
null `HttpExchange` from the cleanup snapshot.

## Fix

`collect_collection_elements()` now:

- snapshots `org/eclipse/jetty/util/BlockingArrayQueue` through its real
  `size()` and indexed `get(int)` list contract;
- gates the `Arrays$ArrayList` array-slot heuristic so it only applies to actual
  `java/util/Arrays$ArrayList` receivers.

## Verification

Baseline binary:
`/data/data/bin/cratonvm-jetty-exchange-npe-20260708-baseline`

Fixed binary:
`/data/data/bin/cratonvm-jetty-exchange-npe-20260708-fix1`

Targeted Spring class:

```text
org.springframework.http.client.JettyClientHttpRequestFactoryTests  OK  found=6 succ=6 fail=0
```

Native collections tests:

```text
cargo test -p cratonvm-native-collections
all tests passed
```
