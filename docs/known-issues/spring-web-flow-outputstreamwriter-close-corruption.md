# spring-web http.client Flow/Reactive hangs: OutputStreamWriter internal-state corruption + 2 unrelated hangs

## Status: Root cause #1 FIXED (commit `6744812d`, merged `963d59b3`); root causes #2/#3 still OPEN

Branch `fix/httpclient-jdkclient-hangs-0706b` (off `dev` @ `9f1db39d`), Azure
host worktree `/data/data/wt-hc-hangs-0706b`. This continues the
"Residual C — Genuine hangs" investigation from
`docs/known-issues/http-client-cluster-redefine-dispatch-and-jdk21-gaps.md`
for `JdkClientHttpRequestFactoryTests`, `OutputStreamPublisherTests`,
`SubscriberInputStreamTests`, `reactive.ClientHttpConnectorTests`.

**Update 2026-07-07:** Root cause #1 (below) is FIXED — see the "FIXED"
section right after it. `JdkClientHttpRequestFactoryTests` and
`reactive.ClientHttpConnectorTests` were NOT investigated in the fix session
either (both use real `MockWebServer`/sockets, unrelated to
`OutputStreamWriter`) and remain OPEN; this doc stays in `known-issues/`
until those are triaged too.

## Summary table

| Class | Uses MockWebServer? | Root cause | Status |
|---|---|---|---|
| `OutputStreamPublisherTests` | No (pure `Flow`+Reactor `StepVerifier`) | `closed()` test hung due to `OutputStreamWriter`/`StreamEncoder` real-field corruption (see below) | ✅ **FIXED** — 5/6 pass (`chunkSize()`'s pre-existing, unrelated `"bar"`-vs-`"b"` failure remains, not in scope) |
| `SubscriberInputStreamTests` | No (pure `Flow`, no Reactor) | Same bug, reached via `SubscriberInputStreamTests.closed()`'s identical pattern | ✅ **FIXED** — 5/5 pass |
| `JdkClientHttpRequestFactoryTests` | Yes (`AbstractMockWebServerTests`) | NOT investigated — does not use `OutputStreamWriter`; uses `SimpleAsyncTaskExecutor` (real `new Thread()` per task) | OPEN, separate investigation needed |
| `reactive.ClientHttpConnectorTests` | Yes (`MockWebServer` field) | NOT investigated | OPEN, separate investigation needed |

## Root cause #1 (FIXED 2026-07-07): `StreamEncoder` native shim hardcoded inherited `Writer` field slots

### The bug, precisely

```java
OutputStream out = new ByteArrayOutputStream();
OutputStreamWriter writer = new OutputStreamWriter(out, "UTF-8"); // any overload reproduces
writer.write("foo");
writer.close();
writer.write("bar"); // should throw IOException("Stream closed") -- did NOT, pre-fix
```

### Actual root cause (this was NOT a general interpreter/GC bug)

`native-io/src/stream_encoder.rs`'s `sun.nio.cs.StreamEncoder` shim allocates
the encoder object with the REAL `StreamEncoder` class id (so real
`OutputStreamWriter` bytecode dispatches `se.write(...)` to the native), but
the pre-fix version addressed its own bookkeeping (the underlying
`OutputStream`, the canonical charset name, a monotonic side-table id) via
hardcoded field indices `0`/`1`/`2`, on the mistaken assumption those were
`StreamEncoder`'s own first three declared fields.

`javap` on the real JDK25 `sun.nio.cs.StreamEncoder`/`java.io.Writer` classes
confirms the REAL layout is:

```
0: Writer.writeBuffer   (char[], inherited)
1: Writer.lock          (Object, inherited)
2: StreamEncoder.closed (boolean, StreamEncoder's own first field)
3: StreamEncoder.cs
4: StreamEncoder.encoder
...
7: StreamEncoder.out
```

So the shim's writes to indices 0/1/2 actually landed on `Writer.writeBuffer`,
`Writer.lock`, and `StreamEncoder.closed` — **not** scratch slots of its own.
This exactly explains both symptoms originally reported:
- `Writer.lock` held the charset-name string (written to index 1) instead of
  the lock object.
- `StreamEncoder.closed` read `true` immediately after construction (written
  to index 2 was the shim's monotonic id counter, which starts at 1 — a
  nonzero value in a `boolean` slot reads as `true`).

This is the same bug class already tracked in
`docs/internal/audits/native-hardcoded-inherited-field-slots.md` ("native
hardcodes an inherited field's slot"), just not yet swept for this file.

**Why the earlier session's bisection didn't find it:** all of that
session's checks (JIT on/off, `CRATONVM_DBG_STRAYSTACK`, GC/stale-pointer,
`alloc_concurrent_synthetic` sizing, synthetic Java repros matching the same
bytecode shape) were sound and correctly ruled out — the bug genuinely
wasn't any of those. It also wasn't reachable by disassembling
`Charset`/`StreamEncoder`'s own bytecode, because that bytecode never runs:
`forOutputStreamWriter`/`write`/`close`/etc. are fully native-intercepted in
real-JDK mode. The corruption was in the **Rust native's own hardcoded slot
constants**, a layer none of those checks inspected.

**Separate, and the actual proximate cause of the reported hang:** even
independent of the field corruption, the write natives never checked a
closed flag and threw — they silently no-op'd once the underlying-stream
reference was gone, so `writer.write("bar")` after `close()` did not throw
`IOException("Stream closed")` as real `StreamEncoder.ensureOpen()` does.
AssertJ's `assertThatIOException().isThrownBy(...)` found no exception and
threw an uncaught `AssertionError`; `OutputStreamPublisher$OutputStreamSubscription.invokeHandler()`
only catches `catch (Exception ex)`, so the `AssertionError` propagated
straight through and silently killed the executor worker thread before
`this.actual.onComplete()`/`onError()` was ever called — the
`Flow.Subscriber` (and thus `StepVerifier`/`SubscriberInputStream.read()`)
never received a terminal signal and blocked forever.

### The fix

Commit `6744812d` (branch `fix/streamencoder-inherited-field-slots-0707`,
merged to `dev` at `963d59b3`):

- Resolve the two real fields this shim legitimately owns semantically
  (`out`, `closed`) **by name** (`get_field_by_name`/`set_field_by_name`,
  which walk the real class's field metadata) instead of hardcoded indices —
  the fix recipe from `native-hardcoded-inherited-field-slots.md`.
- Keep the canonical charset name and the pending-bytes buffer (already
  side-tabled pre-fix) in the same Rust-side table, now keyed by
  `ctx.identity_hash_code(obj)` instead of a monotonic id stashed in a
  scratch field slot — this touches zero real fields for that bookkeeping.
- Added the missing `ensureOpen()`-equivalent check: `write`/`flush` now read
  the real `closed` field and throw `IOException("Stream closed")` when
  already closed, matching real `StreamEncoder`. `close()`/`implClose()` is
  now also idempotent (`if (closed) return;`), matching real
  `StreamEncoder.close()` — the pre-fix version would re-flush/re-close the
  underlying stream on a second `close()` call.

No change to the buffering/commit-threshold logic
(`docs/internal/fixed-suite-bugs/dohead-streamencoder-eager-flush-commit-threshold-FIXED.md`,
a separate, already-fixed concern) — `buffer_and_maybe_flush`/`write_through`
are untouched.

### Verification

- The zero-dependency repro above now prints `Got expected IOException:
  Stream closed` (was `NO EXCEPTION THROWN - BUG`).
- `OutputStreamPublisherTests`: was HANG (15s+ timeout), now `found=6 succ=5
  fail=1 ms=580 status=FAIL` — the one failure is `chunkSize()`
  (`expected: "bar" but was: "b"`), a separate, pre-existing, unrelated bug
  the original investigation already flagged as out of scope (not
  investigated further here either).
- `SubscriberInputStreamTests`: was HANG, now `found=5 succ=5 fail=0 ms=495
  status=OK`.
- `native-io` crate unit tests: unaffected, all pass.

## Root cause #2/#3 (still NOT investigated): `JdkClientHttpRequestFactoryTests` / `reactive.ClientHttpConnectorTests`

`JdkClientHttpRequestFactoryTests` and `reactive.ClientHttpConnectorTests`
both use real `MockWebServer` (`AbstractMockWebServerTests` /
`mockwebserver3.MockWebServer` field respectively) and do NOT use
`OutputStreamWriter` anywhere in their exercised code paths (`grep` over
`JdkClientHttpRequest.java` / `reactive/*.java` main sources found no
matches). `JdkClientHttpRequestFactoryTests`'s underlying
`JdkClientHttpRequest` DOES use the same `OutputStreamPublisher` class, but
via `SimpleAsyncTaskExecutor` (Spring's own executor, which spawns a
genuine real `new Thread()` per task — confirmed via
`spring-core/.../SimpleAsyncTaskExecutor.java` source, "fires up a new
Thread for each task") rather than `Executors.newSingleThreadExecutor()`,
so the `ExecutorService.execute()` inline-execution bug fixed in the
original session (see below) does not apply, and neither does the
`OutputStreamWriter` field-corruption bug fixed above unless
`JdkClientHttpRequestFactoryTests` itself calls a code path using
`OutputStreamWriter` (not checked). **Next concrete step for these 2
classes**: attach `gdb -p <pid> -batch -ex 'thread apply all bt'` to a live
hung instance (`timeout 60 ./target/release/cratonvm --java-home
/data/data/jdk25-real -cp "<spring-suite-runner-shared>:<testcp>" KRun
org.springframework.http.client.JdkClientHttpRequestFactoryTests` /
`...reactive.ClientHttpConnectorTests`, then `gdb` a still-running process)
and read the resulting backtrace against MockWebServer/okio/real-socket
code paths specifically — genuinely not done yet.

## A separate, real, low-risk fix landed in the original session (does NOT fix root cause #1 or #2/#3)

`native_es_execute` (`ExecutorService.execute(Runnable)`/
`ThreadPoolExecutor.execute(Runnable)`, `native-builtins/src/lib.rs`) ran
the submitted `Runnable` synchronously **inline on the calling thread**
instead of dispatching to a separate worker thread — a genuine violation of
`Executor.execute()`'s fire-and-forget contract, and the exact same defect
class as the already-fixed ForkJoinPool/CompletableFuture "Bug D"
(kafka-suite-0617). Fixed by routing through the existing
`spawn_runnable_on_real_thread` bounded real `ThreadPoolExecutor`, verified
via a minimal standalone repro
(`Executors.newSingleThreadExecutor().execute(...)` correctly runs on a
freshly-spawned `pool-1-thread-1` and `execute()` returns before the task
completes).

**This fix is currently dead code for the reported hangs**: `native_es_execute`
lives in `register_synthetic_overrides` (`native-builtins/src/lib.rs`,
guarded by `#[cfg(feature = "synthetic-jdk")]` AND
`config.use_synthetic_jdk`), which is never called in real-JDK mode
(`--java-home`, the mode all 4 hangs are reported/reproduced in) — confirmed
via `grep`-verified call-graph tracing AND empirically (a temporary debug
eprintln in `native_es_execute`, reverted before commit, never fired during
any of the 4 classes' hangs). It is kept as a genuine, real,
independently-useful fix for synthetic-JDK-mode callers of
`Executors.newSingleThreadExecutor()`/`newFixedThreadPool()`/
`newCachedThreadPool()`, documented precisely as scoped-but-inert for this
specific investigation so a future session does not re-discover "the
executor doesn't spawn a thread" and assume it explains these hangs (it
doesn't, in real-JDK mode).

## Reproduction

```bash
# Azure host, dev @ 963d59b3 or later (fix already merged)
ssh -i ~/.ssh/azure.pem victor@<current-IP>
cd /data/data/cratonvm   # or a fresh worktree off dev

CP="$(cat /data/data/spring-framework-shared/spring-web/build/cratonvm-testcp.txt)"

# Cheapest possible repro (no suite harness at all) — now passes:
cat > /tmp/WriterCloseTest.java <<'EOF'
import java.io.*;
public class WriterCloseTest {
    public static void main(String[] args) throws Exception {
        OutputStream out = new ByteArrayOutputStream();
        OutputStreamWriter writer = new OutputStreamWriter(out, "UTF-8");
        writer.write("foo");
        writer.close();
        try {
            writer.write("bar");
            System.out.println("NO EXCEPTION THROWN - BUG");
        } catch (IOException e) {
            System.out.println("Got expected IOException: " + e.getMessage());
        }
    }
}
EOF
/data/data/jdk25-real/bin/javac -d /tmp /tmp/WriterCloseTest.java
./target/release/cratonvm --java-home /data/data/jdk25-real -cp /tmp WriterCloseTest
# Expected (HotSpot) AND now CratonVM: "Got expected IOException: Stream closed"

# Full-class repro (used to hang, now completes):
timeout 60 ./target/release/cratonvm --java-home /data/data/jdk25-real \
  -cp "/data/data/spring-suite-runner-shared:$CP" \
  KRun org.springframework.http.client.OutputStreamPublisherTests
# RESULT found=6 succ=5 fail=1 (chunkSize, unrelated) ms=580 status=FAIL

# Still OPEN — root causes #2/#3, unrelated to the above:
timeout 60 ./target/release/cratonvm --java-home /data/data/jdk25-real \
  -cp "/data/data/spring-suite-runner-shared:$CP" \
  KRun org.springframework.http.client.JdkClientHttpRequestFactoryTests
timeout 60 ./target/release/cratonvm --java-home /data/data/jdk25-real \
  -cp "/data/data/spring-suite-runner-shared:$CP" \
  KRun org.springframework.http.client.reactive.ClientHttpConnectorTests
```
