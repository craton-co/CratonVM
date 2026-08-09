# Native collections & IO — GC-root and object-identity audit

> Status as of 2026-08-01, branch `feat/c2-review-remediation`.
> Scope: `native-collections/` and `native-io/` — every place Rust-side state
> stands in for a Java object and therefore holds an `ObjectRef` or a raw heap
> address.

Every such holder owes the collector two things, and this branch has repeatedly
found holders with one and not the other:

* **scan** — the collector must find the reference, or the object is freed
  while still reachable from Rust;
* **remap** — the collector must rewrite the reference after a moving
  collection, or Rust reads a pre-copy address.

There is a third obligation that is *not* satisfied by rooting: **sweep**.
A side table keyed by an object's identity must drop the entry when the object
dies. Rooting the key instead is the wrong fix — it pins dead collections
forever, which is exactly what the `lhm_heap_backed` skip in
`for_each_overlay_ref` (`native-collections/src/lib.rs:34501`) exists to undo
after unbounded pinning exhausted the young gen.

## What the premise got right and wrong

The lane was scoped as if these two crates were unaudited. Most of
`native-collections` is the opposite: `for_each_overlay_ref`
(`native-collections/src/lib.rs:34417`) is a single funnel that both GC hooks
share precisely so a new table cannot be added to one and forgotten in the
other, `gc_prune_dead_collection_overlays` (`:34870`) sweeps dead keys, and
`gc_relocation_harness.rs` asserts each table is reached. Three of the four
comparator-driven binary searches had already been converted to pin-and-re-read
(`tm_binary_search` `:35147`, `ts_binary_search` `:35220`, `pbq_offer_locked`
`:48243`, the last of those dated 2026-07-31).

Two things the premise got right, both of them *holdouts from a partial fix*:

* the **fourth** binary search (`cslm_binary_search`) was never converted;
* `native-io`'s async completion path had been converted for **one field on one
  struct** (`Job::Write`'s buffer, 2026-07-26 audit) and left the handler on the
  same struct, plus three sibling job variants, bare.

Two things the premise got wrong, both found by checking registration before
writing code:

* the `ConcurrentSkipListMap` natives are **not registered** — the real JDK
  bytecode runs (`native-collections/src/lib.rs:1760`, guarded by the
  `concurrent_skip_list_map_not_intercepted` test at `:52196`). The defect was
  real code but unreachable.
* `native-io`'s `Job::Read` was **never constructed** — the handler-form read
  had been rerouted to `Job::ReadFd`, which roots everything correctly.

## Census

`scanned?` = reachable from a GC root provider. `remapped?` = rewritten after a
moving collection. `swept?` = entry dropped when its owner dies.

### `native-collections/` — overlay side tables (all keyed by `widened_obj_key`)

| Holder | What it holds | Scanned? | Remapped? | Swept? | Action |
|---|---|---|---|---|---|
| `hm_int_fast_shards` `:1286` | canonical key object + value per `HashMap<Integer,?>` entry | yes (`:34452`) | yes | yes (`:34936`) | none |
| `ll_overlay` `:27717` | LinkedList head/tail `Node` refs | yes (`:34469`) | yes | yes (`:34971`) | none |
| `lhm_overlay` `:29227` | LHM `table`/`head`/`tail` | yes, *conditionally* (`:34501`) | yes | yes (`:34959`) | none — the skip is deliberate; see "Open" |
| `lhm_heap_backed` `:29254` | keys whose refs are mirrored into real heap fields | n/a (no refs) | n/a | yes (`:34965`) | none |
| `lhm_ptr_cache` `:29389` | raw addr → packed key | n/a (no refs) | n/a | yes, by value (`:35008`) | none |
| `tm_array_table` `:34358` | TreeMap `data` array + comparator | yes (`:34533`) | yes | yes (`:34977`) | none |
| `tm_fast_table` `:33822` | authoritative BTreeMap values | yes (`:34547`) | yes | yes (`:34983`) | none |
| `ts_array_table` `:34400` | TreeSet `data` array + comparator | yes (`:34558`) | yes | yes (`:34989`) | none |
| `cslm_comparator_table` `:47131` | CSLM custom comparator | yes (`:34573`) | yes | yes (`:34997`) | none (table is fine; the *search* was not — see below) |
| **`tm_force_array_set`** `:34229` | sticky "array mode" flag | n/a (no refs) | n/a | **NO** | **FIXED — swept now** |
| `obj_key_registry` shards `:538` | `last_ptr` address markers | n/a | yes (`:34812`) | yes (`:34909`) | none |
| **`overlay_owner_keys`** `:551` | owner addr → overlay keys | n/a | yes — but it was **not single-step** | yes (`:35013`) | **FIXED 2026-08-01 — rebuilt, not relocated in place; see below** |
| `HM_INT_FAST_LAST_KEY` (TLS) `:1311` | (raw ptr, identity hash, key) memo | n/a | n/a | validated, not swept | none — see note |

`overlay_owner_keys` is the row this audit's own criteria could not catch, and
it is worth saying why: the three columns ask whether a remap **exists**, not
whether it is **correct**. One did exist, and it was wrong.

It relocated entries in place, one move at a time. A `pointer_map` may hold
both `A -> B` and `B -> C` — old-gen sliding compaction hands one live object
the address another live object just vacated — so applying `A -> B` first
parked A's keys at B, and the later `B -> C` swept them onward with B's own.
The collection that really was at B ended up with no entry at its own address,
`gc_overlay_roots_for_collection(B)` answered "owns nothing", and the
non-moving young marker and `old_gen_gc`'s mark BFS both skipped its backing
array. `HashMap` iteration order decided whether it fired. Every *other* remap
in the file is a single-step lookup, which is what `pointer_map` means: the
collector composes `young -> promoted -> compacted` chains into one hop before
handing it over.

Pinned by
`overlay_owner_liveness_tests::a_chained_pointer_map_does_not_sweep_one_owners_keys_onto_another`,
verified to FAIL on the old algorithm. Full write-up in
`fixed-suite-bugs/springboot/springboot-basicerrorcontroller-checkcast-abort-20260731-FIXED.md`.

**For the next pass over this table:** a `yes` in `Remapped?` should mean the
transformation is single-step and order-independent, not merely that a remap
function is wired. The only other address-keyed holder here is
`obj_key_registry`'s `last_ptr`, which visits each slot once and is therefore
already single-step.

`HM_INT_FAST_LAST_KEY` deserves its row explained because it is the shape this
branch has previously got wrong. It is an **address-keyed cache**, and the
correct treatment is neither rooting nor remapping: the memo is validated
against the identity hash as well as the pointer (`:1339`), so a recycled
address cannot resolve to the dead map's key. Rooting the address would pin a
dead map; remapping it would need a per-thread hook the collector does not have.
Validation is the right answer and it is already there.

`tm_force_array_set` is the row that changed. It is the only
`widened_obj_key`-keyed table `gc_prune_dead_collection_overlays` did not
clear. It holds no `ObjectRef`, so this was never a use-after-free — but the
prune's own comment (`:34952`) states the rule it broke: a leftover entry under
a reclaimed object's key is "both a leak and — once that 32-bit identity hash is
recycled by a new same-class object — stale state the newcomer could alias
onto". Concretely: a fresh `TreeMap` inheriting the flag starts life pinned to
array mode, silently forgoing fast mode for its whole lifetime. Fixed by adding
it to the sweep (`:34988`), not by rooting anything.

### `native-io/` — async completion path

| Holder | What it holds | Scanned? | Remapped? | Swept? | Action |
|---|---|---|---|---|---|
| `Completion.handler` / `.attachment` | `CompletionHandler` + attachment, parked across a blocking syscall and a queue hop | **NO** | **NO** | n/a | **FIXED — `HandlerRoots` (global roots)** |
| `Job::Connect.handler/.attachment` | ditto, across `connect(2)` (30 s policy timeout) | **NO** | **NO** | n/a | **FIXED** |
| `Job::Connect.channel` | the `AsynchronousSocketChannel` whose `F_CONNECTED` a failed connect resets | **NO** | **NO** | n/a | **FIXED — `channel_gref`** |
| `Job::Accept.handler/.attachment` | ditto, across `accept(2)` — *unbounded* wait on an idle server | **NO** | **NO** | n/a | **FIXED** |
| `Job::Write.handler/.attachment` | ditto, across `write(2)` | **NO** | **NO** | n/a | **FIXED** |
| `Job::Write.bb_gref` | source `ByteBuffer` | yes | yes | released on delivery | none (fixed 2026-07-26) |
| `PendingFieldReset.target` | channel to reset, applied on another thread | **NO** | **NO** | n/a | **FIXED — `target_gref`** |
| `PendingArrayWrite.arr` | destination `byte[]` of an async read | **NO** | **NO** | n/a | **REMOVED with its only (dead) producer** |
| `Job::ReadFd` / `ReadCompletion` / `FutureCompletion` `*_gref` | handler, attachment, buffer, future | yes | yes | released on delivery | none — this is the correct pattern the rest now follows |
| `PendingRootRelease.gref` | parked buffer root | n/a (a handle) | n/a | released on flush | none |
| `TEMPORARY_BUFFERS` (TLS) `direct_buffer.rs:1062` | pooled `DirectByteBuffer`, held as `add_global_root` and keyed by `vm_identity` | yes | yes | released on eviction | none — correct, and a good model |
| `unsafe_allocs` / `generations` / `freed_addrs` `direct_buffer.rs:826,903,944` | **off-heap** addresses only | n/a | n/a | generation-pruned | none — not Java heap |
| `MMAP_REGISTRY`, `FILE_LOCKS`, `net`/`nio_selector`/`pipe` registries | fds, ids, OS handles | n/a | n/a | id-keyed | none — no `ObjectRef` |

The pattern across the whole `native-io` block is one thing: the file already
contained the correct design (`ReadCompletion`, `FutureCompletion`,
`Job::ReadFd` — every Java reference a `usize` global-root handle, resolved on
the delivering thread) *and* an older path that had never been converted. The
comment above the one field that had been converted even names the hazard —
"a moving collection can run while the worker is parked in `write(2)`" — and
then ships the `CompletionHandler` on the same struct as a bare `ObjectRef`.

## Bytecode-visibility findings

The question is whether an object mutated through a native shim and through
ordinary Java bytecode sees the same state.

* **LinkedList / LinkedHashMap.** The overlay is *mirrored* into the real JDK
  heap fields when the layout permits (`lhm_set`, tracked by
  `lhm_heap_backed`), and read-through prefers the real slot. Divergence is
  therefore bounded to objects whose real fields could not be resolved. This is
  the mechanism the whole `lhm_heap_backed` root-skip rests on; see "Open".
* **TreeMap / TreeSet.** State lives only in the side table, but `TreeMap`'s
  real-bytecode `readObject` path is explicitly reconciled by replaying the
  rebuilt red-black tree into the fast-mode BTreeMap (`:33880` doc comment) —
  i.e. the one place real bytecode writes the object, the native side is
  resynchronised. Not symmetric in general, but the asymmetry is handled where
  it is reachable.
* **ConcurrentSkipListMap.** The comparator exists *only* in
  `cslm_comparator_table` — there is no object slot for it, because the
  synthetic class declares three fields. This would be a genuine invisibility
  bug if the natives were live. They are not (see above), so the real JDK class
  and its own `comparator` field are what actually run. No action; the
  divergence is unreachable.
* **`native-io` async channels.** All state is fd/id-keyed and reached only
  through natives; there is no ordinary-bytecode writer to diverge from.

No new refusal was needed: nothing was found writing a natively-backed value
that ordinary bytecode could observe as stale.

## Atomicity findings

* `Collections.synchronizedMap/List/Set/Collection` — **already correct.** They
  construct the real `java/util/Collections$Synchronized*` wrapper
  (`native-collections/src/lib.rs:44671-44735`), so every method is
  `synchronized (mutex)`. The identity-stub version named in the brief was
  fixed earlier; `native_collections_identity` survives but no longer backs
  these four names (`:44538-44558`).
* `ConcurrentSkipListMap` — striped `parking_lot::RwLock` per map
  (`cslm_stripe_for` `:47098`), write lock on `put`/`remove`, read lock on
  `get`/`size`/`isEmpty`/`containsKey`. Genuinely serialised. Two caveats, both
  moot while the natives stay unregistered, both recorded under "Open".
* `ConcurrentHashMap` segment locks (`SEG_LOCKS` `:2139`) and the CHM resize
  epochs (`:2158`, `:2188`) are real locks, not stubs.
* `native-io`'s `AtomicI32`-generated ids (`net.rs:761`, `pipe.rs:70`,
  `async_socket.rs:79`) are monotonic counters, not user-visible `Atomic*`
  classes — no atomicity claim is being made to a caller.

Nothing in these two crates was found *claiming* atomicity it did not provide.
The `Atomic*` array natives named in the brief live outside this lane
(`native-builtins`) and were fixed in an earlier wave.

## `native-io` buffer state

* **position/limit/mark.** `dbb_rel_addr` (`direct_buffer.rs:1362`) refuses
  rather than guesses: any field that does not read back as the modelled shape
  returns `None` and the caller bails to real bytecode, which runs the JDK's own
  `nextGetIndex()`. The comment at `:1355` records the deliberate decision to
  commit `position` only *after* a successful access, because the bytecode it
  bails to bumps `position` itself — committing first would double-advance.
  Correct as written.
* **Address caching across a GC point.** `dbb_elem_addr` / `dbb_rel_addr` read
  `address` out of the buffer and use it within the same call, with no
  intervening allocation. No cached address survives a GC point on this path.
* **`copy_to_native_memory` vs. raw `memcpy`.** The one raw `copy_nonoverlapping`
  on a worker thread lived in the dead `Job::Read` arm and went with it.
* Mark/reset aliasing between `duplicate()`/`slice()` views: **not re-audited**
  this pass — see "Open".

## Changes made

`native-collections/src/lib.rs`
* `cslm_binary_search` (`:47221`) rewritten to pin and re-read `owner`, `keys`,
  `values`, `comparator` and the searched `key` across every `tree_compare`
  dispatch, taking them as `&mut` so a caller cannot keep a pre-search copy.
  Modelled on `tm_binary_search`. Comparison orientation preserved exactly
  (CSLM compares `(existing, searched)`; TreeMap compares the other way and a
  non-antisymmetric comparator can tell).
* New `cslm_arrays` returns the possibly-relocated receiver alongside the lazily
  created backing arrays.
* `native_cslm_put` additionally pins the inserted `value` across the search and
  pins `this`/`key`/`value` across `cslm_ensure_capacity`'s two allocations —
  without which a growth-triggered collection dropped the `CSLM_FIELD_SIZE`
  write and stored dangling refs.
* `native_cslm_get` / `remove` / `containsKey` updated to the new signature.
* `tm_force_array_set` added to `gc_prune_dead_collection_overlays`; its
  accessors switched to poison-recovering locks to match the GC funnel.
* Test hooks: `__test_cslm_{init_comparator,put,get,size}` (the natives are
  unregistered, so this is the only way to cover them),
  `__test_tm_{force_array_mode,set_force_array,force_array_len}`.

`native-io/src/async_socket.rs`
* New `HandlerRoots` (handler + attachment as global-root handles) with a strict
  ownership rule: exactly one of `push_handler_completion` or
  `queue_handler_release` must be reached on every path.
* `Completion`, `Job::Connect`, `Job::Write`, `Job::Accept` converted;
  `PendingFieldReset` now carries `target_gref` plus a `release_root` marker so
  the connect-failure path's two resets share one root and free it once.
* Roots are taken at *enqueue* time (the only place with a `NativeContext`) and
  released on every failure path, including the send-failed paths that
  previously had nothing to release.
* `aio_asc_connect` additionally roots before `decode_addr`, which dispatches
  `getPort()`/`getHostString()` on the `SocketAddress` — a pre-existing GC point
  ahead of the first use of `this`/`handler`/`attachment`.
* `drain_completions` resolves the handler and attachment through their roots
  immediately before each `invoke`, re-resolving after the `Integer`/`IOException`
  allocations, and releases both exactly once per completion. The `failed` path
  now allocates the message `String` before the exception object, so the
  exception reference is not held across an allocation.
* Removed the unreachable `Job::Read` arm and the `PendingArrayWrite` table it
  was the sole producer for. `flush_pending_array_writes_inner` is retained as a
  documented no-op so its four drain sites keep their shape.

`native-io/src/test_support.rs`
* `MockNativeContext::invoke` now records interface-dispatch calls (that is the
  entry point `CompletionHandler.completed`/`failed` go through).
* New `relocate_global_root(handle, new_obj)` — models the collector's remap of
  the global-root table, which is what makes "is it remapped?" assertable.

## Tests

`native-collections/tests/gc_side_table_root_audit.rs` (new)
* `cslm_put_survives_a_comparator_triggered_relocation` — pins the receiver and
  both backing arrays (as the VM's `safe_native_call` does), relocates them from
  inside the comparator dispatch, and asserts the size write and the inserted
  entry land on the post-move objects. Against the old code the `size` write
  went to a dead address and the map stayed at 2.
* `cslm_existing_entries_survive_a_relocating_put` — the shift/insert loop must
  not empty the array through a stale `keys`.
* `dead_treemap_force_array_flag_is_swept` — asserts on the table's entry
  **count**, not on a presence check. Pruning also drops the dead object's
  registry slot, so re-deriving its key afterwards mints a fresh generation and
  a presence check reads `false` whether or not the sweep happened. This is the
  difference between a test and a test that passes for the wrong reason.

`native-io/src/async_socket.rs` unit tests (new)
* `audit_completion_handler_is_remapped_before_delivery`
* `audit_failed_delivery_is_remapped_and_releases_roots`
* `audit_handler_less_job_releases_its_attachment_root`
* `audit_pending_field_reset_targets_the_relocated_channel` — asserts both that
  the write lands on the new address *and* that it does not land on the old one.
* `audit_two_field_resets_share_one_channel_root`

Each relocates the root between park and delivery; the pre-fix bare-`ObjectRef`
shape delivers the pre-move address and fails.

## Open

1. **CSLM holds a stripe write lock across arbitrary Java.**
   `native_cslm_put`/`remove` take `cslm_stripe_for(...).write()` and then run
   `cslm_binary_search`, which dispatches a user comparator. `parking_lot`'s
   `RwLock` is not reentrant, so a comparator that touches the same map — or any
   map colliding on one of the 256 stripes — self-deadlocks. Not fixed: the
   natives are unregistered, and narrowing the guard would forfeit the
   atomicity the stripes were added for. Must be resolved before re-enabling.
   *Recipe*: a comparator whose `compare` calls `map.get` on the same map, run
   under a build that registers the CSLM natives; it hangs in `write()`.
2. **`cslm_comparator` is read before the stripe lock is taken** (`:47541`), so
   a concurrent `(Comparator)` re-init could be missed. Same gating.
3. **`lhm_overlay` is the one table the root scan conditionally skips.** Its
   premise — that a heap-backed LHM's `head`/`tail`/`table` stay alive through
   mirrored real fields — is the one the post-GC audit still reports dangling
   refs from (600 `ZEROED(reclaimed)` reports over 68 addresses in one
   `DefaultCatalogAndSchemaTest` run, per the comment at `:34490`). Not touched
   here; it needs the `CRATONVM_LHM_ROOT_ALL=1` A/B lever run to completion, not
   an argument.
4. **`dbb_elem_fields` is a process-global `OnceLock` of field indices**
   (`direct_buffer.rs:1247`), resolved once against `java/nio/DirectByteBuffer`
   for the whole process. Every other per-VM cache in this file is keyed by
   `ctx.vm_identity()` (see `TEMPORARY_BUFFERS` at `:1062`). Two VMs in one
   process with different `DirectByteBuffer` layouts — real-JDK vs synthetic —
   would have the second read `address` from the first's slot index and write to
   an arbitrary native address. Not fixed: it needs a low-overhead VM-scoped
   memo on a per-byte hot path, and the divergence needs to be demonstrated
   before paying for it. *Recipe*: boot two VMs in one process, one with
   `--features synthetic-jdk`, and compare
   `resolve_field_index("java/nio/DirectByteBuffer", "address")` in each.
5. **`duplicate()`/`slice()` view aliasing** was not re-audited. The known
   `ByteBuffer` mark/reset address-aliasing defect was fixed in an earlier wave
   (`docs/known-issues/bytebuffer-mark-reset-address-aliasing-fixed.md`); this
   pass confirmed the *absolute and relative single-byte* accessors are sound
   and did not extend to the bulk/view surface.
6. **`Job::Read` was deleted rather than converted.** If a registry-backed
   handler-form read is ever restored, it must root its references the way
   `Job::ReadFd` does. The removal comment at the `Job` enum records this.

## Deliberately not done

* No change to `for_each_overlay_ref`'s table list — every table it should
  reach, it reaches; the gap was in the *prune*, not the scan.
* No change to `vm/` or `gc/`. Nothing in this lane needed one: the fixes use
  the existing `pin_native_root`/`read_native_pin`/`unpin_native_roots` and
  `add_global_root`/`resolve_global_root`/`remove_global_root` contracts.
* The fd/id-keyed registries across `net.rs`, `nio_selector.rs`, `pipe.rs`,
  `file_channel.rs`, `watch.rs` and `process.rs` were censused and hold no
  `ObjectRef` or heap address. They are out of the collector's concern and were
  not disturbed.
