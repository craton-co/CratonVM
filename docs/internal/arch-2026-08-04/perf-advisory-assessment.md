# Performance advisory — item-by-item assessment

**Slug:** `perf-advisory-assessment`
**Date:** 2026-08-04
**Status:** ASSESSED. Of eleven distinct claims: **five already implemented**,
**three refuted or stale as written** (one of them actively dangerous to
follow), **two partly done**, **one genuinely open and correctly diagnosed**.

Every verdict below was reached by reading the consumer — the emitter, the call
site, the wiring — not by grepping for a name. That discipline is here because
the sibling review on this date got four of its own nine findings wrong the
other way (see `architecture-review-a1-a9.md`).

---

## Summary

| # | Advisory claim | Verdict |
|---|---|---|
| 1 | Migrate `NativeMethodRegistry::find` sites to memoized `NativeCallSite` | **MECHANISM RIGHT, IMPACT OVERSTATED — hot path was already migrated; one real redundancy fixed** |
| 2 | "Fix young-GC activation; remove the unconditional guard that ignores `CRATONVM_MOVING_YOUNG`" | **REFUTED — already removed; following this now would reintroduce heap corruption** |
| 3 | "Layout-registry lookup is 50% of runtime; add a HashMap cache" | **REFUTED as written — the file does not exist; the real one is already better than proposed** |
| 4 | Interpreter: table-driven / computed-goto dispatch | **OPEN — this is A4b; measured, scoped** |
| 5 | "GC appears single-threaded; parallelize scanning" | **REFUTED for young gen — already parallel and heap-size-adaptive** |
| 6 | TLAB sizing | **DONE — adaptive per-thread sizer** |
| 7 | Card-table write barrier batching | **DONE — per-thread dirty buffer, drained at GC start** |
| 8 | `thread_registry` may use a mutex; make it concurrent | **DONE — it is an `RwLock`, deliberately, with rationale** |
| 9 | `System.arraycopy` should use bulk memcpy | **DONE — `copy_within` / `copy_nonoverlapping`** |
| 10 | JIT bounds-check elimination / LICM | **DONE — `bce.rs`, `licm.rs`, `licm_int.rs`** |
| 11 | Memory-map large jar/jmod files | **PARTLY DONE — jars are mmapped; jimage deliberately is not** |

---

## 1. Native dispatch — OPEN, and the advisory is right

This is the one action item worth doing as written, and it is correctly
identified as the highest-priority one.

`NativeMethodRegistry::find` → `find_with_kind` → `slot_for_exact`
(`native-api/src/registry.rs:5922`) does, on **every** call:

1. `native_class_hash(class_name)` — hash the class-name string;
2. a `classes_with_natives` set membership prefilter;
3. `native_method_hash_from(..)` — hash method name + descriptor;
4. `slot_index_for_key(..)` — digest → index, **then three `str` comparisons**
   to verify the triple (deliberate: a raw-digest map without name verification
   is a shipped defect this codebase already had, per the
   `loaded_classes` "Round 4 audit fix (CRIT)" note).

It is well-engineered, and it is still string hashing per call.

`NativeCallSite` (`native-api/src/native_id.rs:200`) is exactly the bypass the
advisory proposes — a single `AtomicU64` memo cell with a generation check,
`const fn new()` so it can be a `static` at the call site.

### Update after taking it on (2026-08-04)

The mechanism is real and the description of `slot_for_exact` above is
accurate. The **sizing was not**, and both of my numbers were wrong.

**"10 `NativeCallSite` sites vs 44 `find(` sites" was wrong twice over.**

- It missed the adoption that matters. `CachedBytecodeMethod` carries a
  **per-method** `NativeCallSite` (`native_call_site()` / `native_dispatch()`,
  `jit-api/src/lib.rs:317`), so **steady-state cached dispatch already does no
  string hashing at all**. Counting only `static NativeCallSite::new()`
  declarations missed the entire hot path. (`native_id.rs`'s own rationale still
  describes that field as an `OnceLock<Option<NativeCallback>>` with a stale-
  negative bug — that is out of date; it is already a `NativeCallSite`.)
- "44" counted test code. The production figure is **123**, once both
  `#[cfg(test)]` *and* `#[cfg(all(test, feature = ...))]` regions are excluded —
  `vm.rs` alone contributed 38 false positives, because its 76K-line test module
  uses the second spelling.

**Of those 123, almost none are migratable or hot:**

- 34 are `vm_init` bootstrap registration — cold by construction.
- Most of the interpreter ones are *guarded interception* paths
  (`intercept_force_registered_native_cached`, the SSL/Spring-loader arms) whose
  triples are **variable**. A `NativeCallSite` is keyed on the registry
  generation alone and never re-verifies the triple on a warm hit, so one cell
  **structurally cannot** serve them. Two of those arms are constrained to 3 and
  4 possible triples respectively — they would need one cell per arm, and one is
  already preceded by a `class_manager.read()` that dwarfs the lookup.

**What was actually worth doing, and was done:**

1. `dispatch_static.rs` ran `find(..).is_some()` then `kind_of(..)` on the
   **same triple**, back to back — two full `slot_for_exact` passes where
   `find_with_kind` gives both facts in one. Fixed.
   Safe because the two forms differ only on the cold descriptor-quirk path
   (`kind_of` → `None`, `find_with_kind` → `Bridge`), and neither equals
   `SyntheticStub`, which is this site's only consumer.
2. The two constant-triple reflection sites in `invoke.rs` migrated to their own
   `static` cells, mirroring `dispatch_virtual.rs`.

**Measured, not assumed:**

- The `dispatch_static` path is per-call-**site** resolution, not per-invoke —
  `execute_invokestatic_cached` skips it once warm. A counter compiled into a
  release binary showed **fewer than 5,000 hits per run** on `HelloWorld`,
  `DistinctEquals`, `MapEqRepro` and `StringNativeAllocationChurn`. Hundreds,
  not millions: a warm-up win.
- The two `invoke.rs` sites are **cold**. An `eprintln` probe recorded **zero**
  hits across `wp2_2_method_invoke_matrix`, `wp2_1_reflect`, `wp2_5_proxy` and
  `wp2_7_annotation_proxy`. They are migrated for consistency, and no gain is
  claimed for them.

**Revised verdict:** the advisory correctly identified a real mechanism and a
real (if small) redundancy, but its premise — that native dispatch pays a string
hash *per call* — does not hold: that cost was already removed from the
steady-state path. "Effort medium, risk low" was right; "could improve Java
library performance significantly" was not.

## 2. Young-GC activation — REFUTED, and dangerous to act on

The advisory says: *"remove any unconditional guard that ignores
`CRATONVM_MOVING_YOUNG`… ensure copies happen when requested"*, naming
`gc/src/heap.rs`.

Three things are wrong with this now:

- **`moving_young` already defaults to ON.** `types/src/flags.rs:583`:
  `DEFAULT_MOVING_YOUNG: bool = true`. It is an opt-*out*
  (`CRATONVM_NO_MOVING_YOUNG`); `CRATONVM_MOVING_YOUNG` is a retained no-op
  opt-in. A default build already requests a moving cycle.
- **The specific guard described is already gone.** The
  `fail_closed_non_moving = is_active() && !allow_moving_young` term and the
  `CRATONVM_ALLOW_MOVING_YOUNG` flag were removed on 2026-07-26. The only
  surviving references in this checkout are in a stale sibling worktree, not in
  `dev`.
- **What remains is not a bug.** A cycle may still divert to the non-moving
  sweep via `divert_for_incomplete_moving_coverage`, which requires a
  **per-cycle root-coverage proof**: every live compiled frame's active
  safepoint must certify `moving_young_coverage_complete`, no unregistered JIT
  frame may be on the native stack, no peer thread may be in JIT, and a
  frame-band verifier must find no young-resident word the shadow stack did not
  publish.

That proof exists because relocating without it corrupted the heap — five
codegen sites pushing untagged object references onto the JIT's simulated
operand stack, fixed 2026-07-26. **Removing the diversion to "ensure copies
happen when requested" would reintroduce that class of corruption.** Do not do
this.

The real question — "did this process compact?" — is answered by
`gc_metrics::collector_decision_report()`, not by the flag. A nonzero
`coverage_fallbacks` is the number to read.

## 3. Layout registry — REFUTED as written

`gc/src/layout_registry.rs` **does not exist**, and no `layout_registry` /
`LayoutRegistry` symbol exists anywhere in the tree.

The real thing is `types/src/field_layout.rs`, and it is already strictly better
than the proposed fix:

- `class_layout(class_id)` indexes a **dense array by `class_id`**
  (`CLASS_LAYOUTS.read().unwrap().get(class_id as usize)`). The advisory
  proposes "a HashMap from class to layout index" — that would be *slower* than
  the direct index already in place.
- In front of it sits an 8-entry **per-thread MRU cache**
  (`CurrentLayoutCache`), i.e. the "caching recent lookups" half is done too.
- JIT code bakes the layout slot's **stable address** plus a 2-instruction
  replace-counter guard (`layout_replace_guard`), backed by a fixed-capacity
  never-reallocating table, so compiled code skips the lookup entirely.

The "50% of run time" figure is not reproducible against this code and no
citation for it survives in the tree. Note also that a memoization attempt in
this same neighbourhood (`get_field_by_name`) was **measured at a 37%
slowdown** and reverted — more caching here has already been tried and refuted
once.

Residual worth a look, but small: `class_layout` takes a `std::sync::RwLock`
read on the cold path (the thread-local MRU absorbs the hot path). That is the
same shared-cache-line shape as the sibling review's A8.

## 4. Interpreter dispatch — OPEN, and already measured

This is finding **A4b** of the sibling review, where it is scoped in detail.
Corrections to the advisory's framing:

- **"~50K LoC" is stale.** `vm/src/runtime/interpreter.rs` is **8,118 lines**
  after the 2026-07-30 split, plus submodules.
- **"~1.8K bytecode cases" is wrong.** There are ~200 JVMS opcodes; the raw
  fast-path `match` carries **122 arms**.
- **The "compact operand stack (SoA)" the advisory credits is indeed already
  done** — `CompactValue`, 8-byte NaN-boxed slots.
- The 122 arms are **superinstruction fusions** (one fuses
  `iload_X; iload_Y; if_icmplt` with inline PGO recording and the OSR hook), not
  duplicate opcode implementations. `#[inline(always)]` on "handlers" does not
  apply — there are no separate handler functions to inline; they are match arms.

What *is* real: the per-bytecode prologue. The per-bytecode safepoint poll alone
was measured at **≈7%** on an interpreter-bound loop (22 interleaved `--nojit`
runs; minima only — the box drifted 2.4×). So the table-driven rewrite has
measurable upside.

It is not free to take: the poll cannot simply be moved, because poll frequency
feeds the moving collector's per-cycle root-coverage proof (§2), where the
failure mode is a silent rise in `coverage_fallbacks` rather than a crash.

## 5. Parallel GC scanning — REFUTED for the young generation

*"Currently GC appears single-threaded."* Not for young-gen marking.

`gc/src/young_mark.rs` implements parallel young-generation marking, and it is
wired: `gen_heap.rs:7418` computes `young_gc_threads(bytes_before)` — worker
count adaptive to heap size, with `0`/`1` disabling parallelism — and
`gen_heap.rs:7419` calls `young_mark::drain_parallel`. Span zeroing is
parallelised over disjoint memory too, so workers need no synchronisation.

Old-gen and full-heap scanning are a fair target; the blanket claim is not.

## 6–10. Already done

- **TLAB sizing** — `gc/src/tlab.rs` has per-thread **adaptive sizing** state
  (T5.5.1) over a 256 KB default with a 1 MB cap for the sizer to work in.
- **Card-table batching** — the mutator barrier writes a **per-thread dirty
  buffer** (`thread_local_dirty_addr`), drained by `drain_pending` at GC start.
  `mark_dirty` (which takes the `cells` lock) is explicitly documented as the
  *slow*, GC-internal path the barrier must not call. Batching is the design.
- **`thread_registry`** — already an `RwLock`, not a mutex, changed
  deliberately in ARCH-2026-07-26 with the rationale recorded on the field: of
  ~50 accessors only 14 mutate, and the other ~40 (including every O(N)
  safepoint census) previously serialised on one global L5 lock.
- **`System.arraycopy`** — routes to `ctx.bulk_array_copy`, implemented with
  `copy_within` / `copy_nonoverlapping`. Not element-by-element.
- **JIT BCE / LICM** — `jit/src/x64/bce.rs`, `licm.rs`, `licm_int.rs` all
  present as separate passes.

## 11. mmap — partly done

- **Jars: done.** `classloading/src/class_path.rs:48` holds
  `Mapped(Arc<memmap2::Mmap>)`; `:161` maps the file, with a
  `loader_flags().disable_jar_mmap` escape hatch. `memmap2` is a real dependency
  of `classloading` and `native-io`.
- **jimage (`lib/modules`): deliberately not.** `reader/src/jimage.rs:465`
  records the choice — avoiding mmap keeps the reader portable without a
  `memmap2` dependency — and flags it as a future optimization. That is a
  standing decision, not an oversight.

---

## What to actually do

1. **Migrate `native_methods.find(` call sites to `NativeCallSite`** (§1). The
   only item that is both open and correctly specified. 44 sites, mechanism
   already proven at 10.
2. **Interpreter dispatch rewrite** (§4), justified by the ~7% measurement, but
   sequenced with the GC coverage-proof analysis, not ahead of it.
3. Optionally, the `class_layout` cold-path `RwLock` (§3) — small, same shape as
   A8's fix.

Items 2, 3, 5–11 of the advisory need no work. Item 2 in particular should be
struck from any future copy of that document: it describes removing a safety
proof that exists because its absence corrupted the heap.
