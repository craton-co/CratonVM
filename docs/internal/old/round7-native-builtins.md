# Round 7 — `native-builtins/` Audit

Scope: round-6 wave-1 fixes plus carry-over from round-5 and new angles.

---

## 1. [CRIT] `TL_MAP` key is a raw pointer — invalidated by moving GC

- **File**: `native-builtins/src/phases_early.rs:2197-2205` (`tl_key`, `TL_MAP`).
- **Issue**: `tl_key(this) = this.as_ptr() as usize`. CratonVM has a moving GC (compact-header forwarding at `gc/src/compact_header.rs:209`; G1/gen_heap also move). After a GC compaction the original `ThreadLocal` is relocated; subsequent `get`/`set` from any thread compute a *new* `tl_key` against the same Java identity but find no entry, returning null. Worse, the freed address is recycled — a brand-new `ThreadLocal` allocated at the old slot **inherits the dead entry's value** across threads. This silently corrupts every `ThreadLocal`-keyed cache (logging MDC, Jackson `ObjectMapper`, Tomcat thread context).
- **Fix**: Key by `ctx.identity_hash_code(this) as u32` (lazy-installed in the compact header at `gc/src/compact_header.rs:393`, pinned across moves). Treat `i32` collisions by chaining `(idhash, slot0_marker)` — slot 0 is already reserved (`TL_FIELD_VALUE`) and stores a per-instance disambiguator written in `<init>`. Add a `weak_ref::on_object_freed` hook to drop stale entries.

## 2. [CRIT] `withInitial` never stores the supplier — other threads see null forever

- **File**: `native-builtins/src/phases_early.rs:2262-2279` (`native_tl_with_initial`).
- **Issue**: The comment claims "other threads will lazily re-invoke their own initial-value provider on first access," but the supplier reference is **never stored on the ThreadLocal object** and `native_tl_get` (line 2236) returns `Value::Object(None)` unconditionally on a miss. Every thread except the creator gets null from a `withInitial`-built TL — breaking `ThreadLocal.withInitial(() -> new SimpleDateFormat(...))`, the canonical use case.
- **Fix**: Stash the supplier in slot 0 during `with_initial`, and in `native_tl_get`, on a map miss read slot 0 — if it is a Supplier, invoke `get()` on it, insert into TL_MAP, return. Update the slot-0 docstring at line 2230 accordingly.

## 3. [CRIT] `InheritableThreadLocal` does not inherit from parent thread

- **File**: `native-builtins/src/phases_early.rs:2219-2224`.
- **Issue**: ITL reuses `native_tl_get` against a per-thread map keyed only by the TL pointer. The JDK contract is that when a child thread starts, **every ITL value live in the parent's map is copied via `childValue(parentValue)` into the child's map.** No copy happens — `MDCAdapter` (SLF4J) and Netty's `FastThreadLocal` parent-inheritance break.
- **Fix**: On thread creation (`threading/jvm_thread.rs` thread-spawn), snapshot the parent thread's TL_MAP for keys whose ThreadLocal class is `InheritableThreadLocal` (identifiable via a side-table tag installed in the ITL `<init>`) and seed the child's TL_MAP before `run()`.

## 4. [HIGH] `TL_MAP` grows without bound — leaks every dead `ThreadLocal`

- **File**: `native-builtins/src/phases_early.rs:2197-2199`.
- **Issue**: Once a thread `set`s a value for a ThreadLocal, the entry stays in `TL_MAP` until the thread dies even if the ThreadLocal itself is GC-unreachable. Long-lived worker pools (Tomcat, Netty) leak.
- **Fix**: Register a weak-key GC callback (`gc/src/reference.rs` `on_unreachable`) that walks each thread's TL_MAP and removes entries whose ThreadLocal pointer is no longer live. Mirrors `j.l.ThreadLocal$ThreadLocalMap.expungeStaleEntry`.

## 5. [HIGH] `varhandle_get` re-reads `kind`/`field_index`/`type_desc` 2-3× per call

- **File**: `native-builtins/src/lang_invoke.rs:795-867`.
- **Issue**: `vh_meta_get(this)` returns `Arc<VarHandleMeta>` (line 168) but the function uses it only for `(kind, field_index)`. `vh_type_desc(ctx, this)` (line 811, 830, 850) re-reads `VH_FIELD_DESC` from the heap each time, and the name-resolution branch (line 817) does **another** `vh_meta_get`. Hot VarHandle dispatch costs 3 trait calls + heap reads instead of one Arc clone.
- **Fix**: Single `let meta = vh_meta_get(this)?;` up front; capture `type_desc` into the `VarHandleMeta` struct (already cached on construction). Drop `vh_type_desc(ctx, this)` calls inside the kind branches.

## 6. [HIGH] `StackFrame.getMethodType()` returns null — `LambdaMetafactory` paths NPE

- **File**: `native-builtins/src/phases_late.rs:14061-14066`.
- **Issue**: Round-5 carry-over (finding 10). Returning `Value::Object(None)` is fine for "quiet bootstrap probes" but real callers like `LambdaMetafactory.altMetafactory` and `MethodHandleInfo.reveal` chain through `.getMethodType().parameterCount()` and NPE.
- **Fix**: Read the descriptor stored in the synthetic StackFrame slot 4 (set in `fetchStackFrames` lowering) and call `parse_method_type_from_descriptor(ctx, desc)` — the existing helper in `lang_invoke.rs` that constructs a real `MethodType` from `(...)R` strings.

## 7. [MED] `Field.getBoolean` accepts any non-zero `Int` — masks data corruption

- **File**: `native-builtins/src/lang_class.rs:3293-3304`.
- **Issue**: Round-5 carry-over (finding 11). Descriptor-check ensures `Z`, but if a prior buggy `setInt(field, 42)` raced past the descriptor guard (e.g. via `Unsafe.putInt` on the boolean slot), `getBoolean` silently returns `true` instead of surfacing the corruption. JDK throws `IllegalArgumentException` if the field was tampered with.
- **Fix**: After the descriptor check, assert `v == 0 || v == 1`; raise IAE with `"Field.getBoolean: corrupted boolean field, value={v}"` otherwise. One branch, zero hot-path cost on legal payloads.

## 8. [MED] `ObjectInput.readObject` blanket UOE breaks RMI/JNDI bootstrap

- **File**: `native-builtins/src/serialization.rs:16-20` (`serialization_not_supported`), registered for `java/io/ObjectInput.readObject` and friends.
- **Issue**: Round-5 carry-over (finding 12). JNDI initial context (`com.sun.jndi.rmi.registry.RegistryContext`) and Quarkus' `RemoteCache` probe `readObject` on startup and treat UOE as a hard failure rather than `OptionalDataException`. Bootstrap halts.
- **Fix**: Return `Value::Object(None)` (i.e. EOF-equivalent null) for `ObjectInput.readObject` when no buffer is registered for the stream pointer; throw `EOFException` only when a buffer exists and is drained. UOE only stays for `writeUnshared`/`readUnshared` where a real implementation is not feasible.

## 9. [MED] `DateTimeFormatter.ofPattern(p)` re-allocates per call — no formatter cache

- **File**: `native-builtins/src/util_time.rs:2041-2047` (`alloc_dtf`), `2483-2491` (registration).
- **Issue**: Hot logging paths (`Instant.toString` via formatter, JSON serializers) call `ofPattern("yyyy-MM-dd'T'HH:mm:ss")` per record. Each call allocates a new synthetic and tags the GC.
- **Fix**: Add a process-wide `OnceLock<DashMap<String, ObjectRef>>` keyed by pattern; on first call allocate + insert, otherwise return cached. The formatter is immutable in the JDK contract, so sharing is safe. ~50ns/call vs 2µs alloc.

## 10. [MED] No small-value cache for `Integer.valueOf` — `TL_MAP` pattern unblocks it

- **File**: `native-builtins/src/lang_math.rs:2266-2274`.
- **Issue**: Round-5 carry-over (Integer/Boolean/Character cache deferred for `!Send` blocker). Round-6 proved the `thread_local!` pattern works for `ObjectRef`. JDK boxes a billion small ints during bootstrap.
- **Fix**: Mirror the round-6 ThreadLocal recipe:
  ```rust
  thread_local! { static INT_CACHE: RefCell<[Option<ObjectRef>; 256]> = const { RefCell::new([None; 256]) }; }
  ```
  In `native_integer_value_of`, range-check `-128..=127`, lookup, alloc-and-fill on miss. 2-slot `BOOL_CACHE` for `Boolean.valueOf`. Cache survives GC because identity is per-thread and the `ObjectRef`s are GC roots (held in the cache).

## 11. [MED] `Method.invoke` has no descriptor-match fast path

- **File**: `native-builtins/src/lang_class.rs:3969-4083`.
- **Issue**: Every `Method.invoke` walks `coerce_arg_strict` (lang_class.rs:2426) per argument even when the supplied `Object[]` already holds primitives that exactly match the parameter descriptors. ByteBuddy / Jackson hit this thousands of times during boot.
- **Fix**: Before the per-arg coerce loop (line 4075), iterate once and check `primitive_tag_of(arg) == expected_desc.chars().next()`; if every slot matches, skip into a `push` loop with no coercion. Falls through to the slow path on any mismatch.

## 12. [LOW] LongAdder CAS-loop spins on every `add` — no striped cells

- **File**: `native-builtins/src/lib.rs:28998-29023` (and `_increment`/`_decrement` siblings).
- **Issue**: Round-6 fixed correctness via single-slot CAS, but under heavy contention this defeats `LongAdder`'s entire purpose (striped cells). Single-cell CAS still livelocks under 32-thread benchmark load — same as `AtomicLong`.
- **Fix**: Allocate a `NUM_CPUS`-sized cell array in `<init>` (slot 1 = `Long[] cells`); in `add`, hash `Thread.currentThread().threadId()` to a cell index, CAS that cell; `sum()` reduces across cells. Mirrors `j.u.c.atomic.Striped64`.

---

**Audit confidence**: round-6 CAS, soft-ref-touch, bulk-array-copy, unsafe IAE-on-get, and `atomic_fetch_add_*` defaults are correct. The ThreadLocal fix is structurally sound (per-thread isolation works) but the three CRIT bugs above defeat it for the most common use patterns.
