# `OnClassCondition`'s second thread never finishes, and `main` waits in `Thread.join` forever — FIXED / RETIRED 2026-08-04

**Status: OPEN.** Signature captured with a watchdog stack dump; cause not
located. Filed because it is what stopped
`BasicErrorControllerIntegrationTests` from producing 14 *consecutive* clean
runs on 2026-08-01 — the class itself is green (see
[`basicerrorcontroller-jit-only-failure-20260731-FIXED.md`](basicerrorcontroller-jit-only-failure-20260731-FIXED.md))
and this is a separate defect that was in the way.

## Symptom

The process stops making progress between two per-test Spring Boot context
boots and never resumes. No exception, no output, no exit.
`--stack-dump-on-timeout 1500` names the wait site exactly:

```
tid=0 os_tid=3449799 name="main" alive=true daemon=false blocked=true roots=973
  top=java/lang/Thread.join@129
   <- java/lang/Thread.join@2
   <- org/springframework/boot/autoconfigure/condition/OnClassCondition$ThreadedOutcomesResolver.resolveOutcomes@4
```

and the 90-odd frames below it are an ordinary Spring Boot startup:

```
BasicErrorControllerIntegrationTests.load
  SpringApplication.run → refreshContext → refresh
    AbstractApplicationContext.invokeBeanFactoryPostProcessors
      ConfigurationClassPostProcessor.processConfigBeanDefinitions
        ConfigurationClassParser$DeferredImportSelectorGrouping.getImports
          AutoConfigurationImportSelector.getAutoConfigurationEntry
            OnClassCondition$ThreadedOutcomesResolver.resolveOutcomes
              Thread.join()
```

`OnClassCondition` splits Spring Boot's auto-configuration class-presence
filtering across TWO threads — the caller evaluates one half, a spawned thread
evaluates the other, and the caller `join()`s it. The spawned thread never
completes, so `join()` never returns.

Two occurrences captured this way (2026-08-01), both at the same wait site.

## What is and is not established

**Established.** The wait site, and that the process is otherwise idle.

**NOT established.** What the spawned thread is doing. The dump prints frames
for the wait-site thread only; every other entry in the 226-thread summary is
`top=<no-frame-trace>`, and the summary's `alive=` / `blocked=` columns are not
maintained (every thread but `main` reads `alive=false blocked=true
state="<unset>"`, including threads that must have been live). **Do not read
`alive=false` on the joined thread as "the thread is dead and the join missed
its wakeup"** — that field cannot support the claim. Extending the watchdog to
dump every registered thread's frames is the obvious next step and would
probably settle this in one occurrence.

## Rate

Same class, same host and fixture, 3 concurrent, 2026-08-01:

| binary | runs | stalls |
|---|---|---|
| pristine `origin/dev` `5443fae920` | 34 | **0** |
| the `fix/basicerrorcontroller-jit-20260801` branch, before merging dev | 20 | 2 |
| the same branch merged with dev | 54 | 1 |

**Not load-gated, contrary to a first reading.** The first two occurrences
happened while the shared 16-core host was carrying an external load average
above 100, which invited the conclusion that contention was the trigger. The
third happened at load ~22, on an otherwise quiet box. Scheduling pressure may
change the odds; it is not the mechanism.

Three events in 74 runs of the branch against **0** in 34 of pristine dev
**does not distinguish the two trees**: at a ~4% rate a 34-run control comes up
empty about a quarter of the time. A same-binary A/B of the branch's only
JIT-churn change at the time (an optimizing-tier code-buffer retry, since
removed, 20 interleaved on/off pairs) came back 20/20 clean on BOTH arms, so
that change was not the trigger either. Anyone tempted to blame — or clear — a specific change on these
numbers should collect a much larger control first.

## Where to look first

`OnClassCondition`'s two-thread filtering already produced one confirmed
CratonVM defect in the same shape — the main thread and the resolver thread
race to load the same classes through an isolated loader, and
`ucl_try_define_local_class` (`native-builtins/src/classloader.rs`) probed
`find_loaded_class_for_loader` before taking its per-`(loader, name)` define
lock and never re-probed inside it. That one surfaced as
`NoClassDefFoundError`, was fixed 2026-07-27, and is written up in
[`data-redis-urlclassloader-uncached-classpath-hang-FIXED.md`](data-redis-urlclassloader-uncached-classpath-hang-FIXED.md)
(item 4). A *hang* in the same two-thread window is the natural sibling: the
resolver thread parked on a define lock whose holder never clears
`in_progress`, or the two threads each holding what the other waits for.

Second candidate, independent of classloading: the thread-termination
notification. If a thread can finish without waking a `join`er, this shape
follows directly. `Thread.join(long)` is `while (isAlive()) wait(0)` under the
thread object's monitor, so the exit path must publish liveness and
`notifyAll` **under that same monitor** — a notify that does not take it can
slip between the `isAlive()` check and the `wait`.

## Reproduction

Not reliably reproducible on demand — three occurrences in 54 runs.

```bash
D=/data/data/spring-boot-tomcat-crossmodule-20260717
CP="$(cat $D/module/spring-boot-webmvc/build/cratonvm-test-cp.txt):$D/sb-runner"
CLS=org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests
cd "$D" && <cratonvm> --stack-dump-on-timeout 1500 -Xmx2g -cp "$CP" SbRunner "$CLS"
```

Always arm `--stack-dump-on-timeout`; the first occurrence was killed by the
harness with no dump and cost the information.

## Affected classes

- `module/spring-boot-webmvc` — `org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests`. Nothing about the stall is specific to this class: `OnClassCondition` runs on every Spring Boot context boot, and this class boots one per test, which is simply a lot of chances.

## Correction (2026-08-04): the "second candidate" theory does not apply here

Before closing this doc, its "Second candidate" theory (a missed
`notifyAll` under the Thread object's own monitor, because `join()` is
`while (isAlive()) wait(0)`) was checked against the actual implementation
and does not hold for this codebase. `Thread.join()` with no timeout
(`native_thread_join` in `native-builtins/src/lang_system.rs` →
`ThreadRegistry::join`, `vm/src/threading/thread_registry.rs`) blocks on a
raw `std::thread::JoinHandle::join()` — an OS-level join with no Java
monitor involved at all. It unblocks unconditionally the moment the spawned
OS thread's closure returns, whether that closure returns normally, via an
early `?`, or by unwinding a panic — there is no notify/wakeup step in this
path for a signal to miss. Left here so a future investigator does not
re-chase this specific mechanism; if a join-hang recurs, the closure body
in `vm/src/vm/vm_exec.rs` (the ~150-line thread-termination sequence after
`Runnable.run()` returns: `deposit_root_snapshot` →
`GcBarrier::finish_after` → `mark_dead` →
`release_monitors_held_by_except` → the termination monitor's
`notify_all`/`exit`) is the more promising place to look for a **true**
indefinite hang, since it is the one unconditional step between "the
resolver's Java work is done" and "the OS thread actually returns."

## Closure (2026-08-04)

Re-verified from scratch in a fresh worktree
(`/data/data/wt-occjoin-20260803`, branch
`fix/onclasscondition-join-hang-20260803`, forked from `origin/dev` @
`a9241eedf3`), specifically to see whether this doc's own suggested next
step — a full per-thread frame dump on the watchdog, instead of the
3-frame-capped, blocked-threads-only summary that made the original
capture inconclusive — would settle it in one occurrence, as the doc
predicted. It did not need to: the bug did not reproduce at all.

**Diagnostic added regardless of outcome** (kept and merged since it is a
genuine improvement, independent of this bug):
`ThreadRegistry::dump_thread_summary_to_stderr` now also prints every
*alive* thread's complete deposited frame chain, not just a 3-frame `top=`
for threads that happened to block. The original capture's "every thread
but `main` reads `alive=false` ... including threads that must have been
live" observation was investigated with this — it is very unlikely to
have been reading a live resolver thread's state at all. This process runs
one Spring Boot context boot per test method in a long-lived loop, and the
registry never prunes dead entries (`ThreadRegistry::mark_dead` only flips
a flag), so a 226-entry summary late in such a run is expected to be
almost entirely stale threads from earlier, already-completed test
methods. `alive=false` on those is correct, not evidence of a missed
wakeup.

**499 clean runs, zero reproductions**, `--stack-dump-on-timeout` armed
throughout, classifying each watchdog fire by dump *content* (only a
`main` thread reported `blocked=true` sitting in
`java/lang/Thread.join` — the doc's own signature — would count) rather
than by "did the watchdog fire at all": this shared host spends much of
its time at load 15–47 from other concurrent sessions, and a merely-slow
Spring Boot boot under that load fires the same watchdog while `main` is
still actively dispatching bytecode (`blocked=false`) — noise, not a
repro. Same fixture, same class, same repro command as this doc's own
"Reproduction" section, `CRATONVM_REAL=net-sockets,aqs` +
`CRATONVM_JIT=rootsnap-cache` (matching `/data/sbrun.sh`, the actual
suite-runner's flags — real AQS/sockets, not the synthetic replicas, per
[[feedback_synthetic_replica_can_hide_the_bug]]), real JDK 25
(`/data/jdk25-real-20260717/jdk-25.0.3+9`):

| tree | runs | clean | notable (host-contention slow / one unrelated known SIGSEGV) |
|---|---|---|---|
| `fix/onclasscondition-join-hang-20260803` @ `a9241eedf3` (34-run + 24-run + 200-run + 300-run batches) | 558 attempted | 419 | 138 slow, 1 crash (`docs/known-issues/jit/sigsegv-in-unmapped-code-buffer-20260801.md`, unrelated) |
| the same branch **merged with `origin/dev` @ `e22e61cfe7`** (332 commits ahead) | 80 | 80 | 0 |

At this doc's own documented rate (~4%, 3 events in 74 runs), the odds of
419 clean runs with zero reproductions by chance alone are on the order of
1 in 20 million (`0.96^419`); the 80/80 clean confirmation on the merged
tree — the one that actually ships, and where a fix like this could
otherwise be silently relocated or dropped — adds to that rather than
resetting it.

**No code change was needed or made** to fix the hang itself (the frame-dump
diagnostic above is retained as a standalone improvement, not a fix for
this bug). Investigated whether a specific landed commit explains the
disappearance: `4a1c141bd1` ("a LinkageError from a fast-path invoke
skipped every Java handler", 2026-08-01) closes exactly the gap where
`IncompatibleClassChangeError` — the precise exception type
`ucl_try_define_local_class`'s classloader-race retry logic is designed to
produce and catch (see "Where to look first" above) — used to escape
*uncaught* past every Java exception handler when thrown from the
`invokestatic`/`invokespecial`/`invokevirtual`/`invokeinterface` fast
paths, which is exactly how `Class.forName`/`ClassLoader.loadClass` are
reached. It is not a clean single-commit explanation, though: this doc's
own rate table's third row ("the same branch merged with dev": 1 stall in
54 runs) was very likely already built on a binary that included
`4a1c141bd1` (that fix landed at 13:16 -03 on 2026-08-01; the branch's own
merge-with-dev commits are timestamped 00:21–01:33 UTC on 2026-08-02, i.e.
after it), and still hit one stall. The more defensible reading, consistent
with this doc's own "Not load-gated" section and with the identical
pattern in
[`basicerrorcontroller-class-cluster-20260728-FIXED.md`](basicerrorcontroller-class-cluster-20260728-FIXED.md)'s
closure, is that the fast pace of concurrent, independently-landed
classloading/JIT/GC fixes across 2026-08-01 through 2026-08-03 — including
but not limited to `4a1c141bd1` and `71fc283d37` (JVMS 5.3.4 loader
constraints at supertype link, same day) — shifted timing enough that
whatever race window this needed no longer opens, rather than any one
commit closing it.

**Retiring this doc.** If the exact signature reappears (`main` blocked in
`Thread.join` under `OnClassCondition$ThreadedOutcomesResolver`), it
deserves its own live repro with the frame-dump diagnostic above, not a
reopen of this one — this closure has no single fix commit to point a
regression at.
