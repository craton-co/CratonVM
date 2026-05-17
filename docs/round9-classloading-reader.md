# Round 9 — classloading / reader review

10 findings. Audit of round-8 fixes + remaining deferred work + new angles.

---

## CRIT

### 1. `class.init_state` is dead — side-table always wins; per-Class atomic never updates
`classloading/src/class_manager.rs:3005-3013` + `class.rs:383`
`class_init_state_handle` reads the side-table FIRST; if a handle was ever minted before the Class hit the store (or by `redefine_generation_handle` / a side-table init), that side-table Arc is returned. `set_class_init_state` (line 3034) then writes through THAT Arc — never `class.init_state`. The round-8 doc claims "both stores must be kept in lockstep"; the code does the opposite. Anyone who follows the round-8 invitation to `class.init_state.load(Acquire)` directly will get permanent UNINITIALIZED. **Fix:** swap the order — probe `class_store.get(class_id)` first and return `class.init_state`; only fall through to the side-table for synthetic IDs without a Class. Then collapse `init_states` into a synthetic-only side-table.

### 2. Two sibling lookups still allocate `Arc::from(name)` per probe — round-8 fix incomplete
`classloading/src/class_manager.rs:74, 1190`
Round-8 routed `find_class_by_name` + `find_class_by_name_in_loader` through `intern_arc`, but left `ClassStore::lookup` (line 74) and `get_loaded_class_id` (line 1190) doing `Arc::<str>::from(name)` — a fresh `ArcInner<str>` per probe that never `ptr_eq`-matches the interned `loaded_classes` keys. Both are on hot paths (vm dispatch and reflective `Class.forName`). **Fix:** identical conversion — `let probe: Arc<str> = rustjvm_types::intern_arc(name);`.

### 3. `read_bytes_overflow_returns_error` test does NOT exercise `checked_add`
`reader/src/buffer.rs:172-187`
Test sets `position = usize::MAX - 1` and calls `read_bytes(2)`. `(MAX-1).checked_add(2)` = `Some(MAX)` — NO overflow. The error comes from the bounds check (`data.get(pos..end)` returns None for the huge slice), not from `checked_add`. The regression the prompt added passes vacuously; a future refactor that removes `checked_add` will not be caught. **Fix:** use `read_bytes(3)` (or `usize::MAX`) so `(MAX-1) + 3` actually wraps.

---

## HIGH

### 4. `decode_attribute_body` ptr_eq dispatch still misses `ModulePackages` + `ModuleMainClass`
`reader/src/attribute.rs:707-805` (canon table) + match arms at `1050, 1058`
Round-8 added 10 canon entries but omitted `ModulePackages` (very common — every named module emits one) and `ModuleMainClass`. Both have Attribute variants (lines 92, 96) and `&str` match arms. Every module-info.class on a JPMS-aware app falls through the 28-arm `ptr_eq` chain into the slow `&str` compare. **Fix:** add `canon!(CANON_MODULE_PACKAGES, "ModulePackages")` + `canon!(CANON_MODULE_MAIN_CLASS, "ModuleMainClass")` plus the matching `Arc::ptr_eq` arms.

### 5. JNI `GetFieldID` cache key drops the field signature — wrong-type field can stick
`vm/src/native/jni.rs:1496-1517`
Key is `(class_id, name_str, "")`. JNI spec says `GetFieldID(cls, name, sig)` resolves on BOTH name and signature so callers can disambiguate a field shadowed in a subclass with a different type. The current cache stores the FIRST resolved field and serves it for ANY `sig` arg on subsequent calls. `find_field_recursive` itself ignores sig (`classloading/src/class.rs:820`), so the cache locks in a pre-existing wrong-resolution bug across all repeat calls. **Fix:** use the JNI sig in the cache key AND pass it down to a sig-aware `find_field_recursive_with_descriptor`.

### 6. `BUILTIN_LOADER_DELEGATION_CHAIN` constant defined, duplicated 3× elsewhere
`classloading/src/loaders.rs:45` (constant) vs `class_manager.rs:75, 1191, 3820`
Each of `ClassStore::lookup`, `get_loaded_class_id`, `find_class_by_name` re-inlines the same 3-element array. Adding a new built-in (platform loader for JEP-261 modular boot) requires 4 edits in lockstep. **Fix:** `use crate::loaders::BUILTIN_LOADER_DELEGATION_CHAIN;` at each site.

### 7. `LinkResolver` still unwired for non-JNI reflection paths
`classloading/src/lib.rs` + `native-builtins/src/lang_class.rs`
Round-8 wired only `jni_get_method_id`/`jni_get_field_id`. The same hot triple gets resolved on every `Class.getDeclaredMethod`, `Class.getMethod`, `Method.invoke`, MethodHandle `findVirtual/Static/Special` — none routes through `link_resolver.resolve_or_compute`. Spring/Jackson reflective probing pays the linear `find_method_recursive` cost per call. **Fix:** wrap each reflective entry in `native-builtins/src/lang_class.rs` with `shared.link_resolver.resolve_or_compute(...)` using the same closure pattern as JNI.

### 8. `Module` attribute dispatch decodes but ModuleRegistry rebuild is O(N²) (round-8 carry)
`classloading/src/class_manager.rs:2253-2254`
`register()` + immediate `build_readability_graph()` per lazily-loaded module-info. A modular runtime with 80 modules pays 80 full rebuilds. Flagged round-8; unfixed. **Fix:** set a `graph_dirty` flag on `register()`, build lazily on first `is_readable`/`module_for_package` query.

---

## MED

### 9. `LazyLock<Arc<str>>` per call to `decode_attribute_body` — 28 deref checks per attribute
`reader/src/attribute.rs:702-741`
The 28 `canon!()` invocations declare `static $name: LazyLock<...>` inside the function. Each `Arc::ptr_eq(name, &CANON_*)` dereferences the LazyLock — that's an `AcqRel` once-flag check per arm per dispatch. On a Spring class file with 200 attributes that's 5,600 LazyLock checks. **Fix:** lift all 28 canon constants to module scope (or a single `static CANON: [Arc<str>; 28]` initialized once), so the per-call cost is just pointer compares.

### 10. `check_module_access` allocates `module_pkg_of(&target.name)` on every cross-module call
`classloading/src/access_control.rs:228`
Fires on every cross-module method/field dispatch on a JPMS app. `module_pkg_of` scans the name for `/` (likely allocates a slice). The result is fully a function of `target.name` (immutable) — cache it on `Class` once. **Fix:** add `target_pkg_cache: OnceLock<Arc<str>>` on `Class`, populate on first call; the readability check then needs only an `Arc<str>` clone (refcount bump).

---

(529 words)
