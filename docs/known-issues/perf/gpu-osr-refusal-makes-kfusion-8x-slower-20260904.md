# `--gpu` refuses OSR for the hot loop, and kfusion runs 8x slower

## Status

**Root-caused and measured. Not fixed.** The chain below is established
end to end by counters added on 2026-09-03/04, all of which are on `dev`,
and is confirmed from both sides: the gate census names
`Integration.integrate` as blocked, and the OSR refusal census names the
gate that turned it away.

## The symptom

`kfusion.java.Benchmark` under `--gpu` offloads nothing (1,271 hook
refusals in a frame, 94% of them `not_offloadable`) and still runs ~8x
slower than the same binary without the flag. One frame, `--Xms 4g`:

| | integration | GC collections | dead objects |
|---|---|---|---|
| no `--gpu` | 42.9 s | 2-16 (unstable) | 107,186,457 |
| `--gpu` | 327.6 s | 13 (stable) | 136,071,169 |

## The chain

1. **`--gpu` refuses OSR for `Integration.integrate`.** With
   `CRATONVM_DBG_OSR=1`, the baseline prints
   `[cratonvm-osr] enter kfusion/java/algorithms/Integration.integrate`.
   Under `--gpu` that method gets **0** OSR enters while **14** other
   methods still enter in the same run. Ordinary compilation is
   unaffected: 299 vs 297 methods published, the *same* 10 kfusion
   methods, `comm -3` empty. `Integration.integrate` is never published
   in either arm — it is a counted loop, so OSR is the only door it has.

   `offload_jit_gate` is wired into exactly this path and documents this
   exact case: *"the OSR path is the exact case the gate exists for:
   OSR-compiling a hot loop that contains an offload-eligible
   invokestatic would silently end GPU dispatch at that call site."*
   `integrate`'s loop calls `VolumeShort2` / `GraphicsMath` statics,
   which are offload-eligible shapes.

2. **So the frame's dominant loop interprets**, and its per-voxel
   `Short2` / `Float3` temporaries allocate through the VM's slow path
   (`alloc_object_shared`, which bumps `bytes_allocated_total` for
   non-TLAB allocation).

3. **845 MB flows through the counted paths instead of 1.27 MB** —
   `bytes_allocated_total` 844,985,104 against 1,273,520. That counter
   is essentially deterministic for this workload (1,273,392 /
   1,274,192 / 1,273,520 across three baseline runs, within 800 bytes),
   which is what makes the 664x a signal rather than noise.

   It is the SAME allocation, routed differently, not more of it: the
   extra 843,711,584 bytes over the extra 28,884,712 swept objects is
   **29.2 bytes an object** — a 16-byte header plus a field or two,
   i.e. `Short2`/`Float3`.

4. **One forced collection per 64 MB.** `tlab_refill_wedge_break`
   re-arms once per 64 MB of `bytes_allocated_total`:
   844,985,104 / 64 MiB = **12.6**, against the **13** wedge breaks
   observed. Every collection in both arms is an allocation-failure
   retry — `[GC] zgc-entry` shows `forced=13`, `maybe_gc_needs=0`,
   `from_native=0` — so nothing ever *decides* to collect.

5. **Each collection is expensive and scales with CAPACITY, not live
   data.** `sweep_us=113175` of `total_us=147378` (77%) against
   `mark_us=1641`. Doubling the heap to 8 GB left the count (13) and the
   garbage (136,071,767) identical and **doubled** integration to
   1038.7 s. That is a second, independent defect: a cycle should cost
   what is live, not what is reserved.

## What this is NOT

Each was measured and eliminated, in this order:

* **the offload hook** — 1,271 refusals x 52 us = 67 ms, 0.02% of a frame;
* **the invoke cache** — the site-promotion work bought ~750 ns of a
  ~6,900 ns bench overhead, 11%;
* **the JIT admission gate, by count** — 7 methods blocked, all one-shot
  `<clinit>`s (`Integer`, `HexFormat`, `UUID`, ...);
* **CUDA host reservations shrinking the heap** — 8 GB changed nothing;
* **arena allocation refusal** — `hard_alloc_refusals=0`;
* **a wedge-break feedback loop** — the post-break refill retry runs 13
  times and succeeds **0**, so nothing is seeded and nothing re-arms;
* **lost escape analysis** — the arithmetic fitted perfectly (29.2 bytes
  an object) and was wrong: the baseline scalar-replaces nothing either,
  `scalar_new=[]` on all 21 traced kfusion compilations.

## The gate census was NOT undercounting — that claim was wrong

This page first recorded, as an open link, that `gpu_jit_gate_census`
reported 7 blocked methods with `Integration.integrate` absent while OSR
was refused for it. That was a reading error, not a defect: the 7 came
from a run that judged only 301 methods. A run that reaches the whole
pipeline reports

    gpu jit gate: methods judged=623 blocked_from_jit=31 admitted=592

and `Integration.integrate` IS in the blocked list. Both censuses agree.

What DID need fixing is separate and real: `compile_osr_artifact` sets
`osr_stage("entry")` and then four early gates `return None` without
setting a stage of their own, so every early refusal reported
`stage=entry` — sometimes a STALE `entry` left by a previous compile on
the same thread. The four were indistinguishable, and a genuine refusal
looked like a method that had never been considered. Each gate now names
itself and `osr_refusal_census` counts refusals where they are taken:

    osr refusals: attempts=28 refused_at_early_gate=4
      gpu-offload: 4
        sun/nio/cs/SingleByte.initC2B
        kfusion/tornado/algorithms/ImagingOps.bilateralFilter
        kfusion/tornado/algorithms/GraphicsMath.vertex2normal
        kfusion/java/algorithms/Integration.integrate

(no `--gpu`: 33 attempts, 0 refused.)

## The scale of it, which is not a kfusion quirk

The 31 blocked methods split 21 `writes-primitive-array` / 10
`calls-eligible-kernel`, and the second group is the alarming one. The
targets whose eligibility blocks their callers include:

    java/lang/Math.max(II)I
    java/lang/Math.min(II)I
    uk/ac/manchester/tornado/api/types/utils/FloatOps.sq(F)F
    uk/ac/manchester/tornado/api/types/utils/StorageFormats.toRowMajor...

`Math.min(II)I` is judged an offload-eligible KERNEL, so
`TornadoMath.clamp(III)I` is denied compilation, and so is anything
calling it. The 21 array-writers are the whole TornadoVM vector and image
accessor family — `Float3.set`, `Short2.set`, `Int3.set`,
`ImageFloat.get`/`set` — which is precisely what kfusion's inner loops
are built from. Under `--gpu` that entire data-structure layer is denied
JIT, on any application using it, whether or not anything ever offloads.

## Repro

```bash
cd apps/kfusion-tornadovm
CP="target/classes;target/*"
# 1-frame dataset: the layout is 16 + w*h*5 bytes per frame, so
# `head -c 1536016` of the converted .raw is exactly frame 0.
CRATONVM_DBG_OSR=1 cratonvm.exe        --java-home <jdk25> --Xms 4g -cp "$CP" \
    kfusion.java.Benchmark conf/bm-1f.settings 2>&1 | grep 'osr.*integrate'
CRATONVM_DBG_OSR=1 cratonvm.exe --gpu  --java-home <jdk25> --Xms 4g -cp "$CP" \
    kfusion.java.Benchmark conf/bm-1f.settings 2>&1 | grep -c 'osr.*integrate'
CRATONVM_GC_STATS=1 ...   # [GC] zgc-entry / zgc-trigger attribute the cycles
```

## FIXED 2026-09-04 — the BREADTH, not the trade

Both reasons above were far wider than the thing they protected, and
both narrowings landed in `fix/gpu-jit-gate-overbroad-20260904`. What is
left afterwards is a real trade, argued at the bottom of this page.

### `writes-primitive-array` — dissolved

This reason was standing in for a write barrier that neither compiled
tier had. Both lower primitive array stores INLINE — the single-pass
backend through `emit_int_astore_regs` and friends, the IR backend
through a raw `MOV`/`MOVSS` — so `jit_iastore`, the helper that DOES
call `input_cache::invalidate`, is reached from neither. Refusing to
compile the method was the only thing keeping the residency cache
coherent.

`jit::gpu_barrier` gives them one. A compiled store marks the written
array's bucket in a 64-byte side table; `input_cache::drain_compiled_writes`
evicts from Rust before any read of the cache, and at the top of
`remap_and_sweep` before the post-GC re-key, while the keys are still the
addresses the store bucketed. Eleven instructions, three of which run
when nothing is cached. Unarmed it emits nothing, so a run without
`--gpu` is byte-identical.

Two intrinsics that the gate could never have caught are covered by the
same barrier: `System.arraycopy` and `Arrays.fill` are lowered inline as
`REP MOVSB` / `REP STOS` and write a primitive array with **no `*astore`
opcode in the method at all**, so the gate's seven-opcode scan never saw
them. Those were admitted to the JIT even while every ordinary writer was
refused — a live hole, not one this branch opened.

### `calls-eligible-kernel` — narrowed to what can actually dispatch

`Eligible` answers "could this bytecode be lowered to PTX". The gate was
reading it as "could this be offloaded", which is a different question
with a different answer. `try_dispatch` applies two more gates to every
hit, and `offload_jit_gate::target_can_ever_dispatch` now mirrors both:

* only a `)V` map or a proven `)I`/`)J` reduction is transparently
  dispatched — anything else takes the PERMANENT `FallThrough` arm, not
  `FallThroughKeepHooked`, so it is refused on every call forever;
* the work estimate is `largest_primitive_array_len(args)`, which is `0`
  for every call to a descriptor with no array parameter, so with
  `--gpu-min-work` above zero such a target can never clear it.

`java/lang/Math.min(II)I` fails the first. So do `Math.max(II)I`,
`FloatOps.sq(F)F` and `StorageFormats.toRowMajor(III)I`.

### Measured on the real TornadoVM API jar

`tornado-api-5.2.0-jdk25.jar`, exercising `Float3.set`, `Int3.set`,
`ImageFloat.get`/`set` and `TornadoMath.min`/`max`; one binary, the two
kill switches as the control arm:

    CRATONVM_JIT_GPU_ARRAY_BARRIER=0        9 of 109 methods denied JIT (8.3%)
    CRATONVM_GPU_JIT_GATE_DISPATCHABLE=0
      Float3.set(IF)V                       writes-primitive-array
      Int3.set(II)V                         writes-primitive-array
      java/lang/Integer.<clinit>            writes-primitive-array
      java/util/HexFormat.<clinit>          writes-primitive-array
      sun/nio/cs/SingleByte.initC2B         writes-primitive-array
      ImageFloat.set(IIF)V    -> calls StorageFormats.toRowMajor(III)I
      ImageFloat.get(II)F     -> calls StorageFormats.toRowMajor(III)I
      TornadoMath.min(II)I    -> calls java/lang/Math.min(II)I
      TornadoMath.max(II)I    -> calls java/lang/Math.max(II)I

    default                                 0 of 109, identical output

### And the barrier is doing the work, not getting lucky

`bench-gpu/jit-writer-stale.sh` at `n=8192 rounds=6000`: 7 array writers
admitted, the barrier fires **5,497** times and evicts **21,988**
buckets, and all four checksums stay bit-identical to HotSpot.

That harness had to be hardened to show it. Its old default of 2000
rounds passed with the barrier firing ZERO times: `bump*` is called 2000
times for 128 iterations and never gets hot enough to compile, so every
store ran interpreted, every interpreted store evicts, and the fixture
proved nothing about the compiled tier it exists to test. It now fails
on `drains=0`.

`GpuWarm f 2^24 5` — the measurement that sent `ArrayWriterPolicy::AllowJit`
back in the first place, because giving up the cache cost 5x there —
shows no difference between the arms: `warm_ms` 9-10 with the barrier
against 10-13 without, same `SAMPLE`. The barrier keeps the cache AND
compiles the writers, which is why it is not a third point on that trade.

`runtime-stress.sh` (7 scenarios) and `ci-gate.sh` (4 gates) pass.

## What is LEFT, and why it is a trade rather than a bug

A caller of a kernel the dispatcher really can launch is still denied
JIT and OSR, and that is the case this gate was built for: compiling it
*would* stop the interpreter hook seeing the site, and offload would
silently end there. On kfusion the site refuses 1,271 times and
dispatches 3, so the trade is still 8x for an offload that almost never
happens.

A gate that gave up on a site after N refusals, the way the invoke cache
now can (`CRATONVM_GPU_MIN_WORK_GIVEUP`), is the obvious shape. It is
NOT implemented here, deliberately: the invoke cache's version is keyed
per call SITE and this gate's unit is a whole METHOD, so giving up would
kill every offload site in it at once — and the workload that would
show whether that pays is kfusion, whose dataset and build output are no
longer on this box. Building it without being able to measure it is how
the first site-promotion attempt earned an 18x regression.

So: the breadth is fixed and measured; the narrow refusal is recorded
here, unfixed, with the measurement a fix would have to produce.
