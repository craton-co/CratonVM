# Round 5 — Classloading Crate Review

Audit of round-4 fixes plus carry-over and new findings. Severity: `[CRIT]` correctness/security, `[HIGH]` hot path / latent bug, `[MED]` smaller perf / robustness, `[LOW]` cleanup.

---

## (A) Round-4 fix audit

### 1. [HIGH] `get_loaded_class_id` + `find_class_by_name` still linear-scan `loaded_classes`

**File:** `classloading/src/class_manager.rs:942-963` and `:3351-3418`.

`get_loaded_class_id` (line 958) iterates **every** `(loader, name)` entry to cover user-defined loaders; round-4 fix only addressed `find_class_by_name`. Worse, `find_class_by_name` (line 3397-3404) still walks `loaded_classes.keys()` *every call* to materialise the `user_loaders: FxHashSet<ClassLoaderId>` — defeating its own optimisation. On a 15 k-class app the keys() walk dominates.

**Fix:** Maintain `user_loaders: FxHashSet<ClassLoaderId>` as a `ClassManager` field; insert/remove on define/unload. Then both hot paths become O(loaders).

### 2. [HIGH] `redefine_class` snapshot/rollback omits `vtable_descriptors`

**File:** `classloading/src/class_manager.rs:3074-3174,3204-3206`.

The rollback covers `methods/constant_pool/bootstrap_methods/annotations/source_file`. If verification at line 3148 **succeeds**, `build_vtable_descriptors` (line 3204) runs and overwrites `self.vtable_descriptors[class_id]` — good. But if verification **fails** AFTER the new methods were briefly installed, the methods are rolled back but no `vtable_descriptors` mutation happened yet (good). However, `class_bytes_cache` was **not** snapshotted: the `insert_class_bytes` at line 3187 is reached only on success, so OK there too — but `ResolutionCache::invalidate_class` (step 9) and the JIT invalidate (step 8) are skipped on rollback. That's correct (no swap → no staleness) but worth a comment for future readers.

**Real bug:** the snapshot does **not** record `cls.signature`, `cls.has_finalizer`, `cls.code_source`, `cls.nest_host`, `cls.nest_members`, `cls.permitted_subclasses`, `cls.inner_classes`, `cls.enclosing_method`, `cls.record_components`. JEP 109 forbids changing these — and the swap above doesn't write them — so a failed verify can't corrupt them. OK as written, but assertion-only verification of "swap fields == snapshot fields" would future-proof it. **Fix:** add a debug_assert! enumerating the mutated field set, then add any future field to the snapshot.

### 3. [MED] Stale `class_bytes_cache` evicted entry → empty `old_bytes` for `ClassFileLoadHook`

**File:** `classloading/src/class_manager.rs:2785-2796`.

On JVMTI redefine, `class_bytes_cache.get(&existing_name).cloned()` may return `None` if the 16 MiB FIFO evicted the class earlier. We pass `&[]` to the agent — comment says "agents tolerate that", but JaCoCo and Mockito Inline read original-bytes to diff. **Fix:** when JVMTI agent attached with `can_retransform_classes` or `can_redefine_classes`, raise the cap to `usize::MAX` (or pin retained classes); detect via the same `RESOLUTION_INVALIDATE_HOOK_ACTIVE`-style flag.

### 4. [MED] `vtype.rs` Arc<str> migration — `Arc::from(format!(...).as_str())` re-allocates per merge

**File:** `classloading/src/vtype.rs:280,286,367,371,372,375` (also `common_superclass` returns `String`).

`Arc::from(hierarchy.common_superclass(a,b).as_str())` allocates `String` then `Arc<[u8]>` (refcount + copy). Constant literals (`"java/lang/Object"`) re-Arc on every merge call — hot during frame merge. **Fix:** intern `"java/lang/Object"` once via `rustjvm_types::intern_arc` into a `Lazy<Arc<str>>`; change `common_superclass` to return `Arc<str>` so the merge path is a refcount bump.

### 5. [LOW] `is_subclass_of_inner` visited-set fix correct, but allocates on every call

**File:** `classloading/src/class.rs:610-666`.

`FxHashSet::default()` allocates per call. Call-sites in `interpreter.rs`, `vm_exec.rs`, `helpers.rs` hit it on every `instanceof`/`checkcast`/exception-dispatch. **Fix:** pass a reusable `&mut SmallVec<[ClassId; 16]>` from the caller (interpreter has a per-frame scratch already), or use a small-stack visited that falls back to heap only after >16 entries.

---

## (B) Round-4 carry-over

### 6. [MED] `class_path.rs:990` `fs::canonicalize(dir)` per probe still uncached

**File:** `classloading/src/class_path.rs:990-1000`.

Round 4 MED #10 unresolved. `canon_dir` is recomputed per `find_class` call (two syscalls × N directories × M classes). On Windows the GetFinalPathNameByHandle round-trip is ~50 µs each. **Fix:** add `canonical_root: OnceLock<PathBuf>` to `ClassPathEntry::Directory`; compute once in `add_path`/`new`.

### 7. [MED] `CpBuilder` allocates `String` key before dedup probe

**File:** `classloading/src/proxy_gen.rs:420-432,437-447,450-461,464-477`.

Round 4 MED #11 unresolved — `CpKey::Utf8(s.to_string())` still happens on every hit. **Fix:** change `CpKey::Utf8(Arc<str>)`, intern once via `rustjvm_types::intern_arc` (returns the same Arc on second call → free dedup); or use `HashMap::raw_entry_mut().from_key(s)` for probe-by-`&str`.

### 8. [MED] vtable Vec cloned twice per class link

**File:** `classloading/src/class_manager.rs:2270-2271,3205-3206,2344-2349`.

`self.vtable_descriptors.insert(id, entries.clone()); fire_vtable_install_hook(id, entries)` — one full Vec clone solely so the hook can take owned data. Same shape at line 3205-3206 in `redefine_class`. And the superclass seed (`:2344-2349`) clones the whole parent vec. **Fix:** store `Arc<[Option<VtableSlotDescriptor>]>`; `Arc::clone` for both insert and hook hand-off, and the subclass seed.

---

## (C) New angles

### 9. [MED] `ClassState::Initialized` fast-path goes through `RwLock` read on every static field access

**File:** `vm` interpreter sites — class state lives behind `class_manager.read()`. Once `state == Initialized`, the lock cost is pure overhead (state never regresses except on redefine, and redefine doesn't reset it).

**Fix:** keep a per-class `AtomicU8` "init state" outside the RwLock-guarded `Class`; interpreter's `getstatic`/`putstatic`/`invokestatic` does a relaxed atomic load — `== Initialized` → skip both the lock and the init slow path entirely. Mirror of HotSpot's `_init_state` byte.

### 10. [MED] No `LinkResolver` cache for non-cp-keyed resolutions

**File:** `classloading/src/resolution.rs:239-383`.

`ResolutionCache` is keyed `(ClassId, cp_index)` — but `Lookup.findVirtual`, `MethodHandles.lookup().findStatic`, and reflective `getDeclaredMethod` all hit the resolver with `(class, name, descriptor)` triples that have **no cp index**. Each goes through the full hierarchy walk. **Fix:** add a second cache keyed `(ClassId, Arc<str>, Arc<str>) → ResolvedMethod`, invalidated by the same `invalidate_class` hook (round-4 fix #3).

### 11. [LOW] Class-loader parent chain re-walked on every miss

**File:** `classloading/src/class_manager.rs:1557-1578`, `find_class_bytes_delegated`.

The parent chain (CDS → Bootstrap → Extension → Application) is fixed at process start. Each miss does 4 trait calls + 4 inner classpath probes. **Fix:** flatten into a single `Vec<&dyn ClassFinder>` walked once, plus a `Bloom` filter per finder over its known class-name set populated lazily — a miss short-circuits without touching the JAR central-directory hash table.

### 12. [LOW] `find_class_by_name` re-collects user-loader set every call

**File:** `classloading/src/class_manager.rs:3397-3404` — already covered by #1 above; listing as a distinct fix since #1's index would also let `get_loaded_class_id`'s linear scan vanish.

---

## Notes

- Round-4 `name_to_id` removal is correct: 6 former lookup/insert sites all rewritten with `(loader_id, Arc<str>)` keys; built-in-loader probes are loader-aware; the only remaining linear scans are the user-defined-loader fallbacks called out in #1.
- `RESOLUTION_INVALIDATE_VM` wiring is race-free: redefine holds `&mut ClassManager` for steps 5-9, so no concurrent reader can observe new methods before the resolution-cache invalidate completes.
- `is_subclass_of` migration to `is_subclass_of_inner` is called from all 25 hit sites I checked (classloading + vm); no recursive caller bypasses the visited set.
