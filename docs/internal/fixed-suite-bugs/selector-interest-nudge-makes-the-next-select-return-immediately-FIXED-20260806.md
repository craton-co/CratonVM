# `Selector.select(timeout)` returned instantly after every `interestOps()` — the interest nudge had no in-flight gate on Linux

**Status: FIXED — 2026-08-06.** `fix/selector-eintr-premature-return-20260806`.
Linux only. Found while closing out
[`reactor-netty-outbound-request-line-corruption`](springboot/reactor-netty-outbound-request-line-corruption-RESOLVED-20260806.md),
whose log carried 268 of Netty's `Selector.select() returned prematurely 512
times in a row; rebuilding Selector` and left them unexplained.

## The defect

`selector_set_interest` writes a byte into the selector's wakeup pipe after
`epoll_ctl(MOD)`, because a `MOD` does not reliably interrupt an
already-blocked `epoll_wait` — Tomcat arms `OP_WRITE` after a partial gathering
write, and without the nudge the last HTTP/2 frame sat queued until shutdown
and the peer saw a truncated GOAWAY
([`dohead-post-fix-sporadic-residuals-FIXED`](tomcat/dohead-post-fix-sporadic-residuals-FIXED.md),
2026-07-18).

**The byte does not expire.** Written while nobody is parked, it sits in the
pipe until the *next* `select()`, whose `epoll_wait` finds the wakeup fd
readable and returns at once having selected nothing.

The non-Linux branch immediately below it has always been gated, and says why:

```rust
// See selector_register: only an active WSAPoll needs interrupting.
// Sending a nudge before select starts would make that next select
// return spuriously with zero ready keys.
if st.in_flight_selects != 0 { st.nudge_blocked_poll(); }
```

The Linux branch had no such gate. That is the whole bug, and it explains why
this was never seen on Windows.

## Why Netty in particular

A Netty event loop sets interest ops **between** selects, which is exactly the
ungated case, so every iteration queued a byte and the following
`select(1000)` returned in ~0 ms with nothing ready. `NioIoHandler` counts
precisely that condition — no keys AND the timeout had not elapsed — and after
512 in a row logs the warning and rebuilds the selector. The rebuild
re-registers every channel, which sets more interest ops, which queues more
nudges: the storm feeds itself, which is why the 2026-08-05 log shows one
rebuild roughly every 10 ms for the whole run rather than a single burst.

## Measurement

`probes/SelectorInterestNudgeProbe.java` registers a connected socket that can
never become readable, so a correct `select(timeout)` blocks for the full
timeout whether or not `interestOps()` is called first. Azure Linux, load ~2:

| arm | premature | min elapsed |
|---|---:|---:|
| HotSpot 25, `interestOps` before each select | 0 / 8 | 1001 ms |
| CratonVM pre-fix, **no** `interestOps` (control) | 0 / 8 | 1000 ms |
| CratonVM pre-fix, `interestOps` before each select | **8 / 8** | **0 ms** |
| CratonVM fixed, `interestOps` before each select | 0 / 8 | 1000 ms |

The no-`interestOps` control is what says the probe measures the nudge and not
the selector generally.

Unit tests, and they are a pair — neither is meaningful alone:

* `set_interest_before_a_select_does_not_shorten_it` — the defect. Verified RED
  on Linux with the gate reverted: *"returned after 10.836µs of a 200ms
  timeout"*.
* `set_interest_wakes_a_select_that_is_already_parked` — the Tomcat OP_WRITE
  property the nudge exists for, which this gate could have deleted instead of
  the storm. Green both with and without the gate, as it must be.

Both assert on the WALL CLOCK, because a correct select and the defective one
both answer "0 ready keys" and only the elapsed time separates them — which is
also exactly what Netty measures.

## What this does NOT claim

**The field storm was not reproduced, before or after.** None of ~40 runs of
`ReactorClientHttpRequestFactoryBuilderTests` on either host — pre-fix or
fixed, serial or 8-way concurrent — logged a single "returned prematurely"
line. Netty only counts CONSECUTIVE premature selects, and any select that does
useful work resets the counter; whatever kept the 2026-08-05 run in a pure
`interestOps`→`select` loop 512 times running is not reproduced by running the
class today. So this fix removes the MECHANISM that produces that log line; it
is not an observation of the line disappearing from the suite.

**No measurable wall-clock change**, either. Interleaved A-B-B-A-A-B on one
host with two binaries differing only in this gate: ungated 6/5/6 s, gated
5/6/5 s. The earlier impression of "11 s → 5 s" was host load, not the fix.

## Adjacent gap found and deliberately NOT fixed here

`kernel_select_linux` and `kernel_select_poll` both do this on a signal:

```rust
if err.kind() == ErrorKind::Interrupted || err.raw_os_error() == Some(libc::EINTR) {
    finish_in_flight_linux_select(id);
    return Ok(0);      // caller's remaining timeout is discarded
}
```

The JDK does not: `EPollSelectorImpl.doSelect` re-enters the syscall with the
time that is LEFT and only reports 0 once the deadline has genuinely passed. So
`select(1000)` interrupted at 1 ms returns 0 to Java after 1 ms — the same
premature-return shape as the bug above, from a different cause. It is left
alone because it is **unproven**: a SIGCONT storm across every thread of the
process produced 0 premature selects on both HotSpot and CratonVM (a signal
only interrupts a syscall when it is caught by a handler, and nothing in either
runtime handles SIGCONT), so there is no failing test to fix against and no
evidence it fires in practice. Changing a blocking syscall's retry loop without
one is how a fast failure becomes a hang. Filed here so the next reader of this
code does not have to re-derive it.

## Affected

Nothing is listed as fixed by name: no suite class is known to have failed on
this, and the class that carried the log line passes with and without the gate
today. The value is the mechanism and the two tests.
