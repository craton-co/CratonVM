# bc-java, the full 53: the eight `pqc.*` classes, measured for the first time

## Scope

Earlier passes on this suite scoped to the 45 non-`pqc` classes
(bug-bcjava-53class-residuals-20260817.md). This is the first run of all **53**
on both VMs, same harness, same heap.

Harness: `/data/bc53-shard.sh`, `-Xmx 1g`, JIT ON, `CLASS_TIMEOUT=1800`, three
shards. CratonVM binary built from `dev` at `43cc8b527`.

| | CratonVM | HotSpot 25 |
|---|---|---|
| PASS | 49 | 51 |
| FAIL | 2 | 2 |
| HANG (1800s cap) | 2 | 0 |

Only four rows differ between the two VMs:

```text
jce.provider.test           cvm=FAIL  hotspot=PASS
openssl.test                cvm=PASS  hotspot=FAIL   <- CratonVM wins; HotSpot OOMs at 1g
pqc.crypto.test             cvm=HANG  hotspot=PASS
pqc.jcajce.provider.test    cvm=HANG  hotspot=PASS
```

`pkix.test` fails on BOTH, on the same five rows, so it does not appear here.

## Neither `HANG` is a hang

Re-run standalone with a 5400s cap, `pqc.crypto.test` **completes**:

```text
org.bouncycastle.pqc.crypto.test.AllTests   FAIL   3254s   rc=1
   Tests run: 120,  Failures: 1
   1) testTestVectors(org.bouncycastle.pqc.crypto.test.HAETAETest)
```

So the sharded `HANG` was a timeout artifact, and the timeout was hiding a real
correctness failure underneath it.

A mid-run check nearly filed this the other way: the log was byte-identical over
a 30-second window, which reads as frozen. Over 60 seconds it grew
(22575 -> 22927 bytes). **Thirty seconds is not long enough to call a JUnit run
stalled** when each dot can be minutes of work.

## `HAETAETest` cannot be compared across VMs as written

This page previously quoted HAETAE at **">900s against HotSpot's 0.297s"** and
built a theory on the ratio. The ratio was not a measurement of anything.
`TestSampler`, which every `pqc` KAT test uses:

```java
Random random = new Random(System.currentTimeMillis());
this.offSet = random.nextInt(10);
...
return count != 0 && ((count + offSet) % 9 != 0);
```

The sampler seeds from the **wall clock** and then runs roughly every ninth KAT
vector. Two runs execute **different vectors**, so the CratonVM run and the
HotSpot run were never doing the same work — and neither number is reproducible.
This is the standing rule about checking whether a test seeds its RNG before
comparing VMs, and it applies to the timing as much as to the verdict.

`HaetaeKat` (`/data/probe/src/.../HaetaeKat.java`) replaces it: same operations
as `TestUtils.testTestVector`, the **first n vectors** of a chosen file, no
sampler, each of the four KAT checks reported separately. Both VMs then do
bit-identical work.

Like-for-like, the gap is ordinary:

| | HotSpot | CratonVM (JIT) |
|---|---:|---:|
| mode2, 3 vectors | 119 ms | 1061 ms |
| mode3, 2 vectors | 130 ms | 1016 ms |
| mode5, 2 vectors | 156 ms | 678 ms |

**5-9x and still warming**, not three orders of magnitude. And `pk`, `sk`, `sig`
and `verify` all read `OK` on CratonVM for the early vectors of all three
parameter sets.

## What the full sweep found: two JIT-only defects at named vectors

Running mode2 to exhaustion is what the sampler can only do by luck. CratonVM
with JIT on:

```text
count=0 keygen=360ms sign=149ms verify=10ms | pk=OK sk=OK sig=OK verifies=true
count=1 keygen=224ms sign= 87ms verify= 9ms | pk=OK sk=OK sig=OK verifies=true
count=2 keygen=123ms sign= 36ms verify= 9ms | pk=OK sk=OK sig=OK verifies=true
count=3 keygen=245ms sign= 34ms verify= 9ms | pk=OK sk=OK sig=OK verifies=true
count=4 keygen= 38ms sign=151ms verify= 9ms | pk=OK sk=OK sig=OK verifies=true
count=5 keygen= 62ms sign= 86ms verify= 8ms | pk=OK sk=OK sig=OK verifies=FALSE
count=6 ... never returns (SIGKILL at 240s)
```

Two separate wrong behaviours, at two specific vectors:

* **count=5 — a wrong answer, not a slow one.** The signature CratonVM produces
  matches the KAT **byte for byte** (`sig=OK`), and then
  `HAETAESigner.verifySignature` rejects it. Signing is right; verification is
  wrong.
* **count=6 — never completes.** 240s hard cap, killed, against HotSpot's 8ms
  for the same vector.

**`--nojit` clears both.** Same binary, same vectors, interpreter only:

```text
count=5 keygen=185ms sign=315ms verify=12ms | pk=OK sk=OK sig=OK verifies=true
count=6 keygen=220ms sign= 61ms verify=12ms | pk=OK sk=OK sig=OK verifies=true
...
TOTAL 7569ms for 10 vector(s) of PQCsignKAT_haetae_mode2.rsp
```

Ten vectors, clean, in 7.6 seconds. HotSpot passes all 100 vectors of all three
files (300 total, 2.7s). So both the wrong verify and the hang are **JIT
defects**, and they are very likely one defect: a value computed wrong in
compiled code, which at count=5 makes a verification fail and at count=6 makes a
rejection-sampling loop never accept.

`--nojit` FIRST remains the cheapest discriminator in this tree, and it has now
collapsed a bc-java crypto cluster into a single JIT bug once again.

### Where the stuck process is

`perf record` on the hung count=6 process (25s, 199Hz) — and, as a control, on a
**healthy** HAETAE workload that completes normally:

| | stuck (count=6) | healthy |
|---|---:|---:|
| `ZgcRealHeap::alloc_raw_tlab` | 14.31% | 14.26% |
| `jit::helpers::jit_newarray` | 8.50% | 7.03% |
| `MonitorTable::prune_dead` | 8.22% | 8.21% |
| `collect_garbage` (+`closure#4`) | 5.57% | 5.33% |
| `interpreter::execute_frame_from_index` | 2.49% | 2.08% |
| `bc_keccak_permute` | 2.13% | 1.78% |

The two profiles are **the same shape**. The hang is not a new code path and not
a livelock in the VM — it is the ordinary HAETAE loop, allocating ordinary
arrays, simply never terminating. That is what a rejection sampler does when the
value it is testing is wrong.

It also retires this page's earlier claim that HAETAE is "native-dispatch
bound", with its ~25% dispatch / 1.6% interpreter split. That reading came from
a profile of a run that was **already stuck**, and a profile of a stalled
process describes the stall, not the workload. On the healthy workload the
native funnel does not reach the 1.2% cut; allocation and collection are ~33% of
the attributable Rust time. (The named Rust symbols account for ~45% of samples;
the remainder is JIT-compiled Java without symbols.)

## Naming the native hammers

`--dump-native-registry` on a healthy single-iteration HAETAE run — 132 distinct
natives, 107,708 invocations:

| invocations | share | native |
|---:|---:|---|
| 90,212 | 83.8% | `java/lang/Object.<init>()V` |
| 7,294 | 6.8% | `org/bouncycastle/util/Pack.intToLittleEndian(I[BI)V` |
| 3,946 | 3.7% | `org/bouncycastle/util/Pack.littleEndianToInt([BI)I` |
| 2,832 | 2.6% | `org/bouncycastle/crypto/digests/KeccakDigest.KeccakExtract()V` |
| 344 | 0.3% | `java/lang/Integer.parseInt(Ljava/lang/String;I)I` |
| 70 | 0.1% | `KeccakDigest.KeccakAbsorb([BI)V` |

SHAKE/Keccak was the stated first hypothesis and it is present but small.
`KeccakPermutation` never fires at all, and absorb runs 70 times against
extract's 2,832 — the signature of an XOF being **squeezed** for pseudorandom
output rather than fed input, which is exactly what a lattice scheme does.

The row that stands out is `java/lang/Object.<init>()V` at **84% of every native
invocation in the process**. It is registered in `native-builtins/src/lib.rs` as
`native_noop_with_this`, whose entire body is `Ok(None)`, so every object
allocation whose constructor chain reaches `Object` pays a native dispatch to do
nothing. It also trivially satisfies all four clauses of the leaf contract — no
allocation, no safepoint, no collection, no pending exception — and is **not**
marked leaf, so it takes the full funnel rather than `safe_native_call_leaf`.

**But the census counts frequency, not cost, and the profile does not support
promoting it.** On the same workload the native funnel is below the 1.2% cut.
A frequency table is a map of what runs; only the profile says what it costs,
and here the two point in different directions. `Object.<init>` being 84% of
invocations is worth recording as a VM-wide fact — it is every allocation in
every workload, not a HAETAE property — but on this evidence a leaf promotion
buys single-digit milliseconds here, and it should be justified on a workload
where the funnel actually shows up.

## What is worth doing next, in order

1. **Bisect the count=5 JIT defect.** It is deterministic, it reproduces in
   about ninety seconds, and the `--nojit` control is unambiguous — the
   strongest starting position any JIT bug in this tree has had. `HaetaeKat 6 0`
   is the repro; `verifies=false` alongside `sig=OK` is the signal. Vector
   count=6 of the same file is the same defect with the loop unable to exit, and
   gives a second, independent signal for the same edit to clear.
2. **`MonitorTable::prune_dead` at 8% of a crypto workload** is a standalone
   perf question. HAETAE takes no locks; whatever this is walking, it walks on
   every collection of an allocation-heavy loop.
3. The rest of `pqc` is interpreter-bound and is ordinary tier-up work
   (`pqc.crypto.lms`: CratonVM 280s vs HotSpot 13s; the three
   `rbc6-handler-reads-unsafe-local` refusals are named, but the `--nojit` arm
   runs >1500s, so the refusals are not the cost).

None of this is touched by the JCA work in the sibling page; these classes were
simply never measured before.

## LOCALISED: the C1 compile of `HAETAEEngine.ntt`

Not fixed on current dev. A fresh binary from dev `6559b300d` — 143 commits and a
different md5 from the one the rows above were measured on — reproduces count=5
`verifies=false` unchanged.

`CRATONVM_JIT_DENY` matches on `class.method`, so it bisects to a method. A
binary search over `HAETAEEngine`'s 147 methods, ~2s per arm:

```text
control(no deny)  = verifies=false
control(all deny) = verifies=true
  deny [0,73)  n=73 -> verifies=true
  deny [0,36)  n=36 -> verifies=false
  deny [36,54) n=18 -> verifies=true
  ...
CULPRIT INDEX 43 -> ntt
confirm deny-only-that: verifies=true
```

Denying that ONE method — `CRATONVM_JIT_DENY=HAETAEEngine.ntt` — turns the KAT
green. And the tier knobs say which compile of it is wrong:

| arm | count=5 |
|---|---|
| control | `verifies=false` |
| `CRATONVM_JIT_DENY=HAETAEEngine.ntt` | **`verifies=true`** |
| `CRATONVM_JIT_FORCE_C2=1` | **`verifies=true`** |
| `CRATONVM_TIER_C1_THRESHOLD` huge (never C1) | **`verifies=true`** |
| `CRATONVM_TIER_C2_THRESHOLD` huge (C1 only) | `verifies=false` |
| `CRATONVM_TIER_C2_THRESHOLD` huge + deny ntt | **`verifies=true`** |
| OSR backedge and/or threshold huge | `verifies=false` |

**It is the C1 full compile of `ntt`.** C2's compile of the same method is
correct, the interpreter is correct, and OSR is not involved — the disassembly
shows an OSR body for `ntt` as well, but disabling OSR changes nothing.

All 53 off-switchable JIT passes were swept individually against the repro
(`aaload-licm`, `bce`, `unroll`, `local-liveness`, `long-intrinsics`,
`scalar-replacement`, `precise-reg-spill`, `slot-mirror`, …). **None of them
makes it pass**, so this is core C1 codegen, not an optional pass.

### The repro: `probes/NttRealProbe.java`, ~2 seconds

It drives the real method through its public wrapper and needs no reference
implementation, because it checks something that must hold on any VM: every
iteration hands `polyNtt` a freshly built array with identical contents, so
every iteration must return the identical result. Iteration 0 defines the
answer.

```text
HotSpot 25              iterations=5000 diverged=0    firstBad=-1  => PASS
CratonVM --nojit        iterations=5000 diverged=0    firstBad=-1  => PASS
CratonVM JIT            DIVERGED first at iteration 502, element [0]:
                            got=-827016358 expected(iteration 0)=-27922
                        iterations=5000 diverged=4498 firstBad=502 => FAIL
```

**Iteration 502 against a `c1_threshold=500`.** Everything before the C1 compile
is right and everything after it is wrong, which is what makes vector count=5 of
mode2 special: nothing about that vector's data matters, it is simply where the
invocation counter crosses the threshold. The same reading explains count=6's
hang — once `ntt` is wrong, the rejection sampler downstream never accepts.

### What `ntt` looks like

```java
private void ntt(int[] a)
{
    int k = 0, j;
    for (int len = 128; len > 0; len >>= 1)
        for (int start = 0; start < HAETAEParameters.N; start = j + len)
        {
            int zeta = ZETAS[++k];
            for (j = start; j < start + len; ++j) { ... }
        }
}
```

`j` is declared outside both loops, advanced by the INNER loop, and read back by
the MIDDLE loop's update expression (`start = j + len`) — so it is live across
the inner loop's exit and feeds the enclosing loop's induction. `javap` confirms
that is what the JIT sees: slot 3 is stored at pc 82 inside the inner loop and
loaded at pc 88 for the middle loop's update.

That shape is the obvious suspect, and it is **not on its own sufficient**: a
standalone probe reproducing exactly that structure (`probes/NttShapeProbe.java`,
static method, same `sipush 256` bound, same `getstatic` zeta table) passes on
CratonVM at every tier, including pinned to C1. So something else about the real
method — it is an instance method reached through a wrapper, among other
differences — is needed to trigger it. Naming that difference is the next step,
and it is what turns this into a standalone regression test.

### Two negative results on the standalone repro (do not repeat these)

`probes/NttShapeProbe.java` — static method, exact loop shape, `sipush 256`
bound, `getstatic` zeta table, same montgomery-reduce arithmetic. **PASSES** on
CratonVM at default tiering, pinned to C1 (`CRATONVM_TIER_C2_THRESHOLD` huge),
and under `--nojit`.

`probes/NttShape2Probe.java` — the same, but `ntt` is a private INSTANCE method
reached through a public `polyNtt` wrapper, matching the real class's shape, and
self-checking the same way `NttRealProbe` does. **Also PASSES.**

So neither the loop shape nor the instance-method-through-a-wrapper structure is
sufficient on its own. What is still different about the real
`HAETAEEngine.ntt`, and is where the next attempt should look:

* it lives in a 3,860-line class of which ~44 methods are compiled in the same
  run (`jit-method-stats`: `c1=44 c2=53 osr=17`), against 3-4 in the standalone
  probes — so register pressure, code-cache state, or an interaction with a
  neighbouring compile are all still live;
* `montgomeryReduce` in the real class is reached as `invokestatic` from a
  method that is itself hot, and is compiled too; the probes' copy may be
  getting different treatment;
* the real `ntt` has several callers (`polyNtt`, `polyveckNtt`, `polyveclNtt`,
  `polyvecmNtt`, …), the probes have one.

The cheapest way to close the gap is probably from the other end: read the C1
disassembly of `ntt` (`CRATONVM_DBG=jit-disasm CRATONVM_DBG_JIT_DISASM=1`, which
emits it as `full org/bouncycastle/…/HAETAEEngine.ntt([I)V … len=4465`) against
the C2 body of the same method, and find where the middle loop's
`start = j + len` update reads its `j`. The bytecode is unambiguous about what
it should be: slot 3 stored at pc 82 inside the inner loop, loaded at pc 88 for
the update.

**Status: localised, not fixed.** The defect is live on dev at `b6fab962e`.
`CRATONVM_JIT_DENY=HAETAEEngine.ntt` is a working local workaround for anyone who
needs the pqc suite green in the meantime.
