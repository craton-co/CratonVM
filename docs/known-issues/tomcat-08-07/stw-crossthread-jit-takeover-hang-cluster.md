# STW cross-thread JIT takeover — stuck waiting for cooperative mutators (5-class hang cluster)

**Status:** OPEN — root cause for the cluster now identified (see "Root
cause, 2026-07-13" below); two real, independent missing-blocking-region
bugs found and fixed along the way but **neither resolves this cluster**.
**Severity:** high (indefinite hang, no crash/timeout recovery).
**HotSpot:** PASS on all 5 (fresh-verified).

## Update 2026-07-13 — two real bugs fixed (do not close this doc — see below)

Branch `fix/elinjsp-stw-takeover-20260713`, worktree
`C:\data\CratonVM-elinjsp-20260713`. Found and fixed two genuine instances
of the missing-`begin_blocking_region`/`end_blocking_region` bug class
(same family as the already-fixed `MulticastSocket.receive` /
`Selector.select` cases — see `reference_native_io_read_stale_objectref_pin_fix`-
adjacent code and `docs/internal/fixed-suite-bugs/keycloak-model-stw-takeover-hang-eventloopgroup-shutdown-FIXED.md`):

1. **`java.net.DatagramSocket.send`/`.receive`** (`native-builtins/src/net_phase_e.rs`,
   `register_re7_datagram_socket`) — both handlers called `ctx.fd_table().udp_send`/
   `udp_recv` directly with no blocking-region bracket at all, unlike the
   parallel, correctly-bracketed `java/net/MulticastSocket` registration in
   `native-io/src/net.rs`. A thread parked in an unbounded `udp_recv` (no
   read timeout set) is neither in JIT (can't be forcibly taken over) nor at
   a safepoint (can't cooperate) — genuinely un-freezable, un-arrivable.
   Fixed: both handlers now bracket the blocking call
   (`receive` additionally re-syncs `pkt`/`data_arr` via
   `end_blocking_region_refs`, since a moving GC completing mid-block would
   otherwise leave them at stale pre-GC addresses).
2. **`HttpURLConnection`'s plain-HTTP request/response exchange**
   (`native-builtins/src/http_url_connection.rs`, `perform`) — the
   `established_https_stream_id` early-return branch and the plain-HTTP
   (non-TLS) `else` branch both did a real blocking `write_all`/`flush`/
   `read_response` (a genuine blocking `recv()`) with **no** blocking-region
   bracket and no `set_active_native_context` (unlike the HTTPS branch,
   which deliberately uses the latter for its own documented reason). Since
   `TomcatBaseTest.getUrl(...)` — the client-fetch helper nearly every
   embedded-Tomcat test in this suite uses — goes through this exact path,
   this is a broad, general-purpose fix, not narrowly scoped to the 5
   classes here. Fixed: both branches now bracket the exchange.

Both fixes are real, low-risk (pure GC-visibility bracketing, no functional
behavior change), and were spot-verified against a 60-class regression
sample (indices 1-60, `-Category all -Jit on -Jdk real`) with **no
regressions** — every test exercising the newly-bracketed plain-HTTP path
(`TestHttpServlet`, `TestServletSecurity*`, `TestCookie*`, etc.) still
PASSED; the only failures in that sample were the pre-existing, unrelated
`TestHttpServletDoHead*` cluster (documented separately, see
`reference_tomcat_dohead_gc_safepoint_deadlock` and siblings) and one
unrelated `jakarta.el.TestBeanSupport` failure.

**However: re-running all 5 target classes against the fixed binary shows
all 5 STILL HANG, identical signature.** These two fixes were real bugs
worth fixing on their own merit, but they are not what blocks this
specific cluster. See "Root cause, 2026-07-13" below for what actually
does.

## Root cause, 2026-07-13 — ForkJoinPool worker doubles as STW initiator, stranding its own pool

Deep-dived `TestOrderInterceptor` (the clearest repro) with
`CRATONVM_DBG_STW_CENSUS=1` plus a symbol-carrying build (`debug =
"line-tables-only"` is already on in `[profile.release]`; just needed a
matching `.pdb` copied alongside a uniquely-named `.exe` — the shared
worktree's `target/release/cratonvm.pdb` gets overwritten by every build,
including other concurrent sessions', so grab both files together
immediately after a build) + `cdb -p <pid> -c "~*kn 40; qd"`.

Findings, precisely evidenced:

- `[stw-request] initiator=<tid> alive=29 blocked=22 ... expected=6` — the
  STW **initiator is itself one of the `Tribes-Task-Receiver-N`
  `ForkJoinPool.commonPool()` worker threads** that Tribes' internal task
  executor spins up (confirmed across two separate repro runs: `initiator=49`
  once, `initiator=30` — always a pool worker, never `main` or a dedicated
  GC thread). I.e. this VM runs GC **on the allocating mutator thread**,
  which is normal/expected for a young-gen STW pause — but here that
  mutator happens to be a live member of a Java-level thread pool with its
  own internal coordination protocol.
- Of the 6 "expected" mutators, only 1-3 ever call `arrive_and_wait_auto`
  (confirmed via `[stw-arrive]` log lines — never more than 3 in any run,
  across minutes of wall-clock time with the process still not producing a
  JUnit summary). The remainder are **other `Tribes-Task-Receiver-N`
  threads**, all parked at the identical Java-level location:
  `AbstractQueuedSynchronizer$ConditionNode.block` <-
  `ForkJoinPool.unmanagedBlock`/`managedBlock` <-
  `AbstractQueuedSynchronizer$ConditionObject.await` (an **untimed**
  `Condition.await()` — it only wakes on an explicit `signal()`/`signalAll()`
  from another thread, never on its own).
- Symbolicated `cdb` stacks prove these stuck threads reach the VM's
  **correctly-bracketed** native `park()` path exactly as designed:
  `native_lock_support_park` -> `NativeContextImpl::park` (`vm/src/vm/vm_exec.rs:6384`,
  which does call `deposit_root_snapshot()` then `gc_barrier.enter_blocked()`
  before actually parking, matching the source) -> `ParkState::park_interruptible`
  -> `parking_lot::condvar::Condvar::wait_for` -> `WaitOnAddress`. This is
  **not** a missing-bracket bug — the parking path is fully correct and
  GC-safe.
- The actual mechanism: these sibling `ForkJoinPool` workers are waiting on
  a `Condition` that only the pool's own internal coordination (or a task
  producer) would ever `signal()`. That signal depends, transitively, on
  the pool making forward progress — which requires the initiator thread
  (itself a pool member) to return to doing pool work. But the initiator is
  now permanently inside `stw_take_over_and_wait`'s loop (spinning
  indefinitely — see `docs/known-issues/tomcat-08-07/` siblings on the
  takeover loop having no bounded fallback), never returning to the Java
  side. **The pool member the others are waiting on is the same thread the
  barrier is waiting on** — a genuine mutual-starvation livelock, not a
  simple missing GC-visibility bracket. In a real JVM this class of
  interaction is invisible because a safepoint pause is bounded to
  microseconds/low-milliseconds; here the pause is unbounded (see the
  takeover loop's lack of a hard fallback, already flagged as a defect in
  this doc's original recommendation), long enough for a thread-pool's
  internal liveness assumptions to actually break.

This matches, almost exactly, the newer
`docs/known-issues/wildfly-standalone-boot-stw-jit-takeover-hang.md`
finding (`pending=6`, hit during WildFly's `parallel-extension-add` step
spinning up 30-40 concurrent threads) — that doc's author independently
concluded "a genuinely different code path failing to reach a safepoint
under heavy concurrent thread creation/classloading" without yet pinning
the exact mechanism. Strongly suspect this is the SAME underlying
architectural gap (STW-initiator-is-a-pool-worker-and-gets-stranded),
reached via two different application-level thread-pool shapes (Tribes'
task executor here, WildFly's extension-loading executor there). The two
docs should likely be merged/cross-fixed together rather than treated as
separate bugs.

**Not attempted this session:** an actual fix. The candidate directions
(never let a Java-managed pool worker become the takeover initiator —
hand off to a dedicated background GC-driver thread instead; or bound the
takeover loop's total wait and force-degrade past a stuck initiator) both
touch core `vm/src/threading/gc_barrier.rs` / `vm/src/runtime/interpreter.rs`
STW machinery that a **different, concurrently-active session in this same
worktree** is already independently modifying (see its
`stw_takeover_should_scan` scan-cadence backoff fix, already present in
this branch's `interpreter.rs` — a real, complementary, non-conflicting fix
for a related but distinct starvation mode: excessive OS-level
suspend-scan overhead during a long wait, not this initiator-stranding
issue). Attempting a solo redesign of the STW-initiator model in a shared,
actively-edited, safety-critical file without coordinating first risks a
correctness regression far worse than this hang. Recommend: dedicated
session, coordinate with whoever owns the WildFly-hang investigation,
reproduce via the much cheaper/faster `TestOrderInterceptor` repro instead
of the multi-minute WildFly boot.

## Summary

Five unrelated-on-the-surface Tomcat test classes all HANG at the full
1200s timeout with the identical diagnostic warning repeating in the log:

```
WARN cratonvm_vm::runtime::interpreter: STW cross-thread JIT takeover is
  still waiting for cooperative mutators rounds=64 pending=N taken=0
```

Affected classes:
- `org.apache.jasper.compiler.TestJspConfig`
- `org.apache.jasper.optimizations.TestELInterpreterTagSetters` (3
  occurrences of the warning in its log)
- `org.apache.naming.TestEnvEntry`
- `org.apache.catalina.tribes.group.interceptors.TestOrderInterceptor`
- `org.apache.tomcat.websocket.TestWsWebSocketContainerTimeoutClient`

`taken=0` across all of them means the stop-the-world JIT takeover
mechanism never succeeds in getting even one mutator thread to cooperate,
for the full duration of the run (`rounds=64` and climbing) — the process
doesn't crash or recover, it just spins/blocks forever until the external
1200s test-harness timeout kills it.

`TestOrderInterceptor`'s log additionally shows repeated
`McastService` multicast-receive timeouts (`os error 10060`,
`WSAETIMEDOUT`) immediately before the STW warning starts — the tribes
membership/multicast churn may be what triggers the STW takeover attempt
in that case (heavy allocation/GC pressure from repeated socket-timeout
retries), but the other four classes don't share that specific trigger, so
multicast isn't the root cause — just one way to reach the same underlying
STW-takeover deadlock/livelock.

Found via a fresh Windows full-suite rerun (dev commit `080e79256`,
2026-07-12, real JDK, JIT on, 1200s timeout). Verified via a fresh
same-session HotSpot run (dev commit unchanged, 2026-07-13): all 5 PASS
cleanly on HotSpot.

## Reproduction

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName stwtakeover `
  -Start <idx> -Count 1 -TimeoutSec 300 -Parallel 1
# org.apache.jasper.compiler.TestJspConfig
# org.apache.jasper.optimizations.TestELInterpreterTagSetters
# org.apache.naming.TestEnvEntry
# org.apache.catalina.tribes.group.interceptors.TestOrderInterceptor
# org.apache.tomcat.websocket.TestWsWebSocketContainerTimeoutClient
```
Grep any of the five classes' stderr log for `STW cross-thread JIT
takeover` to confirm the signature reproduces.

## Recommendation (original, 2026-07-12 — superseded, kept for history)

Find `"STW cross-thread JIT takeover is still waiting for cooperative
mutators"` in `vm/src/runtime/interpreter.rs` and trace what "cooperative
mutator" means in this context — see "Root cause, 2026-07-13" near the top
of this doc for what this investigation actually found:
`taken=0`/`pending` staying nonzero is not a missing-safepoint-poll or
missing-blocking-region issue (two real instances of the latter were found
and fixed elsewhere along the way, but they don't explain this cluster);
it's the STW initiator itself being a stranded `ForkJoinPool` worker whose
sibling pool threads depend on it to make progress. Next step is an actual
fix, not further root-causing — see the "Not attempted this session"
paragraph above for the two candidate directions and why they weren't
attempted solo in this pass.
