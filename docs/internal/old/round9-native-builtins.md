# Round 9 - native-builtins audit

Scope: `native-builtins/`. Verifying round-8 "fixes" + new angles.

**Verdict: every CRIT round-8 listed is still live in code.** The round-8
doc described intended fixes; the code is unchanged. 1-5 are regressions,
6-12 are remaining/new work.

---

## (A) Regression audit — round-8 CRITs NOT actually fixed

### CRIT-1 — VH_META_TABLE still keyed by raw pointer
`native-builtins/src/lang_invoke.rs:182, 188, 193, 200` —
`vh_meta_put/get/update_field_index` key on `vh.as_ptr() as usize`, NOT
`identity_hash_code(vh)`. After moving GC every VarHandle entry is
orphaned; VH ops fall to the descriptor-coerced slot reads (comment
:131-149) and silently no-op. Fix: thread `&dyn NativeContext` through and
key on `ctx.identity_hash_code(vh)` (i32). The id-hash IS GC-stable —
`gc/src/compact_header.rs:303` (`HashCodeTable::update_after_gc`) remaps.

### CRIT-2 — Boolean.valueOf still thread_local, breaks JLS `==` identity
`native-builtins/src/lang_math.rs:2275, 2585-2606` — `BOOLEAN_CACHE` is
`thread_local!`; two threads see distinct ObjectRefs.
`Boolean.TRUE == Boolean.valueOf(true)` returns false, breaking
`Optional`/`Map.containsValue`/lambda-capture invariants. Replace with
`OnceLock<[ObjectRef; 2]>` seeded once, registered as GC roots in
`vm/src/memory/roots.rs`.

### CRIT-3 — INTEGER_CACHE still thread_local, no GC root scan
`native-builtins/src/lang_math.rs:2272-2297` — `RefCell<[Option<ObjectRef>;
256]>` per OS thread. `vm/src/memory/roots.rs:20-164` never visits these.
Under semispace compaction cached refs become stale forwarding addresses,
`Integer.valueOf(5).intValue()` reads garbage. Hoist to process-global +
GC root.

### CRIT-4 — LongAdder cell strip table keyed by raw pointer
`native-builtins/src/lib.rs:29065, 29173, 29227, 29257` —
`long_adder_cells_table` keys on `this.as_ptr() as usize`. After GC the
LongAdder relocates, the next `add()` allocates a fresh strip, every
contended add is lost, `sum()` returns base-only. Key on
`ctx.identity_hash_code(this)` (matches the TL_MAP round-7 fix). Sibling:
`long_adder_stripe` (:29078) FxHashes dense `ThreadId`s — 4-thread loops
collide on 2/8 cells. Multiply by `0x9E37_79B9_7F4A_7C15` before masking.

### CRIT-5 — synthetic_field_store + class_atomic_side_store keyed by raw ptr
`native-builtins/src/lib.rs:12182-12219, 12265-12270` — both stores key on
`obj.as_ptr() as usize`. The parking_lot migration landed; the pointer-key
bug is unchanged. Every `Unsafe.putReference` fallback (`synthetic_put`)
and every `Class$Atomic` CAS that survives a GC silently misses, re-inserts
fresh, old slot leaks, CAS livelocks (the exact symptom the comment at
:12222 says we're working around). Re-key on identity hash; add a dead-key
sweep called from `vm/src/memory/gc.rs`.

---

## (B) Round-7/8 items still open

### HIGH-6 — ObjectInput.readObject still returns null
`native-builtins/src/serialization.rs:1995-1997` — TODO from round-7 H-12
unaddressed. Implement type-tag dispatch for `String/Integer/Long/Boolean`
via the existing `ois_read_object` decoder at `:925`.

### HIGH-7 — Field.getInt accepts boolean fields (JDK divergence)
`native-builtins/src/lang_class.rs:3227-3239` — matches only on
`Value::Int`, ignoring descriptor. A `Z` field stored as `Value::Int(0|1)`
passes `getInt`, but JDK throws IAE. Read descriptor from
`read_field_meta` and reject anything not `B/S/C/I`. Same for `getLong`
(accepts `Z`), `getFloat`, `getDouble`.

### MED-8 — DateTimeFormatter pattern cache unbounded
`native-builtins/src/util_time.rs:4210-4240` — no eviction; user patterns
grow forever. Bound at ~256 with ArrayDeque-driven LRU.

### MED-9 — LambdaMetafactory + StringConcatFactory CallSites uncached
`native-builtins/src/lang_invoke.rs:2239-2293` and
`native-builtins/src/phases_late.rs:10730-10752` — every BSM call
allocates a fresh CCS + 17-slot MH. Cache on `(samMethodType,
implMethod-id-hash, instantiatedType)` and on the recipe string.

### LOW-10 — tl_with_initial_suppliers locked on every TL.get miss
`native-builtins/src/phases_early.rs:2335` — global Mutex even with zero
suppliers. Add `AtomicUsize` count + `Relaxed == 0` fast-path skip.

---

## (C) New angles

### MED-11 — Unsafe.compareAndExchange family entirely missing
`native-builtins/src/unsafe_natives.rs:943-1024` — only `compareAndSet*`
and `weakCompareAndSet*` registered. JDK21+ `VarHandle.compareAndExchange*`
lowers to `Unsafe.compareAndExchangeInt/Long/Reference`; our registry
returns "method not found", caller falls back to non-atomic load/store.
Add the 12 variants (`Int/Long/Reference` × `Plain/Acquire/Release/Volatile`)
delegating to the heap CAS and returning the witnessed prior value.

### MED-12 — ScopedValue bindings: keys not in GC roots
`vm/src/memory/roots.rs:142-147` scans `thread.scoped_values` *values* but
not the `_key` ObjectRefs. A ScopedValue carrier reachable only via its
binding becomes a dangling key after compaction; `runWhere` unwind keys
by identity and silently drops the binding. Push the key ObjectRef too.
