# `java.util.HashMap` put/get native-dispatch overhead — FIXED

Status: **Fixed and retired on 2026-07-11.** The requested target was at least a
10x improvement over the pinned CratonVM baseline. The final isolated five-round
result is 1,719 ms versus 18,113 ms before this pass: **10.54x faster**, with the
same checksum in every run.

## Original symptom and root cause

The `HashMap<Integer,Integer>` million-entry put/get probe was about 200-360x
slower than HotSpot. Sampling showed a constant per-call tax rather than an
algorithmic-complexity problem. The dominant path was
`jit_invoke_dispatch -> invoke_or_native -> safe_native_call`, repeated for map
operations and boxed-Integer helpers. Its largest costs were eager conservative
JIT-frame root publication, repeated native-registry/metadata lookups, generic
receiver classification, and synthetic HashMap node materialization.

## Fix

- A JIT-to-native object return remains rooted in `native_pending_return` during
  the immediate Rust-to-JIT handoff. When no STW is pending, the handoff avoids an
  eager full frame snapshot; blocking/interpreted calls, exceptions, and an
  already-requested STW retain eager publication.
- Exact `HashMap.put/get` receiver guards cache their native callbacks and use a
  fixed inline argument buffer with prevalidated object roots.
- `Integer.valueOf` and `Integer.intValue` JIT callsites cache the canonical
  built-in callbacks, avoiding repeated registry and generic-dispatch lookup while
  preserving normal native pinning and allocation behavior.
- Fresh exact `HashMap<Integer, ?>` instances use a GC-integrated lazy overlay.
  Dense non-negative keys use direct indexing; arbitrary integers use an FxHashMap.
  Unsupported operations materialize or consume the overlay through ordinary map
  contracts. Keys and values participate in GC rooting, relocation, and pruning.
- Primitive-wrapper recognition and compact-field lookup use generation-validated
  thread-local caches; native-call root-index scratch space stays inline for the
  common small-argument case.

## Validation

Azure Linux host `20.83.144.174`, CPU 1 pinned, unique release binary and target
directory, one million puts plus one million gets:

| Build | Runs | Mean |
|---|---:|---:|
| CratonVM baseline | 17,762 / 18,703 / 17,876 ms | 18,113 ms |
| CratonVM fixed | 1,722 / 1,722 / 1,719 / 1,715 / 1,719 ms | 1,719 ms |
| JDK 21 same-minute reference | 49 / 56 / 46 / 44 / 44 ms | 47.8 ms |

The fixed build is **10.54x faster than the pinned CratonVM baseline**. It remains
about 36x slower than HotSpot in the isolated cold-process probe; that remaining
gap is not represented as a correctness issue here. A five-repetition in-process
probe stabilizes at 671-680 ms after first-tier compilation, with an identical
aggregate checksum.

Correctness evidence includes `HashMapSemanticsProbe` normally and with
`CRATONVM_DBG_GC_STRESS=1048576`, plus the `cratonvm-native-collections` unit and
integration tests (including native-pin and overlay-relocation suites).
