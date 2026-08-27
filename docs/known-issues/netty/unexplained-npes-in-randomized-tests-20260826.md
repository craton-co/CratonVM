# Three `NullPointerException`s in randomized/property tests — not yet explained, not yet cross-checked against HotSpot

## Status
**OPEN, not root-caused.** New this session (2026-08-26 complete-suite ZGC×4-shard
run, `dev` HEAD `3c09f9d93`). No existing doc anywhere in the repo mentions any of
these three classes.

## Why these three are grouped together

All three are randomized/property-style tests (a loop of random operations
checked against an oracle, or a fuzz-shaped stress loop), and all three throw a
bare `NullPointerException` with no message, directly at an AssertJ
`assertThat(...)` chain call site in the test's own body — not inside any
netty or CratonVM production code visible in the trace. That shape is
consistent with (a) a real divergence between the code under test and its
oracle that only an unboxed-null comparison surfaces, (b) AssertJ's own
handling of a null actual/expected value in a chain that doesn't expect one, or
(c) something upstream returning null where the test assumes non-null. Not
distinguished yet.

## 1. `io.netty.util.internal.LongLongHashMapTest.randomOperations`

```
java.lang.NullPointerException
	at io.netty.util.internal.LongLongHashMapTest.randomOperations(LongLongHashMapTest.java:80)
```

Line 80, in context:
```java
} else {
    long v = expected.get(value);
    assertThat(actual.put(value, -v)).isEqualTo(expected.put(value, -v));   // :80
}
```
`actual` is netty's own primitive-specialized `LongLongHashMap` (the class under
test); `expected` is a plain `java.util.HashMap<Long, Long>` used as the
correctness oracle. `Map.put()` returns the previous value or `null` if the key
was absent. If `actual` and `expected` have diverged — `actual` believes `value`
is already a key (hence taking this branch) while `expected` does not —
`expected.put(value, -v)` returns `null`, and `AbstractLongAssert.isEqualTo`
resolving to the primitive-`long` overload would NPE unboxing that null. That
would mean the two maps' key-sets have genuinely diverged, i.e. a real
`LongLongHashMap` correctness bug — but this has not been confirmed; the
alternative (a harmless AssertJ overload-resolution artifact) has not been
ruled out either.

## 2. `io.netty.buffer.ByteBufDerivationTest.testMixture`

```
java.lang.NullPointerException
	at io.netty.buffer.ByteBufDerivationTest.testMixture(ByteBufDerivationTest.java:197)
```

Line 197: `assertThat(nestLevel(newDerived)).isLessThanOrEqualTo(3);`, inside a
loop that randomly re-derives a `ByteBuf` via slice/duplicate/`order()`/
`asReadOnly()`/`unmodifiableBuffer()` each iteration. `nestLevel` is a test
helper that presumably walks the derived buffer's unwrap chain — an NPE here
could mean an unwrap chain terminated in `null` unexpectedly for some specific
derivation combination the randomizer picked.

## 3. `io.netty.resolver.dns.DnsQueryContextTest.writeQueryMustNotSendWhenIdSpaceExhausted`

```
java.lang.NullPointerException
	at io.netty.resolver.dns.DnsQueryContextTest.writeQueryMustNotSendWhenIdSpaceExhausted(DnsQueryContextTest.java:104)
```

This test drains all 65,536 IDs from a `DnsQueryIdSpace`/query-context manager
in a loop, then asserts the next `add()` returns -1 (space exhausted). Line 104
is inside that drain loop. An NPE while exhausting a large ID space is at least
plausible as an internal array-resize or table-growth bug in whatever backs the
ID-to-context map at scale — but equally plausible as a pre-existing netty test
issue unrelated to CratonVM. Not cross-checked against HotSpot.

## Possibly related, uncertain

`io.netty.handler.proxy.ProxyHandlerTest` failed 8 of 47 parameterizations this
same run with `array lengths differ, expected: <0> but was: <1>` (one extra
byte arrived in an AUTO_READ success-path check) — not an NPE, and plausibly
just contention (see `timing-margin-fails-under-4shard-zgc-self-contention-20260826.md`),
but noted here in case a future investigation of the NPEs above turns up a
shared root cause with unexpected extra/missing data.

## Not yet done
- No isolated single-fork rerun of any of the three (all three occurred inside
  a heavily-loaded 4-shard concurrent run — see the timing-margin page above
  for this run's contention context; these three are called out separately
  *because* an NPE at an assertion call site is a different, less
  contention-explicable shape than a timeout or connection-refused).
- No HotSpot cross-check.
- No `-Dcraton.batch=1` isolated single-class repro attempted yet.

## Repro

```bash
cd apps/netty-suite-runner
cratonvm.exe --java-home <jdk25> --Xmx 1500m -XX:+UseZGC @common.args -Dcraton.batch=1 \
  CratonRunner io.netty.util.internal.LongLongHashMapTest
# similarly for io.netty.buffer.ByteBufDerivationTest and io.netty.resolver.dns.DnsQueryContextTest
```
