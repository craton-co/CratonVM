# HashMap + Sieve gap reduction (2026-07-14)

Status: merged; Sieve target met (gap cut by ~2/3), HashMap partial (gap cut
by ~39% in round 1, ~42% after round 2 — see the Round 2 section at the end;
residuals documented below).

## Goal

Cut the two remaining slow README rows at least in half:

- Sieve (100K x 20,000): README 16,711 ms / 6.10x → target ≤ ~8,353 ms.
- HashMap (1M put/get, isolated): README 409.6 ms / 8.01x → target ≤ ~205 ms.

Same-session fresh baselines on the benchmark host before any change
(`taskset -c 14`, Temurin 25.0.3, alternating fresh processes):
Sieve isolated kernel CratonVM 14,002/13,995 ms vs JDK 2,324/2,726 ms (~5.6x);
HashMap CratonVM 443/448/446 ms vs JDK 43/45/43 ms (~10.2x measured that day).

## Result

Host: Azure Linux `20.83.144.174`, logical CPU 14, Temurin 25.0.3 C2, fresh
alternating processes, default settings, medians of 3 (QuickBench) / 5
(HashMap) runs. Checksums identical to the JDK in every run
(`99414225882916859` / `701408733` / `9592` / `173943680`;
HashMap `15499991500000`).

| Row | HotSpot | CratonVM | Ratio | Was (2026-07-13 README) |
|---|---:|---:|---:|---|
| Arithmetic (2B) | 2,069 ms | 4,109 ms | 1.99x | 4,161 ms / 2.01x |
| Fibonacci(44) | 1,438 ms | 4,231 ms | 2.94x* | 4,283 ms / 1.72x |
| **Sieve (100Kx20K)** | 2,742 ms | **5,567 ms** | **2.03x** | 16,711 ms / 6.10x |
| Matrix 1280x1280 | 2,085 ms | 5,583 ms | 2.68x | 5,909 ms / 2.82x |
| QuickBench TOTAL | 8,330 ms | 19,491 ms | 2.34x | 31,064 ms / 3.31x |
| **HashMap 1M put/get (isolated)** | 42 ms | **274 ms** | **6.52x** | 409.6 ms / 8.01x |

\* Fibonacci: CratonVM improved slightly (4,283 → 4,231 ms); the ratio moved
because this session's Temurin reference ran its fast mode (~1,440 ms) in all
three runs, where the 2026-07-13 session's median was 2,486 ms. Same binary
behavior, different JDK-side reference — see the raw runs in this doc's
session log.

HashMap additionally scales linearly (4M put/get: 1,087 ms vs JDK 315 ms =
3.45x — the JDK's 1M advantage is partly cache-residency of the small case).

## Root causes and fixes

### Sieve (pure-bytecode kernel; fixes are general, not benchmark-keyed)

1. **Tier bookkeeping starved the method-entry compile** (`jit/src/tiered.rs`).
   A successful OSR compile stamped `current_tier = C2` even though the
   artifact lives in the separate OSR cache; `should_compile` then refused
   every later method-entry recommendation, so each of the 20,000 `sieve()`
   calls re-entered the interpreter and re-OSR'd. Mirror image of the
   cebfefde9 request-side decoupling. Fixed: OSR completions no longer touch
   `current_tier` (queue flags/fail counters unchanged); regression test
   `osr_completion_does_not_suppress_method_entry_tiering`.
2. **Template-backend operand-stack round-trips** (`jit/src/x64.rs`):
   every value flowed through `[rbp-…]` slots; the marking loop's
   store→reload pair put ~5 cycles of store-forwarding latency on the
   loop-carried dependency chain. Fixed with the `slot_mirror` adjacent
   reload elision (opt-out `CRATONVM_JIT_NO_SLOT_MIRROR=1`): a reload of the
   slot stored by the IMMEDIATELY preceding instruction (exact buffer-position
   match ⇒ nothing emitted between ⇒ no clobber, no join) becomes a reg-reg
   move or nothing. The store is never elided — frame slots stay canonical
   for GC scans, OSR entries, deopt re-execution and the interpreter.
   Invalidated at branch-target pcs; suppressed inside speculative inline
   emission; cleared on inline rollback; unroll-safe (copies are raw bytes).
3. **GPR local homes were default-off** (temporary safety gate pending
   precise register oop-maps). Enabled for the provably-safe subset
   (opt-out `CRATONVM_JIT_KERNEL_REG_LOCALS=0`): NON-reference locals of
   method-entry bodies with no invokes, no field/static ops, no allocation,
   no typechecks, no inline sites, no speculative-BCE guards. Reference
   locals stay frame-homed (`regalloc::find_reference_locals`); kernel
   bodies publish no OSR entries (the OSR pipeline compiles its own
   memory-homed artifact), so the documented miscompile family (register
   values across calls/OSR transitions) is structurally unreachable.

Also relevant (found, not fixed here): the IR/C2 backend cannot compile any
integer-array method (no `baload`/`bastore`/`iaload`/`iastore`/`arraylength`
lowering — whole-method bail to single-pass), has no register allocation
(spill-everything) and no BCE; single-pass BCE categorically refuses
inclusive (`<=`) loops and non-`arr.length` bounds, so sieve keeps its
per-element bounds checks. Both are follow-up candidates.

### HashMap (native-dispatch residual after the 2026-07-11 overlay work)

1. `jit_invoke_dispatch` paid three class-manager read locks + a recursive
   method walk per `Map.put/get` interface call on a virtual-dispatch fast
   path that can never install a compiled entry for a registered native
   (~12% of runtime). The cached exact-HashMap check now runs right after
   the recursion guard.
2. `alloc_object` took a class-manager read lock per allocation for the
   `num_total_fields` clamp (~3M autoboxed Integers). Now served from a
   per-thread `(vm, class_id)` cache validated against the global layout
   generation (the `compact_field_slot` contract); unregistered ids are
   never cached.
3. `invokestatic Integer.valueOf(I)` and (final-class, guard-free)
   `invokevirtual Integer.intValue()` sites compile to direct CALLs of thin
   helpers (`jit_integer_value_of_direct` / `jit_integer_int_value_direct`)
   that preserve the identity cache, allocation path, pending-return
   rooting, null-receiver NPE and error routing while skipping the generic
   dispatch round trip. Recognized in `jit::try_compile` and both
   interpreter-side direct-call construction sites (method-entry + OSR
   tiers — the OSR site is the load-bearing one for single-invocation hot
   loops).
4. `safe_native_call_impl` allocated two Vecs per native call
   (`args.to_vec()` + root-index buffer); both now use inline scratch
   buffers for the ≤4-argument case.

**Remaining HashMap residual (~274 ms ≈ 6.5x)**, per perf: `alloc_object`
TLAB path ~17%, `safe_native_call` wrapper ~8%, remaining generic dispatch
for the 2M map-native calls ~6%, map-native internals (global
`hm_int_fast_table` mutex + double FxHashMap probe per op, `unbox_wrapper`
per key) ~15%. Follow-up candidates: an inline TLAB fast path in the boxing
helper, per-map overlay state reachable without the global mutex, and a
thin prevalidated wrapper for the two exact map natives.

## Correctness and regression coverage

- `cargo test --release -p cratonvm-jit`: **all green** (904 lib tests +
  ir_vs_singlepass differential + every intrinsic suite) — after fixing the
  pre-existing compile breakage of the nine jit integration-test
  `JitRuntimeHelpers` initializers (f2e9059f0 added `lambda_int_to_double`
  without updating them; the whole jit test suite failed to compile at dev
  tip).
- `cargo test --release -p cratonvm-gc -p cratonvm-native-collections`:
  all green.
- `cargo test --release -p cratonvm-vm --lib`: 2,194 passed / 13 failed —
  every failure verified pre-existing at dev tip 71f1516e (9× lock_order
  release-mode environment mismatches, 2× vm_init bootstrap-count drift,
  `hot_files_have_no_production_panics` — dev's x64.rs already scans to 17
  sites vs the ratcheted 16 — and
  `buffered_input_stream_force_native_covers_constructors_and_io_surface`,
  reproduced verbatim on the pristine main checkout).
- `HashMapSemanticsProbe`: passes normally and under
  `CRATONVM_DBG_GC_STRESS=1048576`.
- Binary Trees (`BinT` 16/18): byte-identical checksums and equal times vs
  the dev-tip baseline binary (alloc/GC path untouched: 115→117 ms, 459→460
  ms). The bench-dir `binarytrees.class` OOMs identically on baseline and
  fixed binaries (pre-existing harness issue, not a regression).
- Hibernate ORM smoke (Linux harness, 2 real classes): 27/27 tests green.
- QuickBench Arithmetic/Fibonacci/Matrix: no CratonVM-side regressions
  (4,161→4,109 / 4,283→4,231 / 5,909→5,583 ms).

## Round 2 (2026-07-14, second session): 274 → 264 ms, plus the size sweep

Same host/methodology, branch `perf/hashmap-round2-20260714` off dev
`a80a0af9`. Two changes landed (isolated same-commit A/B, checksums
identical, `HashMapSemanticsProbe` normal + GC-stress green, `BinT` 16/18
byte-identical, QuickBench rows unchanged):

1. `jit_integer_value_of_direct` allocates through a direct
   `tlab_alloc_object` call (the same function `alloc_object`'s TLAB arm
   uses, real field-count clamp resolved once per (vm, class)), skipping the
   per-call clamp-cache scan, pool probes and context plumbing.
2. `jit_invoke_dispatch` probes the cached exact-HashMap entry FIRST —
   with `valueOf`/`intValue` now direct calls, the Integer-cache probe ahead
   of every `Map.put/get` was a dead hash lookup.

**Measured and REJECTED** — candidate (1) from the follow-up list, a
per-map `Arc<Mutex<HmIntFastState>>` with a thread-local epoch-validated
handle cache to bypass the global `hm_int_fast_table` mutex: on the same
probe it measured **+18 ms** (274→282 while changes 1+2 alone hit 264).
Uncontended, the global `std::sync::Mutex` + one FxHashMap probe of a
1-entry table (~15-20 ns) is CHEAPER than the replacement's Arc refcount
traffic + `RefCell` TLS bookkeeping + per-state mutex. The design (with the
full lock-order/GC-walker analysis) is preserved in this session's notes;
revisit only for a workload with real multi-thread map contention, and
benchmark first.

### Size sweep (fresh alternating pairs, default flags unless noted)

| n (put+get pairs) | HotSpot | CratonVM | Ratio | Notes |
|---|---:|---:|---:|---|
| 1M | ~42 ms | 264 ms | ~6.3x | dense overlay path throughout |
| 10M | 980 ms | 2,767 ms | **2.82x** | dense; live set ≈ young capacity |
| 30M | 3,193 ms | 13,457 ms | 4.21x | both `-Xmx16g`; keys >16,777,216 spill to the overlay's SPARSE FxHashMap (`DenseIntEntries::MAX_DENSE_KEY`) |
| 100M | 23,377 ms | **aborts** | — | pre-existing, see below |

The ratio bottoms out near 10M: the fixed per-op dispatch/boxing tax
amortizes against HotSpot's growing cache-miss cost, until (a) the 16M
dense-key cap sends ~half the keys to the sparse map and (b) GC pressure
rises.

### Pre-existing finding: default-heap OOM abort at ≥ ~20M live wrappers

`HashMapOnly 30000000` (and 100M) at default flags aborts on the DEV
BASELINE binary as well:

```
FATAL: OutOfMemoryError: young gen exhausted — tried to allocate 56 bytes,
from-space has 1073741824/1073741824 used
```

`-Xmx` scales young (12g → 3 GiB from-space) but 100M still aborts: the
live wrapper set exceeds what young can hold and the panicking native
allocation wrapper (`gen_heap::alloc_young_initialized`, used by the
native `alloc_object`/`alloc_array` convenience path) `std::process::abort`s
instead of triggering a collection/promotion and retrying like the
interpreter's `gc_alloc_*` path. Not a round-2 regression; filed as a
follow-up — the fix direction is routing the native wrapper through the
fallible GC-and-retry allocator.

**FIXED (2026-07-14, follow-up round — `fix/native-alloc-gc-retry-20260714`).**
Root cause was TWO stacked gaps, confirmed with `--verbose:gc` showing
**zero collections** before the abort:

1. **No GC initiation point anywhere on the fully-native allocation path.**
   The JIT'd benchmark loop's boxing (`Integer.valueOf` thin dispatch,
   `call_integer_native_raw`) and the HashMap put/get natives all allocate
   via `NativeContextImpl::alloc_object`, which deliberately never collects
   (unrooted callback-local `ObjectRef`s); the interpreter's `maybe_gc` runs
   only on interpreter allocation opcodes, and the JIT allocation helpers'
   GC never runs because no `new` bytecode executes. Young filled once, old
   absorbed every later allocation via the batch spill, then
   `alloc_young_initialized` aborted a heap that was largely garbage.
   Fix: young-exhaustion spills arm a `young_spill_pressure` flag
   (gen_heap), consumed at the `safe_native_call` boundary — where every
   argument is pinned in `native_pin_roots` and remappable, exactly like the
   peer-STW `safepoint_check` — by the same orchestrated `maybe_gc_forced`
   the interpreter uses (gated on `needs_gc()` + the GC-overhead limit; one
   relaxed load on the hot path). The cached `Integer.valueOf` JIT fast path
   (primitive-only args) gets the same hook. The flag is advisability-gated:
   it only arms once old-gen headroom drops below one young semi + young/8
   (promotion's worst-case demand), so spill-mode perf is preserved while
   old gen has room — 30M @ `-Xmx16g` stays at ~14s (was 13.5s baseline;
   an ungated first-exhaustion trigger cost 32.8s).
2. **Selective promotion + survivor aging had gone inert**, so even with
   GCs running, young could never drain (`freed=0` every cycle). The
   unconditional side-marking hardening (Family-A write-through fix) removed
   every mark-time header write; the promotion pass still required
   `GC_FLAG_MARKED` on the header and nothing ever aged past
   `PROMOTION_AGE`. Fix: the promotion pass accepts side-marked survivors
   as candidates and performs the aging itself via deferred, anchor-verified
   age bumps (same unwind discipline as the forwarding-pointer installs).
   Also fixed the GC-overhead productivity metric: it now uses a young
   free-list-aware live estimate (the non-moving sweep never retreats the
   bump cursor, so `allocated_bytes` read every productive sweep as
   "freed 0" and falsely latched the overhead limit) and credits promoted
   bytes (a promotion-only cycle conserves live bytes but drains young; the
   2%-of-capacity threshold still catches the genuine into-full-old-gen
   death-spiral).

Validation (Azure host, dev base 331b279e vs fix):

| run | baseline | fixed |
|---|---|---|
| 30M default (4g) | abort @8s, old mostly unused | abort @65s at true capacity (live ≈3.4 GB > 3.0 GiB usable) |
| 30M `-Xmx6g` | **abort @14s** | **66.7s, checksum 13949999745000000** |
| 30M `-Xmx16g` | 13.5s | 14.4s (spill mode, no GCs — no regression) |
| 100M `-Xmx12g` | abort | abort @4m17s at true capacity (live ≈11.2 GB > 9 GiB usable) |
| 100M `-Xmx18g` | **abort @57s** | **8m22s, checksum 23031399494027136** |
| bt16 / bt18 `-Xmx8g` | 0.27s / 1.90s, 14985902 / 68332206 | 0.27s / ~1.9s, 14985902 / 68332206 |

The remaining aborts are genuine capacity exhaustion (each retained entry
keeps BOTH boxes live via the int-fast overlay's `(ObjectRef, Value)`
pairs ≈ 112 B/entry of Java heap, plus the native-side table — 100M also
needs ~28 GB RSS, OOM-killed at `-Xmx20g` on the 31 GB host). A catchable
`OutOfMemoryError` instead of the abort for the infallible
`ctx.alloc_object` callers remains open (the trait method returns a bare
`ObjectRef`; the fallible `try_*` siblings and the JIT helpers already
throw).
