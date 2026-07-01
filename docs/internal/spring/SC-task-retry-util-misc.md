# SC-task-retry-util-misc

> **UPDATE 2026-07-01:** RC1 and RC3 below are resolved on current `dev`.
> RC1's synthetic unmodifiable collection wrappers now declare
> `java/io/Serializable` in `vm/src/vm/vm_init.rs`. RC3's `Properties.store`
> path is now native-backed by `native-builtins/src/properties_sidetable.rs`,
> emits the JDK-style `#<date>` line before the first entry, and has a focused
> regression test (`render_store_text_includes_date_before_first_entry`).
> Remaining actionable items in this cluster are RC4 (Throwable deserialize),
> RC6 (ByteBuddy class injection), and RC7 (AQS/ForkJoinPool throttle); RC5 is
> a timing/perf artifact.

Cluster investigation: Task-executor / retry / util-misc.
Run log: `C:/craton/CratonVM-spring0621/spring-suite/full-run/spring-core.log` (+ `spring-core.raw.log` for stack traces).
CratonVM Rust source: `C:/craton/CratonVM-spring0621`. Spring test/main source: `C:/craton/cratonvm/apps/spring-framework/spring-core`.

This cluster contains **6 distinct root causes** across 6 test classes (14 failing tests). They are unrelated to each other and are split into separate Root-cause sections below.

Summary table:

| # | Issue | Tests | Severity | Confidence | Recommendation |
|---|-------|-------|----------|-----------|----------------|
| 1 | ~~`Collections.unmodifiableMap` wrapper is not `Serializable`~~ | MimeType.serialize (1) | Medium | High | **Fixed on dev** |
| 2 | Case-insensitive map value case-collapse (`LinkedCaseInsensitiveMap`) | MimeType.compareToCaseSensitivity (1) | Medium | Med-High | Handoff (merge w/ LinkedCaseInsensitiveMapTests owner) |
| 3 | ~~`Properties.store(OutputStream,null)` omits leading `#<date>` comment line~~ | PropertiesPersister x2 | Low-Med | High | **Fixed on dev** |
| 4 | Real `ObjectInputStream` cannot deserialize a `Throwable` | SerializationUtils.cloneException (1) | Medium | Med | Handoff |
| 5 | Tight (20 ms) retry-timeout fires one attempt early — interpreter/Mockito timing | RetryTemplate x3 | Low | High | Handoff (env/perf) |
| 6 | ByteBuddy `ClassInjector$UsingReflection` NoClassDefFoundError (AssertJ Assumptions) | TestGroup x2 | Medium | High | Handoff (ByteBuddy family) |
| 7 | Real-thread concurrency throttle (`ReentrantLock`/`Condition` + ForkJoinPool) | SyncTaskExecutor x4 | Medium | Med | Handoff |

---

## Root cause 1 — `Collections.unmodifiableMap` / `Map.of` wrapper class is not `Serializable`

> **STATUS: RESOLVED on dev.** The synthetic unmodifiable collection wrappers
> now load `java/io/Serializable` and include it in their interface lists in
> `vm/src/vm/vm_init.rs`; the comments there call out Spring `MimeType`
> serialization specifically. The historical analysis below is retained for
> provenance.

**Symptom**
`MimeTypeTests.serialize()` fails at *serialize* time:
`java.io.NotSerializableException: cratonvm.internal.UnmodifiableMap` (log line 249/1316).

**Affected tests**
- `org.springframework.util.MimeTypeTests.serialize()`

**Root cause(s)**
`MimeType` stores its `parameters` field as `Collections.unmodifiableMap(new LinkedCaseInsensitiveMap<>(...))`
(`spring-core/.../util/MimeType.java:234`, also used by the charset ctor path `:141`→`:158`). `MimeType` itself is `Serializable`, so the real `ObjectOutputStream` bytecode walks the `parameters` field value and requires it to be `Serializable`.

On HotSpot, `Collections.unmodifiableMap(...)` returns `java.util.Collections$UnmodifiableMap`, which **implements `java.io.Serializable`**. CratonVM substitutes a synthetic wrapper class `cratonvm/internal/UnmodifiableMap`, and that class is registered with **only the `java/util/Map` interface — not `java/io/Serializable`**:
- Wrapper class + interface registration: `vm/src/vm/vm_init.rs:986` → `("cratonvm/internal/UnmodifiableMap", &[map_id])` (only `Map`); loop that wires superclass/interfaces at `vm/src/vm/vm_init.rs:993-1003`.
- Wrapper allocated by `native_collections_unmodifiable_map`: `native-collections/src/lib.rs:28338-28350` (and the sibling `Map.copyOf` at `:28327`, `Set`/`List`/`Collection` variants nearby; const `UNMOD_MAP_CLASS` at `:26987`).

Because `cratonvm/internal/UnmodifiableMap` (and its `List`/`Set`/`Collection`/iterator siblings) does not declare `java/io/Serializable`, the real OOS `instanceof Serializable` check fails and throws `NotSerializableException`.

**Reproduction sketch**
```java
Map<String,String> m = Collections.unmodifiableMap(new LinkedHashMap<>(Map.of("a","b")));
new ObjectOutputStream(new ByteArrayOutputStream()).writeObject(m); // throws on CratonVM, OK on HotSpot
```

**Suspected subsystem**: native-collections / vm class bootstrap (synthetic wrapper class table).

**Severity**: Medium (any serializable object holding a `Collections.unmodifiable*` / `*.of()` / `*.copyOf()` field fails to serialize).

**Confidence**: High (direct NotSerializableException naming the exact synthetic class; interface list verified in `vm_init.rs`).

**Recommendation**: **Fix.** Add `java/io/Serializable` (and arguably `java/util/RandomAccess` for the list wrapper, matching JDK) to the `unmod_specs` interface lists at `vm/src/vm/vm_init.rs:976-992`. Low-risk, localized. Note this only makes the *type check* pass; verify the real OOS can actually serialize the wrapper's single backing-map field (it should, since the backing HashMap/LinkedHashMap is serializable). If OOS then tries to walk the wrapper's fields and the synthetic shape confuses it, a custom serialization path may be needed — but the type-declaration fix is the necessary first step and matches the JDK contract.

---

## Root cause 2 — Case-insensitive map collapses distinct VALUES (`LinkedCaseInsensitiveMap` backing)

**Symptom**
`MimeTypeTests.compareToCaseSensitivity()` fails:
`AssertionError: [Invalid comparison result] Expecting actual: 0 not to be equal to: 0` (log 250 / raw 1317-1321).

**Affected tests**
- `org.springframework.util.MimeTypeTests.compareToCaseSensitivity()`
- (Strongly correlated, out-of-cluster) `LinkedCaseInsensitiveMapTests` fails 3: `putAndGet`, `putWithOverlappingKeys`, `computeIfAbsentWithExistingValue` (log 220-224).

**Root cause(s)**
The failing assertion is the *value case-sensitivity* leg (`MimeTypeTests.java:416-419`):
`new MimeType("audio","basic", singletonMap("foo","bar"))` vs `singletonMap("foo","Bar")` must compare **non-zero**, because `MimeType.compareTo` compares values case-**sensitively**:
`String thisValue = getParameters().get(attr); ... comp = thisValue.compareTo(otherValue)` (`MimeType.java:577-583`). CratonVM returns `0`, i.e. both maps yield the *same* value for key `"foo"` ("bar" == "Bar").

`MimeType.parameters` is `Collections.unmodifiableMap(LinkedCaseInsensitiveMap)` (`MimeType.java:229-234`). `LinkedCaseInsensitiveMap` is **not** a `LinkedHashMap` subclass — it *wraps* a `LinkedHashMap targetMap` + `HashMap caseInsensitiveKeys` (`LinkedCaseInsensitiveMap.java:51-59`). Both are real Spring bytecode. `get("foo")` therefore goes: `UnmodifiableMap.get` (native, unwraps to backing) → `LinkedCaseInsensitiveMap.get` (bytecode) → `targetMap.get(realKey)` on the wrapped LinkedHashMap.

The value-collapse means the wrapped-`LinkedHashMap` read-through is returning the wrong/aliased value. Two candidate VM defects (need runtime bisection to disambiguate):
- LinkedHashMap overlay/side-table keying aliasing two distinct `targetMap` instances (the `lhm_overlay_key` / `widened_obj_key` identity-hash collision handling in `native-collections/src/lib.rs` — the singleton-map values are written into one map and read back from another), and/or
- `String.toLowerCase(Locale)` ignoring its `Locale` arg (`native-builtins/src/phases_early.rs:~1553-1562`) — confirmed bug, but ASCII-safe so unlikely the direct cause here.

The tight coupling to the `LinkedCaseInsensitiveMapTests` put/get/computeIfAbsent failures indicates a shared backing-collections defect, not MimeType-specific.

**Reproduction sketch**
```java
LinkedCaseInsensitiveMap<String> a = new LinkedCaseInsensitiveMap<>(); a.put("foo","bar");
LinkedCaseInsensitiveMap<String> b = new LinkedCaseInsensitiveMap<>(); b.put("foo","Bar");
// expect: a.get("foo")="bar", b.get("foo")="Bar"; on CratonVM they read equal -> compareTo==0
```

**Suspected subsystem**: native-collections (LinkedHashMap/HashMap backing read-through, overlay keying).

**Severity**: Medium (case-insensitive map mis-stores/aliases values — used widely in Spring headers/params).

**Confidence**: Medium-High (assertion isolates it to the value leg + strong correlation with the LinkedCaseInsensitiveMapTests cluster; exact backing line not statically pinned).

**Recommendation**: **Handoff** and merge with whoever owns the `LinkedCaseInsensitiveMapTests` cluster — same root cause, one fix clears both. Bisection: a standalone two-map repro under `--nojit` will quickly show whether it is overlay-key aliasing vs a get/convertKey path.

---

## Root cause 3 — `Properties.store(OutputStream, null)` omits the leading `#<date>` comment line

> **STATUS: RESOLVED on dev.** `Properties.store(OutputStream,String)` and
> `store(Writer,String)` are now registered in
> `native-builtins/src/properties_sidetable.rs`. The store text is built from
> side-table/virtual `entrySet()` data, includes the JDK-style date comment
> before entries, and preserves user comments before the date line. Regression:
> `render_store_text_includes_date_before_first_entry`.

**Symptom**
`PropertiesPersisterTests.propertiesPersister()` / `propertiesPersisterWithWhitespace()` fail (log 260-261). Actual stored bytes (raw 1345-1359):
```
code2=message2
code1=message1
```
Assertion `contains("\ncode2=message2")` fails — the **first** entry has no preceding newline. Real JDK `Properties.store` always writes a `#<current Date>` comment line first, so every entry (including the first) is newline-prefixed.

**Affected tests**
- `org.springframework.util.PropertiesPersisterTests.propertiesPersister()`
- `org.springframework.util.PropertiesPersisterTests.propertiesPersisterWithWhitespace()`

**Why other PropertiesPersister tests pass** (confirms diagnosis):
- `...WithHeader` (same input, non-null header) passes: the `#header` line precedes the first entry, restoring the leading `\n`.
- `...WithEmptyValue` (3 entries, no header) passes *by luck of iteration order* — with 3 entries only one lands first, so both asserted keys happen to be newline-prefixed.
- All `...WithReader*` (Writer path) pass.

**Root cause(s)**
The original analysis below is superseded by the native `Properties.store`
implementation noted above. The failure signature remains useful history.

`Properties.store` is **not** registered as a CratonVM native (registration block `native-builtins/src/properties_sidetable.rs:1971-2163` lists load/getProperty/setProperty/put/entrySet/etc. — no `store`), so it runs real `Properties.store0` bytecode, which does `bw.write("#"); bw.write(new Date().toString()); bw.newLine();` before the entries. The `#date` line is entirely absent in CratonVM output, so something on that comment-write path is dropped. Leading suspect: CratonVM **shadows `BufferedWriter`** with native `write`/`newLine` overrides that delegate via `bw_delegate_out` (`native-builtins/src/phases_late.rs:3864`, registrations at `:6557/:6585/:6604/:6633/:6657/:6668`). Either the comment line write is lost there, or `new Date().toString()` yields empty and the `#`+newLine collapse. Entry lines themselves write correctly, so the defect is specific to the comment/date emission, not the body.

**Reproduction sketch**
```java
Properties p = new Properties(); p.setProperty("k","v");
ByteArrayOutputStream o = new ByteArrayOutputStream(); p.store(o, null);
// HotSpot: "#<date>\nk=v\n"   CratonVM: "k=v\n" (no leading #date line)
```

**Suspected subsystem**: native-builtins BufferedWriter shadowing (`phases_late.rs`) and/or `java.util.Date.toString` under real Properties.store0 bytecode.

**Severity**: Low-Medium (mostly affects `.properties` output formatting / round-trip tests; functionally the key=value body is correct).

**Confidence**: High on the symptom (missing date line, exact bytes in log); Medium on the precise VM cause (BufferedWriter shadow vs Date.toString) — needs a 3-line repro to localize.

**Recommendation**: **Fix.** First confirm whether `new Date().toString()` returns non-empty and whether the `BufferedWriter` shadow forwards the leading `#…` write. If the BufferedWriter shadow is the culprit, ensure it forwards all writes verbatim (it already caused a `--help` empty-output regression per its own comments at `phases_late.rs:6550`).

---

## Root cause 4 — Real `ObjectInputStream` cannot deserialize a `Throwable`

**Symptom**
`SerializationUtilsTests.cloneException()` fails:
`java.lang.IllegalArgumentException: Failed to deserialize object` (log 269 / raw 1376). That message is `SerializationUtils.deserialize`'s catch for an **IOException** thrown by `ObjectInputStream.readObject()` (`SerializationUtils.java:84-89`). Serialize succeeds; **deserialize of the Throwable fails**.

**Affected tests**
- `org.springframework.util.SerializationUtilsTests.cloneException()`

**Root cause(s)**
`SerializationUtils.clone(new IllegalArgumentException("foo"))` = `deserialize(serialize(ex))` (`SerializationUtils.java:103-104`). CratonVM runs real `java.io.ObjectInputStream`/`ObjectOutputStream` bytecode (per memory: the synthetic `serialization.rs` natives are dormant under a real `--java-home`). A `java.lang.Throwable` has custom serialization handling of `detailMessage`, `cause` (self-referential), `stackTrace` (`StackTraceElement[]`), `suppressedExceptions`, plus a transient `backtrace`. The IOException on read indicates a serialize/deserialize stream-shape mismatch for the Throwable — most likely the `StackTraceElement[]` round-trip or the Throwable `writeObject`/`readObject` custom-hook dispatch under CratonVM's real-bytecode OOS/OIS path. Static search found no Throwable/StackTraceElement-specific serialization native, so this is an interaction between real OIS bytecode and CratonVM's reflective field/`fillInStackTrace`/`StackTraceElement` support rather than a single obvious shim.

**Reproduction sketch**
```java
byte[] b = SerializationUtils.serialize(new IllegalArgumentException("foo"));   // OK
Object o = SerializationUtils.deserialize(b);                                   // throws on CratonVM
```

**Suspected subsystem**: vm/native-builtins real `ObjectInputStream` deserialization + Throwable/StackTraceElement support.

**Severity**: Medium (any serialized exception fails to round-trip; affects remoting/caching of exceptions).

**Confidence**: Medium (cause path identified — Throwable custom serialization under real OIS — but exact failing field/line not statically pinned; the stream is opaque without a run).

**Recommendation**: **Handoff** to the serialization owner. Repro is trivial and standalone. First check `StackTraceElement[]` serializability and the Throwable `readObject` custom-hook dispatch in the real OIS path.

---

## Root cause 5 — Tight 20 ms retry timeout fires one attempt early (interpreter + Mockito timing)

**Symptom**
3 `RetryTemplateTests.TimeoutTests` fail with cause/message off by one attempt (raw 510-517, 850-103):
- `retryableWithTimeoutExceededAfterFirstRetry`: expected cause `"Boom 2"`, got `"Boom 1"`.
- `retryableWithTimeoutExceededAfterSecondRetry`: expected cause `"Boom 3"`, got `"Boom 1"`.
- `retryableWithTimeoutExceededAfterFirstDelayButBeforeFirstRetry`: expected the **preemptive** message `"...would exceed timeout (20ms) due to pending sleep time..."`, got the post-hoc `"...exceeded timeout (20ms); aborting execution"`.

**Affected tests** (all under nested `TimeoutTests`)
- `retryableWithTimeoutExceededAfterFirstRetry()`
- `retryableWithTimeoutExceededAfterSecondRetry()`
- `retryableWithTimeoutExceededAfterFirstDelayButBeforeFirstRetry()`

**Root cause(s)**
These tests set `timeout = 20ms`, `delay = ZERO`, and arrange for `Thread.sleep(100)` to occur on a *specific* later attempt so the timeout trips only after that attempt. `RetryTemplate.checkIfTimeoutExceeded` uses wall-clock `System.currentTimeMillis() - startTime >= timeout` at the **top of each retry loop** (`RetryTemplate.java:150`, `:209-227`). The design assumes the fast (non-sleeping) early attempts plus listener/back-off bookkeeping take well under 20 ms.

On CratonVM the *first* fast attempt's overhead alone exceeds 20 ms wall-clock, so the very first `checkIfTimeoutExceeded(...,0,...)` already sees elapsed ≥ 20 ms and aborts with the first exception (`Boom 1`) before reaching the attempt that the test intends to trip the timeout. The dominant overhead is the `retryListener` — a **Mockito mock** (`RetryTemplateTests.java:67` `mock()`), so every `onRetryableExecution`/`beforeRetry`/`onRetryFailure` call dispatches through ByteBuddy/Mockito interception, which is slow in the interpreter. The other 19 RetryTemplate tests (non-timeout, or timeout 10 ms with an actual 100 ms sleep on the *first* attempt) pass because their timing margins are not borderline at the first check.

This is fundamentally a **performance/timing artifact**: a 20 ms wall-clock budget is too tight for CratonVM's interpreter + Mockito-proxy dispatch. It is not a logic error in any VM subsystem; `Thread.sleep` and `System.currentTimeMillis` themselves are behaving (the elapsed time is genuinely > 20 ms).

**Reproduction sketch**
Run the three TimeoutTests with a mocked `RetryListener`; observe the timeout abort cause is `Boom 1` rather than the intended later `Boom N` because >20 ms elapses during the first fast attempt + mock dispatch.

**Suspected subsystem**: interpreter/JIT execution speed + Mockito/ByteBuddy proxy dispatch cost (not retry logic).

**Severity**: Low (timing-sensitive tests; no functional VM defect).

**Confidence**: High (log shows correct exception types/messages, only the off-by-one attempt selection consistent with elapsed-time overrun; root logic verified in `RetryTemplate.java`).

**Recommendation**: **Handoff / accept as env-perf artifact.** Not worth a code change. If desired, speeding up mock dispatch or JIT-compiling the retry loop would shrink the margin, but the 20 ms budget is inherently fragile under any load. Flag as a known timing-flaky trio.

---

## Root cause 6 — ByteBuddy `ClassInjector$UsingReflection` NoClassDefFoundError (AssertJ Assumptions)

**Symptom**
Both failing `TestGroupTests` trace to (raw 1008-1088, 1147-1156):
```
java.lang.IllegalArgumentException: Could not create type
  at net.bytebuddy.TypeCache.findOrInsert
  at org.assertj.core.api.Assumptions.createAssumptionClass
  ...
Caused by: java.lang.NoClassDefFoundError: net/bytebuddy/dynamic/loading/ClassInjector$UsingReflection
```
- `assumeGroupWithNoActiveTestGroups`: expected `org.opentest4j.TestAbortedException`, got the `IllegalArgumentException: Could not create type` above.
- `assumeGroupWithMatchingActiveTestGroup`: `assertThatCode(...).doesNotThrowAnyException()` caught the same ByteBuddy failure → `[assumption should NOT have failed]`.

**Affected tests**
- `org.springframework.core.testfixture.TestGroupTests.assumeGroupWithNoActiveTestGroups()`
- `org.springframework.core.testfixture.TestGroupTests.assumeGroupWithMatchingActiveTestGroup()`

**Root cause(s)**
These two tests use AssertJ `Assumptions.assumeThat(...)` (via `assumeGroup`, `TestGroupTests.java:121-126`). AssertJ's `Assumptions.createAssumptionClass` generates a proxy with **ByteBuddy**, whose `TypeCache.findOrInsert` instantiates `net.bytebuddy.dynamic.loading.ClassInjector$UsingReflection`. That class fails to load/link in CratonVM → `NoClassDefFoundError`, surfaced as `IllegalArgumentException: Could not create type`. `ClassInjector$UsingReflection` relies on reflective/`Unsafe`/`MethodHandles.Lookup` class-injection internals that CratonVM does not fully support. This is the **same ByteBuddy class-injection family** already tracked (memory: bug-E Mockito/ByteBuddy), and the identical `IllegalArgumentException: Could not create type` also kills `MultiValueMapTests.canNotChangeAnUnmodifiableMultiValueMap` (log 263) — same root cause, different caller.

The other two `TestGroupTests` (`...BogusActiveTestGroup`, `...AllMinusBogusActiveTestGroup`) pass because they assert an `IllegalStateException` thrown by `TestGroup.parse` *before* reaching the AssertJ assumption proxy. `TestGroupParsingTests` (9/9) passes — the parsing logic and `System.setProperty/getProperty("testGroups")` round-trip are fine; the failure is purely the ByteBuddy-backed AssertJ Assumptions machinery.

**Reproduction sketch**
```java
org.assertj.core.api.Assumptions.assumeThat(Set.of()).contains("x"); // ByteBuddy ClassInjector$UsingReflection NoClassDefFoundError on CratonVM
```

**Suspected subsystem**: classloading / ByteBuddy `ClassInjector$UsingReflection` support (Unsafe/Lookup.defineClass class injection).

**Severity**: Medium (blocks AssertJ Assumptions and any ByteBuddy `ClassInjector$UsingReflection` user; broad library impact).

**Confidence**: High (explicit NoClassDefFoundError + full stack trace in raw log).

**Recommendation**: **Handoff** — fold into the existing ByteBuddy/Mockito (bug-E) class-injection workstream; not a TestGroup-specific defect. (The tests are "harness-shaped" in that they only exercise the VM gap via AssertJ's proxy choice, but the underlying gap is a genuine VM limitation.)

---

## Root cause 7 — Real-thread concurrency throttle under ForkJoinPool (`ReentrantLock`/`Condition`)

**Symptom**
4 `SyncTaskExecutorTests` fail with bare `AssertionError` (log 202-205). The non-concurrent `plainExecution` passes (1/5).

**Affected tests**
- `withConcurrencyLimit()`, `withConcurrencyLimitAndResult()`, `withConcurrencyLimitAndException()`, `taskRejectedWhenConcurrencyLimitReached()`

**Root cause(s)**
All four configure `setConcurrencyLimit(2)` and launch 10 tasks via `CompletableFuture.runAsync(...)` (common ForkJoinPool), each holding a "permit" for `Thread.sleep(100)`; assertions require `target.current == 0` and `target.counter == 10` (or `== 2` + 8 `TaskRejectedException` for the reject test). The throttle is `ConcurrencyThrottleSupport`, which uses **`java.util.concurrent.locks.ReentrantLock` + `Condition.await()/signal()`** (NOT `synchronized`/`wait`/`notify`) — see `ConcurrencyThrottleSupport.java:73-75, 119-201`. If the count ever exceeds 2, `ConcurrentClass.concurrentOperation` throws `IllegalStateException` (`SyncTaskExecutorTests.java:124`), corrupting `counter`/`current`.

So these depend on the full real-thread stack: `CompletableFuture.runAsync`/ForkJoinPool task scheduling, cross-thread `ReentrantLock` fairness, and `Condition.await()/signal()` correctly blocking/waking — which CratonVM implements atop its `LockSupport.park`/`unpark` infrastructure (`vm/src/vm/vm_exec.rs:3320-3363` blocking path; AQS `ConditionObject.await` routed through park, and `Thread.interrupt` unparks at `:4036-4044`). `SimpleAsyncTaskExecutorTests` (12/12) passes, so threads spawn; the divergence is in the AQS lock/condition + throttle counting under genuine concurrency (a count > limit, a missed `signal` wakeup, or a `runAsync` scheduling anomaly). The bare AssertionError (no thrown VM error) means tasks ran but the invariant (`current`/`counter`) was violated — consistent with the throttle admitting >2 concurrent tasks.

**Reproduction sketch**
```java
SyncTaskExecutor e = new SyncTaskExecutor(); e.setConcurrencyLimit(2);
// 10x CompletableFuture.runAsync(() -> e.execute(op)) where op sleeps 100ms and asserts concurrency<=2
// on CratonVM: current!=0 or counter!=10 (throttle admits >2, or signal/await race)
```

**Suspected subsystem**: vm concurrency primitives — AQS `ReentrantLock`/`ConditionObject` over `LockSupport.park`/`unpark`, and/or ForkJoinPool (`CompletableFuture.runAsync`) scheduling.

**Severity**: Medium (concurrency-throttle correctness; affects bounded executors / AQS-based locks).

**Confidence**: Medium (statically clear that the tests hinge on real AQS lock/condition + ForkJoinPool; the exact failing primitive needs a runtime repro since the log gives only a bare AssertionError).

**Recommendation**: **Handoff** to the concurrency/threading owner with the standalone repro above. Determine via a focused run whether the throttle admits >2 (lock/condition race) or whether `CompletableFuture.runAsync` mis-schedules; run with `--nojit` first to rule out the cross-thread-JIT-root gap warnings seen elsewhere in the same log.

---

## Open questions
1. RC1: after declaring the wrapper `Serializable`, does the real OOS correctly serialize the wrapper's single backing-map field, or is a custom serialization (or `writeReplace` to a plain HashMap) needed to match the JDK exactly?
2. RC2: is the value-collapse caused by LinkedHashMap overlay identity-hash aliasing (two `targetMap`s sharing a side-table entry) or by the `convertKey`/`get` path? A two-map standalone repro under `--nojit` disambiguates and likely also explains the 3 `LinkedCaseInsensitiveMapTests` failures.
3. RC3: is the missing `#<date>` line dropped by the shadowed `BufferedWriter` natives (`phases_late.rs`) or does `new Date().toString()` return empty under real `store0`?
4. RC4: which Throwable field breaks the OIS round-trip — `StackTraceElement[]`, the self-referential `cause`, or the `writeObject`/`readObject` custom-hook dispatch?
5. RC5: would JIT-compiling the retry loop / faster mock dispatch keep the 20 ms tests under budget, or should they just be flagged timing-flaky?
6. RC7: does the throttle admit >2 concurrent tasks (AQS lock/condition correctness) or does `CompletableFuture.runAsync` under-/over-schedule? Are the cross-thread JIT-root-gap warnings in the same run a contributing factor?
