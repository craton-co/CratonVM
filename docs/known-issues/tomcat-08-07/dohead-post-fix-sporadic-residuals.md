# DoHead family — post-fix sporadic residual singletons (catalogue)

**Status:** OPEN (low priority, low rate). **Severity:** low. **Context:**
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
