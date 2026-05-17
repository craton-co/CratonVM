# Round 5 — VM crate review


## 1. [CRIT] Wave-2 ≥5-arg JIT bailout fix only applied to MIC; three inline sites in `jit_invoke_dispatch` still silently `_ => 0`

**File:** `vm/src/jit/helpers.rs:1229`, `:1253`, `:1295`, `:1319`, `:1359`, `:1387`

`call_jit_compiled_method_entry` (`:163`) with bailout is wired into
MIC at `:1722`, but the three inline arms in `jit_invoke_dispatch` —
DISPATCH_CACHE hit, JIT-cache hit, post-compile — still contain
literal `_ => 0,`. Callees with ≥5 i64 args silently return zero.

**Fix:** Replace each match-arm body with
`call_jit_compiled_method_entry(...)`; reconstruct `bail_args` once
before the cache check.

## 2. [CRIT] OSR rejected entry-PC banned for lifetime of frame — wave-2 broke retry-on-transient-failure

**File:** `vm/src/runtime/frame.rs:154`, `vm/src/runtime/interpreter.rs:2668` (and 11 more sites)

`!osr_attempted_entry_pcs.contains(&entry_pc)` permanently bans the
loop entry on first reject. Skip-list races, compile-queue pressure,
or eviction kill OSR for the frame's lifetime. Round-4 doc recommended
modulo retry, not a permanent ban.

**Fix:** Replace Vec ban with `(bc - last_attempt) >= OSR_THRESHOLD`
plus ~3-attempt cap; drop `osr_attempted_entry_pcs`.

## 3. [CRIT] Wave-1 NPE-on-null-array fix incomplete: `jit_baload`/`bastore`/`iastore`/`aastore` still silently no-op

**File:** `vm/src/jit/helpers.rs:509`, `:524`, `:560`, `:597`

Wave-1 fixed `iaload`/`aaload`/`arraylength` but four sister helpers
still `return 0` / `return;` on null array, violating JVMS NPE
requirements on null array store.

**Fix:** `if array_ptr == 0 { set_jit_pending_npe(); return; }`
(loads return `i64::MIN`). Interpreter drain at `:12331` handles it.

## 4. [HIGH] `jit_invoke_dispatch` still allocates 3× `Arc::from(&'static str)` per JIT-cache lookup

**File:** `vm/src/jit/helpers.rs:1261-1263`

`info.class_name`/method_name/descriptor are `&'static str`; each
`Arc::from` heap-allocs. Round-4 doc #5 flagged; wave-2 missed.

**Fix:** `JitCache::get_by_str(&str, &str, &str)` keyed on borrowed
slices (same fxhash as Arc form).

## 5. [HIGH] `ThreadLocalResolveCache::get_method`/`get_field` still double-lookup (`contains_key` then `get`)

**File:** `vm/src/runtime/lockfree_resolve.rs:139-147`, `:172-180`

Round-4 fixed VecDeque eviction but left double SipHash probe per
hit. Hot for resolution churn.

**Fix:** `match self.methods.get(key) { Some(t)=>{self.hits+=1;Some(t)}
None=>{self.misses+=1;None} }`.

## 6. [HIGH] Monitorenter/exit unconditionally calls `Instant::now()` + clones 2 Strings per op

**File:** `vm/src/runtime/interpreter.rs:6717-6748`

`_diag_method`/`_diag_class` are `.to_string()`'d on every op even
though the closure defers. Plus `Instant::now`/`elapsed` on every
uncontended lock (~20 ns each on Windows QPC).

**Fix:** Drop the unused String bindings; gate `Instant::now` behind
`shared.config.jfr_enabled`.

## 7. [HIGH] `make_method_key` still clones 2 Arc per profiled branch

**File:** `vm/src/runtime/interpreter.rs:2366-2372`

Wave-2 added `..._arc_ref()` borrow but still `Arc::clone` twice
into `MethodKey`. Per back-edge during PGO warmup.

**Fix:** Make `MethodKey` hash-based (`class_id, name_hash, desc_hash`);
ProfileStore interns internally.

## 8. [HIGH] `CompactValue::object` still has release-mode `assert!` (round-4 #15 unfixed)

**File:** `types/src/compact_value.rs:229-237`

Two release `assert!` on every object push/aload. Trusted callers
should not pay two branches per slot.

**Fix:** Demote to `debug_assert!` (lines 216/220 already cover debug);
untrusted callers use `try_from_pointer`.

## 9. [HIGH] `invoke_method_shared` double-copies bytecode: `Arc<[u8]>::to_vec()` then `padded_bytecode` clones again

**File:** `vm/src/runtime/interpreter.rs:2251` + `frame.rs:25`

`code_attr.code` is already `Arc<[u8]>`; we copy to Vec then re-clone
into padded Arc. Two full bytecode mem-copies per non-cached frame.

**Fix:** `Frame::new_from_arc_code(code_arc, ...)`; cache padded Arc
in `CachedBytecodeMethod` so each method pads once.

## 10. [MED] OSR back-edge logic duplicated verbatim across ~12 sites

**File:** `vm/src/runtime/interpreter.rs:2664+`, `:2994+`, `:3032+`, `:3100-3313`

Same 14-line block inlined 12×, several on single-line spans. Any
correctness fix to finding #2 must be applied 12 times.

**Fix:** Extract `#[inline(always)] fn try_back_edge_osr(...)`,
replace each site with one call.

## 11. [MED] `class_disables_interp_fast_path` does `contains("springframework")` per Frame::new

**File:** `vm/src/runtime/frame.rs:346`

Mid-string substring scan runs for every class. Spring names start
with `org/springframework/`.

**Fix:** `starts_with("org/springframework/")`. Better: implement
`TODO(round-4-wave-3)` per-method `unsafe_for_fast_path` flag at
install time.

## 12. [MED] `find_exception_handler` and `find_exception_handler_any_pc` are 70-line near-duplicates

**File:** `vm/src/runtime/interpreter.rs:4698-4767` vs `:4783-4834`

Identical lock-hoist + catch-type resolution; differ only by caller's
`pc == usize::MAX` sentinel. Correctness drift risk.

**Fix:** Merge to one function; explicit
`if entry.catch_type == 0 && pc == usize::MAX { continue; }` for the
no-PC case.
