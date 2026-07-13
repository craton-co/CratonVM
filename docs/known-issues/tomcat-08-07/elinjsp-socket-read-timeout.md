# TestELInJsp — 4/25 failures with client-side SocketTimeoutException

**Status:** PARTIALLY FIXED (2026-07-13) — the STW cross-thread JIT takeover
mechanism that caused `testBug45427` to fail is root-caused and fixed;
`testBug45427` now PASSES reliably. A separate, still-open residual remains
(see below), shared with
[stw-crossthread-jit-takeover-hang-cluster.md](stw-crossthread-jit-takeover-hang-cluster.md).
**Severity:** medium. **HotSpot:** PASS (fresh-verified).

## Root cause #1 (FIXED) — STW takeover self-inflicted scan storm

`testBug45427` compiles a JSP with 16 EL expressions (Jasper's embedded JDT
compiler). While that compile/serve work is in flight, a GC pause gets
requested with exactly one pending mutator: the *client's own*
`HttpURLConnection` thread (in this single-process embedded-Tomcat test,
client and server share one JVM and one STW barrier). Root-caused via a
live `cdb` attach + `CRATONVM_DBG_STW_CENSUS=1`/`CRATONVM_DBG_XT_JIT_ROOT_SCAN=1`:
`vm/src/runtime/interpreter.rs`'s `stw_take_over_and_wait` loop was
re-running the expensive OS-level `take_over_pass` scan (Windows: a full
`CreateToolhelp32Snapshot` + per-peer `OpenThread`/`SuspendThread`/
`GetThreadContext`/`ResumeThread`) **unconditionally on every single 1ms
tick**, indefinitely, once the loop had spun even once — regardless of
whether anything was actually in JIT. Suspending/resuming every peer
thread ~1000×/sec (including the exact mutator being waited on) competes
with that peer for scheduler time: a self-amplifying livelock where the
longer the wait takes, the more it starves the very thread it needs to
arrive. Measured: a single pause with one pending mutator took ~5-6 real
minutes and logged hundreds of thousands of "0 newly taken over" scans,
long enough to blow past the client's own read timeout.

**Fix:** `stw_takeover_should_scan` (`vm/src/runtime/interpreter.rs`) —
scan every round for the first 20ms (unchanged latency for the common
case), then back off to every 20ms, then every 200ms for long stalls.
Unit-tested (`stw_takeover_scan_cadence_backs_off`). Merged to `dev`
(commit history: `395be5d21` on branch `fix/elinjsp-stw-takeover-20260713`,
folded into the same branch's later `945e44920`/merge commits, now on
`dev`).

**A second, deeper root cause was found the same day by continued work on
this branch** (see
[stw-crossthread-jit-takeover-hang-cluster.md](stw-crossthread-jit-takeover-hang-cluster.md)
for the full writeup): several native lock/wait implementations
(`ReentrantReadWriteLock`, `StampedLock`, XNIO's `IoFuture`,
`SynchronousQueue`) had raw `parking_lot::Condvar::wait` loops with **zero
`begin_blocking_region`/`end_blocking_region` bracket**, so a thread
contending one of these stayed counted in the STW barrier's `expected`
forever — a stronger, unconditional version of the same symptom the scan-
cadence fix mitigates. Both fixes are needed for the full cluster; only the
scan-cadence fix was required to resolve `testBug45427` specifically (its
one pending mutator was a normal Java thread that just needed to not be
starved, not one stuck on an unbracketed lock).

**Validated (this session, `C:\data\CratonVM-elinjsp-20260713`,
`cratonvm-elinjsp-fix1.exe`/`cratonvm-elinjsp-devmerged.exe`):** ran the
full `TestELInJsp` class 3 times back-to-back against the fixed binary.
`testBug45427` passed cleanly every time — the `[stw-request]`/
`[stw-arrive]` log lines show the same "one pending mutator" shape as
before the fix, but now resolving in well under a millisecond instead of
minutes (confirmed for both `testBug45427` itself and a later test hitting
the identical `expected=1` shape, `testBug56147`).

## Root cause #2 (STILL OPEN) — server never responds for specific EL/JSP requests

With root cause #1 fixed, `TestELInJsp` still shows 2 residual failures,
reproduced consistently across 2 back-to-back runs:

- `testBug49555` — **NOT the same bug.** Fails with
  `org.apache.jasper.JasperException: Unable to compile class for JSP`,
  a genuine JDT-compiler error compiling a JSP that references
  `TesterFunctions.Inner$Class` (a static nested class in an EL function
  mapping) — a JSP-compile correctness bug, unrelated to timeouts/threading.
  Not investigated further here; worth its own doc if it reproduces on a
  clean/idle host.
- `testBug61854a` — `java.net.SocketTimeoutException: Read timed out`,
  **same signature as this doc's original 4 failures.** No
  `[stw-request]` line shows a stuck pending mutator for this case
  (`expected=0` at the point of request), and — like the residual 4 classes
  in the hang-cluster doc — the actual mechanism looks like the embedded
  Tomcat server simply never sends a response for this specific request.
  `TestELInJsp`'s `getUrl()` sets an explicit
  `DEFAULT_CLIENT_TIMEOUT_MS = 300_000` (5 min), which is presumably why
  this manifests as a `SocketTimeoutException` here rather than the
  infinite hang seen in `TestJspConfig`/`TestEnvEntry` (which call
  `getUrl()` without an explicit timeout, so the JDK default of "wait
  forever" applies). **Strongly suspected to be the same underlying defect
  as "Residual: 4 classes still hang" in
  [stw-crossthread-jit-takeover-hang-cluster.md](stw-crossthread-jit-takeover-hang-cluster.md)** —
  that doc recommends merging the two docs and investigating server-side
  why Jasper/EL sometimes never produces a response.

**New clue found this session, NOT yet in the hang-cluster doc:**
re-reproduced `TestJspConfig` end-to-end against a fresh `dev`-tip build
(`cratonvm-elinjsp-devmerged.exe`) with a `cdb` attach after ~150s stuck on
`testServlet22NoEL` (a *different* test method than the hang-cluster doc's
`testErrorOnELNotFound01` — that one actually PASSED in this run, which
itself may indicate the underlying defect is intermittent/order-dependent,
or this box's severe concurrent load — see below — makes different runs
hit different methods). `main-vm`'s stack was NOT parked in
`HttpURLConnection`/socket code this time; it was live inside:
```
cratonvm_native_io::dis_read_one
 <- dis_read_exact
 <- native_dis_read_unsigned_short
```
i.e. `DataInputStream.readUnsignedShort()` (or `.readChar()`, same native),
which delegates via `ctx.invoke_virtual(inner, "read", "([BII)I", ...)` to
whatever stream `inner` actually is — not resolved further this session.
This is a genuinely different-looking stuck point than the previous
session's `HttpURLConnection` finding for `testErrorOnELNotFound01`,
consistent with "server never responds" being a JSP/EL-request-processing
defect with more than one manifestation rather than a single stuck call
site. Worth checking what `inner`'s concrete type is (a class-file reader?
a JAR entry stream? something in Jasper's own compiled-JSP-class loading
path?) as the next step.

**Confound to account for before trusting timing on this box:** this
session independently hit (a) a severe, confirmed-active cryptominer
infection (see memory `windows-box-cryptominer-infection-20260710`,
re-checked today: `RecoveryManager` scheduled task still `Running`), (b)
host-wide disk exhaustion (`C:` briefly down to 84MB free out of 813GB, a
few builds failed with genuine `IO failure on output stream: no space on
device`), and (c) 10-25+ concurrent `cargo`/`rustc` processes from other
sessions throughout. Any single hang/pass verdict on this box should be
treated skeptically until re-confirmed on a quieter host.

## Reproduction

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName elinjsp `
  -Start <idx> -Count 1 -TimeoutSec 120 -Parallel 1
# org.apache.el.TestELInJsp
```

## Recommendation

1. Merge this doc with
   [stw-crossthread-jit-takeover-hang-cluster.md](stw-crossthread-jit-takeover-hang-cluster.md)
   per that doc's own recommendation — both point at the same
   "Jasper/EL request sometimes never gets a server response" defect.
2. Investigate from the server side: what is the `http-nio-*-exec-N`
   worker thread actually doing (or not doing) for the specific hung/
   never-responding request? The client-side view (this doc + the
   hang-cluster doc) has now been dead-ended twice from two different
   angles (`HttpURLConnection` socket read; `DataInputStream` native read)
   without finding the server-side cause.
3. Re-run on an idle, unloaded host (no concurrent builds, miner
   remediated) before drawing further conclusions — this session's
   results, while internally consistent (3 clean confirmations of the
   root-cause-#1 fix), were gathered under severe host contention that
   makes any *new* timing-sensitive finding (like the `dis_read_one`
   clue above) less trustworthy than the code-level fixes already merged.
