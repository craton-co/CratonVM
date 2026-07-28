# `org/h2/` JIT ban (HIB-LONGTAIL.1) — residuals 1–3 CLOSED, residual 4 root-caused, ban STAYS

**Status (2026-07-27, second pass):** residuals 1, 2 and 3 stay FIXED, and the
two things the previous revision left open are now closed:

* **The `memLZF:` correctness failure is fixed.** It was not a memory-ordering
  violation in compiled `org/h2` code. `AtomicIntegerArray.compareAndSet` — the
  spin lock the test uses — was not a compare-and-swap at all. Every
  read-modify-write native on `AtomicIntegerArray`, `AtomicLongArray`,
  `AtomicReferenceArray` and `VarHandle` was a bare read followed by a bare
  write. General VM defect, not H2, not JIT.
* **The socket half of residual 1's `interruptor` gap is fixed.**
  `SocketChannel` / `ServerSocketChannel` were left with a null
  `AbstractInterruptibleChannel.interruptor` because seeding it needs an
  allocation on a path that carries a warning against allocating. Sequencing the
  allocation after the monitor fields and pinning across it removes the
  objection.

**Residual 4 is root-caused** and is no longer "a flat interpreter tail with no
second hot spot to attack". It is two separable things. One — a JIT that
recompiled the same OSR entry 701 times in 20 operations and executed none of
it — is **fixed**. The other is the actual gap, and it is one specific
combination, not a general slowness: LZF over a `ByteBuffer` moves one byte per
`DirectByteBuffer.get(int)` call, and the same LZF over a `byte[]` is 12x
against HotSpot where the `ByteBuffer` version is 1,130x. That one is **not**
fixed here; it is a change to the VM's hottest dispatch path and it is now
scoped precisely enough to be a bounded project rather than a mystery.

The `org/h2/` ban itself **stays**. The two Eclipse JDT bans this page's flip
forced back on are **gone again** — a concurrent session bisected the actual
defect (`613b10f4c`, an LICM/speculative pre-header a forward branch into the
loop header skips) and removed them causally rather than on staleness. The
40-run null result this revision measured independently is corroboration for
that, and is kept below with the reason it looked like a contradiction at the
time.

None of the closed residuals was an H2 bug.

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

**Bug B, socket half — now also FIXED (2026-07-27).**
`SocketChannel` and `ServerSocketChannel` are built by the same bridge and had
the same null `interruptor`. The previous revision left them alone on the
grounds that seeding the field needs an allocation, and the shared
`init_channel_locks` (`native-io/src/socket_channel.rs`) carries an explicit,
empirically-earned warning against allocating on that path: an allocation there
once relocated the channel under concurrent load and left `closeLock` null,
killing the Apache httpasyncclient reactor.

That objection is about **ordering**, not about allocating as such. The three
monitor fields are now seeded *first* — a moving GC rewrites the reference they
hold along with every other, so a lock seeded before the allocation survives the
move — and only then is the interruptor built, with `ch` pinned across the call
and the live address read back (`pin_native_root` / `read_native_pin`, the
native stale-local pattern `FileChannelImpl` already uses). `init_channel_locks`
now returns the post-GC ref and is `#[must_use]`, so no caller can keep using a
stale one. All three construction sites (`SocketChannel.open`,
`ServerSocketChannel.open`, `accept`) are covered.
Witness: `regression-suite/src/RSocketChannelInterrupt.java` — asserts a
non-null `interruptor` of the JDK's own `AbstractInterruptibleChannel$…` type
**and** a non-null `closeLock` on all three, so a regression in either direction
fails. It fails on the pre-fix binary on the first channel it looks at.

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

## The `memLZF:` correctness failure — FIXED, and it was not what the doc said

The previous revision recorded this as the most concrete blocker to lifting the
ban: with the ban lifted, `TestFileSystem`'s `memLZF:` `testConcurrent` failed
intermittently (2 of 4 runs) with

```
java.lang.AssertionError: Expected: 3900 actual: 3897
java.lang.AssertionError: Expected: 5128 actual: 5168
```

and it concluded: "The reader holds the same `AtomicIntegerArray` spin lock the
writer held … seeing fresh file contents with a stale `expected` is a
memory-ordering violation … That points at compiled `org/h2` code reordering
across `AtomicIntegerArray.set`/`compareAndSet`."

The reader does not hold the same lock, because there was no lock.
`AtomicIntegerArray.compareAndSet` was implemented as

```rust
let cur = ctx.get_array_element(arr, idx);
if cur == expected {
    ctx.set_array_element(arr, idx, Value::Int(update));
    Ok(Some(Value::Int(1)))          // "I won"
}
```

— a plain read, a comparison, and a plain write, with nothing in between. Two
threads can both read `0` and both report success. H2 spells its lock exactly
this way:

```java
while (!locks.compareAndSet(pos, 0, 1)) { }
try { e = expected.get(pos); f.read(byteBuff, pos * 64 * 1024); }
finally { locks.set(pos, 0); }
```

so writer and reader ran the critical section simultaneously, and the reader
read one of `expected` / the file on each side of the writer's update. That
produces mismatches in **both** directions, which is what the two recorded
assertions show (3900 vs 3897 one way, 5128 vs 5168 the other) — a pure
reordering story only explains one.

It is a general VM defect, and it was the whole family, not one method:
`AtomicIntegerArray`, `AtomicLongArray` and `AtomicReferenceArray` had this
shape in **both** of their registration sites
(`native-builtins/src/util_concurrent_ext.rs`, used in real-JDK mode, and
`native-builtins/src/phases_early.rs`, used in synthetic-jdk mode) for
`compareAndSet`, `getAndSet`, `getAndAdd`, `getAndIncrement`, `getAndDecrement`,
`incrementAndGet` and `decrementAndGet`. `VarHandle.compareAndSet` and its
`weakCompareAndSet*` aliases (`native-builtins/src/phases_late/reflect_invoke.rs`)
had it too, on both the array-element and instance-field paths. The scalar
`AtomicInteger` / `AtomicLong` natives were always correct — they use
`NativeContext::compare_and_swap_field`, which takes the per-object CAS lock.
The array classes simply never got the same treatment.

Fixed by routing every array RMW through `compare_and_swap_field`, which already
special-cases array receivers, via two helpers in `util_concurrent_ext.rs`
(`atomic_array_cas`, `atomic_array_rmw`). While there: the interpreter
force-routes a fixed method list for these three classes to a native (the "C23"
block in `vm/src/vm/vm_exec.rs`, because the real JDK bodies go through
`VarHandles$Array$*`), and a dozen names on that list had no registration and so
fell back to exactly the bytecode the force-route exists to avoid —
`addAndGet`, `lazySet`, `getPlain`/`setPlain`, `getAcquire`/`setRelease`,
`getOpaque`/`setOpaque`, `weakCompareAndSet*`, `compareAndExchange*`. Registered.
`VarHandle` statics keep the old read-then-write shape: there is no static-field
CAS primitive on `NativeContext` to route them through.

Witness: `regression-suite/src/RAtomicArray.java`. Four threads take a
per-slot `compareAndSet` lock 80,000 times and check that only one is ever
inside; plus contended `getAndIncrement`/`addAndGet` totals, contended
`AtomicReferenceArray` null→token claims, and single-threaded return-value
conformance diffed against HotSpot. It fails on the pre-fix binary **on every
run, within a second**, which is the useful part: the H2 face needed a 10,000
operation run and reproduced 2 times in 4.

## Residual 4 — `TestFileSystem.testConcurrent` on `nioMemLZF:1:` — root-caused

The previous revision measured the gap and stopped, calling the profile "a long
interpreter tail with no second hot spot to attack" and the whole thing "a
throughput project on interpreted LZF + `ByteBuffer` inside a backoff-free spin
lock". Three of those four nouns are wrong.

Measurements below are from `LzfProbe`, a standalone replica of
`testConcurrent` (see *Reproducing*) that reports ms per 100 operations, so the
loop can be measured in seconds instead of against a 25-minute cap.

**It is not the spin lock.** Running the writer alone — no reader thread, so no
contention on the `compareAndSet` lock at all — still costs 43–50 s per 100
operations (the spread is host load; this box carried other sessions
throughout). Adding the reader back roughly doubles the wall clock, which is
what a second thread doing the same work costs, not what lock contention costs.

**It is not LZF, it is not `ByteBuffer`, and it is not interpretation in
general.** All four prefixes, same session, same configuration (writer only,
100 operations, `org/h2/` ban in place, host load ~13):

| prefix | backing | compressed | HotSpot | CratonVM | ratio |
|---|---|---|---|---|---|
| `memFS:1:` | `byte[]` | no | 1.9 ms | 7.6 ms | 4x |
| `nioMemFS:1:` | `ByteBuffer` | no | 2.6 ms | 11.6 ms | 4.5x |
| `memLZF:1:` | `byte[]` | yes | 3.7 ms | 44.7 ms | 12x |
| `nioMemLZF:1:` | `ByteBuffer` | yes | 44.0 ms | 49,705 ms | **1,130x** |

Read down the table: `ByteBuffer` alone costs 4.5x (fine), LZF alone costs 12x
(fine, ordinary interpreter territory), and the two *together* cost 1,130x. The
cost is not in either ingredient; it is in one specific combination.

**What it actually is:** `FileNioMemData` stores its pages as
`ByteBuffer.allocateDirect`, so it calls the `CompressLZF.compress(ByteBuffer,
…)` / `expand(ByteBuffer, ByteBuffer)` overloads, which read and write **one
byte at a time through `DirectByteBuffer.get(int)` / `put(int, byte)`**. A 64 KB
page is ~65,000 of those per pass and each one is a full interpreted
`invokevirtual` into a JDK method — roughly half a microsecond end-to-end. Two
to four page passes per operation is ~250 KB of per-byte traffic, which is the
~0.5 s. `memLZF:` runs the same algorithm over a `byte[]`, where the same loop
is `baload`/`bastore` — no call at all — and is ~1,100x faster for it. That is
also why the *uncompressed* `nioMemFS:` prefix is fine: it moves whole pages
with bulk `ByteBuffer` operations, one call per page instead of one per byte.
The `ByteBuffer` abstraction is not the problem; per-element access through it
is.

The profile is consistent with that and only reads as "flat" if you do not know
what is being called: `NativeMethodRegistry::slot_for_exact` 14.4%,
`try_jit_compile_callee` 8.2%, `__memcmp_evex_movbe` 3.9%,
`jit_invoke_virtual_mic` 3.2%, `execute_invokevirtual_cached` 2.7%,
`safe_native_call_impl` 2.7%, `invoke_or_native` 2.3% — every one of them
per-invoke dispatch overhead, paid ~65,000 times per page. `slot_for_exact` is
top because it is a *miss*: `DirectByteBuffer.get(I)B` is real JDK bytecode, so
the registry is digested and probed on every call only to answer "no native".

This is still a project, but a bounded and general one — per-call-site
memoisation of the native-registry answer, or an interpreter intrinsic for
`ByteBuffer` element access — and it is worth far more than this test. It is
**not** fixed here.

### What IS fixed: the OSR compile livelock

`CRATONVM_DBG_JIT_COMPILED` on the ban-lifted arm showed
`org/h2/compress/CompressLZF.compress(Ljava/nio/ByteBuffer;I[BI)I` — the hottest
method in the whole workload — being OSR-compiled **701 times in 20
operations**. `CRATONVM_DBG_JITC` showed all of them at the *same* back-edge,
`entry_pc=220`.

The cause is a disagreement inside the OSR pipeline. A compile is requested for
one back-edge PC; the artifact then decides for itself which PCs it will accept,
and `can_osr_enter` refuses any PC whose `osr_dead_mask` is non-zero. Those two
decisions can disagree — the compile succeeds and the resulting body refuses the
very entry it was compiled for — and nothing memoised the disagreement, so the
next trip over the same back-edge ran the full x64 pipeline again, forever, for
zero executed compiled code. It is the same waste loop this module already
memoises twice (RBC.2's 2,610 recompiles of `SecP521R1Curve$1.lookup`, RBC.4's
35,923 re-run pipelines on `Nat.inc`) and it was simply missing a third memo.

Two fixes, both in the general JIT:

1. **A per-(method, entry_pc) OSR reject memo** (`jit::mark_osr_entry_rejected` /
   `is_osr_entry_rejected`, consulted by `compile_osr_artifact`). The verdict is
   a pure function of a deterministic compile, so it is permanent. Keyed per PC,
   not per method, because a method's other back-edges are usually fine.
   **701 compiles → 1.**
2. **A precise `osr_dead_mask`** (`jit/src/x64.rs`). The mask was
   `every register-resident local not live at this PC`, but the hazard the
   2026-07-04 refusal rests on is *sharing*: a dead local whose register is also
   a live local's home. A dead local that owns its register outright has no
   coalesced state to reconstruct. The mask now names only the sharing ones.
   `CRATONVM_JIT_OSR_DEAD_MASK_BLANKET=1` restores the old behaviour.
   `CRATONVM_DBG_OSR_META` now prints the published mask next to the blanket set
   it is refined from — it previously recomputed the blanket value and so would
   have silently disagreed with the real metadata.

`entry_pc=220` in `CompressLZF.compress` is still refused after (2): its two
dead locals genuinely do share registers with live ones. The memo is what turns
that from a livelock into a single wasted compile.

## Where the ban stands — it STAYS

### This revision's own 218-class run

The fixes above were gated on a full 218-class run with the ban in place
(`154 PASS / 18 FAIL / 46 HANG`) against the previous revision's matching arm
(`166 / 21 / 31`). The host carried a load average of 28–33 on 16 cores for the
whole run — from other sessions, not this one — so the raw totals are not
comparable and were not treated as such. The per-class diff gives 12 apparent
regressions; **every one was chased down and none is real**:

| apparent regression | verdict |
|---|---|
| `TestMvccMultiThreaded2`, `TestPageStoreCoverage`, `TestReopen` (PASS→FAIL) | PASS on **both** binaries when re-run in isolation |
| `TestKillRestart` (PASS→HANG) | 4/4 PASS on **both** binaries, alternating arms |
| `TestNestedJoins`, `TestCompress`, `TestStringCache` (PASS→HANG) | PASS on both in the targeted re-run |
| `TestIndex`, `TestLimit`, `TestIntPerfectHash` (PASS→HANG) | HANG on **both** |
| `TestFuzzOptimizations` (PASS→HANG) | FAIL on **both** |
| `TestRunscript` (PASS→HANG) | flaky on **both** — see below |

`TestRunscript` is worth recording separately, because the previous revision
listed it as one of three CRASHes and as a class that "regressed again". Over 8
alternating rounds per arm it is `3 PASS / 2 FAIL / 3 HANG` on this revision's
binary and `5 PASS / 1 FAIL / 2 HANG` on the pre-fix one, with the **same**
failure on both (`AssertionError: expected: GRANT "TESTROLE" TO`) and a runtime
of 250–300 s against a 300 s cap. It is a pre-existing flaky, borderline class,
and any single-run verdict about it — in either direction — is noise.

**The lesson the previous revision recorded is worth restating:** it warned
"run comparison arms one at a time" after three concurrent suites produced two
false regressions. That is necessary but not sufficient on this host, where the
load that matters is other people's. Alternate the two binaries round by round
over the same class, and print the counts.

### The previous revision's A/B

Same-binary 218-class A/B, with the dispatch fix in place throughout:

| arm | PASS | FAIL | HANG | CRASH |
|---|---|---|---|---|
| dispatch flag OFF, ban in place (pre-2026-07-27 dev behaviour) | 162 | 24 | 32 | 0 |
| dispatch flag ON, ban in place | **166** | 21 | 31 | 0 |
| dispatch flag ON, ban lifted | 155 | 28 | 32 | 3 |

The flip is worth +4 net and has **no** regressions: its only two apparent ones
were re-run 3x each in isolation and both are artifacts of running three suites
concurrently (`TestMvccMultiThreaded` passes 3/3 in *both* configurations;
`TestOpenClose` hangs 3/3 in *both*).

The `memLZF:` correctness failure that this revision fixes was the strongest
argument against lifting, and it is gone. What remains is the throughput
deficit: lifting still costs net PASS, `nioMemLZF:` still does not finish, and
the three CRASHes (`TestRunscript`, `TestPageStoreCoverage`, `TestReopen`) are
untouched by anything here. Note that `TestReopen` was one of the six classes an
earlier revision recorded as *closed* — it regressed again once the dispatch fix
let compiled H2 code actually run compiled, which is a good reason to distrust
any per-class verdict taken before that fix.

## The flip's own fallout: two Eclipse JDT bans had to be RESTORED (since fixed)

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

Both were restored on 2026-07-27. `parser/` is directly re-confirmed by the
bisection above; `ast/` was restored on the shadowing argument alone.

**UPDATE 2026-07-28 — both bans are now REMOVED again, and this time the defect
is root-caused rather than assumed stale.** Bisected across the 150 commits from
the restore point to `dev` (two runs per step, both packages allowed, direct-entry
path ON) to `613b10f4c`, "fix(jit): LICM/speculative pre-header bypassed by a
branch into the loop header", and confirmed causally on a single current-`dev`
binary carrying an env-gated revert of that guard — pre-fix behaviour 2/2 FAIL
with the identical `ClassCastException`, shipping behaviour 2/2 PASS. So the
`CRATONVM_JIT_DENY=org/eclipse/jdt/internal/compiler/parser/` bisection recorded
above was pointing at the *victim* package, not at a JDT-specific defect: the
bypassed pre-header is a general x64 backend bug that ECJ's `Parser`/AST code
happens to hit hard. Re-verified with the bans deleted:
`TestOptionalELResolverInJsp` 3/3, `TestFormAuthenticatorA/B/C` 2/2 each,
`TestCompiler` 2/2, 167 parser + 29 ast methods compiling per run. Full evidence
in the retired `jasper-jdt-2-3-fixed-licm-preheader-20260728` write-up. None of
this changes the H2 verdicts on this page.


This revision reached the same place independently, from the other end, and its
numbers are worth keeping as corroboration. `TestOptionalELResolverInJsp` with
`parser/` JIT-eligible and the direct-entry path on is **20/20 PASS on this
tree and 20/20 PASS on the pre-fix `dev` it branched from** (`017bc3734`) — with
the control checked, because a lift that does not lift proves nothing:
`CRATONVM_DBG_JIT_COMPILED` counts 0 compiled `parser/` methods with the ban
active and 166 with it lifted. `TestFormAuthenticatorA` with `ast/` lifted is
3/3. At the time that looked like an unexplained disagreement with the
restore, and it was written up as "keep the bans, a null result does not
overturn a positive one — go re-run the bisection". The bisection above is that
re-run, and it settles it: **`613b10f4c` is an ancestor of `017bc3734`**, so
both of those binaries already carried the LICM pre-header fix. Two clean arms
was not a flaky defect going quiet; it was the defect being gone from both.

**The rule this leaves behind:** any ban whose mechanism is compiled-to-compiled
virtual dispatch must be re-verified with
`CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY` **on**, or it verifies
nothing. That flag was default-OFF for the entire period in which the
2026-07-25/26 ban sweep did its removals. And the check is cheap: count
`CRATONVM_DBG_JIT_COMPILED` lines for the banned package in both arms before
believing either.

(This section appeared twice, verbatim, in the previous revision. Deduplicated.)

## Also fixed — `ClassCastException` named an array receiver by its component

`java.lang.String cannot be cast to java.lang.String` for a `String[]` receiver.
The header word of a reference array carries the *component* class id, so the
`class_id_of` → `class.name` lookup that both the interpreter's `checkcast` and
the JIT's `jit_checkcast` used reported the component. HotSpot prints the
descriptor. Fixed in both; nested and primitive arrays covered; asserted on the
interpreted and the compiled path in
`regression-suite/src/RJitArrayTypecheck.java`.

## Three diagnostics added, all permanent and env-gated

* `CRATONVM_DBG_INTERRUPT` — one line plus the Java frame stack for every
  `Thread.interrupt()`. A spurious interrupt is invisible where it is
  *consumed* (the victim only sees a flag), so the only way to attribute one is
  to record the producer. It named residual 1's producer on the first run.
* `CRATONVM_DBG_JIT_COMPILED` — one line per successfully published
  compilation. The only way to answer "is this method actually running
  compiled?", which is exactly what an A/B that differs only in throughput
  cannot tell you. Tallying its output is what found the 701-compile livelock.
* `CRATONVM_DBG_JITC` `OSR-compile` / `OSR-reuse` / `OSR-reject` lines carry
  `entry_pc`, which is what separated "many back-edges" from "one back-edge, 256
  times" in about a minute.

## Not caused by any of this, found on the way

`RCollections` and `RReflect` in `regression-suite/` were recorded here as
failing on `dev`. They pass now: `fix/regsuite-rcollections-rreflect-20260726`
landed on `dev` in the meantime (`75fdcdbff`, `AbstractSet.hashCode` on foreign
layouts + anonymous/local class naming). Verified, not assumed — the full
17-class suite is green on the binary this revision was validated with.

## Reproducing

### The atomic-array defect (seconds, no H2 needed)

```bash
CV=<binary> JDK=/data/data/jdk25-real ONLY=RAtomicArray bash regression-suite/run.sh
```

### The `nioMemLZF:` throughput gap (minutes, no suite runner needed)

`LzfProbe.java` (a standalone replica of `testConcurrent` with per-100-operation
timing; `-Dprobe.reader=false` drops the reader thread to separate contention
from per-operation cost):

```bash
H2CP=/data/data/h2database/h2/target/classes
javac -cp $H2CP -d /data/tmp/probe LzfProbe.java
TMPDIR=/data/tmp <binary> --java-home /data/data/jdk25-real \
  -Dprobe.reader=false -cp $H2CP:/data/tmp/probe \
  LzfProbe 'nioMemLZF:1:/probe' 100
```

Swap the prefix for `nioMemFS:1:`, `memLZF:1:` or `memFS:1:` for the comparison
row. Add `CRATONVM_DBG_JIT_COMPILED=1` and pipe through
`grep DBG_JIT_COMPILED | sed 's/.*: //' | sort | uniq -c | sort -rn` to see the
compile tally.

### The full suite

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

- `vm/src/jit/skip_list.rs` — the `HIB-LONGTAIL.1` comment and the
  `JASPER-JDT.2`/`.3` removal comment (restored 2026-07-27, removed again
  2026-07-28 once root-caused).
- `vm/src/jit/helpers.rs` — `direct_virtual_compiled_callee_entry_enabled`.
- `native-builtins/src/util_concurrent_ext.rs` — `atomic_array_cas` /
  `atomic_array_rmw`, and the comment recording why the array atomics were not
  atomic.
- `jit/src/x64.rs` — the precise `osr_dead_mask`.
- `docs/internal/fixed-suite-bugs/jit-osr-linux-regression-triad.md` — the
  2026-07-04 decision the dead-mask refinement is careful to preserve.
- `docs/known-issues/jit-bans/jit-ban-sweep-consolidated-status-20260726.md` — the sweep this came out of.
- `docs/known-issues/jit-bans/full-ban-inventory-status-20260726.md` — the cross-session ban tracker.
- `docs/internal/jit-bans/hib-antlr-1-removed-shadowed-20260726.md` — the
  `org/antlr/v4/runtime/` half of this same ban. **Removed 2026-07-27**: the
  H2 suite never exercises ANTLR, and a 57-class Hibernate HQL A/B came back
  equivalent. HIB-LONGTAIL.1 is `org/h2/`-only now, so the three residuals
  below are all that is left of it.
