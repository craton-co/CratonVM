# `org/h2/` JIT ban (HIB-LONGTAIL.1) — residuals 1–3 CLOSED, residual 4 re-measured, ban STAYS

**Status:** residuals 1, 2 and 3 are FIXED. Residual 4 is still open but is now
quantified instead of guessed, and the fix the previous revision proposed for it
is measured and shown to be far too small. The ban itself stays, on new evidence
that is different from — and stronger than — the evidence it used to rest on.

None of the three closed residuals was an H2 bug. Two of them were one general
VM defect; the third was two concurrency defects stacked.

Predecessors, both archived, neither needed to act on this:

* `h2-jitban-schema-not-found-on-reconnect-FIXED.md`
* `bug-h2-testfilesystem-testconcurrent-async-hang-FIXED.md`

## The headline: a JIT-compiled caller's `invokevirtual` never reached a JIT-compiled callee

`direct_virtual_compiled_callee_entry_enabled()` (`vm/src/jit/helpers.rs`) was
default-OFF, and it gates the **only** code that ever writes
`mic.cached_entry_ptr`. With it off, the inline MIC/PIC cascade the codegen
emits at every compiled `invokevirtual` can never open, so every virtual call
out of compiled code fell through `invoke_or_native` into the **interpreter**.

Compiling a method therefore made its callees slower, and compiling *more* of a
program made the program slower overall. That is the whole content of residuals
2 and 3, and it is why they appeared only when the `org/h2/` ban was lifted:
lifting the ban is what made the *callers* compiled.

Flipped to default-ON; `CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY=0`
opts out. Validated on the full 218-class H2 suite (below) and the Tomcat suite.

## Residual 1 — `TestStreamStore` `Interruptible.interrupt` NPE — FIXED

Not intermittent. 10/10 FAIL with the ban lifted, 0/10 with it in place, same
binary. (The previous revision said intermittent and asked for a failure rate
first. Asking for the rate was right; the doc's own answer was wrong.)

`CRATONVM_JIT_BISECT_ONLY` narrowed it to one method,
`org/h2/test/store/TestStreamStore$RandomStream.read` — a pure `byte[]`-filling
PRNG that cannot produce a `java.nio` NPE. That was the tell: JIT eligibility
was only moving timing around. Two real bugs were underneath.

**Bug A — `ThreadPoolExecutor.shutdown()` interrupted RUNNING workers.**
`CRATONVM_DBG_INTERRUPT` (added here) named the producer on the first run:

```
CRATONVM_DBG_INTERRUPT: target_obj=0x… target_tid=Some(3) by_tid=0
  INT-STK[10] org/h2/util/Utils.shutdownExecutor pc=8      <- executor.shutdown()
  INT-STK[9]  org/h2/mvstore/FileStore.shutdownExecutors
  …
  INT-STK[3]  org/h2/test/store/TestStreamStore.testSaveCount
```

`shutdown()` is an *orderly* shutdown: previously submitted tasks run to
completion, and only IDLE workers are interrupted — the JDK separates the two
with `w.tryLock()` in `interruptIdleWorkers`. CratonVM's bridge
(`interrupt_executor_workers`) used `shutdownNow()`'s "interrupt every worker"
for both. H2 closes its MVStore while the buffer-save worker is inside
`FileChannel.write`, so that worker took an interrupt mid-write.

Fixed with `interrupt_executor_workers_filtered(.., only_idle)`; the graceful
path passes `true`, `shutdownNow()` keeps interrupting everything. It fails
OPEN — if `tryLock` cannot be invoked at all the worker is interrupted anyway,
because an unwoken idle worker turns `awaitTermination(1, DAYS)` into a hang,
which is worse than an over-eager interrupt.
Witness: `regression-suite/src/RExecutorShutdown.java`.

**Bug B — `AbstractInterruptibleChannel.interruptor` was always null.**
Independent of A and broader: `interruptor` is a `final` field the JDK
constructor always assigns, and `begin()` dereferences it unconditionally once
`Thread.currentThread().isInterrupted()` is true. CratonVM builds
`FileChannelImpl` through a native bridge that never runs that constructor and
explicitly set the slot to null — so **any** channel operation on a thread whose
interrupt flag happened to be set died with an NPE instead of performing the
specified asynchronous close. Confirmed directly by reflection: `interruptor`
was `NULL` on CratonVM and `…$1` on HotSpot, on every channel, always.

Fixed by constructing the real `AbstractInterruptibleChannel$1` in the bridge.
Witness: `regression-suite/src/RChannelInterrupt.java`.

**Still open, deliberately:** `SocketChannel`/`ServerSocketChannel` are built the
same way and have the same null `interruptor`. Not fixed there — the shared
`init_channel_locks` carries an explicit, empirically-earned warning against
allocating on that path (an allocation there previously relocated the channel
under concurrent load and left `closeLock` null, killing the Apache
httpasyncclient reactor). Seeding `interruptor` needs an allocation, so it needs
that path reworked first.

## Residuals 2 and 3 — `TestFreeSpace`, `TestNestedJoins` "300s hang" — FIXED

Neither was a hang. The previous revision reasoned that "with `org/h2/`
JIT-eligible, H2 code is throughput-competitive, so a 300s HANG is more likely a
lost wakeup or a livelock than slowness". The premise was exactly backwards.

Instrumented `TestFreeSpace` (classpath overlay, per-phase timing) showed steady
progress that got monotonically slower: 0.79s per 2000 iterations at the start,
7.9s per 2000 by iteration 20000 — on *identical* work. The per-iteration digest
and every produced string matched HotSpot and the banned arm exactly, so nothing
was diverging.

`CRATONVM_JIT_DENY` narrowed it to `org/h2/mvstore/FreeSpaceBitSet`, per-call
timing inside the test to `toString()` and `allocate()` — the two methods that
loop over a `java.util.BitSet` — and instrumenting `FreeSpaceBitSet` itself gave
the decisive number: identical loop iteration counts (30453, 42584, …) in both
arms, 400ms of loop time with the ban and 5300ms without.

`CRATONVM_DBG_JIT_COMPILED` (added here) then showed `java/util/BitSet.
nextClearBit` was compiled in **both** arms. Compiled callee, compiled caller,
call still interpreted — the headline defect. Setting the flag took that loop
from 5582ms to **169ms**, and flat instead of growing.

With the flag on, both classes pass with the ban lifted: `TestFreeSpace` 86s and
`TestNestedJoins` 54s against a 300s cap — both faster than their own
ban-in-place times (105s / 157s).

## Residual 4 — `TestFileSystem.testConcurrent` on `nioMemLZF:1:` — STILL OPEN, re-measured

The class is not uniformly slow. Instrumented per prefix, every other filesystem
clears in 1–7s (≈13s total); `nioMemLZF:1:` `testConcurrent` alone does not
finish inside a 25-minute cap. Instrumenting the operation loop gives the rate:

| | per 100 operations |
|---|---|
| HotSpot | 7–13 ms |
| CratonVM (ban in place) | ~86,500 ms |

**~8,600x, and steady** (86.5s, 86.5s, 82.8s, 87.2s per 100 ops) — a constant
per-operation cost, not a leak, livelock or degradation. 10,000 operations at
that rate is ~2.4 hours.

**The previous revision's "one concrete lead" is measured and is not enough.**
It proposed threading `ResolvedMethod::native_target`/`native_kind` through
`invoke_or_native` so the native registry is not probed per call, citing a
profile that attributed 12.2% to `NativeMethodRegistry::slot_for_exact`. That
attribution still holds — `slot_for_exact` remains the single largest symbol at
7.6–12.4% depending on configuration — but a 12% saving against an 8,600x gap
is not a fix, and the profile behind it is otherwise flat: a long interpreter
tail with no second hot spot to attack. This is a throughput project on
interpreted LZF + `ByteBuffer` inside a backoff-free spin lock, not a residual.

Lifting the `org/h2/` ban does not help either (it stalls at the same prefix),
and it makes the sibling `memLZF:` prefix fail outright — see below.

## Where the ban stands — it STAYS

Same-binary 218-class A/B, with the dispatch fix in place throughout:

| arm | PASS | FAIL | HANG | CRASH |
|---|---|---|---|---|
| dispatch flag OFF, ban in place (previous dev behaviour) | 162 | 24 | 32 | 0 |
| dispatch flag ON, ban in place | **166** | 21 | 31 | 0 |
| dispatch flag ON, ban lifted | 155 | 28 | 32 | 3 |

The flip is worth +4 net and has **no** regressions: its only two apparent ones
were re-run 3x each in isolation and both are artifacts of running three suites
concurrently (`TestMvccMultiThreaded` passes 3/3 in *both* configurations;
`TestOpenClose` hangs 3/3 in *both*).

Lifting the ban still costs 11 net PASS and introduces three CRASHes
(`TestRunscript`, `TestPageStoreCoverage`, `TestReopen`). Note that
`TestReopen` was one of the six classes the previous revision recorded as
*closed* — it regressed again once the dispatch fix let compiled H2 code
actually run compiled, which is a good reason to distrust any per-class verdict
taken before that fix.

The most concrete new blocker is a **correctness** failure, not a throughput
one: with the ban lifted, `TestFileSystem`'s `memLZF:` `testConcurrent` fails
intermittently (2 of 4 runs) with

```
java.lang.AssertionError: Expected: 3900 actual: 3897
java.lang.AssertionError: Expected: 5128 actual: 5168
```

The reader holds the same `AtomicIntegerArray` spin lock the writer held, and
reads `expected.get(pos)` and then the file contents; seeing fresh file contents
with a stale `expected` is a memory-ordering violation, since the writer wrote
the file, then `expected`, then released the lock. That points at compiled
`org/h2` code reordering across `AtomicIntegerArray.set`/`compareAndSet`, and it
is the thing to root-cause before lifting this ban is worth attempting again.

## The flip's own fallout: two Eclipse JDT bans had to be RESTORED

Turning the dispatch flag on regressed one Tomcat class:
`jakarta.el.TestOptionalELResolverInJsp` went PASS -> FAIL, reproducibly (3/3
with the flag on, 3/3 PASS with it off, same binary, run in isolation). Its JSP
compile dies inside the Eclipse JDT compiler with

```
ClassCastException: org.eclipse.jdt.internal.compiler.ast.QualifiedTypeReference
  cannot be cast to org.eclipse.jdt.internal.compiler.ast.FieldDeclaration
  -> JasperException: Unable to compile class for JSP  -> HTTP 500
```

`CRATONVM_JIT_DENY` bisection puts it in
`org/eclipse/jdt/internal/compiler/parser/` — denying that one package restores
PASS, while denying `ast/`, `lookup/` or `util/` does not.

That package is **JASPER-JDT.2**, and it was REMOVED on 2026-07-26 as "no longer
reproduces on current dev", along with its sibling JASPER-JDT.3 (`ast/`). Both
removals were careful — four repeat runs each, real Tomcat fixtures — and both
are void, because every one of those runs was made while the virtual
direct-entry path was default-OFF. With that flag off a compiled caller never
reaches a compiled callee at all, so the compiled-to-compiled dispatch these
bans guard was *inert during the verification*: those runs could not have
reproduced the defect whatever its state. Same shadowing shape this module
already annotates for other removed bans, just hidden behind a flag instead of
behind another rule.

Both are restored. `parser/` is directly re-confirmed by the bisection above;
`ast/` is restored on the shadowing argument alone — its own repro
(`TestFormAuthenticatorA`) has not been re-run under the flag, and leaving it
out would assert something no measurement supports. With both back,
`TestOptionalELResolverInJsp` is 3/3 PASS with the flag on.

**The rule this leaves behind:** any ban whose mechanism is compiled-to-compiled
virtual dispatch must be re-verified with
`CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY` **on**, or it verifies
nothing. That flag was default-OFF for the entire period in which the
2026-07-25/26 ban sweep did its removals.

## The flip's own fallout: two Eclipse JDT bans had to be RESTORED

Turning the dispatch flag on regressed one Tomcat class:
`jakarta.el.TestOptionalELResolverInJsp` went PASS -> FAIL, reproducibly (3/3
with the flag on, 3/3 PASS with it off, same binary, run in isolation). Its JSP
compile dies inside the Eclipse JDT compiler with

```
ClassCastException: org.eclipse.jdt.internal.compiler.ast.QualifiedTypeReference
  cannot be cast to org.eclipse.jdt.internal.compiler.ast.FieldDeclaration
  -> JasperException: Unable to compile class for JSP  -> HTTP 500
```

`CRATONVM_JIT_DENY` bisection puts it in
`org/eclipse/jdt/internal/compiler/parser/` — denying that one package restores
PASS, while denying `ast/`, `lookup/` or `util/` does not.

That package is **JASPER-JDT.2**, and it was REMOVED on 2026-07-26 as "no longer
reproduces on current dev", along with its sibling JASPER-JDT.3 (`ast/`). Both
removals were careful — four repeat runs each, real Tomcat fixtures — and both
are void, because every one of those runs was made while the virtual
direct-entry path was default-OFF. With that flag off a compiled caller never
reaches a compiled callee at all, so the compiled-to-compiled dispatch these
bans guard was *inert during the verification*: those runs could not have
reproduced the defect whatever its state. Same shadowing shape this module
already annotates for other removed bans, just hidden behind a flag instead of
behind another rule.

Both are restored. `parser/` is directly re-confirmed by the bisection above;
`ast/` is restored on the shadowing argument alone — its own repro
(`TestFormAuthenticatorA`) has not been re-run under the flag, and leaving it
out would assert something no measurement supports. With both back,
`TestOptionalELResolverInJsp` is 3/3 PASS with the flag on.

**The rule this leaves behind:** any ban whose mechanism is compiled-to-compiled
virtual dispatch must be re-verified with
`CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY` **on**, or it verifies
nothing. That flag was default-OFF for the entire period in which the
2026-07-25/26 ban sweep did its removals.

## Also fixed — `ClassCastException` named an array receiver by its component

`java.lang.String cannot be cast to java.lang.String` for a `String[]` receiver.
The header word of a reference array carries the *component* class id, so the
`class_id_of` → `class.name` lookup that both the interpreter's `checkcast` and
the JIT's `jit_checkcast` used reported the component. HotSpot prints the
descriptor. Fixed in both; nested and primitive arrays covered; asserted on the
interpreted and the compiled path in
`regression-suite/src/RJitArrayTypecheck.java`.

## Two diagnostics added, both permanent and env-gated

* `CRATONVM_DBG_INTERRUPT` — one line plus the Java frame stack for every
  `Thread.interrupt()`. A spurious interrupt is invisible where it is
  *consumed* (the victim only sees a flag), so the only way to attribute one is
  to record the producer. It named residual 1's producer on the first run.
* `CRATONVM_DBG_JIT_COMPILED` — one line per successfully published
  compilation. The only way to answer "is this method actually running
  compiled?", which is exactly what an A/B that differs only in throughput
  cannot tell you.

## Not caused by any of this, found on the way

`RCollections` and `RReflect` in `regression-suite/` already fail on `dev`,
independently (verified on three separately built dev-based binaries and with
the dispatch flag both on and off): a null `String` receiver in a collections
path, and an empty `getSimpleName()` for an anonymous class.

## Reproducing

```bash
cd apps/h2database-suite-runner
./run-h2-suite.sh discover          # required in a fresh worktree; meta/ is not committed
ONLY='TestStreamStore|TestFreeSpace|TestNestedJoins'
TMPDIR=/data/tmp H2_ROOT=/data/data/h2database/h2 CRATONVM_BIN=<binary> \
  CRATONVM_JIT_ALLOW_PACKAGES='org/h2/' \
  OUTROOT=<out> ./run-h2-suite.sh run --category all --only "$ONLY" --tag lifted
```

Without `discover` the runner prints `nothing to run` and exits 0.
`TMPDIR=/data/tmp` is required on the Azure host: `/` is full and the runner's
internal `mktemp` silently produces empty results otherwise. The same full root
filesystem breaks `cargo build` (`cc` for `zstd-sys`/`libsqlite3-sys`/
`libmimalloc-sys` dies with "No space left on device" writing to `/tmp`), so
build with `TMPDIR=/data/tmp/build`.

Run comparison arms **one at a time**. Three concurrent 218-class runs on this
host produced two false per-class regressions that both evaporated on isolated
re-runs.

## Related

- `vm/src/jit/skip_list.rs` — the `HIB-LONGTAIL.1` comment and the restored
  `JASPER-JDT.2`/`.3` entries.
- `vm/src/jit/helpers.rs` — `direct_virtual_compiled_callee_entry_enabled`.
- `docs/known-issues/jit-bans/jit-ban-sweep-consolidated-status-20260726.md` — the sweep this came out of.
- `docs/known-issues/jit-bans/full-ban-inventory-status-20260726.md` — the cross-session ban tracker.
- `docs/internal/jit-bans/hib-antlr-1-removed-shadowed-20260726.md` — the
  `org/antlr/v4/runtime/` half of this same ban. **Removed 2026-07-27**: the
  H2 suite never exercises ANTLR, and a 57-class Hibernate HQL A/B came back
  equivalent. HIB-LONGTAIL.1 is `org/h2/`-only now, so the three residuals
  below are all that is left of it.
