# Round 4 — Classloading Crate Review

Sorted by impact. Severity tags: `[CRIT]` correctness/security, `[HIGH]` measurable hot-path win or latent bug, `[MED]` smaller perf gain or robustness, `[LOW]` cleanup.

---

## 1. [CRIT] `name_to_id` keyed by raw FNV-1a hash — collisions return wrong ClassId

**File:** `classloading/src/class_manager.rs:618` (declaration), `:712-721` (hash fn), `:837-839` (lookup), `:2147,3116,3267,3469` (inserts).

`name_to_id: FxHashMap<u64, ClassId>` stores **only** the 64-bit FNV-1a digest of the class name — no name verification on lookup. `get_loaded_class_id` returns the first ClassId mapped to that digest, so any two class names that collide under FNV-1a return the wrong class. FNV-1a is a 64-bit non-cryptographic hash; collisions are rare but realistic on adversarial input (e.g. an attacker-crafted class name in a JAR), and the failure mode is silent type-confusion downstream. Worse: the key carries no `ClassLoaderId`, so different loaders cannot register classes with the same name through this map at all — only the `loaded_classes` map captures loader identity.

**Impact:** Wrong-class returned from every fast-path lookup that goes through `get_loaded_class_id` (called from `load_class:1360`, `synthesize_array_class:3312`, `ensure_synthetic_class:853`, `create_synthetic_stub:3128`). With multiple loaders, cross-loader name namespace collapses.

**Fix:** Either (a) make the value `(Arc<str>, ClassLoaderId, ClassId)` and verify name+loader on hit, or (b) drop `name_to_id` entirely and key `loaded_classes` with `(ClassLoaderId, &str)` lookups via `Borrow<str>` — `FxHashMap` already hashes the tuple cheaply.

---

## 2. [CRIT] `redefine_class` skips Pass-2 and Pass-3 verification on new bytes

**File:** `classloading/src/class_manager.rs:2597-2996` (`redefine_class`); contrast `:2101` where `define_class_with_options` calls `verifier::verify_class`.

The JVMTI `RedefineClasses` path validates the class file header (magic+length, `this_class` name match) and structural equivalence (same super, same interfaces, same field/method signatures), then writes the new `methods` + `constant_pool` straight onto the live `Class` with **no call to `verifier::verify_class` or `bytecode_verifier::verify_bytecode`**. Malformed bytecode (bad branch targets, broken StackMapTable, type-state-incompatible operand stack) bypasses verification entirely and crashes the interpreter mid-method.

**Impact:** Real correctness bug. Any JVMTI agent (or `Instrumentation.redefineClasses` call) can install bytecode that the verifier would have rejected.

**Fix:** Between step 5 (in-place swap) and step 6 (vtable install), call `verifier::verify_class(&new_class_view, &self.class_store, &hierarchy)`. Snapshot the previous methods/cp before the swap so a verification failure can roll back atomically.

---

## 3. [CRIT] `ResolutionCache` entries stale after `redefine_class`; only `InvokeCache` invalidated

**File:** `classloading/src/class_manager.rs:2972-2986` (redefine bumps generation + fires JIT invalidate); `classloading/src/resolution.rs:239-343` (`ResolutionCache`).

`redefine_class` step 7 bumps the per-class generation counter (which `InvokeCache::get` polls via `RedefineGate`) and step 8 fires the JIT-invalidate hook. But `ResolutionCache` — keyed `(ClassId, cp_index)` and holding `ResolvedField`/`ResolvedMethod`/`ResolvedCallSite`/condy values that were resolved against the **old** constant pool — has no gate and no invalidate hook. After redefine, the cp indices in the new pool may refer to different fields/methods, but `resolve_field`/`resolve_method` happily return the cached old resolution.

**Impact:** Wrong field/method resolved on every cached cp-index for the redefined class after the redefine. Affects accuracy of `getfield`/`getstatic`/`invokestatic`/`invokevirtual` slow paths and condy evaluation.

**Fix:** Either (a) add a `RedefineGate` to each cached entry and check on get (mirror `InvokeCache`), or (b) add a `clear_for_class(class_id)` helper to `ResolutionCache` and call it from `redefine_class` step 8 alongside the JIT hook. (b) is simpler — redefine is cold.

---

## 4. [HIGH] `VType::ObjectRef(String)` / `ArrayRef(String)` allocates on every clone — verifier frame-clone hot path

**File:** `classloading/src/vtype.rs:69,72` (declaration); `verify_frame.rs:48,92,126,155,180,209` (every frame transition clones the locals/stack vec, which deep-clones VType strings); `verify_insn.rs` and `verifier.rs` create `VType::ObjectRef(name.to_string())` ~15 times.

`VerificationFrame::clone()` is called once per declared StackMapTable frame target, once per exception handler entry, and once per branch on the worklist verifier (`verifier.rs:425,580+`). Each clone walks `locals` + `stack` (both `Vec<VType>`) and heap-allocates a fresh `String` for every `ObjectRef`/`ArrayRef`. A typical 200-instruction method with 20 StackMapTable frames clones ~500 VType strings per verification — and verification runs once per class load.

**Impact:** Hot during cold-start class loading (every class with branches gets verified). Profiled, this is consistently among the top String allocators.

**Fix:** Change `ObjectRef(String)`/`ArrayRef(String)` to `Arc<str>`. Constructors (`from_verification_type_info`, `from_field_descriptor`, `initial_frame`, `compact_initial_frame`) already hand out short-lived owned values; switch to `intern_arc()` from `cratonvm_types`. Clone becomes a refcount bump.

---

## 5. [HIGH] `Class::is_subclass_of` re-walks interfaces with no visited set — exponential on diamond hierarchies

**File:** `classloading/src/class.rs:600-631`.

The recursion walks the superclass chain **and** every interface at every level, without a visited set. In a diamond hierarchy where two interfaces share a super-interface (extremely common: `Collection` ← `List`, `Collection` ← `Set`, etc.), the same node is visited 2^depth times. This is called from `check_field_access`/`check_method_access` (`access_control.rs:75,132`) on every protected-access check, and from many other VM hot paths.

**Impact:** A class with ~10 interfaces and a few diamonds can cost milliseconds per `is_subclass_of` call. Multiply by every protected member access in a tight loop.

**Fix:** Switch to an iterative BFS/DFS with a small `SmallVec<[ClassId; 16]>` visited set (most hierarchies are shallow). Or memoize at the `ClassManager` level keyed on `(child, parent)` since the hierarchy is mostly stable post-link.

---

## 6. [HIGH] `class_bytes_cache` keeps a `Vec<u8>` copy of every class file — doubles class-data resident memory

**File:** `classloading/src/class_manager.rs:626` (declaration), `:2148` (insert on every define), `:2693,2950` (read/update on redefine).

Every `define_class` runs `self.class_bytes_cache.insert(name.to_string(), bytes.to_vec())` — full copy of the input bytecode, kept alive forever, in addition to the parsed `Class.constant_pool` + methods + attributes. A medium Spring app loads ~15k classes averaging ~6 KB each → ~90 MB of duplicate bytes resident even when no JVMTI agent will ever call `RetransformClasses`/`getResourceAsStream`.

**Impact:** Sustained RSS overhead proportional to total .class size; not a CPU win but a real footprint hit.

**Fix:** Store `Arc<[u8]>` instead of `Vec<u8>` and share the same allocation with the reader's input buffer where possible. Additionally, gate the cache behind a flag enabled only when JVMTI is attached or `Class.getResourceAsStream` is observed — most processes never read it.

---

## 7. [HIGH] `find_class_by_name` falls back to O(n × |loaders|) full scan of `loaded_classes`

**File:** `classloading/src/class_manager.rs:3019-3065`.

When the targeted-loader probes miss, the fallback at `:3052-3063` iterates **every entry** in `loaded_classes` for each key candidate (the slash and dot forms). With 20k loaded classes and the canonical 3-loader chain, the worst case is 60k comparisons per call. This is reachable from `Class.forName` / mirror lookups that probe by display name.

**Impact:** Linear scan triggers whenever a name probe misses the bootstrap/extension/application loaders — a common case for user-defined loaders. O(n) per probe scales badly past a few thousand classes.

**Fix:** Either (a) drop the fallback (probes with non-standard loaders should go through `find_class_by_name_in_loader` with the correct id), or (b) maintain a second index keyed on `Arc<str>` → `Vec<(ClassLoaderId, ClassId)>` for the rare cross-loader name resolution.

---

## 8. [HIGH] `define_class_with_options` walks the class-level attribute list ~10 times

**File:** `classloading/src/class_manager.rs:1709-1899` (source_file, bootstrap_methods, annotations loop, signature, nest_host, nest_members, record_components, permitted_subclasses, inner_classes, enclosing_method, module — 10+ separate `find_map`/`for` passes over `class_file.attributes`).

Each `class_file.attributes.iter().find_map(...)` is an O(n) walk; with N attributes and K extractions, total work is O(N·K). Attribute lists are tiny (~5-15 entries) but the work runs on every single class load. The same pattern is duplicated in `upgrade_synthetic_class` (`:3557-3656`) and `redefine_class` (`:2898-2918`).

**Impact:** Microseconds per class, multiplied by 15k+ classes at cold start = ~10-50 ms of pure overhead, plus L1 cache churn (each walk re-reads the full attribute Vec).

**Fix:** Single `for attr in &class_file.attributes { match attr.as_decoded() { ... } }` with mutable accumulators for each output field. Halves the work, fits in one cache line per attribute, drops 30+ lines of repetition.

---

## 9. [HIGH] Three SipHash `std::collections::HashMap`s in `bytecode_verifier::verify_method`

**File:** `classloading/src/bytecode_verifier.rs:11` (import), `:172,176,491` (`HashMap::with_capacity`), `:497` (`HashSet::new`). Same pattern in `verifier.rs:268-294,425-430,1087`.

These are the per-method type-state maps (`declared_frames: HashMap<u16, VerificationFrame>`, `handler_targets: HashMap<u16, VType>`, `frame_at: HashMap<usize, VerificationFrame>`, `enqueued: HashSet<usize>`). Keys are `u16`/`usize` — trusted internal indices, ideal `FxHashMap` candidates. SipHash adds a measurable per-key cost compared to FxHash on integer keys, and these maps are probed/inserted at every instruction during type-state walk.

**Impact:** Per-method verification cost. Multiplied by all methods in all loaded classes during cold start.

**Fix:** Swap to `FxHashMap`/`FxHashSet` from `crate::fx_hash` — these are the exact use cases the existing FxHash module was added for. Mirror in `verifier.rs` (the duplicate copies).

---

## 10. [MED] `ClassPath::find_class` fs::canonicalize per Directory entry, per probe

**File:** `classloading/src/class_path.rs:990-1011`.

For every classpath Directory entry, every `find_class` call does `fs::canonicalize(dir)` **and** `fs::canonicalize(&full_path)` — two syscalls per directory per class load. Both are bounded by symlink resolution and disk I/O. The directory root canonical path is constant across the process lifetime.

**Impact:** Two syscalls per (loaded class × directory entry) on the cold-start path. On Windows in particular `canonicalize` is expensive.

**Fix:** Cache the canonical root once per `ClassPathEntry::Directory` (compute in `add_path` / `new`, store as a field, reuse). Still need to canonicalize the resolved file to defeat symlink-escape, but the root-side cost amortizes to zero.

---

## 11. [MED] `CpBuilder::add_utf8` allocates a `String` key on every lookup (cache miss or hit)

**File:** `classloading/src/proxy_gen.rs:420-433` (also `add_class:438`, `add_string:451`, etc).

`add_utf8(&str)` builds `CpKey::Utf8(s.to_string())` **before** the dedup probe, allocating a heap String even on cache hit. Proxy generation emits many redundant utf8/class entries (interface names, method names, descriptors) and dedupes them — so the allocate-then-probe pattern blows out the cache hit fast path.

**Impact:** N allocations per proxy class instead of (unique-N) allocations. Lambda / JDK Proxy generation runs frequently in modern apps.

**Fix:** Either (a) use `HashMap`'s `raw_entry_mut` API to probe by `&str` without allocating, or (b) restructure `CpKey::Utf8` to wrap `Arc<str>` and intern once at the call site, or (c) for the common-case probe-then-insert, compute the hash with a borrowed key first and only allocate on insert.

---

## 12. [MED] `vtable_descriptors` cloned wholesale into every subclass at link time

**File:** `classloading/src/class_manager.rs:2254-2278,2180`.

`build_vtable_descriptors_with_overrides` clones the **entire** superclass vtable descriptor vec into the new class's seed (`:2256-2259`). For deep hierarchies (Spring's `AbstractApplicationContext` chain, Scala's `AbstractIterable` chain), each class along the chain ends up holding its own copy of the inherited slots — O(depth × width) total memory for what is structurally a copy-on-write delta.

Then the class's own descriptor vec is `clone()`d again into `self.vtable_descriptors.insert(...)` and again for the install hook (`:2180-2181`).

**Impact:** Two redundant Vec clones per class link + super-vec copy. Linker memory churn dominated by these on deep hierarchies.

**Fix:** Store vtable descriptors as `Arc<[Option<VtableSlotDescriptor>]>` and have subclasses CoW via `Arc::make_mut` only when they actually override. The override list could even be stored as a delta layer until the install hook materializes.

---

## 13. [LOW] `module.rs` SipHash `HashMap`s on package/module name keys

**File:** `classloading/src/module.rs:173-211` (`modules`, `package_to_module`, `readable`, `extra_reads`, `extra_exports`, `extra_opens` — all `HashMap<String, _>`).

Module/package names are short trusted internal strings, perfect FxHash targets. `module_for_package` (`:257`) is called from `access_control::check_module_access` (every cross-module access check). `reads` (`:455`) probes `readable` on every JPMS access check.

**Impact:** Per-access check overhead; not catastrophic but consistent.

**Fix:** Swap to `FxHashMap`. The `extra_*` mutation paths are cold; the read paths are hot.

---

## 14. [LOW] `find_method_recursive` / `find_field_recursive` use `std::HashSet<ClassId>` for visited

**File:** `classloading/src/class.rs:784,843`.

These are recursive interface-walk helpers — called from `vm/runtime` dispatch slow paths and verifier. Keys are `ClassId` (trusted u32). SipHash is overhead.

**Fix:** Use `FxHashSet` from `crate::fx_hash`. Better: for the typical small visited set (<32 ids), a sorted `SmallVec<[ClassId; 16]>` with linear-scan `insert` beats any hashmap.

---

## Notes for Round 5 scope

- **`verifier.rs` ↔ `bytecode_verifier.rs` duplication**: `verify_method_typestate` (`verifier.rs:179-396`) is a near-line-for-line copy of `bytecode_verifier::verify_method` (`bytecode_verifier.rs:80-450`) — extracted to share infrastructure rather than maintaining two copies. Not listed above because the perf win is uncertain; it's a maintainability concern.
- **`verify_class_structure` is called inside `verifier::verify_class` but its own `verify_method_structural_only` is then redundantly re-invoked from `verify_method_typestate:186`** — confirm this isn't double work.
- **`module.rs::detect_cycles` uses unbounded recursion** in DFS (`:412-439`); a `MAX_MODULES`-deep adversarial graph would stack overflow. Cold path, but flagging.
