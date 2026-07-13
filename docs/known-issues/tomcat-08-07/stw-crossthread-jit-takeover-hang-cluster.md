# STW cross-thread JIT takeover — stuck waiting for cooperative mutators (5-class hang cluster)

**Status:** RESOLVED for the actual STW-takeover mechanism (root-caused and
fixed, 2026-07-13, see below) — validated via `TestOrderInterceptor`, which
now PASSES cleanly (was 100% hang before). **4 of the original 5 classes
still hang, but for an unrelated reason** — see "Residual: 4 classes still
hang, but NOT an STW-takeover bug" at the bottom; that part needs its own
follow-up, likely merged with `elinjsp-socket-read-timeout.md`.

**HotSpot:** PASS on all 5 (fresh-verified, 2026-07-12).

## Root cause (found 2026-07-13) — raw Rust locks with zero GC-blocking-region bracket

Branch `fix/elinjsp-stw-takeover-20260713`, worktree
`C:\data\CratonVM-elinjsp-20260713`, merged to `dev` at `73b916123` (interim)
and again with the real fix below.

### How it was found

An earlier pass this session (see git history on this doc / branch) found
and fixed two real but *insufficient* missing-`begin_blocking_region` bugs
(`DatagramSocket.send`/`.receive`, `HttpURLConnection`'s plain-HTTP
exchange) and initially mis-diagnosed the remaining hang as a "ForkJoinPool
worker doubling as STW initiator strands its own pool" architectural
livelock — that hypothesis was **wrong**, based on reading an unsymbolicated
`cdb` stack too coarsely (it matched the shape of a correctly-bracketed
`LockSupport.park()` call). Rebuilding with a **matching `.pdb`** (the
release profile already has `debug = "line-tables-only"`, `strip = "none"`;
just remember to copy the `.pdb` alongside a uniquely-renamed `.exe`
*immediately* after each build — the shared worktree's
`target/release/cratonvm.pdb` gets overwritten by the next build, including
other concurrent sessions') and re-attaching cdb to a hung
`TestOrderInterceptor` repro gave the real answer:

```
14  Id: e390.e7c0 "Tribes-Task-Receiver-1"
 ...
 06  parking_lot::condvar::Condvar::wait_until_internal
 08  cratonvm_native_builtins::stamped_lock::rw_write_lock
 09  register_rwlock_natives::closure$5
```

Every one of the "expected"-but-never-arriving mutator threads was
genuinely parked inside **`native-builtins/src/stamped_lock.rs`'s
`rw_write_lock`/`rw_read_lock`** (backing `java.util.concurrent.locks.
ReentrantReadWriteLock`) — a hand-rolled `parking_lot::Mutex` +
`Condvar`-based reader/writer lock, contended because Tribes' internal
executor infrastructure uses one. The blocking `while ... { slot.cv.wait
(&mut state); }` loops in these functions had **zero
`begin_blocking_region`/`end_blocking_region` bracket** — called directly
from `native-builtins/src/lib.rs`'s `ReentrantReadWriteLock$ReadLock`/
`$WriteLock` native registrations with no GC-visibility at all. A thread
contending for this lock while another thread holds it stays counted in
the STW barrier's `expected` forever: it's not in JIT (can't be forcibly
taken over) and never reaches a Java-bytecode safepoint (can't cooperate)
while blocked in this Rust-level wait — exactly the `taken=0`,
never-decreasing-`pending` signature this doc describes.

### Full sweep — same pattern found in 4 more places

Grepped the whole workspace for the same raw-`Condvar::wait` shape (no
`ctx.begin_blocking_region()` anywhere nearby) and found:

1. `native-builtins/src/stamped_lock.rs` `rw_write_lock`/`rw_read_lock` —
   `ReentrantReadWriteLock`'s `WriteLock`/`ReadLock` `.lock()`/
   `.lockInterruptibly()` (4 call sites in `lib.rs`). **The one that
   actually caused `TestOrderInterceptor`'s hang.**
2. `native-builtins/src/stamped_lock.rs` `stamped_write_lock`/
   `stamped_read_lock` — the real `java.util.concurrent.locks.StampedLock`
   (not the `ReentrantReadWriteLock` above; a separate implementation) —
   same missing-bracket gap, 4 call sites (`readLock()`, `writeLock()`,
   and the `ReadLockView`/`WriteLockView.lock()` convenience wrappers).
3. `native-builtins/src/xnio_async.rs` `native_iof_await`/
   `native_iof_await_timed`/`native_iof_get` — XNIO's `IoFuture.await()`/
   `.get()`, used heavily by **Undertow (WildFly's web server)** for async
   I/O completion. A thread waiting on someone else's I/O-completion
   notification stays counted in `expected` forever if that notifier is
   itself stalled behind a GC pause. **This is very plausibly also
   implicated in `docs/known-issues/wildfly-standalone-boot-stw-jit-takeover-hang.md`**
   (`pending=6` during WildFly's `parallel-extension-add`, spinning up
   30-40 threads) — worth re-testing that repro against this fix before
   doing any further WildFly-specific investigation.
4. `native-builtins/src/concurrent_extras.rs` `SynchronousQueue.put`/
   `.take`/`.poll(timeout)` — bounded waits (`SQ_BLOCK_CAP` = 2s max), so
   lower severity (can't cause an indefinite hang on their own), but the
   same architectural gap — fixed for consistency and to stop needlessly
   delaying any GC pause requested during that window.

All fixed by bracketing the blocking call site in
`ctx.begin_blocking_region()`/`ctx.end_blocking_region()` (or
`end_blocking_region_refs` where a Java heap `ObjectRef` — e.g.
`SynchronousQueue`'s `this`, used again after the wait via
`consume_item` — needs re-syncing against a moving GC that completed
mid-block). None of the `stamped_lock.rs`/`xnio_async.rs` call sites touch
Java heap refs inside the wait itself, so those needed only a plain
begin/end pair.

**Ruled out as NOT part of this bug class** (checked, correctly designed
already): `native-io/src/async_socket.rs`'s `wait_for_pending` (explicitly
runs on the dedicated AIO dispatcher thread, which carries no
`NativeContext` and isn't a counted STW mutator at all) and its internal
`crossbeam_compat::Receiver::recv` (only ever called from the raw
`cratonvm-aio-N` worker-pool threads, same non-mutator category).

### Validation

`TestOrderInterceptor` — the clearest, fastest repro (~20-30s to hang
before the fix) — now **PASSES** (`OK (2 tests)`, ~20s wall time) on a
clean rebuild with all five fixes. Verified this is not a fluke: reproduced
the *unfixed* hang 6/6 times in a row before applying the `stamped_lock.rs`
fix (using `CRATONVM_DBG_STW_CENSUS=1`/a new `CRATONVM_DBG_STW_EXPECTED_IDS=1`
diagnostic added to `interpreter.rs`/`vm_exec.rs` this session — kept in
the tree, env-var-gated, zero cost when unset), then confirmed the pass
after.

Regression-checked twice (once after the interim fix, once after the real
fix) against a 60-class sample (`-Category all -Start 1 -Count 60`) with
**no regressions** in either pass — identical result both times: 26 PASS,
1 pre-existing unrelated `TestBeanSupport` FAIL, 33 HANG in the
already-documented, unrelated `TestHttpServletDoHead*` cluster (see
`reference_tomcat_dohead_gc_safepoint_deadlock` and siblings).

## Residual: 4 classes still hang, but NOT an STW-takeover bug

**Update 2026-07-13 (later the same day):** followed this residual up in
[elinjsp-socket-read-timeout.md](elinjsp-socket-read-timeout.md) — found and
fixed a real O(n)-per-byte performance bug in `DataInputStream`/
`RandomAccessFile`'s bulk-read natives (byte-by-byte via a full
`invoke_virtual` dispatch instead of one bulk `read()` call — thousands of
interpreter round-trips for a multi-KB class-file/JAR-entry read). Confirmed
via symbolicated `cdb` that this was actively being hit, and ruled out both
"just needs a longer timeout" (still didn't finish at 300s) and
"JIT-specific" (reproduces with `CRATONVM_DISABLE_JIT=1`) along the way.
**Fixed, but does not fully resolve this residual** — all 4 classes still
hang at the suite's 90s timeout after the fix, with zero regressions
elsewhere. See that doc's "Root cause #2" section for the full evidence
chain and next-step recommendation (get a multi-snapshot, all-threads
Java-level capture on an idle host to distinguish one genuine stuck point
from several different slow operations chained together).

Re-ran all 5 original classes against the fully-fixed binary:

| Class | Result |
|---|---|
| `TestOrderInterceptor` | **PASS** (was HANG) |
| `TestJspConfig` | HANG (unchanged) |
| `TestELInterpreterTagSetters` | HANG (unchanged) |
| `TestEnvEntry` | HANG (unchanged) |
| `TestWsWebSocketContainerTimeoutClient` | HANG (unchanged) |

Deep-dived `TestJspConfig` (hangs on its very first test method,
`testErrorOnELNotFound01`) the same way: `CRATONVM_DBG_STW_CENSUS=1` +
symbolicated `cdb`. **No `[stw-request]`/`[stw-census]` line is ever
printed** — this hang does not go through `stw_take_over_and_wait` at all,
confirming it is a structurally different bug. The `main-vm` thread
(the JUnit runner — the only thread doing real work; every `http-nio-*`
worker sits idle in a correctly-bracketed `LockSupport.park`) is stuck in:

```
main-vm:
 recv (ws2_32) <- TcpStream::read <- http_url_connection::read_eof_tolerant
 <- read_response <- perform <- huc_real_perform <- HttpURLConnection.getResponseCode()
```

I.e. `TomcatBaseTest.getUrl()`'s client-side HTTP GET is blocked reading a
response that the embedded Tomcat server **never sends** for this specific
request. Since Java's `HttpURLConnection` default read timeout is `0`
(infinite) unless a test explicitly sets one, and this code path is now
correctly GC-blocking-region-bracketed (fixed earlier this session), the
client-side wait itself is *architecturally* correct (matches real JDK
"no timeout configured" semantics) — the actual bug is server-side: **why
does Tomcat/Jasper never produce a response** for
`testErrorOnELNotFound01` (an EL-not-found error-page scenario) under
CratonVM? That's a Jasper/EL error-handling or request-processing question,
unrelated to GC/STW/threading. Spot-checked `TestEnvEntry` too (also hangs
on its first test method with no `[stw-request]` line ever printed) —
same shape, though I didn't get a clean symbolicated stack for it before
running out of session time; strongly suspect the same "server never
responds" pattern given the identical harness (`TomcatBaseTest.getUrl()`)
and lack of any STW signature.

This is very likely the same underlying issue (or a more severe,
non-timing-out variant of it) as the already-open
`docs/known-issues/tomcat-08-07/elinjsp-socket-read-timeout.md`
(`TestELInJsp` — 4/25 failures with client-side `SocketTimeoutException`,
also EL/JSP, also via `getUrl()`) — that doc's failures eventually time out
client-side (meaning a read timeout WAS configured for those specific
calls) where this cluster's hangs never do (no timeout configured), but
both point at the same underlying "Jasper/EL request sometimes never gets
a server response" defect. **Recommend merging these two docs** and
investigating from the server side: capture what the embedded Tomcat's own
worker thread is doing (or not doing) for the specific hung request — e.g.
`CRATONVM_DBG_SOCK=1` / a `cdb` attach that also inspects the (currently
believed idle) `http-nio-*-exec-N` threads' Java-level state via the
`debug_thread_census` frame trace, or add server-side request-lifecycle
logging to Tomcat's `Http11Processor`/Jasper's EL-error servlet path to see
whether the request is ever dispatched at all.

## Reproduction

```powershell
cd apps\tomcat-suite-runner
# Fixed (now passes):
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName verify -Start 277 -Count 1 -TimeoutSec 90 -Parallel 1
# org.apache.catalina.tribes.group.interceptors.TestOrderInterceptor

# Still hang (residual, different bug — needs -Dtomcat.test.basedir etc.,
# see cdb-hang.ps1 for the full arg list if reproducing outside the suite runner):
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName verify -Start 450 -Count 1 -TimeoutSec 90 -Parallel 1
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName verify -Start 466 -Count 1 -TimeoutSec 90 -Parallel 1
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName verify -Start 497 -Count 1 -TimeoutSec 90 -Parallel 1
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName verify -Start 649 -Count 1 -TimeoutSec 90 -Parallel 1
# org.apache.jasper.compiler.TestJspConfig
# org.apache.jasper.optimizations.TestELInterpreterTagSetters
# org.apache.naming.TestEnvEntry
# org.apache.tomcat.websocket.TestWsWebSocketContainerTimeoutClient
```

New diagnostics added this session (kept, env-var-gated, zero cost when
unset): `CRATONVM_DBG_STW_EXPECTED_IDS=1` (alongside
`CRATONVM_DBG_STW_CENSUS=1`) prints the exact `ThreadId` set counted as
"expected" at STW-request time (`[stw-expected]`), and every `park()` call
now logs `[stw-park] tid=<N> pre_stw=<bool>` — together these make it much
faster to tell whether a given hang is even going through the STW-takeover
mechanism at all (as the residual 4-class hang above demonstrates it is
not).

## Original summary (2026-07-12 finding, superseded by "Root cause" above)

Five unrelated-on-the-surface Tomcat test classes all HANG at the full
1200s timeout with the identical diagnostic warning repeating in the log:

```
WARN cratonvm_vm::runtime::interpreter: STW cross-thread JIT takeover is
  still waiting for cooperative mutators rounds=64 pending=N taken=0
```

Found via a fresh Windows full-suite rerun (dev commit `080e79256`,
2026-07-12, real JDK, JIT on, 1200s timeout) — a busier, more concurrent
run than the isolated single-class repro used above, which is presumably
why all 5 showed the STW signature there (concurrent GC pauses from other
classes' threads overlapping with these classes' now-separately-diagnosed
issues) even though only 1 of the 5 turns out to be an actual STW-takeover
bug in isolation. Verified via a fresh same-session HotSpot run (dev commit
unchanged, 2026-07-13): all 5 PASS cleanly on HotSpot.
