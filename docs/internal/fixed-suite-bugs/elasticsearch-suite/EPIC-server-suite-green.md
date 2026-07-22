# EPIC — Make the Elasticsearch `:server` unit suite run green on CratonVM

**Status:** OPEN (multi-fix epic). 4 layers fixed; chain continues.
**Scope:** Elasticsearch 9.5 `:server` unit tests (≈2477 classes) under CratonVM vs HotSpot.
**Date:** 2026-06-18
**Fix branch:** `fix/es-fail-04-arraylist-sublist-toarray` (worktree `C:\craton\CratonVM-jitfix`).

---

## Why this is an epic, not a single bug

Every ES `:server` test extends `ESTestCase` → `LuceneTestCase` and runs under
carrotsearch **randomizedtesting** + the **Lucene test framework**. That stack
exercises a very wide slice of JDK/VM surface during *suite setup alone*
(before any `@Test` method runs): native-access bootstrap, JMX/management
beans, `Throwable`/`StackTraceElement` machinery, randomized seeding, codec
SPI, mock filesystems, thread-leak control, etc.

CratonVM has an **independent gap at almost every one of those layers**, and
each one fails suite setup. Fixing one unmasks the next. So "get `:server`
green" is a *sequence* of unrelated VM fixes, not one root cause. Each fix
below is a real, general CratonVM correctness fix (not an ES-specific hack),
verified individually; each required a full release rebuild (~5–9 min) to
reach the next layer.

The single witness class used throughout is
`org.elasticsearch.common.unit.ByteSizeValueTests` (a trivial unit test);
the failures are all in the shared `ESTestCase`/RandomizedRunner setup path,
so they generalize to essentially the whole suite.

---

## Layers fixed (in discovery order)

| # | Bug | Symptom (HotSpot vs CratonVM) | Fix | Commit |
|---|-----|-------------------------------|-----|--------|
| 0 | **ES-HANG-01** — JIT `WeakHashMap$ValueSpliterator.tryAdvance` livelock | default-JIT hangs every `ESTestCase`; HotSpot fine | skip-list the WeakHashMap spliterator family | **already on dev** as `1cd0ab26` (= kafka-bug-C); confirmed independently |
| 1 | **ES-FAIL-04** — `ArrayList.subList().toArray(T[])` missing on the synthetic SubList | `NoSuchMethodError` | register the typed `toArray(T[])` snapshot delegation | `ec979daf` |
| 2 | **ES-FAIL-05** — `ThreadMXBean.isThreadContentionMonitoringSupported()/Enabled()` unregistered | `AbstractMethodError` ("no Code attribute"); `HotThreads.initializeRuntimeMonitoring()` calls it from `ESTestCase.<clinit>` | register both (return false) on both ThreadMXBean wiring paths | `8c190cba` |
| 3 | **ES-FAIL-06a** — JDK 25 `StackTraceElement.computeFormat()` NPEs on null `declaringClassObject` | `NPE: Cannot invoke getClassLoader0 on null` whenever a trace with a constructor-built element (RandomizedRunner's `SeedInfo` frame) is formatted | override `computeFormat` as a null-safe no-op; fall back to `Object`'s mirror when a frame class can't be resolved | `8512b3c0` |
| 4 | **ES-FAIL-06b** — `StackTraceElement.of` backfill / capture-key mismatch | `NPE: Cannot invoke startsWith on null` in `RandomizedRunner.seedFromThrowable`. Root: `getStackTraceDepth()` keys the trace on the *throwable*, but `initStackTraceElements` keys on the *`backtrace` arg* — for some throwables those disagree, so the array is sized N but 0 slots get filled (every element empty / null `declaringClass`) | backfill unpopulated slots with a non-null placeholder; honor an explicitly-set `stackTrace` field in the synthetic-mode native | `4185be7a` |

> **ES-FAIL-03 is RETRACTED** — the native-access `catch (LinkageError)` works
> fine; that was a misdiagnosis (see `ES-FAIL-03-RETRACTED-*`).

---

## Remaining chain (next items, observed but not yet fixed)

After layer 4, suite setup gets *further* and surfaces these (all visible in a
single instrumented run of the witness class). None are fixed yet:

1. **Stack-capture key mismatch (proper fix).** Layer 4 band-aids the symptom
   (empty trailing elements) with a placeholder. The proper fix is to make
   `getStackTraceDepth`, `initStackTraceElements`, and the capture/store in
   `fillInStackTrace` all key the stored trace on the **same** identity (the
   throwable, or consistently `backtrace == throwable` self-reference). Until
   then, some throwables silently lose their real stack trace (the placeholder
   shows `(unknown)` frames). General correctness bug, worth a standalone fix.

2. **`IllegalArgumentException: bound must be positive`** thrown during
   `Elasticsearch.initializeProbes()` → `ProcessProbe`/`OsProbe`/`JvmInfo`
   init. A `Random.nextInt(bound)` (or similar) is called with `bound ≤ 0`
   because some count/list CratonVM produces is empty/zero where HotSpot's is
   positive. `java.util.Random.nextInt` itself is byte-perfect on CratonVM, so
   the bad bound comes from a probe-init divergence; the exact source needs the
   throw-site (currently truncated by CratonVM's 8-frame `ATHROW` capture and
   the lost trace in item 1).

3. **`NoSuchMethodError: cratonvm/internal/UnmodifiableMap.floorEntry(Object)`**
   — the synthetic `Collections.unmodifiableNavigableMap`/`UnmodifiableMap`
   view is missing the `NavigableMap.floorEntry` (and likely sibling
   `ceilingEntry`/`higherEntry`/`lowerEntry`) overloads. Same class of gap as
   ES-FAIL-04.

4. **`IllegalArgumentException: "<x> is not a platform management interface"`**
   for `com.sun.management.HotSpotDiagnosticMXBean` — `ManagementFactory
   .getPlatformMXBean(...)`/`getPlatformManagementInterfaces()` doesn't include
   the `HotSpotDiagnosticMXBean` (and probably other `com.sun.management`
   interfaces) in CratonVM's platform-MXBean set.

Beyond suite setup, the 42 actual `ByteSizeValueTests` methods still have to
execute — likely surfacing further gaps (codec/Directory randomization, etc.).

---

## How to drive this forward

Witness + method (all under `apps/elasticsearch/cratonvm-suite/`):

```bash
FIX=C:/craton/CratonVM-jitfix/target/release/cratonvm.exe   # branch build
export CLASSPATH="<probe-dir>;$(cat server/build/craton-testcp.txt)"
export CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
# RunListen.java prints the full failure exception chain (JUnitCore swallows it):
"$FIX" --nojit -Dtests.asserts=false -Dtests.seed=B17AC9D3E1F2A0C4 \
  --add-opens=java.base/java.util=ALL-UNNAMED --enable-native-access=ALL-UNNAMED \
  RunListen org.elasticsearch.common.unit.ByteSizeValueTests
```

Useful gates: `CRATONVM_DBG_STTRACE` (stack-trace fill/element tracing, added
in `4185be7a`), `CRATONVM_DBG_ATHROW` (throw-site + 8-frame stack per throw).
`RunListen` is the key tool — JUnitCore's terse "Test mechanism" summary hides
the real cause; `RunListen` dumps `failure.getException()` + cause chain.

## Recommendation

Merge the 4 committed fixes (they are general VM correctness fixes, valuable
independent of full-green), and treat the remaining items above as the ordered
backlog of this epic. Estimated several more fix-and-rebuild cycles to first
green, with no guarantee the tail is short.
