# `AsynchronousFileChannel.close()` waited for the read it was meant to cancel

**Status: FIXED (2026-08-16).** `WrongCredentialsTest` 96 s FAIL → 11–14 s PASS,
and the Windows hibernate-reactive FAIL bucket with it: **18/18 on an A/B
sample, 186 PASS of 238 on the full list** (44 of the rest have no tests at
all). Retired from `known-issues/hibernate-reactive/` on 2026-08-17.

Everything below is Windows 11 (`C:\craton\CratonVM`, Docker Desktop 29.6.1,
Linux engine, JDK 25 Adoptium), branch `fix/afc-namedpipe-and-young-walk-20260816`.

## The defect

`docker-java`'s `NamedPipeSocket` drives `\\.\pipe\docker_engine` entirely
through `AsynchronousFileChannel` — its `connect()` is
`AsynchronousFileChannel.open(path, READ, WRITE)` and both its streams are
`Channels.new{Input,Output}Stream` over the same channel. Testcontainers waits
for a container by following its log with `follow=true`.

When the container goes quiet the follow read parks in `ReadFile`. That is
CORRECT — there are no bytes. The wait strategy, having already matched its
line, then calls `close()` on the stream.

`close()` could not run:

```rust
fn afc_read_at(id: u32, buf: &mut [u8], position: u64) -> io::Result<usize> {
    let entry = afc_file_entry(id)?;
    let mut handle = entry.lock();          // <-- held across the blocking read
    ...
    let read_result = handle.file.read(buf);
```

```rust
fn afc_remove_file(id: u32) {
    if let Some(entry) = afc_files().lock().remove(&id) {
        let delete_on_close = entry.lock().delete_on_close.clone();   // <-- waits
```

Two separate faults, and the second is the worse one:

1. **`close()` waits for the parked read.** The emulation needed `&mut File` to
   `seek` / `read` / `seek` back, so every operation took the per-handle
   `Mutex` for the whole of a genuinely blocking OS call. `close()` needed the
   same mutex.
2. **It waits holding the PROCESS-GLOBAL handle table.** Under edition 2021 the
   temporary `MutexGuard` in `if let Some(entry) = afc_files().lock().remove(&id)`
   lives for the whole `if let`, body included. So while `close()` waits, every
   `AsynchronousFileChannel` operation in the process — including `open` —
   waits with it.

Measured on the repro, with the handle id logged next to every operation:

```
03:06:39  read id=2 → 406 bytes :: 18f\r\n…time=…level=INFO msg=starting connection_timeout=1m0s
03:06:39  read id=2 want=8192 ENTER          <- parks; Ryuk has nothing more to say
03:06:40  close id=2                          <- queues behind it, holding the table
03:07:39  read id=2 → 327 bytes :: …msg="prune check"…   <- Ryuk finally prints
03:07:39  open  id=3                          <- 60 s after it was asked for
```

Testcontainers reports that as `Container testcontainers/ryuk:0.12.0 started in
PT1M0.99S` and then `Could not connect to Ryuk at localhost:60499`. On the
hibernate-reactive classes that reach a database container the quiet stream is
Postgres — which stops logging once it is ready — so the freeze lasts until the
120 s JUnit timeout, which is the "hang" this page was filed for.

## The fix

Stop needing the lock rather than shortening it.

`read(dst, position, …)` is a POSITIONAL read by definition, and both platforms
expose that as a `&self` call: `FileExt::seek_read`/`seek_write` on Windows,
`read_at`/`write_at` on Unix. `AfcFileHandle` is now immutable and lock-free,
and the I/O path takes no lock at all. That is also strictly more correct than
the old emulation, which mutated a file pointer shared by every concurrent
operation and restored it afterwards.

`close()` now takes the entry out of the table, **releases the table lock**,
sets the close flag, and calls `CancelIoEx` — which cancels every outstanding
request on a handle regardless of the issuing thread, a parked synchronous
`ReadFile` included. The parked read returns `ERROR_OPERATION_ABORTED` (995)
and reports `AsynchronousCloseException`, which is the contract
`AsynchronousChannel` states and which this code could never reach before.

Both facts were verified against the live pipe *before* the change was written,
with a 60-line probe: positional read/write work on `\\.\pipe\docker_engine`
(a real `_ping` exchange), and `CancelIoEx` from another thread unblocks a
parked read within a millisecond.

## What it is worth

Same box, same Docker daemon, back to back.

| arm | `WrongCredentialsTest` | Ryuk startup | `close` → `cancelled-pending-io` |
|---|---|---|---|
| HotSpot JDK 25 (control) | **PASS 7.5 s** | — | — |
| CratonVM, current `dev` | FAIL 96 s | `PT1M0.99S` | 60 s |
| CratonVM, fixed | **PASS 11.2 s** | `PT1.58S` | **0 ms** |

The HotSpot control matters: it was taken on the same box minutes apart, so
"the Ryuk container is flaky here" is excluded.

### The 176 Windows FAILs were one defect

Section 8 of the filing asked whether this class and the rest of the Windows
FAIL bucket are the same bug. They are.

`others.txt` head, 20 classes, `--gc g1 --shards 4`, control first:

| binary | result | wall |
|---|---|---|
| current `dev` | **FAIL 18**, NOTESTS 2 | 8m19s |
| fixed | **PASS 18**, NOTESTS 2 | 2m27s |

Then the whole 238-class list on the fixed binary: **PASS 186, NOTESTS 44,
FAIL 6, HANG 1.** The 44 have no test methods (`BaseReactiveTest` and friends).
What is left is a genuinely separate, much smaller residual:

```
FAIL  MultithreadedInsertionTest
FAIL  MultithreadedIdentityGenerationTest
FAIL  ORMReactivePersistenceTest
FAIL  it.LocalContextTest
FAIL  it.quarkus.qe.database.DatabaseHibernateReactiveTest
FAIL  techempower.TechEmpowerTest
HANG  MultithreadedInsertionWithLazyConnectionTest
```

Three of the seven are the multithreaded-insertion family, which is a different
shape and wants its own filing.

## Two sibling defects found on the way

### SLF4J's varargs overload dropped every argument but the first

`slf4j_log_msg` read `args[2..]` as the arguments. For
`info(String, Object...)` `args[2]` is the ARRAY, so the shim rendered the
array's `toString()` into the first `{}` and left every later placeholder
literal:

```
Can not connect to Ryuk at [Ljava.lang.Object;@2795a:{}      (CratonVM)
Can not connect to Ryuk at localhost:60499                   (HotSpot)
```

This is the same family as the 2026-08-14 `Object@<argument index>` fix and was
left behind by it: that one corrected HOW an argument is rendered, this one
corrects WHICH arguments there are. Every multi-argument SLF4J call in every
workload was losing all but its first — which is exactly why the failing run
could not be read, and it cost this investigation its first hour.

The trap worth remembering: **there are two registrars for `org/slf4j/Logger`
in `logging_shims.rs` and the LAST one wins.** The obvious block
(`register_slf4j_natives`) is the loser; the `for sig in [...]` loop in
`register_logging_natives` is the one that decides, and its own comment says
so. Fixing only the obvious one changes nothing — the regression test
(`m15_slf4j_varargs_spreads_the_array_across_every_placeholder`) still failed
with the identical message. log4j2's `Logger` has the identical shape and is
fixed too.

### `id=1` was never a lost handle

Section 8 asked whether the `id=1` that never appears in the trace is an idle
channel or a second connection that lost its handle. With the id logged at
`open`, it is neither:

```
open  id=1 read=true write=true path=\\.\pipe\docker_engine
close id=1
open  id=2 …
```

Testcontainers' provider-strategy probe opens a channel, pings, and closes it.
Nothing was lost.

## Section 8's other two questions, answered

**"Dump the bytes actually delivered into the Java `ByteBuffer`, not just what
the OS returned."** Done — every read now reads its own write-back out of the
buffer and compares. Every read in every run reports `delivered_ok=true`. The
transport was never losing bytes; the previous investigation could not see the
`close()` that was blocking, because nothing traced it.

**"Fix §4 regardless, through the `async_socket.rs` worker pool and
dispatcher."** Attempted, measured, and **deliberately not shipped** — see
below.

## §4, the two HotSpot differentials — one fixed, two measured and deferred

### The submit blocking without bound: mechanism fixed, contract not

The consequence §4 named is gone: a parked pipe read no longer owns a lock, no
longer blocks `close()`, and no longer freezes the process. What remains is
that the *submit itself* still returns only when the operation completes, so
the observable §4 states — `read(dst, pos, att, handler)` returning in under
2 s on an idle pipe — is still `false`.

The asynchronous submit was built (AIO worker jobs, `HandlerRoots`, a
`ReadBytes` completion that fills the destination buffer on the dispatcher,
`AsynchronousCloseException` carried through the worker-to-dispatcher string
channel) and it does work at the byte level: the whole Docker conversation
completes, and the trace shows completions delivered 5–10 ms after submit,
byte-identical to the synchronous arm.

It was still **worse**, three runs in a row:

| arm | `WrongCredentialsTest` | Ryuk startup | `close` latency |
|---|---|---|---|
| synchronous submit (shipped) | PASS 11.2 s | `PT1.58S` | 0 ms |
| async, shared worker pool | FAIL 116 s | `PT1M2.2S` | 58.5 s |
| async, one worker per channel | FAIL 100 s | `PT1M0.8S` | 58.5 s |

Two findings came out of it, and they are the reason this is a deferral rather
than an unexplained failure:

1. **The synchronous submit was serializing the pipe by accident, and the pipe
   needs it.** A named pipe has ONE byte stream and no positions; two
   overlapping reads race for the same bytes. The trace shows the async arm
   doing exactly that — two reads SUBMITTED 3 ms apart on the follow stream
   with no completion in between, which is impossible when the caller has to
   block. A per-channel serialized worker queue fixes the ordering and is in
   the branch history, but did not recover the run.
2. **Once operations are owned by workers, `close()` becomes slow again** — the
   timestamped trace puts 58.5 s between `close id=2` and its
   `cancelled-pending-io`, which is the same 60 s wait for the container's next
   log line that the original defect had, arriving through a different door.

So the correct asynchronous design for this family is per-channel ordering
**plus** a close path that is provably independent of any worker, and that is a
redesign rather than a fix. Trading a verified 186-class result for it is a bad
trade today. The kill switch, the jobs and the queue are all in the branch
history at `961a1aa7d..` if someone picks it up.

### The fabricated class name: not taken, and why

`getClass().getName()` reports `java.nio.channels.AsynchronousFileChannel` —
the ABSTRACT class — against HotSpot's
`sun.nio.ch.WindowsAsynchronousFileChannelImpl`. CratonVM instantiates the real
abstract JDK class with a 3-field synthetic layout.

Making it a concrete impl needs one of two things, both larger than the
divergence:

* a synthetic class that declares a REAL superclass, which the synthetic-class
  API has no way to express (`try_ensure_synthetic_class` takes a name and a
  field count); or
* adopting `SimpleAsynchronousFileChannelImpl`'s real field layout, which means
  implementing the JDK class rather than standing in for it.

Registering the natives under an impl name without the superclass link would
make every `invokevirtual AsynchronousFileChannel.read` on the receiver an
`IncompatibleClassChangeError` waiting to happen. Nothing in the suite branches
on the class name, and the sibling `AsynchronousSocketChannel` fabrication has
the same trait, so this is a family-wide fidelity gap and not this page's to
close.

## Reproducing

```bash
cd apps/hibernate-suite-runner
CRATONVM_DBG_AIO=1 <cratonvm> --java-home <jdk25> --Xmx 1500m \
  -XX:+UseG1GC @common.args -Dcraton.batch=1 CratonRunner \
  org.hibernate.reactive.WrongCredentialsTest
```

`CRATONVM_DBG_AIO=1` now traces the AFC family with a millisecond stamp and the
handle id on every line: `open`/`close`/`read`/`write`, the OS-level result,
and whether what landed in the Java `ByteBuffer` matches what the OS returned.
Reading `close id=N` against `close id=N cancelled-pending-io` is the whole
regression test by eye — they must share a timestamp.

## Related

- `testcontainers-jackson-jit-stall-blocks-eventloop-20260812-FIXED.md`
  — the pattern this class was separated from on a signature it cannot emit.
  Section 2 of the filing was right to doubt the separation.
- `vertx-pg-sasl-scram-handshake-fails-20260812-FIXED.md`
  — the authentication defect this was guessed to resemble; it was never
  reached.
- `azure-linux-residual-3-classes-20260813.md`
  — the Linux run in which this class passes. Linux reaches Docker over a Unix
  domain socket and never executes this code, which is why the platform split
  was real and why a green Linux could not close it.
