# Process-global mutable state in `vm/src/` — census, fixes, and what remains

**Status: 🟡 SUPERSEDED 2026-08-01 — read
[`vm-process-global-state-round-2.md`](vm-process-global-state-round-2.md)
first.** Round 2 closed both items this document leaves "open — dangerous"
(the `ClassFileTransformer` chain and `DIRECT_BUFFERS`) plus two of the per-VM
leaks, and deleted the VM-agnostic `register_native_root_source` registry
entirely. It also **corrects** two recommendations made below: the
`DIRECT_BUFFERS` recipe ("the object-field read is correct on its own") is wrong
in real-JDK mode, and the `FIELD_WATCHPOINTS` recipe should not be applied in
isolation. Everything here that is still accurate: the census, the three-category
taxonomy, and the two fixes described under "Fixed in this change".

This is the `vm/src/` half of the C2 review's P0 "remove remaining
process-global runtime state"; the `native-builtins/` half is the
`SECURITY_STATE` fix in `native-builtins/src/security_manager.rs`, which is the
template followed here.

## The three categories

A process-global `static` is only a bug if what it holds differs between VMs,
or if it holds a pointer into a heap. Everything below is classified as:

* **Benign** — process-wide and immutable after init (env-var gate caches,
  build-mode flags), a pure diagnostic counter, or a cache whose key is
  globally unique (a content hash, an `Arc` allocation identity, a raw
  process-unique pointer).
* **Per-VM leak** — holds state that differs between VMs: a policy, a
  registry, or a cache keyed on something that is only unique *within* a VM
  (`ClassId`, loader id, a JVMTI class/field id). Two VMs in one process see
  each other's entries. This breaks sequential embedding, not just
  concurrency — a VM created after the first one is torn down is enough.
* **Dangerous** — holds an `ObjectRef` or a raw heap address. Two failure
  modes stack on top of the isolation bug:
  * not registered as a GC root source ⇒ use-after-free;
  * registered with a scan half but no remap half (or a remap driven by the
    *wrong* VM) ⇒ stale from-space pointer after a moving collection.

## Census (as of 2026-08-01, `vm/src/` only)

| measure | count |
| --- | --- |
| `static` items matched under `vm/src/` (incl. `#[cfg(test)]`) | 467 |
| …of which sit inside a `thread_local!` block | 67 |
| `thread_local!` blocks | 58 |
| statics whose initializer reads an env var / `flags()` (benign gate caches) | 95 |
| statics holding a container (`HashMap`/`HashSet`/`Vec`) — the accumulating ones | 72 |
| statics with `Mutex`/`RwLock` at the top level | 12 |
| by shape | 180 atomic, 92 `OnceLock<scalar>`, 82 `Cell`/`RefCell`, 49 other `OnceLock`/`LazyLock`, 12 lock, 52 other |

Reproduce with `scripts`-free ripgrep:
`rg -n '^\s*(pub(\([^)]*\))?\s+)?static\s+[A-Z_0-9]+\s*:' vm/src/`.

### Category counts

* **Benign — the large majority (~400).** The 95 env/flag gate caches
  (`OnceLock<bool>` around `runtime_var_os`), ~60 diagnostic counters and
  debug ring buffers, and the 67 thread-local cells. Also genuinely benign and
  worth naming so nobody re-audits them:
  * `runtime/frame.rs` `padded_code_cache` — keyed on a content hash of the
    code bytes, so a cross-VM hit returns the same bytes.
  * `runtime/local_liveness.rs` `cache` — keyed on the `Arc<[u8]>` allocation
    identity and validated with `Weak::upgrade` + pointer equality, so a
    recycled address cannot produce a stale hit.
  * `runtime/stackwalker.rs` `method_slot_memo` — keyed on
    `(ClassId, signature_hash)`, which *is* per-VM, **but every hit is
    re-verified against the live `Class`** (`vm/src/runtime/stackwalker.rs:173-183`:
    the memoized index is only returned if `m.name == name && m.descriptor ==
    descriptor`). A cross-VM collision degrades to the authoritative scan. Not
    a bug; do not "fix" it.
  * `vm/src/vm/vm_exec.rs:12310` `unloaded_native_libraries` — already keyed on
    the `NativeRealm` address (`native_realm_key`), with a documented
    reuse argument.
  * `vm/src/runtime/interpreter.rs:378` `MEMO`, `vm/src/runtime/offload_jit_gate.rs:167`
    `GATE_CACHE`, `vm/src/vm/vm_exec.rs:1309`/`2673`, `vm/src/runtime/invokedynamic.rs:1284`
    `LAMBDA_SINGLETON_CACHE`, `vm/src/runtime/offload.rs:3504` `input_cache::CACHE` —
    all already carry `vm_identity` in the key.
* **Per-VM leak — 4 named below.**
* **Dangerous — 6 total: 2 fixed here, 2 already correct, 2 open.**

## Fixed in this change

### 1. `ObjectStreamClass` cache — process-global raw-pointer root registry (DANGEROUS)

`vm/src/runtime/serialization/oscache.rs`.

Every `OscCache` published a raw `*const RwLock<FxHashMap<ClassId, ObjectRef>>`
into a process-global `Mutex<Vec<SendPtr>>` on first insert, and hooked
`scan_osc_cache_roots` / `remap_osc_cache_refs` into
`memory::native_roots::register_native_root_source` — the **VM-agnostic**
fan-out, which takes no `SharedVm`. Consequences:

* every VM's root scan walked *every* live cache, so VM B's collector was
  handed descriptor addresses belonging to VM A's heap;
* every VM's post-move fixup rewrote *every* live cache's entries through its
  own relocation map, so VM B's forwarding table repointed VM A's descriptors.

The premise check that made this worth fixing: `OscCache::scan_roots` /
`OscCache::remap_roots` **already existed** (added by an earlier wave, with a
doc comment calling the global callback a "compatibility backstop") and had
**zero callers anywhere in the repo** — the process-global path was the only
live one, not a backstop.

**Fix.** Deleted the registry, `SendPtr`, `register_with_gc`, the two global
`fn`s, and the `Drop` impl that existed only to deregister the raw pointer
before the map was freed. `shared.classes.osc_cache.scan_roots/remap_roots` is
now driven from the VM-scoped inventory as `root_source!("osc-cache", …)` in
`vm/src/memory/native_roots.rs`. This also removes the module's `unsafe`
entirely — the registry's own doc recorded 5 SIGSEGVs and 2 permanent hangs in
1500 filtered runs from the use-after-free it created.

Tests (`vm/src/runtime/serialization/oscache.rs`):
`scan_does_not_leak_another_caches_descriptors` and
`remap_does_not_rewrite_another_caches_descriptors` both fail against the old
process-global fan-out and pass now.

**Side effect worth knowing:** `runtime/instrument.rs` is now the *only*
remaining caller of `register_native_root_source` in the whole repository. If
that one is converted too (see below), the VM-agnostic half of
`memory/native_roots.rs` can be deleted outright.

### 2. Long-smuggle mint registry — one set of raw heap addresses for all heaps (DANGEROUS)

`vm/src/memory/smuggled_longs.rs`.

`static SET: Mutex<Option<HashSet<u64>>>` held raw heap addresses that were
deliberately handed to Java as `long` bits. It is the *only* signal that lets
`ValueStack::update_object_refs` tell a genuine smuggled jobject handle (which
must be rewritten across a moving GC) from a primitive `long` whose value
happens to equal a moved object's from-space address (which must not be). One
flat set across all heaps was wrong in both directions:

* **Mints were silently unregistered by an unrelated VM.** `remap_and_sweep`
  drops any entry that is neither in the collector's pointer map nor still a
  live object *in the heap being collected*. VM B's collection therefore swept
  VM A's handles out — `is_object_address` on a foreign address always misses.
  When VM A later moved that object, the rewrite arm asked `is_minted`, got
  `false`, and left the Java-visible `long` pointing into vacated from-space.
  That is exactly the use-after-move the module exists to prevent.
* **Foreign mints were rewritten by the wrong pointer map.** A value minted
  against heap A that collides with an address in heap B's relocation map was
  rewritten to B's new address.

This was not hypothetical: `colliding_primitive_long_is_not_rewritten` in
`vm/src/runtime/value_stack.rs` carried an explicit
`if is_minted(old_addr) { return; }` bail-out commented "the registry is
process-global and another test's heap could have handed out an identical
address" — the test skipped itself on collision.

**Fix.** `static SETS: Mutex<Option<HashMap<HeapId, HashSet<u64>>>>`, keyed on
`heap_id(&VmHeap) = heap as *const VmHeap as usize`.

**Why the heap and not `vm_identity`.** Everything stored is a raw heap
address, and a heap address is only meaningful against the heap that produced
it; `shared.mem.heap` is a by-value field of `HeapRealm` inside
`Arc<SharedVm>`, so its address is stable for the VM's life and distinct from
every other *live* heap's — the two are in bijection. The decisive practical
reason is reachability: `ValueStack::update_object_refs(&mut self,
pointer_map, heap: &VmHeap)` (`vm/src/runtime/value_stack.rs:1456`) is where
`is_minted` is consulted, and it has a `&VmHeap` and nothing else. Threading
`vm_identity` there would cascade a new parameter through
`interpreter.rs:4535`, `gc.rs:540`, `gc.rs:1762` and `vm_exec.rs:2233`/`3970`.

Call sites updated: `memory/gc.rs:388` (signature unchanged — it already
passed `&shared.mem.heap`), `native/jni.rs:3007`/`3895`/`4961`,
`runtime/value_stack.rs:1574-1575`, `jvmti/mod.rs:580`.

`VmLocalVariableProvider` gained a `heap: Option<&'a VmHeap>` field because
`JvmThread` carries no route to its `SharedVm`. **Note:** that struct is
currently constructed only by its own unit tests
(`vm/src/jvmti/mod.rs:975`/`1011` — the sole constructors in the repo), so the
JVMTI `GetLocal*` mint chokepoint is dead code today. Any production wiring
MUST pass `Some(&shared.mem.heap)` or that mint will not be registered.

Tests (`vm/src/memory/smuggled_longs.rs`):
`mint_is_not_visible_to_another_heap`,
`foreign_collection_does_not_sweep_away_our_mint`,
`foreign_pointer_map_does_not_rewrite_our_mint`,
`own_sweep_relocates_and_drops`. The `value_stack.rs` bail-out is now a hard
assertion that a fresh heap starts with an empty mint table.

**Documented residual:** an allocator that reuses a dropped `VmHeap`'s address
for a new `VmHeap` inherits the dead heap's table. The new heap's first
`remap_and_sweep` clears it, and until then a stale entry can only matter for
a value that also appears in the new heap's pointer map. `forget_heap(&VmHeap)`
is provided for a teardown path that wants to reclaim eagerly; nothing calls
it yet.

## Open — dangerous

### `runtime/instrument.rs` — `ClassFileTransformer` chain (DANGEROUS, isolation only)

`vm/src/runtime/instrument.rs:101` `transformer_chain(): &'static RwLock<Vec<TransformerEntry>>`,
where `TransformerEntry::transformer_ref` is a live Java `ObjectRef`.

**Root-source status: both halves present and correctly paired.**
`scan_transformer_roots` (`:133`) and `remap_transformer_refs` (`:155`) are
registered together at `:177` via `ensure_transformer_root_source_registered`,
so *within one VM* this is neither a UAF nor a stale pointer. The bug is purely
isolation, on two axes:

1. **GC.** Registration goes through the VM-agnostic
   `register_native_root_source`, so VM B's collection scans and remaps VM A's
   transformer refs — reporting one heap's addresses to another heap's
   collector and rewriting one VM's entries through another VM's relocation
   map. Identical to the oscache bug fixed above.
2. **Semantics.** A `ClassFileTransformer` installed by an agent in VM A is
   applied to classes loaded in VM B. The module doc asserts "The Java spec
   mandates a single chain per JVM" — correct, and *per JVM* is exactly the
   scope this static does not have.

**Recipe (unchanged from the oscache fix).** Key the chain on `vm_identity`,
add a `vm: usize` parameter to `add_transformer_entry`,
`remove_transformer_entry`, `snapshot_transformer_chain`,
`reset_transformer_chain` and the internal `transformer_chain()` accessor, and
move the pair into `VM_ROOT_SOURCES` as
`root_source!("class-file-transformers", …)`. Everything is contained in
`instrument.rs` — there are no callers of these functions outside that file —
but it touches ~6 production sites and ~20 test sites, which is why it was
left out of this change rather than done blind without a build.

### `native/jni.rs` — `DIRECT_BUFFERS` keyed on a raw heap address (DANGEROUS)

`vm/src/native/jni.rs:6279`:

```rust
static DIRECT_BUFFERS: LazyLock<Mutex<HashMap<u64, (SendPtr, i64)>>>
```

The `u64` key is `obj_to_jobject(obj)` — the **raw heap address** of the
`DirectByteBuffer` object (`:6311`, `:6322`). This table is:

* never remapped after a moving collection,
* never swept,
* process-global (so two heaps' addresses alias),
* and consulted **before** the authoritative object-field read in both
  `jni_get_direct_buffer_address` (`:6336`) and
  `jni_get_direct_buffer_capacity` (`:6357`).

The dangerous case needs no second VM: the buffer object moves or dies, its
old address is recycled for a different object, and a later
`GetDirectBufferAddress` on that new object hits the stale entry and returns
the *previous* buffer's `malloc` pointer. Native code then reads/writes
unrelated memory. Growth is unbounded for the same reason.

**Recipe.** The side table is a pure optimization — the fallback path reads
the address and capacity out of fields 0 and 1 of the object, which the
collector maintains correctly. The minimal safe change is to consult the
object fields **first** and treat the side table as the fallback (a live
object's field read always succeeds, so a stale entry can never win); the
complete change is to delete the table. Either wants a JNI-level test
(`NewDirectByteBuffer` → GC → `GetDirectBufferAddress`), which is why it is
documented rather than changed here — an untested edit to this path is a
regression risk for Netty/ES workloads.

### `runtime/ec_watch.rs` — watched `(ObjectRef, …)` table (DANGEROUS, debug-gated)

`vm/src/runtime/ec_watch.rs:56` `T: OnceLock<Mutex<Vec<(ObjectRef, u32, usize, u32)>>>`.
Process-global, holds `ObjectRef`s, and `remap` rewrites them with whichever
VM's pointer map is collecting. Entirely gated behind `CRATONVM_DBG_ECWATCH`
(`:36`), so it is dead in every default build. Key it on the heap if the
watchpoint is ever un-gated; otherwise leave it.

### Already correct (verified, listed so they are not re-audited)

* `runtime/invokedynamic.rs:1284` `LAMBDA_SINGLETON_CACHE` — key is
  `(vm_identity, ClassId, site_pc)`; both halves scoped in
  `native_roots.rs` as `"lambda-singletons"`.
* `runtime/offload.rs:3504` `input_cache::CACHE` — per-VM table keyed on
  `vm_identity`, swept via `memory::addr_keyed::remap_and_sweep` from
  `gc.rs:401`. (`gpu-offload` feature only.)

## Open — per-VM leak (no heap pointers)

* **`runtime/jvmti.rs:3096` `FIELD_WATCHPOINTS`** —
  `RwLock<Option<HashMap<(u64 class_id, usize field_index), FieldWatchpoint>>>`.
  `class_id` is per-VM, so a watchpoint set by an agent in VM A fires on an
  unrelated field of an unrelated class in VM B. The lock-free mirror
  `FIELD_WATCHPOINTS_ACTIVE` (`:3106`) is likewise process-wide, so any VM
  installing a watchpoint puts *every* VM's getfield/putfield on the slow
  path. Fix: `(vm_identity, class_id, field_index)` key, and make the active
  flag a per-VM field or accept the (conservative, correct) global fast path.
* **`runtime/instrument.rs`** — see the dangerous entry above; the semantic
  half is also a per-VM leak.
* **`threading/thread_state.rs:521` `CELLS`** —
  `OnceLock<RwLock<Vec<Arc<ThreadStateCell>>>>`, a process-global census of
  live shadow cells. Threads from every VM land in one list, so a per-VM
  thread-state census (and the STW holdout census that reads it) counts other
  VMs' threads. No heap pointers; `Arc`-managed, so no lifetime bug.
* **`runtime/interpreter/invoke.rs:3211`/`3222`**
  `LAMBDA_IMPL_BYTECODE_CACHE` and `TDIGEST_DOUBLE_GET_FIELD_CACHE` — thread-
  local `FxHashMap<(u32 class_id, u32), …>`. A thread normally belongs to one
  VM, so this is benign in practice; it becomes a leak for an OS thread that
  attaches to a second VM (JNI `AttachCurrentThread`) or a test thread reused
  across `SharedVm`s. Low severity, listed for completeness.

## Explicitly out of scope for this pass

* `native/jni.rs:323` `PROCESS_VM: Mutex<Option<Weak<SharedVm>>>` and `:355`
  `DESTROY_VM_HOOK`. These are the *same shape* as the `RedefineClasses`
  single-`Weak` bug an earlier wave fixed, and their doc comments explicitly
  assert "There is exactly one VM per process" (`:315-323`). That premise is
  false for the embedding cases this whole workstream is about — a second
  `JNI_CreateJavaVM` replaces the cell, and a foreign thread attaching through
  the *first* `JavaVM*` then resolves the *second* VM. Fixing it means keying
  on the `JavaVM*` invocation-table pointer the caller actually passes (which
  `jni_attach_current_thread` currently ignores), i.e. a real API change to
  the invocation path, not a re-keying. Named here so the next pass starts
  from the right framing.
* Everything under `vm/src/jit/` (`VIRTUAL_TARGET_CACHE`,
  `JIT_TYPECHECK_TARGET_CACHE`, `JIT_SUBTYPE_POSITIVE_CACHE`,
  `DISPATCH_CACHE`, `VIRTUAL_DISPATCH_CACHE`, `CONCURRENT_HASHMAP_CLASS_CACHE`,
  `JIT_ENTRY_CHAIN`, `RANGE_SNAPSHOT`, …). Several are `ClassId`-keyed inline
  caches with no VM key and are strong per-VM-leak candidates — an inline
  cache that resolves a `ClassId` to a target in the wrong VM is a
  miscompile-grade fault, not just a stale entry. Left alone because the JIT
  lane was owned by other agents this wave; audit it next.
