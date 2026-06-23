# Round 4 — `native-builtins` performance/bug review

Scope: `native-builtins/` crate, with focus on `lang_*`, `util_*`, `security_manager*`, reflection (`lang_class`, `lang_reflect`, `lang_misc`), and adjacent hot-path natives. Findings are listed in roughly descending impact. Each citation uses absolute Windows paths.

---

## 1. [HIGH] `System.arraycopy` ignores the existing `bulk_array_copy` intrinsic

- **File**: `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-builtins\src\lang_system.rs:235-256` (also lines 196-207, 242-254).
- **Issue**: The marked `TODO(audit-2026-05-16)` says "replace this per-element loop with a bulk intrinsic when `NativeContext` exposes one." The intrinsic *does* exist now — `bulk_array_copy(src, src_off, dst, dst_off, len)` was added in `native-api\src\registry.rs:270`. The primitive fast path (and the ref-array forward arm after pre-check passes) still does one virtual `get_array_element`/`set_array_element` round-trip per element.
- **Impact**: `System.arraycopy` is one of the very hottest natives — every `String.concat`, `Arrays.copyOf`, `StringBuilder.append([C)`, `ByteArrayOutputStream.write`, `ArrayList.add/grow` lands here. Replacing N virtual-trait round trips with one `copy_nonoverlapping` is a literal 50–100× win on multi-KB primitive copies and the prerequisite for closing the gap on parsers and codec hot paths.
- **Fix**: After the existing element-type / overlap pre-check, call `ctx.bulk_array_copy(src, src_pos as usize, dest, dest_pos as usize, length as usize)`; fall back to the per-element loop only when `bulk_array_copy` returns `false`. The ref-array forward arm (lines 209–229) can call the bulk intrinsic for the prefix on success after each successful element-write batch — or just keep its current loop, since the per-element typecheck dominates anyway.

---

## 2. [HIGH] `current_privileged_cert_digests()` allocates `Vec<String>` per `checkPermission`

- **File**: `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-builtins\src\security_manager.rs:261-269` (and `:309-310`, used in `check_permission_impl`).
- **Issue**: Round 3 converted `PrivilegedFrame` to `Arc<str>` + `Arc<[Arc<str>]>` so push/pop is a refcount bump (rationale at `:200-209`). But the read side immediately rebuilds an owned `Vec<String>` with `iter().map(|a| a.to_string())` on every `checkPermission` — defeating the optimisation. `current_privileged_code_base()` (`:249-255`) has the same shape (`Arc<str>` → owned `String`).
- **Impact**: `checkPermission` is on every reflection / classloader / file / property access path; JDK boot alone runs it tens of thousands of times. We allocate one `Vec` + one `String` per cert + one `String` for the code base every call — typically 1–3 heap allocs per check.
- **Fix**: Change the two getter signatures (and their lone caller `check_permission_impl`) to hand back the `Arc` shapes directly: `pub fn current_privileged_code_base() -> Option<Arc<str>>` and `pub fn current_privileged_cert_digests() -> Arc<[Arc<str>]>` (or expose a `with_*` closure helper so the borrow lives entirely on the per-thread stack). Then change `policy_allows_full` to accept `&[Arc<str>]` / `&[impl AsRef<str>]`. The hot path becomes zero-alloc.

---

## 3. [HIGH] Per-call `std::env::var("CRATONVM_DBG_BB")` syscalls on reflection hot path

- **File**: `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-builtins\src\lang_class.rs:452, 1846, 1876, 6176, 7434, 7522`.
- **Issue**: `Class.getName`, `getSuperclass`, `getGenericSuperclass`, `getGenericInterfaces`, and other ByteBuddy-touched reflection natives all call `std::env::var("CRATONVM_DBG_BB").is_ok()` once per invocation. `std::env::var` is a syscall (`getenv`) under a global mutex inside std and allocates a `String` for the value (or a `VarError`). It runs on every reflection call regardless of whether the flag is set. Several sites additionally do `strict_name.clone()` unconditionally (e.g. `lang_class.rs:7430, 7518`) to support the debug `eprintln!` even when the flag is off.
- **Impact**: ByteBuddy / CGLIB-heavy apps call `getName` / `getSuperclass` millions of times; a `getenv` syscall + lock-contended std internals per call is measurable. The unconditional `.clone()` on `Arc<str>` is cheap but the `String` clone of the raw name on the `mirror_class_name(...)` arm is not.
- **Fix**: Lift the env probe to a `OnceLock<bool>` at module scope: `static DBG_BB: OnceLock<bool> = OnceLock::new(); fn dbg_bb() -> bool { *DBG_BB.get_or_init(|| std::env::var_os("CRATONVM_DBG_BB").is_some()) }`. Same pattern for `CRATONVM_IAE_TRACE`, `CRATONVM_BD_DEBUG`, `CRATONVM_DIAG_METHOD_INVOKE_NULL` in `Method.invoke` (`lang_class.rs:4081, 4100, 4119`) and `CRATONVM_DBG_DOPRIV` (`security_manager.rs:736`). Also hoist the `strict_name.clone()` to a `let this_name_for_dbg = if dbg_bb { ... } else { String::new() }` block so the off-path doesn't pay the clone.

---

## 4. [HIGH] `Unsafe.putByte/Short/Int/Long(long, T)` silently swallows out-of-arena failure

- **File**: `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-builtins\src\unsafe_natives.rs:312-435`.
- **Issue**: The arena layer documents the contract as: `unsafe_arena_put_byte` returns `false` when the write would leave the arena, and "Java natives map `false` to `IllegalArgumentException`" (see `lib.rs:13574-13578`). But every `native_unsafe_put_*_at_address` does `let ok = ...; if ok || cache_hit { refresh_arena_cache(...); } Ok(None)` — the failure is dropped on the floor and the Java caller observes a write that didn't happen with no exception. Same shape for `put_short` (`:349-367`), `put_int` (`:383-401`), `put_long` (`:417-435`).
- **Impact**: Real bug. Native code that writes past an arena bound silently corrupts logic (the Java mirror believes it wrote a value, then reads back stale data). Buffer-allocator clients (java.nio.Bits, Direct ByteBuffer slicing, FFM `MemorySegment.set`) lose error visibility — bugs surface as later NPEs or wrong data instead of the IAE the writer expects at the offending call.
- **Fix**: When `!ok && !cache_hit`, return `Err(RuntimeError::IllegalArgumentException { message: format!("Unsafe.put*: address {addr:#x} not in any live arena") }.into())`. The doc comment on `unsafe_arena_put_byte` already promises this — the natives just need to honour it.

---

## 5. [HIGH] `vh_meta_get` clones full `VarHandleMeta` + takes Mutex on every VarHandle op

- **File**: `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-builtins\src\lang_invoke.rs:147-161` (definition), `:779, 801, 822, 870, 892, 911, 957, 982, …` (call sites in `varhandle_get`, `varhandle_set`, `varhandle_compare_and_set`).
- **Issue**: `VH_META_TABLE` is a `Mutex<FxHashMap<usize, VarHandleMeta>>` and `vh_meta_get` does `t.get(&key).cloned()` — cloning a struct that contains four owned `String`s every time. A single `VarHandle.get()` on the instance-field fallback path hits `vh_meta_get` three times (`:779`, `:801`, then again via `vh_type_desc → vh_field_desc → vh_meta_get` at `:814`). Each call grabs the Mutex; under any contention this serialises all VarHandle ops process-wide.
- **Impact**: VarHandles back AtomicInteger/AtomicLong/AtomicReference/ConcurrentHashMap on JDK 9+ — extremely hot under concurrent workloads. Three mutex acquisitions + three 4-String clones per CAS turns lock-free Java code into a global-mutex-bottlenecked path.
- **Fix**: (a) Replace `Mutex` with `parking_lot::RwLock` (or `std::sync::RwLock`) so concurrent reads don't serialise. (b) Wrap the value in `Arc<VarHandleMeta>` so the read path returns `Arc::clone` instead of cloning four Strings. (c) Inside `varhandle_get` / `_set` / `_cas`, call `vh_meta_get` once and pass the `Arc` down instead of re-fetching. Better still, convert the four `String` fields to `Arc<str>` so even the per-field "extract by clone" is a refcount bump.

---

## 6. [HIGH] `alloc_wrapper` resolves "java/lang/Integer" by name on every autoboxing call; no Integer cache

- **File**: `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-builtins\src\lang_math.rs:2179-2188` (helper), `:2190-2198` (`Integer.valueOf`), and every `box_value` arm at `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-builtins\src\lang_class.rs:2202-2247`.
- **Issue**: `alloc_wrapper` calls `ctx.ensure_class_initialized(class_name)` with a `&str` literal every invocation; the VM has to hash/lookup the string on each call. Worse, `Integer.valueOf(int)` always allocates a fresh wrapper — there's no implementation of JDK's IntegerCache (`Integer.valueOf(-128..=127)` returns a cached instance per spec). Same omission for `Byte`, `Short`, `Character`, `Long.valueOf(-128..=127)`, `Boolean.TRUE/FALSE`.
- **Impact**: Autoboxing fires on every `int -> Integer` (Collections, generics, varargs autobox, format args). Every `VarHandle.get` on a primitive field also goes through `box_value`. Hot loops over `Map<Integer, X>` autobox each key on every probe.
- **Fix**: (a) Add a `static WRAPPER_CIDS: OnceLock<[ClassId; 8]>` resolved once per VM and pass the `ClassId` to `alloc_object` directly. (b) Implement the small-int cache — `static INTEGER_CACHE: OnceLock<Mutex<[Option<ObjectRef>; 256]>>` populated lazily, and `native_integer_value_of` returns the cached ref for values in `-128..=127`. Same for `Boolean.TRUE`/`FALSE`. Note: ObjectRefs in the cache need to survive GC — if the VM has weak references for boxed values use those, otherwise cache an Arc-rooted handle.

---

## 7. [MED] `Policy::implies_full` allocates 2N+ Strings per call via `normalize_class`

- **File**: `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-builtins\src\security_manager\policy.rs:215, 237, 260-262, 269, 345, 769, 773`.
- **Issue**: `normalize_class(s) = s.replace('/', ".")` always allocates a new `String`, even when the input is already dotted. On every grant iteration `class_matches` (`:264-270`) calls `normalize_class(grant_class)` (one alloc), plus the inner `if normalize_class(&perm.class_name) == "java.security.AllPermission"` (`:237`) allocates another. So with N grants × M permissions per grant, we allocate ~2N·M Strings — and the request-side `norm_class` (`:215`) is computed once but immediately discarded.
- **Impact**: Every SecurityManager `checkPermission` walks the active policy. JDK boot does ~50k of these (matches the doPrivileged count). At even a modest 10-grant policy that's a million transient `String` allocs over boot.
- **Fix**: (a) Normalise the grant class names *once at parse time* and store `Grant::permissions[i].class_name` already in dotted form (or keep a `class_name_normalized: Arc<str>` next to the raw). (b) Replace `normalize_class(s) == other` with `s.chars().map(|c| if c == '/' { '.' } else { c }).eq(other.chars())` for the borrow-only compare, or an inlined `bytes()` loop. The hot-path `AllPermission` check at `:237` then becomes a pre-computed `perm.is_all_permission: bool` flag set at parse time.

---

## 8. [MED] `String.equals` / `String.charAt` / `String.indexOf(int)` do per-element trait dispatch instead of using bulk char-read intrinsic

- **File**: `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-builtins\src\lang_string.rs:687-693` (`equals`), `:713-721` (`indexOf(int)`), `:763-769` (`indexOf(int, from)`), `:54-60` (`fill_string_chars` per-element loop).
- **Issue**: `read_char_array_into(&self, arr, off, dst: &mut [u16]) -> usize` already exists on `NativeContext` (`native-api\src\registry.rs:254-262`) and the VM-side override is a single `copy_nonoverlapping`. The thread-local scratch (`STRING_CHARS_SCRATCH_A`) was added in a prior round to avoid the `Vec` alloc, but the *fill loop* `fill_string_chars` is still a per-element virtual dispatch (one trait call per char). The comment at `:53` even admits the workaround. `native_string_equals` skips the scratch entirely and just walks both arrays with `get_array_element` calls.
- **Impact**: `String.equals` is on essentially every hash-map probe and string-compare hot path. For a 32-char string compare this is 64 virtual calls vs. 2 `read_char_array_into` + 1 `slice == slice` (which SIMD-memcmps). String hot paths are estimated to spend 30–50% of their time in this loop today.
- **Fix**: (a) Update `fill_string_chars` to grow the `Vec<u16>` to `len`, then call `ctx.read_char_array_into(arr, 0, &mut dst[..len])` — one trait call per side. (b) Rewrite `native_string_equals` to use `with_two_string_chars_scratches(... |a, b| a == b)` after the fast `as_ptr() == as_ptr()` check. (c) Same change for `native_string_index_of` / `native_string_index_of_from` — the loop body becomes `chars.iter().position(|&c| c as i32 == needle)`. The byte-array compact-string path needs a parallel `read_byte_array_into` call followed by a coder-aware comparison.

---

## 9. [MED] `Clock.fixed(Instant, ZoneId)` ignores the Instant argument

- **File**: `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-builtins\src\util_time.rs:4845-4861`.
- **Issue**: The doc comment promises "we allocate a two-field variant when this path is hit" and "Callers that require the fixed-instant semantics should use the real JDK bytecode path." But the code calls `alloc_clock(ctx, zone)` which is the **one-field** synthetic (`CLOCK_NUM_FIELDS: usize = 1`, `:4792`), and the Instant argument (`args.get(1)` for the instant — note: it's `args[0]` for the static factory) is discarded. Calls to `Clock.fixed(some_instant, UTC).instant()` then return wall time, not the fixed instant — observably wrong per the JDK spec.
- **Impact**: Real semantic bug. Tests that mock time via `Clock.fixed` (a common pattern: `LocalDateTime.now(Clock.fixed(...))`) get non-deterministic wall-clock output. Only fires with `synthetic-jdk` feature enabled (file is `#[cfg(feature = "synthetic-jdk")]`), so production NEW-11 mode is unaffected — but unit tests of time-dependent logic break under synthetic mode.
- **Fix**: Add `const CLOCK_FIELD_INSTANT: usize = 1; const CLOCK_NUM_FIELDS_FIXED: usize = 2;`, allocate a 2-field variant in `native_clock_fixed`, store the Instant in slot 1, and make `native_clock_instant` (`:4823`) check if slot 1 is non-null and return that instead of `SystemTime::now()`. Same for `native_clock_millis` (`:4832`) which currently ignores its `this` argument entirely.

---

## 10. [MED] `populate_sfi` allocates the dotted class name per stack frame instead of using the `dotted_class_name` cache

- **File**: `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-builtins\src\lang_stackwalker.rs:73, 79`.
- **Issue**: `populate_sfi` does `ctx.create_string(&entry.class_name.replace('/', "."))` — that's a fresh `String` allocation (the `.replace(...)`) plus a new Java String per frame. `lang_misc::native_throwable_get_stack_trace_element` (the equivalent for `Throwable`) already uses `dotted_class_name(cid, &class_name)` (`lang_misc.rs:314-317`) so the `Arc<str>` is shared and `.replace` runs at most once per class. The StackWalker path *also* re-allocates the internal name at `:79`.
- **Impact**: StackWalker is the canonical path Spring's `deduceMainApplicationClass` walks, plus any `Throwable.getStackTrace` that flows through `StackFrameInfo`. On a 30-deep stack with 3 distinct classes we currently allocate ~3 fresh dotted strings; with the cache we'd allocate them once per JVM run.
- **Fix**: Replicate the lang_misc pattern: `let dotted = match ctx.class_id_by_name(&entry.class_name) { Some(cid) => crate::lang_class::dotted_class_name(cid, &entry.class_name), None => Arc::from(entry.class_name.replace('/', ".")) }; let cls_str = ctx.create_string(&dotted);`. The `decl_internal` string (`:79`) is already in `/` form and the same `Arc<str>` already exists in the entry — pass `&entry.class_name` directly without a separate Java-String alloc if `set_field`'s value can reuse a recently-created string ref (or keep this one alloc but at least skip the dotted re-replace).

---

## 11. [LOW] `Method.invoke` builds invoke args with un-capacitated `Vec::new()`

- **File**: `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-builtins\src\lang_class.rs:4022-4059`.
- **Issue**: `let mut invoke_args: Vec<Value> = Vec::new();` then pushes `param_descs.len() + (if is_static { 0 } else { 1 })` elements. Each `push` past the initial growth doubles capacity (1, 2, 4, 8…). For a 5-arg instance method that's 4 reallocations + copies before settling.
- **Impact**: Per-reflective-invoke overhead — minor unless reflection is on a profiled hot path. Bigger reflective workloads (Jackson deserialisation, RestTemplate marshalling) call this thousands of times.
- **Fix**: One-liner: `let mut invoke_args: Vec<Value> = Vec::with_capacity(param_descs.len() + if is_static { 0 } else { 1 });`. Same opportunity in `lang_invoke.rs:2912, 2933` (`invokeWithArguments` builds `unpacked: Vec<Value>` via `.collect()` from a `(0..len).map(...)` — this auto-sizes correctly via TrustedLen so it's actually fine — but verify under `--release` that the iterator's `size_hint` survives).

---

## 12. [LOW] Constructor mirror side-table uses SipHash on usize keys

- **File**: `C:\Projects\CratonVM\.claude\worktrees\crazy-kirch-fa64d7\native-builtins\src\lang_class.rs:5054-5081`.
- **Issue**: `constructor_mirror_side_table()` returns `&'static Mutex<HashMap<usize, ConstructorMirrorSideMeta>>` — the standard `HashMap` uses SipHash. The key is a raw `ObjectRef` pointer, which already has perfectly good entropy. `rustc_hash::FxHashMap` is already imported and used elsewhere in this file (`:11, 44`). Also, the lookup `peek_constructor_mirror_side` clones the entire `ConstructorMirrorSideMeta` (one owned `String` per descriptor) per call, even when only the `accessible` flag is consulted.
- **Impact**: Low — Constructor lookup is not on the absolute hottest path, but Surefire / JUnit / Jackson reflective construction calls land here. Replacing SipHash with FxHash on a pointer-keyed map is a 2–3× hash speedup, and an `Arc<ConstructorMirrorSideMeta>` value would skip the per-call String clone.
- **Fix**: Change the type to `Mutex<FxHashMap<usize, Arc<ConstructorMirrorSideMeta>>>` (or `parking_lot::RwLock<FxHashMap<...>>` for read-mostly access). Update `register_*` to insert `Arc::new(...)` and `peek_*` to return `Option<Arc<ConstructorMirrorSideMeta>>` so callers can take `&meta.descriptor` without cloning.

---

## Out-of-scope but worth flagging once

- `panama.rs:1142-1148` carries an explicit TODO about a `Box<Cif>` leak when DowncallHandles are GC'd. The mitigation comment says it's bounded ("typically a handful of Cifs"). Acceptable as-is per the documented analysis.
- `serialization.rs:1987` throws `UnsupportedOperationException` for `ObjectInput.readObject` — this is the intended sentinel, not a stub regression. Java object serialization is genuinely unimplemented (out of scope for this audit).
- The thread-local `STRING_CHARS_SCRATCH_*` infrastructure in `lang_string.rs:28-96` is well-designed; the issue (Finding 8) is that the underlying fill loop wasn't updated to use the bulk intrinsic.
