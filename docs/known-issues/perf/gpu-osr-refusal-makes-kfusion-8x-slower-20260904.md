# `--gpu` refuses OSR for the hot loop, and kfusion runs 8x slower

## Status

**Root-caused and measured. Not fixed.** The chain below is established
end to end by counters added on 2026-09-03/04, all of which are on `dev`.
The last link — *why* the gate census does not count the refusal — is
open, and is a defect in the instrument rather than in the VM.

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

## The open link

`gpu_jit_gate_census` reports 7 blocked methods and
`Integration.integrate` is not among them, yet OSR is refused for it.
Either the OSR site reaches a refusal that bypasses `caller_blocks_jit`,
or the census under-counts because the verdict is cached per
`(vm, class, method)` and only first computations are counted. Until
that is resolved the "7 blocked" figure should not be trusted as a
measure of the gate's reach.

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

## What a fix has to weigh

The gate is not wrong to exist: OSR-compiling the loop *would* stop the
interpreter hook seeing that call site, and offload would silently end
there. But on this workload the trade is 8x for an offload that never
happens — the site refuses 1,271 times and dispatches 3. A gate that
gave up on a site after N refusals, the way the invoke cache now can
(`CRATONVM_GPU_MIN_WORK_GIVEUP`), would keep the hook for sites that
actually offload and hand the loop back to OSR for sites that do not.
