# `java.util.HashMap` put/get native-dispatch overhead — FIXED

Status: **Fixed and retired on 2026-07-11.** The acceptance target is CratonVM
elapsed time no more than 10x the same-methodology HotSpot result. The final
pre-rebase five-round candidate measured 338-345 ms versus 44-52 ms HotSpot
(6.6-7.1x), with the same checksum in every run.

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
- The generic `HashMap.put` tiering window now admits a fresh exact-class integer
  map into the lazy overlay from its first entry. Previously the generic path
  materialized 2,000 real nodes before exact dispatch was installed, permanently
  disqualifying the overlay's fresh-map guard. Non-integer puts materialize the
  overlay before ordinary node insertion, and `Map.equals` reads an overlay-backed
  receiver's authoritative size.
- Small native-created objects use the mutator's existing TLAB. Once young space
  cannot refill, same-layout old-generation objects are allocated in a batch under
  one allocator lock and unused objects remain in a GC-remapped per-thread pool.
- JIT type-check target resolution and native descriptor lookup have VM-scoped
  thread-local last-entry caches. Exact `Integer` checkcasts and repeated wrapper
  field-0 accesses no longer take metadata/cache locks per iteration.
- After the first ordinary `Integer.valueOf` initializes the class and discovers
  its real ClassId, out-of-range JIT boxing allocates through the same native
  context path directly. The mandated `-128..127` identity cache still uses the
  canonical native callback. `Integer.intValue` performs its already-validated,
  non-allocating field-0 read directly and preserves `native_pending_return` for
  object-return handoff rooting.

## Validation

Azure Linux host `20.83.144.174`, pinned CPU, unique release binaries and target
directory, one million puts plus one million gets:

| Build | Runs | Mean |
|---|---:|---:|
| CratonVM baseline | 17,762 / 18,703 / 17,876 ms | 18,113 ms |
| First-pass CratonVM | 1,722 / 1,722 / 1,719 / 1,715 / 1,719 ms | 1,719 ms |
| First-pass HotSpot | 49 / 56 / 46 / 44 / 44 ms | 47.8 ms |
| Within-10x candidate | 343 / 338 / 339 / 344 / 345 ms | 341.8 ms |
| Same-minute HotSpot | 48 / 50 / 49 / 49 / 52 ms | 49.6 ms |
| Merged-dev CratonVM | 660 / 412 / 400 / 367 / 374 / 370 / 371 / 368 / 364 ms | 409.6 ms |
| Merged-dev HotSpot | 55 / 54 / 50 / 51 / 51 / 50 / 49 / 49 / 51 ms | 51.1 ms |

The pre-rebase candidate is **6.89x HotSpot** by the five-round means. The final
merged-dev nine-round means are **8.01x HotSpot**, including the first CratonVM
outlier (660 ms); rounds 2-9 stabilize at 364-412 ms. The merged implementation is
approximately 44x faster than the original 18,113 ms pinned CratonVM baseline.

Correctness evidence includes `HashMapSemanticsProbe` normally and with
`CRATONVM_DBG_GC_STRESS=1048576`, plus the `cratonvm-native-collections` unit and
integration tests (including native-pin and overlay-relocation suites). The
semantics probe covers overwrite/remove, views after GC, mixed-key overlay
materialization, `putAll`, symmetric equality/hashCode, `toString`, and clear.
