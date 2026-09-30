# Virtual-thread stand-in frames: which HotSpot do we match?

Status: OPEN (owner decision)
Area: stack-trace capture on virtual threads (`vm/src/runtime/stackwalker.rs`
`virtual_thread_standin_frames` / `StandinScreen::VirtualThrowable`, `vm/src/vm/vm_exec.rs`
`append_native_standin_frames`); `--compatible` and `--jdk-only` disagree today
Severity: LOW (trace text of `InterruptedException` / `IllegalMonitorStateException` raised in
`sleep` / `wait` on a virtual thread)
Found by: round 14 wave 7 lane trace6 (carried from
`r13w13-trace3-vm-served-jdk-methods-residuals-FIXED-20260929.md` item 5)

## What is wrong

A "virtual" thread on this VM is a `java.lang.ThreadBuilders$BoundVirtualThread`
(`native-builtins/src/phases_late/concurrent.rs` registers `ContinuationSupport.isSupported0()` =
`false`), never a `java.lang.VirtualThread`. The JDK 25 bodies branch on
`currentThread() instanceof VirtualThread` (`Thread.sleepNanos`, Thread.java:505;
`Object.wait(long)`) or `this instanceof VirtualThread` (`Thread.join(long)`, 1867), so on this VM
the REAL bytecode always takes the platform branch -- exactly what HotSpot does under
`-XX:-VMContinuations`. HotSpot's DEFAULT runs `VirtualThread` code there instead.

The capture currently mixes the two references:

| shape (virtual thread, interrupted) | HotSpot default | HotSpot `-XX:-VMContinuations` | `--jdk-only` here | `--compatible` here |
|---|---|---|---|---|
| `Object.wait()` | `wait0`, `wait(Object.java:382)`, `wait` | `wait0`, `wait(389)`, `wait` | 389 (what ran) | 382 (wave-5 first-call rule) |
| `Thread.sleep(ms)` | `VirtualThread.sleepNanos(971 or 982)`, `Thread.sleepNanos(506)`, `sleep(540)` | `sleepNanos0`, `sleepNanos(508)`, `sleep(540)` | the `-VMContinuations` rows (`sleepNanos0` via the native-leaf rule) | caller only (the virtual screen admits no `Thread` row) |
| `t.join()` of a virtual `t` | `AQS.acquireSharedInterruptibly`, `CountDownLatch.await`, `VirtualThread.joinNanos`, `join`, `join` | `wait0`, `wait`, `join(1887)`, `join` | `-VMContinuations` rows | `-VMContinuations` rows (census; wave 7 keeps them for a `BoundVirtualThread` target) |

(Line numbers: the Temurin 25.0.3 `src.zip` on this host, and the wave-5 page's measured 382/389;
the probe host's JDK is authoritative.) Neither mode can print HotSpot-default rows for `sleep` or
`join`: that code does not run here, and `--jdk-only` must not rewrite frames that ran.

## Proposed fix

Decide the reference. Recommended: **HotSpot `-XX:-VMContinuations`** (it is what this VM's threads
ARE, and it makes the two modes agree):
- `stackwalker.rs`: in `append_native_standin_frames` (vm_exec.rs) call `native_standin_frames` for
  virtual threads too (drop `virtual_thread_standin_frames` / `StandinScreen::VirtualThrowable` and
  its `first_call`), behind a new default-on switch, e.g.
  `CRATONVM_THROWABLE_STANDIN_VIRTUAL_AS_BOUND=1` (`0` = today's rows). Effects: `--compatible`
  virtual `wait` prints 389 (as `--jdk-only`), a virtual `sleep` gets the platform chain.
- Re-take `R14Trace4NativeLeaf` `virtualWait` (then equal in both modes, differs from HotSpot default
  by design) and add a `virtualSleep` row.

The alternative (match HotSpot default) needs athrow-kind rows into `VirtualThread.sleepNanos` with a
site hint from the served sleep (pre-interrupted = first `new InterruptedException`, during the park
= second), a loaded `VirtualThread` class to read, and still leaves `--jdk-only` different; not
recommended until continuation support exists, at which point the bytecode itself runs those frames.

## How to confirm

`C:\craton\jitr14-probes\src\R14Trace4NativeLeaf.java` `virtualWait` row and
`C:\craton\jitr14-probes\src\R14Trace6ArgCheckSites.java` `vjoin` row, default and `--compatible`
arms: today `virtualWait` differs between the two CratonVM arms; after the fix both CratonVM arms
print the same rows.
