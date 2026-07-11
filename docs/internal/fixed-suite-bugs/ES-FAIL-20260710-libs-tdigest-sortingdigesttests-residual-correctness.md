# ES FAIL - libs/tdigest org.elasticsearch.tdigest.SortingDigestTests correctness residual

Status: RETIRED (all correctness clusters FIXED) — see "Final update
2026-07-10 (retirement)" at the bottom. The `-Jit off` cluster was fixed by
dev `3e489a1c`; the `-Jit on` cluster was an IR-lowerer miscompile of
`java/util/DualPivotQuicksort`, fixed on `fix/es-tdigest-jiton-20260710`;
the last `testMonotonicity` NSME stopped reproducing after merging dev
`395f7246` (concurrent GC-audit work). The one remaining suite failure is a
PERFORMANCE timeout, split into
[`ES-FAIL-20260710-libs-tdigest-testmonotonicity-suite-timeout-slowness.md`](../../known-issues/elasticsearch-suite/ES-FAIL-20260710-libs-tdigest-testmonotonicity-suite-timeout-slowness.md).

Found while retiring
[`ES-CRASH-20260709-libs-tdigest-org-elasticsearch-tdigest-sortingdigesttests-0249530511-FIXED.md`](../../internal/fixed-suite-bugs/ES-CRASH-20260709-libs-tdigest-org-elasticsearch-tdigest-sortingdigesttests-0249530511-FIXED.md)
— that doc's `System$1.findNative` crash is fixed and confirmed gone; this
doc tracks two separate, newly-exposed bugs the crash was previously
masking (the process never got far enough to hit them before).

Observed in:
- Runs: `repro-tdigest-0249530511` (`-Jit on`) and
  `repro-tdigest-jitoff-0249530511` (`-Jit off`), manual re-runs of the
  retired doc's repro on current `dev`
- VM: `craton`
- rc: `1` (both modes)
- status: `FAIL` (both modes)
- tests: 20, failed: 6 (both modes) — **different 6 tests fail in each mode**

Collection context:
- Host: `victor@20.83.144.174`
- Worktree: `/data/data/cratonvm-worktrees/20260710-093821-es-tdigest-sortingdigest`
- Branch used for collection: `fix/es-tdigest-sortingdigest-20260710-093821`
- Collection binary: `/data/data/cratonvm-targets/es-tdigest-sortingdigest-20260710-093821/release/cratonvm-es-tdigest-sortingdigest-20260710-093821`
- Binary base dev SHA: `df1650e1dcbc825d295faee60b844b9236d91493`, reverified after
  merging forward to `c9e68f12` (post-merge commit `51b8acfc`) — same 6/20
  failures in the same test methods in both modes, crash still absent

Re-run one class:
```powershell
# -Jit on
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category others -Jit on -Vm craton -ElasticsearchRoot "<worktree>/apps/elasticsearch" -WorkDir "<worktree>/apps/elasticsearch-suite-runner/.suite-<run>" -Exe <exe> -JdkHome /usr/lib/jvm/java-25-openjdk-amd64 -TimeoutSec 600 -RunName repro-tdigest-0249530511 -ModeName repro-tdigest-0249530511 -Start 154 -Count 1

# -Jit off (different failures, see below)
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category others -Jit off -Vm craton -ElasticsearchRoot "<worktree>/apps/elasticsearch" -WorkDir "<worktree>/apps/elasticsearch-suite-runner/.suite-<run>" -Exe <exe> -JdkHome /usr/lib/jvm/java-21-openjdk-amd64 -TimeoutSec 600 -RunName repro-tdigest-jitoff-0249530511 -ModeName repro-tdigest-jitoff-0249530511 -Start 154 -Count 1
```

Both modes also print a benign, unrelated bootstrap warning that is NOT
the cause of the 6 failures (Elasticsearch's own `NativeAccess` bootstrap
catches it and disables native access, which is expected/inert in this
sandboxed test environment):

```text
java.lang.AbstractMethodError: method java/lang/foreign/SegmentAllocator.allocate(JJ)Ljava/lang/foreign/MemorySegment; has no Code attribute
	at java.lang.foreign.SegmentAllocator.allocate(SegmentAllocator.java:318)
	at org.elasticsearch.nativeaccess.jdk.JdkPosixCLibrary.<clinit>(JdkPosixCLibrary.java:123)
	...
```

## Failures with `-Jit on` (6 of 20) — STILL OPEN, see update below

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

Two symptom clusters:

- **Wrong quantile/count values** (`testMidPointRule`, `testSingletonAtEnd`,
  `testFewRepeatedValues`): computed values diverge from real-JDK-expected
  values. `testFewRepeatedValues`'s `2.1E-322` (a subnormal double near
  zero) instead of `3000.0` is a strong signal of reading
  garbage/uninitialized memory as a double rather than a simple
  off-by-one arithmetic error.
- **Garbage array index** (`testSorted`, `testMonotonicity`): the *specific*
  bad index is NOT stable across builds — an initial build (base SHA
  `df1650e1`) hit the identical value `7598259162470311681`
  (`0x6972707372666701`) in both methods; after merging forward to
  `c9e68f12`/`51b8acfc` and rebuilding, the same two methods instead hit
  `1106770591` and `-1` respectively. The *pattern* is stable (`testSorted`
  and `testMonotonicity` always throw `ArrayIndexOutOfBoundsException` at
  the same call site), but the value itself is clearly garbage/uninitialized
  rather than a deterministic off-by-one, and is sensitive to unrelated
  code changes elsewhere in the VM — consistent with reading a stale/
  uninitialized slot (register, stack slot, or field) whose contents shift
  with codegen/layout changes rather than with anything `SortingDigest`
  itself computes.

## Failures with `-Jit off` (6 of 20) — FIXED, see update below

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

The interpreter-level `NoSuchMethodError` has a precise VM-side signal in
stderr:

```text
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError method="java/lang/Object.get(I)D" caller="org/elasticsearch/tdigest/Dist.quantile(DILjava/util/function/Function;)D @pc=88"
```

## Why this wasn't caught before

The original `System$1.findNative` crash aborted the process before any
of `SortingDigestTests`' 20 tests ran (`rc=139`, 0 tests parsed). Once that
crash was fixed, the tests actually execute and these two independent,
pre-existing residuals became visible for the first time under this class.

## Update 2026-07-10: `-Jit off` cluster FIXED (dev `3e489a1c`)

**Root cause**: `allocate_lambda_proxy` in `vm/src/runtime/invokedynamic.rs`
pops a lambda's captured values off the Java operand stack into a raw,
GC-invisible Rust `Vec<Value>` *before* allocating the proxy object. If the
first `try_alloc_object` call fails under memory pressure (young-gen
exhaustion — very plausible deep into a heavy test like `testMonotonicity`,
which inserts 100,000 values into one digest before sweeping 10,001 quantile
points), the fallback path calls `maybe_gc_forced_pub` — an actual GC cycle —
while the `captures` Vec holds zero GC roots. A moving/copying collector can
relocate a captured object during that window (it stays alive via its real
owner, e.g. `SortingDigest.values`, but gets evacuated to a new address); the
STALE pre-move `ObjectRef` — still sitting in the untracked `captures`
Vec — then gets written straight into the new proxy's field with no re-read.

Every later dispatch through that one proxy instance reads the captured
field, finds the vacated old address, and resolves whatever garbage/zeroed
class id header now sits there instead of the real receiver class. In this
case the vacated slot decoded as `ClassId(0)` / `java.lang.Object`, so
`org/elasticsearch/tdigest/arrays/TDigestDoubleArray::get` (a bound
interface method-reference lambda, `values::get`) dispatched as
`java/lang/Object.get(I)D` and threw a clean `NoSuchMethodError` in the
interpreter. (The log4j `ReusableParameterizedMessage` `ClassCastException`s
in the original 6-failure list turned out to be an unrelated, already-fixed
issue — they no longer reproduce on current dev independent of this fix;
likely fixed by an intervening, unrelated commit between the doc's base SHA
and dev `4b08ffad`.)

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

**Verification**:
- The exact `NoSuchMethodError` repro (`SortingDigestTests`, `-Jit off`,
  `-Start 154 -Count 1`) no longer throws it; of the original 6 `-Jit off`
  failures, all 6 are gone (1 was this bug, 5 were the unrelated
  already-fixed log4j issue).
- `cargo test -p cratonvm-vm --lib`: 2175 passed both before and after the
  fix (11 pre-existing, unrelated failures — `runtime::lock_order::tests::*`
  require a debug build's `debug_assert!` and fail identically under
  `--release` on unmodified dev tip; `vm_init::tests::ensure_system_streams_creates_objects`
  and `real_jdk_mode_registers_fewer_natives` are drifting-baseline
  assertions, also confirmed failing identically on unmodified dev tip via
  `git stash`).
- A standalone reflection-based driver
  (`RealTDigestRepro` — real `MemoryTrackingTDigestArrays` +
  `SortingDigest.create`, 100K adds + 2M repeated `quantile()` calls under a
  128 MB heap to force GC churn) shows zero mismatches on the merged+rebuilt
  binary.
- **Full-suite verification via the standard ES suite runner is currently
  blocked** by an unrelated, already-tracked, suite-wide bootstrap bug that
  landed in `dev` in the same batch of commits this fix was merged on top
  of — see
  [`ES-FAIL-20260710-randomizedrunner-classmodel-modifier-stringjoiner-cce.md`](ES-FAIL-20260710-randomizedrunner-classmodel-modifier-stringjoiner-cce.md)
  (`ClassCastException` in `Modifier.toString`/`StringJoiner.add`, breaks
  `RandomizedRunner.<init>` for every ES test class before any test method
  runs). Once that doc is fixed, re-run this doc's exact `-Jit off` repro
  end-to-end through the suite runner to confirm at the JUnit-report level
  (the standalone/reflection-based verification above already confirms the
  underlying VM mechanism is fixed).

### New finding: `testMonotonicity` is now extremely slow under `-Jit off`

With the `NoSuchMethodError` no longer aborting it early, `testMonotonicity`
(100K adds + 10,001-point quantile/cdf monotonicity sweep, all fully
interpreted since it previously failed before completing) takes **at least
13+ CPU-minutes** and did not finish within a 1-hour wall-clock budget in
this session. Live `gdb` backtraces taken ~10s apart during a run showed
the VM thread at *different* locations each time (not stuck at one PC), so
this is **not a livelock** — it is genuinely making forward progress, just
very slowly. Both samples were inside per-call method-resolution machinery:
`cratonvm_classloading::class::find_method_recursive` /
`resolved_private_invokevirtual_target` (one sample caught mid
`HashMap::insert` → `reserve_rehash`), and `try_lambda_dispatch` →
`invoke_or_native` → `jit_method_calls_native_shadowed`. This is worth a
follow-up look — possibly a cache that isn't being reused/is growing
unboundedly across repeated lambda dispatches through the same call site —
but is a **separate performance issue**, not a correctness bug, and out of
scope for this fix. Filed as a lead for whoever picks this up next; not yet
a standalone doc.

## Update 2026-07-10: `-Jit on` investigation — STILL OPEN

Extensive investigation this session did **not** find the root cause of the
`-Jit on` garbage-index cluster. Findings, to save time for the next pass:

- **Confirmed a genuinely separate bug from the `-Jit off` one.** The
  `-Jit off` fix above (landed dev `3e489a1c`) was verified to have **zero
  effect** on the `-Jit on` failures — rebuilt and re-ran the exact same
  `-Jit on` repro after the fix; all 6 failures reproduce identically
  (same test methods, same `AssertionError`/`AIOOBE` shapes).
- **The affected methods do get JIT-compiled.** A `CRATONVM_DBG_JITC=1`
  trace of the failing run confirms `org/elasticsearch/tdigest/Dist.quantile(DILjava/util/function/Function;)D`
  (the private method with the `lo`/`hi` index arithmetic + two
  `Function.apply` calls) and
  `org/elasticsearch/search/aggregations/metrics/MemoryTrackingTDigestArrays$MemoryTrackingTDigestDoubleArray.get(I)D`
  (the actual bounds-checked array read at the bottom of the call chain)
  both get `bg-compile`d during the failing run. `SortingDigest.compress()`/
  `.cdf(D)D`/`.quantile(D)D` also compile. `Dist.quantile(double,
  TDigestDoubleArray)` (the one that actually contains the `invokedynamic`
  that builds the lambda) does **not** compile — methods containing
  `invokedynamic` are rejected upstream by `jit_scan` (see
  `jit/src/ir.rs` comment near line 2607) — so lambda *creation* always runs
  interpreted (going through the same, now-fixed, `allocate_lambda_proxy`),
  but lambda *dispatch* (`Function.apply` from inside the JIT-compiled
  `Dist.quantile(D,I,Function)`) can run JIT-compiled.
- **`jit_invoke_virtual_mic` is never hit for this call site.** A targeted
  trace gated on `method_name == "apply"` inside
  `jit_invoke_virtual_mic` (`vm/src/jit/helpers.rs`) recorded **zero**
  hits across a full failing run. Dispatch instead goes through
  `jit_invoke_dispatch` (same file), whose `invoke_kind 0 | 2` (virtual/
  interface) arm calls `NativeContextImpl::invoke_virtual`
  (`vm/src/vm/vm_exec.rs` ~5995).
- **`invoke_virtual` already has lambda-proxy-aware dispatch AND its own
  staleness-recovery mechanism.** Line ~6002-6014 of `vm_exec.rs` calls
  `self.shared.heap.load_and_forward(receiver)` then
  `recover_stale_lambda_receiver_from_native_pins(...)` *before* checking
  `lambda_proxies` — i.e. the CratonVM authors already anticipated and
  built a recovery path for a stale-lambda-receiver class of bug in exactly
  this function. That recovery mechanism is keyed off `native_pins`, so it
  cannot help with the specific staleness this session found and fixed
  (a stale reference baked directly into a *heap field*, not sitting in a
  transient pin) — but its existence is a strong hint that a *similar*
  staleness bug affecting this exact dispatch path, not caught by the
  existing recovery, is a plausible next place to look.
- **No minimal standalone repro achieved**, despite several attempts:
  - A synthetic `Function<Integer,Double>` over a bound interface method
    reference (`values::get`) with heavy GC churn (2M iterations, 32 MB
    heap) did not reproduce.
  - A synthetic method mirroring `Dist.quantile`'s exact int-local shape
    (`lo`/`hi` computed via `Math.floor`, two `Function.apply` calls) over
    a growing array, hot-looped 3M times under a small heap, did not
    reproduce.
  - A driver using the **real** ES classes
    (`MemoryTrackingTDigestArrays` + `SortingDigest` via reflection),
    100K adds + 2M repeated `quantile()` calls under a 128 MB heap, did
    not reproduce.
  - A closer driver matching `TDigestTests.testMonotonicity()`'s exact
    bytecode shape (100K adds, then a `while (q <= 1) q += 1e-4` sweep
    calling both `quantile()` and `cdf()` with string-concatenation debug
    messages built each iteration, wrapped in a loop of fresh digests) was
    catastrophically slow under `-Jit on` too (>90s for round 0 alone,
    no crash) — plausibly related to the `testMonotonicity` performance
    finding above rather than to the AIOOBE bug, but not conclusively
    separated.
  - Reproducing therefore likely needs the *exact* combination present in
    the real `SortingDigestTests` run — something about JIT tiering
    timing, code layout, or a still-unidentified precondition that these
    synthetic drivers didn't hit.
- **Working theory, unconfirmed**: since JIT-compiled `Dist.quantile(D,I,
  Function)` never triggers `jit_invoke_virtual_mic`'s dedicated lambda-proxy
  fast path (0 hits observed) and instead always takes the colder
  `jit_invoke_dispatch` → `invoke_virtual` route, the bug is most likely
  either (a) a receiver-decode issue specific to `jit_invoke_dispatch`'s
  `invoke_kind 0 | 2` arm (`vm/src/jit/helpers.rs` ~4292-4317) when handed a
  raw register/stack value for a lambda-proxy receiver under JIT calling
  conventions, or (b) a caller-side register/stack-slot corruption in the
  JIT-compiled `Dist.quantile(D,I,Function)` itself across the call boundary
  into the dispatch helper — i.e. a value the caller needs *after* the call
  (the other of `lo`/`hi`, or `n`) gets clobbered by something inside the
  call machinery and isn't correctly restored. Neither is confirmed;
  distinguishing them needs either a disassembly-level look at the
  generated code for `Dist.quantile(D,I,Function)`'s compiled body, or a
  reproducible standalone case (still missing — see above).

## Next steps

- `-Jit on` cluster: **STILL THE PRIORITY** — see the working theory above.
  A disassembly of the JIT-compiled `Dist.quantile(DILjava/util/function/Function;)D`
  body (dump via whatever `CRATONVM_DBG_JIT_*` flag exposes generated
  x86-64, or attach `gdb` mid-run and disassemble the cached entry pointer)
  is probably the fastest remaining path, since black-box repro attempts
  have not converged.
- `testMonotonicity` slowness (new, `-Jit off`): investigate whether
  `find_method_recursive`/`resolved_private_invokevirtual_target`'s
  resolution result is being cached at all for private-method dispatch
  reached via lambda proxies, and if so why the cache doesn't appear to be
  hit (repeated expensive `HashMap::insert`/`reserve_rehash` observed via
  live `gdb` sampling during a single test method's execution).
- Blocking suite-wide bug
  ([`ES-FAIL-20260710-randomizedrunner-classmodel-modifier-stringjoiner-cce.md`](ES-FAIL-20260710-randomizedrunner-classmodel-modifier-stringjoiner-cce.md)):
  once fixed, re-verify this doc's `-Jit off` repro end-to-end through the
  normal suite runner (currently only verified via standalone drivers that
  bypass `RandomizedRunner`).
- Not yet checked whether other `*DigestTests` classes in
  `libs/tdigest` (e.g. `AVLTreeDigestTests`, `MergingDigestTests`) hit the
  same `-Jit on` residual — worth a quick category sweep once it's fixed.

## Final update 2026-07-10 (retirement)

The `-Jit on` cluster was **root-caused and fixed** on branch
`fix/es-tdigest-jiton-20260710`. It was never a dispatch/receiver bug: the
C2/IR backend (`jit/src/ir_lower.rs`) zero-initialises its `node_slot` table
and `slot_of()` silently returned that `0` default for an SSA node the
scheduler never placed in an emitted block. Compiling
`java/util/DualPivotQuicksort.insertionSort([DII)V` (IR-eligible pure
compute) left the pc17 `ArrayLoad(Double)` feeding a GVN-collapsed loop phi
unallocated, so the `a[i+1] = ai` store read `[rbp - 0]` — the saved caller
RBP — as the double value and wrote **stack addresses into the array**
(subnormal doubles ~6e-310 with pointer bit patterns; the insertion shift
then smeared them). Every large `Arrays.sort(double[])` under `-Jit on` was
silently mis-sorted; the "garbage index" AIOOBEs and all the wrong
quantile/count values in this doc were downstream of sorting on corrupted
data. Fix: latch any unallocated-slot readback and bail the lowering to the
single-pass backend.

Diagnosis chain, for the record: `CRATONVM_JIT_BISECT_SKIP` narrowed the
failures to `DualPivotQuicksort.sort`'s compiled callees; a standalone
`Arrays.sort(double[10000])` driver reproduced in 30 s; per-callee bisect
plus `CRATONVM_JIT_IR_FP=0` isolated the IR backend; a hardware watchpoint
(gdb, ASLR off) caught the poisoning `movsd [rax+rcx*8+0x28], xmm0` writing
`rbp+0x2E0` into `a[0]`; the annotated disasm showed the `movsd xmm0,[rbp]`
load; an env-gated `slot_of` probe named the unallocated node.

Three companion soundness gaps found on the way are fixed in the same
commit: the OSR dead-local mask ignored XMM-resident locals (garbage seeded
into coalesced double locals on OSR entry at `sort`'s bci 575/599 — this was
the doc's original "OSR-reuse" lead); the raw self-recursive CALL path
pushed a phantom RAX "return value" for void methods; and the precise-maps
innermost-RBP mirror was never restored when a compiled callee returned into
Rust dispatch helpers.

The prior working theory in this doc (receiver-decode in
`jit_invoke_dispatch` / caller-side register clobber) is REFUTED — the
dispatch machinery was fine; per-callee-skip experiments only appeared to
implicate call linkage because skipping a callee also removed its
IR-compiled artifact.

Verification on the merge of `fix/es-tdigest-jiton-20260710` +
dev `395f7246`:

- Standalone sort repros (10K-120K doubles): corrupted → ALL OK.
- `SortingDigestTests -Jit on`: 19/20 method passes on the fix binary
  (pre-merge run, 50 s wall) with only the then-open `testMonotonicity` NSME;
  on the merged tip the NSME is gone and every executed method passes — the
  class-level FAIL that remains is `testMonotonicity` abandoned at the
  RandomizedRunner suite timeout (it did not complete even with the budget
  raised to 3,600 s; run `.suite-long3`, wall 3,611 s, "Tests run: 18,
  Failures: 2" where both entries are the timeout wrappers). That is the
  split-off PERFORMANCE issue, not a correctness failure.
- `SortingDigestTests -Jit off`: the NSME-cluster fix (`3e489a1c`) was
  verified at class level in the earlier session (all original 6 failures
  gone). A runner-level re-run on the merged tip is blocked by the SAME
  slowness: interpreted `testMonotonicity` needs 13+ CPU-minutes, beyond the
  runner's default 580 s suite budget (two attempts at a raised budget were
  killed by host-wide OOM during a post-reboot load spike — the box was
  running 13+ concurrent sessions; not a VM failure).
- `cargo test -p cratonvm-vm --lib`: 2174 passed, 12 failed — every failure
  confirmed identical on unmodified dev `395f7246` (the 11 from this doc's
  earlier baseline + `bouncycastle_crypto_hotpath_carveout_keeps_math_ec_banned`,
  which is new origin/dev drift, verified by running it on a detached
  `395f7246` checkout).

The `testMonotonicity` `-Jit on` `NoSuchMethodError: java/lang/Object.get(I)D`
that surfaced once the AIOOBE was gone (stale pointer baked into the lambda
proxy's capture field; no forwarding pointer; fresh field re-read returns
the same dead address — see the `[lambda-nsme-diag]` gated diagnostic landed
in `try_lambda_dispatch`) stopped reproducing after merging dev `395f7246`
and is credited to the concurrent GC-audit work (`35436546` region, not
bisected). The remaining suite-visible failure is the pre-documented
`testMonotonicity` slowness, now a suite-timeout at the default
`-Dtests.timeoutSuite=580000!` — tracked in the split-off doc named in the
Status line.
