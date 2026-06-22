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
and the host deopts back to the interpreter, which observes no partial GPU state
(input arrays aren't written by the failed path, and outputs are separate
buffers materialized only after a successful kernel). Integer division-by-zero
handling is planned. Any other Java exception means the method isn't
offload-eligible, and the analyzer rejects it up front.

## Status & follow-ups

The GPU path is structured so the CPU build is completely unaffected, and the
analyzer/lowering/marshalling/cache machinery is in place and tested on machines
without a GPU. The final kernel-launch glue and a number of lower-priority items
(broader opcode coverage in the lowering stage, true reduction kernels, and
on-GPU benchmark numbers) require validation on real NVIDIA hardware and are
tracked as follow-ups.

## FAQ

**Why feature flags instead of a runtime config flag?**
A runtime flag would leave dead GPU branches in the interpreter's hot path. With
compile-time gating, a CPU build contains *no* GPU code at all — easier to audit
and impossible to regress accidentally.

**What hardware is required?**
An NVIDIA GPU with a CUDA driver, and a build compiled with `--features
gpu-driver`. Everything else degrades gracefully to CPU.
