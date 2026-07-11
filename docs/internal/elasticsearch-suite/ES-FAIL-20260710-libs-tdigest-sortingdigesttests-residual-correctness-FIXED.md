# ES FAIL - libs/tdigest org.elasticsearch.tdigest.SortingDigestTests correctness residual

Status: FIXED — both clusters resolved. `-Jit off` (`NoSuchMethodError`
receiver-identity-loss + the log4j `ClassCastException`s) fixed at dev commit
`3e489a1c` (merged `d8aee876`). `-Jit on` (garbage-index
`ArrayIndexOutOfBoundsException` + wrong quantile/count values) fixed at dev
commit `05f6930e`. `SortingDigestTests` now passes 19/20 in both modes; the
20th (`testMonotonicity`) is blocked by a separate, pre-existing, OPEN
performance issue — see
[`testmonotonicity-quantile-cdf-dispatch-performance.md`](../../known-issues/elasticsearch-suite/testmonotonicity-quantile-cdf-dispatch-performance.md).

Found while retiring
[`ES-CRASH-20260709-libs-tdigest-org-elasticsearch-tdigest-sortingdigesttests-0249530511-FIXED.md`](../fixed-suite-bugs/ES-CRASH-20260709-libs-tdigest-org-elasticsearch-tdigest-sortingdigesttests-0249530511-FIXED.md)
— that doc's `System$1.findNative` crash is fixed and confirmed gone; this
doc tracked two separate, newly-exposed bugs the crash was previously
masking (the process never got far enough to hit them before). Both are now
fixed.

Observed in:
- Runs: `repro-tdigest-0249530511` (`-Jit on`) and
  `repro-tdigest-jitoff-0249530511` (`-Jit off`), manual re-runs of the
  retired doc's repro on current `dev`
- VM: `craton`
- rc: `1` (both modes)
- status: `FAIL` (both modes)
- tests: 20, failed: 6 (both modes) — **different 6 tests fail in each mode**

Re-run one class:
```powershell
# -Jit on
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category others -Jit on -Vm craton -ElasticsearchRoot "<worktree>/apps/elasticsearch" -WorkDir "<worktree>/apps/elasticsearch-suite-runner/.suite-<run>" -Exe <exe> -JdkHome /usr/lib/jvm/java-21-openjdk-amd64 -TimeoutSec 600 -RunName repro-tdigest -ModeName repro-tdigest -Start 154 -Count 1

# -Jit off
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category others -Jit off -Vm craton -ElasticsearchRoot "<worktree>/apps/elasticsearch" -WorkDir "<worktree>/apps/elasticsearch-suite-runner/.suite-<run>" -Exe <exe> -JdkHome /usr/lib/jvm/java-21-openjdk-amd64 -TimeoutSec 600 -RunName repro-tdigest-jitoff -ModeName repro-tdigest-jitoff -Start 154 -Count 1
```

Both modes also print a benign, unrelated bootstrap warning that is NOT
the cause of any failure (Elasticsearch's own `NativeAccess` bootstrap
catches it and disables native access, which is expected/inert in this
sandboxed test environment):

```text
java.lang.AbstractMethodError: method java/lang/foreign/SegmentAllocator.allocate(JJ)Ljava/lang/foreign/MemorySegment; has no Code attribute
	at java.lang.foreign.SegmentAllocator.allocate(SegmentAllocator.java:318)
	at org.elasticsearch.nativeaccess.jdk.JdkPosixCLibrary.<clinit>(JdkPosixCLibrary.java:123)
	...
```

## `-Jit on` cluster (6 of 20) — FIXED at dev `05f6930e`

Original failures:

```text
1) testMidPointRule
java.lang.AssertionError: expected:<1.0> but was:<0.7>
2) testSorted
java.lang.ArrayIndexOutOfBoundsException: Index 1106770591 out of bounds for length 57100
3) testSingletonAtEnd
java.lang.AssertionError: expected:<0.25> but was:<0.125>
4) testSingletonAtEnd
java.lang.AssertionError:
Expected: <0L>
     but: was <160L>
5) testFewRepeatedValues
java.lang.AssertionError: expected:<3000.0> but was:<2.1E-322>
6) testMonotonicity
java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 102899
```

**Root cause**: On-Stack Replacement (OSR) corruption, specifically for
`java/util/DualPivotQuicksort.sort`'s two overloads, reached via
`SortingDigest.compress()` -> `values.sort()` -> `Arrays.sort(double[])`.
Every one of these 5 failing tests calls `quantile()`/`cdf()`, which always
`compress()`es first.

The "garbage index"/"wrong value" symptom was a strong early clue: the
`ArrayIndexOutOfBoundsException` index was NEVER a plausible array index —
it always decoded (in hex) to a plausible CratonVM heap `ObjectRef` address
(e.g. `2200195372192` = `0x20045dd14a0`; CratonVM's heap lives at the
`0x2000_0000_0000`-ish range). That means somewhere, a raw pointer was being
read where an `int` local was expected.

**Investigation** (see the full writeup in the fix commit, `05f6930e`, for
complete detail):
- `Dist.quantile`/`Dist.cdf` (the methods actually doing the index
  arithmetic) never successfully JIT-compile at all — confirmed via
  `CRATONVM_DBG_JITC=1`: they perpetually re-enqueue background-compile
  attempts that silently bail, so they always run interpreted, in both
  `-Jit on` and `-Jit off` modes. This ruled out the initial hypothesis (a
  register-allocation bug in `Dist.quantile`'s own compiled body).
- `CRATONVM_JIT_OSR=0` (disabling On-Stack Replacement VM-wide, leaving
  ordinary JIT compilation on) made all 5 failures disappear with no other
  behavior change — proving the corruption is specifically in OSR, not in
  regular method compilation.
- `CRATONVM_DBG_OSR=1` on the failing repro showed the ONLY methods ever
  OSR-**entered** during the whole run are `DualPivotQuicksort.sort`'s two
  overloads (`([DIII)V` and `(Ldk$Sorter;[DIII)V`) — nothing in the tdigest
  code itself, nothing in the failing test methods' own loops.
- Both overloads are self-recursive (standard dual-pivot partitioning). An
  OSR-entered frame's `stack_floor_slot_off` is seeded to the OSR
  trampoline's `usize::MAX` sentinel (`emit_osr_trampoline`,
  `jit/src/lib.rs`), which makes the inline self-recursive-call fast-path
  check (`RSP > floor`) always false — so an OSR-entered instance of either
  method always routes its recursive calls through the
  `self_call_stack_guard` helper path (`jit/src/x64.rs`, the
  `guard_skip_patch` block) instead of the plain direct call every
  non-OSR invocation uses. That combination (OSR entry + self-recursion)
  gets far less exercise than either feature alone, and is the leading
  suspect for where the corruption lives — but the exact single faulty
  instruction was NOT pinpointed via disassembly within the time spent.

**Fix** (`jit/src/tiered.rs`, `05f6930e`): extended `is_osr_denied()` with a
static check that denies OSR specifically for these two `DualPivotQuicksort`
method signatures. This is a **targeted mitigation, not a root-cause
instruction-level fix** — it mirrors the codebase's existing precedent for
this class of problem (the BC-crypto `newarray`-OSR ban). It does not
disable normal (non-OSR) JIT compilation for these methods: a fresh call
still tiers up and runs fully compiled; only an already-interpreting
invocation is prevented from jumping into compiled code mid-loop via OSR.
If a future session finds and fixes the actual x64-codegen defect, this
static deny should be revisited (see the commit message for the exact
mechanism hypothesis to start from).

**Verification**: `SortingDigestTests` `-Jit on`, 19/20 pass cleanly with
zero `E` (error) markers before `testMonotonicity` (the 20th, blocked by an
unrelated performance issue — see below). `cargo test -p cratonvm-jit --lib`:
889/889 passed, no regressions.

## `-Jit off` cluster (6 of 20) — FIXED at dev `3e489a1c`

Original failures:

```text
1) testMonotonicity
java.lang.NoSuchMethodError: java/lang/Object.get(I)D
2) testMonotonicity
java.lang.ClassCastException: java.lang.Object cannot be cast to org.apache.logging.log4j.message.ReusableParameterizedMessage
3) testBigJump
java.lang.ClassCastException: java.lang.Object cannot be cast to org.apache.logging.log4j.message.ReusableParameterizedMessage
4) testBigJump
java.lang.ClassCastException: java.lang.Object cannot be cast to org.apache.logging.log4j.message.ReusableParameterizedMessage
5) testNaN
java.lang.ClassCastException: java.lang.Object cannot be cast to org.apache.logging.log4j.message.ReusableParameterizedMessage
6) testNaN
java.lang.ClassCastException: java.lang.Object cannot be cast to org.apache.logging.log4j.message.ReusableParameterizedMessage
```

The interpreter-level `NoSuchMethodError` had a precise VM-side signal in
stderr:

```text
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError method="java/lang/Object.get(I)D" caller="org/elasticsearch/tdigest/Dist.quantile(DILjava/util/function/Function;)D @pc=88"
```

**Root cause**: `allocate_lambda_proxy` (`vm/src/runtime/invokedynamic.rs`)
popped a lambda's captured values off the Java operand stack into a raw,
GC-invisible Rust `Vec<Value>` *before* allocating the proxy object. If the
first `try_alloc_object` call failed under memory pressure (young-gen
exhaustion — plausible deep into a heavy test like `testMonotonicity`,
which inserts 100,000 values into one digest before sweeping 10,001 quantile
points), the fallback path called `maybe_gc_forced_pub` — an actual GC
cycle — while the `captures` Vec held zero GC roots. A moving/copying
collector could relocate a captured object during that window (it stays
alive via its real owner, e.g. `SortingDigest.values`, but gets evacuated to
a new address); the STALE pre-move `ObjectRef` — still sitting in the
untracked `captures` Vec — then got written straight into the new proxy's
field with no re-read.

Every later dispatch through that one proxy instance read the captured
field, found the vacated old address, and resolved whatever garbage/zeroed
class id header now sat there instead of the real receiver class. In this
case the vacated slot decoded as `ClassId(0)` / `java.lang.Object`, so
`org/elasticsearch/tdigest/arrays/TDigestDoubleArray::get` (a bound
interface method-reference lambda, `values::get`) dispatched as
`java/lang/Object.get(I)D` and threw a clean `NoSuchMethodError` in the
interpreter.

The log4j `ReusableParameterizedMessage` `ClassCastException`s turned out to
be an unrelated, already-fixed issue by the time of the fix — they no
longer reproduced on current dev independent of this fix, likely fixed by
an intervening, unrelated commit between the doc's base SHA and dev
`4b08ffad`.

**How it was found**: a targeted, class-name-gated `eprintln!` in
`try_lambda_dispatch`'s `InvokeVirtual|InvokeInterface` arm
(`vm/src/runtime/interpreter.rs`), run against the real failing test class
with `CRATONVM_DBG_LAMBDA=1`, showed the exact same captured-field pointer
(`recv_ptr`) resolve correctly (`ClassId(2747)` /
`MemoryTrackingTDigestArrays$MemoryTrackingTDigestDoubleArray`) for
thousands of consecutive `Function.apply` dispatches on one proxy, then flip
to `ClassId(0)` / `java/lang/Object` on the *same unchanged pointer value*
for the dispatch that failed — the smoking gun for a stale-post-GC read
rather than a receiver-resolution or dispatch-logic bug.

**Fix**: pin every captured object reference into
`thread.native_pin_roots` (which the GC remaps on relocation) before
attempting allocation, and refresh `captures` from the pins afterward —
mirroring the existing `coerce_lambda_args` GC-safety pattern a few hundred
lines away in the same file, which already does this correctly for the SAM
call args but not for capture-time allocation.

**Verification**: the exact `NoSuchMethodError` repro no longer throws it;
of the original 6 `-Jit off` failures, all 6 are gone (1 was this bug, 5
were the unrelated already-fixed log4j issue). `cargo test -p cratonvm-vm
--lib`: 2175 passed both before and after the fix (11 pre-existing,
unrelated failures — confirmed identical on unmodified dev tip via `git
stash`). A standalone reflection-based driver (real
`MemoryTrackingTDigestArrays` + `SortingDigest.create`, 100K adds + 2M
repeated `quantile()` calls under a 128 MB heap to force GC churn) shows
zero mismatches.

## Why this wasn't caught before

The original `System$1.findNative` crash aborted the process before any
of `SortingDigestTests`' 20 tests ran (`rc=139`, 0 tests parsed). Once that
crash was fixed, the tests actually executed and these two independent,
pre-existing residuals became visible for the first time under this class.

## Residual (split out, OPEN)

`testMonotonicity` (the 20th test, in both JIT modes) does not fail
incorrectly — it never throws or asserts a wrong value — but does not
complete within a reasonable time (900+ seconds under `-Jit on`, does not
finish in an hour under `-Jit off`/`--nojit`). This is a genuinely separate,
pre-existing performance issue (not a correctness bug, and not caused by
either fix above — confirmed via live `gdb` sampling showing real forward
progress, not a livelock). See
[`testmonotonicity-quantile-cdf-dispatch-performance.md`](../../known-issues/elasticsearch-suite/testmonotonicity-quantile-cdf-dispatch-performance.md).

## Update 2026-07-10 (later, concurrent session): true root cause of the `-Jit on` cluster

The OSR-deny for `DualPivotQuicksort.sort` (dev `05f6930e`, above) fixes the
symptom but is a MASK: it works because denying OSR for `sort` also prevents
the OSR pipeline's eager-callee compiles, so the actually-poisoned artifact
is never created. The underlying defect — found independently and
concurrently on `fix/es-tdigest-jiton-20260710` — is in the **C2/IR backend
lowerer** (`jit/src/ir_lower.rs`): `node_slot` is zero-initialised and
`slot_of()` returned that `0` default for an SSA node the scheduler never
placed in an emitted block. The emitted use then reads `[rbp - 0]` — the
saved caller RBP — as a data value. Concretely, IR-compiling
`java/util/DualPivotQuicksort.insertionSort([DII)V` (IR-eligible call-free
FP compute) left the pc17 `ArrayLoad(Double)` feeding a GVN-collapsed loop
phi unallocated, so `a[i+1] = ai` stored the frame pointer into the array
being sorted (stack addresses as ~6e-310 subnormals, then smeared by the
insertion shift). Every large `Arrays.sort(double[])` under `-Jit on` was
silently mis-sorted whenever that artifact was reachable; all six failure
shapes in this doc were downstream of sorting on corrupted data.

Pinned by: `CRATONVM_DBG_PRECISE` showing ZERO GC activity before the
corruption (eliminating every GC/staleness theory), a standalone
`Arrays.sort(double[10000])` repro, per-callee `CRATONVM_JIT_BISECT_SKIP`
bisection, `CRATONVM_JIT_IR_FP=0` isolating the IR backend, a hardware
watchpoint (gdb, ASLR off) catching `movsd [rax+rcx*8+0x28], xmm0` writing
`rbp+0x2E0` into `a[0]`, and an env-gated `slot_of` probe naming the
unallocated node (`CRATONVM_DBG_IRSLOT=1`).

Fix (same branch): any `slot_of()` readback of an unallocated slot latches
`unallocated_slot_use` and the lowering bails to the single-pass backend —
closing the whole class of "scheduler gap ⇒ silent frame-pointer read"
miscompiles rather than this one victim. Three companion JIT soundness gaps
found during the hunt are fixed in the same commit: the OSR dead-local mask
ignored XMM-resident (double) locals (garbage seeded into coalesced pivot
registers on OSR entry at `sort`'s bci 575/599 — this doc's original
"OSR-reuse at two entry PCs" observation); the raw self-recursive CALL path
pushed a phantom RAX return value for void methods; and the precise-maps
innermost-RBP mirror was never restored when a compiled callee returned into
a Rust dispatch helper. With the IR-lowerer bail in place the OSR deny for
`DualPivotQuicksort.sort` is no longer load-bearing for correctness (kept as
a harmless perf-policy choice).

Also for the record: the `testMonotonicity` `-Jit on`
`NoSuchMethodError: java/lang/Object.get(I)D` that briefly surfaced between
the IR fix and this merge was the stale-lambda-capture-field family — a
gated diagnostic (`CRATONVM_DBG_LAMBDA=1`, landed in `try_lambda_dispatch`)
showed the dead pointer baked into the proxy's capture field with no
forwarding pointer and a fresh field re-read returning the same dead
address. It stopped reproducing after merging dev `395f7246` (concurrent
GC-audit work; exact commit not bisected).
