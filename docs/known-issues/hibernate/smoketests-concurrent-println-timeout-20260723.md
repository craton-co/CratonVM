# Hibernate SmokeTests concurrent `println` timeout

**Status:** OPEN (2026-07-23)

`org.hibernate.orm.test.sql.exec.SmokeTests#testQueryConcurrency` executes
20,000 HQL queries through five workers. On the real-JDK JIT runner it times
out at Hibernate's 120-second method limit while the remaining 16 tests pass.

The standard test resources enable JDBC bind/extract TRACE logging, but this
is only a contributing cost: suppressing both Hibernate SQL output and the
JDBC TRACE logger still times out. The actual workload remains an
interpreter-throughput problem in the concurrent H2/Hibernate query path.

The package-wide Hibernate JIT quarantine is justified by a deterministic
miscompile in `TransactionUtil.wrapInTransaction(SharedSessionContract,Object,
Consumer)`: after its interface callback returns, pc 30 resumes with an
underflowed operand stack and leaks all five H2 connections. A hard per-method
JIT exclusion removes that corruption. With the rest of Hibernate admitted
for diagnosis, SmokeTests improves from 164.7s to 135.6s, but still misses the
120-second test limit. H2, Log4j/JBoss logging, ANTLR, Java collections,
synthetic AQS, earlier C2, virtual-call IR, and main-method inlining either do
not help or regress. The remaining top interpreter methods are the Log4j
enablement chain and H2 accessors.

The current HotSpot baseline passes all 17 methods in 9281 ms while CratonVM
reliably reaches 16/17 and times out only in `testQueryConcurrency`. Pending
closure requires a bytecode-equivalent acceleration of the remaining logging
enablement/H2 hot path, then release-binary validation of `InPredicateTest`,
`SmokeTests`, and `LockTest` in JIT and `--nojit` modes.

Further targeted experiments retained the same result and were reverted:

- preserving real `ReadLock`/`WriteLock` view layout while routing the public
  operations to the native RWL backend: 16/17, 170.2 s;
- replacing the global CAS-lock registry lookup with 256 fixed lock stripes:
  16/17, 165.7 s;
- enabling Hibernate-only JIT admission at threshold 100: 16/17, 152.8 s;
- replacing the JIT-excluded `TransactionUtil.wrapInTransaction` Consumer
  wrapper with an exact pinned native begin/callback/commit-or-rollback path:
  correct lifecycle but 16/17, 174.4 s.

The allocator diagnostic produced no TLAB refill-failure samples during the
workload, so guarded TLAB refill is not the responsible throughput cliff.

An independent JIT correctness defect was fixed during this investigation:
the precise deopt snapshot emitted after an invoke retained the invoke BCI
even though code generation had already consumed the receiver and arguments.
Resuming that frame re-executed the invoke with an empty operand stack,
causing `TransactionUtil.wrapInTransaction(...Consumer)` to panic at PC 30
and leak pool connections. Resuming at the invoke successor eliminates the
panic and the pool leak. With the wrapper admitted for this diagnostic,
`SmokeTests` completed 16/17 in 144.3 seconds; the remaining issue is solely
the unchanged `testQueryConcurrency` 120-second throughput limit.

Latest local evidence (2026-07-27): the runner still reports `16/17` with
`testQueryConcurrency` timing out after 120 seconds (`@@RESULT ... ms=145915`).
`CRATONVM_DBG_GCPAUSE` emitted no collection pause of 100 ms or greater, so a
stop-the-world GC pause is not responsible for the timeout. An inline-cache
repair reduced forced-native virtual cache misses from roughly 20.2 million to
10.6 million, but did not materially lower the end-to-end time; a guarded
ASCII `Character.isJavaIdentifierPart` experiment regressed and was removed.

After merging current `origin/dev` (2026-07-27), a fresh task-specific
full-LTO release binary reproduced the same residual in both modes:
`--nojit` reported `found=17 started=17 ok=16 failed=1` in 164391 ms and
normal JIT reported the same count in 156152 ms. In both runs the sole failure
was the JUnit 120-second timeout in `testQueryConcurrency`. A diagnostic
H2-only JIT lift did compile `ParserBase.readIf(String)`,
`ExpressionColumn.isEverything`, and `JdbcStatement.checkClosed`, but did not
reduce the end-to-end timeout; it is therefore not a candidate delivery fix.

The later runtime-architecture merge (`origin/dev` at `ed93c79a8`) was also
built in the same isolated release target and re-run uncontended. Its
`--nojit` result remained `found=17 started=17 ok=16 failed=1`, with the same
sole JUnit timeout in `testQueryConcurrency` (`ms=172665`). This establishes
that the architecture merge is compatible with the class, but does not close
the throughput residual.

On that merged runtime, the diagnostic opt-out `CRATONVM_ROOTSNAP_CACHE=0`
reduced the runner time to `ms=162190`, but still produced the same `16/17`
timeout. The cache therefore contributes to the regression but cannot be
disabled as a closure for this test.

The real-JDK `ParserBase.testToken(String, Token)` native was then rewritten
to take its common IdentifierToken path directly through the token fields and
the VM's exact native String equality routines. The release `--nojit` run
remained functionally correct and improved to `ms=157048`, but still timed out
in the same sole method. It is retained as a measured improvement, not a
closure; the remaining gap is too large to be explained by parser-token
dispatch alone.

Removing the old rate-limited `Unsafe.compareAndSetLong` failed-CAS diagnostic
was also rejected: it changed the contention behavior and regressed the same
run to `ms=195348`. The probe was restored; it is not a simple logging cost to
remove. The next root-cause target is the monitor-backed linearizable CAS
implementation used by the real AQS/RRWL state path.

Additional rejected diagnostics/experiments (2026-07-27):

- letting the real `Trace.isDebugEnabled()` bytecode replace its registered
  native bridge produced the same sole `testQueryConcurrency` timeout
  (`found=17 started=17 ok=16 failed=1`, `ms=176342`);
- an exact allocation-free native mirror of `QueryParameterNamedImpl.hashCode()`
  likewise retained the sole timeout (`found=17 started=17 ok=16 failed=1`,
  `ms=273632` on a contended host), so it was removed rather than delivered as
  an unproven micro-optimization;
- root-snapshot telemetry did not reach its first 200,000-call report during
  the timed workload, and GC-pause telemetry produced no pause at or above
  100 ms. Neither collector-boundary path explains the missing throughput.

The remaining actionable design is to avoid the repeated sharded map lookup
and `Arc` churn in `MonitorTable::with_cas_lock` for the same AQS state object,
without weakening per-object linearizability or retaining stale locks across a
moving collection. A candidate must invalidate any thread-local lock handle
on the heap collection epoch and preserve the existing SATB pre-barrier and
post-write barrier ordering before it can be measured.

That epoch-invalidated per-thread handle cache was implemented and measured:
it preserved the existing mutex linearization point and completed normally,
but the runner again reported only `ok=16 failed=1` with the sole
`testQueryConcurrency` timeout (`ms=235932` on the shared host). It was
therefore removed. The cache does not remove enough of the actual AQS/RRWL
cost to be a delivery fix; further work must target the primitive state access
itself while preserving moving-GC and SATB semantics.

The follow-up cache keeps the same registry-owned mutex but caches only its
raw pointer for the current OS thread and a monitor-table epoch. The epoch is
bumped under stop-the-world before a GC re-key or exact-dead prune can remove
the owning `Arc`, so the fast path cannot dereference a stale lock. On the
current shared-host reproduction it improved the no-JIT runner from
`ms=275110` to `ms=226606`, but both runs still reported `ok=16 failed=1`
with the same 120-second `testQueryConcurrency` timeout. It is therefore a
measured partial improvement, not a closure. The remaining hot path includes
the concrete JBoss/Log4j2 native log emission performed for every explicitly
enabled Hibernate JDBC TRACE event; any further change must preserve the
actual backend's enabled state and Java-level output-capture semantics.

The framework-log `printed_lines` mirror was temporarily removed as a
behavior-preserving allocation hypothesis. A fresh full-LTO `--nojit` runner
still reported only `found=17 started=17 ok=16 failed=1` (`ms=216360`) with
the same 120-second `testQueryConcurrency` timeout. Since the canonical
direct-fd logging path relies on that mirror for internal output capture, the
change was reverted rather than weakening the logging contract for an
insufficient gain. The remaining target is the primitive implementation of
the repeated RRWL/AQS state CAS itself, not framework-log retention.

The RRWL/AQS CAS path was then changed to hold the collector's volatile-slot
stripe once across its mutex-linearized read/compare/write, replacing two
separate volatile accesses and four fences with one stripe acquisition and
one full-fence pair. It preserved functional behavior and improved the same
no-JIT class to `ms=158369`, but still timed out at 16/17. The class emitted
roughly 136,000 TRACE lines during that run. The canonical logging path had
already built the full line buffer but discarded it, then took the fd/stdio
locks twice to write text and its separator. The next candidate writes that
same complete buffer once; it preserves bytes, ordering, and line atomicity
while removing the redundant per-line lock/write operation.

That one-write logging variant also failed to close the class: the fresh
no-JIT runner again reported `found=17 started=17 ok=16 failed=1`, with the
same `testQueryConcurrency` timeout (`ms=190802` on the shared host). It was
reverted rather than retained as an unproven I/O micro-optimization. The
single-stripe CAS improvement remains the only retained throughput change;
the next investigation must reduce RRWL/AQS contention itself rather than
individual output writes.

The follow-up that removed the monitor-table CAS mutex for non-array slots
and used only the volatile stripe as the CAS linearization point was also
rejected.  Its fresh full-LTO `--nojit` run reported the same sole
`testQueryConcurrency` timeout (`found=17 started=17 ok=16 failed=1`,
`ms=211476`).  The previous CAS-mutex plus single-stripe implementation was
restored: the direct-stripe variant neither established enough throughput nor
provided a compelling reason to weaken the existing per-object CAS contract.
