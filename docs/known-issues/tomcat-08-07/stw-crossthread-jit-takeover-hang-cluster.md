# STW cross-thread JIT takeover — stuck waiting for cooperative mutators (5-class hang cluster)

**Status:** OPEN. **Severity:** high (indefinite hang, no crash/timeout
recovery). **HotSpot:** PASS on all 5 (fresh-verified).

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

## Recommendation

Find `"STW cross-thread JIT takeover is still waiting for cooperative
mutators"` in `vm/src/runtime/interpreter.rs` and trace what
"cooperative mutator" means in this context (likely a JIT-compiled thread
checking a safepoint/poll flag at loop back-edges or call sites) — with
`taken=0` after 64 rounds, either the polling check itself isn't being hit
by the specific bytecode shapes these five classes' worker/background
threads execute (e.g. a tight native-call loop, a blocking I/O wait, or a
thread parked in a way that never re-enters JIT code to observe the
takeover request), or the takeover-request signal itself isn't reaching
the target thread. Cross-reference against
`reference_vtable_classmanager_lock_ordering_deadlock` (OPEN, in this
codebase's prior findings) and the STW/GC-safepoint related findings this
session's memory already tracks — this may be the same family or a
distinct instance of cross-thread JIT coordination not completing under
specific thread states (parked, blocked-on-native-I/O, or blocked-on-
multicast-socket-read as seen in `TestOrderInterceptor`).
