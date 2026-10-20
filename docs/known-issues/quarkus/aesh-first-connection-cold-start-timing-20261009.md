# Quarkus aesh: the first remote connection of a JVM misses two tests' hard-coded time budgets — 2026-10-09

| | |
|---|---|
| **Status** | OPEN, performance (cold-start latency), not a correctness defect |
| **Classes** | `io.quarkus.aesh.websocket.deployment.AeshWebSocketMalformedMessageTest` (2 of its 3 tests pass; the one that runs first in the JVM fails), `io.quarkus.aesh.ssh.deployment.AeshSshIdleActivityResetTest` |
| **Measured** | 2026-10-09, `--jdk-only` launcher, release build, Windows 11, JDK 25.0.3; both classes PASS on HotSpot 25.0.3 (9 s and 17 s wall); both fail identically on every CratonVM run (3 of 3 and 2 of 2), alone and in sequence |
| **Found by** | the per-module suite run recorded in `docs/internal/fixed-suite-bugs/quarkus/nonpassed-classbyclass-census-FIXED-20261009.md` |

## `AeshWebSocketMalformedMessageTest`

Each of its three tests opens one WebSocket, sends `init`, waits a fixed 500 ms (test timers), sends
`hello\r`, and waits at most 10 s for a message containing `Hello!`. On CratonVM the output is only
`[quarkus]$ hello` and a fresh prompt, never `Hello!`. With `-Dquarkus.log.level=DEBUG` the cause is one
line, logged by `org.aesh.console.ReadlineConsole` at SEVERE on the connection's `aesh-ws-0` thread:

```
Got exception while starting new process: java.lang.NullPointerException:
  Cannot invoke "org.aesh.console.ProcessManager.execute(...)" because "this.processManager" is null
```

`ReadlineConsole.accept(Connection)` (aesh 3.16.4) calls `read(connection, readline)`, which
attaches the stdin handler and replays input the decoder already holds, **before** it assigns
`this.processManager = new ProcessManager(this)` a few instructions later. If `hello\r` is already queued
when `accept` reaches `read`, the line executes with a null `processManager`. On HotSpot the
connection setup (`accept`, the Windows terminal probe `reg query`, class loading of aesh and readline)
finishes in about 100 ms, far inside the test's 500 ms, so the queue is empty. On CratonVM it takes
about 1 s of cold start (interpreted class loading and linking of the aesh/readline classes), so the
input is queued first. The two other tests of the class pass because the classes are warm by then. This is
an upstream race made visible by latency; with a 100 ms-slower HotSpot the test would fail too.

## `AeshSshIdleActivityResetTest`

The server idle timeout is 2 s, the test sends `hello` every 1.5 s for four rounds. The session activity
stamp (`IdleTrackingConnection.lastActivityMs`) is refreshed only inside the wrapped stdin handler, which
readline re-installs per prompt; the first command on a cold connection takes long enough that the idle
checker (period 1 s) closes the channel (`SshChannelClosedException: ... channel already closed`) before the
second `hello` is written.

## What would close it

Both are the same fact: the first aesh connection costs ~10x HotSpot's wall time because the VM runs its
bytecode in the interpreter while thousands of classes load. A faster first-use path (class loading and
linking throughput, warm-up of the interpreter for the readline/aesh working set, or an AOT cache for the
suite) closes them; nothing in the suite or in Quarkus is wrong in a way CratonVM can fix by itself.
Re-measure with `modrun.sh` on the two classes of `list-B.tsv`. When both pass, move this page to
`docs/internal/fixed-suite-bugs/quarkus/`.
