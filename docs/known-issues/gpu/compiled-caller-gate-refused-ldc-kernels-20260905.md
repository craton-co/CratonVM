# The compiled-caller gate refused every kernel with a constant-pool constant

> Renamed 2026-09-05, from
> `compiled-caller-hook-drops-64bit-arrays-20260905.md`. That title named
> the symptom this was first scoped by, and the symptom was a coincidence:
> the defect has nothing to do with element width. The old name is kept in
> git history via the rename.

## Status

**Found, diagnosed and FIXED 2026-09-05.** Regression from
`01256e4fb feat(gpu): a compiled caller offloads by default`, landed the
same day — though the bug it exposed was written the day before, in the
gate's own analyzer call.

Kernels whose **body needs a constant-pool constant** (`ldc`, `ldc_w`,
`ldc2_w`) stopped offloading entirely once their caller compiled: the
site compiled, the caller ran, and the device was never touched. Up to
**10.8x slower** on a compute-heavy kernel.

## The cause

`offload_jit_gate` scans a candidate caller's `invokestatic` targets and,
in `CallerGateMode::CompiledHook`, registers each offloadable one with
`jit_cuda::offload_hook` so the compiled dispatch helper will consult it.
A target that is never registered has its call sites bound directly, and
they go dark.

The scan judged each target with `jit_cuda::analyzer::analyze` — the
**constant-pool-free** entry point. Its own doc comment says what that
means:

> This entry point has no constant pool, so `ldc`/`ldc_w`/`ldc2_w`
> always reject with `Reason::LoadConstant` regardless of what they
> target — see `analyze_with_pool` for the CP-aware variant that admits
> numeric-literal `ldc`.

But the dispatcher this gate exists to *predict* calls
`analyze_with_annotations_and_pool`, which admits a numeric literal. So
the gate was strictly more conservative than the thing it models, and
every kernel with a pool constant was judged ineligible, never
registered, and silently un-offloadable.

### Why it looked like a 64-bit problem

Java has no small-immediate form for a `long` or `double` literal.
`3` is `bipush`; `3L` and `3.0` are `ldc2_w`. So in the fixture that
found this — `GpuIntensitySweep`, whose kernels are `v * 3 - 7` in four
element types — the `long[]` and `double[]` kernels were exactly the ones
that always tripped the gate, and the `int[]`/`byte[]` ones exactly the
ones that never did. The correlation was perfect and entirely
coincidental.

**Measured, on a fixture built to break the correlation**
(`test_classes/gpu/GpuLdcSplit.java`, one binary, `--gpu-min-work 1`):

| kernel | element type | constants | before | after |
| --- | --- | --- | --- | --- |
| `smallConstI` | `int[]` | `bipush` | offloads 40/40 | offloads 40/40 |
| **`bigConstI`** | **`int[]`** | `ldc` (> `sipush`) | **DARK** | offloads 40/40 |
| **`noLdcJ`** | **`long[]`** | `lconst_1` only | **offloads 40/40** | offloads 40/40 |
| `ldcJ` | `long[]` | `ldc2_w` | DARK | offloads 40/40 |

An `int[]` kernel goes dark and a `long[]` kernel is fine. Element width
was never the variable; the constant pool was.

## The fix

Pass the target class's constant pool:
`analyze_with_pool(target_method, &target_class.constant_pool)`.

Annotations are deliberately still not passed — hint-loosened kernels
remain the documented "Known limitation" in the module docs. That one is
a choice; the pool-free call was not.

## Verified

RTX 2060 (sm_75), CUDA 13.3, driver 610.88, Windows 11, JDK 25.0.3, at
`632d25166`. `GpuIntensitySweep <type> 262144 30 <ops>`, `--gpu-min-work 1`:

Engagement, before → after — all eight cells now dispatch:

| | ops=1 | ops=16 |
| --- | --- | --- |
| `byte[]` | offloads → offloads | offloads → offloads |
| `int[]` | offloads → offloads | offloads → offloads |
| `long[]` | **dark** → offloads | **dark** → offloads |
| `double[]` | **dark** → offloads | **dark** → offloads |

Timing at ops=16, against `CRATONVM_GPU_JIT_GATE_CALLERS=block` as the
within-binary control:

| type | before (default) | after (default) | control (`=block`) |
| --- | ---: | ---: | ---: |
| `double[]` | 8,606,336 ns | **730,570 ns** | 767,843 ns |
| `long[]` | — | **639,333 ns** | 646,256 ns |

The default arm now edges out the control, which is what "the caller
compiles AND the site still offloads" is supposed to buy. Before, it lost
to it by 10.8x.

Battery: `ci-gate.sh` 5/5, `gate-overbroad.sh` PASS, `runtime-stress.sh`,
`marshal-stress.sh` (all six kernels engaged), `residency-gc.sh` on three
collectors, `jit-writer-stale.sh`, and
`cargo test -p cratonvm-vm --features gpu-offload --lib` (2699 passed).

## Why `=block` appeared to be a clean control, and only half was

The original scoping used `CRATONVM_GPU_JIT_GATE_CALLERS=block` as the
"old behaviour" arm and saw the 64-bit kernels offload there. That is
real but incidental. Under `Block` the gate `return`s **true on the first
eligible target it finds**, and `GpuIntensitySweep.run` calls all twelve
kernels — so the `int[]`/`byte[]` ones (which never needed the pool)
blocked `run` outright, it stayed interpreted, and the `double[]` calls
then offloaded through the *interpreter* hook.

A caller that only ever called `ldc`-using kernels would have gone dark
under `=block` too. The control arm was measuring "this caller was
blocked for some other reason", not "the old path handles 64-bit arrays".

## What made it invisible: a refusal with no census

The gate's other two narrowings each have a counter, on an argument
already written into `types::gpu_jit_gate_census`:

> Both narrowings are silent by construction — they show up as methods
> that are NOT in the list above — so without these two counters a run
> where neither fired and a run where both did read identically.

The analyzer refusal was a third narrowing of exactly that shape and had
**no counter at all**. It was a bare `continue`. So a run in which every
`ldc` kernel had silently stopped registering printed a `gpu jit gate`
census byte-for-byte identical to a healthy one — verified: the `int[]`
and `double[]` runs differed in nothing but `compiled-write drains`.

Now counted, and **split**, because a census that folds "never a
candidate" into "refused" is one nobody reads — the first version emitted
172 refusals and named 48, nearly all JDK bootstrap:

* **never-a-candidate** (`NonStatic`, `NativeOrAbstract`, `NoCode`,
  `BadDescriptor`, `UnsupportedParamType`, `UnsupportedReturnType`,
  `Synchronized`, `GpuExcluded`) — counted only. One `GpuIntensitySweep`
  run walks past ~154.
* **kernel-shaped, body refused** (`LoadConstant`, `Compare`, `Invoke`,
  `Allocation`, …) — counted **and named, with the reason**. ~18 per run,
  and the actionable half.

Under the broken gate this would have printed

```
gpu jit gate:   kernel-shaped, body refused: GpuIntensitySweep.workD16([D[D)V — LoadConstant
```

which names the defect outright. Confirmed end to end that a kernel-shaped
body refusal really is named and reasoned
(`test_classes/gpu/GpuLdcNamed.java`).

## Repro (of the original defect)

```bash
CV=target-gpu/release/cratonvm.exe
TG=test_classes/gpu
for w in smallConstI bigConstI noLdcJ ldcJ; do
  "$CV" --gpu --gpu-min-work 1 --java-home <jdk25> --Xmx 8g \
      -cp "$TG" GpuLdcSplit $w 262144 40 2>&1 \
    | grep -oE 'cuMemAlloc=[0-9]+|considered=[0-9]+ offloaded=[0-9]+'
done
```

Before the fix, `bigConstI` and `ldcJ` print nothing — no `cuMemAlloc`,
no compiled-caller census line at all, because the site was never even
*considered*. The census being **absent** rather than zero is itself the
signal: `gpu_compiled_offload_census::exit_summary` returns early when
every counter is zero.
