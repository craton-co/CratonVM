# `TestIndex` and `TestMVStore`: two divergences unmasked by the 2026-08-02 JIT div-guard fix

## Status
**OPEN**, both. Neither is new code — both were sitting behind a hard
`InternalError` that killed the class before it reached them. Fixing that
(`fix/h2-testdiskfull-livelock-20260802`; see the retired
`unresumable-unconditional-trap-mvmap-20260802` write-up) let both classes run
far enough to fail on their own merits.

Recorded so the change that revealed them is not mistaken for the change that
caused them.

## What changed

`origin/dev@86a01abf90`, both classes, in isolation, `--java-home
/home/victor/jdk25 --Xmx 1g`, 600 s:

| class | before | after |
| --- | --- | --- |
| `org.h2.test.db.TestIndex` | `InternalError: … refusing side-effecting replay` ×2 | `AssertionError` in `testFunctionIndex` |
| `org.h2.test.store.TestMVStore` | `InternalError: … refusing side-effecting replay` ×2 | `UnsupportedOperationException: remove` in `testIterate` |

Both now report **zero** `refusing side-effecting replay`.

## 1. `TestIndex.testFunctionIndex` — `Expected: 1 actual: 0`

```
java/lang/AssertionError: Expected: 1 actual: 0
  at org/h2/test/TestBase.assertEquals(TestBase.java:506)
  at org/h2/test/db/TestIndex.testFunctionIndex(TestIndex.java:758)
```

**Stock HotSpot passes the whole class** (`rc=0`, same classpath, same JDK), so
this is a CratonVM divergence, not upstream flakiness. `testFunctionIndex`
exercises a table function used as an index source; a row count of 0 where 1 is
expected points at the function-table read path rather than at indexing.

## 2. `TestMVStore.testIterate` — `UnsupportedOperationException: remove`

```
java/lang/UnsupportedOperationException: remove
  at org/h2/test/TestBase$1.invoke(TestBase.java:1546)
  at org/h2/test/store/TestMVStore.testIterate(TestMVStore.java:1939)
```

`TestBase$1` is a `java.lang.reflect.Proxy` invocation handler, so the throw
escapes *through* a dynamic proxy. H2's cursor iterator throws
`UnsupportedOperationException` from `remove()` by design and the test expects
to observe it; the failure is that it propagates out of `test()` instead of
being caught where the test catches it.

**HotSpot also fails this class**, but *earlier and elsewhere* — `testCacheSize`
(line 85) with `Cache 1Mb, reads: 2800 expected: 1750`, an upstream cache-ratio
assertion that is sensitive to timing. CratonVM gets past `testCacheSize` and
dies at `testIterate` (line 112), which HotSpot never reaches. So the two are
different failures and this one still needs explaining on its own.

## Reproducing

```bash
H2=<apps/h2database/h2>
CP="$H2/target/classes:$H2/target/test-classes:$(cat $H2/craton-testcp.txt)"
<cratonvm> --java-home <jdk25> --Xmx 1g -c "$CP" org.h2.test.db.TestIndex
<cratonvm> --java-home <jdk25> --Xmx 1g -c "$CP" org.h2.test.store.TestMVStore
```

Both are deterministic in isolation and need no fault injection. Run the same
classpath under `$JAVA_HOME/bin/java -cp "$CP"` for the control — `TestIndex`
passes there, which is what makes it the cheaper of the two to chase first.

Note both classes need a *fresh working directory* per run (H2 writes `./data`),
and both take minutes, so a 180 s class timeout records them as `TIMEOUT` rather
than as the failures above.
