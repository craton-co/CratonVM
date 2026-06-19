# Intermittent reactor worker-thread leak at client shutdown (RUNNABLE, empty stack)

**Status:** OPEN (intermittent, ~1 in 16–60 runs; rate inflated by concurrent peer-session load on the
shared worktree). Follow-up to the `Thread.getState()` fix (commit `16d23e7b`).

## UPDATE: cross-thread stack walking now works; leak thread has NO Java frames

`Thread.dumpThreads()` / `getStackTrace()` were stubs returning empty arrays — the reason the leak report
showed `at (empty stack)`. **Cross-thread stack walking is now implemented** (commits in HEAD +
`26382a17`): each thread publishes a frame snapshot at its blocking deposit points
(`deposit_root_snapshot` → `stackwalker::capture_frames_no_lines` → registry `frame_trace`), and
`dumpThreads` materialises it. Verified vs HotSpot (`scratch/eshang/StackDumpTest.java`): a
`LinkedBlockingQueue.take()`-parked worker now reports its real 5-frame stack.

**But the leaked reactor thread STILL reports an empty stack** when caught (it reproduced at ~1/16–1/60
even with the stack walker). That is itself the key finding: the leaked thread has **no Java frames at
all**, so it is parked in a **native that does not deposit a frame snapshot** — i.e. a blocking *socket
I/O* native (read / accept), NOT `park`/`Object.wait`/a select-with-Java-frames (those deposit and now
show frames). The earlier infinite-`select()` cap (`CRATONVM_SELECT_MAX_BLOCK_MS`, commit `26382a17`) is a
sound defensive fix but did **NOT** stop the leak — confirming the stuck thread is not in `select()`.

### PIVOTAL FINDING — it is GC stale/zeroed-OOP corruption, NOT a socket block

A leaking run captured WITH the cross-thread stack walker (`/tmp/leakstk2/run21.log`) shows the real
cause, and it is **not** a parked socket native. During the post-test teardown/leak-handling phase there
is **widespread stale/zeroed-object corruption** — multiple distinct objects logged with an *all-zero
header* (`num_slots=0`, `class_id=ClassId(0)`) being dereferenced:

```
Stale pointer ... receiver (ptr=0x870d27d8, all-zero header) — fallback CP class java/lang/StringBuilder
Stale pointer ... receiver (ptr=0x89f988a8, all-zero header) — fallback CP class java/lang/StringBuilder
gen_heap::get_field OOB: obj=0x89fe0340 index=7 num_slots=0
Stale pointer ... receiver (ptr=0x82099d28, all-zero header) — fallback CP class java/lang/Thread
gen_heap::get_field OOB: obj=0x82099d28 index=2 num_slots=0   <- the leaked Thread's tid slot
NoSuchMethodError java/lang/StringBuilder.flush()V            <- corrupted dispatch off a zeroed obj
NPE: Cannot read field 'randomnesses' ...   NPE: Cannot read field 'group' ...
```

`0x82099d28` is the leaked thread's **`java.lang.Thread` object**, zeroed by the GC while the thread is
still registry-`alive`. That is why `getStackTrace()` is empty (the object — not a "non-depositing
native" — is the problem), why it is "uninterruptible" (operations on a freed object no-op), and why the
report shows `state=RUNNABLE` (`getState` reads the tid at field 2 of a zeroed header → registry lookup
degrades). The concurrent StringBuilder zeroing + the bogus `StringBuilder.flush()` are the same epidemic.

**Root cause is therefore the GC zeroing live objects — the "stale/zeroed-OOP dispatch" class (DF02 in
CRATONVM_BUGS / the young-gen sweep zeroing live make/check nodes; see
[[reference_reflrepro_a2_register_root]] and the precise-JIT-stack-maps work), not thread lifecycle or
socket I/O.** The intermittent reactor "thread leak" is a *downstream symptom*: a Thread object happens to
be one of the objects zeroed during teardown GC churn, so randomizedtesting can't resolve/interrupt it.
The earlier socket-frame-deposit idea is SUPERSEDED — the thread is not blocked in a socket native.

### Concrete next step (redirected)

Chase the GC stale/zeroed-OOP corruption directly (DF02), not this leak in isolation. Repro:
`/tmp/leakstk2/run21.log`; grep for `all-zero header` / `num_slots=0` to see the zeroed objects. The
cross-thread stack walker added here makes a zeroed *Thread* object visible as an empty-stack leaked
thread, but the fix is in the GC sweep/root path that is reclaiming still-live objects. CAVEAT: Heisenbug
(any `eprintln` tracing masks it) + concurrent peer-session worktree load (`cratonvm_*`) inflates the rate
and confounds measurement — run on an idle machine.

## Progress (commit `4064580d`)

One lost-wakeup window was closed: `selector_wakeup()` documented that "the woken flag still gets
observed at top of select()", but the blocking select paths (`WSAPoll`/`epoll_wait`/`poll`) only
checked `woken` in the empty-key sleep branch — the normal path went straight into the kernel wait.
A pre-wait `woken` check (drain + return 0) was added to all three `kernel_select_*` paths, so a
`wakeup()` that lands *before* `select()` enters the wait is no longer lost. Verified no regression

## Progress (commit `4064580d`)

One lost-wakeup window was closed: `selector_wakeup()` documented that "the woken flag still gets
observed at top of select()", but the blocking select paths (`WSAPoll`/`epoll_wait`/`poll`) only
checked `woken` in the empty-key sleep branch — the normal path went straight into the kernel wait.
A pre-wait `woken` check (drain + return 0) was added to all three `kernel_select_*` paths, so a
`wakeup()` that lands *before* `select()` enters the wait is no longer lost. Verified no regression
(multi-host stays 4/4; NIO connect+selector test green). **The intermittent leak persists at ~1/16**,
so the dominant cause is a *different* window (below).

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

## Deeper diagnosis (this pass)

Added gated selector tracing (`CRATONVM_DBG_SELECTOR=1`, commit pending) and ran the suite ~44 times
with it on — **0 leaks**, vs ~1/16 without it. So this is a **Heisenbug**: any `eprintln`-based tracing
(stderr lock + I/O) perturbs the interleaving and masks the race. Logging therefore cannot diagnose it.

The trace (on clean runs) established the reactor I/O workers `select()` with a **finite 1000 ms
timeout**, not an infinite one. That rules out the simplest theory: a missed wakeup on a 1000 ms select
only delays shutdown ≤1 s, which would clear well within randomizedtesting's multi-second linger window —
it cannot produce a *persistent* leak.

Candidate-elimination of the VM's blocking primitives (all can be ruled out as *permanent*-stuck causes):
- **`LockSupport.park`/AQS** — `ParkState::park` is mutex-serialised (unpark can't be lost), and
  `park_interruptible` polls the interrupt flag every **5 ms**, so an interrupted/unparked AQS waiter
  (`LinkedBlockingQueue.take` in a `ThreadPoolExecutor` worker, lock/condition `await`) self-heals in ≤5 ms.
- **`Object.wait()`** — `Monitor::wait` likewise polls every 5 ms for the interrupt flag, so an interrupted
  waiter wakes in ≤5 ms.
- **`Thread.join()`** — uses the Rust `JoinHandle::join` (OS-level), no condvar lost-wakeup.
- **Selector** — finite timeout + sticky `woken` (now) + buffered wakeup byte.

Remaining permanent-stuck candidate: a thread in `Object.wait()` or a no-timeout blocking native that is
**neither notified nor interrupted** at shutdown (so the 5 ms interrupt poll never trips). Note the
`state=RUNNABLE` in the leak report is imprecise — `getState()` (post-`16d23e7b`) maps any alive thread to
RUNNABLE, so the leaked thread may actually be parked/waiting.

## THE blocker to fix first: cross-thread stack walking

`java.lang.Thread.dumpThreads(...)` (native, `native-builtins/src/lib.rs`) returns **empty**
`StackTraceElement[][]` ("we don't have full per-thread stack walking"), which is why
`Thread.getAllStackTraces()` / the leak report shows `at (empty stack)`. Implementing it — safepoint the
target (or read its frames while it's parked in a blocked region) and materialise its frame descriptors —
would make the EXISTING leak reports show exactly where the worker is parked, **at leak-detection time,
without per-call tracing that masks the race**. That is the prerequisite for a targeted fix; do it first,
then catch one untraced leak and read the real stack.

## Next steps (remaining ~1/16 leak)

The before-entry window is closed (`4064580d`); the residual is most likely the **worker already
inside the kernel wait when `wakeup()` fires** (the UDP-loopback nudge raced/was dropped, so `WSAPoll`
didn't return) — or the worker is blocked in a *non-select* native (a blocking socket read/accept)
that no `wakeup()` can interrupt.

1. **Pinpoint the block.** Blocked here by a CratonVM gap: cross-thread `Thread.getStackTrace()`
   returns an EMPTY stack (randomizedtesting prints `at (empty stack)`), so the leak report doesn't
   say where the worker is parked. Either (a) add cross-thread stack-walk support, or (b) add gated
   selector tracing (`CRATONVM_DBG_SELECTOR`): log `tid` on WSAPoll enter/exit and on `wakeup(id)`,
   run until a leak, and check whether a thread entered `WSAPoll` and never exited despite a
   `wakeup()` for its selector.
2. If confirmed in-`WSAPoll`: make the in-flight wakeup reliable — e.g. verify the UDP `wakeup_peer`
   address/non-blocking receiver, retry the nudge, or switch the Windows wakeup to a mechanism
   `WSAPoll` can't miss. The `woken` flag is now also re-checked after the poll (Phase 3), so the
   gap is purely *interrupting an in-progress* `WSAPoll`.
3. If blocked in a non-select native: that worker loop needs an interruptible wait or a
   close-driven unblock.
4. Verify the reactor's shutdown reaches `wakeup()` for *every* worker (vs. only the first).
5. Acceptance: `RestClientSingleHostIntegTests` ≥16 consecutive clean runs (no `ThreadLeakError`),
   no regression to `RestClientMultipleHostsIntegTests` (stays 4/4).

## Related

- `Thread.getState()` fix: commit `16d23e7b`.
- ES-HANG-02 residuals + selector/connect work: [ES-HANG-02-residuals-handoff.md](ES-HANG-02-residuals-handoff.md).
