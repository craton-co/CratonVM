# `LongLongHashMapTest.randomOperations` throws `NullPointerException` on CratonVM — the class under test cannot NPE, so this is CratonVM's own execution

## Status
**OPEN, confirmed real, not root-caused to the exact mechanism.**

## Confirmation

Reproduced identically on four separate runs: the original 2026-08-26 contended
4-shard ZGC run, an isolated quiet single-shard rerun (2026-08-27), a second
quiet single-shard rerun after a fresh `dev` rebuild (2026-08-27), and a direct
manual repro. Direct HotSpot A/B, same host, same classpath, same fixture:

```
$ java -cp <common.args classpath> -Dcraton.batch=1 CratonRunner io.netty.util.internal.LongLongHashMapTest
@@RESULT io.netty.util.internal.LongLongHashMapTest found=3 started=3 ok=3 failed=0 aborted=0 skipped=0 ms=1067
```

HotSpot: clean pass, every time. CratonVM: fails `randomOperations` every time.

## The failure

```
java.lang.NullPointerException
	at io.netty.util.internal.LongLongHashMapTest.randomOperations(LongLongHashMapTest.java:80)
```

`randomOperations` (`common/src/test/java/io/netty/util/internal/LongLongHashMapTest.java:60-85`)
runs 300,000 randomized put/get/remove operations against `LongLongHashMap`,
cross-checked against a plain `java.util.HashMap<Long,Long>` oracle. Line 80 is:

```java
} else {
    assertThat(actual.get(value)).isEqualTo(-1);        // :80 <- throws here
    assertThat(actual.put(value, value)).isEqualTo(-1); // :81
    expected.put(value, value);
}
```

## Why this cannot be a bug in the class under test

`LongLongHashMap` (`common/src/main/java/io/netty/util/internal/LongLongHashMap.java`)
is a pure open-addressing `long[]`-backed map:

```java
public final class LongLongHashMap {
    private long[] array;
    ...
    public long get(long key) {
        if (key == 0) return zeroVal;
        int index = index(key);
        for (int i = 0; i < maxProbe; i++) {
            long existing = array[index];
            if (existing == key) return array[index + 1];
            index = index + 2 & mask;
        }
        return emptyVal;
    }
    ...
}
```

`get`/`put`/`remove`/`index` touch only primitive `long` locals and one
`long[]` field. There is no reference type anywhere in the hot path, no boxing,
nothing that can hold or produce `null` in ordinary Java semantics. Whatever
value `actual.get(value)` returns, it is a primitive `long` — there is no
expression at this call site that Java's own semantics allow to NPE.

## Where the NPE must actually be coming from

Since the class under test cannot produce it, the NPE must originate in one of:

1. **CratonVM's own execution of the JIT-compiled hot loop.** 300,000
   iterations of `assertThat(long).isEqualTo(int)` is enough traffic to get
   `LongLongHashMap`'s methods and/or AssertJ's assertion chain JIT-compiled.
   Every raw log from this session's testing shows CratonVM's own
   `cratonvm::gc::guard` firing constantly on exactly this class of defect —
   type confusion between primitive and reference values in JIT-compiled code
   (`"a non-reference value was stored into a slot the class declares as a
   REFERENCE"`, `"a descriptor-aware field access DESTROYED the value it was
   handed"`) — on paths where a guard exists to catch it. This NPE is a
   plausible instance of the same underlying defect class surfacing on a path
   that has no guard yet.
2. **AssertJ's own generic/overload-resolution machinery**, as executed by
   CratonVM, resolving `assertThat(long)` differently than HotSpot does, or
   producing a null somewhere in its internal `Long`/comparison plumbing that
   HotSpot's identical bytecode does not.

Not distinguished between these yet.

## Not yet done

- No isolated repro that strips away AssertJ (call `LongLongHashMap.get`/`put`
  directly in a tight loop with a hand-written oracle check, no AssertJ) to
  determine whether AssertJ's dispatch is load-bearing for the failure or
  incidental.
- No `CRATONVM_DBG_LAYOUT=1` / `CRATONVM_DBG_COERCION=1` run to see if either
  debug guard fires anywhere near this NPE, which would directly tie it to the
  known type-confusion defect class instead of just resembling it.
- Not checked whether the failure is deterministic at a specific iteration
  count / specific random values (the test uses `ThreadLocalRandom.current()`,
  unseeded) or varies run to run — worth checking since a value- or
  count-dependent trigger would narrow the search a lot.

## Repro

```bash
cd apps/netty-suite-runner
cratonvm.exe --java-home <jdk25> --Xmx 1500m -XX:+UseZGC @common.args -Dcraton.batch=1 \
  CratonRunner io.netty.util.internal.LongLongHashMapTest
```

## Related
- `known-issues/netty/unexplained-npes-in-randomized-tests-20260826.md` — this
  class's original report, alongside two other NPEs (`ByteBufDerivationTest`,
  `DnsQueryContextTest`) that did NOT reproduce on a quiet host and are most
  likely unrelated one-off noise. This page supersedes that one for
  `LongLongHashMapTest` specifically.
