# GPU Offload

CratonVM has an **opt-in** feature that offloads suitable Java methods to an
NVIDIA GPU: it identifies *pure static methods over primitive arrays*, lowers
them from Java bytecode to NVIDIA PTX, and runs them on a CUDA device.

> **The feature is off by default.** A default build of the JVM is byte-for-byte
> the same as a build with no GPU support at all: no CUDA link, no `--gpu*`
> flags, and no extra branches in the interpreter's hot path. If you only run on
> CPU, you can ignore this chapter.

## What can be offloaded

The offload analyzer accepts a narrow, safe shape: a **static** method whose
parameters are primitives and primitive arrays, containing a canonical counted
loop, with **no** calls, allocation, field access, type checks, monitors,
switches, exceptions, or reference arrays. Anything outside that shape falls
through to the normal interpreter/JIT with no observable difference.

The canonical example is element-wise array work:

```java
public static void vectorAdd(int[] a, int[] b, int[] out) {
    int n = a.length;
    for (int i = 0; i < n; i++) {
        out[i] = a[i] + b[i];
    }
}
```

The emitted kernel is SIMT and element-wise: each GPU thread computes one output
index, with a per-access bounds check that safely bails out to the CPU on
violation.

## Build modes

GPU offload is gated behind Cargo features. There are three build levels:

| Build | What you get |
|-------|--------------|
| `cargo build` | **CPU-only JVM.** No GPU code linked, no `--gpu*` flags. |
| `cargo build --features gpu` | The above, plus the GPU plumbing in **stub mode** (device probes report "no driver") and the `--gpu*` CLI flags. Useful for testing the plumbing on machines with no GPU. |
| `cargo build --features gpu-driver` | The above, plus real CUDA driver bindings. Requires a CUDA toolkit (12.x). |

The features compose: `gpu-driver` implies `gpu`, which drags the whole offload
stack in. Pulling on nothing keeps the CPU path pristine.

## CLI flags (only under `--features gpu`)

| Flag | Default | Meaning |
|------|---------|---------|
| `--gpu` | off | Enable offload of eligible static methods. |
| `--gpu-device <N>` | `0` | CUDA device ordinal. |
| `--gpu-min-work <N>` | `4096` | Skip offload when estimated work (array length / loop trips) is below this — avoids host↔device round-trip overhead on tiny inputs. |
| `--print-gpu-decisions` | off | Log one line per analyzer verdict (eligible / rejected). |
| `--gpu-info` | — | Probe the device, print its name + compute capability + memory, and exit. |

If `--gpu` is requested but no CUDA driver is present, the flag is silently
demoted and the program runs on CPU:

```text
[cratonvm-cli] --gpu requested but no CUDA driver available; running on CPU
```

## How it fits together

```text
  vm-cli (--features gpu)            parse --gpu*, set the offload config
        │
        ▼
  interpreter invokestatic hook  →   offload cache: analyze → lower → load
        │                            (cache the compiled PTX kernel per method)
        ▼
  marshalling  ↔  CUDA bridge        pack arrays host→device, launch, copy back
        │
        ▼
  GC safepoint coordination          a no-GC critical section + pinned arrays
                                     keep the collector from moving live data
                                     while a kernel reads it
```

The pieces are: a thin **CUDA Driver API bridge**, a **bytecode→PTX lowering**
crate (analyzer + loop recognizer + emitter), heap↔device **marshalling**, an
**offload cache + dispatch hook** in the interpreter, and **GC safepoint
coordination** (a critical-section token plus array pinning so the collector
can't move an array a kernel is reading).

## Exceptions inside an offloaded method

Array-bounds violations are handled: the kernel flags the failure and returns,
and the host deopts back to the interpreter. Input arrays are never written by
the failed path, so the re-run always starts from the same inputs the original
call had.

One narrow case can leave part of an *output* array already written when the
deopt happens. A large enough write-only output is streamed back in chunks, so
that one chunk's copy overlaps the next chunk's kernel, and each chunk lands in
the Java array as its own copy completes — which is before the failure flag has
been read. This is deliberately restricted to an array the kernel **writes and
never reads**, and it is unobservable: the interpreter re-runs the whole method
from the first iteration, rewrites every element it would have written, and
throws at the same index, so the elements an early chunk committed are a subset
of those plain Java writes before the throw, holding the same values. An array
the kernel also reads is never streamed this way, precisely because there a
partial commit would become the re-run's own input. Every other output still
materializes only after the flag has been checked.

Integer division-by-zero and `INT_MIN`/`LONG_MIN` ÷ `-1` overflow are handled
the same way, not merely planned. PTX's `div.s32`/`rem.s32` (and the 64-bit
forms) are undefined on a zero divisor and on the `MIN_VALUE / -1` overflow
case, where Java requires an `ArithmeticException` or a defined wraparound
respectively. The lowering stage emits predicate guards ahead of every
`idiv`/`irem`/`ldiv`/`lrem`: a zero-divisor check and, for division only, a
`MIN_VALUE`-and-`-1` check, each branching to the same deopt exit as an
out-of-bounds access. That exit sets the failure flag and returns; the VM then
re-runs the whole method on the CPU, where normal Java semantics (throwing
`ArithmeticException`, etc.) apply. A method can opt out of just the
zero-divisor guard with `@GpuKernel(admit = AdmissionHint.ALLOW_DIV_BY_ZERO)`
(`craton.gpu.GpuKernel`'s `admit` element) — the overflow guard on division
still stays in. Any other Java exception (`NullPointerException`,
`ClassCastException`, and so on) means the method isn't offload-eligible in
the first place, and the analyzer rejects it up front rather than trying to
handle it on the device.

## Status & follow-ups

The full launch path — interpreter hook → offload analyzer/cache →
bytecode→PTX lowering → CUDA bridge → `cuLaunchKernel` — is implemented and
has been validated end-to-end on real NVIDIA hardware (RTX 2060, sm_75),
with kernel output checksums matching HotSpot bit-for-bit across every kernel
measured. See [GPU offload benchmarks](benchmarks.md) for the numbers, and
[Float bit-exactness](../../../gpu/README.md#float-bit-exactness) for what
keeps a float kernel bit-exact: chiefly the explicit `.rn` rounding modifiers
that stop ptxas contracting `a*b + c` into a single-rounding FMA.

That validation pass found and fixed two bugs: offload-eligible call sites
were being promoted into the interpreter's invoke cache, which silently ended
offload after a call site's first invocation, and the kernel's bounds-failure
flag was read before outputs were written back, which could miss a late
failure. Both are fixed in current `dev`.

The follow-ups from that validation pass — wiring reduction kernels
(non-`void` return) into the dispatch path, closing the JIT-caller bypass of
the offload hook, and broadening opcode coverage in the lowering stage — are
now closed; see [`docs/gpu/README.md`](https://github.com/craton-co/cratonvm/blob/dev/docs/gpu/README.md)
for the current feature-doc index.

## FAQ

**Why feature flags instead of a runtime config flag?**
A runtime flag would leave dead GPU branches in the interpreter's hot path. With
compile-time gating, a CPU build contains *no* GPU code at all — easier to audit
and impossible to regress accidentally.

**What hardware is required?**
An NVIDIA GPU with a CUDA driver, and a build compiled with `--features
gpu-driver`. Everything else degrades gracefully to CPU.
