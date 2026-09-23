# H9-3 — `pqc.crypto.test.AllTests` is slower than the cap, not hung, and the cost is the native CROSSING, not the kernel

**Status:** open (throughput), with the cause re-measured and the recommended
first step **retracted**. Measured 2026-09-22 on
`claude/bc-crypto-alltests-hang-20260922`.
**Verdict it carries:** `CV-BROKEN`.

The first version of this page named BouncyCastle's pure-Java SHA-256 as the
cost and recommended widening the native seam from `processBlock` to
`update`/`doFinal`, with a fidelity warning attached. That recommendation is
**retracted**, and the measurement below is why: the bytecode a wider seam would
absorb is about a tenth of a block, and the crossing a wider seam does NOT
remove is the rest. It would have taken on the `Memoable` fidelity risk the
page itself flagged in exchange for the smaller half.

A second thing this page got wrong is corrected further down, and it is this
lane's own: the crossing cost was attributed to the nine single-slot field
accesses by arithmetic. Batching them is measurably worth a few percent, not
the bulk, so where the rest goes is still unlocated and is now stated as such.

## How it was reported, and why that reading was wrong

`bash regression-suite/corpus/run-corpus.sh run bc-java --mode jdk-only
--timeout 2400` scored:

```
org.bouncycastle.pqc.crypto.test.AllTests   CV-TIMEOUT-STALLED   cv=TIMEOUT-STALLED  hs=RAN
  killed at the 2400s cap after 2375s with NO output: SILENT AT THE WALL.
  That is a hang/stall, not slowness -- on this VM it is very often a SIGSEGV
  or a livelock that printed no result line.
```

That note is wrong, and not by accident: **a `junit`-kind workload cannot
produce output mid-run at all.** It runs under `SbRunner`, which registers a
`SummaryGeneratingListener` plus a silent `TestExecutionListener` and prints
exactly one line, `SBRUNNER_RESULT`, when the whole run is over. So "silent at
the wall" is true of *every* killed junit arm by construction, and it is not
evidence about whether the arm was stuck.

Fixed on this branch: `classify_arm` now takes the workload kind and refuses to
emit `TIMEOUT-STALLED` for a junit arm whose only evidence is silence,
answering `TIMEOUT-UNKNOWN` with a note that names the reason. `TIMEOUT-BUSY`
stays available to junit — recent *output* is still informative; it is silence
that carries nothing. Both behaviours are pinned in `run-corpus.sh selfcheck`,
and `regression-suite/corpus/README.md`'s taxonomy says so.

## It is not hung — measured

The suite's own `AllTests.suite()` adds 36 classes. Run one at a time under a
driver that prints a flushed line per class (`SuiteTimer`, scratch), on the
same binary, same `--jdk-only`, same working directory:

**29 of the 36 classes were run; all 29 complete with `fail=0 err=0`** — the
whole XMSS/XMSSMT family and `SLHDSATest`, the largest class in the suite, among
them. Nothing hangs; nothing fails. The walk was stopped after 7.5 hours inside
`SnovaTest`, the 30th; the seven not reached are `SnovaTest`,
`FaestKeyPairAndSignerTest`, `FaestKatTest`, `HawkTest`, `UOVTest`, `MQOMTest`
and `MQOMKatTest`, together 358 s of HotSpot's 848 s.

`SnovaTest` was classified separately rather than left unexplained, because at
the point it was stopped it had been running 184 minutes against HotSpot's
182 s (61x) — outside the range every other class sits in. Re-run alone under
`--stack-dump-on-timeout`, twice:

```
--- T19.H1 thread summary: 3 registered thread(s) ---
  tid=0 name="main" alive=true blocked=false
      top=org/bouncycastle/pqc/crypto/test/TestUtils.testTestVector@72
       <- org/bouncycastle/pqc/crypto/test/SnovaTest.testTestVectors@20
  tid=1 name="Reference Handler"   blocked=true
  tid=2 name="Common-Cleaner"      blocked=true
```

Three threads, no monitor wait, `main` live inside the KAT loop
(`testTestVector` is a plain sequential read-and-verify over the vector files).
It is slow, like the rest, not stuck.

## The per-class walls

CratonVM against HotSpot 25.0.3+9, same binary, same `--jdk-only`, same working
directory, one class per `junit.textui.TestRunner` call. Every row is
`fail=0 err=0` on both arms.

| class | HotSpot | CratonVM | ratio |
|---|---|---|---|
| `SABERVectorTest` | 0.3 s | 42.8 s | 155.6x |
| `CMCEVectorTest` | 17.8 s | 1 244.9 s | 69.8x |
| `XMSSMTPrivateKeyTest` | 1.9 s | 80.8 s | 43.1x |
| `XMSSSignatureTest` | 9.8 s | 423.1 s | 43.0x |
| `XMSSReducedSignatureTest` | 0.2 s | 9.7 s | 39.8x |
| `MLKEMTest` | 0.7 s | 25.9 s | 36.9x |
| `XMSSMTTest` | 58.8 s | 1 934.3 s | 32.9x |
| `SLHDSATest` | 284.9 s | 9 115.8 s | 32.0x |
| `HSSTest` | 0.6 s | 19.2 s | 30.3x |
| `XMSSTest` | 51.8 s | 1 501.0 s | 29.0x |
| `SNTRUPrimeTest` | 11.3 s | 323.6 s | 28.7x |
| `NTRUTest` | 2.0 s | 55.2 s | 27.7x |
| `MayoTest` | 8.5 s | 189.6 s | 22.4x |
| `XMSSPrivateKeyTest` | 20.4 s | 433.9 s | 21.2x |
| `NTRULPRimeTest` | 6.3 s | 118.1 s | 18.7x |
| `FalconTest` | 1.1 s | 18.8 s | 17.6x |
| `CrystalsDilithiumTest` | 1.6 s | 22.0 s | 13.7x |
| `HQCTest` | 1.7 s | 21.6 s | 12.6x |
| `MLDSATest` | 7.5 s | 71.5 s | 9.5x |
| `FrodoVectorTest` | 5.0 s | 44.8 s | 9.0x |
| `LMSTest` | 0.3 s | 1.3 s | 4.2x |

(Rows under ~0.3 s on HotSpot are omitted; their ratios are dominated by
fixed startup and say nothing.)

The shape is consistent: the hash-based schemes (XMSS/XMSSMT, SLH-DSA, LMS/HSS)
and the KAT-driven vector tests sit at 20-40x, and nothing sits where a hang
would put it. `SABERVectorTest` at 155.6x is the one row whose ratio is an
artefact — 0.3 s of HotSpot against 42.8 s, i.e. mostly this VM's fixed
per-class startup, which every 0.3 s row pays.

HotSpot 25.0.3+9 needs **848 s** for the same 36 classes (the corpus arm
measured 888 s including the launcher), and reports 183 tests, 0 failures. This
is a genuinely large workload: on HotSpot alone `SLHDSATest` is 285 s,
`SnovaTest` 182 s, `MQOMKatTest` 86 s. A 2400 s cap is 2.7x the oracle's own
wall — for comparison, two *sibling* PQC suites in the same sweep AGREE while
running at 28x (`pqc.crypto.lms`) and 79x (`pqc.math.ntru`) the oracle. The cap
was simply too small for this row, not a wall a hang ran into.

## Where the time goes — and the correction

`CRATONVM_DBG=profile-sample-ms=5` over `XMSSSignatureTest`, 103 166 samples:

```
62.89%  org/bouncycastle/pqc/crypto/xmss/WOTSPlus.chain
28.64%  org/bouncycastle/pqc/crypto/xmss/KeyedHashFunctions.coreDigest
 5.62%  org/bouncycastle/pqc/crypto/xmss/XMSSNodeUtil.randomizeHash
```

91.5% in two methods, and `SHA256Digest.update` / `processWord` /
`processBlock` do not appear at all — they are inlined into their callers, so
the profile can name the hash path but cannot split it. Splitting it takes
kernels, and the split is where the first version of this page went wrong.

### Kernel 1 — the seam is not too narrow

`NativeSeamProbe` (scratch) runs each shape twice: once on BouncyCastle's
`SHA256Digest`, whose `processBlock` this VM replaces with a Rust kernel, and
once on `ProbeSha256`, a structural clone under a name no native is registered
for — same algorithm, same field shape, same per-word buffering, verified
digest-identical on 13 lengths through both buffering paths before timing. The
difference between the columns **is** the native.

| | HotSpot | CratonVM native | CratonVM pure-Java | native/pure |
|---|---|---|---|---|
| `coreDigest` 3x32 B + doFinal | 658 ns | 10 020 ns | 15 428 ns | 0.65 |
| `update` 64 B (1 block) | 316 ns | 7 017 ns | 8 900 ns | 0.79 |
| `update` 1024 B (16 blocks) | 4 836 ns | 94 283 ns | 136 579 ns | 0.69 |

The native is worth 21–35%. It is not worth 37x, and the whole compression
function is already in it.

### Kernel 2 — the bytecode around the kernel is ~10% of a block

A block is sixteen `processWord` decodes plus one compression, so
`BlockSplitProbe` (scratch) times the sixteen decodes alone, as pure bytecode,
paired in the same session with the block itself:

| | HotSpot | CratonVM | ratio |
|---|---|---|---|
| 16x `processWord` (a call each) | 81 ns | 593-706 ns | 7-9x |
| 16x word decode, inlined by hand | 62 ns | 145-187 ns | 2-3x |
| `int[64]` fill floor | 55 ns | 73-78 ns | 1.4x |
| one 64-byte block (from `update` 1024 B / 16) | 371 ns | 6 201-7 199 ns | 17-19x |

**~650 ns of ~6 700.** The bytecode around the kernel is about a tenth of the
block, and this VM runs it at 7-9x, which is ordinary here. The Rust
compression is a few hundred ns. Everything else — roughly **5.5-6 us per
block** — is the crossing into the native and back.

That is why widening the seam was the wrong first move. A seam at
`update`/`doFinal` removes the **per-call** overhead — the difference between
the 1-block and 16-block rows, which is the `update` loop, not the crossing —
and leaves the per-BLOCK crossing untouched, while taking on the
`getEncodedState`/`Memoable` fidelity risk the old page flagged.

## What was done, and what it was worth

The first thing inside the crossing that could be cut without touching fidelity
was the field traffic: nine single-slot `NativeContext` accesses per block
(eight chaining words in, eight plus `xOff` out), each of them a
canonicalisation, a corrupt-cell watch, a straystack gate, a descriptor
resolution and a coercion.

`NativeContext` grows `set_fields_typed`, the write twin of the `get_fields_typed`
batch door that already existed and that documents this exact motivation ("pays
that, and the dynamic dispatch, once instead of once per field"). The
BouncyCastle SHA-256 registration now:

* reads `H1..H8` **and** `X` in one `get_fields_typed` call instead of nine
  single-slot reads;
* writes `H1..H8` and `xOff` in one `set_fields_typed` call instead of nine
  single-slot writes;
* reads **sixteen** words of `X` instead of 64. The tail is scratch the message
  schedule writes before it reads — `x[t]` for `t` in `16..64` is computed from
  `x[t-2]`, `x[t-7]`, `x[t-15]`, `x[t-16]`, so the first iteration reads
  `x[14]`, `x[9]`, `x[1]`, `x[0]`, all live input, and every later one reads
  only words the same loop has already written. The write-back still restores
  all 64, so everything `copy()`, `reset(Memoable)` and `getEncodedState` can
  observe is byte-identical to what BouncyCastle's own bytecode leaves.

Fidelity is checked by the probe itself: `ProbeSha256` is a transcription of
BouncyCastle's algorithm that no native touches, and it must agree digest-for-
digest with `SHA256Digest` on 13 lengths through both buffering paths before
any timing is printed. A marshalling slip changes the answer, not just the
speed.

**It is worth a few percent, and that is the interesting part.** Interleaved
A/B, three reps of each binary alternating, medians of the native column:

| | before | after | |
|---|---|---|---|
| `update` 64 B (1 block) | 8 430 ns | 7 784 ns | -7.7% |
| `update` 1024 B (16 blocks) | 114 524 ns | 110 974 ns | -3.1% |
| `coreDigest` 3x32 B + doFinal | 14 040 ns | 13 690 ns | -2.5% |

Never slower, in either direction, across six runs. But a few percent is not
5.5 us, so **the fourteen field-door calls this change removed were a few
hundred nanoseconds of the crossing, not the bulk of it.** The accounting that
predicted otherwise — seventeen heavy doors at a couple of hundred ns each —
was arithmetic, not measurement, and the measurement disagrees with it.

Where the remaining ~5.5 us per block goes is **not located**, and this page
does not claim it. The two candidates, in the order the evidence supports:

* the generic native dispatch path itself (argument marshalling into `Value`s,
  the `contain` panic guard, the `CURRENT_NATIVE_STACK` push/pop, the census
  increment) — which would be a VM-wide finding, not a BouncyCastle one;
* the two bulk `int[]` transfers, though those are `copy_nonoverlapping` of 64
  and 256 bytes and should not be microseconds.

A no-op native called in a loop would separate them in one run. That probe does
not exist yet and is the honest next step.

A caution on the numbers above: this machine's microbenchmark noise is large —
the pure-Java control column swings by 3x between reps of the *same* binary as
the JIT tiers differently — so single-run differences under ~10% on this probe
are not readable, and the table above is medians of interleaved runs for that
reason. The 17-19x block ratio and the ~10% word-decode share are robust
because they are ratios within one run.

## What this does NOT fix

The suite stays `CV-BROKEN`. Even a SHA-256 block at HotSpot's own speed would
leave the hash-based schemes several times over, and 29 of 36 classes take
7.5 hours here against 848 s there. Nothing in a seam change reaches that, and
this page should not be read as claiming it does.

The two remaining candidates, now in the order the measurement supports:

1. **The other single-slot doors.** The batch pair fixes one native. `get_field`
   and `set_field` are called from hundreds of registrations, and the per-call
   prologue measured here is the same prologue everywhere. A census of which
   natives call them more than twice per invocation would say how much of this
   VM's native surface is paying it.
2. **`SHA256Digest.<init>`**, which the probe isolates at 1 805 ns against
   HotSpot's 129. BouncyCastle's constructor also runs
   `CryptoServicesRegistrar.checkConstraints`, which the pure-Java clone does
   not, so that row is not a clean A/B and wants its own measurement before
   anyone acts on it.

## Scope

This lane's defect was the `crypto.test.AllTests` hang, which was a BigInteger
key-generation problem and is closed (see
`docs/internal/comparison-handoff/bug-bc-crypto-regression-timeout.md`). This
page is the honest statement of what the remaining red row is: a correct but
slow suite, with the cost now localised to the native crossing rather than to
"BouncyCastle's SHA-256", and a harness note that was making a hang claim it
could not support.
