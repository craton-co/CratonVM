# Bug: Bouncy Castle suite — separate real fails from harness artifacts

The `test-infra/bc-suite-3way.sh` / `run-vm-comparison.sh` BC results mix genuine VM
failures with harness artifacts. Clean this up so the BC pass/fail picture is trustworthy.
Independent of the other `continue_prompt_*` bugs. (math-ec is excluded — it's blocked by
the `org/bouncycastle/` JIT ban, handled by the separate BC-EC codegen effort.)

## Items
- `crypto-prng-regression`: now **PASSES** on CratonVM (rc=0, ~59s). Confirm + record.
- `util-encoders`: harness shows FAIL, but it **passes when run cleanly** ("OK (15 tests)").
  Root cause is the harness classpath: under MSYS/Git-bash the `;`-separated `-cp` is
  mangled so `junit.textui.TestRunner` isn't found (fails identically on HotSpot — so NOT a
  VM bug). **Fix the harness** (write the classpath to a Java `@argfile`, or invoke the
  Windows `java.exe` with a properly-quoted native cp), then re-measure.
- `pqc-crypto-regression`: was TIMEOUT, now **completes-then-fails**. Capture the actual
  failure on a clean run — is it a real BC assertion or a wrong CratonVM intrinsic? (Run
  `org.bouncycastle.pqc.crypto.test.RegressionTest` directly and read the first failing
  line.)
- `crypto-regression`: OOMs at `-Xmx1g` on HotSpot too — not a fair test at 1g. Bump heap
  (e.g. 4g) or mark inconclusive.

## Repro
```
BC=apps/_test-suites/bc-java
CP="$BC/core/build/classes/java/main;$BC/core/build/classes/java/test;$BC/core/build/resources/main;$BC/core/build/resources/test"
JUNIT="$TEMP/junit-3.8.2.jar"
CV=target/release/cratonvm.exe; JDK="C:/Program Files/Java/jdk-25"
# SimpleTest-style (no junit): prints "<Name>: Okay"
$CV --java-home "$JDK" -cp "$CP" org.bouncycastle.crypto.prng.test.RegressionTest
$CV --java-home "$JDK" -cp "$CP" org.bouncycastle.pqc.crypto.test.RegressionTest
# JUnit-textui (needs junit jar; mind the MSYS cp mangling): "OK (n tests)"
$CV --java-home "$JDK" -cp "$CP;$JUNIT" junit.textui.TestRunner org.bouncycastle.util.encoders.test.AllTests
```
Compare each against `C:/Program Files/Java/jdk-25/bin/java.exe` with the same cp.
Memory: `reference_cross_vm_comparison_harness`.

## RESOLVED — 2026-06-04 (branch `fix/bc-suite-artifacts`)

Harness fixed in both `test-infra/bc-suite-3way.sh` and `test-infra/run-vm-comparison.sh`:

1. **Classpath mangling → fixed.** Added `export MSYS2_ARG_CONV_EXCL='*'` +
   `export MSYS_NO_PATHCONV=1` near the top of both scripts. MSYS/Git-bash was
   auto-converting the `;`-separated Windows `-cp` as a POSIX path list, dropping the
   junit jar so `junit.textui.TestRunner` became "not found" (fails identically on
   HotSpot ⇒ harness artifact, not a VM bug). The `@argfile` idea does **not** work for
   cratonvm (its launcher has no `@`-file handling) and cratonvm does not reliably honor
   the `CLASSPATH` env var under MSYS — both tested — so disabling arg path-conversion is
   the portable fix that keeps the `-cp` verbatim for every VM.
2. **Unfair heap → fixed.** Heap is now a per-suite `|` field applied uniformly across all
   VMs: `crypto-regression`=4g, `pqc-crypto-regression`=2g, the rest 1g. TSVs/render gained
   a `heap` column.

Per-item verdicts (measured with a stable copy of `target/release/cratonvm.exe`, JDK-25,
MSYS guard active; a concurrent main-checkout build was contending for CPU during some runs):

| Suite | CratonVM | HotSpot | Verdict |
|---|---|---|---|
| `crypto-prng-regression` | **PASS** rc=0 ~72s, "All tests successful." | — | Genuine pass. Confirmed. |
| `util-encoders` | **PASS** "OK (15 tests)" rc=0 | **PASS** "OK (15 tests)" | Harness artifact (cp mangling). Fixed. |
| `pqc-crypto-regression` | **TIMEOUT** (0 test output in 260s) | PASS 2s (2 tests) | Genuine VM throughput limit — see below. |
| `crypto-regression` @1g | — | **OOM** rc=1 ~107s | Unfair at 1g (OOM on HotSpot too). |
| `crypto-regression` @4g | **TIMEOUT** 300s (no OOM) | PASS rc=0 130s* | 4g is fair; cratonvm slow (throughput). |

\* HotSpot @4g completes but the `LEA` test throws
`StringIndexOutOfBoundsException: Range [0, -1)` — a **pre-existing BC/JDK-25 suite bug**
that reproduces on HotSpot, i.e. NOT a CratonVM fault.

**`pqc` is NOT "completes-then-fails" and NOT a BC assertion or a wrong intrinsic.**
A `--stack-dump-on-timeout` thread dump shows the main thread grinding, in the interpreter,
through SPHINCS-256 signing:
`Sphincs256Test.performTest → SPHINCS256Signer.crypto_sign → Horst.horst_sign →
HashFunctions.hash_2n_n → Permute.chacha_permute → permute → rotl`. The ChaCha permutation
(invoked an enormous number of times by `horst_sign`) runs at interpreter speed and never
reaches the first test's print before the timeout (HotSpot JITs it and finishes in 2s).
So `pqc-crypto-regression` and `crypto-regression`-on-cratonvm are the **same class of
issue**: BC crypto kernels are too slow under the interpreter / JIT isn't accelerating them
— a performance problem for a separate JIT/throughput effort, **distinct from a correctness
failure**, and now correctly surfaced by the harness rather than masked as a heap fault.

## Throughput root-cause + partial fix — 2026-06-04 (branch `fix/bc-suite-artifacts`)

The interpreter-speed is the `org/bouncycastle/` JIT ban (RBC.1 in
`vm/src/jit/skip_list.rs`) — it forces every BC method into the interpreter. Lifting it
(`CRATONVM_JIT_ALLOW_PACKAGES=org/bouncycastle/`) makes the hot crypto kernels JIT, but
exposes BC JIT miscompiles. Findings:

- **FIXED (commit `249ebb8` on the branch): cached-JIT deopt double-pop → value-stack
  underflow.** `execute_jit_call` (`vm/src/runtime/interpreter.rs`) pops the callee args,
  runs the compiled method, and treats a return of `i64::MIN` as the deopt sentinel →
  returns `CacheMiss` → the slow path re-pops the args → operand stack underflows. The
  in-band `i64::MIN` sentinel **collides with a legitimate `i64::MIN` `long` return**:
  `Pack.bigEndianToLong` (SHA-512 byte→long, per-word under SPHINCS-256) returns
  `0x8000_0000_0000_0000` for some words → false deopt → double-pop → panic at
  `value_stack.rs` `pop_int_unchecked` ("len 24, index 18446744073709551615") in
  `LongDigest.processWord`'s `lastore`. Isolated via `CRATONVM_JIT_BISECT_SKIP=org/
  bouncycastle/util/Pack.bigEndianToLong` (skipping just that method removes the crash).
  Fix = save the popped slots and restore them before `CacheMiss`. Verified: with the ban
  lifted, pqc no longer underflows.
- **STILL OPEN (keeps the ban in place):**
  1. A **wrong-result** miscompile — with the ban lifted, `Sphincs256` throws
     `Exception` (bad value, not a crash). Separate BC JIT codegen bug, not yet isolated.
  2. **Deopt-thrash perf** — the `i64::MIN`-as-deopt sentinel collision means an
     `i64::MIN`-returning hot method deopts+re-executes on every such return. The proper
     fix is an out-of-band deopt flag (genuine deopts set it; a real `i64::MIN` return does
     not) so `b'J'`/`b'D'` returns aren't misread — more invasive, deferred since the ban
     stays anyway.

So the underflow CRASH is fixed, but the `org/bouncycastle/` ban is **NOT lifted** — full
BC-JIT throughput needs the wrong-result miscompile fixed too. The branch fix is a genuine
latent-crash fix (any `long`/`double`-returning JIT method that returns `i64::MIN` would
double-pop), independent of BC.
