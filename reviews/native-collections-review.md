# native-collections review

Crate: `C:\Projects\CratonVM\native-collections` (cratonvm-native-collections v0.3.0).
Single source file `src/lib.rs` (23 609 lines) + 5 integration test files (855 lines).

## Summary
- **HIGH (GC-correctness):** LinkedList / LinkedHashMap / TreeMap / TreeSet overlay side-tables are still keyed by `this.as_ptr() as usize` (`src/lib.rs:10368,10377,11368,13954,14041,14070,14073,14085`). Under any moving GC the stored state is lost on relocation, and two distinct objects can alias the same address. The in-tree GC-relocation test `tests/gc_relocation_harness.rs` fails today with `size_post=0` — claims to lock in a fix ("C21") that has not landed in production code.
- **HIGH (test/feature gating):** 5 in-`lib.rs` unit tests (`array_blocking_queue_registered`, `blocking_queue_interface_registered`, `concurrent_linked_{queue,deque}_registered`, `linked_blocking_queue_registered`, `src/lib.rs:22903-22954,23079-23087`) call `register_collections_natives` and assert BQ classes are present, but BQ registration is `#[cfg(feature = "synthetic-jdk")]` (`src/lib.rs:152`). `cargo test` under default features fails 5 tests; tests pass only with `--features synthetic-jdk`.
- **HIGH (correctness / soundness):** `ChmMonitorGuard` `mem::transmute`s a `*mut dyn NativeContext` to a `'static`-lifetime fat pointer (`src/lib.rs:348-355`) — sound only because of the `PhantomData<fn() -> &'a mut …>` invariance discipline at the type level; one careless container store will UB without a compile error. Mock test harness does not exercise this path.
- **MED (concurrency):** `native_lbq_put_blocking` spins 10 000 times then falls through to `native_lbq_offer` unconditionally (`src/lib.rs:20003-20006`), bypassing the capacity check; `native_lbq_poll` is unsynchronised (`src/lib.rs:20034-20058`); CHM bulk collectors (`chm_collect_all_*`) walk segments without per-segment locks (`src/lib.rs:16441-16499`).
- **MED (OSS readiness):** `src/lib.rs` lacks the workspace SPDX/copyright header (tests have it); a corrupted single-line `pub fn make_iterator_from_array` accidentally embedded in a `///` doc-comment at `src/lib.rs:171` (the real definition is at line 94); 1 dead `register_scheduled_executor_natives` (`src/lib.rs:20602`) never called from `register_collections_natives`.

## 1. Code review

### Bugs (HIGH)

- **GC-relocation state loss in overlay side-tables.** All four overlays are still pointer-keyed:
  - LinkedList: `ll_get/ll_set` use `this.as_ptr() as usize` (`src/lib.rs:10368, 10377`).
  - LinkedHashMap: `lhm_overlay_key` (`src/lib.rs:11367-11369`).
  - TreeMap: `tm_obj_key` (`src/lib.rs:13953-13955`) backing `tm_array_table`, `tm_fast_table`, `tm_force_array_set`.
  - TreeSet: `ts_array_table` (`src/lib.rs:14183-14201`).
  
  The accompanying doc comment at `tm_obj_key` and the entire `tests/gc_relocation_harness.rs` test claim the fix re-keys these on `ctx.identity_hash_code(this)` ("C21 contract"). It has not. The test FAILS today (`cargo test --test gc_relocation_harness` → `size_post=0, expected 3`). The only spot that does it correctly is CHM segment striping (`src/lib.rs:1960, 16920`).

- **Doc-comment swallowed function definition** at `src/lib.rs:171`: a `///` line contains a whole second `pub fn make_iterator_from_array` body inline, including the function signature. The real definition is at `src/lib.rs:94-104`. Because `///` is a line doc-comment, the second function is silently discarded — but the file still compiles and the duplicate-looking declaration on a single physical line is hostile to any reader/IDE. Strip the comment.

- **Tests broken under default cargo features.** `src/lib.rs:22903-22954, 23079-23087` panic in default `cargo test` because `register_blocking_queue_natives` is `#[cfg(feature = "synthetic-jdk")]` gated (`src/lib.rs:152`). Either gate the tests with the same `cfg`, or unconditionally call the registration in tests.

- **`ChmMonitorGuard` lifetime transmute** (`src/lib.rs:324-386`). `unsafe { mem::transmute }` from `*mut (dyn NativeContext)` to `*mut (dyn NativeContext + 'static)` is sound only because callers never escape the guard into a longer-lived owner; the `PhantomData<fn() -> &'a mut dyn NativeContext>` marker enforces this *at the type-system level* for direct stores into structs but does NOT prevent a future contributor from `mem::forget`ing the guard or extending its scope with a closure capture. The Drop also `catch_unwind`s a second panic — sensible but masks bugs.

### Bugs (MED)

- **Blocking-queue races.** `native_lbq_put_blocking` (`src/lib.rs:19981-20007`): the size/capacity check is taken under monitor, but if the loop times out after 10 000 spins the function calls `native_lbq_offer` regardless of capacity — overflowing a full bounded queue. Real `BlockingQueue.put` MUST block forever or throw on interrupt; falling through past capacity violates the JDK contract.
- **`native_lbq_poll`** (`src/lib.rs:20034-20058`) takes no monitor; concurrent `offer`+`poll` causes the size read at line 20039 to race against the shift at line 20053. Same for `native_lbq_poll_last` and `peek`.
- **`native_lbq_init_cap`** (`src/lib.rs:19897-19912`) silently rounds non-positive `cap` to 1 instead of throwing `IllegalArgumentException` as real `ArrayBlockingQueue(int)` does.
- **CHM bulk collectors are not snapshot-isolated.** `chm_collect_all_entries`, `chm_collect_all_keys`, `chm_collect_all_values` (`src/lib.rs:16441-16499`) walk each segment chain without acquiring the per-segment Java monitor or the striped RwLock. A concurrent writer can splice a node mid-walk; the collector either skips it or sees torn key/value. The walking sites for `forEach`, `forEachKey`, `forEachValue`, `search`, `forEach_parallel` all chain through these collectors.
- **`map_resize` chain-splice racing with lock-free reads** (`src/lib.rs:1991-2150`). The doubling-split path mutates old-chain `NEXT` pointers in place; lock-free CHM readers (`chm_seg_get`) serialise behind the striped read-lock, but plain-HashMap callers do not. Plain HashMap is documented not thread-safe, so this is acceptable, but the in-place rewrite path also has a cycle-detection branch (`src/lib.rs:2024-2086`) that allocates a `HashSet<*const ()>` on every cycle-tripped bucket — a quadratic allocation pattern if a malicious construction provides many hash-collided keys (cf. hash-flooding).
- **Hash flooding mitigation.** `map_hash_key` (`src/lib.rs:1663-1723`) reproduces `String.hashCode` (UTF-16) and `HashMap.hash` (`h ^ h>>>16`) faithfully. There is no per-process seed — same as the JDK, but worth noting if any consumer expects DoS-resistance.
- **Integer overflow in load-factor sizing.** `native_chm_init_full` (`src/lib.rs:16857-16859`) computes `(total_cap as f64 / load_factor as f64).ceil() as usize`; if `total_cap` is `i32::MAX` and load_factor is tiny, the cast can produce `usize::MAX` and the subsequent `.next_power_of_two()` panics in debug.
- **`lhm_get` / `lhm_set` global Mutex on every access** (`src/lib.rs:11371, 11378, 11391`). Every `LinkedHashMap.get` / `put` / `size` / `iter` step takes a process-global `Mutex<HashMap<usize, HashMap<String, Value>>>`. With many LHMs in use across threads, this is a single chokepoint.

### Vulnerabilities

- No `unsafe` outside the `ChmMonitorGuard` transmute and its matching `Drop`. No FFI in this crate.
- No I/O or networking — the crate is pure compute on the heap.
- Resource exhaustion: `AL_MAX_CAPACITY = 1 << 30` (`src/lib.rs:654`), `MAP_MAX_CAPACITY = 1 << 30` (`src/lib.rs:1894`), CHM segment cap clamped to 256 (`src/lib.rs:16856`). Acceptable.

### Stubs / dead code

- `register_scheduled_executor_natives` at `src/lib.rs:20602` is defined but never called from `register_collections_natives` (which calls `register_executors_scheduled_natives` at `src/lib.rs:21801` instead). 270-line dead function.
- The `make_iterator_from_array` duplicate at `src/lib.rs:171` (comment-swallowed).
- `_CHM_NUM_FIELDS` (`src/lib.rs:16389`) is dead.
- 18 TODO/FIXME/XXX markers across the file, including 2 TODOs to "replace the RwLock with a clone-resize" (`src/lib.rs:238, 1953`).

### Performance

- File is one 23 609-line `lib.rs`. Refactoring into modules (one per `register_*_natives`) is the single biggest readability/compile-time win available.
- `obj_to_display_string` (`src/lib.rs:393-451`) calls `invoke_virtual(toString)` for every non-primitive element of `toString()` operations — for ArrayList/HashMap/CHM `toString()`, this allocates a Java string per element via JNI-shaped callback.
- `el_set`/`al_set` / `al_set_data` recompute slot indices on every call by calling `al_slots(ctx)` (`src/lib.rs:538-549`) which goes through `resolve_field_index` twice per write. The `#[inline]` annotation is present but the inner `resolve_field_index` walks a trait method — opaque to LTO unless devirtualised.
- LHM/LinkedList/TreeMap overlay path locks a global `Mutex` on every field access (see MED above).
- `native_lbq_offer`'s `lbq_ensure_capacity` does a 2× grow with an O(N) element-copy loop (`src/lib.rs:19932-19937`) — no `bulk_array_copy` like ArrayList uses.
- `map_resize` cycle-tripped fallback allocates a fresh `std::collections::HashSet<*const ()>` per bucket (`src/lib.rs:2061`) — should use a thread-local scratch.

## 2. Tests

**Counts**: 77 `#[test]` total — 55 in `src/lib.rs` (mostly registration completeness), 22 in `tests/`. Under default features 50 pass / 5 fail; under `--features synthetic-jdk` 55 pass / 0 fail in the lib; the GC-relocation integration test still fails post-rebuild.

**Coverage estimate**: ~25 % of the 23 609-line crate is exercised by behavioural tests. The integration tests touch ArrayList (6 tests), HashMap (9), access-order LHM (3), TreeMap (2), and GC harness (2). Registration-completeness tests touch every `register_*` function but do not actually exercise the natives.

**Gaps**:
- ConcurrentHashMap (`register_concurrent_hashmap_natives`, ~1 160 lines): zero behavioural coverage. No segment-routing, lock-free-read, computeIfAbsent, replace_kv tests.
- LinkedList, LinkedHashMap (non-access-order), Stack, Vector, PriorityQueue, ArrayDeque, all `BlockingQueue`s, ConcurrentSkipListMap, StampedLock, Phaser, PriorityBlockingQueue, Properties — registration-only.
- Stream / Collectors / Optional / IntStream / LongStream / DoubleStream — ~3 000 lines, registration-only.
- No fuzz target despite a workspace `fuzz/` crate (and the file contains hash-spread, bucket-index, UTF-16 hash, binary-search insertion paths that would benefit from proptest).
- No large-N tests beyond the 100-key HashMap path; the cycle-trip / split-resize fallback in `map_resize` (`src/lib.rs:2018-2086`) is not covered.
- No concurrency tests at all. The CHM striped-lock and `ChmResizeLockGuard` reasoning is untested under contention.
- No fail-fast iterator (CME) tests — the implementation does not enforce this, which is itself a behavioural gap worth pinning with a test.

**Brittle/known-broken**:
- `tests/gc_relocation_harness.rs::lhm_state_survives_simulated_gc_relocation` **fails today** — the test is correct, the production code is missing the C21 fix.
- 5 lib tests fail under default features (BQ registration cfg gate).

**Concrete additions** (priority order):
1. Rekey LHM/LinkedList/TreeMap/TreeSet overlays on `identity_hash_code` and make the GC test green.
2. CHM behavioural tests: put/get round-trip, computeIfAbsent, replace_kv, null-rejection, concurrent put + get via thread spawn, segment-distribution sanity.
3. Proptest: random key sets vs an oracle `std::collections::HashMap` for HashMap/CHM; random TreeKey vs `BTreeMap`.
4. Iterator weak-consistency test for CHM (mutate-during-iterate must not panic or torn-read).
5. Cycle-trip fallback test: construct a self-referencing bucket chain and confirm resize completes.
6. `synthetic-jdk` cfg gate on the 5 currently-broken BQ tests.

## 3. Documentation

**Existing**:
- Crate-level rustdoc header (`src/lib.rs:1-10`) marks the crate "DEPRECATED (Session 15)" — yet the workspace still uses it. The deprecation note conflicts with the README "Scope" section.
- README (`native-collections/README.md`, 44 lines): scope/non-goals/usage/status/license — workspace-consistent, no link rot.
- Inline doc comments are dense and refer to "S111r-bug-fix (peaceful-sammet)", "Round-10 CRIT fix", "Bug 1+2 CRIT round-10", "C21", "C24", "C25" — internal session labels that are not navigable from outside.
- `pub` functions (`make_iterator_from_array`, `native_al_init`/`size`/…, `clone_lhm_overlay`, `register_collections_natives`) all have doc comments, though `native_al_*` items have minimal rationale.

**Missing**:
- No module-level doc on layout invariants (NODE_FIELD_*, AL_FIELD_*, MAP_FIELD_*). The mirror "JDK layout vs synthetic layout" decision is repeated in ~20 inline comments — should be one section.
- `ChmMonitorGuard` soundness preconditions (no escape, no `mem::forget`) are buried — should be a `# Safety` section.
- No documentation of the side-table architecture (LL/LHM/TM overlays) — the most critical correctness contract in the crate is described only in inline comments.
- The "deprecated" crate-level note is itself out of date or misleading — clarify whether the crate is shipping or removable.

## 4. OSS readiness

- **Cargo.toml** (`Cargo.toml:1-32`): inherits version/edition/license/repository/keywords/categories from workspace. `description = "Java collections native methods for CratonVM"` is fine. `readme = "README.md"` present. `publish` defaults to workspace `publish = false` so this would NOT publish without an override. Good.
- **SPDX headers**: ALL six test files have `// SPDX-License-Identifier: Apache-2.0 // Copyright 2024-2026 Craton Software Company.`. **`src/lib.rs` does not.** The crate-level doc comment starts immediately at line 1. Add the standard 2-line header for consistency.
- **NOTICE**: not in this crate; the README directs to workspace root, which is the standard convention.
- **Lints**: `[lints] workspace = true`; workspace allows `dead_code`, `unused_imports`, etc., and a string of rustdoc lints — pragmatic for this synthetic-natives codebase but masks real dead code (e.g. `register_scheduled_executor_natives`).
- **Blockers for OSS publishing**:
  1. Failing tests under default cargo features (5 BQ tests).
  2. Failing integration test under any feature set (GC-relocation harness).
  3. Misleading rustdoc claiming a "C21" fix that is not present.
  4. Internal session labels ("S111r28", "peaceful-sammet", "round-10 CRIT") leak into public docs.
  5. Missing SPDX header on `src/lib.rs`.
  6. 23 609-line single file is not idiomatic Rust and will deter contributors.

Verdict: **NOT ready** for OSS publication today; passes-tests blocker plus the missing GC fix are correctness issues, not cosmetic.

## Top 5 fix priorities

1. **Rekey LHM / LinkedList / TreeMap / TreeSet overlay side-tables on `ctx.identity_hash_code(this)`** (`src/lib.rs:10359-10380, 11362-11396, 13945-14107, 14180-14210`). This is the test-breaking bug, the moving-GC correctness bug, and the "claim vs reality" doc bug — all one change.
2. **Gate the 5 BQ unit tests with `#[cfg(feature = "synthetic-jdk")]`** (`src/lib.rs:22903-22954, 23079-23087`) so `cargo test` is green by default. Or, alternatively, drop the feature gate on `register_blocking_queue_natives` (it is unconditional registration of natives, harmless under real-JDK builds).
3. **Fix `native_lbq_put_blocking` to NOT fall through after spin-exhaustion** (`src/lib.rs:19981-20007`) and add monitor synchronisation to `native_lbq_poll/peek/poll_last` (`src/lib.rs:20034-20100`).
4. **Strip the comment-swallowed `make_iterator_from_array` line** (`src/lib.rs:171`) and delete the dead `register_scheduled_executor_natives` (`src/lib.rs:20602`); add SPDX header to `src/lib.rs`.
5. **Split `lib.rs` into modules** — one per `register_*_natives` family — and document the layout-invariant story (NODE_FIELD_*, AL_FIELD_*, MAP_FIELD_*, LHM_NODE_*, TM_FIELD_*) in a single `layouts.md` or top-of-file section. This unlocks subsequent maintainability and review work.
