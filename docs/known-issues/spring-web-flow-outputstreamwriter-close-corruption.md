# spring-web http.client Flow/Reactive hangs: OutputStreamWriter internal-state corruption + 2 unrelated hangs

## Status: OPEN (root cause narrowed, not fully resolved)

Branch `fix/httpclient-jdkclient-hangs-0706b` (off `dev` @ `9f1db39d`), Azure
host worktree `/data/data/wt-hc-hangs-0706b`. This continues the
"Residual C — Genuine hangs" investigation from
`docs/known-issues/http-client-cluster-redefine-dispatch-and-jdk21-gaps.md`
for `JdkClientHttpRequestFactoryTests`, `OutputStreamPublisherTests`,
`SubscriberInputStreamTests`, `reactive.ClientHttpConnectorTests`.

**All 4 classes still hang** on current `dev`. This session found that they
are **at least 2, likely 3, distinct root causes** — the original doc's
"maybe one shared MockWebServer-related cause" hypothesis is refuted for 2
of the 4 classes, which use no MockWebServer/sockets at all.

## Summary table

| Class | Uses MockWebServer? | Root cause this session | Status |
|---|---|---|---|
| `OutputStreamPublisherTests` | No (pure `Flow`+Reactor `StepVerifier`) | `closed()` test hangs due to `OutputStreamWriter`/`StreamEncoder` internal-state corruption (see below) | Root cause narrowed, NOT fixed |
| `SubscriberInputStreamTests` | No (pure `Flow`, no Reactor) | Same `OutputStreamWriter` bug reached via `SubscriberInputStreamTests.closed()`'s identical pattern (not independently confirmed but near-certain given identical code shape) | Root cause narrowed, NOT fixed |
| `JdkClientHttpRequestFactoryTests` | Yes (`AbstractMockWebServerTests`) | NOT investigated this session — does not use `OutputStreamWriter`; uses `SimpleAsyncTaskExecutor` (real `new Thread()` per task, unaffected by the executor bug found below) | OPEN, separate investigation needed |
| `reactive.ClientHttpConnectorTests` | Yes (`MockWebServer` field) | NOT investigated this session | OPEN, separate investigation needed |

## Root cause #1 (found + explained, NOT fixed): `OutputStreamWriter` internal state corrupted from construction

### The bug, precisely

```java
OutputStream out = new ByteArrayOutputStream();
OutputStreamWriter writer = new OutputStreamWriter(out, "UTF-8"); // any overload reproduces
writer.write("foo");
writer.close();
writer.write("bar"); // should throw IOException("Stream closed") -- does NOT
```

Confirmed via reflection on a live CratonVM process (`--java-home` real-JDK
mode, JDK 25, `docs/.../wt-hc-hangs-0706b`):

- `sun.nio.cs.StreamEncoder.closed` (a plain `private volatile boolean`,
  should default `false`) already reads **`true` immediately after
  construction**, before `close()` is ever called.
- `java.io.Writer.lock` (inherited `protected Object lock`, which
  `OutputStreamWriter`'s constructor chain should set to the `OutputStream`
  argument via `super(out)`) instead holds **the charset-name `String`**
  (e.g. `"UTF-8"`, or `"ISO-8859-1"` when that name is used instead — the
  value tracks whatever charset name/`Charset.defaultCharset()` resolution
  happened, not the real lock object). This holds across ALL constructor
  overloads (`(OutputStream)`, `(OutputStream,String)`,
  `(OutputStream,Charset)`), even ones whose bytecode never mentions the
  charset-name string that ends up there.

Because `closed` reads `true` from the start, `StreamEncoder.close()`'s
real bytecode (`getfield lock; monitorenter; getfield closed; ifeq
<call implClose+set closed>; <else> monitorexit; return`) takes the
already-closed fast-return path on the very FIRST call — it never calls
`implClose()` and never re-executes the `putfield closed`. This is why a
targeted `putfield`-diagnostic trace (below) never observed a `closed`
write: the write path is dead code given the (already-wrong) starting
state.

### Downstream consequence: the actual hang

`OutputStreamPublisherTests.closed()` / `SubscriberInputStreamTests.closed()`:

```java
Flow.Publisher<byte[]> publisher = new OutputStreamPublisher<>(outputStream -> {
    OutputStreamWriter writer = new OutputStreamWriter(outputStream, UTF_8);
    writer.write("foo");
    writer.close();
    assertThatIOException().isThrownBy(() -> writer.write("bar")).withMessage("Stream closed");
    latch.countDown();
}, this.byteMapper, this.executor, null);
```

Since `writer.write("bar")` after close does NOT throw, AssertJ's
`assertThatIOException().isThrownBy(...)` itself throws an
**`AssertionError`** (an `Error`, not an `Exception`) to report the missing
exception. `OutputStreamPublisher$OutputStreamSubscription.invokeHandler()`
(`org/springframework/http/client/OutputStreamPublisher.java`) only catches
`catch (Exception ex)` around the handler body — an `AssertionError`
propagates straight through, uncaught, and **silently kills the executor
worker thread** before `this.actual.onComplete()` / `onError()` is ever
called. The `Flow.Subscriber` (and thus `StepVerifier`/`SubscriberInputStream.read()`)
never receives a terminal signal and blocks forever. This is confirmed via
isolated per-method JUnit Platform runs
(`org.junit.platform.launcher... selectMethod`) against
`OutputStreamPublisherTests`:

```
basic            -> succ=1 fail=0
flush            -> succ=1 fail=0
chunkSize        -> succ=0 fail=1  (expected: "bar" but was: "b" -- separate, unrelated bug, not investigated)
cancel           -> succ=1 fail=0
closed           -> HANGS (15s+ timeout, 100% CPU one thread, no RESULT line)
negativeRequestN -> succ=1 fail=0
```

Running the WHOLE class via `KRun` therefore also hangs, since JUnit
Platform runs all `@Test` methods in one process/one JVM invocation.

`SubscriberInputStreamTests` was not isolated per-method this session but
has an identically-shaped `closed()` test using the same
`OutputStreamWriter` pattern, so this is very likely the same bug there too
(not independently confirmed with a live capture — flagged as "near
certain, not proven" per the task's evidence-based-reporting requirement).

### What's ruled out (checked and refuted this session)

- **Not a JIT bug**: identical corruption with `CRATONVM_DISABLE_JIT=1`.
- **Not an out-of-bounds heap write**: `CRATONVM_DBG_STRAYSTACK=1` (existing
  diagnostic at `vm/src/runtime/interpreter.rs:12138`, checks
  `field.field_index >= object.num_slots`) shows zero hits — the putfield
  target slot is within the object's allocated bounds.
- **Not a `alloc_concurrent_synthetic`-undersized-object bug**: verified
  (via a temporary `CRATONVM_DBG_ACS` trace, reverted before commit) that
  `Charset.forName`/`Charset.newEncoder()`'s native overrides (which DO
  return synthetic objects, `native-builtins/src/lib.rs:41331` /
  `native-builtins/src/phases_late.rs` `register_p58_charset_coder`, both
  active in real-JDK mode via `vm_init.rs`) correctly auto-upsize to the
  real class's field count (`alloc_concurrent_synthetic`'s existing
  `num_fields.max(real)` logic, `native-builtins/src/lib.rs:34852`) — AND,
  more importantly, **these natives are never even invoked** on the actual
  `new OutputStreamWriter(out, "UTF-8")` path (confirmed: the trace fires
  for a direct top-level `Charset.forName(...)` call from application code,
  but NOT when `Charset.forName`/`newEncoder` are called from *within*
  `sun.nio.cs.StreamEncoder`'s own bytecode — even when
  `StreamEncoder.forOutputStreamWriter` is invoked directly via reflection).
  This means `Charset.forName`/`newEncoder` run as **100% real JDK bytecode**
  in the failing path, hitting real `Charset`'s static provider-lookup/cache
  machinery (`cache1`/`cache2` static fields, SPI `CharsetProvider`
  lookups) — untested territory this session, and a plausible next-step
  target.
- **Not a generic field-layout/putfield bug for this exact class shape**:
  multiple synthetic Java repros matching the EXACT bytecode shape
  (`Writer`-like 2-field superclass + subclass fields; `new/dup/args/
  invokestatic/invokespecial` construction shape; 2-level constructor
  delegation with an intervening 3-call virtual chain; a static-factory
  wrapper around the whole thing) were built and run correctly on
  CratonVM — see
  `/tmp/dl/repro/{MinimalCtorTest,StackShapeTest2,StackShapeTest3,NewDupTest,NewDupTryCatch,FieldOrderTest,FullShapeTest}.java`
  in this session's scratch dir (not committed; recreate from this doc if
  needed). Only the REAL `java.io.Writer`/`OutputStreamWriter`/
  `sun.nio.cs.StreamEncoder`/`java.nio.charset.Charset` classes trigger the
  bug — something specific to these actual boot classes (or their
  specific interaction with `Charset.forName`'s real bytecode), not a
  general interpreter defect reproducible with equivalent user classes.
- **Class metadata/field-index computation is correct**: a temporary
  `CRATONVM_DBG_LAYOUT` trace (reverted before commit) confirmed
  `compute_field_layout` (`classloading/src/class_manager.rs:9229`)
  correctly computes `Writer` = 2 total fields (`writeBuffer`, `lock`),
  `StreamEncoder`'s own fields correctly starting at index 2 (`closed` is
  index 2, not 0), `OutputStreamWriter`'s own field `se` correctly at index
  2. The metadata is right; something at RUNTIME still corrupts the
  slots 1-2 boundary only for this real-class combination.
- **Not a GC/stale-pointer issue**: a targeted interpreter-level putfield
  trace showed the CORRECT object reference (`out`, not the charset name)
  being written to `Writer.lock` at construction time — the corruption is
  not "wrote wrong value" at the observed putfield. (A companion
  "immediate readback via `shared.heap.get_field`" check in the same trace
  showed a mismatch too, but this was determined to be a FALSE LEAD: the
  identical readback-mismatch pattern also appeared for the
  `FieldOrderTest`/`FullShapeTest` control repros that behave CORRECTLY
  end-to-end, meaning `shared.heap.get_field`'s indexing convention does
  not directly correspond to `field.field_index` as used by regular
  bytecode dispatch, and is not a valid way to cross-check the real
  getfield/putfield path from that call site. Do not reuse that exact
  diagnostic without first establishing the correct index-conversion
  between the two.)

### Next steps (not attempted this session)

1. Instrument (or attach `gdb`/manual single-step) specifically inside
   REAL `Charset.forName`'s bytecode execution (not our natives) when
   called transitively from `StreamEncoder.forOutputStreamWriter` — confirm
   whether it returns a well-formed `Charset` instance (e.g. check its
   *actual* runtime class — should be `sun.nio.cs.UTF_8` or similar
   concrete subclass, not the abstract `Charset` — a previous quick check
   in this session on a DIRECT `Charset.forName` call from application code
   showed `Charset class: class java.nio.charset.Charset`, i.e. the
   ABSTRACT class itself, which is already wrong/suspicious for a
   native-intercepted call — but that specific call path was confirmed NOT
   to be what `StreamEncoder` hits internally, so this needs to be
   re-checked specifically for the internal-call path).
2. Try reproducing with `CRATONVM_REAL_AQS=1` / other env toggles seen used
   elsewhere in this session's `ps aux` output on the shared host (other
   concurrent sessions use `CRATONVM_REAL_NET_SOCKETS`,
   `CRATONVM_REAL_AQS`) in case an existing "more real bytecode" toggle
   happens to route around whatever `Charset`/`StreamEncoder`-adjacent
   native or fast-path is responsible.
3. Consider whether `java.nio.charset.Charset`'s STATIC fields
   (`cache1`/`cache2`, `defaultCharset`) — which are populated once and
   shared across every `Charset.forName` call process-wide — could be
   corrupted by an EARLIER, unrelated call in the same process (bootstrap
   `System.out`/JDK-internal `Charset.defaultCharset()` calls happen very
   early); a minimal repro that does *nothing* before
   `new OutputStreamWriter(...)` still reproduces, but the JDK itself may
   already have called `Charset.forName`/`defaultCharset()` many times
   during its own bootstrap before `main()` even starts, so "minimal user
   code" does not mean "minimal actual `Charset` call history in this
   process."
4. Because the bug reproduces with a bare `ByteArrayOutputStream` and zero
   Spring/executor/Reactor code
   (`/tmp/dl/repro/WriterCloseTest.java`,
   `/tmp/dl/repro/MinimalCtorTest.java` in this session's scratch dir),
   this is the cheapest possible repro to hand to a fresh investigation —
   no suite harness, classpath, or MockWebServer needed, just
   `--java-home <jdk> -cp <dir> WriterCloseTest`.

## Root cause #2/#3 (NOT investigated this session)

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
so the `ExecutorService.execute()` inline-execution bug fixed this session
(see below) does not apply, and neither does the `OutputStreamWriter`
corruption unless `JdkClientHttpRequestFactoryTests` itself calls a code
path using `OutputStreamWriter` (not checked). Live `gdb` process captures
for BOTH classes were taken this session (see raw output in session
transcript) but not analyzed frame-by-frame in the same depth as
`OutputStreamPublisherTests` — this is the concrete next step for these 2
classes: attach `gdb -p <pid> -batch -ex 'thread apply all bt'` to a live
hung instance (`timeout 60 ./target/release/cratonvm --java-home
/data/data/jdk25-real -cp "<spring-suite-runner-shared>:<testcp>" KRun
org.springframework.http.client.JdkClientHttpRequestFactoryTests` /
`...reactive.ClientHttpConnectorTests`, then `gdb` a still-running process)
and read the resulting backtrace against MockWebServer/okio/real-socket
code paths specifically — genuinely not done this session.

## A separate, real, low-risk fix landed this session (does NOT fix the above)

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
# Azure host, branch fix/httpclient-jdkclient-hangs-0706b off dev @ 9f1db39d
ssh -i ~/.ssh/azure.pem victor@<current-IP>
cd /data/data/wt-hc-hangs-0706b   # or a fresh worktree off this branch
CP="$(cat /data/data/spring-framework-shared/spring-web/build/cratonvm-testcp.txt)"

# Cheapest possible repro (no suite harness at all):
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
# Expected (HotSpot): "Got expected IOException: Stream closed"
# Actual (CratonVM):  "NO EXCEPTION THROWN - BUG"

# Full-class repro (hangs):
timeout 60 ./target/release/cratonvm --java-home /data/data/jdk25-real \
  -cp "/data/data/spring-suite-runner-shared:$CP" \
  KRun org.springframework.http.client.OutputStreamPublisherTests
# Never prints a RESULT line; 100% CPU on the main-vm thread.
```
