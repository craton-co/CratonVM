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

### The annotation half, closed the same day

The first version of this fix passed the pool but **not** annotations,
on the grounds that hint-loosened kernels were a documented, deliberate
"Known limitation". Re-reading that limitation after measuring this one
retired it. It said:

> intentionally conservative in the direction that costs offload
> throughput, not correctness — the worst case is a JIT-compiled caller
> that stops offloading

That worst case is the 10.8x above. The same sentence would have
justified the constant-pool gap right until it was priced, so "costs
throughput, not correctness" is not a reason to leave a gate asking a
different question from the dispatcher it models.

The gate now calls `analyze_with_annotations_and_pool` with the target's
own annotations — the identical call `lookup_or_compile` makes, reusing
its `decode_method_attrs`. It costs one attribute decode per scanned
target, beside a full bytecode scan already being paid.

The disagreement ran **both** ways, and only one of them was predictable
from the docs. Measured on `test_classes/gpu/annotations/GateAnnotationParity.java`,
two builds differing only by this change:

| kernel | before | after |
| --- | --- | --- |
| `hinted` — eligible only via `ALLOW_INTRINSIC_CALLS` | no census at all: never registered, **site went dark** | `cuMemAlloc=3`, `considered=40 offloaded=40` |
| `excluded` — `@GpuExclude` | `considered=40 offloaded=0`: hook **armed 40 times** for a method the dispatcher always refuses | no census: never registered |

The second row is the one the docs did not predict. An annotation-free
gate calls a `@GpuExclude` method `Eligible` and registers it, arming the
offload hook for a target `lookup_or_compile` short-circuits and never
launches — and under `CallerGateMode::Block` that would deny its caller
compilation for an offload that cannot happen.

What is still deliberately excluded: forward class references and
`invokedynamic`-mediated calls, both still listed in the module docs.

### Found twice, independently, on the same day

`f5c7963e9 fix(gpu): the JIT gate and the dispatcher disagreed about any
kernel with an FP constant` landed on `dev` while this was being scoped,
and makes the identical one-line change. It arrived from a different
symptom: `bench-gpu/GpuFloatDivChain.divChain`, whose `+ 1.0000001` is an
`ldc2_w`, measured at 9,276 ms against its int twin's 8 ms.

Its framing is narrower than the defect. That commit reasons that the int
twin worked because `+ 12345` is a `sipush` "with no pool entry" — true
of *that* constant, but it generalises the wrong way: `12345` is inside
`sipush` range (±32767) and `1000003` is not. **The boundary is the
constant pool, not the type**, which is what `GpuLdcSplit` above shows —
an `int[]` kernel with a large constant went dark and a `long[]` kernel
with only `lconst_1` never did. Two independent scopings of this bug each
generalised from the types their own fixture happened to use ("64-bit
arrays" here, "FP constants" there), and both were wrong in the same way.

## Verified

RTX 2060 (sm_75), CUDA 13.3, driver 610.88, Windows 11, JDK 25.0.3.
`GpuIntensitySweep <type> 262144 30 <ops>`, `--gpu-min-work 1`.

Engagement, before → after — all eight cells now dispatch:

| | ops=1 | ops=16 |
| --- | --- | --- |
| `byte[]` | offloads → offloads | offloads → offloads |
| `int[]` | offloads → offloads | offloads → offloads |
| `long[]` | **dark** → offloads | **dark** → offloads |
| `double[]` | **dark** → offloads | **dark** → offloads |

Timing at ops=16. The **before** column was measured on the pre-fix
binary at `632d25166`, single run — it is a 10x effect and does not need
repeats. The **after** columns are medians of 5, with the full sample
range, because at this size the run-to-run spread on a desktop is wide
enough to invent a result from one sample:

| type | before (pre-fix) | after, default | after, `=block` control |
| --- | ---: | ---: | ---: |
| `double[]` | 8,606,336 ns | **775,880** (741k–810k) | 874,576 (833k–1134k) |
| `long[]` | — | **739,426** (681k–964k) | 690,376 (673k–1076k) |

Read that table carefully: the fix removes a **10.8x** loss, and after it
the default and `=block` arms are at **parity — the difference is inside
the noise, and its sign flips by element type**. An earlier draft of this
page claimed the default arm "edges out" the control on 730,570 against
767,843; that was one sample each, a 5% gap on a distribution that spans
50%, and it did not survive repeats. Parity is the correct claim and the
expected one: `=block` reaches the device through the interpreter hook,
so both arms end up offloading the same kernel, and what the fix buys is
that the caller no longer has to stay interpreted to get there.

Battery at this commit: `ci-gate.sh` 5/5, `gate-overbroad.sh` PASS,
`runtime-stress.sh`, `marshal-stress.sh` (all six kernels engaged),
`jit-writer-stale.sh`, and `cargo test -p cratonvm-vm --features
gpu-offload --lib` (2701 passed). `residency-gc.sh` passes, but flaked
once in 25 runs — see the note below; it is not this defect.

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

## An unrelated flake seen while verifying this — and what it turned out to be

`residency-gc.sh` failed intermittently during verification: 1 in 25
first, then **2 in 40** on a binary that already carried both
`input_cache` race fixes. It is not this defect and not the gate — that
script never exercises the compiled-caller registration path.

**A first guess at the cause was wrong, and the refutation is one line of
fixture.** This page originally blamed the two races in
[concurrent-dispatch-wrong-answer-20260905.md](concurrent-dispatch-wrong-answer-20260905.md),
because both live in the cache `residency-gc.sh` hammers. But
`test_classes/gpu/GpuResidencyGc.java` contains **no `Thread`,
`Executor`, `parallel` or stream** — `main` is a sequence of direct
calls — and GC relocation runs with the world stopped. Both races need
two Java threads racing in the cache, so **neither can fire here**. The
2-in-40 above, on a binary with both fixes in, says the same thing
empirically. "Lives in the same module" is not attribution.

**What it actually is.** The script routes each arm's stderr into a temp
directory it deletes, so the failure looked like an empty arm. Captured
directly, it is a VM panic:

```
panic: forwarding target must have its low 2 bits clear (>= 4-byte aligned)
  types/src/heap_types.rs:1562
[PANIC_IN] GpuResidencyGc.main pc=127   thread="main-vm"
```

Same assert text and same file as **Cluster D** of the OPEN page
`g1-evac-forwarding-assert-and-three-sigsegv-clusters-20260905.md`
(recorded there at `heap_types.rs:1529`; the file has moved since that
binary), and the same G1-only scope. One difference to keep in view: that
page's cluster panics on an evac worker via `gc/src/evac_pool.rs`, this
one on `main-vm`.

If it is the same defect, the useful part is the reproducer: that page's
is a 640-class Tomcat suite run on Azure, and this is a single local
fixture — though see the rate below before treating it as convenient.

**Rate, corrected.** An earlier draft of this note put it at "~4%",
reading 3 failures in 66 `residency-gc.sh` runs as a per-run rate. That
is the wrong denominator: each script run launches the VM about twelve
times (three collectors x four arms), so the observed events are roughly
3 in 800 launches — order **0.4% per launch**, ten times rarer than
stated.

**Not established: whether `--gpu` is required.** An alternating A/B of
150 launches per arm returned **0/150 on both**. That is not evidence of
independence — at 0.4% it expects about one event per arm, so it cannot
tell 0 from 1. It does refute the inflated 4% figure, which would have
made 0/300 essentially impossible.

Reproducing this wants either a few thousand launches per arm or an
amplifier. Worth noting the events clustered: three fell within one
stretch of loaded-host activity and none in 300 launches afterwards, so
whatever forces it may be load- or timing-dependent rather than uniformly
random, and a quiet-host zero should not be read as a fix.
