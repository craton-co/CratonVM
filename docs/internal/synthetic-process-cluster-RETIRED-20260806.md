# The `cratonvm/synthetic/Process` cluster — RETIRED, `--jdk-only` returns a real `java.lang.ProcessImpl`

**Status:** CLOSED 2026-08-06. Both halves are fixed and measured. Retired from
`docs/known-issues/jdk-only/synthetic-process-cluster-and-the-supertype-lie.md`.

Under `--jdk-only`, `ProcessBuilder.start()` now returns a real
`java.lang.ProcessImpl` and the whole `java.lang.Process` surface is
**byte-identical to HotSpot 25**. Compatible `--real-jdk` mode is byte-identical
to the build before the change and still answers with the VM's own process
object — which is the difference the two modes exist to express.

```
probes/RealProcessSurfaceProbe — 21 lines, HotSpot 25 vs cratonvm --jdk-only

  class=java.lang.ProcessImpl          stdinRoundTrip=hello-stdin
  super=java.lang.Process              timedWaitFalse=false
  isProcess=true                       destroyEndsIt=true
  pidPositive=true                     onExitCode=3
  handlePidMatches=true                merged=A=true,B=true
  stdout=out-line                      mergedErrStreamEmpty=true
  stderr=err-line                      redirectOutputToFile=file-line
  exitCode=7                           redirectInputFromFile=from-file
  aliveAfterExit=false                 inheritExit=0
  exitValue=7                          emptyArgKept=[][z]
                                       noSuchCommand=IOException
                                       DONE
```

`SubprocessKindProbe`, `SubprocessSubtypeProbe` (all nine subtype questions,
including the hand-walked `getSuperclass()` chain) and `UserProcessInterceptProbe`
are likewise byte-identical to HotSpot under `--jdk-only`.

The §5 measurement the old record's step 3 asked for, from
`--jdk-only-report` on the subprocess workload:

| | before | after |
|---|---:|---:|
| classes fabricated in strict mode (`compatibility_classes`) | 4 | **0** |
| boot-image classes loaded | 493 | 542 |

The four were `cratonvm/synthetic/{Process, ProcessPipeInputStream,
ProcessPipeOutputStream, ProcessExitWaiter}` — the exact substitution §5
forbids, running in the mode named after forbidding it.

## What it actually took: five things, four of them not in the plan

The old record's plan was "retag the 37 rows". Retagging was necessary and was
about a third of the work. Each of the other four was found by running the
thing, not by reading it, and each hid the next.

### 1. The rows, retagged (planned)

Contract §1.5 defines a `Bridge` as what an `ACC_NATIVE` method binds to.
`cratonvm/synthetic/{Process, ProcessPipeInputStream, ProcessPipeOutputStream,
ProcessExitWaiter}` and `cratonvm/synthetic/AnonymousObject$2` are in no JDK
image on any platform, so there is nothing for these to bridge to; every one was
`Bridge` by inheritance from an ambient `set_category` and by nothing else.
Restated as `SyntheticStub`, strict mode drops them at registration.

The adjudication opened with **37**. It landed on **29**: `58f0ffb30` deleted
the other eight — `phases_late`'s duplicate `cratonvm/synthetic/Process` surface,
superseded by `native-io`'s and reading pid from a slot that is now the stderr fd
— while this change was in flight.

### 2. `ProcessImpl.forkAndExec` had to be written, and its `int[] fds` is the whole point

The registration that existed was on `java.lang.UNIXProcess` — the pre-JDK-9
name, on no supported image, so it could never resolve. Its decoder read
parameters from index 0, i.e. off by one throughout, because `forkAndExec` is an
*instance* method and nothing had ever called it to notice.

The live one is `java/lang/ProcessImpl.forkAndExec(I[B[B[BI[BI[B[IZ)I`, a
genuine §1.5 bridge (`acc_native: true` on the image). Its contract, from the
JDK's own javadoc: *"On input, a value of -1 means to create a pipe... On
output, a value which is not -1 is the parent pipe fd... An element of this
array is -1 on input if and only if it is not -1 on output."*

Writing that array back is not bookkeeping. It is the only channel by which the
child's pipes reach `initStreams`, so a native that spawns the child perfectly
and leaves the array alone hands back a live process whose three streams are all
`ProcessBuilder.Null*Stream` — a child that looks hung or mute, with nothing
pointing at the missing write-back.

Two details worth keeping:

* The values are **`FdTable` ids, not OS descriptors**. The JDK builds them with
  `fdAccess.get(fis.getFD())`, and in this VM a `FileInputStream`'s `fd.fd`
  holds a table id. Ids 0/1/2 are permanently the VM's own standard streams and
  the counter starts at 3 — which is exactly what `Redirect.INHERIT` writes into
  slots 0, 1 and 2. "Inherit" and "this descriptor" name the same three streams,
  so the two readings cannot be confused.
* `argc`/`envc` are passed for a reason. The old decoder split the block on NUL
  and dropped every empty piece, which also drops a legitimately empty argument.
  `printf '[%s]' "" z` printed one field instead of two.

### 3. The VM's handle is not the pid — and a sibling session fixed it first

`NEXT_HANDLE.fetch_add` versus `child.id()`. Invisible while the only callers
were the VM's own `Process` natives, which carry the handle in a field — but the
real `ProcessImpl` never sees a handle. It keeps the **pid** and calls
`ProcessHandleImpl.{waitForProcessExit0, isAlive0, destroy0, parent0}` with it,
so every one of them looked up a handle that was never minted.

This branch built a `pid_index` for it. `58f0ffb30` landed `handle_for_pid` for
the same reason, with the same four callers, while it was in flight — so the
duplicate was dropped at merge and dev's stands.

**Dev's is also right where this branch's was wrong, and the difference is
exactly the bug dev measured.** `isAlive0` returns a start time, not a boolean:

```java
long startTime = isAlive0(pid);
return startTime >= 0 && (startTime == this.startTime
                          || startTime == 0 || this.startTime == 0);
```

so any value >= 0 means alive and -1 is the only way to say otherwise. This
branch returned **0 for an exited child**, which is >= 0, and every handle
`build_process_handle` mints has `this.startTime == 0` — so the third disjunct
would have made `ProcessHandle.isAlive()` answer `true` for a child that had
already been reaped. Dev's convention (alive 0, exited -1) is correct.

Worth naming why this branch's own probes did not catch it:
`RealProcessSurfaceProbe` asks `Process.isAlive()`, which on a real `ProcessImpl`
is a field read behind a lock and never reaches the native. The failing question
is `p.toHandle().isAlive()`, which dev's `probes/ProcHandleProbe.java` asks and
this branch's does not. Two probes over the same subsystem, one blind spot each.

### 4. `destroy()` silently stopped working, because the reaper always holds the child

`ProcessImpl`'s constructor ends in `ProcessHandleImpl.completion(pid, true)`,
which puts a reaper thread into `Child::wait()` for **every** child, from the
instant it is spawned. The process table used to hand the `Child` *out* to
whoever waited on it, so from a later `destroy()`'s point of view every child
was permanently missing: `destroy` became a no-op, and

```java
Process p = new ProcessBuilder("sleep", "30").start();
p.destroy();
p.waitFor();     // blocked the full 30s and returned 0, not 143
```

The table now holds `Arc<Mutex<Child>>`. A waiter clones the `Arc` and releases
the table lock; a killer takes the child's mutex with `try_lock`, and **failing
that lock is information, not an obstacle**: someone is inside `Child::wait()`,
therefore the child has not been reaped, therefore its pid is still its own and
signalling it directly is safe. That is what makes the pid-signal fallback
correct rather than a race with pid recycling.

### 5. Three `java.io` shims skipped `closeLock`, and one answered `available()` wrongly

Neither is a Process defect. The real `ProcessImpl` is simply the first strict-mode
caller to walk these paths.

**`closeLock`.** `BufferedOutputStream(OutputStream)`, its sized twin,
`FilterOutputStream(OutputStream)` and `FileInputStream`/`FileOutputStream(FileDescriptor)`
each had a shim that assigns the wrapped stream and stops. Each real constructor
also initializes `private final Object closeLock = new Object()`, and every
matching `close()` opens with `synchronized (closeLock)`. So every stream built
through a shim throws **NullPointerException, not IOException**, on its first
close — and `ProcessImpl.destroy`'s `try { stdin.close(); } catch (IOException
ignored)` cannot absorb an NPE. Restated as stubs; strict mode runs the real
two-line constructors.

**`available()`.** `FileDescriptorTable::available` answered `Err("bad fd")` —
which the native maps to 0 — for the three subprocess pipe entries. Zero there
is not "no data"; it is a wrong answer that destroys data.
`ProcessPipeInputStream.processExited()` drains the pipe with
`while ((j = in.available()) > 0)` and then **closes it**, installing whatever it
drained as the stream's new source. A 0 ends the loop immediately, so the child's
output is replaced by `NullInputStream` and the application reads EOF from a
child that printed perfectly well. The reaper runs that the moment the child
exits, so the shorter the child, the likelier it wins: `sh -c 'echo out-line'`
lost its output every time. Now answered with `FIONREAD`.

## The cluster had to move together

`java/lang/ProcessBuilder` is 11 registrations, every one shadowing ordinary
bytecode (`acc_native: false, has_code: true` for all eleven), so §1.4 gives the
real method precedence and none of them is a bridge. Restating `start()` alone
leaves `<init>([Ljava/lang/String;)V` writing a raw `String[]` into the `command`
field; the JDK's own `start()` then reaches `command.toArray(...)` on an array
and dies with `AbstractMethodError: java/util/List.toArray has no Code
attribute`. That is measured — it is what the first build with only `start`
restated did.

## Counts

45 registrations changed kind; **none of them is new**. The stub ratchet is
re-frozen 644 → 689 and the kind-map baseline re-frozen from the same census.

| group | rows |
|---|---:|
| `cratonvm/synthetic/Process*` + `AnonymousObject$2` (29 of the record's 37; dev deleted 8) | 29 |
| `java/lang/ProcessBuilder` | 11 |
| `java.io` constructors | 5 |

The kind-map gate also carried **2 rows of dev's** — `ExecutorService`/
`ThreadPoolExecutor.execute`, whose retirement landed without re-freezing this
baseline. The gate was already red on this branch's base; verified by scoring
the pre-change census against it. They are absorbed here because both baselines
must be frozen from one census.

## The supertype lie (fixed earlier the same day, kept for the record)

`fabricate_class` gives `cratonvm/synthetic/Process` a real `java.lang.Process`
superclass. Before it, `isAssignableFrom` said yes and the reflective hierarchy
said no, in the same VM about the same pair of classes — the shape serialization
frameworks, DI containers, matchers and mock frameworks use to decide
assignability. That fix still matters: it is what compatible mode returns.

**A flake surfaced during it, and it was mine.** `Process.onExit()`'s default is
`CompletableFuture.supplyAsync(this::waitForInternal)` — it calls the subclass on
a pool thread, so whether it has bumped the probe's call counters by print time
is an unbounded race. It showed up once as HotSpot reporting
`concrete.count.exitValue=2` against `1` on twelve other runs, which reads
exactly like a regression in whatever change is under test. The counters are now
snapshotted before that rung and the source carries the ordering requirement.

## What this leaves open

* **`java/lang/Process`'s own 21 registrations are still `Bridge`.** They did not
  need to move: a real `ProcessImpl` overrides every one of them and the
  most-derived override wins, which is why the probes are byte-identical with
  those rows in place. They remain a §1.4 question for the reclassification wave.
* **`native-shadows-bytecode` violations rose 93 → 155 on this workload.** Not
  new shadows: 49 more boot-image classes now load, so more registrations get
  adjudicated against real bytecode. The rise is the census seeing further, which
  is what it is for.
* **`ProcessBuilder$Redirect.PIPE`/`INHERIT`** are registered as methods the
  image does not declare (`Redirect.PIPE` is a field). Dead registrations, not
  reached by anything; left for a dead-row sweep.
* **`waitForProcessExit0` answers -1 for a pid it did not spawn, where HotSpot's
  own native answers `NOT_A_CHILD` (-2) on `ECHILD`.** The difference is real:
  -1 completes `ProcessHandle.of(pid).onExit()` immediately with a fabricated
  exit status of -1, while -2 makes the JDK poll `isAlive0` until the process
  actually ends and then complete with 0. It only differs for a handle to a
  process this VM did not spawn, no probe covers that path, and the code is
  `58f0ffb30`'s and freshly probe-verified — so it is recorded here rather than
  changed unmeasured. Fixing it is a one-line change plus a probe rung that
  holds a foreign pid.
* **`native-builtins/src/lib.rs`'s synthetic-mode `ProcessBuilder` block** —
  `<init>` ×2, `command`, `start` — still carries the ambient `Bridge`. That
  registrar is not on the default boot path, so no gate here measures it and the
  strict-mode counts above are unaffected; it is the same adjudication as the 11
  `phases_late` rows and should move with them in the reclassification wave.
