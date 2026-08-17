# ✅ FIXED — bc-java PQC: not a hang, and the SHA-256 hypothesis named the right kernel behind the wrong class

## Status
**RESOLVED 2026-08-17** on `fix/bcjava-pqc-lms-throughput-20260817`.

**`org.bouncycastle.pqc.crypto.lms.AllTests` passes** — `OK (29 tests)`, the
same count HotSpot reports — in **1460 s unmodified** and **681 s with the fix**,
where the page recorded `HANG (3000s, rc=124)`. `HSSTests` — 15.9 of that
suite's 16 HotSpot seconds — passes on the *unmodified* binary in 2062 s once
the forced-interpreter flag is dropped. With the SHA-256
compression leaf made native it is **1047 s**, and the isolated kernel improves
**1.8–3.2x** under the JIT and **~6x** under `--nojit`.

What is deliberately **not** claimed: `pqc.crypto.test.AllTests` is not fixed,
and the decomposition below shows why no single-algorithm fix could have fixed
it. Two costs the profile exposed are handed to their own pages rather than
carried here.

Filed originally as: *`pqc.crypto.lms.AllTests` / `pqc.crypto.test.AllTests`
exceed even a 10x timeout — confirmed CPU-bound, not deadlocked.*

This page also owns a third class, handed over by the bc-java residual sweep
(bug-bcjava-residual-suite-failures-20260816.md in this directory, Residual 0): `pqc.jcajce.provider`, 271 s on HotSpot.

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

### First: `lms.AllTests` was never hanging — it PASSES

The page's headline class was filed as `HANG (3000s, rc=124)`. With the JIT on
and no budget cap it passes on **both** binaries — including the unmodified one:

| arm | wall | verdict |
|---|---|---|
| HotSpot 25 | 16 s | `OK (29 tests)` |
| CratonVM, **unmodified `dev`** | **1460 s** | `OK (29 tests)` |
| CratonVM, **+fix** | **681 s** | `OK (29 tests)` |

29 is exactly the five constituent classes' counts summed (13 + 7 + 1 + 7 + 1),
and it is the same count HotSpot reports for the same suite — so this is a real
green, not a `started=0` one.

**It reproduces.** A second, independent base/fix pair run hours later from a
different script:

| run | base | +fix | gain |
|---|---|---|---|
| first | 1460 s | 681 s | 2.14x |
| second | 1008 s | **315 s** | **3.20x** |

Four runs, two binaries, two harnesses, all `OK (29 tests)`. The pass is not a
one-off, and the fix is worth 2–3x on this suite depending on host load.

The class that owns 15.9 of those 16 HotSpot seconds behaves the same way:

| arm | wall | verdict |
|---|---|---|
| CratonVM, **unmodified `dev`** | **2062 s** | `OK (13 tests)` |
| CratonVM, **+fix** | **1047 s** | `OK (13 tests)` |

That retires the `HANG` label outright, and it retires it *without* the fix: the
class was filed as hanging because the sweep runs `--nojit` against a 3000 s
cap, and `--nojit` is ~46x slower than the default on this workload.

(In both arms the whole 29-test suite comes in *under* `HSSTests` alone. That is
not a paradox and not a measurement error: within each run `HSSTests` was
launched first, while this investigation's other builds and suites were still
saturating the shared host, and `AllTests` ran after they drained — and one JVM
running all five classes amortizes JIT warm-up across them. Wall clock here is
an order of magnitude, not a benchmark. The controlled comparisons are the
interleaved kernel rounds and the same-class base/fix pairs.)

### The intrinsic engaged, and it is the native that served the calls

A flat A/B cannot tell "did not help" from "never ran", so the census was taken
first (`--dump-native-registry`):

```
org/bouncycastle/crypto/digests/SHA256Digest.processBlock()V
    kind=intrinsic  invocations=14000
```

14 000 is exactly right: 7 000 digests x 2 compression blocks each.

### Correctness

| check | result |
|---|---|
| kernel unit tests, 4 published FIPS 180-4 vectors | pass |
| BouncyCastle's own `SHA256DigestTest` (includes the `Memoable` copy/reset state) | **`SHA-256: Okay`** |
| `LMSKeyGenTests` / `LMSTests` / `PublicKeyParseTests` / `TypeTests` | pass, **test counts identical to HotSpot** (1 / 7 / 7 / 1) |
| `HSSTests`, both arms | `OK (13 tests)` |

None of these is a `started=0` green: every count was diffed against HotSpot's
own run of the same class.

### Regression: `SHA256Digest` is used all over BouncyCastle, so the sweep is wider than LMS

`processBlock` now serves HMAC, signatures, PKIX, CMS and the provider tests
too, so the check is whether anything *else* moved. Base vs fix, same host:

| suite | base | +fix | outcome |
|---|---|---|---|
| `crypto.test.AllTests` | 1407 s | **707 s** | `Tests run: 21, Failures: 1, Errors: 14` on **both**, and the 15 failing test names diff **identical** |
| `cms.test.AllTests` | 187 s | 221 s | `Tests run: 433, Failures: 0, Errors: 1` on **both**, same single test name |
| `lms.AllTests` | 1008 s | **315 s** | `OK (29 tests)` on both |
| `openssl.test.AllTests` | 18 s | 20 s | `OK (5 tests)` on **both** |
| `jce.provider.test.AllTests` | 2 s | 2 s | identical on both (a harness artefact — the class exposes no JUnit suite, same as `crypto.test.RegressionTest`) |
| `crypto.test.RegressionTest` | 8 s | 7 s | identical (same harness artefact) |

**Six suites, zero regressions.** Every verdict, count and failing test name is
the same on both arms.

The residuals are pre-existing and unrelated. `crypto.test.AllTests`'s fourteen
errors are all `HPKETestVectors` failing `CryptoServiceConstraintsException:
service does not provide 192 bits of security only 128`; `cms.test.AllTests`'s
single error is `NewEnvelopedDataTest.testKeyTransDESEDE3Short`. Every one fails
identically without the intrinsic.

**`cms.test.AllTests` is the honest counter-example**: 187 s → 221 s, i.e. no
gain, and if anything slightly worse inside run-to-run variance on a loaded
shared host. That is expected — CMS is not SHA-256-bound, and this fix buys
nothing where the digest is not the workload. It is reported rather than
dropped, because a table of only the favourable suites would misrepresent what
the intrinsic does.

### Throughput

| workload | HotSpot | CVM base +JIT | CVM +fix | gain |
|---|---|---|---|---|
| `lms.AllTests` (the page's headline suite) | 16 s | **1460 s, OK (29)** | **681 s, OK (29)** | **2.14x** |
| `HSSTests` (the whole class) | 15.9 s | **2062 s** | **1047 s** | **1.97x** |
| `LMSTests` | 0.77 s | 61 s | 33 s | 1.85x |
| `LMSKeyGenTests` | 0.39 s | 6 s | 5 s | — |
| SHA-256 kernel, 3 interleaved rounds | 0.65–1.17 us | 49.5 / 54.7 / 61.7 us | 19.1 / 22.2 / 30.0 us | 1.82x / 2.23x / 3.23x |

The host is shared, so treat wall clock as orders of magnitude; the interleaved
kernel rounds and the identical-binary `HSSTests` pair are the controlled
comparisons.

**In `--nojit` — the mode the harness actually runs — the intrinsic is worth
much more**, because there it replaces interpreted bytecode rather than
compiled code:

| `--nojit` kernel | base | +fix | gain |
|---|---|---|---|
| round 1 | 1 808 166 ns/op | 301 026 | **6.0x** |
| round 2 | 1 804 905 ns/op | 333 612 | **5.4x** |

That does not rescue `--nojit` as a sweep mode: forced interpretation is still
~35–60x the JIT-on cost, so `HSSTests` under `--nojit` remains far beyond any
sane per-class budget even after a 6x. It is an argument for dropping the flag,
not for raising the timeout.

### Re-profiled after the fix

| | before | after |
|---|---|---|
| `[JIT]` compiled code | 47.0% | **12.3%** |
| `sha256_process_block` (the actual SHA-256 maths) | — | **3.38%** |
| `getfield` helper chain | ~31% | ~30% |
| native-call dispatch | ~9% | ~23% |

The crypto is now essentially free. What is left is the `getfield` helper — its
own page — and native-call dispatch, which rose in *share* because the
denominator shrank, not in absolute cost.

### One thing the microbench could not see

On the real suite (but not on the kernel probe, which never reaches them),
`jit-method-stats` reports four methods that never compile, all with the same
refusal:

```
hot_but_stuck_in_interpreter=4 (ineligible-by-policy=1, compile-failures=3)
  884  HSSSignature.getInstance    reason=rbc6-handler-reads-unsafe-local(pc=115,op=0xbb)
  820  HSS.rangeTestKeys           reason=rbc6-handler-reads-unsafe-local(pc=16,op=0xbb)
  692  LMOtsSignature.getInstance  reason=rbc6-handler-reads-unsafe-local(pc=121,op=0xbb)
  628  LMSSignature.getInstance    reason=rbc6-handler-reads-unsafe-local(pc=152,op=0xbb)
```

These are ASN.1 parsers reached once per signature. At 628–884 invocations
inside a 1047 s run dominated by millions of hash blocks they cannot be
material, so they are recorded rather than chased — but the verdict comes from
the invocation counts, not from the microbench's `hot_but_stuck=0`.

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
