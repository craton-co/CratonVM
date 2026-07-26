# spring-bug-04: JUnit `@Timeout` interceptor chain invoked twice (`proceed()` called multiple times)

| | |
|---|---|
| **Category** | VM-CORRECTNESS (threads / invocation) |
| **Module** | spring-core (any test using JUnit `@Timeout` / `assertTimeoutPreemptively`) |
| **CratonVM** | FAIL — `JUnitException: Chain of InvocationInterceptors called invocation multiple times` |
| **HotSpot JDK 25** | OK |
| **CratonVM HEAD** | 694c957f (dev) |
| **Status** | FIXED — commit `fc20e970` (merged to `dev`) |
| **Suggested owner** | — |

## ★ FIXED (2026-06-18) — NOT threading/MethodHandle: a synthetic-collection compare raised the wrong exception TYPE, masked by the JUnit chain

The "interceptor called multiple times" message is a **mask**, not the bug. The real failure
is a single test, `ValueCodeGeneratorTests.generateWhenSetOfClass()`, and it is **deterministic
and single-threaded** — it has nothing to do with `@Timeout` worker threads or `MethodHandle`
re-entry (the suspected-root-cause section below is wrong).

**Root cause.** Spring's `ValueCodeGeneratorDelegates.SetDelegate.orderForCodeConsistency` does
`new TreeSet<Object>(set)` over a `Set<Class<?>>` and **catches `ClassCastException`** to fall
back to the original set when the elements aren't `Comparable`:
```java
try { return new TreeSet<Object>(set); }
catch (ClassCastException ex) { return set; }   // Class is not Comparable
```
On real JDK, inserting the second non-`Comparable` `Class` runs `((Comparable)key).compareTo(..)`,
whose `checkcast java/lang/Comparable` throws `ClassCastException` — caught, test passes.
CratonVM's synthetic natural-order compare (`native-collections` `compare_via_compare_to`) instead
invoked `compareTo` blindly, so a non-`Comparable` receiver raised **`NoSuchMethodError:
java/lang/Class.compareTo(Ljava/lang/Object;)I`** — a *different* exception type that Spring's
`catch (ClassCastException)` does not catch. It escaped the test body and, unwinding through the
`@Timeout` interceptor, tripped JUnit's "called multiple times" detector. (Same masking family as
[[junit-multiple-times-masks-vm-linkage-error]] and the already-fixed `Function.compare` /
`Stream.sorted` case in `native-collections`.)

**Fix.** `../../../../native-collections/src/lib.rs`, `compare_via_compare_to`: before dispatching `compareTo`,
check `implements_comparable(receiver)`; if false, return `RuntimeError::ClassCastException`
(`class <name> cannot be cast to class java.lang.Comparable`) — JDK-faithful. This is the single
natural-ordering dispatch point used by `TreeSet`/`TreeMap` (`natural_compare`) and `Arrays.sort`.

**Verification.**
- `ValueCodeGeneratorTests`: was 40/41 FAIL → **41/41 OK**.
- Standalone probes (`spring-suite/probe/TreeSetCCE.java`, `SortRegress.java`): CratonVM now
  matches HotSpot byte-for-byte — `Comparable` `TreeSet`/`TreeMap`/`Arrays.sort` still sort
  correctly; non-`Comparable` `TreeSet`/`TreeMap`/`Arrays.sort` throw `ClassCastException`.

### Class-list correction — the other two listed classes were MIS-ATTRIBUTED
At the older base the doc grouped three classes under this symptom. On dev `694c957f` only
`ValueCodeGeneratorTests` actually shows "multiple times". The other two fail for **unrelated**
reasons (verified identical on the fixed and unfixed binary — this fix neither helps nor hurts
them), and neither even uses `@Timeout`:
- `core.ReactiveAdapterRegistryTests` :: `toMulti()` — `AbstractMethodError: Collector.supplier()
  has no Code attribute` (separate; bug-10 "no Code"/synthetic-dispatch family).
- `core.PropagationContextElementTests` — `NoSuchMethodError:
  AtomicLongFieldUpdater$RustJvmImpl.incrementAndGet` (see worktree `fix/atomic-field-updater-methods`)
  + Kotlin `kotlinx.coroutines` `Dispatchers.IO` `DispatchException`.

Those belong to their own bugs, not spring-bug-04.

---

### (original analysis below — the "threading/MethodHandle" suspicion is SUPERSEDED by the above)

## Symptom
```
org.junit.platform.commons.JUnitException: Chain of InvocationInterceptors called invocation
  multiple times instead of just once: org.junit.jupiter.engine.extension.TimeoutExtension
  at …InvocationInterceptorChain.proceed
```
JUnit's `TimeoutExtension` runs the test body on a **separate thread** with a timeout. Under
CratonVM the interceptor's `proceed()` ends up invoked more than once, which JUnit detects and
rejects. Strongly suggests a CratonVM bug in cross-thread invocation or `MethodHandle`/lambda
invocation re-entry used by the timeout machinery.

## Affected test classes (confirmed CV-unique, HotSpot OK)
```
aot.generate.ValueCodeGeneratorTests
core.ReactiveAdapterRegistryTests
core.PropagationContextElementTests
```
(Will recur in every module — many Spring tests use `@Timeout`. Fixing this clears a broad band.)

## Reproduce
```bash
CP="$H;$(tr -d '\r' < .../spring-core/build/cratonvm-testcp.txt)"
KRUN_STACK=1 "$VM" --java-home "$JDK" -cp "$CP" KRun org.springframework.aot.generate.ValueCodeGeneratorTests
"$JDK\bin\java.exe" -cp "$CP" KRun org.springframework.aot.generate.ValueCodeGeneratorTests   # passes
```

## Suspected root cause
`TimeoutExtension.interceptTestableMethod` submits the invocation to a single-use executor and
joins with a timeout. Candidates:
- the worker thread and the calling thread both run the invocation (thread start semantics — cf.
  memory note "boot-jdk-mismatch makes `new Thread(runnable)` no-op"; here we DO boot JDK25 so
  threads run, but the executor future may also run inline),
- a `MethodHandle.invoke` / lambda is dispatched twice.

## Notes
Verify whether the body executes on both the test thread and the timeout worker. Related to the
threading notes in memory. Likely **one** root cause across all listed classes.
