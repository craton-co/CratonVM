# Bounded socket operations hang indefinitely about one run in five

**Status:** OPEN, found 2026-08-04. **Pre-existing on `dev` and
mode-independent** — reproduced on an unmodified `dev` binary under
`--real-jdk`. Not a `--jdk-only` defect; found while validating one.

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
|---|---:|---:|---:|---:|
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

## What it is not

* **Not host load.** See the HotSpot control.
* **Not the probe.** Same class file in every arm.
* **Not `--jdk-only`.** The `dev` binary under `--real-jdk` hangs at the same
  rate, and that binary predates every change on this branch.
* **Not a timeout that is merely too short.** A short timeout produces
  `SocketTimeoutException` and a `SECTION-FAILED` line, which is exactly what
  the `dev` strict runs show. A hang produces neither.

## Where to start

The section stops somewhere between the `nio` line and its own `println`, so
the candidates are: `ServerSocket.accept()` under `setSoTimeout` on the spawned
thread, `Socket.connect(addr, timeout)`, `InputStream.read` under
`setSoTimeout`, `Thread.join(2000)`, or the try-with-resources `close()` of
either socket. All five are supposed to be bounded, so whichever it is, a
timeout is being dropped rather than exceeded.

The cheapest next step is a jstack-equivalent at the moment of the hang —
`CRATONVM_DBG_THREADSTART=1` names the spawned thread, and the VM's own thread
dump will say which of the five it is parked in. Do that before reading any
code: five candidates is few enough that guessing is slower than measuring, and
this reproduces in under a minute.

## Why it matters beyond this probe

A dropped socket timeout is invisible in exactly the way that matters: the
caller asked for a bound precisely because it intended to survive the peer
misbehaving, and instead the thread is gone for good. Any suite that opens a
socket with a timeout — Tomcat, the H2 server tests, anything with a health
check — can lose a worker this way and report it as a slow run.
