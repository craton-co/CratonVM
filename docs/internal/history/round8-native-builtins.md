# Round 8 — native-builtins audit

Scope: `native-builtins/`. Verifies round-7 wave-1+2 fixes; surfaces new issues.
Per round-8 rules, audit (regression) findings tagged CRIT.

---

## (A) Regressions from round-7

### CRIT-1 — LongAdder cell side table keyed by raw pointer
`native-builtins/src/lib.rs:29044`, `:29153`, `:29207`, `:29237` — keys
`FxHashMap<usize, …>` on `this.as_ptr() as usize`. Same pointer-key bug
round-7 fixed for `TL_MAP` (`phases_early.rs:2183`). Under moving GC the
LongAdder relocates, the key misses, subsequent ops see a fresh strip and
silently lose every contended add. Fix: key by `ctx.identity_hash_code(this)`
exactly like `TL_MAP`.

### CRIT-2 — VarHandle metadata side table keyed by raw pointer
`native-builtins/src/lang_invoke.rs:182, 188, 193, 200` — `vh_meta_table`
uses `vh.as_ptr() as usize`. After GC compaction every VH op falls to the
slot fallback whose slot 0 was clobbered by descriptor-coercion (see
comment `:131-149`) — VHs silently no-op across a GC cycle. Re-key on
`identity_hash_code(vh)`.

### CRIT-3 — LongAdder cell strip NOT lazily allocated
`native-builtins/src/lib.rs:29065-29075` — `native_long_adder_init`
eagerly allocates `8 × AtomicI64 + Arc + map entry` for every LongAdder.
The audit specifically called out "lazy on first contention". Real JDK
Striped64 keeps `cells` null until the first failed CAS. Drop the
`let _ = long_adder_cells(this);` and let the contended branch in `_add`
materialise via `or_insert_with`.

### CRIT-4 — LongAdder stripe hash non-uniform on small ThreadIds
`native-builtins/src/lib.rs:29058` — `FxHasher` on dense u64 ThreadIds
(1..16) leaves low 3 bits correlated; a 4-thread hot loop frequently
collides on 2 cells of 8. Spread first:
`h.finish().wrapping_mul(0x9E37_79B9_7F4A_7C15)` then mask.

### CRIT-5 — Boolean.valueOf cached per-thread breaks `==` identity
`native-builtins/src/lang_math.rs:2275, 2585-2606` — JLS guarantees
`Boolean.valueOf(true) == Boolean.TRUE` (a global static field). Per-thread
cache returns distinct refs across threads; `b1 == b2` returns false.
Move to a process-wide `OnceLock<[ObjectRef;2]>` seeded once and registered
as a GC root.

### CRIT-6 — INTEGER_CACHE not registered as GC root
`native-builtins/src/lang_math.rs:2272-2297` — cached refs held only in
`thread_local!`; GC never scans them. Under moving/sweeping GC `Integer(5)`
can be reclaimed or relocated while the cache returns a stale ObjectRef.
Pin via root registration on insert, or hoist to process-wide cache + root.

### CRIT-7 — TL identity hash i32 collision contaminates ThreadLocals
`native-builtins/src/phases_early.rs:2220, 2285` — keys are i32 identity
hashes. With ≥50 k live TLs (Tomcat + Netty + Hibernate) birthday-paradox
collision probability is ≈58%; two TLs hashing to the same i32
cross-contaminate (`TL_MAP.get(&key)` returns the *other* TL's value).
Same risk for `WITH_INITIAL_SUPPLIERS`, `INHERITABLE_TL_IDS`. Use a
`(ClassId, identity_hash)` composite key or widen to i64.

---

## (B) Round-7 items still unfixed

### HIGH-8 — `ObjectInput.readObject` returns null forever
`native-builtins/src/serialization.rs:1995` — null saves JNDI bootstrap
but breaks Spring session / RMI / Jackson back-compat. Implement
String/Integer/Long via `ois_read_object` at `:925`.

### MED-9 — StringConcatFactory CallSite target is a no-op MH
`native-builtins/src/phases_late.rs:10730-10752` — both `makeConcat`
variants return a 17-slot synthetic MH with zero behaviour; invoking the
CallSite returns null instead of the concatenated string. Parse recipe
from args[1]/[3] and emit a builtin concat MH kind dispatched by the
interpreter.

### MED-10 — LambdaMetafactory CallSite not cached
`native-builtins/src/lang_invoke.rs:2238-2293` — every `metafactory` call
allocates a fresh CCS + MH. Cache on
`(invokedType, samMethodType, implMethod ptr, instantiatedMethodType)`.

---

## (C) New angles

### MED-11 — DateTimeFormatter pattern cache unbounded
`native-builtins/src/util_time.rs:4210-4240` — no eviction; user-controlled
patterns can grow the map indefinitely (DoS vector for web log filters).
LRU-bound at ~256 entries.

### LOW-12 — `tl_with_initial_suppliers` lock on every TL miss
`native-builtins/src/phases_early.rs:2335` — every TL.get() miss locks the
global Mutex even with zero registered suppliers. Add `AtomicUsize::load(Relaxed)
== 0` fast-path to skip the lock in the common case.
