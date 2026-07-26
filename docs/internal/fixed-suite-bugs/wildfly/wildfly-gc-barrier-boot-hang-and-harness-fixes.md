# WildFly-under-CratonVM boot: GC-barrier hang FIXED, container.java.home + orphaned-server harness gaps closed

Status: FIXED / CLOSED — the three items this doc tracks are all resolved as of 2026-07-07, dev @ `bee86ff0`+
(GC-barrier fix) / harness fixes below (not code-tracked, see "How to apply" for each).

This closes out the three follow-up items flagged in
`wildfly-infinispan-remove-listener-segfault.md`'s "Related harness
findings" and in `wildfly-xnio-mockselector-mutex-segfault.md`'s "Reproduction attempt 2026-07-07 —
current dev HANGS at boot" section.

## 1. GC-barrier boot-hang — FIXED (dev `bee86ff0`)

**Symptom**: WildFly standalone boot under CratonVM (real-JDK backend, JIT on) wedged permanently
partway through boot — no crash, no progress, just a permanent stall, reproducing on essentially every
attempt.

**Root cause**: `../../../../vm-cli/src/main.rs`'s post-`main()` cleanup calls
`ThreadRegistry::wait_for_non_daemon_threads(None)`, which blocks the calling thread (`main-vm`) in a raw
`pthread_join` loop. This function never calls into the GC barrier's blocked-region bookkeeping
(`GcBarrier::enter_blocked`/`mark_blocked_region_enter`), so `main-vm` stays counted as an "alive,
not-blocked" participant in `GcBarrier::expected` for as long as this wait lasts — which, for a real
server whose bootstrap `main()` hands off to background (non-daemon) worker threads, can be the entire
remaining lifetime of the process. `main-vm` never runs Java bytecode again once here, so it can never
reach an interpreter safepoint to cooperatively call `arrive_and_wait` — any GC/STW pause requested by one
of the still-running worker threads (WildFly's `ServerService Thread Pool` / `EnhancedQueueExecutor`
workers routinely trigger GC during class loading) then waits forever for a participant that will never
arrive.

Root-caused via a live gdb attach caught at the exact stall moment (a tight poll loop watching the
process's own `CRATONVM_DBG_STW_CENSUS=1` diagnostic output, gdb-attaching the instant "still waiting for
cooperative mutators" appears — see the reusable technique note below). The dump showed
`stw_take_over_and_wait` spinning at `rounds=64 pending=1 taken=0` while `main-vm`'s own backtrace was:

```text
Thread N (LWP ...) "main-vm":
#0  __futex_abstimed_wait_common64 (...)
#3  __pthread_clockjoin_ex (...)
#4  <std::sys::thread::unix::Thread>::join ()
#6  cratonvm_vm::threading::thread_registry::ThreadRegistry::join ()
#7  cratonvm_vm::threading::thread_registry::ThreadRegistry::wait_for_non_daemon_threads ()
#8  cratonvm::run ()
```

— while the other two expected participants were correctly parked in `arrive_and_wait_inner` (having
cooperatively reached an interpreter safepoint), confirming `main-vm` specifically was the uncounted
holdout, not a JIT-codegen or safepoint-polling gap.

**Fix**: wrap the wait with `gc_barrier.mark_blocked_region_enter()` / `mark_blocked_region_leave()`,
mirroring the already-established, already-tested pattern `NativeContextImpl::park` uses for the identical
"genuinely blocking, no bytecode running" shape (`../../../../vm/src/vm/vm_exec.rs`) — including handling the
`pre_stw` race (a pause already active at the moment of entering the blocked region) by calling
`arrive_and_wait` once before actually blocking, exactly as `park` does.

**Verification**:
- The standalone.sh direct-boot repro (previously hanging on effectively every attempt, always consuming
  the full external timeout — 150s/180s/200s across many tries) now completes without hanging on 2/2
  consecutive attempts, reaching a separate, pre-existing, unrelated blocker (the `java.util.logging`
  LogManager gap, see the sibling doc) in 15-70s instead.
- A quick multi-threaded regression probe (`main()` starts a non-daemon thread that calls `System.gc()` in
  a loop, then `join()`s it before returning) still completes correctly and promptly (0.485s wall-clock),
  confirming no regression for the common case where the wait never actually races a GC.
- The real Arquillian/Maven-driven repro (`EarClassLoadingTestCase`, the harness path actual test suites
  use) dropped from a consistent 68-69s (always hitting Arquillian's internal 60s "managed server not
  started" timeout) to 12s (fails fast on the LogManager gap instead) once both this fix and the
  `container.java.home` fix below were in place together.

**Reusable technique — catching an exact-moment live gdb attach on a non-deterministic-timing stall**:
launch the repro backgrounded, then in the SAME shell script poll the boot log every ~0.3s for the
diagnostic line that only appears at the stall (`CRATONVM_DBG_STW_CENSUS=1`'s "still waiting for
cooperative mutators"), and `sudo -n gdb -q -batch -p $PID -ex 'thread apply all bt' -ex detach -ex quit`
the moment it appears. A fixed `sleep N` before attaching is unreliable — the stall's exact timing varies
run to run, so either you attach too early (process hasn't reached the stall yet, backtrace shows normal
boot activity) or too late (the census log line you're cross-referencing is stale by the time you attach).
The tight poll-and-pounce loop removes the guesswork.

## 2. `container.java.home` harness gap — closed (build + document a `cratonvm-javahome` dir; wire into runner)

**Symptom** (documented in the infinispan doc): `container.java.home` isn't literally undefined — WildFly's
own `testsuite/integration/pom.xml`/`testsuite/preview/pom.xml` default it to `${java.home}`, i.e.
whatever JVM **Maven itself** runs under (real JDK, by the runner's own design — Maven stays on a
known-good JDK while only the Surefire-forked test JVM runs on CratonVM). Left at that default, the
Arquillian-managed WildFly server Maven spawns runs under real JDK, never exercising CratonVM as the
WildFly host at all — only as the JUnit/Arquillian *client* JVM.

**Fix (harness-level, not code — `apps/wildfly-suite-runner` and `../../../../apps/wildfly` are NOT git-tracked, so
this must be reproduced per-host)**:
1. Build a normal CratonVM release binary.
2. Create a directory shaped like a JDK home with only `bin/java` populated:
   `mkdir -p <dir>/bin && cp <cratonvm-binary> <dir>/bin/java && chmod +x <dir>/bin/java`. No other JDK
   layout (`lib/`, `conf/`, etc.) is needed — Arquillian's `ManagedDeployableContainer` only ever invokes
   `<javaHome>/bin/java <args...>` directly; it doesn't inspect the rest of the directory.
3. Pass `-Dcontainer.java.home=<dir>` via `MAVEN_ARGS` (or `run-suite-linux.sh`'s own default `MAVEN_ARGS`
   construction, if adding this as a standing default to a local runner copy).
4. `CRATONVM_JAVA_HOME` (already set by `run-suite-linux.sh` for the Surefire-forked client JVM) is
   inherited by the spawned server process automatically — no separate wiring needed, since the server
   process is a child of the (CratonVM-hosted) client process and env vars propagate down. The server
   correctly picks up real-JDK class support through the same env var.

**Verified working**: direct-invocation smoke test (`<cratonvm-javahome>/bin/java -jar jboss-modules.jar
...` with the full, correct `-mp` including both the base WildFly modules dir and the testsuite's module
overlay) produces real WildFly boot log output (JBoss Remoting version banner, Undertow/JAXRS/deployment-
scanner activation, etc.) — i.e. CratonVM genuinely executes the server's bytecode, not a fast-fail. Then
confirmed end-to-end via the real harness (`run-suite-linux.sh` + `MAVEN_ARGS=-Dcontainer.java.home=...`):
the previously-uniform "every attempted class either FAILs fast (~19-23s, container.java.home unset) or
occasionally CRASHes" signature became "the server genuinely attempts to boot, taking 60s+ and hitting
Arquillian's own internal readiness-poll timeout" — i.e. real, different, and correctly-attributable
behavior instead of the harness silently testing real JDK the whole time.

**Gotcha discovered**: a hand-rolled direct boot command that only passes ONE `-mp` path (the base modules
dir, omitting the testsuite module overlay dir) fails with an unrelated
`WFLYCTL0079: Failed initializing module org.jboss.as.logging` — a manual-repro artifact matching this
codebase's own established "[[jboss-modules-repro-needs-mp-root]]" lesson, not a real bug. Always use the
module's own `standalone.sh`/the real harness's constructed `-mp`, or combine
`<wildfly-dist>/modules:<testsuite-module>/target/modules` explicitly, when hand-rolling a repro.

## 3. Orphaned-server harness bug — already fixed, now documented

**Symptom** (documented in the infinispan doc): when the Surefire-forked CratonVM test JVM crashes or
times out, the Arquillian-managed WildFly server it spawned as a child process previously survived
(no process-group cleanup), staying bound to the module's default ports (8080/9990/etc.) indefinitely and
corrupting every subsequent class's run in that module until manually killed.

**Status**: already fixed in the current `run-suite-linux.sh` (present in
`/data/data/wt-wildfly-bugbash-20260707-runner/run-suite-linux.sh` and its copies) — undocumented until now
since the runner script is not git-tracked. The per-class invocation is wrapped in `setsid` (making the
forked Maven process its own session/process-group leader) and, after `wait`, the runner sends
`kill -TERM -- "-$class_pid"` (negative PID = whole process group) followed by `kill -KILL` after a 1s
grace period — guaranteed to reap the whole tree including any spawned WildFly server, regardless of
whether the class completed normally, crashed, or timed out. The script's own comments note an earlier
version used a pre-class `pkill` scoped by module path instead, which had a real bug: concurrent shards
processing different classes of the *same* module share a `jboss.home.dir`, so one shard's "cleanup" could
kill a sibling shard's legitimately-running server (observed as spurious `LifecycleException` /
`SIGKILL exit 137`). The process-group approach fixes this by scoping the kill to exactly one invocation's
own PGID.

**How to apply**: when setting up a fresh copy of `apps/wildfly-suite-runner` on a new host, copy
`run-suite-linux.sh` from an existing worktree that already has this fix (search for `setsid` in the
per-class invocation) rather than an older/stale copy — this script isn't git-tracked, so there's no single
canonical source of truth to `git pull`; verify by grepping for `setsid` and the process-group `kill`
comment before relying on a copy for a long unattended run.

## Related

- `wildfly-infinispan-remove-listener-segfault.md` — the SIGSEGV whose
  doc first flagged these three harness findings as follow-up work.
- `wildfly-logging-subsystem-requires-real-logmanager.md` — the next blocker in the
  chain, newly reachable now that these three are resolved.
- `docs/known-issues/wildfly-xnio-mockselector-mutex-segfault.md` — its "Reproduction attempt 2026-07-07"
  section documented the same GC-barrier hang from an earlier, less-precise investigation (`CRATONVM_DBG_STW_CENSUS`
  census only, no live gdb attach at the exact stall) before this session's live-attach root-cause.
