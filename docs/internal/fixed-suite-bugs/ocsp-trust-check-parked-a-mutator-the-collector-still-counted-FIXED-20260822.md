# A server-side OCSP fetch parked a thread the collector still counted, and the whole VM stopped — `ocsp.TestOcspSoftFailInternalError`

| | |
|---|---|
| **Status** | ✅ FIXED — 2026-08-22, `native-builtins/src/x509_manager.rs` + `t27_tls.rs` |
| **Severity** | high — a whole-VM stall, not a test failure: every thread parks behind it |
| **HotSpot** | PASS (20 tests) |
| **CratonVM** | `rc=124` at a 900 s cap, and again at 1800 s → **OK (20 tests), WALL 7 s** |
| **Root cause** | `gc_blocked_syscall()` reads a thread-local that only the HTTPS **client** path published, so on the **server** trust-check path the `GcBlockingSocket` wrapper was inert and the thread blocked in `recv` while still counted as a cooperative mutator |

## How it was found, and the two wrong turns on the way

The class stalled with one line of evidence:

```
WARN cratonvm_vm::runtime::interpreter::gc_and_alloc: STW cross-thread JIT
     takeover is still waiting for cooperative mutators rounds=64 pending=1 taken=0
```

`pending=1` — exactly one thread refusing to cooperate. Finding *which* took two
corrections worth recording, because both are reusable mistakes.

**Wrong turn 1: "it is not OCSP, because the log never says OCSP."**
`grep -ci ocsp` on the run's log is 0, and the stall begins ~1 s after
`Starting ProtocolHandler`. That is reasoning from absent log *text* about code
that logs nothing. The stack says OCSP.

**Wrong turn 2: "the OCSP check is client-side."** `check_revocation`'s own
comment says *"this is only the client-side (`PKIXRevocationChecker`) default"*,
which is true of the 30 s timeout constant it sits next to and not of the call
path. The stall is on `checkClientTrusted` — the **server** validating the
**client's** certificate.

**What settled it** was `sudo gdb -p <pid> -batch -ex 'thread apply all bt'`
(plain `gdb` cannot attach here — `ptrace_scope`). One of 34 threads:

```text
Thread 11 "https-jsse-nio-":
  #0  __libc_recv (fd=19)                                   <- BLOCKED
  #6  GcBlockingSocket::read        net_phase_e.rs:8110     <- the wrapper IS here
  #7  ocsp_http_post                x509_manager.rs:3477
  #8  check_ocsp                    x509_manager.rs:3586
  #9  check_revocation              x509_manager.rs:3678
  #10 validate_ordered_chain        x509_manager.rs:2328
  #11 validate_chain                x509_manager.rs:2125
  #12 do_check_trusted              x509_manager.rs:5170
  #13 check_client_trusted          x509_manager.rs:5127
```

Two `/proc` samples 6 s apart had already shown `utime` flat on that thread —
genuinely blocked, not looping — which is what ruled out a perf cliff before the
debugger was reached for.

## Root cause

`GcBlockingSocket::read` calls `t27_tls::gc_blocked_syscall()`, which opens the
region through `with_active_native_context` — a **thread-local** published by
`set_active_native_context`. Its own doc says it "returns an inert guard when no
context is published — that is the pre-existing behaviour, not a new failure
mode, and the paths this is used from all publish one."

That last clause had become false. Both publishers are on the HTTPS **client**
path (`http_url_connection::perform`'s handshake loop, and `net_phase_e`'s
raw-socket path). Nothing published on the **server** trust-check path, so the
wrapper was on the stack and doing nothing: the thread sat in `recv` on the OCSP
responder while the collector still counted it as a cooperative mutator. A
stop-the-world request could then never be satisfied — the thread is neither in
JIT code (cannot be forcibly taken over) nor at a safepoint (cannot cooperate) —
and every other thread, including the client half of the very exchange that
responder answer belonged to, parked behind it. Nothing could make progress
until the harness timeout.

This is the third instance of one hazard class in this tree (`gc_blocked_syscall`
and `net_phase_e`'s `re5` note record the first two), and the first where the
mechanism was present but **inert**.

## Fix

1. **`x509_manager::do_check_trusted` publishes the context.** Publishing does
   not mark the thread blocked; it only makes the region `GcBlockingSocket`
   opens around each syscall reachable. The function runs Java (`invoke_virtual`
   on the delegate `TrustManager`, allocations), which a blocked thread must
   never do — and does not have to, because the region covers the syscall and
   nothing wider. Same split `gc_blocked_syscall` documents for rustls.

2. **`ActiveNativeContextGuard` now saves and restores** the previous pointer
   instead of clearing to `None`. With a second publisher the two nest on a
   client connection that validates a chain, and the old drop would have handed
   the enclosing frame an empty window — for `JavaKeyManagerResolver::resolve`
   that is not a crash but "no client certificate", a silent wrong answer on the
   one path whose whole job is to produce one. Restoring is sound because the
   outer pointer comes from a frame that is still live.

**Regression test:**
`t27_tls::tests::a_nested_active_native_context_restores_the_outer_one` asserts
through `with_active_native_context` — the real reader — that an inner guard
publishes its own context while alive, restores the OUTER one on drop, and that
the outermost guard still clears. Negative control (drop reverted to
`set(None)`, test kept): red on the restore assertion.

## Verification

`bin/cratonvm-stw-34653fd43`, Azure Linux, real JDK 25, `apps/tomcat` fixture,
run alone (see the measurement hazard below):

| | before | after |
|---|---|---|
| `ocsp.TestOcspSoftFailInternalError` | `rc=124` @900 s, and @1800 s | **OK (20 tests)** |
| wall | ≥1800 s (never completed) | **7 s** |
| `STW cross-thread …` warnings | 1, then silence | **0** |
| test cases reached | 1 of 20 | **20 of 20** |

HotSpot runs the same 20 tests. Re-run on the SAME binary 3× sequentially:
`OK (20 tests)` every time, walls **7 s / 7 s / 6 s**, zero `STW` warnings —
this is not a coin toss that landed heads once. Before the fix the same class
never reached test case 2 in any of three arms, including `--nojit`.

Full 16-class SSL/TLS + OCSP sweep on the same binary: **15 green**, up from 14
before this change and 12 before the session-resumption fix that preceded it.
The two that remain red are the accepted renegotiation-emulation limits tracked
in `known-issues/tomcat/ssl-renegotiation-emulation-limits-20260822.md`
(`TestClientCert.testClientCertPostZero`, `TestSsl.testClientInitiatedRenegotiation`).
Nothing green before this change is red after it.

`cratonvm-native-builtins` 4136 passed / 1 failed — the same
`proxy_selector::tests::env_proxy_lookup_respects_case_insensitive_windows_storage`
that fails on the merge base with these changes reverted.

## Measurement hazard, kept from the OPEN page it retires

The fixture takes an exclusive **`flock`** on
`apps/tomcat/test/org/apache/tomcat/util/net/ocsp/ocsp-responder.lock`, so two
concurrent CratonVM runs of *any* OCSP class serialise and the loser blocks in
`locks_lock_inode_wait` for its whole timeout — an `rc=124` indistinguishable
from this defect. Confirmed in `/proc/locks` (holder and waiter on inode
`27810154`). `/data/cratonvm/apps/tomcat` is a **shared** fixture on a
multi-session host, so a neighbouring session is enough. Run OCSP classes one at
a time before believing a hang.

Separately: HotSpot takes a **POSIX** (`fcntl`) lock on that same file where
CratonVM takes a **`flock`**. The two kinds do not block each other on Linux, so
a HotSpot control does not serialise against a CratonVM run — which means a
HotSpot control cannot be used to prove the lock was free, and is a
`FileChannel.lock()` divergence worth its own look. Recorded, not fixed here.
