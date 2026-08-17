# bc-java PQC: not a hang, and the SHA-256 hypothesis named the right kernel behind the wrong class

## Status
**IN PROGRESS** on `fix/bcjava-pqc-lms-throughput-20260817`. Every measurement
below is final; the fix-result table is pending its verification run.

Filed originally as: *`pqc.crypto.lms.AllTests` / `pqc.crypto.test.AllTests`
exceed even a 10x timeout — confirmed CPU-bound, not deadlocked.*

This page also owns a third class, handed over by the bc-java residual sweep
(fixed-suite-bugs/bc-java/bug-bcjava-residual-suite-failures-20260816.md,
Residual 0): `pqc.jcajce.provider`, 271 s on HotSpot.

## The original report was right about what it declined to claim

It refused to call this a deadlock, and that was correct. Nothing here is
blocked; the 99.7–99.9% CPU evidence it recorded stands, and the classes
terminate with correct results given a large enough budget.

What it could not see is *why* the work costs what it does — and its leading
hypothesis pointed at a class this workload never loads.

## `pqc.crypto.test.AllTests` decomposed — there is no dominant contributor

The original page decomposed `lms.AllTests` and left this one as "not yet
identified". Timing all 25 constituent classes individually under HotSpot:

| class | HotSpot ms | class | HotSpot ms |
|---|---|---|---|
| `SQIsignTest` | 165 954 | `MayoTest` | 16 148 |
| `UOVTest` | 125 077 | `CrystalsDilithiumTest` | 12 506 |
| `SNTRUPrimeTest` | 112 035 | `FalconTest` | 10 359 |
| `AIMerTest` | 111 206 | `MLKEMTest` | 9 993 |
| `NTRULPRimeTest` | 64 630 | `SABERVectorTest` | 6 404 |
| `MLDSATest` | 51 683 | `LMSTest` | 5 498 |
| `HQCTest` | 43 493 | `SimpleTestTest` | 3 100 |
| `FrodoKEMVectorTest` | 30 835 | `SmaugTTest` | 1 872 |
| `SDitHTest` | 30 707 | `XWingTest` | 1 545 |
| `NTRUTest` | 29 846 | `NTRUParametersTest` | 1 526 |
| `HSSTest` | 17 303 | `HAETAETest` | 1 523 |
| | | `PqcMalformedInputTest` | 1 336 |
| | | `PqcUnmappedAlgorithmOidTest` | 1 084 |
| | | `PublicKeyLengthValidationTest` | 798 |

(Measured on a contended shared host — read as a ranking, not a benchmark.
Per-class JVM start-up is included, which is why the column sums to more than
the 195 s the suite takes as a single JVM.)

**This answers the original page's first next-step, and the answer is
negative.** Unlike `lms.AllTests` — where `HSSTests` is 15.9 of 16 seconds —
this suite has no single owner. Ten classes across unrelated PQC families
(isogeny, multivariate, lattice, code-based, MPC-in-the-head) each contribute
30–166 s. There is no one class to target, and no fix aimed at a single
algorithm can move this suite much.

## The SHA-256 hypothesis: right kernel, wrong class

The original page proposed checking "whether CratonVM has an
accelerated/intrinsic SHA-256 implementation equivalent to HotSpot's
(`sun.security.provider.SHA2` typically gets a native/intrinsic fast path)".

Three facts, in the order that matters:

1. **LMS/HSS never touches `java.security.MessageDigest`.** That package's own
   `DigestUtil.createDigest`
   (`core/src/main/java/org/bouncycastle/pqc/crypto/lms/DigestUtil.java`) does
   `return new SHA256Digest();` — BouncyCastle's pure-Java digest, constructed
   directly. No provider lookup, no JCA.
2. **CratonVM's native SHA-256 is real and was never on this path.** It exists
   (`native-builtins/src/jca/message_digest.rs`) and is fast. It sits behind
   `MessageDigest.getInstance`, which this workload does not call.
3. **HotSpot has no intrinsic for BouncyCastle's class either.** Its
   `@IntrinsicCandidate` is on `sun.security.provider.SHA2.implCompress0`, not
   on `org.bouncycastle.crypto.digests.SHA256Digest`. HotSpot's 16 s is plain
   C2-compiled Java.

So "CratonVM lacks a SHA-256 fast path where HotSpot has one" is **false as
stated** — neither VM has one here. But the instinct underneath it — *HSS/LMS
signing is essentially "hash a very large number of times", and that kernel is a
concrete, independently fixable target* — is exactly right, and the profile
confirms it.

## What the JIT-enabled rerun actually showed

The original page's second next-step was to drop `--nojit`. Done, on the
isolated kernel (BouncyCastle `SHA256Digest`, 64-byte message, 200 000
iterations, uncontended host, hot loop in a callee so OSR is not refused):

| arm | ns/op | vs HotSpot |
|---|---|---|
| HotSpot 25 | **815** | 1x |
| CratonVM **+JIT** | **30 366** | **37x** |
| CratonVM `--nojit` | **1 396 923** | **1 714x** |

`--nojit` is ~46x worse than JIT-on, so the harness's forced-interpreter mode
does account for most of the reported budget overrun — the original page's
suspicion was correct. But 37x with the JIT fully engaged is the real number,
and nothing is failing to compile:

```
[cratonvm] JIT method stats: 15 distinct methods tracked, 14 ever invoked
  | still-interpreted=1 c1=0 full-profile=0 c2=14
  | c1_threshold=500 hot_but_stuck_in_interpreter=0
      (of which ineligible-by-policy=0, compile-failures=0)
```

**`hot_but_stuck_in_interpreter=0`.** This is where this page parts company with
its commons-math sibling
(fixed-suite-bugs/bug-commonsmath-accuratemathtest-psquarepercentiletest-interpreter-throughput-cliff-20260816-FIXED.md),
whose entire cause was three opcodes with no codegen arm. Here every hot method
reaches C2, and the compiled code is simply much slower than C2's.

## Where the 37x lives

`perf record -F 999 -g`, same kernel, ZGC (the default collector since
2026-08-10):

| | share |
|---|---|
| VM binary (runtime helpers) | **50.8%** |
| `[JIT]` compiled code | **47.0%** |

and within the helper half:

| symbol | % of run |
|---|---|
| `VmHeap::is_object_address` | 11.5 |
| `ZObjectStarts::contains` | 10.4 |
| `jit::helpers::jit_getfield` | 9.0 |
| `try_jit_site_cached_native_dispatch` | 2.6 |
| `vm_exec::safe_native_call_impl` | 2.3 |
| `value::single_thread_guard_enabled` | 1.9 |
| every individual JIT'd code address | < 0.5 each |

Profiling the **real `HSSTests` workload** rather than the microbench gives
55.7% / 43.5% with the same helper chain at 31.8%, so the microbench is a
faithful proxy and the conclusion transfers.

Two distinct costs are visible, and only one belongs to this page:

* **~31% is a whole-VM JIT issue, not a bc-java one.** Every compiled `getfield`
  takes the checked helper, which runs a full heap-membership validation per
  field read. Split out to
  known-issues/jit/every-jit-getfield-takes-the-helper-because-the-guarded-inline-check-always-fails-20260817.md
  — it costs every field-dense compiled workload on every collector, and fixing
  it has ZGC coloured-pointer hazards this page should not carry.
* **~47% is the SHA-256 round schedule itself**, running as ~2 500 bytecodes per
  64-byte block. That is what this page can fix, and it is exactly the target
  the original report pointed at.

## The fix

Register `org.bouncycastle.crypto.digests.SHA256Digest.processBlock()V` as a
native `Intrinsic`.

This is not a new mechanism — it is the seam the repository already uses for
**Blake2s** (`compress`), **Keccak** (`KeccakPermutation` / `KeccakAbsorb`),
**GOST3411** and **Whirlpool** (`processBlock`). SHA-256 was the one digest
missing from that set, and the one LMS/HSS needs.

Why `processBlock` is the right seam:

* It is a `protected`, argument-less leaf that calls nothing out. Every input is
  a field of the receiver (`H1..H8`, `X`), so marshalling is bounded, and the
  big-endian decode stays on the Java side in `GeneralDigest.processWord` where
  it cannot disagree with the kernel.
* The kernel reproduces `processBlock`'s exact post-state — including the
  expanded message schedule left in `X[16..64]` and the cleared `X[0..16]` — so
  `copy()`, `reset(Memoable)` and `getEncodedState()` observe what they would
  have. Buffering, padding, length encoding and digest output stay real
  bytecode.
* `SHA256Digest` has no subclass in BouncyCastle, so the superclass walk in
  `intercept_force_registered_native` cannot divert another digest's
  `processBlock` here.

Two supporting changes, both needed for the intrinsic to be worth having:

* **Bulk `int[]` accessors on `NativeContext`**, with the VM's
  `copy_nonoverlapping` override, mirroring the existing byte/char twins. The
  per-element path costs a virtual dispatch plus a `Value` box per word; 64 of
  those per block would have eaten the win.
* **A per-`ClassId` slot cache** instead of `get_field_by_name`. That helper
  takes the class-manager read lock and walks the class hierarchy by name on
  every call, and `processBlock` touches ten fields — eighteen lock round-trips
  per block would have measured as "the intrinsic did not help", for a reason
  with nothing to do with SHA-256.

Correctness is pinned to **published FIPS 180-4 vectors** (empty, `"abc"`, the
two-block case, a multi-block case) rather than to a round trip against itself:
a mis-transcribed rotate still round-trips against itself perfectly.

## Results

*Pending the verification run: the invocation census proving the native actually
served the calls, BouncyCastle's own `SHA256DigestTest`/`HMacTest`, the four LMS
suite classes against HotSpot, and the interleaved kernel A/B.*

## What is NOT claimed

**This does not close the interpreter-vs-HotSpot gap and was never going to.**
It removes one kernel from the bytecode path. What remains is the ordinary gap
between this JIT and C2 on call-dense code, plus the `getfield` helper cost,
which has its own page.

**It does almost nothing for `pqc.crypto.test.AllTests`.** That suite's cost is
spread across ten classes in unrelated PQC families, most of them SHAKE/Keccak
and polynomial arithmetic rather than SHA-256. The decomposition table above is
the evidence, and it is why that suite is not claimed as fixed here.

## For the suite harness

The original page's practical recommendation stands and is now quantified.
**These classes were never hanging, and the harness was measuring `--nojit`,**
which is ~46x slower than the default configuration on this workload. A
timeout-bounded sweep should either drop the forced-interpreter flag or price
its per-class budget against interpreter cost rather than against HotSpot's.
Classifying an overrun as `HANG` because the wrapper returned `rc=124` cannot
distinguish "blocked" from "slower than the budget", and for every class on this
page the answer was the second one.

## The transferable part

**A missing-intrinsic hypothesis has to name the class that actually runs.**
"CratonVM has no SHA-256 fast path" was checkable with one grep of the caller
(`DigestUtil.createDigest`), and the answer changed the whole shape of the fix:
the fast path existed, was fast, and sat behind a door this workload never
opened. Ask which implementation serves the call before asking whether it is
optimised.

**`hot_but_stuck_in_interpreter=0` is a real answer, not a null result.** One
run ruled out the entire family of causes that the sibling commons-math page
turned out to be — and only then was profiling the right next move.
