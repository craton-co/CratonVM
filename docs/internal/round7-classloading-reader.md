# Round 7 — classloading + reader audit

Round-6 wave-1 verification (Section A) plus carry-overs (B) and new angles (C). Audit-A findings tagged **CRIT**.

---

## (A) Round-6 wave-1 verification

### [CRIT] 1. `RedefineInvariantSnapshot` exhaustiveness check is debug-only — release builds silently lose invariants
**File:** `classloading/src/class_manager.rs:865-866, 896, 908-939`

The whole `RedefineInvariantSnapshot { ... }` struct and `impl from_class` block are gated on `#[cfg(debug_assertions)]`. The clever "no trailing `..` in the destructure → adding a new `Class` field is a compile error" trip-wire fires **only in debug builds**. A release-only contributor who adds a new `Class` field (e.g. `module_version: Option<Arc<str>>`) ships a class manager that silently drops the new field from the redefine invariant set — exactly the failure mode the snapshot was designed to prevent.

**Fix:** Drop the `cfg(debug_assertions)` on the *struct + `from_class`* (compile in all builds; cost: 0 instructions in release if unused), keep it only on the `assert_eq` call site. Or: add a `#[cfg(not(debug_assertions))] fn from_class(_: &Class) {}` stub that still pattern-matches with all fields so the compile error still fires.

### [CRIT] 2. `loaded_classes` insert at lines 3990 / 4194 bypasses `user_loaders` set but the rationale isn't asserted
**File:** `classloading/src/class_manager.rs:3989-3990, 4193-4194`

Both sites hard-code `ClassLoaderId::Bootstrap` so skipping `user_loaders.insert` is correct **today**. But there is no `debug_assert!(matches!(loader_id, ClassLoaderId::Bootstrap))` and no documented invariant; a future refactor that parametrizes either synthesis path on the caller's loader silently desyncs `user_loaders` from `loaded_classes`, breaking `find_class_by_name` for user-defined loaders.

**Fix:** Replace the literal `ClassLoaderId::Bootstrap` with a named `const BOOTSTRAP_ONLY_SYNTHESIS: ClassLoaderId = ClassLoaderId::Bootstrap;` and add `debug_assert_eq!(BOOTSTRAP_ONLY_SYNTHESIS, ClassLoaderId::Bootstrap)` plus a one-line comment at both sites pointing to `define_class_with_options:2458` for the `user_loaders` invariant.

### [CRIT] 3. `class_init_state_handle` slow path drops the read guard implicitly before write — fine, but races create duplicate Arcs for one tick
**File:** `classloading/src/class_manager.rs:2930-2946`

Read-then-write upgrade is correct (Acquire/Release pair at the consumer side is fine). However when N threads race the same uncached `class_id`, all N enter the write lock serially and *each* calls `or_insert_with`. The first allocates the `Arc<AtomicU8>`; the rest find the entry and skip. Correct, but each loser still allocated a `Box<AtomicU8>` candidate via `or_insert_with`? — `entry().or_insert_with(...)` only invokes the closure on vacant, so this is actually fine. **Verified safe.** No fix needed; flagging for completeness.

### [HIGH] 4. `canonicalize_cache` is unbounded and never invalidated
**File:** `classloading/src/class_path.rs:29, 423-438`

`HashMap<PathBuf, PathBuf>` grows monotonically across process lifetime; if a probed path is later deleted/moved/symlink-retargeted, the cache returns the stale canonical path and the symlink-safety check (the whole reason the cache exists) silently accepts a now-traversal-able path. Comment promises "errors are not cached" but says nothing about successful entries.

**Fix:** Either (a) document the contract — "process-lifetime cache; classpath roots are immutable post-startup" — and add a `debug_assert!(self.classpath_frozen)` guard, or (b) bound the cache (LRU, 4096 entries) and re-canonicalize on cache eviction.

### [HIGH] 5. `to_arc()` is called inline at vtable install but the result is not deduped across overrides
**File:** `classloading/src/class_manager.rs:2694`

When a subclass inherits a non-overridden method, the vtable install copy-out builds a *fresh* `VtableMethodSnapshot` per subclass with `code_attr.code.to_arc()` — each subclass allocates+memcpys the same parent bytecode. For deep hierarchies (Spring: ~40k vtable slots, ~30% inherited) this is ~12k redundant `Arc<[u8]>` allocations.

**Fix:** Cache the `Arc<[u8]>` on the *declaring class*'s `Method` (e.g. `method.code_arc: OnceLock<Arc<[u8]>>`) and `Arc::clone` it into each subclass's snapshot. One alloc per declared method, not per vtable slot.

---

## (B) Round-5 / round-4 carry-overs (still unfixed)

### [HIGH] 6. `stack_map::absolute_offsets` u16 overflow (round-4 #6, round-5 #2)
**File:** `reader/src/stack_map.rs:154`

`p + delta + 1` wraps silently in release builds for methods near 64 KB.

**Fix:** Compute as `u32`, return `Vec<u32>`, reject `absolute > 0xFFFF` with `InvalidClassData`.

### [HIGH] 7. `ClassFileBuffer pos + count` unchecked add (round-4 #7, round-5 #3)
**File:** `reader/src/buffer.rs:32, 46, 58, 74, 94, 102`

Six call sites do `pos + count`; on 32-bit `usize` this wraps before `slice::get` rejects.

**Fix:** Replace with `pos.checked_add(count).and_then(|end| self.data.get(pos..end)).ok_or(...)` via a small macro.

### [MED] 8. `decode_target_info` 0x40/0x41 re-serializes table_length (round-4 #5, round-5 #4)
**File:** `reader/src/attribute.rs:1258-1267`

Reads `table_length` then manually pushes 2 BE bytes plus `read_bytes(6 * table_length)`. Wasted work.

**Fix:** `let raw = buf.read_bytes(2 + 6 * table_length as usize)?; data.extend_from_slice(raw);` — single contiguous slice read.

### [MED] 9. `decode_attribute_body` linear string match (round-4 #8, round-5 #6)
**File:** `reader/src/attribute.rs:661`

`match name { "Code" => ... }` over ~25 arms hashes/strcmps each `&str`. `name` is an interned `Arc<str>` from the constant pool — eligible for pointer-eq dispatch.

**Fix:** Intern the ~25 canonical attribute names into a `OnceLock<AttributeNames>` of `Arc<str>`s; dispatch via `Arc::ptr_eq(&name, &NAMES.code)` chain (fall through to `match &*name` for unknowns).

### [MED] 10. `LineNumberTable` / `LocalVariableTable` per-entry read (round-4 #4, round-5 #5)
**File:** `reader/src/attribute.rs:690-699, 944-956`

Per-entry `read_u16()` x2/x5 instead of one `read_bytes(table_length * stride)` + bytemuck slice.

**Fix:** `let raw = buf.read_bytes(table_length as usize * 4)?;` then `raw.chunks_exact(4).map(|c| LineNumberEntry { start_pc: u16::from_be_bytes([c[0],c[1]]), line_number: u16::from_be_bytes([c[2],c[3]]) }).collect()`.

### [MED] 11. LinkResolver lacks `(class, name, descriptor)` reflective cache (round-5 carry-over)
**File:** `classloading/src/resolution.rs` (cache keyed only on `(referring_class, cp_index)`)

Reflective `Class.getMethod`/`getDeclaredMethod` lookups (which don't go through CP indices) re-walk method tables on each call.

**Fix:** Add a second cache `FxHashMap<(ClassId, Arc<str>, Arc<str>), ResolvedMethod>` populated on first reflective resolve; invalidate via the same `invalidate_for_class(class_id)` cascade.

---

## (C) New angles

### [MED] 12. Memory-mapped JAR reading instead of full slurp
**File:** `classloading/src/class_path.rs:544, 924, 965, 1073, 1390, 1522, 1732`

Every JAR/class file is read via `fs::read` (full into-memory). On Spring-Boot fat-JARs (~80 MB) this is ~80 MB of RSS that could be page-cache-backed via `memmap2::Mmap`. The `zip` crate accepts `&[u8]`, so an `Mmap` deref is drop-in.

**Fix:** Wrap JAR loads in `memmap2::Mmap::map(&File::open(path)?)?` behind a feature flag; keep `fs::read` fallback for `.class` files on disk where the file is smaller than 4 KB (mmap overhead exceeds slurp cost).

### [LOW] 13. ClassFile parse is already parallelizable but not parallelized
**File:** `reader/src/class_reader.rs:51` (`pub fn read_class(data: &[u8])`)

`read_class` is a pure function. Cold-start parses ~6k JDK classes serially. With `rayon`, bootstrap parse becomes ~1.5s → ~250ms on a 16-core host.

**Fix:** Add `pub fn read_classes_par(inputs: &[(Name, &[u8])]) -> Vec<Result<ClassFile>>` that wraps `inputs.par_iter().map(...).collect()`. Caller in `class_manager` slurps the JAR (sequentially), then parses in parallel.

### [LOW] 14. JFR `StringPool` vs `cratonvm_types::intern_arc` are intentionally separate — no unification needed
**File:** `jfr/src/dump.rs:185` vs `types/src/intern.rs:51`

JFR `StringPool` produces wire-format CP indices for the JFR binary file format (u16 ID per event); `cratonvm_types::intern_arc` produces process-lifetime `Arc<str>` for runtime sharing. Different lifetimes, different keying. **Verified: no unification opportunity.** Flagged for closure.
