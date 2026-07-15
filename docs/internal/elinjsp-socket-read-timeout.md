# TestELInJsp — 4/25 failures with client-side SocketTimeoutException

**Status:** PARTIALLY FIXED (2026-07-13) — the STW cross-thread JIT takeover
mechanism that caused `testBug45427` to fail is root-caused and fixed;
`testBug45427` now PASSES reliably. A separate, still-open residual remains
(see below), shared with
[stw-crossthread-jit-takeover-hang-cluster.md](stw-crossthread-jit-takeover-hang-cluster.md).
## Closure update (2026-07-14)

**Status: RESOLVED and retired.** A clean helper-only Tomcat classpath
eliminated the false scan-storm reproduction: `TestELInJsp#testBug61854a`
passes in 76.9s. The remaining genuine JSP compilation failure
(`testBug49555`) was traced to `Class.getCanonicalName()` incorrectly
rewriting the literal `$` in `TesterFunctions$Inner$Class`; the corrected
`InnerClasses`-based canonical and simple-name derivation now matches
HotSpot and the test passes in 76.9s. The linked WebSocket Future defect is
also fixed and its full class passes (`OK (2 tests)`, 16.3s).

**Severity:** closed. **HotSpot:** PASS (fresh-verified).

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

**Update 2026-07-13 (later the same day) — real O(n) performance bug found
and fixed in `DataInputStream`/`RandomAccessFile`, but does NOT fully
resolve the hang.** Followed up on the `dis_read_one` clue above (found by
an earlier pass this session) with a matching-symbol `cdb` build and
`--stack-dump-on-timeout`/`CRATONVM_ENABLE_NATIVE_RING` diagnostics. Found:

- `native-io/src/lib.rs`'s `native_dis_read_bytes` (backs
  `DataInputStream.read(byte[],int,int)`) and `dis_read_fully_impl` (backs
  both `readFully` overloads) both looped `dis_read_one` — a full
  array-alloc + `invoke_virtual` dispatch into the wrapped stream's
  `read()` — **once per byte requested**, instead of issuing one bulk
  `read(buf, off, len)` call (`dis_read_fully_impl`'s own doc comment
  claimed it "tries bulk read on inner stream first, falls back to
  byte-by-byte" — that bulk path did not actually exist in the code; the
  comment was aspirational/stale). For a multi-KB/MB read — exactly what
  reading a compiled JSP class file or JAR entry involves — this is
  thousands of interpreter round-trips where real Java does 1-2 native
  calls. `native_dis_skip_bytes` (`skipBytes`) had the identical pattern.
  native-io/src/lib.rs's `native_raf_read_fully` (`RandomAccessFile.
  readFully`) had a cheaper but analogous per-byte `fd_table().read_byte()`
  loop instead of using the already-existing bulk `fd_table().read_bytes()`.
- **All four fixed**: each now does genuine bulk reads (looping only on
  actual short-reads/EOF, matching real `InputStream.read()`/`readFully()`
  contract semantics exactly, just without the per-byte multiplier).
  `dis_read_one` itself is unchanged and remains correct for its real
  single/double-byte callers (`readByte`, `readUnsignedShort`, etc.) where
  the per-call overhead is negligible.
- **Confirmed via symbolicated `cdb`** that this was a genuine, actively-hit
  bottleneck: multiple independent repro attempts caught `main-vm` (or an
  `http-nio-*-exec-N` worker) live inside `dis_read_one` /
  `native_dis_skip_bytes` / `ExpressionFactory.newInstance` (which itself
  transitively reads class files via `DataInputStream` during classloading)
  at the moment of the snapshot — not idle, not lock-blocked, genuinely
  burning CPU in the anti-pattern.
- **Ruled out "just slow"**: before finding this, ran `TestJspConfig` with
  a 300s timeout (6x the normal 90s the suite runner uses) — it still did
  not complete, ruling out "eventually finishes, just needs a longer
  timeout" as the explanation on its own.
- **Ruled out JIT-specific**: reproduces identically with
  `CRATONVM_DISABLE_JIT=1`.
- **Result after the fix: `TestJspConfig` (and the hang-cluster doc's other
  3 residual classes) STILL HANG** at the suite runner's 90s timeout,
  regression-checked with zero new regressions (60-class sample, same
  26 PASS/1 FAIL/33 DoHead-cluster-HANG baseline as before, plus
  `TestOrderInterceptor` still PASS). So this was a real bug — worth fixing
  on its own merit, and it may well have shortened these hangs
  significantly — but it's **not the sole or complete explanation**. Across
  different repro attempts this session, `main-vm`/worker threads were
  caught stuck in genuinely different-looking places (client `recv()` in
  `HttpURLConnection`; `ExpressionFactory.newInstance`; the now-fixed
  `DataInputStream` byte loop; and, in a couple of runs, actively executing
  deep, seemingly-recursive JIT/interpreter frames with no obvious blocking
  call at all) — consistent with either (a) more than one distinct slow/
  looping code path compounding, or (b) one deeper defect (e.g. a genuine
  infinite or unbounded-retry loop somewhere in classloading/EL setup) that
  this session did not fully localize.

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
2. The `DataInputStream`/`RandomAccessFile` O(n)-per-byte bug is now fixed,
   but did not fully resolve the hang — don't re-chase that specific lead
   again without new evidence. Next: get a clean, symbolicated,
   `--stack-dump-on-timeout`-based capture of ALL threads' Java-level frame
   traces (not just `main-vm`) on an idle host, and specifically watch for
   whether the SAME code path (class name + method) keeps recurring across
   several independent hang snapshots taken a few seconds apart on the
   SAME run — that distinguishes "stuck in one place" (genuine infinite
   loop/deadlock) from "just very slow, progressing through many different
   places" (a chain of several slow-but-finite operations, in which case
   look for more instances of this session's byte-by-byte anti-pattern —
   grep for loops calling a single-unit native helper where a bulk
   equivalent exists, the same signature that found the 3 fixes above).
3. Re-run on an idle, unloaded host (no concurrent builds, miner
   remediated) before drawing further conclusions — this and the prior
   session's results were gathered under confirmed, severe host contention
   (cryptominer infection + 6-25 concurrent `cargo`/`rustc` processes from
   other sessions throughout) that inflates any absolute timing number and
   may itself explain part of why a 90s timeout isn't enough.
