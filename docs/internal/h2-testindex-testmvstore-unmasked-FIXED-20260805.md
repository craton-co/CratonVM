# `TestIndex` and `TestMVStore`: two divergences unmasked by the 2026-08-02 JIT div-guard fix

## Status
**BOTH FIXED, 2026-08-05** (`fix/h2-unmasked-divergences-20260803`). Retired
from `docs/known-issues/h2/`.

| class | filed 2026-08-02 | now |
| --- | --- | --- |
| `org.h2.test.db.TestIndex` | `AssertionError` in `testFunctionIndex` | **PASSES** (`rc=0`), matching stock HotSpot |
| `org.h2.test.store.TestMVStore` | `UnsupportedOperationException: remove` escaping `testIterate` | `testIterate` passes; the class runs on and dies later, on a **third, unrelated** gap — see *What replaced it* |

Neither was new code. Both were sitting behind a hard `InternalError` that killed
the class before it reached them, and both turned out to be general VM defects
that H2 merely happened to be the first thing to notice.

## 1. `TestIndex.testFunctionIndex` — JIT frames were invisible to Java

The test's whole assertion is one line:

```java
for (StackTraceElement element : Thread.currentThread().getStackTrace()) {
    if (element.getClassName().startsWith(Select.class.getName())) { counter++; break; }
}
…
assertEquals(1, testFunctionIndexCounter);
```

It counts a call to the H2 ALIAS function only when an
`org.h2.command.query.Select` frame is above it — i.e. only during query
*execution*, not during prepare/recompile.

**A JIT-compiled method runs without pushing a `runtime::frame::Frame`**, and
`Thread.getStackTrace()` — like every throwable's trace — was built from exactly
that frame list. So the Java-visible stack silently lost every method the JIT had
taken over, and lost more of it the longer a workload ran.

`probes/FnIndexProbe2.java` reduces it to seconds — the same query in a loop,
asking after each iteration whether a `Select` frame was visible:

| | HotSpot | CratonVM before | CratonVM after |
| --- | --- | --- | --- |
| misses in 3000 iterations | 0 | **2496**, first at iteration 503 | 0 |
| reported stack depth | 23 | 21 falling to **6** | 21 falling to 9 |

`--nojit` was 0 misses, and iteration ~502 is the compile threshold; both point
at the same thing.

**Fix** — reuse the chain the GC root scanner already maintains. Every
production entry into compiled code goes through
`JitEntryGuard::enter_with_compiled`, which already recorded a
`*const CompiledMethod` per active compiled frame. Two things were missing:

* **where the frame sits.** `JitFrameChainEntry::interp_depth`, the interpreter
  frame count at entry, taken at the five sites that have a `JvmThread`.
  `try_call_compiled_entry_reentrant` is called *from* compiled code and has
  none, so it inherits the enclosing entry's — exact, because no interpreter
  frame can have been pushed since.
* **a name.** `CompiledMethod::method_label`. The single-pass backend has always
  set it from the compile's method key; the optimizing tier never did, so its
  frames were anonymous to everything holding only an artifact.

`stackwalker::capture_full_trace` then splices the chain into the frame list at
those depths.

**Known limits.** A compiled frame carries no bytecode index, so its line number
is `UNKNOWN` and `StackTraceElement` renders `(Unknown Source)`; a frame with an
unknown line is strictly better than an absent frame. Inlined callees are still
invisible — that is what the residual depth gap (9 vs 23) is.

## 2. `TestMVStore.testIterate` — `Method.invoke` did not wrap what the callee threw

```
java/lang/UnsupportedOperationException: remove
  at org/h2/test/TestBase$1.invoke(TestBase.java:1546)   // Object ret = method.invoke(obj, args);
  at org/h2/test/store/TestMVStore.testIterate(TestMVStore.java:1939)
```

`TestBase.assertThrows` wraps the receiver in a `java.lang.reflect.Proxy` whose
handler catches `InvocationTargetException`. An unwrapped exception sails past
that catch and out of the test.

`java.util.Iterator.remove()` is the shape that finds it: it is force-native in
CratonVM, and a native callee raises its Java exception as a `RuntimeError`
rather than as a materialized `Throwable`.
`wrap_as_invocation_target_exception` materialized exactly **one** variant —
`OutOfMemoryError`, the one somebody had previously hit — and everything else
fell through to `other => return other` and reached the caller raw.

`probes/UnmaskProbe.java` pins all four shapes against HotSpot:

| shape | before | after / HotSpot |
| --- | --- | --- |
| declared virtual method | `ITE(IllegalStateException)` | same |
| user interface `default` | `ITE(IllegalStateException)` | same |
| JDK `Iterator.remove` default | **`RAW(UnsupportedOperationException)`** | `ITE(UnsupportedOperationException)` |
| the same through a `Proxy` | **escaped the handler** | caught, returns normally |

**Fix** — materialize any `RuntimeError` that has a Java counterpart. The
`RuntimeError → (class, detail message)` table moved out of
`vm::runtime::exceptions::throw_runtime_error` onto
`RuntimeError::as_java_throwable`, so the interpreter's own throw site and the
reflective wrapper share one table and cannot drift — the drift *is* the bug.
`NotImplemented` and every `VmError::Internal` still propagate unwrapped, so real
VM defects are not masked, and only the *dispatch of the callee* is wrapped, so
an `IllegalArgumentException` from argument marshalling still surfaces raw, as
`Method.invoke` specifies.

## What replaced it in `TestMVStore`

The class now gets far past `testIterate` and dies with

```
OutOfMemoryError: Direct buffer memory: tried 9756672, used 1069355008, max 1073741824
  at org/h2/mvstore/FileStore.lambda$serializeAndStore$0
```

That is a **third, unrelated gap**: direct `ByteBuffer`s are never reclaimed.
Filed with a ten-line reproducer as
`docs/known-issues/direct-bytebuffers-are-never-reclaimed-20260805.md`.

Note the class cannot pass on this host on **either** VM: stock HotSpot fails it
earlier, at `testCacheSize` (`Cache 1Mb, reads: 2800 expected: 1750`), an
upstream cache-ratio assertion CratonVM gets past. "TestMVStore green" was never
the goal here; `testIterate` was, and it is.

## Validation

Azure host, JDK 25, `--Xmx 1g`:

* `TestIndex` **passes** in isolation (`rc=0`), as on HotSpot.
* Both probes above, against stock HotSpot line for line.
* A 23-class H2 A/B against `origin/dev` moved **nothing**: 22 identical, and
  `TestMVStore` FAIL→TIMEOUT only because it now runs past the failure it used
  to die at. (`TestIndex` reads TIMEOUT in that table on *both* arms — 240 s is
  too short for it under 4-way load; it is the isolated run that counts.)
* `cargo test -p cratonvm-jit`: 1946 lib + integration, 0 failed.
* `cargo test -p cratonvm-vm --lib`: 2407 passed, 0 failed.
* `cargo test -p cratonvm-vm --lib --features synthetic-jdk`: 3920 passed,
  **4 failed — all four pre-existing on the fork-point `dev`**, verified by
  reverting the branch's own files and re-running them
  (`inet_socket_address_basics`, `linked_hashmap_put_get`,
  `linked_hashmap_put_if_absent`, `linkedhashmap_first_last_entry_p64`). The
  blocking gate is red on dev tip independently of this work.

## Lesson

Both defects are the same shape: **a table or a walk that covers the cases
somebody happened to hit**. The reflective wrapper materialized the one
exception variant that had been reported; the stack walk covered the frames the
interpreter happened to own. Neither had a rule that made the uncovered cases
impossible — which is why fixing the first one (a JIT crash) is what exposed
them both.
