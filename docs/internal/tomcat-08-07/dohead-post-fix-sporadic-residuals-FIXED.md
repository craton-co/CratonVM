# DoHead family — post-fix sporadic residuals (FIXED)

**Status: CLOSED 2026-07-17.** This record is retained as an internal history
because the former singleton failures were traced to aliasing of live NIO
objects in native tables keyed only by Java identity hash code. Java identity
hashes are stable across moving GC but are not unique; collisions made channel,
SelectionKey, and Selector state cross-wire under the repeated Tomcat
start/stop pressure used by this family.

The fix buckets each table by stable identity hash and disambiguates the row by
its ObjectRef. Those references are explicitly rooted and remapped after a
moving collection. This covers SocketChannel/ServerSocketChannel synthetic
state, SelectionKey state, and Selector-to-native-id state.

## 2026-07-17 closure evidence

- Commit: `fbd790c7 fix(nio): disambiguate identity hash side tables`.
- Remote probe binary: `/data/data/cvm-dohead-postfix-eintr9-20260717`.
- Exact closure matrix: `/data/data/dohead-postfix-eintr9-full-20260717`.
  Configuration: 64 classes, one pass, two processes, `-Xmx1g`, 900-second
  class timeout. `summary.txt` contains 64 `PASS` records, zero `FAIL`,
  `TIMEOUT`, or `CRASH` records, and ends with `ALL_DONE` at 16:58:17 UTC.
- The prior focused pressure reproducer
  `TestHttpServletDoHeadInvalidWrite511ValidWrite511` passed eight concurrent
  c9 runs (two independent lanes, four passes each), including the c7/c8
  header, HTTP/2 EOF, and selector-stall trigger.
- `cargo test -p cratonvm-native-io --lib -- --test-threads=1`: 349 passed,
  zero failed.

The sections below are the historical open checkpoint and observations that
led to this resolution.


## 2026-07-16 closure attempt checkpoint

**Status: OPEN.** This note must stay in `docs/known-issues`: the current
two-process / 1 GiB Windows stress oracle still produces sporadic HTTP/2
mid-frame EOFs. In the final observed eight-class batch,
`TestHttpServletDoHeadInvalidWrite0ValidWrite1` failed twice and
`TestHttpServletDoHeadInvalidWrite0ValidWrite511` failed once, each with
`End of input stream with [9] bytes left`, out of 288 parameterizations.
Each was in `testDoHeadHttp2`; the server log had no application exception.
Immediate isolated reruns of `0 -> 1`, `0 -> 511`, and `1 -> 1023` passed,
including a two-process rerun of `1 -> {1023,1024}`. This is a low-rate
runtime transport residual, not a completed closure.

### Changes in this checkpoint

1. Prevented a late synthetic `URI.toURL()` registration from replacing the
   real URL-aware implementation. That removed
   `NoSuchMethodError: java/lang/Object.toExternalForm()` and the associated
   dropped HTTP/2 responses. The exact `1023 -> 512` class passed 288/288 and
   the focused `511/512/513/1024` group passed 1,152/1,152.
2. Pinned `URLClassLoader` construction inputs through allocating
   initialization steps, including `ucp` creation, to address the observed
   `WebappLoader.buildClassPath` null-`ucp` path under moving GC.
3. Made in-flight selector close wakeups return normally instead of throwing
   `ClosedSelectorException` into Tomcat `Poller.destroy`. The focused native
   selector suite passed 24/24, and the full `1 -> *` pressure batch no longer
   showed the LifecycleException.
4. Rooted scalar and gathering `SocketChannel` Java buffers across native I/O
   and reload them before buffer updates. `cargo check -p cratonvm-native-io`
   passes. The gathering-write change has not yet been built into a fresh
   release binary or credited as a fix for the remaining EOF residual.

### Evidence and next gate

- `cargo test -p cratonvm-native-builtins --lib`: 2,998 local and 2,999
  isolated-Azure passes before this checkpoint's final socket changes.
- `cargo test -p cratonvm-native-io --lib selector -- --test-threads=1`:
  24 passed after the selector change.
- Four early two-process batches passed 9,216 parameterized cases. A later
  batch exposed the selector, header, and HTTP/2 residuals; the first two were
  removed by the changes above, while EOF remains sporadic.

Do not archive or move this document until a newly built binary containing the
gathering-write root fix completes the full 64-class two-process matrix without
transport, header, selector, loader, or native-stack residuals.

**Historical status:** OPEN (low priority, low rate). **Severity:** low. **Context:**
after the 2026-07-15 fixes (thread-identity aliasing + dying-thread
card-buffer loss, see
`docs/internal/tomcat-08-07/dohead-residual-http2-midrun-hang-FIXED.md`),
the 64-class family was swept plus ~10 further full-class runs — roughly
21,000 Tomcat start/stop cycles on a heavily loaded box (3 concurrent
suite runs, active cryptominer infection). The freed-while-live disease is
gone (corruption telemetry silent). What remains is a catalogue of
UNRELATED singletons, each appearing 1-2 times total, each with ZERO
`gen_heap` containment / stale-pointer telemetry in its run:

1. **WinSock 10053 connection abort mid-read** (2×) — the long-documented
   environmental host-abort flake family (present at the same rate in every
   historical sweep, incl. pre-regression baselines).
2. **`LifecycleException: Protocol handler stop failed` ←
   `IOException: ClosedSelectorException` at `NioEndpoint$Poller.destroy`**
   (2×: one at `-MaxHeap 2g`, one in the family sweep) — a teardown
   ordering race: the poller's selector is already closed when `destroy()`
   runs. Plausibly a CratonVM NIO selector close/wakeup ordering nit;
   worth a look if it climbs above singleton rate.
3. **Sporadic header-count `AssertionError`** (`expected:<4> but was:<3>`,
   `expected:<2> but was:<0>`; 2×, non-reproducing on immediate rerun) —
   response header set off-by-N under heavy load; distinct from the FIXED
   deterministic OSW eager-flush `<2> vs <3>` cluster (that one was
   152/288 deterministic; these are 1/288 singletons).
4. **`IOException: End of input stream with [9] bytes left`** (1×) —
   mid-read disconnect singleton, historical environmental family.
5. **`NPE: Cannot invoke URLClassPath.getURLs() because this.ucp is null`**
   at `WebappClassLoaderBase.getURLs` ← `WebappLoader.buildClassPath`
   during context start (1×) — a `URLClassLoader` observed before/without
   its `ucp` being initialized. Not the (fixed) null-parent HTTP loader
   issue. Candidate: constructor-bypass or field-init ordering on the
   `WebappClassLoader` subclass path.
6. **`EXCEPTION_STACK_OVERFLOW` on an http-exec thread** (1×,
   `TestHttpServletDoHeadInvalidWrite512ValidWrite513` param 3, faulting
   RVA `0x129C76D` on the session's pre-merge binary, symbol unresolved) —
   native-side stack exhaustion, no Java SOE trace printed. Not the fixed
   `ScheduledThreadPoolExecutor.shutdown` self-recursion (that fix was in
   the binary). Needs its own capture (`--stack-dump-on-timeout` won't
   help; a live cdb attach or a bigger `[rust] stack` diagnostic would).

## Reproduction

Standard family runs, e.g.:

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -RunName x -TimeoutSec 900 `
  -Parallel 2 -MaxHeap 1g -ClassFilter 'TestHttpServletDoHeadInvalidWrite'
```

Expect ≥95% of classes 288/288; the shapes above appear as isolated
287/288 singletons (or the rare crash face #6). None reproduced on a
targeted rerun of the affected class this session.
