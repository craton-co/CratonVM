# Round 5 — `native-builtins` follow-up review

Scope: audit of round-4 fixes (A), residual round-4 items (B), stubs / placeholders (C), and new angles around hot collections / atomics (D). Severity tags: CRIT (correctness), HIGH (perf/sem), MED, LOW.

---

## 1. [CRIT] `ThreadLocal.get()/set()` store on the ThreadLocal instance field — shared across threads

- **File**: `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-builtins\src\phases_early.rs:2204-2227`.
- **Issue**: `native_tl_get` / `native_tl_set` / `native_tl_remove` read and write `TL_FIELD_VALUE` (slot 0) on the `ThreadLocal` object itself, not on a per-thread map. Every thread that touches the same `ThreadLocal` sees the last writer's value. `InheritableThreadLocal` shares the same broken pattern at `:2196-2201`.
- **Fix**: Back the storage with a `thread_local! static TL_MAP: RefCell<FxHashMap<usize, Value>>` keyed by `this.as_ptr() as usize`. `get`/`set`/`remove` operate on the current thread's map. Existing `withInitial` (`:2229`) populates the per-thread slot on first read.

## 2. [CRIT] `LongAdder.add / increment / decrement` are non-atomic

- **File**: `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-builtins\src\lib.rs:29052-29093`.
- **Issue**: `get_field` + `set_field` with no CAS — a concurrent `add` from two threads loses one update. `LongAdder` is the entire point of avoiding `AtomicLong` contention; getting it wrong silently corrupts counters in async frameworks (Netty, Hazelcast, Cassandra metrics).
- **Fix**: Mirror the `native_atomic_int_get_and_add` CAS-loop pattern (`lib.rs:14074-14088`) — `loop { current = get_field_volatile; if compare_and_swap_field(this, 0, current, Long(current + x)) { break } }`. Same change for `DoubleAdder.add` at `:29010-29014`.

## 3. [HIGH] `vh_meta_get` still acquires the Mutex 2-3 times per VarHandle op

- **File**: `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-builtins\src\lang_invoke.rs:795-851` (`varhandle_get`), `:886-942` (`_set`), and `vh_type_desc` → `vh_field_desc` at `:78-93`.
- **Issue**: Round 4 wrapped `VarHandleMeta` in `Arc` (good — no per-field `String` clone), but each `varhandle_get` still calls `vh_meta_get(this)` once for kind/index, then again for class/field, then `vh_type_desc` calls `vh_field_desc` which calls `vh_meta_get` a third time. Three `Mutex::lock()` round-trips per CAS.
- **Fix**: Hoist `let meta = vh_meta_get(this);` at the top of `varhandle_get` / `_set` / `_compare_and_set`, then pass `Option<&VarHandleMeta>` (or the `Arc`) to `vh_type_desc` / `vh_field_desc` so they reuse it. Better: switch `VH_META_TABLE` to `parking_lot::RwLock<FxHashMap>` so concurrent reads don't serialise.

## 4. [HIGH] Small-int wrapper cache (Integer/Boolean/Byte) still not implemented

- **File**: `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-builtins\src\lang_math.rs:2266-2274` (`native_integer_value_of`), `:2563-2580` (Boolean/Character).
- **Issue**: Round-4 added the CID cache but `Integer.valueOf(-128..=127)` still allocates a fresh wrapper per call (the spec mandates cache reuse). `Boolean.TRUE`/`FALSE` likewise re-allocate. `ObjectRef` is `!Send` so the cache cannot live in a `static OnceLock`, but a `thread_local! static INT_CACHE: RefCell<[Option<ObjectRef>; 256]>` works (no cross-thread sharing needed — each thread amortises after 256 ints). Populate lazily.
- **Fix**: `thread_local! { static INT_CACHE: RefCell<[Option<ObjectRef>; 256]> = RefCell::new([None; 256]); }`. In `native_integer_value_of`, if `val in -128..=127` look up `INT_CACHE.with(|c| c.borrow()[(val+128) as usize])`; on miss, allocate, cache, return. Same with a 2-slot Boolean cache. Acceptable trade: cache grows once per thread.

## 5. [HIGH] `Policy::implies_full` still calls `normalize_class(&perm.class_name)` per grant-permission

- **File**: `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-builtins\src\security_manager\policy.rs:239`, `:280`, `:356`, `:784-789`.
- **Issue**: Round-4 made `normalize_class` return `Cow<str>` (good — borrow when no `/` present), but the AllPermission check at `:239` and `class_matches` at `:280` still re-normalise the *grant's* `perm.class_name` on every probe. With N grants × M perms per checkPermission, that's still N·M comparisons even if alloc-free.
- **Fix**: Pre-compute at parse time: add `Permission::class_name_dotted: Arc<str>` and `Permission::is_all_permission: bool` alongside the raw `class_name`. The hot path becomes `if perm.is_all_permission { return true; }` and `class_matches(perm.class_name_dotted.as_ref(), norm_class_ref)`.

## 6. [HIGH] StringBuilder hot path uses per-element loops despite bulk intrinsic

- **File**: `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-builtins\src\lang_string.rs:967-1013` (`sb_ensure_capacity`, `sb_append_chars`), `:1109-1164` (char-array appenders), `:1027-1044` (init from string).
- **Issue**: `sb_ensure_capacity` copies the old buffer via a per-element `get_array_element` / `set_array_element` loop. `sb_append_chars` writes each char by `set_array_element`. `native_sb_append_char_array_off_len` materialises a `Vec<u16>` via per-element reads before writing — two N-cost trait round trips per append. `native_string_format` (`:2893`) materialises a full `Vec<char>` of the format string per call.
- **Fix**: Use `ctx.bulk_array_copy(old_buf, 0, new_buf, 0, count)` in `sb_ensure_capacity`. Add `ctx.write_char_array_from(buf, off, &chars)` intrinsic and wire `sb_append_chars` / `native_sb_init_string` through it. In `native_string_format` iterate `fmt_str.as_bytes()` (the format spec is ASCII-only) instead of collecting a Vec.

## 7. [HIGH] `Unsafe.getByte/Short/Int/Long(long)` silently returns 0 on out-of-arena address

- **File**: `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-builtins\src\unsafe_natives.rs:294-310` (get_byte), `:345-356` (short), `:383-394` (int), `:421-432` (long).
- **Issue**: Round-4 added the IAE throw on the *put* side. The matching *get* side still calls `unsafe_arena_get_byte(addr)` (which returns 0 when out of range — see `lib.rs:13570-13572`) and returns that 0 to the caller with no exception. A reader past arena bounds gets garbage 0s, then trips a downstream NPE far from the actual bug.
- **Fix**: Mirror the put-side pattern — make `unsafe_arena_get_*` return `Option<T>` (or a `(value, in_bounds)` tuple), and on `None`/`!in_bounds` return `Err(IllegalArgumentException)` with the offending address. The cache lookup at `:306, 353, 391, 429` already knows when it's a miss.

## 8. [MED] `lang_stackwalker::populate_sfi` redundantly calls `class_id_by_name` twice

- **File**: `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-builtins\src\lang_stackwalker.rs:78` and `:109-112`.
- **Issue**: Line 78 calls `ctx.class_id_by_name(&entry.class_name)` to fetch the cached dotted name; line 109 calls the same lookup again to build the class mirror. With a 30-deep stack and 3 distinct classes, that's 60 hash lookups instead of 30.
- **Fix**: One-liner — hoist `let cid_opt = ctx.class_id_by_name(&entry.class_name);` above the dotted-name match, then reuse `cid_opt` in both the `match` (line 78) and the mirror lookup (line 109).

## 9. [MED] `AtomicInteger.getAndIncrement / addAndGet` use CAS-loops where a direct intrinsic exists

- **File**: `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-builtins\src\lib.rs:14038-14110`, `:14150-14210` (AtomicLong).
- **Issue**: Each `getAndIncrement` makes two virtual trait calls (`get_field_volatile` + `compare_and_swap_field`) per loop iteration; under contention the loop spins multiple times. The VM already has `Unsafe.getAndAddInt` intrinsic (called via `native_unsafe_get_and_add_int`) which on x86 becomes a single `LOCK XADD`. AtomicInteger natives could call straight into that intrinsic — one trait dispatch, no CAS spin.
- **Fix**: Add `ctx.atomic_fetch_add_int(this, slot, delta) -> i32` to `NativeContext` (VM impl: `LOCK XADD` on the field storage). Replace the CAS loops in `getAndIncrement` / `getAndAdd` / `incrementAndGet` / `decrementAndGet` with one intrinsic call. Same for `AtomicLong` / `AtomicReference` (the latter needs `atomic_compare_exchange_ref`).

## 10. [MED] `StackFrame.getMethodType()` returns null instead of constructing MethodType

- **File**: `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-builtins\src\phases_late.rs:14061-14066`.
- **Issue**: Returning `null` for a non-void return type violates `MethodType` non-null contract. Callers that chain `.parameterCount()` / `.returnType()` NPE in unrelated code. The frame already has the descriptor (StackTraceEntry).
- **Fix**: Reconstruct via the existing `descriptor_to_method_type` helper (`lang_invoke.rs`) — given `entry.descriptor`, build the `MethodType` synthetic and return it. If `descriptor` is empty (debug-info-free frame), still return a zero-arg `()V` MethodType rather than null.

## 11. [MED] `Field.getBoolean` accepts any `Value::Int` regardless of declared type

- **File**: `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-builtins\src\lang_class.rs:3280-3305`.
- **Issue**: The descriptor check ensures the field is declared `Z`, but the value is unboxed to `i32`. After a buggy native or reflective `setInt` on the same field, `getBoolean` returns 1 for any non-zero — the field's truth value is corrupted with no diagnostic.
- **Fix**: After the descriptor check, also assert `v == 0 || v == 1`; otherwise throw IAE "field type is boolean but storage holds non-boolean int N". Same hardening for `getByte`/`getShort`/`getChar` (truncation-without-warning today).

## 12. [LOW] `ObjectInput.readObject` blanket UOE blocks legacy RMI / JNDI fallback paths

- **File**: `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-builtins\src\serialization.rs:1982-1990`.
- **Issue**: Interface-level `readObject` throws UOE even when the concrete `ObjectInputStream` subclass has its own implementation; JDK dispatch normally hits the subclass override before this stub. The stub is registered on the *interface* — any virtual dispatch that reaches the interface entry without a subclass override hits this hard fail.
- **Fix**: Replace UOE with `Ok(Some(Value::Object(None)))` and a `tracing::warn_once!` — null is the legitimate "no more objects" sentinel that callers already handle, and avoids hard-failing partial-serialization flows that don't actually need deserialization.

---

Word count: ~590.
