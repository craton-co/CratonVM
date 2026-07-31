# FIXED — collection natives held heap references across their own allocations (`--nojit` SIGSEGV)

| | |
|---|---|
| **Status** | ✅ **FIXED** on `fix/hib-mapresize-put-stale-20260731`. The reproducer that segfaulted at 35 min now completes; a new deterministic reproducer that failed in ~90 s now passes. |
| **ID** | `HIB-MAPRESIZE-STALE.1` |
| **Found** | 2026-07-31, validating the `DefaultCatalogAndSchemaTest` runner accommodation ([`qualfiedtablenaming-runner-timeout-floor-lost-20260731-FIXED.md`](qualfiedtablenaming-runner-timeout-floor-lost-20260731-FIXED.md)). |
| **Repro** | `org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest` under `--nojit`, `--Xmx 1500m`, real JDK. SIGSEGV (rc=139), reproduced 4/4. |

## The original diagnosis was wrong, which is why the first fix changed nothing

This doc's first two revisions blamed `map_resize_inner`'s **reference-typed
stores**: the claim was that each `set_field`/`set_array_element` of a reference
"can allocate a remembered-set entry through the write barrier and therefore
complete a moving young GC". `codex/fix-map-resize-stale-cursors-20260731`
landed a ~50-line pin refactor on that premise, merged, and the SIGSEGV
survived unchanged.

It survived because the premise is false. `GenerationalHeap::write_barrier`
(`gc/src/gen_heap.rs`) bails out on a non-reference or non-cross-generational
store after two address-range comparisons, and when it does record an edge it
calls `card_table.thread_local_dirty_addr(src)` — a push into a per-mutator
Rust buffer. **It never allocates from the Java heap, so a plain reference
store cannot collect.** The `[SETFIELD-GC]` epoch probe another author left
behind in `native_map_put_evict_pinned` had already established exactly this
and been overruled by the comment three lines above it.

The GC points inside these natives are the ordinary two: **allocation**
(`alloc_ref_array`, `alloc_synthetic`, `alloc_object`, `new_object_initialized`,
`alloc_bucket_table`) and **Java dispatch** (`map_hash_key` → the key's
`hashCode()`, `map_keys_equal` → its `equals()`, any `invoke_virtual`, and the
GC-safe blocking monitor wait). `map_resize_inner`'s split walk contains
*neither* below its `alloc_ref_array` — so the landed refactor was, as measured,
inert. It is harmless and has been kept.

## Actual root cause

A native that captures a heap reference in a bare Rust local, calls something
GC-capable, and then reuses the local is addressing a **pre-move** location.
`safe_native_call` pins a native's object arguments, and the collector remaps
that pin — but nothing rewrites the native's own copy of the address.

The second-order effect is the destructive one. A reference that is reachable
**only** from such a local — a table that has just been allocated and not yet
stored anywhere, a chain segment the walk has already unlinked — is not a GC
root at all. A collection there does not merely move it; it **reclaims it**.
That is what produces the zeroed receiver behind

```
gen_heap::set_field: out-of-bounds field write dropped
  index=3 num_slots=0 class_id=ClassId(0) class_name=java/lang/Object
```

(`num_slots=0` with `class_id=0` is an all-zero header, not a relocated one),
and why the crash is *delayed*: the map keeps a dangling chain head, every later
`put`/`get` walks freed nodes, and eventually one dereference lands on memory
that is no longer mapped.

`index=3` is both `HashMap$Node.next` and `LinkedHashMap$Node.next`.

## How it was localised

`RMapResizeGc` (added by the earlier attempt) deliberately uses an
allocation-free key, so its `equals`/`hashCode` never collect under
`CRATONVM_DBG_GC_STRESS` — it cannot express the callback half of the bug class
at all, and stayed green throughout. The new
[`regression-suite/src/RMapGcStress.java`](../../../../regression-suite/src/RMapGcStress.java)
uses colliding keys whose `equals` and `hashCode` **allocate**, so every native
chain walk is guaranteed to span a collection, and exercises head/middle/tail
removal, in-place update, tail-append and full traversal across `HashMap`,
`LinkedHashMap`, `ConcurrentHashMap`, `Hashtable` and `HashSet`.

On dev's tip it fails in ~90 seconds, deterministically, at n=100 as well as
n=600:

```
$ CRATONVM_DBG_GC_STRESS=262144 cratonvm --nojit --Xmx 256m RMapGcStress 300
AssertionError: ConcurrentHashMap/filled: containsKey false for 0
```

and with `CRATONVM_DBG_STALE_OBJREF=1` the canary hard-panics in a native called
straight from `RMapGcStress.verify`:

```
CRATONVM_DBG_STALE_OBJREF: stale ObjectRef detected at 0x1c5ac000000 — this object
was evacuated by a moving GC to 0x1c5fa9e0000 … NO heap holder found — the stale
copy lived only in a frame/register/native local
```

The first confirmed site was `native_chm_contains_key`, which had **no pinning
at all** across `chm_key_hash` while every sibling CHM entry point — including
`native_chm_get` immediately above it — pinned and re-read. A `containsKey` that
reports ABSENT for a key the map holds is a silent wrong answer, not a crash;
it is the same defect *shape* as the crashing ones, and finding it is what made
the whole class visible.

## Fix

`native-collections/src/lib.rs` gains `rooted_across` / `rooted_across1`, which
root a set of references across a GC-capable call and rewrite each in place
afterwards, so the shape is safe by construction instead of re-fixed per site:

```rust
let (this, new_table) = rooted_across1(ctx, this, |ctx| alloc_ref_array(ctx, new_cap));
```

Converted sites (each was a live defect, not a hardening pass):

| area | sites |
|---|---|
| CHM | `native_chm_contains_key`, `native_chm_equals`, `native_chm_new_key_set` |
| LinkedHashMap | `lhm_resize`, `native_lhm_clear`, `native_lhm_entry_set`, `native_lhm_compute_if_absent` |
| HashMap / view backings | `native_map_init`, `native_map_init_capacity`, `native_map_copy_of`, `native_map_entry`, `alloc_view_backing`, `tm_make_entry` |
| HashSet | `native_hs_init`, `native_hs_init_capacity`, `alloc_hs_backing`, `native_hs_clear`, `native_hs_contains`, `native_hs_contains_all`, `native_hs_to_array`, `native_hs_to_array_typed` |
| `Map.Entry` | `native_entry_set_value`, `native_entry_hash_code`, `native_entry_equals` |
| list / deque / queue / skiplist | `native_al_init`, `native_al_clear`, `native_al_trim_to_size`, `native_ad_init`, `ad_ensure_capacity`, `native_pq_init`, `native_pq_init_comparator`, `pq_ensure_capacity`, `native_lbq_init`, `native_lbq_init_cap`, `lbq_ensure_capacity`, `native_cslm_init`, `native_cslm_init_comparator`, `cslm_ensure_capacity`, `cowal_ensure_lock_and_array` |
| TreeSet / executor | `native_ts_clear`, `native_ts_iterator`, `native_ts_descending_iterator`, `native_stpe_init` |

`lhm_resize` is the one that matches the crash signature exactly: it read the
insertion-order `head` through the receiver's **pre-allocation** address, walked
whatever occupied that address as a node chain, wrote `LinkedHashMap$Node.next`
(slot 3) through every "node" it found, and finally published those addresses
into the new bucket array. Its two siblings, `lhm_init_with_cap` and
`map_resize_inner`, had both already been given this treatment; `lhm_resize` and
`native_lhm_clear` were simply missed.

## Verification

| arm | binary | outcome |
|---|---|---|
| baseline (dev tip `32f9db9a2`) | `cratonvm-mapstale-A-baseline-20260731.exe` | **SIGSEGV rc=139 @ 2103 s**, 0 dropped writes logged |
| fixed | `cratonvm-mapstale-B-fix-20260731.exe` | see the run summary at the bottom of this file |

- `RMapGcStress`: fails on baseline in ~90 s; passes on the fix at
  `CRATONVM_DBG_GC_STRESS=65536`, n=400, **both** `--nojit` and JIT-on.
- `regression-suite/run.sh`: **20 passed / 0 failed**, all HotSpot-diffed.
  `RMapResizeGc` and `RMapGcStress` are now in `CORE_CLASSES` — `RMapResizeGc`
  had been added to `src/` by the earlier attempt but never wired into the
  default set, so a plain suite run had never executed it.
- `cargo test -p cratonvm-native-collections`: 166 passed / 0 failed, including
  the new `tests/gc_relocation_collection_stores.rs`. That harness sets
  `MockCtx::set_relocate_pins_on_alloc(true)` — every allocation relocates every
  pinned root and unmaps the old address — and 3 of its 7 cases fail against
  dev's `lib.rs`. The mock now also feeds its pointer map to
  `gc_update_collection_overlay_refs`, matching what the real collectors do for
  the `lhm_overlay` / TreeMap / TreeSet side tables; without that a correctly
  rooted LinkedHashMap looks corrupt in the mock.

## Residual

`probes/audit-unpinned-across-gc.py` reports the functions in this file that
still hold a reference across a GC-capable call with no pin in sight. It is a
deliberately noisy heuristic — a local that only feeds the very call that
produced it is fine — and it still lists ~100 candidates, mostly in the
stream/collector/`CompletableFuture` natives that neither reproducer exercises.
They were **not** converted blind. Run it before adding a native that allocates:

```bash
python probes/audit-unpinned-across-gc.py native-collections/src/lib.rs
```

## Related

- `docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md` — the
  same bug class, found from the other end.
- `HIB-WEAKREF-RECYCLE.1` — a different corrupt writer in the same runs (post-GC
  reference processing); fixed separately, removed 84 bad writes per run, and
  did not stop this one.
- The `reference_native_fastpath_reimplements_library_method_semantics` family:
  a native that owns a library method's whole semantics also owns its GC-safety
  obligations.
