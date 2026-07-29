# Spring Boot `ThreadDumpEndpointTests` JMX `ThreadInfo` diagnostic fidelity

**Status: FIXED - 2026-07-18**

> **Confirmed still failing 2026-07-23** — `ThreadDumpEndpointTests.dumpThreadsAsText`
> fails again in the `craton-rerun-20260723` results, and this worktree's HEAD
> (`a3d75f295`, a 2026-07-23 merge of `origin/dev`) already contains this fix
> as an ancestor commit — so this is not a stale-binary artifact, it's a
> genuine regression on top of the fix. The actual dump text is missing
> exactly the piece this doc's "Resolution" claims is covered: no
> `"\t- parking to wait for <addr> (a java.util.concurrent.CountDownLatch$Sync)"`
> line appears anywhere for the `"Awaiting CountDownLatch"` thread (it shows
> `WAITING` state and a frame, but the parking-blocker/lock annotation the
> assertion regexes for is simply absent from that thread's block):
> `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard7/logs/module_spring-boot-actuator.org.springframework.boot.actuate.management.ThreadDumpEndpointTests.out.log`.
> Not re-investigated this session (out of scope — this doc's fix predates
> the rerun and the discrepancy needs a fresh trace of the JMX
> snapshot/parking-blocker code this doc describes, not a re-derivation from
> the original symptom). Flagging as a confirmed root-cause/status
> discrepancy rather than re-filing a duplicate doc.
>
> **Confirmed still failing 2026-07-28 (craton-rerun-20260728)** — identical
> symptom to the 2026-07-23 note above: `dumpThreadsAsText()` still fails
> the exact same regex match, the `"Awaiting CountDownLatch"` thread's block
> still has no `"\t- parking to wait for <addr> (a
> java.util.concurrent.CountDownLatch$Sync)"` line (shows `WAITING` state
> and a frame, but no parking-blocker annotation):
> ```
> to contain pattern:
>   "	- parking to wait for <[0-9a-z]+> \(a java\.util\.concurrent\.CountDownLatch\$Sync\)"
>        org.springframework.boot.actuate.management.ThreadDumpEndpointTests.dumpThreadsAsText(ThreadDumpEndpointTests.java:102)
> ```
> Log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260728/shard2/logs/module_spring-boot-actuator.org.springframework.boot.actuate.management.ThreadDumpEndpointTests.out.log`.
> Not re-investigated further this session (log-analysis/triage only, no
> build or test execution performed) — the same fresh trace of the JMX
> snapshot/parking-blocker code this doc's 2026-07-23 note called for is
> still the needed next step.

## Boundary

This was independent of the prior JUnit 5 invocation-liveness repair. Its acceptance class reached the endpoint only after `Thread.getState()` accurately reported `WAITING` and `BLOCKED`.

## Resolution

CratonVM now captures a moving-GC-safe JMX snapshot for each live Java thread. The snapshot includes the frame trace, contended and waited-on monitors, monitor owner, locked monitors, parking blocker, and ownable synchronizers. Monitor transitions update the snapshot on enter, exit, `Object.wait`, and synchronized-frame teardown; thread-registry root enumeration and relocation remap every retained lock reference.

`AbstractOwnableSynchronizer.setExclusiveOwnerThread` is routed through a native hook in both interpreter and JIT dispatch, maintaining the ownable-synchronizer index without a racy heap walk. The JMX bindings materialize real JDK 25 `ThreadInfo`, `MonitorInfo`, and `LockInfo` layouts, including `Thread.getState()`, lock owner details, stacks, and monitor/synchronizer arrays.

The compatibility `CountDownLatch` implementation blocks on its public monitor rather than creating the JDK-private `Sync`; its JMX projection therefore reports the public JMM-visible logical lock type, `CountDownLatch$Sync`, instead of leaking that surrogate.

## Validation

Using the task-unique CratonVM binary and real JDK 25:

- `ThreadDumpEndpointTests` passed with `--nojit` (1.1 s, release artifact).
- The same class passed with JIT enabled (1.3 s, release artifact).
- `cargo check -p cratonvm-native-api -p cratonvm-native-builtins -p cratonvm-vm` passed.

The Spring Boot text dump now contains the expected latch park relation, contended-monitor owner relation, monitor state, and `ReentrantReadWriteLock$NonfairSync` ownable synchronizer.

## Residuals

None found in the affected JMX snapshot, monitor, parking, or AQS ownership paths.
