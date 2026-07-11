# `java.util.HashMap` put/get is ~200-360x slower than JDK-25 — root-caused, partially fixed

Status: **Root-caused + partially fixed** (branch
`fix/hashmap-native-dispatch-overhead-20260711`). Three targeted fixes landed for the
safely-addressable slice of the overhead; the largest single bucket (conservative
JIT-frame root scanning) is deliberately left untouched — see "Remaining work" below.

## Symptom

`HashMap<Integer,Integer>` put+get is a stable ~230x-360x slower than real JDK-25 on
CratonVM, essentially flat across sizes (228.6x at 250K entries, 233.8x at 1M in the
original calibration) — a large constant-factor-per-operation overhead, not an
algorithmic complexity bug (contrast the sibling `String`/`Matcher`/`substring` O(n²)
family, now fixed — see
[`matcher-native-full-input-redecode-quadratic.md`](matcher-native-full-input-redecode-quadratic.md)
and
[`../internal/fixed-suite-bugs/substring-large-parent-quadratic-allocation-FIXED.md`](../internal/fixed-suite-bugs/substring-large-parent-quadratic-allocation-FIXED.md)).

## Root cause

Initial hypothesis (allocation/GC pressure from `Integer` autoboxing) was **wrong**.
cdb stack-sampling (attach-and-dump the JIT-compiled benchmark's own call stacks every
~1.5s, the same technique documented for the `bintrees` GC/allocation-ceiling profile
elsewhere in this repo's history) of a P-core-pinned `HashMap<Integer,Integer>`
1M-entry put+get benchmark found:

- **Zero of 34 samples** matched the `bintrees` allocation/GC leaf signature
  (`sweep_young_non_moving`, `try_alloc_object_full`, TLAB refill, free-list scan) —
  versus ~90% stack-presence for that bucket in the allocation-bound `bintrees`
  profile. Direct allocation/boxing showed up in only ~9% of samples.
- Instead, the dominant costs are the **fixed per-native-call overhead** of
  CratonVM's JIT-to-native dispatch path. `java.util.HashMap.put/get` and
  `Integer.hashCode()/valueOf()` are native Rust functions (not JIT-compiled/inlined
  bytecode), so every call round-trips through `jit_invoke_dispatch` →
  `invoke_virtual`/`invoke_or_native` → `safe_native_call`, paying:
  - **Conservative JIT-frame root scanning** (~29% of samples, the largest single
    bucket) — the caller's frame is walked for GC roots on every native call since the
    native could allocate/trigger GC.
  - **RwLock-guarded metadata lookups** (~12%): a `class_layout`/`compact_field_slot`
    lock on every field get/set, and a `superclass_of` lock walking the class
    hierarchy for every `Integer.hashCode()` call (`map_hash_key` tried the rare
    enum-identity check, which needs the superclass walk, BEFORE the common
    primitive-wrapper case).
  - **Generic dispatch-machinery overhead** (~21%): unresolved dispatch leaves, a
    tracing ring-buffer record, a debug-flag check, a GC SATB safepoint flush, and a
    `recover_stale_lambda_receiver_from_native_pins` RwLock (a receiver-corruption
    recovery check that runs on every non-Object-method `invoke_virtual`, including
    every `HashMap.put`/`.get`).
  - Actual useful work (the `hashbrown` SIMD bucket probe + real field read/write) was
    only ~12% of samples.

The benchmark makes 2,000,000 native round-trips (1M put + 1M get) through this
machinery, and that fixed per-call tax — not allocation or GC throughput — produces
the large, size-independent slowdown.

## Fixes shipped

1. **`map_hash_key`/`map_keys_equal` check-order** (`native-collections/src/lib.rs`):
   reordered to check the common primitive-wrapper case before the rare
   enum-identity/`Thread`-mirror cases. A JDK wrapper class can never also be an
   `Enum` subclass or `Thread`, so this is a pure reorder, not a behavior change —
   but it skips a multi-hop `class_manager` RwLock walk for the overwhelmingly common
   case of `Integer`/`Long`/etc. keys.
2. **`recover_stale_lambda_receiver_from_native_pins` fast path** (`vm/src/vm/vm_exec.rs`):
   added a lock-free cached `java/lang/Object` `ClassId` comparison so the common
   (non-corrupted) case skips the `class_manager` RwLock entirely; the real
   corruption-recovery slow path is untouched.
3. **`compact_field_slot` cache** (`gc/src/gen_heap.rs`): a per-thread,
   generation-validated single-entry cache mirroring the already-proven
   `compact_oop_scan` pattern (`gc/src/heap.rs`), avoiding the `CLASS_LAYOUTS` RwLock
   on repeated field access to the same class.

A 5-round, isolated re-measurement (matching this repo's Binary Trees methodology, to
strip out combined-run GC-pressure inflation) post-fix averages ~206x (14,930 ms
CratonVM / 72.6 ms JDK, checksums identical every round) — down from the isolated
methodology's prior ~357x.

## Remaining work (not attempted)

Conservative JIT-frame root scanning (~29% of the profiled overhead, the single
largest bucket) and the SATB GC write-barrier flush are deliberately **not** touched.
Both are correctness-critical: this repo's `conservative_roots.rs` has a real prior
history of heap-corruption/SIGSEGV bugs from exactly this class of change (e.g. a
`frame_base`-vs-`exact_rbp` conflation that dropped ancestor-frame roots), the
"precise" JIT oop-map path is additive over the conservative backstop by design (not
a replacement — see `docs/feature-designs/precise-jit-maps-default.md`), and the root
snapshot also serves cross-thread STW visibility, not just per-call allocation
safety. A real fix here would be a native-dispatch-machinery project (e.g. extending
per-safepoint oop-map coverage to shrink the backstop scan, or skipping the scan for
natives provably unable to trigger GC — a much stronger claim than "doesn't allocate"
given the STW-visibility requirement), not something to attempt inside this pass.
