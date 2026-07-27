# JIT interface cache parity and performance fix (2026-07-27)

## Problem

Interface-heavy kernels compiled successfully but were slower than the
interpreter. The single-pass x64 backend and optimizing IR backend also had
different virtual-call coverage and cache ABI assumptions. A four-receiver
site repeatedly entered `jit_invoke_virtual_mic`, and class-only profile seeds
could produce a null indirect call.

## Root causes

- OSR and early compilation paths passed empty MIC/PIC vectors, so hot loop
  call sites had no dynamic dispatch cache.
- Virtual direct entries and IR virtual calls were opt-in even after their
  correctness prerequisites existed.
- Inline MIC/PIC code assumed every compiled target needed a hidden VM
  context argument; small leaf implementations normally do not.
- Generated guards accepted a matching class id with a zero entry pointer.
- PIC miss branches used rel8 encodings that overflowed after dual-ABI
  marshalling was added.
- The three-entry PIC thrashed on a stable four-receiver call site.
- Reinstalling a class already present in a PIC consumed another way and
  evicted an unrelated receiver.
- Most importantly, loop unrolling cloned the inline guard's cache pointer but
  did not clone the MIC/PIC pointers passed to the slow helper. The helper
  populated the original cache while the cloned generated code probed an empty
  cache forever.
- Megamorphic sites repeated hierarchy resolution and compile-cache lookup
  after every four-way inline miss.

## Fix

- Both interpreter compile entry paths now allocate and own dynamic dispatch
  slots.
- The IR and single-pass backends consume the same default-enabled VM policy,
  emit the same four-way PIC shape, accept both compiled-entry ABIs, and reject
  null cached targets.
- PIC miss branches are rel32.
- `JitPICSlot` now has four generated-code-visible entries, refreshes duplicate
  class mappings in place, and retains a bounded 64-target secondary cache for
  megamorphic helper misses.
- Inline-cache immediates are grouped relocations. During unrolling, the guard,
  MIC helper argument, and PIC helper argument that referred to one original
  slot are rewritten to the same fresh cloned slot.
- Normal VM exit emits JIT method statistics when requested.

## Regression coverage

- The JIT package runs 1,208 tests successfully (1,017 library tests plus all
  integration tests; one additional test remains intentionally ignored).
- The unroll regression inspects emitted machine code and requires every
  cloned PIC address in both its inline guard and helper argument, plus the
  corresponding cloned MIC helper argument.
- `check-interface-jit-performance-20260727.sh` compiles probes into a temporary
  directory, verifies checksums against HotSpot and `--nojit`, requires JIT to
  beat `--nojit` for mono-, poly4-, and mega16 dispatch, and bounds poly4 helper
  entries after warm-up.

## Pinned-host result

Host: Azure probe host, CPU 13, OpenJDK 17, 1,000,000 measured invocations,
three fresh-process repetitions. Timings are medians and are regression data,
not product benchmark claims.

| Shape | Fixed JIT | `--nojit` | HotSpot |
|---|---:|---:|---:|
| mono | 5,956,442 ns | 367,964,257 ns | 4,183,110 ns |
| poly4 | 6,769,296 ns | 732,504,448 ns | 4,901,731 ns |
| mega16 | 133,360,511 ns | 1,146,214,057 ns | 7,032,738 ns |

The fixed JIT is 61.8x faster than its interpreter on mono, 108.2x on poly4,
and 8.6x on mega16. The remaining megamorphic gap to HotSpot is a future
optimizer opportunity, not repeated class/method resolution: the bounded
secondary target cache removes that architectural failure mode.
