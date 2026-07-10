# ES FAIL - libs/tdigest org.elasticsearch.tdigest.SortingDigestTests correctness residual

Status: OPEN

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
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category others -Jit on -Vm craton -ElasticsearchRoot "<worktree>/apps/elasticsearch" -WorkDir "<worktree>/apps/elasticsearch-suite-runner/.suite-<run>" -Exe <exe> -JdkHome /usr/lib/jvm/java-21-openjdk-amd64 -TimeoutSec 600 -RunName repro-tdigest-0249530511 -ModeName repro-tdigest-0249530511 -Start 154 -Count 1

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

## Failures with `-Jit on` (6 of 20)

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

## Failures with `-Jit off` (6 of 20) — different tests, different bugs

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

`Dist.quantile(double, int, Function)` invokes a functional-interface
`get(int)` method (returning `double`) on its `Function`-typed argument.
CratonVM resolves the receiver's runtime class as plain `java.lang.Object`
instead of the actual lambda/functional-interface implementation class,
so the interface method lookup fails with `NoSuchMethodError` rather than
dispatching to the lambda body. This looks like a receiver-identity loss
specific to the no-JIT interpreter path for this call shape (lambda passed
as a generic `java.util.function.Function`-typed parameter, invoked via a
narrower functional-interface method) — not yet root-caused further.

The repeated `ClassCastException: java.lang.Object cannot be cast to
ReusableParameterizedMessage` looks like a separate, log4j-recycling-related
bug (log4j's message object pool returning/holding a plain `Object`
instead of the expected reusable message instance) that only manifests
with JIT off; not yet root-caused.

## Why this wasn't caught before

The original `System$1.findNative` crash aborted the process before any
of `SortingDigestTests`' 20 tests ran (`rc=139`, 0 tests parsed). Once that
crash was fixed, the tests actually execute and these two independent,
pre-existing residuals became visible for the first time under this class.

## Next steps

- `-Jit on` cluster: investigate the garbage-index bug first — it
  reproduces at the same two call sites (`testSorted`, `testMonotonicity`)
  across builds even though the actual bad index value changes per build,
  the strongest lead for a stale 64-bit slot / register reuse. Fixing it
  may also resolve or clarify the wrong-quantile-value failures in the
  same mode.
- `-Jit off` cluster: root-cause why `Dist.quantile`'s `Function`-typed
  parameter resolves to receiver class `java.lang.Object` at the
  interpreter's invokeinterface dispatch — likely a lambda/functional
  interface identity bug specific to the no-JIT path. The log4j
  `ReusableParameterizedMessage` `ClassCastException` may be a distinct,
  unrelated bug (or a downstream effect of the same identity-loss
  mechanism) — verify independently.
- Not yet checked whether other `*DigestTests` classes in
  `libs/tdigest` (e.g. `AVLTreeDigestTests`, `MergingDigestTests`) hit the
  same two residuals — worth a quick category sweep once either is fixed.
