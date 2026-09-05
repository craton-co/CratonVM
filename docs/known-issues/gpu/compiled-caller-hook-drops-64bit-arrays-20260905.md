# The compiled-caller offload hook silently drops `long[]` and `double[]`

## Status

**Found and scoped 2026-09-05, not fixed.** Regression from
`01256e4fb feat(gpu): a compiled caller offloads by default`, landed the
same day. Kernels taking a 64-bit element array stop offloading
entirely: the site compiles, the caller runs, and the device is never
touched. Up to **9x slower** on a compute-heavy kernel.

The way back is the kill switch that commit already ships:
`CRATONVM_GPU_JIT_GATE_CALLERS=block` restores the pre-2026-09-05
behaviour.

## What happens

`GpuIntensitySweep <type> 262144 30 16` — one `--gpu` binary, the two
gate modes, `--gpu-min-work 1` so nothing is refused on size:

| type | width | default (new) | `=block` (old) | ratio | device allocs, default |
| --- | ---: | ---: | ---: | ---: | --- |
| `byte[]`   | 1 |   192,303 ns | — | — | `cuMemAlloc=3` |
| `int[]`    | 4 |   480,253 ns | — | — | `cuMemAlloc=3` |
| `long[]`   | 8 | 2,337,923 ns |   625,140 ns | **3.7x slower** | **none** |
| `double[]` | 8 | 7,473,736 ns |   724,520 ns | **9.4x slower** | **none** |

`double[]` reproduces 3/3: 9.05x, 8.53x, 8.63x.

It is exactly the two 64-bit element kinds — `ParamKind::I64Array` and
`F64Array` — and only those. `byte[]` and `int[]` offload normally in
the same mode, in the same runs, which is what rules out a broken
counter or a mismeasured arm.

## Why the counter is trustworthy here

"Did not offload" is not read off a timing difference. Under the new
default a `long[]` run prints **no `gpu events` line and no
`cuMemAlloc`** at all, while the same binary in `=block` mode prints
`gpu events: created=2  cuMemAlloc=3  pooled=30`. Device allocation is
an independent witness: a kernel that ran cannot have allocated nothing.

The `gpu dispatch memo` census agrees (0 lines vs 1), and `byte[]`
prints it under the new default, so the census is not simply absent in
that mode.

## Why this is attributable to the commit, not the build

`CRATONVM_GPU_JIT_GATE_CALLERS` is a **within-binary** lever. `=block`
is the pre-2026-09-05 behaviour, kept by that commit explicitly as a
control arm — its own doc says the refusal's cost "cannot be attributed
by comparing `--gpu` against no `--gpu`, which differ in everything else
the device touches". So both arms above are one binary, one commit, one
run, differing only in the gate. No cross-binary confound.

## Likely cause

`long` and `double` are JVM category-2: two operand-stack slots each.
The compiled-caller hook is new code that reads the call's arguments at
a compiled site; a hook that walks argument slots as if every parameter
were category-1 would mis-locate a 64-bit array reference, and the
cleanest failure for that is to decide the site is not offloadable
rather than to dispatch wrongly. That is a hypothesis from the scoping,
not a diagnosis — nothing in the hook has been read.

Worth checking first: whichever argument-walk the compiled hook uses
against `ParamKind::I64Array` / `F64Array`, and whether it agrees with
the interpreter hook that `=block` leaves in charge.

## Why the ops=1 sweeps did not catch it

At one arithmetic op per element, offloading a `long[]` or `double[]`
LOSES (0.52x-1.01x, see
`docs/gpu/offload-crossover-and-min-work-20260904.md`), so refusing to
offload them looks like an improvement — the new default measured
*faster* at `n=65536, ops=1` (106 us vs 207 us) precisely because it
declined a bad offload.

The regression only appears where offload pays. At ops=16 the same
kernels win 5-13x, and not taking that win is the 9x above. **A
single-intensity measurement would have passed this commit**, which is
the same shape of error recorded twice already on the crossover page:
a threshold, or here a behaviour change, evaluated on one axis of a
two-axis space.

## Repro

```bash
# needs a --features gpu-driver binary and the fixture
#   (test_classes/gpu/*.class is gitignored; bench-gpu/crossover-n.sh
#    compiles it on demand)
CV=./cratonvm-gpu.exe
TG=test_classes/gpu
for mode in default block; do
  env=""; [ "$mode" = block ] && env="CRATONVM_GPU_JIT_GATE_CALLERS=block"
  env $env $CV --gpu --gpu-min-work 1 --java-home <jdk25> --Xmx 8g \
      -cp "$TG" GpuIntensitySweep D 262144 30 16 2>&1 \
    | grep -oE 'ns_per_call=[0-9]+|cuMemAlloc=[0-9]+'
done
```

Expect the default arm to print a `ns_per_call` roughly 9x the `block`
arm's and **no** `cuMemAlloc` line.
