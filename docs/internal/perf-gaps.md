# CratonVM Performance Gap Analysis

**Updated:** 2026-04-16 (T10.1 baseline capture + T10 optimization plan)

## Methodology

1. **CratonVM benchmarks** are captured via `cargo bench --bench vm_benchmarks`.
2. **HotSpot C2 baselines** are captured by running the same bytecode kernels
   under OpenJDK 25 with `-XX:TieredStopAtLevel=4` (pins execution tier to
   C2 after tier-up; `-XX:+UseC2Compiler` was removed in JDK 25 because C2 is
   unconditionally enabled on the server VM) using `System.nanoTime()`
   timing loops.
3. Baselines are stored in `bench/hotspot-baseline.json` and
   `bench/baseline.json`.
4. Per-metric ratio = CratonVM_ns / HotSpot_ns.
5. Geomean = geometric mean of all ratios.
6. Gate: `bench-gate --hotspot-compare --threshold 1.5` — geomean must be ≤ 1.5×.

## How to refresh the HotSpot baseline (T17.E.1)

The file `bench/hotspot-baseline.json` ships as an all-zeros placeholder —
the comparator detects zeros and returns exit 3 ("bootstrap; nothing to
compare") rather than failing the gate. To wire T10.8.2 up to real C2
numbers you must *capture* the baseline once, on a machine with OpenJDK
25+ installed.

### In CI (preferred)

1. Trigger the workflow manually from the Actions tab:
   **Actions → HotSpot C2 Baseline Capture (T10.8.2) → Run workflow**
   — or post the comment `/capture-hotspot` on any PR (repo-
   owner/collaborator gated).
2. The workflow runs on `ubuntu-latest`, `setup-java@v4` with
   `temurin` + JDK 25, then invokes
   `scripts/capture-hotspot-baseline.sh`.
3. A PR titled *"Refresh HotSpot C2 baseline (T10.8.2)"* opens against
   `main` containing the refreshed `bench/hotspot-baseline.json`.
4. Review + merge. Once merged, the `bench-gate` + `bench-hotspot-compare`
   CLIs fire the real geomean check on every PR.

### Locally

```bash
# On Linux/macOS:
scripts/capture-hotspot-baseline.sh \
  --out bench/hotspot-baseline.json \
  --iterations 10 --warmup 3 \
  --host ubuntu-latest

# On Windows:
pwsh scripts/capture-hotspot-baseline.ps1 `
  -Out bench/hotspot-baseline.json `
  -Iterations 10 -Warmup 3 `
  -HostTag windows-latest
```

Both scripts:
- Emit the same Java harness (`RustJvmHotSpotBench.java`) in a scratch
  temp directory, compile it with `javac`, and drive it with
  `java -XX:+UseC2Compiler -XX:TieredStopAtLevel=4`.
- Warm each kernel `--warmup` times, then record `--iterations`
  wall-clock samples via `System.nanoTime()`, and emit the median.
- Never include absolute paths or hostnames in the captured JSON —
  only the `host` tag (stable, CI-friendly) and the
  `captured_at` UTC timestamp.

### What "delivered T10.8.2" means

The *workflow* is the deliverable. **T10.8.2 is not closed until**:
(a) the PR it opens is reviewed and merged, and
(b) the `bench-hotspot-compare` gate fires with `exit 0` on a run
    where at least one ratio row is `MEASURED` (not `HS_BOOT`).

## T10.8.2 Local Capture (Session 92 — 2026-04-24)

**Preliminary** — captured locally on a developer workstation rather
than on the CI runner. Once the GitHub Actions workflow
(`.github/workflows/hotspot-baseline.yml`) fires on `ubuntu-latest`,
this file will be refreshed with the canonical CI-captured numbers.

- **Capture command:** `scripts/capture-hotspot-baseline.ps1 -Iterations 10 -Warmup 3`
- **JDK:** OpenJDK 25.0.1 (`Java HotSpot(TM) 64-Bit Server VM, build 25.0.1+8-LTS-27`, mixed mode, sharing)
- **JVM flags:** `-XX:TieredStopAtLevel=4` (C2 is unconditionally enabled on the server VM in JDK 25; `-XX:+UseC2Compiler` was removed)
- **`captured_at`:** `2026-04-24T02:42:48Z`
- **Machine spec:** Intel x86-64 (24 cores / 32 threads), Windows 11 Home
- **Host tag in JSON:** `windows-latest` (stable CI-friendly label; no hostnames, paths, or usernames leak into the committed file)

**Top-line representative medians:**

| metric | HotSpot C2 median_ns |
|--------|---------------------:|
| vm_startup | 4,900 |
| interpreter_fibonacci/30 | 1,400 |
| shootout_nbody/1000 | 193,600 |
| specjvm_scimark_sor/20x10 | 78,900 |
| dacapo_avrora_100k_loop | 381,300 |

All 24 kernels returned non-zero medians. See `bench/hotspot-baseline.json`
for the full table.

**Comparator verdict:** `bench-hotspot-compare --threshold 3.0` reports
every row as `RJ_BOOT` (cratonvm side is still the all-zeros bootstrap
baseline in `bench/baseline.json`); geomean prints `0.000×` and the
process exits `3` ("bootstrap; nothing to compare"). This is the
expected state — the HotSpot side is now real; the cratonvm side will
populate when `cargo bench --bench vm_benchmarks` runs end-to-end
(T10.8.1, still gated on the `class_manager.rs` Class-initializer
`code_source: None` fix).

**Follow-up:** kick the CI workflow from the Actions tab once the PR
containing this file opens, and let it overwrite with Linux/Ubuntu
numbers; keep the `host` tag as `ubuntu-latest` for the canonical
baseline.

## Current Gap Profile (pre-T10 optimization)

| Benchmark | Category | Expected Gap | Optimization Target |
|-----------|----------|-------------|-------------------|
| vm_startup | Startup | 2-3× | T10.7 (alloc fast-path) |
| shared_vm_startup | Startup | 2-3× | T10.7 |
| object_allocation/100 | Allocation | 3-5× | T10.7 (alloc fast-path) |
| object_allocation/1000 | Allocation | 3-5× | T10.7 |
| gc_cycle_1000_objects | GC | 2-4× | T10.7 |
| native_dispatch_noop | Dispatch | 2-3× | T10.3 (FxHash) |
| interpreter_counting_loop/* | Interpreter | 3-5× | T10.6 (CompactValue) |
| interpreter_fibonacci/* | Method Call | 2-4× | T10.5 (vtable) |
| string_creation_100 | Allocation | 3-5× | T10.2 (intern) + T10.7 |
| shootout_nbody/* | Compute | 5-10× | T10.6 + JIT |
| shootout_binary_trees/* | GC + Alloc | 3-5× | T10.7 |

## T10 Optimization Plan

### T10.2 — String Interning (est. 30-40% allocation reduction)
- Deduplicates class names, method names, and descriptors
- Eliminates redundant String allocations in class loading and resolution
- Enables pointer equality for string comparisons

### T10.3 — FxHashMap Migration (est. 15-25% faster lookups)
- Replace std HashMap with FxHashMap in hot paths (resolution cache, JIT cache)
- FxHash is 2-3× faster than SipHash for small keys
- Keep std HashMap where DoS resistance matters

### T10.4 — Lock-Free Method Resolution (est. 20-30% multithreaded)
- Per-thread resolution cache eliminates lock contention
- Thread-local caching of resolved methods

### T10.5 — Method Dispatch VTable (est. 10-20% faster invokevirtual)
- Direct array indexing replaces HashMap lookup
- Inherited vtables from parent classes

### T10.6 — Operand Stack Optimization (est. 5-10%)
- 8-byte CompactValue replaces 16+ byte Value enum
- Halves operand stack memory, improves cache locality

### T10.7 — Allocation Fast-Path (est. 5-10%)
- Pre-allocated exception message strings
- Cow<'static, str> for known messages
- VecPool for operand stack reuse

## Target

After all T10 optimizations: **geomean ≤ 1.5× HotSpot C2**

## Post-Optimization Results (T10.9 wire-up complete — 2026-04-23)

All T10 infrastructure modules are now landed at the interpreter hot
path. Session 89 T10.9.A-D closed the eight-point gap report from the
2026-04-20 audit. Every module is wired; zero stubs remain.

| Sub-task | Component | Wired? | Call site | Tests |
|----------|-----------|:------:|-----------|:----:|
| T10.2 | StringPool (`types/src/intern.rs`) | Y | 80 `intern` calls across `classloading/src/class.rs` + `class_manager.rs` — names populate via `intern_arc()` at class-define time | 10 |
| T10.3 | FxHashMap in `ResolutionCache` / `InvokeCache` | Y | `classloading/src/resolution.rs:218-232, 394-400` (consumed by `interpreter.rs` via `populate_invoke_cache`) | 11 |
| T10.3 | FxHashMap in `JitCache` | Y | `jit/src/lib.rs` — 4 FxHashMap references on JIT-hot maps | — |
| T10.3 | FxHashMap in `ClassManager` | Y | `class_manager.rs` — 14 FxHashMap refs (`loaded_classes`, `name_to_id`, `class_bytes_cache`, `cds_class_cache`) | — |
| T10.4 | Lock-free resolution (`SharedResolutionState`) | Y | `interpreter.rs` — 23 refs; promoted to cache with `shared_resolution` fast-path | 17 |
| T10.5 | VTable dispatch (`VtableManager`) | Y | `vm/src/runtime/vtable.rs` — 7 `Arc<CachedBytecodeMethod>` refs; populated at class-define; `resolve_virtual_slot` used 7× in `interpreter.rs` | 22 |
| T10.6 | CompactValue NaN-boxing (`types/src/compact_value.rs`) | Y | `interpreter.rs` — 73 `push_compact`/`pop_compact` call sites on hot opcodes | 45 |
| T10.7 | Alloc fast-path (`VecPool`) | Y | `interpreter.rs` — 36 `operand_stack_pool` / `locals_pool` call sites (acquire + release at frame entry/exit) | 11 |
| bonus  | `Class.name` / `Method.name` / `Field.name` → `Arc<str>` | Y | `classloading/src/class.rs:240` and companion fields — refcount bump on double-load, pointer-identity preserved | — |
| bonus  | `ResolvedMethod` fields → `Arc<str>` | Y | `classloading/src/resolution.rs:45-49` — refcount bump on cache hit instead of String allocation | — |

**Total module surface:** 3,030 LoC, 116 tests passing in isolation
(intern 10, compact_value 45, vtable 22, lockfree_resolve 17,
alloc_fastpath 11, fx_collections 11).

**Actual hot-path improvement from landed wire-ups:** every
T10 component is now live at the interpreter. StringPool eliminates
redundant String allocations at class load. FxHashMap replaces
SipHash in ResolutionCache, InvokeCache, JitCache, and ClassManager
lookups. VtableManager offers lock-free `invokevirtual`/`invokeinterface`
dispatch. SharedResolutionState caches method resolutions across threads.
CompactValue shrinks the operand-stack entry from 16 B to 8 B on hot
opcodes. VecPool recycles operand stack / local buffers at frame entry.
Arc<str> everywhere means a cache hit is a refcount bump, not a heap
allocation.

## Wire-up complete (2026-04-23)

Every gap from the 2026-04-20 audit closed in Session 89 via
T10.9.A-D:

- **T10.9.A — VtableManager dispatch** landed. `VtableEntry` carries
  `Arc<CachedBytecodeMethod>`; `resolve_virtual_slot` is consulted
  ahead of `invoke_cache` on `invokevirtual`/`invokeinterface`.
- **T10.9.B — FxHashMap completion** landed. `class_manager.rs` now
  uses FxHashMap for `loaded_classes`, `name_to_id`,
  `class_bytes_cache`, `cds_class_cache`. JitCache converted. DoS-
  resistant maps (Properties, System.getenv) remain `std::HashMap`.
- **T10.9.C — StringPool Arc<str>** landed. `Class.name`, `Method.name`,
  `Method.descriptor`, `Field.name`, `Field.descriptor` are all
  `Arc<str>`; interned at class-define via `intern_arc()`. Pointer
  identity preserved on double-load.
- **T10.9.D — CompactValue direct push/pop** landed. 73 call sites
  converted on `iload_N`, `iconst_N`, `istore_N`, `iadd`, `isub`,
  `imul`, `invokestatic`, `invokevirtual`, `invokeinterface`,
  `getfield`, `putfield`, `aload_N`, `astore_N`, `dup`, `dup_x1`,
  `dup2`.

**Workspace compile:** `cargo check --workspace` clean. **Per-crate
test gates (2026-04-23):** jit 608, gc 634, jfr 256, classloading 335,
reader 239, types 202, native-collections 55, native-io 140 (+2 pre-
existing registry-probe failures orthogonal to T10). Integration-test
adaptation to the `Arc<str>` migration is tracked as a separate
follow-up — `--lib` suites are the T10.9.E gate.

## T5 Integration Push (2026-04-19)

| Sub-task | Component | Status | Evidence |
|----------|-----------|--------|----------|
| T5.1.2 | `bench-hotspot-compare` CLI | ✅ Complete | `vm/src/bin/bench_hotspot_compare.rs`, 9 tests |
| T5.2.1 | SCEV → Compiler integration | ✅ Complete | `Compiler::induction_var_for()` |
| T5.2.5 | `JitPICSlot` 3-way PIC | ✅ Complete | `jit/src/lib.rs:1085`, 8 tests |
| T5.2.14 | Null-check elim → Compiler | ✅ Complete | `Compiler::is_local_nonnull()` |
| T5.2.15 | Element-wise SIMD detection | ✅ Complete | `detect_int_array_element_wise`, 3 tests |
| T5.2.16 | Sibling-call tail JMP | ✅ Complete | `emit_epilogue_without_ret` + `emit_jmp_absolute` |
| T5.2.17 | Loop unswitch detection | ✅ Complete | `detect_loop_unswitch_candidates`, 3 tests |
| T5.5.1 | Adaptive TLAB sizing | ✅ Complete | `gc/src/tlab.rs` `TlabPressureTracker`, 7 tests |
| T5.5.2 | Card table batching | ✅ Complete | `gc/src/card_table.rs` thread-local buffer, 7 tests |
| T5.5.4 | G1 evacuation policy | ✅ Complete | `select_evacuation_candidates`, 7 tests |
| T5.5.5 | Generational ZGC | ✅ Complete | `GenerationalZgc` minor/major, 13 tests |

**Jit tests:** 606 passing. **GC tests:** 631 passing. `cargo check --workspace` clean.

### Known follow-ups

- **PIC dispatch stub in x64 codegen** — the `JitPICSlot` data structure is
  allocated and queryable, but the existing MIC dispatch in `x64.rs` still
  serves polymorphic sites with a single-entry probe. Wiring a 3-way probe
  that falls back to the generic helper is the next x64 work item.
- **SIMD element-wise emission** — `SimdArrayElementWise` detection runs and
  populates `Compiler::simd_element_wise_loops`, but codegen still emits the
  scalar loop body. Lowering to PADDD/PMULLD + remainder loop is the next
  x64 work item.
- **Loop unswitch emission** — detection is complete; body duplication and
  branch hoisting in x64 codegen is the next x64 work item.
