# Round 4 — VM crate review

Findings are ordered by expected impact (highest first). All file references
are absolute paths within the repo. Focus: interpreter, JIT helpers,
exception path, vtable/itable, frame lifecycle, value stack.

---

## 1. [CRIT] JIT `jit_invoke_dispatch` calls `std::env::var_os` on every call

**File:** `vm/src/jit/helpers.rs:1045-1063` and `:1064-1077`

```rust
if std::env::var_os("RUSTJVM_DISABLE_JIT").map(|v| { ... }).unwrap_or(false) {
    ...
    return 0;
}
if std::env::var_os("RUSTJVM_DBG_JIT_DISPATCH").is_some() { ... }
```

Both checks fire on **every** JIT-dispatched invoke (the hottest call site
in the VM). `env::var_os` on Linux/macOS takes the internal env mutex,
walks the env list, and allocates an `OsString`. On Windows it queries the
process environment block. Cost is roughly 200–500 ns per call vs. the
~5–10 ns of an `AtomicBool::load(Relaxed)`.

**Impact:** Direct addition of hundreds of ns to every JIT→JIT virtual
call. The `RUSTJVM_DBG_JIT_DISPATCH` lookup happens unconditionally even
when the variable is unset.

**Fix:** Read the var once at VM init into an `AtomicBool` (mirror the
pattern used by `dispatch_trace::init_from_env` and
`exceptions::IAE_TRACE`). Gate both checks on that bool.

---

## 2. [CRIT] `Itable::lookup` allocates two `String`s per interface dispatch

**File:** `vm/src/runtime/vtable.rs:289-323`

```rust
pub struct Itable {
    entries: HashMap<(u64, String, String), usize>,
}
pub fn lookup(&self, interface_class_id: u64, method_name: &str, descriptor: &str) -> Option<usize> {
    let key = (interface_class_id, method_name.to_string(), descriptor.to_string());
    self.entries.get(&key).copied()
}
```

Every `invokeinterface` that reaches `resolve_interface` (vtable.rs:615)
allocates two fresh `String`s purely to use as a HashMap probe key. Even
though `VtableEntry` (line 52) intentionally stores `Arc<str>` for exactly
this reason, the parallel `Itable` was missed.

**Impact:** Two heap allocations + two strcpy + SipHash on every
interface dispatch that misses the higher-level invoke caches. For
collection-heavy code (`Iterator.next()`, `Map.get()`, `Comparator.compare()`)
this can dominate dispatch cost.

**Fix:** Mirror `Vtable::fast_lookup` — key the map by
`u64 = fxhash(interface_id) ^ fxhash(name) ^ fxhash(desc).rotate_left(17)`,
store `Arc<str>` for tie-breaking, and probe with `&str` borrowed args.

---

## 3. [CRIT] `RUSTJVM_FRAME_TRACE` and friends called per frame push/pop

**File:** `vm/src/runtime/interpreter.rs:2239`, `:2379`, `:4606`, `:4911`,
`:10042`, `:10100`, `:10595`, `:12828`, `:12954` (and a dozen more)

```rust
if std::env::var_os("RUSTJVM_FRAME_TRACE").is_some() { ... }
if std::env::var("RUSTJVM_TRACE_SB_FILTER").is_ok() { ... }
```

`push_frame_and_fire_entry` (called at every Java method entry),
`pop_and_recycle_frame_with_reason` (every method return/unwind), and the
JIT entry/dispatch sites all call `std::env::var*` unconditionally.

**Impact:** Two env-table walks per method invocation cycle. Per round-2
audit notes (`exceptions.rs:14`), env-var lookups can be hundreds of ns —
this adds the same cost to every call frame.

**Fix:** Mint a `runtime::diagnostics::tracing_gates` module that reads
each `RUSTJVM_*` flag once at VM init into a `static AtomicBool` and
gates all of the call sites on those bools.

---

## 4. [CRIT] OSR back-edge counter uses `==` instead of `>=`/modulo — fails forever after one miss

**File:** `vm/src/runtime/interpreter.rs:2625`, `:2954`, `:2991`, `:3025`,
and ~6 other backward-branch sites

```rust
frame.backward_count += 1;
let bc = frame.backward_count;
if bc == OSR_THRESHOLD {
    let osr_class_id = thread.frames[frame_idx].class_id;
    if let Some(osr_val) = try_osr(shared, thread, ...) { ... }
}
```

The exact-equality check fires `try_osr` exactly once — when `bc == 1000`.
If OSR returns `None` (compilation rejected by skip-list, init complexity
gate, JIT cache eviction, native override discovered, etc.) the same
loop **never re-attempts OSR** even though `backward_count` keeps
climbing past 1000.

**Impact:** Tight loops that get rejected once stay in the interpreter
forever. For workloads where the first OSR attempt loses a race (e.g.
during class-init), entire hot loops are permanently stuck at interpreter
speed. Also: each of the ~10 sites duplicates this logic — high
maintenance burden, easy to drift.

**Fix:** Replace with `bc >= OSR_THRESHOLD && bc % OSR_THRESHOLD == 0`
(retry every 1000 backward branches on failure), and extract the
try-osr-then-unwind block into a helper called from each site.

---

## 5. [HIGH] `jit_invoke_dispatch` allocates 3 fresh `Arc<str>` per JIT-cache lookup

**File:** `vm/src/jit/helpers.rs:1151-1154`

```rust
let class_arc: std::sync::Arc<str> = std::sync::Arc::from(info.class_name);
let method_arc: std::sync::Arc<str> = std::sync::Arc::from(info.method_name);
let desc_arc: std::sync::Arc<str> = std::sync::Arc::from(info.descriptor);
let jit_cache = vm.jit_cache.read();
if let Some(compiled) = jit_cache.get(&class_arc, &method_arc, &desc_arc) { ... }
```

`info.class_name` is already `&'static str` (jit/src/lib.rs:1064). Each
`Arc::from(&'static str)` allocates an Arc header + a fresh byte buffer
and memcpy's the string. Three allocations, three frees, every time the
DISPATCH_CACHE misses.

**Impact:** ~3 × (alloc + copy + free) per JIT call site warmup. For
moderately-sized class names ("org/springframework/context/...") this is
6 cache-miss heap operations per uncached dispatch.

**Fix:** Add a `JitCache::get_by_str(&str, &str, &str) -> Option<...>`
that hashes the borrowed slices directly. Same trick as `Vtable::lookup_slot`.

---

## 6. [HIGH] `find_exception_handler` allocates a `String` per catch entry and takes the class_manager read lock twice

**File:** `vm/src/runtime/interpreter.rs:4668-4697`

```rust
let catch_class_name = {
    let cm = shared.class_manager.read();
    let class = cm.get_class(frame.class_id)?;
    class.constant_pool.get_class_name(entry.catch_type)
        .map(|s| s.to_string())
};
...
let catch_class_id = {
    let cm = shared.class_manager.read();
    cm.find_class_by_name(&catch_class_name)
};
...
let cm = shared.class_manager.read();
if cm.is_subclass_of(exc_class_id, catch_class_id) { ... }
```

Three RwLock read acquisitions per catch entry checked, plus an owned
`String` clone of the catch type name even when the lookup hits. For
exception-driven control flow (parser combinators, Iterator-end
detection, retry loops) this happens hundreds of times per second.

**Impact:** Lock contention amplification under multi-threaded workloads
(every catch search blocks any concurrent classloader write), and
unnecessary allocation on hot exception paths.

**Fix:** Hold a single `class_manager.read()` guard for the whole entry
match. `get_class_name` on a `ConstantPool` should return `&str` (the
`String::to_string` is purely so the value escapes the lock — fix by
keeping the lock).

---

## 7. [HIGH] JIT array helpers swallow null-array dereferences instead of throwing NPE

**File:** `vm/src/jit/helpers.rs:411-465`, `:469-480`, `:547-554`

```rust
pub unsafe extern "C" fn jit_iaload(array_ptr: i64, index: i64) -> i64 {
    if array_ptr == 0 { return 0; }
    ...
}
pub unsafe extern "C" fn jit_arraylength(array_ptr: i64) -> i64 {
    if array_ptr == 0 { return -1; }
    ...
}
```

Per JVMS, `iaload`/`aaload`/`baload`/`caload`/`saload`/`iastore`/`aastore`/
`arraylength` on a null array must throw `NullPointerException`. These
helpers return `0`/`-1` silently. Out-of-bounds also returns `0` instead
of throwing AIOOBE. The thread-local `JIT_PENDING_AIOOBE` slot exists
but is never set from these helpers (only `jit_throw_aioobe` writes it).

**Impact:** Java programs that catch NPE on array access — or that
dispatch on `arraylength == -1` as a probe — silently see wrong values
instead of the spec-mandated exception. This is a correctness bug, not
just performance: any code path that depends on NPE semantics for null
arrays is broken under JIT.

**Fix:** On null/oob, set `JIT_PENDING_AIOOBE`/`JIT_PENDING_EXCEPTION`
(create the NPE Throwable via `create_exception_object`) and return the
deopt sentinel `i64::MIN`. The interpreter already drains both slots
after JIT returns.

---

## 8. [HIGH] `jit_invoke_dispatch` MIC fast-path silently returns 0 for ≥5-arg callees

**File:** `vm/src/jit/helpers.rs:127-170` (`call_jit_compiled_method_entry`),
called from `:1607`

```rust
match n {
    0 => { ... f() }
    1 => { ... f(args_slice[0]) }
    ...
    4 => { let f: unsafe extern "C" fn(i64,i64,i64,i64) -> i64 = ... }
    _ => 0,  // <-- silent corruption
}
```

The non-context arm tops out at 4 i64 args; the with-context arm at 3
(plus vm_ptr). Any JIT-compiled callee with ≥5 register-passed args
**never executes** — the helper returns `0` as if the call succeeded.
This is the same class of latent bug as finding #7: silent wrong answers
instead of a deopt or runtime error.

The same pattern is duplicated in three places in `jit_invoke_dispatch`
(thread-local hit, JIT-cache hit, post-compile path) — all three cap at
the same arities.

**Impact:** Methods with ≥5 i64-flavored args (very common in Java —
constructors, builder methods, comparator chains) silently return zero
when reached via JIT dispatch. Spring's `ConfigurableBeanFactory.registerSingleton`,
many `equals(Object)` chains, etc.

**Fix:** Extend the match all the way to the maximum arg count the JIT
emits (or fall back to a generic stack-based trampoline for `n > 4`).
At minimum, log a hard error and return the deopt sentinel rather than 0.

---

## 9. [HIGH] `pop_object_ref_ctx` takes `Option<String>` — caller always allocates

**File:** `vm/src/runtime/interpreter.rs:13426-13428` (signature), and
~10 call sites such as `:5010`, `:5060`, etc.

```rust
fn pop_object_ref_ctx(stack: &mut ValueStack, context: Option<String>) -> ... {
    match stack.pop()? {
        ...
        Value::Object(None) => Err(RuntimeError::NullPointerException { message: context }.into()),
        ...
    }
}
// Caller:
let array_ref = pop_object_ref_ctx(
    &mut thread.frames[frame_idx].stack,
    Some("Cannot load from null array".to_string()),  // alloc on every call
)?;
```

The `String` is built unconditionally — every successful array load /
field access pays for a heap allocation that is consumed only on the
null path.

**Impact:** Every slow-path `iaload` / `getfield` / `aload` triggers a
heap allocation purely to *not* use it. Hot for JDK code (because the
fast path is gated to non-JDK classes, see finding #11).

**Fix:** Change the signature to `context: &'static str` (most callers
use literals) or `context: impl FnOnce() -> String`. The `Option`
wrapper is also pointless — pass `""` for "no context".

---

## 10. [HIGH] `ThreadLocalResolveCache` evicts with `Vec::remove(0)` — O(n) shift per eviction

**File:** `vm/src/runtime/lockfree_resolve.rs:149-152`, `:178-181`

```rust
while self.methods.len() + self.fields.len() >= self.max_entries
    && !self.method_order.is_empty()
{
    let oldest = self.method_order.remove(0);  // O(n) memmove
    self.methods.remove(&oldest);
}
```

Once the cache fills to its 4096 entry cap, every subsequent insertion
calls `Vec::remove(0)`, which memmoves up to 4096 × 24-byte
`ResolutionKey` slots. Same problem in `put_field`.

Also: `get_method`/`get_field` (line 132-140) do two hashmap lookups
(`contains_key` then `get`) where `entry()`/`get` would do one.

**Impact:** O(n²) behaviour in resolution churn for steady-state code
that touches >4096 method/field combinations. Common in framework-heavy
applications.

**Fix:** Use `VecDeque<ResolutionKey>` for the eviction queue
(`pop_front()` is O(1)). Replace the double-lookup in `get_method` with
a single `if let Some(v) = self.methods.get(key)` and a separate
fast-counter bump.

---

## 11. [MED] Interpreter fast path disabled for *all* JDK classes

**File:** `vm/src/runtime/frame.rs:294-300`

```rust
pub(crate) fn class_disables_interp_fast_path(class_name: &str) -> bool {
    class_name.starts_with("java/")
        || class_name.starts_with("jdk/")
        || class_name.starts_with("sun/")
        || class_name.starts_with("com/sun/")
        || class_name.contains("springframework")
}
```

The hand-tuned super-instruction loop in `execute_frame` (lines
2575–4500) — the entire reason for the `pop_unchecked` infrastructure
— is gated off for every JDK class, which is the vast majority of
executed bytecode. The comment at line 2557-2561 acknowledges that
"real JDK bytecode can produce patterns that the fast path doesn't
handle", but the answer is to *fix the offending fast-path opcodes*,
not blacklist the JDK.

Also: `contains("springframework")` is O(n) on the class name on every
frame creation — even for non-Spring classes.

**Impact:** The N most expensive fast-path optimizations (typed iadd,
NaN-boxed push/pop without enum round-trip, superinstruction look-ahead)
fire for ≤5% of executed methods. The rest of the time the slow path
re-allocates via the `Instruction::decode` dispatch.

**Fix:** Audit which specific opcodes diverged historically (probably
ldiv/lrem and dload/dstore on mixed tag stacks) and harden just those.
The fast path's null/uninit coercions in `pop_int_unchecked` and
`pop_long` already cover most JDK pathology. Drop the JDK blocklist or
shrink it to a handful of known-bad classes.

---

## 12. [MED] `safe_native_call` and other hot paths use the global env mutex

**File:** `vm/src/runtime/interpreter.rs:1272`, `:1277`, `:1582`, `:5886`,
`:6188`, `:6403`, `:6513`, `:8320`, `:8980`, `:9214`, `:9231`, `:9819`,
`:9836`, `:13503`

Same pattern as findings #1 and #3: every `std::env::var*` call inside
the interpreter (not just frame push/pop) reads a process-global env
list. These cover BigDecimal debug, lambda debug, IAE trace, athrow
debug, NPE trace, resume-PC debug, CCE debug — about 14 sites total in
`interpreter.rs` alone.

**Impact:** Cumulative — every interpreter execution path pays one or
two env reads. Negligible in isolation but together easily measurable
in any tight bytecode loop.

**Fix:** Same cache-once-at-init pattern as #1 and #3 — `static
AtomicBool` array indexed by a `DebugFlag` enum, populated from env in
one pass during VM startup.

---

## 13. [MED] `make_method_key` clones 2 `Arc<str>` per profiled branch

**File:** `vm/src/runtime/interpreter.rs:2323-2329`, called from
~20 PGO branch/back-edge sites in the hot interpreter loop

```rust
fn make_method_key(frame: &Frame) -> MethodKey {
    MethodKey {
        class_id: frame.class_id.as_u32(),
        method_name: frame.method_name_arc(),
        descriptor: frame.method_descriptor_arc(),
    }
}
```

Even though `pgo_enabled` is hoisted out of the loop (correct), each
profiled site that *does* record clones two Arc handles (two atomic
refcount increments + matching decrements at MethodKey drop).

**Impact:** PGO warmup is slower than necessary; backward-branch
counting is the dominant profile event and pays this cost on every
loop iteration during warmup. For tight numeric loops this can halve
profile-collection throughput.

**Fix:** Have `ProfileStore::record_branch` etc. take `&Frame` or
`(class_id, method_name: &str, descriptor: &str)` and intern internally,
so the hot path never clones Arcs. The store can deduplicate via its own
`FxHashMap<u64, ProfileEntry>` keyed on a 64-bit hash.

---

## 14. [MED] `pop_double` lookup uses `as_double().unwrap_or(f64::from_bits(cv.to_bits()))`

**File:** `vm/src/runtime/value_stack.rs:630-635`

```rust
CompactTag::Double => {
    self.len -= 1;
    Ok(cv.as_double().unwrap_or(f64::from_bits(cv.to_bits())))
}
```

`as_double()` returns `None` only for NaN-tagged slots (per
`compact_value.rs:367`). The tag has already been confirmed as
`CompactTag::Double` two lines earlier — which by construction means
**not** NaN-tagged. So `unwrap_or(...)` always takes the `Some` path
and the fallback is dead code that LLVM may or may not eliminate.

More importantly, `as_double()` re-checks `is_nan_tagged` after we just
checked the tag. With the dead fallback removed, this collapses to a
direct `f64::from_bits(cv.to_bits())` — one load, no branch.

**Impact:** Small (one redundant predicted branch + a function call
LLVM may not fully inline) but on a fundamental opcode (dload, dadd,
dstore round-trip pathway).

**Fix:** Replace the line with `Ok(f64::from_bits(cv.to_bits()))`. The
function-level doc-comment already acknowledges this is safe.

---

## 15. [MED] `CompactValue::object` runs unconditional `assert!` in release builds

**File:** `types/src/compact_value.rs:215-238`

```rust
pub fn object(ptr: u64) -> Self {
    debug_assert!(ptr != 0, ...);
    debug_assert!(ptr & !PAYLOAD_MASK == 0, ...);
    assert!(ptr != 0, ...);             // release too
    assert!(ptr & !PAYLOAD_MASK == 0, ...);
    Self(make_tagged(SUB_OBJECT, ptr & PAYLOAD_MASK))
}
```

Two `assert!`s on every object push — including the JIT inline TLAB
fast path that calls `CompactValue::object` on every freshly-allocated
object stored to a local/stack slot. The doc comment explains the
release-mode assert as a defence against silent truncation, but the
47-bit constraint is checked at every receiver-push and at every aload.

**Impact:** Two branches per object push. For object-heavy code
(getter chains, iterator construction) this is on the *hottest* part
of the interpreter loop.

**Fix:** Keep only `debug_assert!`. Callers that handle untrusted
pointers (mmap, JNI smuggling) should use `try_from_pointer` which is
already provided.

---

## 16. [LOW] `std::panic::catch_unwind` around every `execute_frame`

**File:** `vm/src/runtime/interpreter.rs:2275-2292`

Every Java method invocation wraps `execute_frame` in `catch_unwind`
purely to convert `pop_unchecked` panics into a `MethodCallFailed`.
On Windows this allocates SEH unwind metadata per invocation; on
Linux it installs a personality-routine entry. The fast path is
careful not to underflow, so panics from `pop_unchecked` indicate a
real bug we'd rather see immediately.

**Impact:** Per-method invocation cost (~50–100 ns on Windows) for
panic insurance that should never fire in correct bytecode.

**Fix:** Remove the catch_unwind. If pop_unchecked panics are still a
real concern, gate the catch behind a `#[cfg(debug_assertions)]` or
a runtime-once-init env flag — production runs shouldn't pay for it.

---

## Out of scope / acknowledged

- The `Vec<CompactValue>` slot layout, NaN-boxed CompactValue
  encoding, sharded VecPool, vtable single-Arc clones, MIC/PIC,
  force-decoded attrs, OSR trampoline cache, exception-tracing gate,
  and value_stack typed helpers are all already done per the
  problem statement and have been confirmed in this pass.
- The `class_manager.read()` count (~118 in interpreter.rs alone) is
  high but most are gated by the per-call resolution caches. The
  exception-handler case (#6) is the only one frequently hit on the
  hot path.
