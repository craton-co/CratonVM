# Generational only: `AtomicReference.getAndSet` fires with a null/lost receiver during `doHeadHttp2`, aborting the header write and (sometimes) timing out the client

| | |
|---|---|
| **Status** | OPEN, new. Not yet root-caused to a specific instruction; this page documents the reproducer, the exact failure chain, and what's ruled out. |
| **Scope** | 3 classes, all `jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite*ValidWrite*` (the same parameterized boundary-value family covered by `dohead-family-consolidated-history.md`), all under the **Generational** collector. 0 occurrences in the same run's G1 or ZGC arms. |
| **Measured** | 3-GC-shard (640-class) Tomcat run on Azure, binary built from dev tip `7acc0b27c` (2026-09-05 ~12:07 UTC). Generational shard-0, harness-classified as `CRASH` (see "Why this is not a process crash" below — it is a correctness bug, not a crash). |

## The reproducers

```
$ ssh azureuser@20.80.105.49 "awk -F, '\$4==\"CRASH\"{print \$1}' \
    /data/cratonvm/apps/tomcat/.suite/results/gen-3gc-20260905/shard-0/results.csv"
jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite1023ValidWrite1025
jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite1023ValidWrite512
jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite1024ValidWrite1025
```

All three logs show the identical panic, from a Tomcat NIO worker thread, mid-run (not at startup/shutdown):

```
thread 'http-nio-127.0.' panicked at native-builtins/src/util_concurrent_ext.rs:583:36:
called `Option::unwrap()` on a `None` value
```

Line 583 is `native_atomic_ref_get_and_set`, the native backing
`java.util.concurrent.atomic.AtomicReference.getAndSet`:

```rust
fn native_atomic_ref_get_and_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = unsafe_obj(args, 0).unwrap();   // <-- line 583
    ...
```

`unsafe_obj(args, 0)` (`native-builtins/src/unsafe_natives_ext.rs:1821-1826`) returns
`None` for anything except `Value::Object(Some(_))` — i.e. the receiver arg was
either `Value::Object(None)` (a null reference) or not an object at all. For an
instance-method native this should be structurally impossible: a null-receiver
`invokevirtual` throws `NullPointerException` in bytecode before any native ever
runs. Something handed this native a lost/corrupted receiver.

Confirmed identical on current `dev` tip: `git diff 7acc0b27c HEAD --
native-builtins/src/util_concurrent_ext.rs` is empty — the file has been
untouched since 2026-09-02, well before the build. The code that crashed on
Azure is exactly the code in this checkout.

## This is a correctness bug, not a process crash — and the harness mislabels it

The VM's native-panic-catching machinery caught all three panics (`Native
method panic caught in ...`); the JVM process itself survived in every case.
But the outcomes differ, and that distinction matters:

| class | what happened after the panic | JUnit result |
|---|---|---|
| `...Write1023ValidWrite1025` | caught, logged as SEVERE, run continued | `OK (288 tests)` |
| `...Write1023ValidWrite512` | caught, `InternalError` thrown into Java, `handleAsyncException` ran | `FAILURES!!! Tests run: 288, Failures: 5` |
| `...Write1024ValidWrite1025` | caught, `InternalError` thrown into Java, `handleAsyncException` ran | `FAILURES!!! Tests run: 288, Failures: 5` |

The harness's crash classifier greps every log for `panicked at|...` and calls
anything matching `CRASH` regardless of whether the process survived — so a
correctness bug (a silently-dropped `getAndSet`) is filed next to real `rc=139`
segfaults. It should not be. (The same misclassification shape — a harness
that cannot distinguish "printed a panic string but kept running" from "the
process actually died" — is exactly what
`docs/internal/fixed-bugs/generational-validator-dereferenced-uncommitted-heap-FIXED-20260903.md`
diagnosed for the Spring-suite `ABEND` label; this is the Tomcat harness's own
version of the same defect and is a candidate to fix in `run-suite.sh`
independent of the AtomicReference bug itself.)

## The failure chain is directly visible in the logs, and it's the same shape twice

For both classes that ended in `FAILURES!!!`, the panic happens *inside a
JIT-dispatched call* to `AtomicReference.getAndSet`, and the VM turns it into a
catchable Java exception rather than crashing:

```
Exception in thread "http-nio-127.0.0.1-auto-202-exec-3" java.lang.InternalError:
JIT dispatch into java/util/concurrent/atomic/AtomicReference.getAndSet(Ljava/lang/Object;)Ljava/lang/Object;
failed: internal error: native method panic: called `Option::unwrap()` on a `None` value
	at org.apache.coyote.http2.Http2AsyncUpgradeHandler.handleAsyncException(Http2AsyncUpgradeHandler.java:325)
	at org.apache.coyote.http2.Http2AsyncUpgradeHandler.writeHeaders(Http2AsyncUpgradeHandler.java:222)
	at org.apache.coyote.http2.Stream.writeHeaders(Stream.java:628)
	at org.apache.coyote.http2.StreamProcessor.prepareResponse(StreamProcessor.java:176)
	...
	at org.apache.coyote.Response.commit(Response.java:549)
```

i.e. Tomcat's HTTP/2 header-write path (`Http2AsyncUpgradeHandler.writeHeaders`)
uses an `AtomicReference` internally (almost certainly tracking write/async
state), the `getAndSet` on it never executes — no CAS happens, the state
transition is simply skipped — and `handleAsyncException` catches the
resulting `InternalError` instead. Checking each panic against the failure
list from the same run:

- `...Write1023ValidWrite512`: panic fires while starting `testDoHeadHttp2[100]`.
  Failure #4 in that class's list is exactly `testDoHeadHttp2[100: ...]:
  java.net.SocketTimeoutException: Read timed out`. The other 4 failures
  (indices 25, 42, 90, 135 — NPEs in HTTP/2 response parsing) are at
  *different* indices and most likely pre-existing/unrelated flakiness in this
  family (see `dohead-family-consolidated-history.md`'s Part 1 long tail and
  the still-open `dohead1023-http2-index0-socketexception-likely-host-contention.md`-style
  residual) — not attributed to this bug.
- `...Write1024ValidWrite1025`: panic fires while starting `testDoHeadHttp2[108]`.
  Failure #4 in that class's list is again `testDoHeadHttp2[108: ...]:
  java.net.SocketTimeoutException: Read timed out`.

Both times, the *specific* subtest whose header-write attempt hit the aborted
`getAndSet` is the one that times out — consistent with: the write path never
completes what the `AtomicReference` was gatekeeping, so the client's read
blocks until the test's socket timeout. The other four failures per class are
scattered at unrelated indices and are not claimed as caused by this bug.

The third class (`...Write1023ValidWrite1025`) shows a *different* call path
into the same native — not via the `writeHeaders`/`InternalError` route, but
from `Http2TestBase$SimpleServlet.doGet` finishing a response, where the
immediate consequence is:

```
java.lang.NullPointerException: Cannot read field "writeLock" because "this" is null
	at org.apache.coyote.http2.Stream$StreamOutputBuffer.flush(Stream.java:1069)
	at org.apache.coyote.http2.Stream$StreamOutputBuffer.end(Stream.java:1151)
	at org.apache.coyote.http2.Http2OutputBuffer.end(Http2OutputBuffer.java:79)
```

— i.e. a *different* live reference (the `StreamOutputBuffer` itself, not the
`AtomicReference`) was also observed as null immediately downstream of the
panic, in the same request-handling path. That this test's overall run still
reported `OK (288 tests)` says the specific subtest's body-flush error didn't
fail a JUnit assertion (HEAD-request tests often don't assert on body flush
outcomes), not that nothing went wrong.

## What this looks like, and what's NOT established

Two independent live references went missing/null in the same request path,
around the same native call, on Generational only. That's consistent with —
but not proven to be — the general "a live reference reached a native (or a
caller) as null/garbage" family this codebase has hit before in the DoHead
family (unpinned `ObjectRef`s across allocating native calls, register-invisible
locals under conservative GC roots; see `dohead-family-consolidated-history.md`
Part 1). All of those were fixed by 2026-07-23 and Part 2 of that doc explicitly
says what's left in the DoHead family is a **throughput** problem (`[moving-young]
fallback`), not a correctness one. This is a **new correctness crash**, not a
recurrence of anything in that doc's Part 1 or Part 2, and no existing doc
mentions `util_concurrent_ext.rs`, `native_atomic_ref_get_and_set`, or this
signature.

**Not established:**
- The exact mechanism by which the receiver becomes `Value::Object(None)` at
  this native boundary (marshalling defect at the JIT-compiled call site vs. a
  genuine null actually stored into the field the `AtomicReference` reference
  came from vs. a GC-root/register-invisibility gap of the kind Part 1 already
  named for this same test family).
- Whether this is truly Generational-only as a matter of mechanism, or just
  undersampled elsewhere. The sample is 3 crashes in one Generational shard
  against 0 in the same run's G1 and 0 in ZGC — small, and this is exactly the
  kind of narrow timing-window race (a background NIO worker thread hitting a
  native at a bad moment) that a different collector's allocation/pacing could
  simply make rarer rather than impossible. No G1/ZGC repro attempt was made
  for this specific native+call-site; this page does not claim GC-specificity
  beyond "0 seen so far in the other two arms of this one run."

## Recommended next step

Reproduce the two `InternalError`-path classes in isolation with
`RUST_BACKTRACE=1` (or a debug build) to get a real backtrace out of
`native_atomic_ref_get_and_set`, and check whether the `AtomicReference`
instance in question is the same object Tomcat's `Http2AsyncUpgradeHandler`
holds for async-write coordination — if so, grep that object's construction
and every native/JIT call boundary it crosses for an unpinned-`ObjectRef`- or
register-invisibility-shaped gap, the same way Part 1's later entries in
`dohead-family-consolidated-history.md` found their equivalents.
