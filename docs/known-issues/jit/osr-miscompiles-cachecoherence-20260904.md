# OSR miscompiles `GpuRuntimeStress.cacheCoherence` — and the GPU gate was hiding it

**Status:** open, reproducible, unrelated to GPU offload.
**Found:** 2026-09-04, while investigating what looked like a defect in
the compiled-caller offload hook. It was not that.

## Repro

```bash
CV=target-gpu/release/cratonvm.exe   # or any build; --gpu is NOT needed
JDK=<jdk25>
TG=test_classes/gpu

"$JDK/bin/javac" -d "$TG" test_classes/gpu/GpuRuntimeStress.java
"$CV" --java-home "$JDK" -cp "$TG" GpuRuntimeStress 2 65536
```

| arm | `cache_coherence` |
|---|---|
| HotSpot | `-6705490297358015087` |
| `cratonvm --nojit` | `-6705490297358015087` |
| **`cratonvm` (JIT on, no `--gpu`)** | **`-8633255346386885231`** |
| `cratonvm CRATONVM_JIT_OSR=0` | `-6705490297358015087` |
| `cratonvm CRATONVM_JIT_DENY=GpuRuntimeStress.cacheCoherence` | `-6705490297358015087` |

Denying only `cacheCoherence` fixes it; denying `scale`, `sum` or `mix`
does not. `CRATONVM_JIT_OSR=0` fixes it; `CRATONVM_JIT_IR_ENABLE=0` and
`CRATONVM_JIT_OSR_DEAD_LOCALS=0` do not.

**So: the OSR compilation of `cacheCoherence` is wrong.** One method,
one tier, no GPU.

## The method

```java
static long cacheCoherence(int n) {
    int[] in = new int[n];
    int[] out = new int[n];
    for (int i = 0; i < n; i++) in[i] = i % 1013;
    long h = 0;
    for (int round = 0; round < 12; round++) {
        scale(in, out);
        h = mix(h, sum(out));
        in[round] = 999_000 + round;
        for (int i = round; i < n; i += 1024) in[i] = i ^ round;
        if (round == 6) {
            for (int i = 0; i < n; i++) in[i] = (i * 7 + round) % 4099;
        }
    }
    return h;
}
```

Three nested loop shapes in one method, an outer loop carrying a `long`
accumulator, a strided inner loop whose start depends on the outer
induction variable, and a conditional third loop entered once. The
answer is a hash of every round, so any one wrong element anywhere
changes it — which is why this is a good detector and a poor localiser.

## Why nothing caught it

`bench-gpu/runtime-stress.sh` runs three arms, and **not one of them
compiles this method**:

* HotSpot — the oracle.
* `cratonvm --nojit` — the control, interpreted by construction.
* `cratonvm --gpu` — the arm under test, where
  `runtime::offload_jit_gate` **refuses to compile** `cacheCoherence`:
  it both writes a primitive array and calls the offload-eligible kernel
  `scale`, so both of that gate's reasons fire.

The GPU gate was masking a JIT correctness bug. That is the general
lesson and it is worth more than this one defect: a gate that keeps
methods interpreted removes them from every differential suite that runs
the compiled tier only through that gate.

It surfaced because `CRATONVM_GPU_JIT_GATE_CALLERS=hook`
(`docs/known-issues/perf/gpu-compiled-caller-offload-hook-20260904.md`)
lets such a caller compile for the first time. That mode is opt-in, so
nothing regressed — but the reason it is opt-in is now THIS defect, not
anything wrong with the hook.

## Next step

Narrow inside the method. The three loops can be bisected by editing a
copy of the fixture — drop the `round == 6` loop, then the strided loop,
then the single-element store — and re-running each against `--nojit`.
`CRATONVM_JIT_OSR_SINGLE_PC` and the `CRATONVM_DBG_OSR_*` family will
say which back edge the OSR entry bound to, which is the other half of
the question.

Worth checking first whether the strided loop's `i += 1024` with a
non-zero start (`i = round`) is the trigger: an OSR entry into a loop
whose induction variable did not start at zero is the least-travelled
shape here.
