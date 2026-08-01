# Process-global mutable state in `vm/src/` — round 2

**Status: 🟢 ALL DANGEROUS ITEMS CLOSED 2026-08-01.** Every entry the round-1
census classified as *dangerous* (holds an `ObjectRef` or a raw heap address) is
now either fixed or is the one documented, deliberate out-of-scope item
(`PROCESS_VM`). Two per-VM-leak items are also closed. The VM-agnostic
`register_native_root_source` registry has been **deleted** — it has no callers
left, and it could not have a correct one.

Round 1 (the census, the three-category taxonomy, and the `OscCache` /
long-smuggle-mint fixes) is
[`vm-process-global-state.md`](vm-process-global-state.md). This document does
not restate it; read it first for the classification vocabulary used below.

---

## Fixed in this change

### 1. `native/jni.rs` `DIRECT_BUFFERS` — deleted (DANGEROUS, no second VM needed)

Round 1's description of the hazard was correct and is verified: the key was
`obj_to_jobject(obj)`, which for a local ref is literally the object's raw heap
address (`vm/src/native/jni.rs:1815-1817` — `obj.as_ptr() as u64`; the local-ref
branch of `jobject_to_obj` at `:1848` confirms the round trip). The table was
never remapped, never swept, process-global, and consulted before the field
read. A recycled address made `GetDirectBufferAddress` hand native code the
previous buffer's `malloc` pointer.

**Round 1's recommendation ("prefer deleting the cache — the field read is
correct on its own") was half wrong, and the half that was wrong is the
interesting part.** The fallback read was `heap.get_field(oref, 0)` matched
against `Value::Long`. That is only correct when the class is a fabricated stub
(tagged `Value` slots, so a `Long` round-trips verbatim). When
`load_class_concurrent("java/nio/DirectByteBuffer")` returns the **real** class,
slot 0 is `java.nio.Buffer.mark`, an `int`:

* `write_compact_field`'s `FieldStorageKind::Int` arm
  (`types/src/field_layout.rs:862-868`) stores `0` for any non-`Int` value, so
  `set_field(obj, 0, Value::Long(address))` wrote **zero**;
* the read then returned `Value::Int(0)`, which the `Value::Long` match rejected
  → the fallback answered null.

So in real-JDK mode the side table was the *only* thing making the getters work,
and deleting it naively would have been a Netty/ES-grade regression. Worse, the
table was masking a second bug it did not fix: the buffer object handed back to
Java had `address = 0`, `mark = 0`, `position = capacity` — a `DirectByteBuffer`
that Java-side `ByteBuffer` operations cannot use at all. This is the same
fixed-slot-vs-real-layout clobber already documented in
`native-io/src/direct_buffer.rs:617-630`.

**What landed** (`vm/src/native/jni.rs:6264-6470`):

* `dbb_slots(shared, class_id)` (`:6309`) resolves `address` (`J`) and
  `capacity` (`I`) through
  `vm_exec::resolve_field_index_in_hierarchy_desc` — descriptor-qualified, so a
  same-named subclass field cannot shadow `Buffer`'s. It answers `Some` only
  when **both** resolve, so the setter and both getters always agree on which
  layout they are addressing.
* `jni_new_direct_byte_buffer` (`:6338`) writes by name in real-JDK mode —
  and seeds `limit`/`position`/`mark` too, so the buffer is actually usable from
  Java — and falls back to the historical fixed slots 0/1 for a stub class.
* `jni_get_direct_buffer_address` (`:6420`) / `..._capacity` (`:6448`) read
  through the same resolution. Both carry one `or_else` fallback that fires only
  when the named read returns `None` (an out-of-bounds slot), which covers a
  stub class upgraded to the real class *underneath a live object*. A named slot
  that reads a real value always wins, so a real `Buffer`'s `mark` can never be
  mistaken for an address.
* `DIRECT_BUFFERS`, `SendPtr`, and the `unsafe impl Send/Sync` are gone.

**Tests** (`vm/src/native/jni.rs:8642-8770`):

* `jni_direct_buffer_address_follows_the_object_not_a_cache` — the regression
  test. Creates a buffer, then re-points the *object's* address/capacity fields
  while leaving the handle (the old cache key) untouched, and asserts the
  getters follow the object. With the cache present this returned the stale
  pointer. It then zeroes the address field — what a recycled slot looks like —
  and asserts a null answer rather than a stale `malloc` pointer.
* `jni_direct_buffers_are_independent` — two live buffers do not cross-talk.
* `jni_direct_buffer_zero_capacity_round_trips` — `capacity == 0` is legal, so
  the capacity getter must not use "non-zero means present" as its presence
  test the way the address getter does.

### 2. `runtime/instrument.rs` transformer chain — re-keyed per VM (DANGEROUS, isolation)

Round 1's premise verified: the scan and remap halves *were* correctly paired,
so this was never a UAF within one VM — purely isolation, on the GC axis and the
semantic axis both.

**What landed:**

* `TransformerChains = HashMap<usize, Vec<TransformerEntry>>`
  (`vm/src/runtime/instrument.rs:123`), keyed on `vm_identity`, behind
  `with_chain` / `with_chain_mut` helpers (`:133`, `:142`) that recover from
  lock poisoning rather than skipping (a GC half must never silently no-op).
  `with_chain` does **not** create a row, so a VM that never installs a
  transformer never allocates one.
* Every public entry point takes `vm: usize`: `add_transformer_entry` (`:237`),
  `remove_transformer_entry` (`:243`), `snapshot_transformer_chain` (`:257`),
  `transformer_count` (`:262`), `reset_transformer_chain` (`:267`),
  `add_premain_transformer` (`:279`). Every production caller has a
  `&mut dyn NativeContext` in scope and passes `ctx.vm_identity()`.
* `scan_transformer_roots(vm, ..)` (`:179`) / `remap_transformer_refs(vm, ..)`
  (`:203`) are now driven from the compile-time inventory as
  `root_source!("instrument-transformers", ..)`
  (`vm/src/memory/native_roots.rs:293-299`, `:346-351`), which passes the
  **owning** `SharedVm`. The lazy `ensure_transformer_root_source_registered`
  `Once` is gone — the source is live from the VM's first collection instead of
  from its first `addTransformer` call.
* `remap_transformer_refs` early-returns on an empty pointer map (the
  non-moving sweep), so it never touches the lock on that path.
* **Teardown:** `forget_vm_transformers(vm)` (`:227`) drops the row, wired into
  `release_vm_native_state` (`vm/src/vm/vm_init.rs:7288`) — the same hook that
  already released the capability set and the SecurityManager row. Without it a
  disposed VM's chain keeps addresses into a dead heap and a later VM reusing
  the identity inherits them as roots.

**Tests** (`vm/src/runtime/instrument.rs`, transformer-chain block): every
pre-existing test now uses its own `unique_vm()` identity, so the block is
genuinely parallel-safe instead of relying on a `reset_transformer_chain()` at
the top of each test that only worked because no two of them happened to
interleave. Three new tests: `chains_are_per_vm`,
`gc_halves_only_touch_the_owning_vm` (VM B's scan must not report VM A's
addresses; VM B's fixup must not rewrite VM A's entries),
`forget_vm_transformers_drops_the_row_and_is_idempotent`, plus
`reading_an_unknown_vm_does_not_create_a_row`.
`vm/src/vm/vm_init.rs`'s `dropping_a_vm_releases_its_capability_and_security_state`
now also asserts the transformer row is released.

### 3. `memory/native_roots.rs` — the VM-agnostic registry is deleted

`register_native_root_source(scan: fn(&mut Vec<ObjectRef>), remap:
fn(&HashMap<usize, usize>))`, its `LazyLock<RwLock<Vec<NativeRootSource>>>`
backing store, `scan_all_native_roots`, `remap_all_native_roots`, the `ScanFn` /
`RemapFn` type aliases and their four tests are gone. The transformer chain was
the last caller (`git grep register_native_root_source` now matches only prose).

This is not just dead-code removal. A callback with no VM parameter **cannot**
be correct: it has no way to know which heap is collecting, so every subsystem
that joined the registry was an isolation bug by construction. Both of its
members turned out to be exactly that (`OscCache` in round 1, the transformer
chain here). The module doc now says so and directs new side tables to
`VM_ROOT_SOURCES`, keyed on `vm_identity` if they must live in a static
(`vm/src/memory/native_roots.rs:4-63`). The stale prose at
`vm/src/memory/roots.rs:889` and `vm/src/memory/gc.rs:1084` that pointed at the
deleted functions is updated.

Two new inventory tests: `every_root_source_pairs_two_distinct_halves` (a row
that names the same function twice would scan on the remap path, and the type
system cannot catch it) and
`instrument_transformer_chain_is_a_registered_root_source`.

### 4. `runtime/interpreter/invoke.rs` — the two thread-local `ClassId` caches are VM-keyed

`LAMBDA_IMPL_BYTECODE_CACHE` and `TDIGEST_DOUBLE_GET_FIELD_CACHE` were keyed
`(u32 proxy ClassId, u32 receiver ClassId)`. Key widened to
`VmScopedClassPairKey = (vm_identity, u32, u32)`
(`vm/src/runtime/interpreter/invoke.rs:3228`), with the two key-construction
sites updated.

Round 1 rated these "low severity, listed for completeness". One detail raises
that: **the `RedefineGate` does not save you.** The gate's staleness handle comes
from the class manager of whichever VM populated the entry, so consulted from
VM B it reports VM A's unchanged redefine generation and answers "fresh". VM B
would then dispatch VM A's cached method body, or read VM A's cached *field
index* against a VM B receiver — a wrong-slot heap read, not merely a stale
lookup. It still needs one OS thread running bytecode in two VMs (`SharedVm`-per-
test on one thread, or `AttachCurrentThread` to a second `Vm`), which is why the
severity is "needs an unusual embedding", not "benign".

**No direct regression test.** Reproducing the collision needs two VMs with
colliding `ClassId`s *and* a populated `lambda_proxies` call site *and* a
receiver class whose `get(I)D` body matches the exact 7-byte accessor shape —
a fixture out of proportion to a three-line key widening. The premise the fix
rests on (`vm_identity` is distinct per `SharedVm`) is already covered by
`vm_identity_is_unique_per_shared_vm` (`vm/src/vm/vm_init.rs`).

---

## Still open

### `runtime/jvmti.rs:3096` `FIELD_WATCHPOINTS` — DO NOT fix in isolation

Round 1's recipe ("`(vm_identity, class_id, field_index)` key, keep the global
active flag as a conservative fast path") is mechanically right and would have
been a small change. **It is the wrong change**, and this is the correction:
re-keying this one map buys no isolation, because everything downstream of it is
still process-global.

`vm/src/runtime/jvmti.rs` has exactly four statics, and two of them are the
problem:

* `:2737` `GLOBAL_MANAGER: OnceLock<Arc<JvmtiEventManager>>` — the single event
  manager holding every registered callback, every `any_*_listener` fast-path
  flag, and the event counters. Event **delivery** is process-global.
* `:2800` `REAL_AGENT_ENV_BRIDGE: OnceLock<Weak<SharedVm>>` — one VM, full stop.

A watchpoint hit in VM B would be delivered to the agent's callbacks through
`GLOBAL_MANAGER` regardless of how the watchpoint map is keyed. Scoping
`FIELD_WATCHPOINTS` alone would produce a subsystem that *looks* isolated in
review and is not — worse than leaving it visibly global.

**Correct scope for the next pass:** move the whole JVMTI environment per VM —
`GLOBAL_MANAGER` and `REAL_AGENT_ENV_BRIDGE` first, `FIELD_WATCHPOINTS` and
`FIELD_WATCHPOINTS_ACTIVE` as a consequence. That is a JVMTI-environment
lifecycle change (JVMTI's own model is one `jvmtiEnv` per agent per VM), not a
re-keying, and it is the same shape as the `PROCESS_VM` item below. Note also
that `FIELD_WATCHPOINTS` holds no `ObjectRef` and no heap address: this is a
per-VM leak producing spurious/missed JVMTI events, never memory unsafety.

### `threading/thread_state.rs:521` `CELLS` — per-VM leak, no heap pointers

Unchanged from round 1 and correctly rated. `OnceLock<RwLock<Vec<Arc<ThreadStateCell>>>>`
where `ThreadStateCell` is `{ thread_id: AtomicU64, state: AtomicU8 }` — no
`ObjectRef`, no heap address, and lifetime is already correct (the `CellHandle`
TLS destructor at `:539` retains the registry on OS-thread teardown, so it stays
O(live threads)). The only defect is that a per-VM thread census counts other
VMs' threads.

Fix when someone needs a correct multi-VM census: add a `vm_identity: AtomicU64`
field to `ThreadStateCell`, stamp it in `bind_current_thread`, and filter in the
census walk. Deliberately not done here — it changes the STW holdout census
path, which is load-bearing for hang triage, for a diagnostics-only benefit.

### `runtime/ec_watch.rs` — debug-gated, dead by default

Unchanged from round 1. Gated behind `CRATONVM_DBG_ECWATCH` (`:36`), so it is
dead in every default build. Key it on the heap if the watchpoint is ever
un-gated.

### `native/jni.rs:323` `PROCESS_VM` — out of scope, unchanged

Round 1's framing is correct and was re-verified: `jni_attach_current_thread`
takes a `_vm: JavaVM` and ignores it (`vm/src/native/jni.rs:6717-6722` at round-1
line numbers), so the fix is keying on the `JavaVM*` invocation-table pointer the
caller actually passes — an invocation-API change, not a re-keying.

### `vm/src/jit/**`

Untouched; owned by other agents this wave. Round 1's assessment stands: several
`ClassId`-keyed inline caches with no VM key, where a wrong-VM hit is a
miscompile-grade fault rather than a stale entry. Audit next.

---

## Standing rules this round establishes

1. **A root source with no VM parameter is not a root source.** There is exactly
   one registry (`native_roots::VM_ROOT_SOURCES`), every row receives the owning
   `SharedVm`, and a row cannot compile without both halves. Do not reintroduce
   a VM-agnostic fan-out.
2. **A side table keyed on a raw heap address must be swept, not rooted** — and
   if the authoritative datum is already in the object, delete the table
   instead. But *verify* the authoritative read first: the `DIRECT_BUFFERS` case
   above is exactly the trap, where the "authoritative" field read was itself
   broken in real-JDK mode and the cache was silently carrying the path.
3. **A redefine/generation gate is not an identity check.** A cache entry
   validated by a `RedefineGate` snapshot is still wrong if the *key* can
   collide across VMs — the gate reads the populating VM's generation and says
   "fresh".
4. **Per-VM state needs a teardown call site.** `release_vm_native_state`
   (`vm/src/vm/vm_init.rs:7288`) is the single hook; anything newly keyed on
   `vm_identity` belongs in it.
