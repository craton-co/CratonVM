# Intermittent reactor worker-thread leak at client shutdown (RUNNABLE, empty stack)

**Status:** OPEN (intermittent, ~1 in 6–8 runs). Lower-priority follow-up to the
`Thread.getState()` fix (commit `16d23e7b`).

## Context

Surfaced while fixing the ES `RestClientSingleHostIntegTests` `ThreadLeakError`. The headline
symptom — a worker reported `state=NEW` — was a **`Thread.getState()` reporting bug** (the VM
never advanced `holder.threadStatus`, so real-JDK `getState()` returned `NEW` for *every* thread,
including finished ones). That is **FIXED** (`16d23e7b`): `getState()` now derives the state from
the VM thread registry and returns the canonical `Thread$State` (finished → `TERMINATED`).

With that fixed, most suite runs are clean, but an **intermittent genuine leak** remains:

```
1 thread leaked from SUITE scope:
   Thread[id=400, name=elasticsearch-rest-client-12-thread-3, state=RUNNABLE, ...]
        at (empty stack)
... There are still zombie threads that couldn't be terminated.
```

## What we know

- The leaked thread is an Apache httpcore-nio I/O reactor worker
  (`elasticsearch-rest-client-N-thread-M`).
- It is `RUNNABLE`, has an **empty Java stack**, and **cannot be interrupted** — the signature of a
  worker parked in a **blocking native** (almost certainly `sun.nio.ch.Selector.select()` /
  `EPoll`/`WSAPoll` wait) that was not woken when the reactor shut down.
- It is now correctly *labelled* RUNNABLE (pre-fix it was mislabelled `NEW`), so this leak existed
  before the getState fix — the fix just stopped masking it among the spurious-NEW reports.
- `CRATONVM_DBG_THREADSTART=1` did not correlate by name (the reactor names its worker after
  construction, so the start-time log shows a default name).

## Likely root cause (to confirm)

`AbstractMultiworkerIOReactor.shutdown()` signals each worker and calls `selector.wakeup()` to
unblock the `select()` call so the worker loop observes the shutdown flag and exits. If CratonVM's
selector `wakeup()` races with a worker that is *about to* re-enter `select()` (or the worker is
created and enters `select()` after the reactor already processed its shutdown), the wakeup is lost
and the worker blocks in `select()` forever → leaked, uninterruptible, empty-stack RUNNABLE thread.

This is in the same selector/reactor machinery touched by the ES-HANG-02 connect fix, but is a
**shutdown-path wakeup race**, not the connect path.

## Next steps

1. Reproduce with a thread dump of the stuck worker (e.g. `--stack-dump-on-timeout`, or a SIGQUIT-
   style dump) to confirm it is blocked in `Selector.select()` / the kernel wait.
2. Audit the selector `wakeup()` ↔ `select()` ordering in `native-io/src/nio_selector.rs`: ensure a
   `wakeup()` that arrives between two `select()` calls is not lost (the wakeup signal must be
   sticky — a `select()` entered after a pending `wakeup()` must return immediately). Check the
   Windows UDP-loopback wakeup pair and the Linux self-pipe/eventfd drain logic for a
   lost-wakeup window.
3. Verify the reactor's shutdown actually reaches `wakeup()` for every worker (vs. only the first).
4. Acceptance: `RestClientSingleHostIntegTests` ≥10 consecutive clean runs (no `ThreadLeakError`),
   no regression to `RestClientMultipleHostsIntegTests` (stays 4/4).

## Related

- `Thread.getState()` fix: commit `16d23e7b`.
- ES-HANG-02 residuals + selector/connect work: [ES-HANG-02-residuals-handoff.md](ES-HANG-02-residuals-handoff.md).
