# W7-53 — the blocking-close-awareness family: 19 sites fixed, 7 named open, and a census that was short in both directions

**Branch:** `fix/blocking-close-awareness-family-20260812`, based on dev
`054348ec0`.

**Nothing in the Rust half of this record was built or run.** This session had
no build. Every "before" is a reading of the tree; every "after" is a claim
about source. The one thing that WAS executed is the probe, against HotSpot
25.0.3 on this Windows 11 host — see "The instrument" below, and read that
section before believing any row here.

Predecessors: `W2-2-blocked-reader-async-close-wakeup.md` (the three readers,
fixed 2026-08-07 and 2026-08-11), and `W7-47-w2-cluster.md`, whose census of
this family is the input to this lane.

---

## The contract

A thread parked in a blocking read/write/accept on a socket, channel or pipe
must come back when another thread closes it. JDK 25's `java.net.Socket.close()`
is unconditional — "Any thread currently blocked in an I/O operation upon this
socket will throw a `SocketException`", *will*, not *may* — and the
`java.nio.channels` types say the same with `AsynchronousCloseException`. Where
it does not hold, `close()` from a second thread does nothing, the blocked
thread stays blocked forever, and shutdown hangs.

**Classification, once, for the whole lane.** No `ClassOrigin` is involved and
`--jdk-only` changes nothing about any of it: every registration touched is
`NativeKind::Bridge`, so `drop_synthetic_stubs` does not reach it. These are
ordinary **Compatible-mode defects that strict inherits unchanged**. Two of the
files are mode-gated for a different reason, stated per row below
(`net_phase_e.rs` is synthetic-socket mode only; `servlet.rs`'s `s2` surface is
the synthetic NIO one).

---

## Census: mine against W7-47's

W7-47 measured "~20 blocking sites, 8 files", named three as already fixed, fixed
`DatagramChannel.receive`'s lock half itself, and handed over "the remaining
~19". Re-deriving it from the tree rather than from the list gives **19 sites
fixed here** — the same number — but **not the same nineteen**. Eight sites are
new, and two of the census's rows do not survive contact with the code.

### Fixed here (19)

| # | site | file | how it could not see the close |
|---|---|---|---|
| 1 | `FileDescriptorTable::udp_recv` | `native-api/src/fd_table.rs` | chokepoint; 4 callers |
| 2 | `FileDescriptorTable::tcp_read` | `native-api/src/fd_table.rs` | chokepoint; 4 callers. **Not on the census** |
| 3 | `FileDescriptorTable::tcp_write` | `native-api/src/fd_table.rs` | **Not on the census** |
| 4 | `FileDescriptorTable::tcp_accept` | `native-api/src/fd_table.rs` | **Not on the census** |
| 5 | `net_write0` | `native-io/src/net.rs` | the missing write twin |
| 6 | `re1_socket_write_stream` | `native-builtins/src/net_phase_e.rs` | the missing write twin |
| 7 | `s2_blocking_accept` | `native-builtins/src/servlet.rs` | parks on a `try_clone`d duplicate |
| 8 | `SocketChannel.read` closure | `native-builtins/src/servlet.rs` | streams left blocking by row 7 |
| 9 | `SocketChannel.write` closure | `native-builtins/src/servlet.rs` | as above |
| 10 | `DatagramChannel.read` | `native-builtins/src/phases_late/net_channels.rs` | **Not on the census** — and it carried the census's own worst defect too |
| 11 | `Job::Accept` | `native-io/src/async_socket.rs` | worker consumed permanently |
| 12 | `Job::Write` | `native-io/src/async_socket.rs` | |
| 13 | `Job::ReadFd` | `native-io/src/async_socket.rs` | private `try_clone`d handle |
| 14 | `Job::ReadFutureFd` | `native-io/src/async_socket.rs` | as above |
| 15 | `Job::WriteFutureFd` | `native-io/src/async_socket.rs` | as above |
| 16 | `source_read_buffer` | `native-io/src/pipe.rs` | handle copied by value |
| 17 | `source_read_bytes` | `native-io/src/pipe.rs` | as above |
| 18 | `sink_write_buffer` | `native-io/src/pipe.rs` | as above; Windows arm partial, see below |
| 19 | `sink_write_bytes` | `native-io/src/pipe.rs` | as above; Windows arm partial, see below |

### Named open (7) — see "What is left" for each

`DatagramChannel.receive`; four TLS stream sites; the multi-acceptor race in
`s2_blocking_accept`; the Windows arm of the pipe sink write.

### Two census rows that do not survive the code

**"Datagram send, a chokepoint with three callers."** The census text says
*receive*, and receive is right; the handover paraphrased it as *send*. A UDP
`send` does not wait on a peer — it is bounded by the socket's own send buffer,
returns, and has no party that could fail to arrive. `udp_send` and
`udp_send_connected` are **not** defects and were left alone. Fixing them would
have been a change to a path with no failure mode, which is how a census grows
rows that later have to be un-fixed.

**"Pipe: the handle is copied by value, so removing the map entry does not close
the handle the parked thread holds."** The entry is not removed and the handle
*is* closed. `close_pipe_end` set `closed = true` and called `close_raw(end.raw)`
immediately, on the same raw handle a parked `ReadFile`/`read(2)` was using. So
the defect is **worse** than the row says, not milder: not merely an unwakeable
park but a use-after-close the instant the OS recycles the handle number onto an
unrelated file. That correction is what decides the mechanism — see below.

### What the census did not look at, and why the misses cluster

The three `fd_table` misses and the `DatagramChannel.read` miss have one shape
in common: **the census read the files it had already named and did not re-ask
which files reach a blocking syscall.** `fd_table.rs` was named once, for
`udp_recv`, and its three TCP siblings sit within eighty lines of it.
`DatagramChannel.read` sits one screen below `DatagramChannel.receive` in the
file the census edited, and carries the same process-wide-lock wedge that the
census called "the worst" — `recv` ran with the `s2_registry` guard alive. The
four TLS sites are in two more files that were never opened.

Not defects, checked and cleared here as well: `Selector.select` (a UDP loopback
wakeup socket is in the wait set and `selector_close` nudges it);
`xnio_conduits`' transports (set non-blocking at every accept/connect site,
though nothing in that file asserts it, so it stays latent);
`socket_channel.rs`'s read/write/accept and `net.rs`'s accept (already
close-aware); `async_socket`'s `try_read_ready_bytes` (gated on an availability
query, so it cannot park); `fd_table`'s `udp_send*`.

---

## The mechanism — one loop, reused, not a second one

All 19 use the shape the three landed readers use:

> park in `poll`/`WSAPoll` on a bounded slice; re-ask the registry **after** the
> poll; return `ErrorKind::Interrupted` once the slot is gone.

Asked *after* the poll on purpose: a close landing while parked is then seen on
the very next pass, and a close that raced a readiness edge still wins — HotSpot
fails an I/O a concurrent `close()` beat, it does not hand back bytes on a socket
Java has already closed.

`ErrorKind::Interrupted` is the carrier everywhere, and it is unambiguous at
every site for the same reason: each path reissues every real EINTR itself, and
every poll primitive here reports EINTR as *not ready* rather than as an error.

**No new binding of `poll(2)`/`WSAPoll` was added.** The census counted seven in
the tree and warned that a consolidation planned against four would leave three
behind. Rather than add an eighth:

* `fd_table.rs`'s existing zero-timeout `poll_readiness` grew a `timeout_ms`
  parameter; the old entry point is now a one-line wrapper, so `poll_ready` is
  unchanged.
* `net.rs` gained `poll_stream_writable`, routed through the same
  `net_poll_stream` as `poll_stream_readable` with `NET_POLLOUT`.
* `net_phase_e.rs`'s single binding grew a direction parameter;
  `re1_socket_poll_readable` keeps its name and contract, so `available()` is
  untouched.
* `servlet.rs` used its own existing `selector_poll`/`PollReq` abstraction.
* `async_socket.rs` and `net_channels.rs` call into the above.

### `SO_RCVTIMEO` vs `SA_RESTART`, stated per regime

The trap this area is documented for, answered explicitly:

* **What the wakeup depends on.** Not on signals and not on `SO_RCVTIMEO`. It
  depends on `poll`/`WSAPoll` returning at its own timeout, and on the registry
  answer changing. Neither regime is load-bearing for the wakeup itself.
* **Where `SA_RESTART` matters.** On Unix, `poll(2)` is *never* auto-restarted
  by `SA_RESTART`, so a signal delivered to a thread parked in it always returns
  EINTR — and this VM sends one on purpose (`jit::xt_root_scan` SIGUSR2s every
  thread for a cross-thread root scan). Every Unix arm added here therefore
  reports EINTR as **not ready** and never re-polls in place: re-polling with the
  same `timeout_ms` would restart the whole wait on every GC and silently defeat
  any deadline above it. Winsock has no EINTR and therefore no `SA_RESTART`
  hazard, so the Windows arms have no such branch.
* **Where `SO_RCVTIMEO` matters, and why it had to be re-derived.** A syscall we
  no longer issue until the socket is ready is a syscall `SO_RCVTIMEO` can never
  bound. Left alone, this fix would have removed one hang and introduced another
  on every timed read. So every site that can carry a timeout reads it back off
  the socket (`read_timeout()`/`write_timeout()`) and enforces it as the loop's
  own deadline: `fd_table`'s four, and `async_socket`'s `aio_read_close_aware`
  (the timed `AsynchronousSocketChannel.read` overload sets a real read timeout
  on the worker's private clone). `SO_RCVTIMEO` stays set and remains the first
  line; the deadline is what still ends the park where it cannot fire.

### On expiry — what every bounded wait added here does

Two different bounds, and only one of them is an outcome:

* The **per-pass slice** (25 ms; 5 ms in `pipe.rs`, for a reason given below)
  expiring is **not** an outcome. It is the point at which the registry is
  re-asked and the loop continues. Nothing is reported, nothing is abandoned.
* The **caller's deadline**, where one exists, **is** an outcome: it ends the
  wait with `TimedOut`, which each surface maps to the `SocketTimeoutException`
  it already specified. It does not merely decline to poll again.

No expiry anywhere leaves an operation running.

### Non-blocking mode is untouched everywhere

Every site gates the close-aware path on the recorded blocking mode and leaves
the non-blocking path byte-for-byte alone. That is not caution for its own sake:
parking a non-blocking socket would take the JDK's `IOStatus.UNAVAILABLE` (-2)
protocol away from its caller, which is every selector-driven reactor in
Netty/Tomcat/Jetty. `fd_table` had no record of the mode, so it gained one — a
side table written by the two `set_nonblocking` entry points and dropped on
`close`, modelled exactly on `net.rs::net_pending_nonblocking`, and answering
"blocking" for an unknown fd for the same deliberate reason (guessing "cannot
park" for a socket that can is the unsafe direction).

---

## Three things fixed that were not on the census's list

**A half-duplex wedge in `fd_table::tcp_read`.** It held the per-fd `Mutex`
across the whole blocking read, so every `tcp_write` on the same fd queued behind
a reader waiting on a peer that might never speak. `try_clone_tcp` exists only
to work around it. The poll now runs outside that mutex, which is taken only
once the socket is already readable.

**A process-wide wedge in `DatagramChannel.read`.** `recv` ran with the guard on
the process-wide `s2_registry` mutex still alive, because the call sat inside the
block that owns it — the same self-sustaining wedge W7-47 called the worst thing
it found in `receive`, one screen up in the same file, and did not check for
below it. On a blocking datagram channel that parks every synthetic socket
operation in the VM until a packet arrives, *including the close that would end
the wait*.

**A platform claim in `async_socket.rs` that is false on this host.**
`aio_shutdown_stream` and `aio_shutdown_fd_table_stream` issue `shutdown(Both)`
before removing the entry, and their doc comments state without qualification
that this "unblocks *every* fd that still references the same
open-file-description, including clones already handed to worker threads". True
on Linux, where `SHUT_RD` wakes a parked `recv` with EOF. **False on Windows**,
where no `shutdown` aborts a pending blocking call — only `closesocket` does, and
that code cannot close a handle a worker is mid-syscall on. The claim is
load-bearing: it is why nothing else was ever added there. The shutdown stays (it
is the cheaper wakeup where it works) and a cancellation flag was added beside
it.

---

## `pipe.rs` — why the registry re-ask cannot be lifted, and what replaces it

The census is right that the mechanism does not transfer. It is right for a
reason one step further along than the one it gives.

Every socket site in this family parks holding an `Arc<TcpStream>` cloned out of
a registry. **Two** things follow, and both are load-bearing:

1. the OS handle cannot be closed while the thread is parked, because the `Arc`
   is still alive — so `close` can only *mark* the registry; and
2. marking the registry is therefore a safe, purely advisory question the parked
   thread may re-ask on any cadence it likes.

Neither holds for a bare `u64` with no ownership attached. `PipeEnd` is `Copy`,
`pipe_end_get` handed out a snapshot, and `close_pipe_end` freed the handle in
that snapshot immediately. A registry re-ask bolted onto that would have been a
question whose answer arrives after the damage.

So the fix is **two halves**, and neither is sufficient alone:

* **`in_flight` / `close_pending`** — an ownership discipline that gives the
  handle the lifetime the socket sites get from their `Arc`. A thread announces
  itself with `pipe_enter` and retires with `pipe_leave`; `close` sets `closed`
  at once but **defers `close_raw` to the last thread out**. Without this half
  the generation answer arrives too late to matter, and the use-after-close
  remains.
* **the `closed` generation, re-read through `pipe_still_open`** between bounded
  readiness probes. Without this half, `in_flight` alone would only make an
  unbounded park *safe* instead of *ending* it.

The readiness probe is what keeps the thread out of the syscall long enough to
ask at all. Unix: `poll(2)` on the fd, both directions. Windows read side:
`PeekNamedPipe`, which is documented to work on anonymous pipes and needs only
the `GENERIC_READ` access `CreatePipe`'s read handle already has — a pure query
that consumes nothing and changes no handle state, so unlike a
`SetNamedPipeHandleState(PIPE_NOWAIT)` dance it cannot race a concurrent reader
into a spurious short read. Because it does not itself wait, the bound is a
`Sleep` between probes, which is why `PIPE_CLOSE_POLL_MS` is **5 and not 25**:
there it is a real latency cost rather than only a liveness bound. On Unix it is
a genuine `poll` timeout and costs nothing.

A close now raises a real `AsynchronousCloseException` rather than a bare
`IOException` whose message mentions the name. The concrete type is what
`java.nio.channels` callers catch, and a message-prefix `IOException` lands in
the wrong arm — the same mistake `sc_read` used to make.

### The one row left open rather than falsely closed

**The Windows sink write.** Windows offers no space-available query for the
write end of a pipe. The two mechanisms that would give one both *change how the
handle behaves* rather than observing it: `SetNamedPipeHandleState(PIPE_NOWAIT)`
(documented as legacy LANMAN compatibility, and it alters every write on the
handle), or creating the pipe with `CreateNamedPipe(FILE_FLAG_OVERLAPPED)` +
`CreateFile` instead of `CreatePipe` and using a bounded `GetOverlappedResultEx`.
The second is the correct fix and is the named follow-up; neither is worth
landing on inspection, without a build to run it against.

So `poll_pipe` answers `None` for a write on Windows, which routes to one plain
blocking `WriteFile` — the pre-existing behaviour — with the generation check
still applied *before* it and the deferred close still applied *after*. A
Windows sink write therefore no longer risks a use-after-close, and does observe
a close that has already happened; a close arriving while it is inside
`WriteFile` is what it still cannot see.

Answering `Some(Ok(false))` there instead would have compiled, looked exactly
like the other three, and spun a loop that could never report readiness while
every caller believed it was close-aware. That is precisely the shape that
removes a site from a census while leaving the defect, and it is why this row is
written down rather than quietly counted.

---

## The instrument

`probes/AsyncCloseProbe.java`, with its HotSpot oracle transcript at
`probes/AsyncCloseProbe.expected.txt`.

W2-2 records a 2026-08-11 measurement taken with an instrument it calls
`AsyncCloseProbe`. **That file was never in the repository** — not in `probes/`,
not in `regression-suite/src/`. The measurement is real; the instrument is not
reproducible from the tree, which for a suite is the same as not having measured
it. W7-47 recorded the gap and declined to fill it, on the grounds that writing a
probe nobody could run would produce an instrument nobody has seen agree with a
control. That objection is answered by running the control, which this session
could do.

13 rows, one per Java-reachable site in the family: `socketRead`, `socketWrite`,
`serverSocketAccept`, `channelRead`, `channelWrite`, `channelAccept`,
`datagramReceive`, `datagramChannelReceive`, `pipeRead`, `pipeWrite`,
`asyncAccept`, `asyncRead`, plus `selfTestNoPark`.

### How it avoids the two shapes that make such a probe worthless

**It does not close before the read blocks.** A close issued ahead of the block
tests nothing: the read then returns immediately for an unrelated reason. Every
row proves the park first — the worker publishes `entered` immediately before the
blocking call, the driver waits for that flag, sleeps 300 ms (12 slices at the
25 ms close-poll cadence), and asserts the worker has **not** returned. A row
that returned inside that window is reported `INCONCLUSIVE`, never `PASS`, and
fails the run.

**It does not hang on failure.** A probe that hangs converts a red into a stuck
job. Every worker is a daemon thread; every wait is bounded
(`join(probe.wakeMs)`, 4 s); an expired row reports `outcome=TIMEOUT` and FAILS;
nothing is retried; each row closes its sockets in a `finally` whether it passed
or not, so an expired row cannot contaminate a later one; and `System.exit`
guarantees the JVM leaves with workers still parked in the kernel.

### Both guards are calibrated, not asserted

* **`selfTestNoPark`** is a negative control — a read whose peer has already sent
  a byte, so it cannot block. It PASSES by *not* parking. Without it,
  `parked=true` on the other twelve rows would rest on a check nobody has seen
  say `false`, which is the standing the missing probe left W2-2's measurement
  in. If that row ever reports `parked=true`, no other row in the file means
  anything however green.
* **`-Dprobe.skipClose=<row>`** suppresses the close so the failure path can be
  executed on any VM. Measured on HotSpot 25.0.3 / Windows 11, 2026-08-12:
  `socketRead` FAILs at `ms=4000`, the run exits 1, wall clock 5.1 s. A harness
  whose failure path has never been executed is not known to have one.

### Measured 2026-08-12 — HotSpot 25.0.3, Windows 11

**13 pass, 0 fail, 0 inconclusive, exit 0.** Full transcript in the
`.expected.txt`. One `want` was corrected against the measurement rather than the
row reshaped to agree with a guess: `channelWrite` answers
`ClosedChannelException`, because the parked write returns its partial count and
the *next* iteration of the row's write loop finds the channel already closed.
All of `AsynchronousCloseException` / `ClosedChannelException` / a partial count
are wakeups; only `TIMEOUT` is the defect.

**There is no CratonVM arm and none should be inferred.** No binary was built.

---

## How to run it

```
javac -d <out> probes/AsyncCloseProbe.java

java -cp <out> AsyncCloseProbe                              # HotSpot 25 — CONTROL, run first
cratonvm.exe -cp <out> AsyncCloseProbe                      # Compatible (--real-jdk)
cratonvm.exe --jdk-only -cp <out> AsyncCloseProbe           # strict
CRATONVM_REAL=-net-sockets cratonvm.exe -cp <out> AsyncCloseProbe   # synthetic java.net.Socket

# calibration, on whichever VM is under test:
java -Dprobe.skipClose=socketRead -cp <out> AsyncCloseProbe selfTestNoPark socketRead
#   expected: socketRead FAIL at ms=4000, exit 1, and the process leaves.
```

Run the HotSpot arm every time, not once. It is the control that says the
instrument works on that host; a CratonVM row is evidence only after the same
row has been seen to pass there.

Rust-side tests, none of which have been compiled:

```
cargo test -p cratonvm-native-io --lib pipe::tests
```

`a_close_wakes_a_reader_parked_on_a_pipe`,
`a_close_aware_pipe_read_still_delivers_bytes` and
`a_close_during_an_in_flight_op_defers_the_raw_close` are RED by construction
against the tree as it stood at `054348ec0`. The first proves the park before the
close in the same way the Java probe does: the reader publishes a started flag,
the closer waits for it plus a further 100 ms (20 poll slices) and asserts
`!is_finished()` before closing anything.

`cargo test -p cratonvm-native-io --lib net::tests` and
`--lib socket_channel::tests` must stay green — they cover the three readers this
lane did not touch. `vm/tests/socket_input_stream_timeout.rs` runs the
`SO_TIMEOUT` fixture across `real_net_sockets` on and off, which is the pair of
arms the `net_phase_e.rs` change straddles; run it before believing the
synthetic-socket row, because a deadline regression there is exactly the failure
this change had to design around.

---

## What would falsify this

The single observation is the probe's `outcome` column, per row, per arm.

* `outcome=TIMEOUT` on a CratonVM arm for a row in the fixed table — the fix is
  not reached. Check the site is the one servicing the call
  (`CRATONVM_DBG_SOCK=1`, `CRATONVM_DBG_SC_READ=1`, `CRATONVM_DBG_NET=1`); if the
  native never fires, some other registration is servicing it and the change is
  aimed at the wrong layer.
* `outcome=returned:-1` — the reader woke on an EOF rather than on the registry
  re-ask. That is the pre-fix Linux answer and means the ordering argument
  failed: the syscall ran before the registry check rather than after it.
* `parked=false` on any row but `selfTestNoPark` — the row tested nothing and its
  neighbours' verdicts are not evidence either until it is fixed.
* `selfTestNoPark parked=true` — the anti-vacuity guard is broken and the whole
  file's output is void.
* `pipeWrite outcome=TIMEOUT` on Windows CratonVM is the **expected** reading of
  the row this record leaves open, not a surprise.
* Any `SocketTimeoutException` on a socket with no timeout set — a deadline arm
  fired that should have been `None`. That is the `SO_RCVTIMEO` regression this
  change was designed against, and it would mean the deadline is being derived
  where no timeout exists.

---

## What is left

| what | where | why not here |
|---|---|---|
| `DatagramChannel.receive` close-awareness | `native-builtins/src/phases_late/net_channels.rs` | The lock half is the census branch's edit (`fix/w2-stream-stack-blocked-reader-moduledesc-20260812`), not yet on dev. Re-doing it here would only produce a conflict. Once it lands, add the same `s2_wait_ready_close_aware` call this branch added to `DatagramChannel.read` twenty lines below it — the helper is already `pub(crate)` |
| Windows pipe sink write | `native-io/src/pipe.rs` | Needs `CreateNamedPipe(FILE_FLAG_OVERLAPPED)` + a bounded `GetOverlappedResultEx`, i.e. a change to how the pipe is created. Not landable on inspection |
| `s2_tls_read_direct`, `s2_tls_write` | `native-builtins/src/servlet.rs` | TLS record layer. A close-aware loop must not abandon a read mid-record, so the wakeup has to be expressed against the *underlying* socket while the record assembler keeps its state. Genuinely a different problem, and not one to solve without a build |
| `rustls_stream_read`, `rustls_stream_write` | `native-builtins/src/t27_tls.rs` | as above. Both already have the correct lock discipline; it is only close-awareness they lack |
| multi-acceptor race in `s2_blocking_accept` | `native-builtins/src/servlet.rs` | With two threads accepting one listener, the loser of the race between the poll and the `accept()` parks again, not close-aware. Not closed by flipping the clone non-blocking: `try_clone` shares the blocking mode with the registry's listener on both platforms. Every accept parked before; at most one loser parks after |
| the seven-way poll-binding consolidation | tree-wide | W7-47's correction stands and this lane did not take it on. No eighth binding was added, and three of the seven grew parameters instead |

---

## Platform

**No row here is described as Windows-only or Unix-only.** The host is Windows
and no Linux arm was run, so a platform claim would be inference rather than
evidence — the same refusal W7-47 made and for the same reason.

What *is* platform-specific is stated structurally rather than empirically, and
only where the code forces it:

* `shutdown` semantics differ, and this is a documented OS contract rather than
  an observation: on Linux `shutdown(SHUT_RD)` wakes a parked `recv` with EOF
  (still the wrong answer where the spec mandates an exception, but not a hang);
  Winsock has no `shutdown` that aborts a pending blocking call. That difference
  is why several of these rows would read as "returns -1" on one platform and
  "never returns" on the other, and why the `async_socket.rs` comment quoted
  above is true where it was written and false where it now runs.
* `poll(2)` is never restarted by `SA_RESTART`; Winsock has no EINTR. Both are
  contracts, not measurements, and each Unix arm added here carries the EINTR
  branch that follows from the first.
* The `#[cfg(unix)]` arms added in `fd_table.rs`, `net_phase_e.rs` and `pipe.rs`
  are **not compilable on this host**, in principle as well as in practice. The
  `#[cfg(not(any(unix, windows)))]` arms are not compilable on any host in this
  campaign.

---

## Flags

None added. `probe.skipClose`, `probe.settleMs`, `probe.wakeMs` and
`probe.enterMs` are Java system properties read by the probe itself, not
`CRATONVM_*` VM flags, so no `flag_groups.rs` / `flag-surface.txt` /
`flag-tokens.md` / `flag-inventory.md` work is implied.

## Tests

No existing test was weakened. Three were added, in `native-io/src/pipe.rs`,
covering the mechanism this lane invented rather than reused.
