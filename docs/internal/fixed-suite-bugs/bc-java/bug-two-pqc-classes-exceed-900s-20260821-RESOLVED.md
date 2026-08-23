# ✅ RESOLVED — the two `pqc` classes: not wedged, not 22x, and one of them was also broken

## Status

**RESOLVED 2026-08-22** on `fix/bcjava-pqc-and-cipherstream-20260822`.

The page filed both classes as `HANG (900 s, rc=124)`, said — correctly — that
the label was wrong, and left three questions open: do they terminate, how much
slower are they really, and are the two leads in the logs the cause. All three
are answered. Two of the answers are "no", and the third turned a lead into a
fixed defect.

| | filed | 2026-08-22 |
|---|---|---|
| 045 `pqc.crypto.test.AllTests` | `HANG 900 s`; "does not finish in an hour" | **terminates on unmodified `dev`** (7166 s, `Tests run: 120, Failures: 1`) and is **`OK (120 tests)` with the fixes below** |
| 046 `pqc.jcajce.provider.test.AllTests` | `HANG 900 s`, same | **terminates on unmodified `dev`** (14 613 s, `Tests run: 316, Failures: 3, Errors: 8`) and is **`OK (316 tests)` with the fixes below** |
| ">22x, a lower bound" | two truncated wall clocks | **17.2x in CPU time** over the whole of 045, both arms `OK (120 tests)` |
| Lead 1 — 14 discarded compiles | "worth sizing before assuming it matters" | **sized: 20 of them cost 19 ms in a 3323 s run** |
| Lead 2 — "a root-collection gap on 046, twice" | "deserves its own investigation" | **not a collector defect; two `ClassCastException`s, cause found and fixed** |

## Neither class is a hang, and both are now green

Run with a bound worth the name (28 800 s) on unmodified `origin/dev`
(`0b0207bf4`), default collector, `--Xmx 1g`:

```text
045   Time:  7,166.403   Tests run: 120,  Failures: 1,  Errors: 0
046   Time: 14,610.588   Tests run: 316,  Failures: 3,  Errors: 8
```

Both terminate. The page's "Nothing here has run them to completion" no longer
holds, and with it goes the last reading on which either could be wedged.

Running them to completion is also what exposed twelve failures, which no 900 s
cap could ever have shown. **All twelve are fixed here**, in four defects:

| failure | cause | now |
|---|---|---|
| 045 `HAETAETest.testTestVectors` | the JIT inline splicer left a 64-bit product in an `int` slot | fixed — see the splicer page |
| 046 `XMSSTest` ×2 and `XMSSMTTest` ×5, all `ClassCastException`, plus `XMSSTest.testKeyRebuild` | `Signature.getInstance` wrapped a provider SPI that is itself a `Signature` | fixed |
| 046 `NewHopeTest.testKeyExchange` | `KeyAgreement.init` refused a null key the JDK forwards | fixed |
| 046 `FalconTest.testRestrictedKeyPairGen` | the engine overwrote the provider's canonical algorithm name | fixed |
| 046 `HaetaeTest.testHaetaeSign` | the splicer defect again | fixed |

With all of them applied, and after merging 152 commits of `dev` on top:

```text
045   OK (120 tests)   3126 s
046   OK (316 tests)   6117 s
```

Both counts are HotSpot's own for the same suites.

**046's green is not unconditional, and the reason is a defect `dev` already
has.** An earlier run of the fixed binary came back `Failures: 13, Errors: 24`,
all of them one pre-existing fault that surfaces on some runs and not others. A
four-class subset (`XMSSTest`, `XMSSMTTest`, `SLHDSATest`, `FalconTest`; 61 tests), run on
both binaries within the same hour on the same host:

| | result | `ClassCache$CacheRef` lines |
|---|---|---|
| unmodified `dev` | `Failures: 2, Errors: 12` | **21** |
| with these fixes | **`Failures: 0, Errors: 6`** | **21** |

The six that survive are all one pre-existing defect — `SoftReference.get()`
answering a `java.io.ClassCache$CacheRef` where the referent belongs, which
`java.lang.invoke.MethodTypeForm.cachedLambdaForm` then fails to cast — and its
count is IDENTICAL on both arms. It is the DEFAULT collector's alone: the same
subset on the same binary is `OK (61 tests)` under `--XX:UseGc G1` and under
`--XX:UseGc Generational`, with zero occurrences. It has its own page,
`softreference-get-answers-another-reference`.

How often it fires is a property of the RUN, not of the build: across five
full-suite runs of 046 the count was 0, 1, 24, 45 and 104, on binaries whose only
differences are unrelated JCA fixes, and the `dev` binary's own two runs bracket
that range (0 and 1). The four-class subset above is the controlled comparison;
a pair of full-suite runs is not, and read that way the pair would say this
change made 046 worse — which the same-hour subset shows it did not.

## How much slower, measured with a meter the host cannot move

The page's ">22x" came from two truncated wall clocks, and it said so. Wall
clock on this host is not usable for a ratio. Over one afternoon the SAME
configuration of one class measured **252 s, 340 s and 934 s** as the machine's
load average swung between 5 and 155.

CPU time is. `perf stat -e task-clock`, `FrodoKEMVectorTest` (a mid-sized member
of 045), two runs per arm:

| workload | HotSpot 25 | CratonVM | ratio |
|---|---|---|---|
| **the whole of 045**, `OK (120 tests)` on both | **179.1 s** | **3071.1 s** | **17.2x** |
| `FrodoKEMVectorTest`, run 1 | 9.12 s | 139.6 s | |
| `FrodoKEMVectorTest`, run 2 | 9.53 s | 140.6 s | **15.1x** (means) |

**17.2x on the suite**, and a measurement rather than a bound. The per-class
figure is reproducible to under 5% across runs.
(`instructions:u` reads `<not supported>` on this VM — no PMU — so `task-clock`
is the meter. It is stable to 0.7% within an arm where wall clock is not stable
to 300%.)

## Lead 1 — the discarded compiles cost about a millisecond each

The page asked for this to be sized before being believed, and nothing in the
tree could size it: the bail was logged per method, the retry is deferred to the
next compile request, and the wall clock of the thrown-away lowering was
recorded nowhere. It is recorded now, beside `total_compile_time_ms`, which is
the denominator that makes it mean something.

The whole of 045, on the fixed binary:

```text
[cratonvm] JIT method stats: … compiles: c1=776 c2=869 osr=246 deopts=61
  c2_bailouts=0 total_compile_time_ms=613
  | code_buffer_bails=20 (discarded_compile_ms=19)
```

**Twenty discarded compiles, 19 ms, inside a 3 323 000 ms run** — 0.0006%. All
compilation in the suite is 613 ms, 0.02%. For this lead to have mattered, each
discarded compile would have had to take about three minutes.

Two things that were live worries, both settled by the same line:

* **The retry works.** On `FrodoKEMVectorTest`,
  `hot_but_stuck_in_interpreter=0` and `compile-failures=0`: the method that
  overflowed is compiled on the next request, at the measured size. The failure
  mode "a `testVectors` method overflows once and stays interpreted for the rest
  of a very long loop" does not happen.
* **The estimate was deliberately not raised.** A blanket safety factor big
  enough to cover the observed 0.5–35% undershoots would enlarge EVERY method's
  buffer, and `ExecutableBuffer::new` charges its full capacity to
  `COMMITTED_JIT_CODE_BYTES`, which is what the code-cache cap bounds. Spending
  code cache to save 0.0006% of runtime is the wrong trade. (`Drop` does
  decrement that counter, so a discarded buffer is not a leak — checked, not
  assumed.)

## Lead 2 — not a root-collection gap. Two ClassCastExceptions.

The page recorded two `cratonvm::gc::guard` ERRORs on 046 and read them as a
collector defect of a new kind — "a stale published snapshot", one collection
behind — deserving its own investigation. They are not that.

**`op_checkcast` calls `report_root_slice_provenance` on EVERY failing cast.**
Not on a suspicious one; on all of them. And by then the receiver has been
popped off the operand stack into a bare Rust local, so
`in_published_snapshot=false` is true there **by construction** —
`typecheck.rs` says exactly that in its own comment, while explaining why
`op_checkcast` must not run Java code:

> the receiver has been popped from the operand stack and is a bare Rust local;
> the VM's own root-collection guard already reports that object as
> `in_published_snapshot=false` at `site="checkcast"`

A five-line probe settles it beyond argument: it reproduces the same guard line
with **`collections_now=0`** — before any collection has run at all. Nothing had
been reclaimed, because nothing had been collected.

What the two lines actually record is two `ClassCastException`s in `XMSSTest`,
and those were real.

### The defect under them

`XMSSTest.testExhaustion` and `.testKeyExtraction` both open with

```java
StateAwareSignature sig =
    (StateAwareSignature) Signature.getInstance(oid, "BCPQC");
```

which on CratonVM was, deterministically, on the first call:

```text
ClassCastException: class java.security.Signature cannot be cast to
class org.bouncycastle.pqc.jcajce.interfaces.StateAwareSignature
```

`Signature.getInstance` wrapped every third-party SPI in this engine's own
`java.security.Signature` synthetic. The JDK does not:

```java
if (instance.impl instanceof Signature sig) { sig.algorithm = algorithm; }
else { sig = new Delegate((SignatureSpi) instance.impl, algorithm); }
sig.provider = instance.provider;
return sig;
```

A provider extends `java.security.Signature` *precisely so* its callers can cast
the result to its own interface, and BouncyCastle's stateful PQC signers are
that shape: `XMSSSignatureSpi extends Signature implements StateAwareSignature`.
Wrapping it threw the identity away.

| | HotSpot | before | after |
|---|---|---|---|
| `getInstance(xmss_SHA256, "BCPQC").getClass()` | `XMSSSignatureSpi$withSha256` | `java.security.Signature` | `XMSSSignatureSpi$withSha256` |
| `XMSSTest` | `OK (22 tests)` | `rc=124` at 900 s, 2 guard ERRORs | **`OK (22 tests)`** |

The unwrapped object is its own SPI, so `initSign`/`update`/`sign`/`verify`
forward to the provider's own `engine*` bytecode, and its private slots are
skipped — `synthetic_base_offset` counts `java.security.Signature`'s fields, so
`base + SIG_OFF_*` on a SUBCLASS indexes into that subclass's own declared
fields, where a read returns the provider's data as ours and a write destroys
it. Everything this engine keeps for such a receiver lives in identity-keyed
side tables that do not care whose class it is.

## Where the time actually goes

`perf record -F 499`, 60 s inside the running 045 on unmodified `dev`:

| | share |
|---|---|
| VM binary (interpreter + runtime helpers) | **89.6%** |
| `[JIT]` compiled code | **9.1%** |
| libc | 1.0% |

and inside the VM binary:

| symbol | % of run |
|---|---|
| `interpreter::execute_frame_from_index` | 18.9 |
| `interpreter::opcodes::op_getfield` | 7.5 |
| `vm_exec::safe_native_call_impl` | 5.8 |
| `jit::helpers::try_jit_site_cached_native_dispatch` | 4.3 |
| `ZgcRealHeap::is_object_address` | 3.7 |
| `ZObjectStarts::contains` | 3.6 |
| `jit::helpers::decode_dispatch_values_into` | 2.8 |
| `VmHeap::load_and_forward_inner` | 2.4 |
| `jit::helpers::jit_invoke_dispatch` | 2.0 |

Three families, and **none of them is pqc**:

* **~26% is plain interpretation.** The suite's own `jit-method-stats` names the
  owners, and they are not diffuse:

  ```text
  12945934 invocations  ineligible-by-policy  GF16Utils.mVecMulAdd
                        reason=singlepass-codegen/dup2-unprovable-top-w
   5711055 invocations  ineligible-by-policy  FalconVrfy.mq_montymul
                        reason=jit-scan-reject
    335476 invocations  compile-failed        SDitHGF2P32.mulNaive16
  ```

  Two methods, 18.6 million interpreted invocations, two named refusal reasons.
* **~18% is the native-call funnel** — `safe_native_call_impl`,
  `try_jit_site_cached_native_dispatch`, `decode_dispatch_values_into`,
  `jit_invoke_dispatch`, `forward_jit_reference_args`, `forward_jit_arg_at`,
  `enter_native_state`. That is the cost
  `static-exception-table-callee-pays-the-funnel` already owns.
* **~10% is heap-membership validation** on reference reads —
  `is_object_address` + `ZObjectStarts::contains` + `load_and_forward_inner`,
  the getfield load-barrier residual.

This is a different mix from the LMS/HSS page, which was 50.8% runtime / 47.0%
compiled code and had one hashing kernel to intrinsify. Here compiled code is
9%: there is no kernel to make faster, because the workload is barely running
compiled code at all.

## One lead that looked decisive and is not: the native-shadow caller seal

The JIT seals a method out of compilation entirely if its bytecode calls any
natively-shadowed method. On `FrodoKEMVectorTest` that is **81 methods**, and
the two that matter are the whole workload:

```text
[cratonvm] JIT skip-seal census: 189 method(s) sealed before any compile
  | clinit=108 calls-native-shadowed-method=81

  org/bouncycastle/pqc/crypto/test/FrodoKEMVectorTest.testVectors()V
  org/bouncycastle/crypto/kems/frodo/FrodoMatrixGenerator$Aes128MatrixGenerator
      .genMatrix([BII)[S
```

`genMatrix` is FrodoKEM's kernel and `testVectors` is its driver, and neither
can ever be compiled. That looks like the whole story, and the measurement lever
exists (`CRATONVM_JIT=-native-shadow-caller-seal`, measurement-only).

**It is worth 1–2%.** CPU time, two runs per arm:

| | run 1 | run 2 |
|---|---|---|
| seal ON (shipping) | 139.62 s | 140.58 s |
| seal OFF | 138.10 s | 138.00 s |

Both arms report `OK (2 tests)`; with the seal off the census's
`calls-native-shadowed-method` bucket is empty.

**The wall-clock version of this same A/B said 1.66x, and it was noise.** Three
interleaved wall-clock rounds gave 934/561, 340/354 and 252/428 seconds — the ON
arm alone spanning 252 s to 934 s, a 3.7x swing on one unchanged configuration,
while the host's load average moved between 12 and 155. The first pair's "1.66x
win" is entirely that drift.

This matches what the seal has measured on every other workload it has been
priced on (netty, WebClient, Spring Boot): a large population, almost no cost.
The per-METHOD-vs-per-SITE compiler change it would take to lift it should not
be attempted for this workload's sake.

## What this page no longer claims

* ~~"Not that they never finish."~~ Both finish, and both are green.
* ~~"> 22x and is a lower bound."~~ 15.1x in CPU time on a representative
  member, measured twice per arm.
* ~~"a root-collection gap … deserves its own investigation."~~ It was a
  failing-cast diagnostic. The cast was failing for a reason that is now fixed.
* ~~"Not that the re-compiles are the cost."~~ Correct, and now quantified:
  0.0006%.
* **"Not that the two classes share a cause"** — still correct, and still the
  right caution. What they shared was a harness bound, and behind it two
  unrelated sets of defects.

## What is still open, and where it lives

The remaining gap is the ordinary CratonVM-vs-C2 one on interpreter-and-native
heavy code. Its components all have their own owners, and this page is not the
right one for any of them:

* two named methods refused by the single-pass codegen —
  `dup2-unprovable-top-w` on `GF16Utils.mVecMulAdd` (12.9M invocations) and
  `jit-scan-reject` on `FalconVrfy.mq_montymul` (5.7M);
* the native-call funnel — `static-exception-table-callee-pays-the-funnel`;
* the getfield load barrier — the ZGC JIT load-barrier page.

Nothing pqc-specific remains.

## The transferable part

**A diagnostic that fires on every instance of a failure is not evidence about
that failure.** `report_root_slice_provenance` runs on every failing cast, and
its message is written as a verdict ("a root COLLECTION gap, not a mark or sweep
one"). Two of those lines read as a new collector bug worth its own
investigation; they were the VM's way of saying "a cast failed", twice. Before
treating a guard's output as a finding, check what makes the guard fire.

**On a shared host, price a change in CPU time, not wall clock.** The same
configuration of one class measured 252 s and 934 s four hours apart. Any A/B
built on those numbers can be made to say anything; `perf stat -e task-clock`
gave the same answer twice to within 0.7% while the load average moved by an
order of magnitude.

**A timeout hides more than the runtime.** Both classes were filed on a symptom
(`rc=124`) that no amount of analysis could turn into a verdict. Raising the
bound until they finished cost two long runs and produced eleven named
failures, ten of which were fixable and none of which the 900 s cap could ever
have shown. "How long does it actually take" is usually the cheapest question on
the page.
