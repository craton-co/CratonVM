# `WrongCredentialsTest` hangs 120s instead of failing fast

**Status:** OPEN, but **re-characterised on 2026-08-14**. The original filing
guessed at a credentials/authentication defect. That guess is now disproven:
the test never reaches authentication. Two genuine defects were found on the
way, one of them fixed; the hang itself is not yet root-caused and the
remaining suspect list is short and recorded below.

Everything here is Windows (`C:\craton\CratonVM`, Docker Desktop 29.6.1,
Linux engine), binary built from `dev` at `53faa8541` unless stated.

## 1. It is not a credentials defect

`WrongCredentialsTest` opens a session with a deliberately wrong password and
expects a prompt `ServiceException`. The original filing suggested it might be
"a related connection/authentication-path defect given the class's purpose",
alongside the SASL/SCRAM handshake bug.

It is not. The test hangs **before the database is ever contacted**. The
in-VM watchdog (`CRATONVM_DEFAULT_WATCHDOG_SEC=75`) dumps 188 threads, and the
two that matter are:

```
tid=6  "vert.x-worker-thread-0"
  org/testcontainers/containers/wait/strategy/LogMessageWaitStrategy.waitUntilReady
  <- org/testcontainers/containers/output/WaitingConsumer.waitUntil

tid=12 "docker-java-stream-130715659"  blocked=true
  com/github/dockerjava/transport/NamedPipeSocket$AsynchronousFileByteChannel.read
  <- java/nio/channels/Channels$1.read
  <- …/SessionInputBufferImpl.fillBuffer
  <- …/ChunkedInputStream.getChunkSize
  <- …/FramedInputStreamConsumer.accept
  <- …/DefaultInvocationBuilder.lambda$executeAndStream$1
```

The test is waiting for Testcontainers to decide the Postgres container has
started. Its stdout stops at `Container docker.io/postgres:18.4 is starting:
<id>` and nothing follows for the full 120 s.

## 2. Why the earlier triage put this class in the wrong bucket

The filing separated this class from the dominant Windows pattern
(`testcontainers-jackson-jit-stall-blocks-eventloop-20260812`) on the grounds
that its `@BeforeEach` **succeeds**, where that pattern times out in `before()`.

`WrongCredentialsTest` overrides `before()` to do **nothing**:

```java
@Override
public void before(VertxTestContext context) {
    // We need to postpone the creation of the factory so that we can check the exception
}
```

So the container start — the thing that stalls — happens inside the test
method rather than in `before()`. This class **cannot** produce a
`before() timed out` signature no matter what is wrong with it. The signature
that separated it from the other 176 classes is one it is structurally
incapable of emitting, so it never separated anything. Whether this is the
same underlying defect as the rest of the Windows FAIL bucket is open, but it
can no longer be assumed distinct.

## 3. Not reproducible on Linux — and that is a fact about the transport

| host | result |
|---|---|
| Azure Linux (`azureuser@20.80.105.49`), real Docker | **PASS, 8.9 s** |
| Windows, HotSpot JDK 25, same loaded box | **PASS, 7.9 s** |
| Windows, CratonVM (G1 and ZGC) | FAIL, 120 s timeout |

The Linux pass is not a general clean bill of health, and per this project's
own rule a green platform cannot close a bug the other platform owns. It is
informative for a specific reason: on Linux docker-java reaches Docker over a
**Unix domain socket**, and on Windows over a **named pipe** driven by
`AsynchronousFileChannel`. The Linux run does not execute the code the Windows
run is parked in.

## 4. A real, HotSpot-differential defect on that path (measured, unfixed)

`java.nio.channels.AsynchronousFileChannel` is a fabricated stand-in in
CratonVM. `native_afc_open` (`native-io/src/lib.rs`) allocates a 3-field object
and only `open`/`isOpen`/`size`/`close`/`read`/`write` are registered. Two
observables, from `probes/` and diffed against real HotSpot on the same box:

| observable | HotSpot | CratonVM |
|---|---|---|
| `getClass().getName()` | `sun.nio.ch.WindowsAsynchronousFileChannelImpl` | `java.nio.channels.AsynchronousFileChannel` — the **abstract** class |
| `read(dst, pos, att, handler)` returns in < 2 s on an idle pipe | `true` | **`false`** |

The second is a contract violation with the mechanism written into the source:

```rust
// Perform the read synchronously (real async would use thread pool)
let result = native_afc_read(ctx, &read_args);
```

`AsynchronousFileChannel.read(ByteBuffer, long, A, CompletionHandler)` is
specified to start the read and return. CratonVM performs it on the calling
thread, so on a pipe with no data yet the *submit* blocks without bound. This
is invisible on a regular file — the data is always there — and unbounded on a
pipe, which is why nothing caught it before Testcontainers.

**This is worth fixing on its own, but it is NOT established as the cause of
the hang**, and the reasoning matters: docker-java reads through
`Channels.newInputStream`, which blocks on the returned `Future` anyway. Making
the submit asynchronous moves where the thread waits; it does not by itself
make the bytes arrive. CratonVM already owns the machinery to do it properly —
`native-io/src/async_socket.rs` has a worker pool, `HandlerRoots` GC roots and
a VM-attached completion dispatcher that `AsynchronousSocketChannel` uses.

## 5. A second defect, found here and FIXED

The slf4j shim rendered any non-String `{}` argument as `Object@<argument
index>`:

```rust
Some(Value::Object(Some(obj))) => ctx
    .read_string(*obj)
    .unwrap_or_else(|| format!("Object@{:x}", param_idx)),
```

Not an identity hash — the argument index. Three unrelated objects in one
Testcontainers startup log all rendered as `Object@2` because each was the
first `{}` of its line:

```
Image pull policy will be performed by: Object@2
... you must set 'testcontainers.reuse.enable=true' in a file located at Object@2
```

SLF4J's contract is `String.valueOf(arg)`. `slf4j_render_arg`
(`native-builtins/src/logging_shims.rs`) now dispatches the argument's own
`toString()`, pinning every object argument across the call because rendering
one re-enters Java and can move the rest, and falling back to
`ClassName@hash` — `Object.toString()`'s own shape — when dispatch cannot
answer. Verified on the real workload: that line now reads

```
... in a file located at C:\Users\Victor\.testcontainers.properties
```

Every diagnostic with a non-String payload was losing its payload. This is why
the original filing's "no raw log inspection of what the test method is doing"
was harder than it should have been.

## 6. Hypotheses eliminated

Each was tested with a `key=value` probe run under CratonVM and real HotSpot on
the same box, against a container printing one line a second. **All of these
matched HotSpot exactly**, so none of them is the defect:

| hypothesis | probe | result |
|---|---|---|
| timed `poll` never times out, so the wait loop never re-checks its clock | `LinkedBlockingQueue`/`Deque`/`ArrayBlockingQueue`/`SynchronousQueue`.`poll(150ms)`, `Object.wait(150)`, `LockSupport.parkNanos` | all return on time on both |
| the pipe transport cannot follow a log stream | write the real `GET /v1.44/containers/<id>/logs?...&follow=true` and read 8 times | 8/8 reads, log text seen, both |
| two channels share one pipe handle, so pooled connections collide | follow on A while issuing `/v1.44/version` on B | both independent, both |
| a reused keep-alive connection breaks the follow | 5 request/response cycles, then follow on the same channel | 6/6 follow reads, both |
| the read fails only off the main thread, or only under GC pressure | follow on a dedicated thread with a concurrent allocation churn thread | 8/8 reads, both |
| the failure is in `Channels.newInputStream`'s reused wrapped buffer | read through `Channels.newInputStream(AsynchronousByteChannel)` at rotating offsets | 8/8 reads, both |

## 7. What the failing run actually shows

With `CRATONVM_DBG_AIO=1` and a payload dump on every pipe read/write, the
whole Docker conversation is legible. It is healthy right up to the end:

```
write id=2 GET /v1.44/containers/<id>/logs?stdout=true&stderr=true&follow=true&since=0
read  id=2 n=234  :: HTTP/1.1 200 OK ... Transfer-Encoding: chunked
read  id=2 n=3685 :: 38e\r\n\x01\x00...The files belonging to this database system...
read  id=2 want=8192 ENTER          <- never returns
```

Exactly **one** dangling read across the run (24 read submits, 23 completions),
so this is not lock contention on the handle. The container is `running` and
its log already contains `database system is ready to accept connections`
1.5 s in, while the read stays blocked for the full 120 s. So Docker has the
bytes and the reader does not get them — yet every isolated reconstruction of
that same read works.

The gap between §6 and §7 is the open question: the transport works in every
shape that has been built by hand, and does not work in the workload.

## 8. Next steps

* Dump the payload of every read **and** the bytes actually delivered into the
  Java `ByteBuffer`, not just what the OS returned. §7 shows what came off the
  pipe; it does not show what the client received. A read that consumes from
  the pipe and then fails to hand over all of it is unrecoverable and looks
  exactly like this.
* Log the handle id assigned by `native_afc_open` next to the path. Two channels
  are opened and every operation in the run uses `id=2`; whether `id=1` is
  genuinely idle or is a second connection that lost its handle is unresolved,
  and §6's two-channel probe only shows that two channels *can* be independent.
* Fix §4 regardless, through the `async_socket.rs` worker pool and dispatcher.
* Re-check whether this class and the other 176 Windows FAILs are one defect —
  §2 removed the only evidence that they are separate.

## Repro

```bash
cd apps/hibernate-reactive-suite-runner
echo org.hibernate.reactive.WrongCredentialsTest > /tmp/one.txt
CV_BIN=bin/cratonvm-hibreactive-g1.exe bash run-hibernate-reactive-suite.sh \
  --list /tmp/one.txt --gc g1 --shards 1 --timeout 300 --out /tmp/repro
```

For the stack dump, bypass the runner (it forces
`CRATONVM_DISABLE_DEFAULT_WATCHDOG=1`):

```bash
CRATONVM_DEFAULT_WATCHDOG_SEC=75 <cratonvm> --java-home <jdk25> --Xmx 1500m \
  -XX:+UseG1GC @common.args -Dcraton.batch=1 CratonRunner \
  org.hibernate.reactive.WrongCredentialsTest
```

For the pipe trace, add `CRATONVM_DBG_AIO=1`.

## Related

- `testcontainers-jackson-jit-stall-blocks-eventloop-20260812-FIXED.md`
  — the pattern this class was separated from on a signature it cannot emit
  (§2).
- `vertx-pg-sasl-scram-handshake-fails-20260812-FIXED.md`
  — the authentication defect this was guessed to resemble; §1 rules it out.
- `azure-linux-residual-3-classes-20260813.md`
  — the Linux run in which this class passes (§3).
