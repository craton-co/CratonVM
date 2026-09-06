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
[concurrent-dispatch-wrong-answer-FIXED-20260905.md](concurrent-dispatch-wrong-answer-FIXED-20260905.md),
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

---

# Residuals, closed 2026-09-06 — and the one that was the same defect again

This page shipped with three things still open. Two were listed as
"deliberately excluded" in the module docs; the third was an unexplained
flake noted at the bottom. All three are settled here, and the page moves
to the internal tree.

## 1. Forward references: the same defect, one class away — 7.5x

The module docs listed this as a known limitation and argued it as a
corner case:

> A caller compiled/scanned before its callee's class has ever been
> loaded will not see the callee as eligible and will NOT be blocked
> from JIT admission — offload can still be silently dropped for that
> specific caller.

Every clause of that is either an understatement or wrong.

**It is not a corner case.** `offload_jit_gate` scans a caller when the
caller is ADMITTED to the JIT, which happens *before* the caller runs. A
callee in another class has therefore usually never been touched at that
moment — the `invokestatic` under judgement is the very thing that will
load it. So the gate cannot judge a cross-class kernel *by
construction*, not by accident of ordering.

**The drop is not limited to "that specific caller", and it is
permanent.** An unjudged target is never registered with
`jit_cuda::offload_hook`, and `helpers.rs::try_compiled_offload` memoized
`CompiledOffloadSite::NotKernel` per call site for the life of the
process. The first execution of the site decided, from a registry that
could not yet know, and the class that same dispatch was on its way to
loading could not change the answer.

**It was worth 7.5x.** Measured on `test_classes/gpu/GpuForwardRef.java`,
built for this: two kernels with byte-identical bodies and signatures,
called from the same driver method, differing in NOTHING but which class
each is declared in.

RTX 2060 (sm_75), CUDA 13.3, Windows 11, JDK 25.0.3. One binary,
`--gpu --gpu-min-work 1`, n=262144, 40 iterations, 5 runs per cell.
`CRATONVM_GPU_JIT_GATE_LATE_REGISTER=0` is the control arm — it restores
the old site memo, so this is a same-binary A/B and not two builds.

| arm | `late-register=0` (the old behaviour) | `late-register=1` (default) |
| --- | --- | --- |
| `sameclass` — `GpuForwardRef.scaleHere` | **40/40 dispatch**, 10.2–11.1 ms | **40/40 dispatch**, 10.4–13.1 ms |
| `otherclass` — `GpuForwardRefKernel.scale` | **0/40 dispatch**, 96.2–116.1 ms | **39/40 dispatch**, 13.6–14.6 ms |

Medians on the `otherclass` row: **104.9 ms → 14.0 ms, a 7.5x loss
removed**. The checksum was identical in all twenty runs, which is the
whole problem — the kernel computes the same answer on the CPU, so
nothing that checks answers could ever have seen this.

Read the `sameclass` row as the control it is: it does not move, in
engagement or in time, when the flag does. The variable really is the
declaring class and nothing else.

39 of 40, not 40 of 40, on the fixed `otherclass` arm. That is correct
and not a residual: the first execution of the site is the thing that
loads the class, so the helper re-asks on the second call and offloads
from there on. Requiring 40 would be a gate that fails on right
behaviour, and `ci-gate.sh`'s new gate f allows exactly one.

### Why the whole battery was green while this was live

Every other GPU fixture in the tree declares its kernels beside its
driver. `GpuLdcSplit`, `GpuIntensitySweep`, `GpuProbe`, `GpuWarm`,
`GpuResidencyGc`, `GpuRuntimeStress` — all of them, without exception.
A kernel in the caller's own class is loaded by construction the moment
anything calls the caller, so the gate can always judge it and the
registry is always populated.

`ci-gate.sh` 5/5, `runtime-stress.sh`, `marshal-stress.sh`,
`jit-writer-stale.sh` and 2700 unit tests could not have found this and
did not. The fixture that finds it had to be written to have a second
class, which is a thing no existing one has. `bench-gpu/RayTracerKernel`
is the one real workload in the tree shaped like this, and it is a
benchmark, not a gate.

### The census said nothing, again — and now says it

This is the second time on this page that a narrowing was invisible
because it showed up only as an ABSENCE. The analyzer refusal that
caused the original defect was a bare `continue` with no counter; the
"class not loaded" skip beside it was a bare `continue` with no counter
too, and the two were three lines apart.

Both are counted now. Under the old behaviour the run above prints

```
gpu jit gate: forward-referenced targets=11 (class not loaded when the
              caller was scanned), late-registered kernels=0
```

and under the fix

```
gpu jit gate: forward-referenced targets=11 ... late-registered kernels=1
gpu jit gate:   registered late, by the compiled site: GpuForwardRefKernel.scale([I[I)V
```

`late-registered` is the actionable number: it counts kernels the caller
scan could not see and the compiled site recovered. A non-zero value is
not a warning, it is the fix working. The `forward-referenced` counter
beside it exists so a run where that number is zero can be told from one
where the question never arose — which is the lesson this page already
had to learn once.

### The fix

Three parts, all in the direction of "ask the dispatcher's own question
at a moment when it can be answered":

* `try_compiled_offload` no longer treats a registry miss as `NotKernel`.
  It asks `offload_jit_gate::target_is_dispatchable_kernel` — the
  identical judgement a caller scan makes, now factored into
  `judge_target` so the two cannot drift — and `note_kernel`s a kernel it
  finds. Registering also repairs every LATER compile, since
  `jit/src/lib.rs` and `jit/src/x64/bytecode_walk.rs` read the same
  registry to decide whether to bind a static call directly or inline it,
  and both of those are one-way once taken.
* A site whose class is STILL unloaded becomes
  `CompiledOffloadSite::Unresolved` rather than `NotKernel`, so the next
  call re-asks. Capped at four tries, for the case where the dispatch
  that would have loaded the class throws instead.
* `offload_hook::arm()`. `jit_invoke_dispatch` consults the hook only
  `if any_kernels()`, and that used to become true only once a caller
  scan had already found a kernel — so a program whose only kernel is
  forward-referenced registered nothing, the flag stayed false, the
  helper never looked, and nothing ever registered it. **The empty case
  sealed itself shut.** Arming on "there is a device" rather than "we
  found something" is what lets the registry learn at all. A run without
  `--gpu` never reaches it and still pays one relaxed bool per compiled
  static dispatch.

The gate query is not free, so it is screened by
`descriptor_could_ever_dispatch` first: `try_dispatch` launches only
`)V`, or `)I`/`)J` for a reduction, and above `--gpu-min-work` 0 it needs
an array parameter. That is read off `info.descriptor` with no lock, no
allocation and no bytecode, and it rejects nearly every compiled static
call site in a program before anything more expensive runs.

What is NOT closed, and it is much narrower than the old bullet: a site
recompiled BETWEEN the first and second execution of a forward-referenced
call — the window in which the class exists, the registry does not yet
know, and a fresh compile can bind directly or inline.

## 2. `invokedynamic`-mediated calls: agreement, not disagreement

The module docs listed this beside the constant-pool and annotation gaps,
which reads as a third instance of the same mistake. It is not one, and
saying so is worth a paragraph on a page whose whole subject is a gate
asking a different question from the dispatcher it models.

Both dispatcher doors are `invokestatic`-only:
`interpreter/dispatch_static.rs`'s hook is inside `execute_invokestatic`,
and `jit/helpers.rs::jit_invoke_dispatch` guards its offload attempt on
`info.invoke_kind == 3`. Neither fires for an `invokedynamic`, so a
`MethodHandle`-mediated kernel does not offload from the interpreter
either. The gate treating it as out of scope is the two asking the SAME
question. It is a feature the offload path does not have — not a gate
modelling the wrong dispatcher — and it stays open as a feature request
rather than as a defect.

## 3. The `residency-gc.sh` flake: closed, and what the script does now

The bottom of this page recorded an intermittent `residency-gc.sh`
failure at roughly 0.4% per VM launch, captured as

```
panic: forwarding target must have its low 2 bits clear (>= 4-byte aligned)
```

and hypothesised as the same defect as Cluster D of
`g1-evac-forwarding-assert-and-three-sigsegv-clusters-20260905.md`.

That hypothesis was right and the page it names has since been fixed and
retired — it now lives at
`docs/internal/tomcat/...-the-serial-arms-header-screens-FIXED-20260905.md`.
The retiring commit `0d28beda7` answers this page's open question
directly: the misaligned target came from the SELF-FORWARD candidate, the
three SIGSEGV clusters share one root cause (the unclamped array/flat
walks write forwarding addresses past the holder), and the kill-switch
A/B it said was never attempted runs 3 CRASH / 6 with the screens off and
0 / 6 with them on.

So the "not established: whether `--gpu` is required" note is moot: the
defect is not GPU-specific and was never in this subsystem. The 2-in-40
on a binary carrying both `input_cache` fixes was correct evidence and
correctly read.

`residency-gc.sh` was nonetheless RED on `dev` as of 2026-09-06, for
something else entirely: a deterministic Generational SIGSEGV, fixed the
same day and written up at
[native-arg-snapshot-stale-across-java-reentry-FIXED-20260906.md](../fixed-bugs/native-arg-snapshot-stale-across-java-reentry-FIXED-20260906.md).
It reproduced 3/3 on a pristine build of this branch's own parent commit,
interleaved against the branch binary, and it turned out to need neither
the GPU nor the JIT gate — `--gpu` only amplified it. Worth reading beside
this one for the contrast: that was a crash under one collector, this was
a silent 7.5x under all of them.
