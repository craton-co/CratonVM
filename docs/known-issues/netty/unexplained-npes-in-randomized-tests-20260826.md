# Three `NullPointerException`s in randomized/property tests — one confirmed real, two did not reproduce on a quiet host

## Status
**`LongLongHashMapTest.randomOperations` CONFIRMED real, moved to its own page:
the retired
`fixed-suite-bugs/netty/longlonghashmaptest-npe-spliced-ctor-this-not-a-gc-root-FIXED-20260828`
write-up — read that one for the current state. Kept here only for the other two
classes' history and this page's own paper trail. Not root-caused to
the exact mechanism. Reproduced identically on THREE separate runs now: the
original 2026-08-26 contended 4-shard run, an isolated quiet single-shard rerun
(2026-08-27), and a second quiet single-shard rerun after a fresh `dev` rebuild
(2026-08-27, wall wasn't even close — same NPE, same line). Direct HotSpot A/B,
same host, same classpath: **HotSpot `found=3 ok=3 failed=0` — clean pass, every
time.** `ByteBufDerivationTest` and `DnsQueryContextTest` (below) did NOT
reproduce on either quiet rerun — most likely they were contention/random-seed
noise from the original run, not a stable defect. Original report and analysis
follows; the class-under-test analysis for `LongLongHashMapTest` was WRONG (see
correction below the class's original section) — keeping the original text
since the correction is the useful part.

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

### Correction (2026-08-27): wrong line, and the class under test cannot NPE at all

The actual failing line, confirmed from two fresh reproductions, is
`LongLongHashMapTest.java:80/81` — the OTHER branch (`!expected.containsKey(value)`):

```java
} else {
    assertThat(actual.get(value)).isEqualTo(-1);        // :80
    assertThat(actual.put(value, value)).isEqualTo(-1); // :81
    expected.put(value, value);
}
```

`LongLongHashMap` (`common/src/main/java/io/netty/util/internal/LongLongHashMap.java`)
is a pure open-addressing `long[]`-backed map: `get`/`put`/`remove`/`index` touch
only primitive `long` locals and one `long[]` field, never a reference type,
never anything that can be `null`. **There is no expression in this class or
this call site that can produce a `null` in ordinary Java semantics.** Whatever
throws the NPE is happening inside CratonVM's own execution of this hot loop
(6000 × 50 = 300,000 iterations, `assertThat(long)`-heavy, running long enough
to get JIT-compiled) or inside AssertJ's own generic/overload-resolution
machinery as CratonVM executes it — not inside the map implementation itself.
Given how many raw logs this session show CratonVM's own GC guards firing on
reference/primitive type-confusion in JIT-compiled code
(`cratonvm::gc::guard`: "a non-reference value was stored into a slot the class
declares as a REFERENCE", "a descriptor-aware field access DESTROYED the value
it was handed") — both on paths *with* a guard catching them — this NPE is a
plausible candidate for the same class of defect surfacing on a path *without*
a guard. Not confirmed; needs a targeted repro (isolate `assertThat(long
primitive).isEqualTo(int)` under heavy JIT compilation, off AssertJ, to see if
it's CratonVM's own dispatch or AssertJ's overload resolution that's wrong)
before this becomes more than an informed guess.

## 2. `io.netty.buffer.ByteBufDerivationTest.testMixture` — did NOT reproduce on a quiet host

Passed cleanly on both 2026-08-27 quiet single-shard reruns. Original report
kept below for reference; treat as probable contention/random-seed noise from
the original contended run unless it reappears.

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

## 3. `io.netty.resolver.dns.DnsQueryContextTest.writeQueryMustNotSendWhenIdSpaceExhausted` — did NOT reproduce on a quiet host

Also passed cleanly on both 2026-08-27 quiet reruns. Same caveat as above.

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
- The exact CratonVM mechanism for `LongLongHashMapTest` is still not found —
  only that it must be CratonVM's own execution (interpreter/JIT dispatch of
  the `assertThat(long)`/AssertJ chain under this hot loop), not the class
  under test. See the correction above for the specific next repro to try.
- `ByteBufDerivationTest`/`DnsQueryContextTest` not retried a third time; if
  either reappears, it stops being explainable as one-off noise.

## Repro

```bash
cd apps/netty-suite-runner
cratonvm.exe --java-home <jdk25> --Xmx 1500m -XX:+UseZGC @common.args -Dcraton.batch=1 \
  CratonRunner io.netty.util.internal.LongLongHashMapTest
# similarly for io.netty.buffer.ByteBufDerivationTest and io.netty.resolver.dns.DnsQueryContextTest
```
