# bc-java, the full 53: the eight `pqc.*` classes, measured for the first time

## Scope

Earlier passes on this suite scoped to the 45 non-`pqc` classes
(`docs/known-issues/bc-java/bug-bcjava-53class-residuals-20260817.md`). This is
the first run of all **53** on both VMs, same harness, same heap.

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

So the sharded `HANG` was a timeout artifact over a class that needs **3254s
against HotSpot's 128s (25x)**, and the timeout was hiding a real, single
correctness failure underneath it.

A mid-run check nearly filed this the other way: the log was byte-identical
over a 30-second window, which reads as frozen. Over 60 seconds it grew
(22575 -> 22927 bytes). **Thirty seconds is not long enough to call a JUnit run
stalled** when each dot can be minutes of work.

## The slowness is TWO different problems, and the profiles separate them

`pqc.crypto.lms` is the tractable probe: same ratio, six minutes instead of
fifty-four (CratonVM 280s, HotSpot 13s, **21x**).

**1. The bulk of `pqc` is interpreter-bound.** `perf record` on
`pqc.crypto.test`, 45s at 199Hz:

```text
25.63%  interpreter::execute_frame_from_index
 8.43%  interpreter::opcodes::op_getfield
 3.26%  zgc::ZObjectStarts::contains
 2.80%  zgc::ZgcRealHeap::is_object_address
 2.80%  jit::conservative_roots::push_entry_full
```

~34% interpreter. `CRATONVM_DBG=jit-method-stats` on the `lms` probe names
three hot-but-stuck methods, all refused by ONE gate:

```text
hot_but_stuck_in_interpreter=3 (ineligible-by-policy=1, compile-failures=2)
  884 inv  HSSSignature.getInstance   rbc6-handler-reads-unsafe-local(pc=151,op=0x12)
  820 inv  HSS.rangeTestKeys          rbc6-handler-reads-unsafe-local(pc=27,op=0x12)
  692 inv  LMSSignature.getInstance   rbc6-handler-reads-unsafe-local(pc=187,op=0x12)
```

**But that refusal is not the cost.** The `--nojit` control on the same probe
runs past 1500s where the JIT arm takes 280s, so the JIT is engaged and worth
**>5.4x** here; 60 methods reach C2. Three refused methods do not explain a 21x
gap. This is the rule that has paid off before — a NAMED refusal on the path is
not the cost until the `--nojit` arm says so.

**2. `HAETAETest` is a different defect entirely — and it is the outlier.**
Standalone it exceeds a 900s cap where HotSpot finishes in **0.297s**. Its
profile does not look like the others at all:

| | HAETAE | rest of `pqc` |
|---|---|---|
| native-call dispatch | **~25%** | ~4% |
| ZGC address checks | **~17%** | ~6% |
| interpreter | **1.6%** | 25.6% |

```text
 9.80%  zgc::ZObjectStarts::contains
 8.16%  vm_exec::safe_native_call_impl
 7.53%  jit::helpers::try_jit_site_cached_native_dispatch
 7.49%  zgc::ZgcRealHeap::is_object_address
 3.46%  jit::helpers::decode_dispatch_values_into
 3.24%  zgc::ZgcRealHeap::alloc_raw_tlab
 2.92%  jit::helpers::forward_jit_reference_args
 2.86%  jit::helpers::jit_invoke_dispatch
 1.60%  interpreter::execute_frame_from_index
```

So HAETAE is not slow because it is interpreted — it is barely interpreted at
all. It is making an enormous number of NATIVE calls, and every one of them
pays the dispatch funnel plus a ZGC address validation. That is a
per-native-call cost multiplied by a very large call count, which is why it is
three orders of magnitude rather than one.

## What is worth doing next, in order

1. **Name the native HAETAE hammers.** The profile has the Rust side; it does
   not have the Java caller. `--dump-native-registry` plus the invocation
   census answers which native serves the call — a lattice signature scheme
   leans on SHAKE/Keccak, which is the first place to look.
2. **`HAETAETest.testTestVectors` is a real correctness failure**, not just
   slowness: known-answer vectors, HotSpot green, CratonVM red. Check first
   whether it seeds its own RNG before filing it as a VM divergence.
3. The interpreter-bound bulk of `pqc` is ordinary tier-up work and is the
   least surprising of the three.

None of this is touched by the JCA work in the sibling page; these classes were
simply never measured before.
