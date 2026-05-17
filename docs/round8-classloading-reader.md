# Round 8 — classloading / reader review

12 findings. Audit of round-7 wave 1+2 + deferred items + new angles.

---

## CRIT

### 1. `LinkResolver::get` allocates 2 refcount bumps per cache hit
`classloading/src/resolution.rs:507-522`
`guard.get(&(class_id, Arc::clone(name), Arc::clone(descriptor)))` — every hit builds a temp tuple, 2 atomic increments just to hash. Defeats most of the dedupe win on Spring's 20-50k repeat lookups. **Fix:** nest into `FxHashMap<ClassId, FxHashMap<(Arc<str>, Arc<str>), _>>` so the outer probe is int-key, or add a `Borrow`-friendly key newtype.

### 2. `LinkResolver` is declared but **never wired**
`classloading/src/lib.rs` re-exports it; no other code calls `get`/`insert`/`invalidate_class`. Cache exists; native-builtins wiring (deferred) never happened — reflective lookups still walk `find_method_recursive` linearly. **Fix:** wire from `native-builtins/src/lang_class.rs` reflection paths AND add `fire_link_resolver_invalidate_hook` at `class_manager.rs:3653` (the existing hook only covers `ResolutionCache`).

### 3. `class_init_state_handle` "fast path" = two RwLocks + HashMap lookup
`classloading/src/class_manager.rs:2969-2988` + `vm/src/vm/vm_util.rs:79-88`
Caller does `class_manager.read()` (outer RwLock) → `class_init_state_handle()` (inner RwLock + FxHashMap::get + Arc::clone) per dispatch. Doc claims "no RwLock round-trip"; reality is two. **Fix:** put `Arc<AtomicU8>` directly on `Class` (or a `ClassId`-indexed parallel `Vec`) so the load is `class.init_state.load(Acquire)`. Same issue at `redefine_generations` (line 810).

---

## HIGH

### 4. ptr_eq dispatch in `decode_attribute_body` misses 10 common annotation/module attrs
`reader/src/attribute.rs:732-769`
List has 19 canonical names but omits `Runtime{Visible,Invisible}{Parameter,Type}Annotations`, `AnnotationDefault`, `Module`, `ModulePackages`, `ModuleMainClass`, `Record`, `PermittedSubclasses`. Spring/Hibernate parameter-annotation decode falls through to the slow `&str` match. **Fix:** add `canon!()` entries + ptr_eq arms for all 10 missing names.

### 5. `find_class_by_name` uses `Arc::from(&str)` instead of `intern_arc` for probe keys
`classloading/src/class_manager.rs:3792, 3818`
`Arc::from(key.as_str())` allocates a fresh `ArcInner<str>` per probe; result never ptr_eq-matches the interned map keys. **Fix:** `rustjvm_types::intern_arc(key)` — one global-table lookup, refcount bump on hit, *and* the resulting Arc ptr_eq-matches pool-interned map keys.

### 6. Redefine rollback only captures 5 mutable fields; destructure trip-wire is debug-only
`classloading/src/class_manager.rs:3466-3501`
`RedefineInvariantSnapshot::assert_eq` is gated `#[cfg(debug_assertions)]`; release builds silently ship a partially-mutated class on verify-fail. The destructure forces compile-time enumeration but the runtime check is debug-only. **Fix:** capture a typed `RedefineMutationSet` (or full `Class` clone) so rollback can restore the documented mutable set regardless of what the new code path touched.

### 7. `ModuleRegistry::build_readability_graph` re-runs O(N²) on every dynamic module-info registration
`classloading/src/class_manager.rs:2238-2239`
`register()` clears `graph_built`; the immediate `build_readability_graph()` redoes the full transitive closure. N dynamic registrations = N² total work. **Fix:** defer rebuild — set a dirty flag and rebuild lazily on next `is_readable`/`module_for_package` query.

### 8. Generic signature parsing has **no cache** and allocates `String`s end-to-end
`reader/src/signature.rs:238-263, 369-379`
`Method.getGenericReturnType`, Spring's `ResolvableType`, Jackson `TypeFactory` all re-parse the same Signature string repeatedly. **Fix:** add `RwLock<FxHashMap<Arc<str>, Arc<ClassSig>>>` keyed on the interned Signature string; consumers ref-count the parsed tree.

---

## MED

### 9. `annotation_type_matches` forces a `String` allocation per probe
`classloading/src/annotations.rs:183-189`
`Fn(u16) -> Option<String>` callers allocate per call. **Fix:** change to `Fn(u16) -> Option<&str>` or `Option<Arc<str>>`.

### 10. `CanonicalizeCache::insert` overwrites value on duplicate but leaves FIFO order stale
`classloading/src/class_path.rs:129-141`
If a value ever differs (symlink mid-run), the stale entry sits in `order` and evicts in the wrong slot. **Fix:** assert value equality on collision, or move the key to the tail.

### 11. `read_file_for_classpath` mmap path always heap-doubles
`classloading/src/class_path.rs:53-86`
`mmap` + `Vec::with_capacity + extend_from_slice` pays syscall pair AND userland copy. Round-7 acknowledged the deferred "deep mmap" fix. **Fix:** change `ClassPathEntry::JarFile` to `ZipArchive<Cursor<Arc<Mmap>>>` so the mmap is the storage.

### 12. `StackMapTable::absolute_offsets` accumulator may panic in debug before the explicit overflow check
`reader/src/stack_map.rs:163-173`
Order is `let absolute = p + delta + 1; if absolute > u16::MAX ...`. In debug `p + delta + 1` panics before the check fires (unreachable in practice — prev was just bounded — but brittle). **Fix:** `p.checked_add(delta).and_then(|s| s.checked_add(1))`.
