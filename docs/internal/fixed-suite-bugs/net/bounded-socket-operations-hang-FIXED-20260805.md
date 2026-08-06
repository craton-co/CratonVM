# Bounded socket operations hang about one run in five — FIXED

| | |
|---|---|
| **Status** | ✅ **FIXED** — retired from `docs/known-issues/` on 2026-08-05 |
| **Cause** | a lock cycle between the socket registry `RwLock` and the per-listener `Mutex`. `parking_lot`'s fair `RwLock` closes it as soon as any writer queues, and all three threads park at 0% CPU |
| **Fixed by** | `84fa69a55` (`net_poll` held the registry read guard across the listener poll) + the `net_poll_raw` EINTR arm from 2026-08-02 + **`net_configure_blocking`, the second half of the cycle, closed here** |
| **Severity** | high — a hard hang of the whole VM, from a program that only asked for a bounded accept next to a socket open |
| **HotSpot** | clean (12/12 at the same load) |
| **Filed** | 2026-08-04, OPEN, "found while validating a `--jdk-only` change" |

## What it was

`net_poll` looked the fd up under `net_sockets().read()` and, for a listener,
`return`ed `net_poll_listener(..)` **from inside that block**. A `return`
evaluates its expression before the block's locals drop, so the whole listener
poll — up to the caller's full `SO_TIMEOUT` — ran with the registry read guard
still held, and the poll loop's own `net_listener_still_registered` takes that
same read lock again. A `socket0` on another Java thread (`register_handle`, a
write) arriving in between wedges all three: the writer waits for the outer
reader, the outer reader waits on its own nested read, nothing releases.

That is why every bound in the probe was honoured and the run still never
finished. The record's "a timeout is being dropped rather than exceeded" was the
right question with the wrong answer — no timeout was dropped. The thread that
should have timed out never got to run.

This was **found twice, independently, on 2026-08-05**, with the same mechanism
and the same fix — once from `gdb` on a live hang, once from the VM's own
`--stack-dump-on-timeout` watchdog. The second diagnosis carries the A/B:
**44/120 hangs before, 0/120 after**, and it establishes that the guard release
alone is not sufficient (10/60 with only that); what reaches zero is the guard
release **plus** the `net_poll_raw` EINTR arm that had landed three days earlier
for an unrelated symptom.

## The half that was still open: `IOUtil.configureBlocking`

Found here, by asking what else in `native-io` has the shape rather than
whether the reported symptom was gone.

`net_configure_blocking`'s listener arm ran `listener.lock().set_nonblocking(..)`
with the registry read guard alive, i.e.

```text
    registry-read  ->  listener-mutex
```

while `net_accept` takes the **opposite** order — it holds
`listener_handle.lock()` across `net_listener_still_registered`, for the whole
of a blocking accept. Two orders is a cycle, and it closes exactly as the first
one did: once a writer queues, the accept thread's nested read waits, so it
never releases the listener mutex, so `configureBlocking` never gets that mutex,
so the read guard it is *still holding* never drops. Reachable from any
`ServerSocketChannel.configureBlocking(false)` next to an accept — which is what
an NIO server does on its way up.

The fix is this file's own convention, stated in `AUDIT 2026-05-17` comments at
a dozen other sites: clone the `Arc` under a brief read-lock, drop the map lock,
then do the work. Every other registry site in `net.rs` already did it; these
two were the exceptions. The handler also moves out of its inline closure into a
named `net_configure_blocking`, because a test cannot call a closure.

## Verification

All on `dev` at `2ed2d65d4`, real JDK 25, Linux, load average 14–29 on 16 cores
— the same host and load band the record was filed at. 12 waves of 10
concurrent probes; a run counts as **hung** only when the 45 s kill fires, and
as **complete** only when the probe prints its own `CENSUSLOAD sections=`
terminator, because a deadlocked run and a short clean run are indistinguishable
to the exit status.

| arm | runs | complete | hung |
|---|---:|---:|---:|
| **positive control** — `dev` with `84fa69a55` reverted | 60 | 55 | **5** |
| `dev` | 120 | 120 | 0 |
| `dev` + the `configureBlocking` fix | 120 | 120 | 0 |

**The positive control is the point.** A harness that has never produced a
single `HUNG` verdict cannot support "0 in 120" — that is an inert arm, not a
result. With the original guard reverted it hangs 5 times in 60, and every one
of the five stops in the same place the record describes: everything printed up
to and including the `nio` line, nothing after it. Then the same harness scores
zero on both fixed arms.

Sequential runs do **not** reproduce it (8/8 clean on the broken code was the
first thing this session measured, before switching to waves): the window needs
another Java thread in `socket0` while a listener poll is in flight, and one
probe at a time on a loaded host rarely lines those up.

Unit tests: `cargo test -p cratonvm-native-io` — 388 pass, 0 fail, including the
new `configure_blocking_does_not_hold_the_registry_lock_waiting_for_a_listener`.
It is bounded throughout, so a regression fails in a second instead of hanging
the suite the way the defect hangs the VM, and it was verified by injection —
re-adding a live read guard fails it on the exact assertion.

## What the record got right, and what to keep from it

Right, and worth keeping: **the HotSpot control is what made this a defect
rather than an observation about a busy machine.** The load was real (19–34 on
16 cores) and writing the hang off as contention was the obvious move; 12 clean
HotSpot runs at the same load on the same host is what closed that off. The
record says so explicitly, and it was correct to.

Also right: "not a timeout that is merely too short — a short timeout produces
`SocketTimeoutException` and a `SECTION-FAILED` line, and a hang produces
neither." That distinction is what kept the search on deadlock rather than on
timeout arithmetic.

The one thing to carry forward: its "where to start" list named five candidate
*Java-level* calls, all of which were innocent. The defect was not in any of
them but in the lock discipline underneath all of them, and no amount of
narrowing between the five would have reached it. The native backtrace of a live
hang did — and, per the fix's own note, only WITHOUT `--stack-sample-ms`, which
perturbs interpreter timing enough to close the window.

## Residual

None known. `net.rs`'s registry sites were swept for the same shape and the two
described above were the only ones holding a guard across a call that can block
or re-enter; every other site already clones the `Arc` and drops the lock first.
