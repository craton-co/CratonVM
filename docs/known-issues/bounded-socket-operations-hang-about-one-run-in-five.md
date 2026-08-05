# Bounded socket operations hang indefinitely about one run in five

**Status:** **DIAGNOSED AND FIXED 2026-08-05.** Root cause is a read-lock
re-entry against a waiting writer in `net_poll`
(`native-io/src/net.rs`), not a dropped timeout. Retained here rather than
retired because the fix's own confirmation run is recorded below and the
"what it is not" section is still the guide for anything of this shape.

Originally found 2026-08-04. **Pre-existing on `dev` and mode-independent** —
reproduced on an unmodified `dev` binary under `--real-jdk`. Never a
`--jdk-only` defect; found while validating one.

## What happens

`probes/JdkOnlyCensusLoadProbe.java`'s `net` section opens a loopback
`ServerSocket`, accepts on a daemon thread, connects, reads four bytes and
joins. **Every** blocking call in it is bounded:

```java
srv.setSoTimeout(4000);
c.connect(new InetSocketAddress("127.0.0.1", port), 4000);
c.setSoTimeout(4000);
...
t.join(2000);
```

Roughly one run in five never returns from that section. Not "takes a long
time" — the run was still alive at a 45-second kill, having printed everything
up to and including the preceding `nio` line and nothing after it. No
`SECTION-FAILED`, no `SocketTimeoutException`, no stack trace: the section that
cannot take more than ~10 seconds by construction simply does not finish.

## The numbers

Eight runs per arm, JDK 25 image, Linux, host load average 19–34 on 16 cores:

| binary | mode | completed 9/9 | hung | other |
|---|---|---:|---:|---:|
| branch | `--jdk-only` | 6 | 1 | 1 |
| branch | `--real-jdk` | 6 | 2 | 0 |
| `dev` | `--jdk-only` | 0 | 0 | 8 |
| `dev` | `--real-jdk` | 6 | **2** | 0 |

**HotSpot 25 control, 12 runs at load average 22–23: 12 clean, 0 hangs.**

That control is what makes this a defect rather than an observation about a busy
machine. The load is real and high, and the temptation to write the hang off as
contention was strong — but the same probe, on the same host, at the same load,
under HotSpot, does not hang once in twelve.

The `dev` / `--jdk-only` row is a *different* defect and is fixed:
[§7 step 3 fell through to `UnsatisfiedLinkError`](../internal/jdk-only-section7-step3-unsatisfiedlinkerror-FIXED-20260804.md).
Those eight runs exit 0 with `failed=2` and 16 `UnsatisfiedLinkError` lines. It
is listed here only so the table is not mistaken for four samples of one thing.

## The diagnosis (2026-08-05)

The record's own "cheapest next step" was a thread dump, and it was right.
Twenty-five runs with the VM's own watchdog armed inside the outer bound —
`--stack-dump-on-timeout=45` under `timeout 90` — hung **6 times**, matching
the one-in-five rate, and every hung run aborted with a frame dump instead of
being `SIGKILL`ed with nothing to read.

**All six dumps are identical.** Last frame per thread:

```
t=0   NAT sun/nio/ch/Net.socket(Ljava/net/ProtocolFamily;Z)...  ->NATIVE sun/nio/ch/Net.socket0(ZZZZ)I
t=2   NAT sun/nio/ch/NioSocketImpl.park(Ljava/io/FileDescriptor;IJ)V  ->NATIVE sun/nio/ch/Net.poll(Ljava/io/FileDescriptor;IJ)I
```

Six for six is a signature, not a sample. And it immediately rules out four of
the five candidates this record used to list: the accept thread is in
`Net.poll`, and the *other* thread is not in any of them — it is in
`Net.socket0`, which creates nothing and cannot block.

`net_poll` classified the fd under a read guard on the process-wide
`net_sockets()` map and then wrote

```rust
Some(NetSocketHandle::Listener(listener)) => {
    let listener = Arc::clone(listener);
    return net_poll_listener(ctx, &listener, fd, events, timeout_millis);
}
```

from inside that guard's scope. Rust evaluates the call **before** unwinding
the scope, so the entire listener park ran holding the read guard — and
`net_poll_listener`'s loop re-acquires the same `RwLock` on every 50 ms slice
via `net_listener_still_registered`.

`parking_lot::RwLock` is writer-preferring. So:

1. accept thread takes read guard #1 and parks in the listener poll;
2. main thread calls `Net.socket0` → `register_handle` → `write()`, which queues
   behind guard #1;
3. accept thread's next slice calls `read()`, which queues behind the writer.

One thread, two read guards, a writer wedged between them. The loop's deadline
is never re-evaluated, so a 4-second bound becomes forever. It needs `socket0`
to land in the window between the two reads, which is exactly why it is one run
in five rather than every run — and why no timeout was ever being dropped.

**Fix:** classify, drop the guard, then wait. That is the discipline
`net_accept`, `net_read0` and `net_write0` already document in this same file
under "AUDIT 2026-05-17" ("clone the per-listener `Arc<Mutex<_>>`, drop the map
lock, then perform the blocking accept"). `net_poll` was the one path that
missed it.

## The A/B, and the first one that proved nothing

**Result: pre-fix 13/60 hung, post-fix 0/60.**

The first attempt was 30 sequential runs per arm on a quiet host and returned
**0/30 on both arms — including the pre-fix binary**. That is not a passing
A/B, it is a failed reproduction, and reporting it as a fix would have been the
"aggregate that is not a before/after" this feature keeps producing. The race
needs `Net.socket0` to land inside a window the accept thread opens twice per
50 ms slice; at load 14 it is never hit, and the original evidence was taken at
load 60–160.

So the second attempt generates its own contention — 10 concurrent copies of
the probe per wave, six waves — and **alternates the arms within each wave**.
Running one arm to completion and then the other is how the first attempt ended
up comparing two different machines.

| wave | pre-fix cumulative | post-fix cumulative |
|---|---|---|
| 1 | 3/10 | 0/10 |
| 2 | 5/20 | 0/20 |
| 3 | 5/30 | 0/30 |
| 4 | 5/40 | 0/40 |
| 5 | 7/50 | 0/50 |
| 6 | **13/60** | **0/60** |

Same host, same minute, same load, same class files. The pre-fix arm hung in
every single wave and the post-fix arm never did.

## What it is not

Every one of these was measured, and all four still stand:

* **Not host load.** See the HotSpot control.
* **Not the probe.** Same class file in every arm.
* **Not `--jdk-only`.** The `dev` binary under `--real-jdk` hangs at the same
  rate, and that binary predates every change on the branch that found it.
* **Not a timeout that is merely too short.** A short timeout produces
  `SocketTimeoutException` and a `SECTION-FAILED` line, which is exactly what
  the `dev` strict runs show. A hang produces neither. The diagnosis above
  explains why: the deadline is never reached, not exceeded.

## Why it matters beyond this probe

A dropped socket timeout is invisible in exactly the way that matters: the
caller asked for a bound precisely because it intended to survive the peer
misbehaving, and instead the thread is gone for good. Any suite that opens a
socket with a timeout — Tomcat, the H2 server tests, anything with a health
check — can lose a worker this way and report it as a slow run. The lock this
one wedges is process-wide, so the casualty is not even the socket's own
thread: here it was a thread doing nothing but creating a new socket.

## The lesson worth keeping

The evidence that closed this was one flag. `--stack-dump-on-timeout=N` set
*inside* an outer `timeout` turns "the run stops after the nio line", which
names no call site, into a frame dump that names two — and it reproduced in
under a minute. Reach for it before reading code whenever a hang has more than
two candidate sites.
