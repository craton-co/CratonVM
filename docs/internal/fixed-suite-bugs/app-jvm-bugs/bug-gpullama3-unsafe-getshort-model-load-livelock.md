# GPULlama3.java — CratonVM livelocks loading a real GGUF model (HotSpot completes fine)

## Status
**OPEN** (2026-08-21). CPU-only path (no `--gpu`, no TornadoVM annotations exercised).

> **2026-08-23 re-confirmed on current dev, and re-created after this file
> vanished from disk** (this doc, along with a second one, disappeared
> between when they were written and a later check in the same session —
> `C:\craton\CratonVM` is a shared worktree and another concurrent session
> was actively running tests in it at the time; not forensically chased
> down, just recreated from this session's own record). Rebuilt from
> `origin/dev` HEAD `3ed73bf89` (into an isolated `target-retest/` dir to
> avoid a live `cratonvm.exe` the other session had locked in `target/`),
> reran the identical repro. Same signature: the process ran 68+ minutes
> before being killed (vs. HotSpot's ~4s), CPU-active the whole time
> (`TotalProcessorTime` advancing ~1:1 with wall time), `WorkingSet`
> essentially flat (~200KB growth over a 10s sample against a 2.4GB model)
> — still spinning without advancing, not blocked. Not yet fixed.

## Severity
**HIGH** — CratonVM never completes model load for this real-world app; the
process must be force-killed. Not a slowness/perf gap — CPU time is actively
being burned with no forward progress.

## Context

Third of three "GPU apps" triaged this session (after the raytracer kernel
benchmark and TornadoVM's own GPU-path correctness bug, both handed off
separately). [GPULlama3.java](https://github.com/beehive-lab/GPULlama3.java) is
a real, actively-maintained JVM-native LLM inference engine (Llama/Qwen/Phi/
Granite via GGUF). Its GPU kernels are written against TornadoVM's own
`@Parallel`/`TaskGraph` API, not CratonVM's `@GpuKernel` annotation model, so a
GPU-offload port is not mechanically attemptable — the tractable CratonVM test
is the app's plain CPU-only inference path (`LlamaApp`, `onGPU=false`), which
is pure JVM correctness/compat, independent of either VM's GPU offload story.

## Symptom

HotSpot (Temurin 25.0.3) runs the CPU path correctly:

```
java --add-modules jdk.incubator.vector -cp target/gpu-llama3-1.0.0-jdk25.jar \
  org.beehive.gpullama3.LlamaApp -m Llama-3.2-1B-Instruct-F16.gguf \
  -p "Explain GPU acceleration in one sentence." -n 40
```
→ coherent English output, `achieved tok/s: 10.91` in 3.67s.

The identical invocation against CratonVM (`target/release/cratonvm.exe`,
build 0.3.0, dev HEAD 2026-08-21):

```
cratonvm.exe --java-home "<jdk25>" --add-modules jdk.incubator.vector \
  -cp target/gpu-llama3-1.0.0-jdk25.jar org.beehive.gpullama3.LlamaApp \
  -m Llama-3.2-1B-Instruct-F16.gguf -p "Explain GPU acceleration in one sentence." -n 40
```

boots, prints its usual clinit-fixup `gc::guard` WARN lines (W7-84 autobox,
G30-1 descriptor-coercion — both pre-existing, logged-and-recovered, unrelated
to this app), then **never produces output**. Killed after ~4 minutes.

## Livelock, not a block — process samples

PID sampled 3x at 8s intervals mid-run (`Get-Process -Id <pid>`):

| t | TotalProcessorTime | WorkingSet |
|---|---|---|
| +0s | 00:01:10.78 | 580,751,360 |
| +8s | 00:01:18.89 | 580,816,896 |
| +16s | 00:01:27.02 | 580,816,896 |

CPU time advances ~1:1 with wall time (actively running, not blocked on I/O or
a lock) while **WorkingSet is flat** at ~580MB against a 2.4GB model file —
i.e. it is not progressively mmap'ing/reading further into the GGUF file. This
is the signature of a spin loop that keeps re-touching the same already-mapped
pages rather than a wait — a genuine livelock, not a slow-but-progressing load.

## Suspect area

`FloatTensor` (`org.beehive.gpullama3.tensor.standard.FloatTensor`) is the
first class HotSpot itself warns about at startup:

```
WARNING: sun.misc.Unsafe::getShort has been called by
  org.beehive.gpullama3.tensor.standard.FloatTensor
```

This is the FP16 GGUF tensor reader's hot path — every weight value is read
via `Unsafe.getShort` off a mapped `MemorySegment`/`ByteBuffer` address and
widened to float. A 1B-parameter FP16 model is on the order of 10^9 such
reads. Leading hypothesis: a CratonVM `Unsafe.getShort` (or the address
arithmetic feeding it) has an off-by-something or wrap bug that leaves some
loop's terminating condition never true for this access pattern, so the
reader loop spins in place instead of advancing — consistent with the flat
WorkingSet (no new pages ever requested) plus live CPU burn (the loop body
still executes, just never terminates or never advances its cursor).
Not yet confirmed against the actual `Unsafe.getShort` implementation or the
tensor-reader loop's source — this is the next step, not a root cause.

## Repro

```bash
# Model (2.4GB, FP16): huggingface.co/beehivelab/... Llama-3.2-1B-Instruct-F16.gguf
cd apps/GPULlama3.java
"C:/craton/CratonVM/target/release/cratonvm.exe" \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" \
  --add-modules jdk.incubator.vector \
  -cp target/gpu-llama3-1.0.0-jdk25.jar org.beehive.gpullama3.LlamaApp \
  -m /path/to/Llama-3.2-1B-Instruct-F16.gguf \
  -p "Explain GPU acceleration in one sentence." -n 40
```
Compare against the same command with `java` in place of `cratonvm.exe` (same
jar, same JDK's module system) — HotSpot completes in ~4s.

A Q8_0 quantized copy of the same model (`Llama-3.2-1B-Instruct-Q8_0.gguf`,
1.3GB) is also on-box and untried against CratonVM — worth a quick check to
see whether the livelock is FP16/`getShort`-specific or a more general
tensor-reader defect (Q8_0 reads bytes, not shorts).

## Next steps

1. Read `FloatTensor`'s FP16 read loop (`javap`/source) to find the exact
   `Unsafe.getShort` call site and its advancing condition.
2. Compare CratonVM's `Unsafe.getShort` native against HotSpot's for the
   specific address-arithmetic shape this reader uses (direct `MemorySegment`
   base + long offset, not a `byte[]` + int index — GGUF mmaps the file).
3. Try the Q8_0 model to bracket whether this is FP16-specific.
4. Once the loop/condition is identified, decide fix vs. workaround
   (e.g. an interpreter-level trace of the same address sequence would
   confirm "same address forever" vs. "wrong length still terminates,
   just very slowly").

## Related files

- `apps/GPULlama3.java/src/main/java/org/beehive/gpullama3/tensor/standard/FloatTensor.java`
- `apps/GPULlama3.java/src/main/java/org/beehive/gpullama3/tensor/GGUF.java` (model loader)
- `/tmp/llama-cratonvm-cpu2.log` — full boot/hang log from the reproduction run
- `bench-gpu/results/raytracer-vs-tornadovm-20260821.md` — the other two "GPU
  apps" triaged this session (CratonVM GPU-offload raytracer vs TornadoVM PTX;
  TornadoVM's own GPU-path correctness bug on this same RTX 2060)
