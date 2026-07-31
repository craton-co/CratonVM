# FIXED — collection natives held heap references across their own allocations; the SIGSEGV on top was a corrupt-header diagnostic

| | |
|---|---|
| **Status** | ✅ **FIXED.** Two independent defects, both landed. |
| **ID** | `HIB-MAPRESIZE-STALE.1` |
| **Found** | 2026-07-31, validating the `DefaultCatalogAndSchemaTest` runner accommodation ([`qualfiedtablenaming-runner-timeout-floor-lost-20260731-FIXED.md`](qualfiedtablenaming-runner-timeout-floor-lost-20260731-FIXED.md)). |
| **Repro** | `org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest` under `--nojit`, `--Xmx 1500m`, real JDK ([`probes/hib-mapresize-repro-20260731.sh`](../../../../probes/hib-mapresize-repro-20260731.sh)). |

Read the two halves separately — conflating them is what made this doc wrong
twice.

1. **The map corruption** — a whole class of collection natives held heap
   references in bare Rust locals across their own allocations and Java
   callbacks. Fixed here.
2. **The SIGSEGV** — `warn_non_object_kind_in_object_arm` `Debug`-formatting an
   invalid `ObjectKind` discriminant and walking a wild pointer inside
   `core::fmt`. Fixed in parallel on `dev` by `22107d512`, inherited by this
   branch through a merge. It was never a collections bug.

## The original diagnosis was wrong, which is why the first fix changed nothing

This doc's first two revisions blamed `map_resize_inner`'s **reference-typed
stores**: the claim was that each `set_field`/`set_array_element` of a reference
"can allocate a remembered-set entry through the write barrier and therefore
complete a moving young GC". `codex/fix-map-resize-stale-cursors-20260731`
landed a ~50-line pin refactor on that premise, merged, and the SIGSEGV survived
unchanged.

It survived because the premise is false. `GenerationalHeap::write_barrier`
(`gc/src/gen_heap.rs`) bails out on a non-reference or non-cross-generational
store after two address-range comparisons, and when it does record an edge it
calls `card_table.thread_local_dirty_addr(src)` — a push into a per-mutator Rust
buffer. **It never allocates from the Java heap, so a plain reference store
cannot collect.** The `[SETFIELD-GC]` epoch probe another author left behind in
`native_map_put_evict_pinned` had already established exactly this and been
overruled by the comment three lines above it.

The GC points inside these natives are the ordinary two: **allocation**
(`alloc_ref_array`, `alloc_synthetic`, `alloc_object`, `new_object_initialized`,
`alloc_bucket_table`) and **Java dispatch** (`map_hash_key` → the key's
`hashCode()`, `map_keys_equal` → its `equals()`, any `invoke_virtual`, and the
GC-safe blocking monitor wait). `map_resize_inner`'s split walk contains
*neither* below its `alloc_ref_array` — so the landed refactor was, as measured,
inert. It is harmless and has been kept.

## Defect 1 — root cause

A native that captures a heap reference in a bare Rust local, calls something
GC-capable, and then reuses the local is addressing a **pre-move** location.
`safe_native_call` pins a native's object arguments and the collector remaps that
pin — but nothing rewrites the native's own copy of the address.

The second-order effect is the destructive one. A reference reachable **only**
from such a local — a table just allocated and not yet stored anywhere, a chain
segment the walk has already unlinked — is not a GC root at all. A collection
there does not merely move it; it **reclaims it**. That is what produces the
zeroed receiver behind

```
gen_heap::set_field: out-of-bounds field write dropped
  index=3 num_slots=0 class_id=ClassId(0) class_name=java/lang/Object
```

(`num_slots=0` with `class_id=0` is an all-zero header, not a relocated one), and
a subsequent write through such a reference is one way an **old-generation object
acquires an invalid header** — the precondition for defect 2's diagnostic to fire
at all. `index=3` is both `HashMap$Node.next` and `LinkedHashMap$Node.next`.

## How defect 1 was localised

`RMapResizeGc` (added by the earlier attempt) deliberately uses an
allocation-free key, so its `equals`/`hashCode` never collect under
`CRATONVM_DBG_GC_STRESS` — it cannot express the callback half of the bug class,
and stayed green throughout. The new
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

The first confirmed site was `native_chm_contains_key`, which had **no pinning at
all** across `chm_key_hash`, while every sibling CHM entry point — including
`native_chm_get` immediately above it — pinned and re-read. A `containsKey` that
reports ABSENT for a key the map holds is a silent wrong answer, not a crash; it
is the same defect *shape* as the destructive ones, and finding it is what made
the class visible.

## Defect 1 — fix

`native-collections/src/lib.rs` gains `rooted_across` / `rooted_across1`, which
root a set of references across a GC-capable call and rewrite each in place
afterwards, so the shape is safe by construction rather than re-fixed per site:

```rust
let (this, new_table) = rooted_across1(ctx, this, |ctx| alloc_ref_array(ctx, new_cap));
```

Converted sites (each a live defect, not a hardening pass):

| area | sites |
|---|---|
| CHM | `native_chm_contains_key`, `native_chm_equals`, `native_chm_new_key_set` |
| LinkedHashMap | `lhm_resize`, `native_lhm_clear`, `native_lhm_entry_set`, `native_lhm_compute_if_absent` |
| HashMap / view backings | `native_map_copy_of`, `native_map_entry`, `alloc_view_backing`, `tm_make_entry` |
| HashSet | `native_hs_clear`, `native_hs_contains`, `native_hs_contains_all`, `native_hs_to_array`, `native_hs_to_array_typed` |
| `Map.Entry` | `native_entry_set_value`, `native_entry_hash_code`, `native_entry_equals` |
| list / deque / queue / skiplist | `native_al_init`, `native_al_clear`, `native_al_trim_to_size`, `native_ad_init`, `ad_ensure_capacity`, `native_ad_to_array`, `native_pq_init(_comparator)`, `pq_ensure_capacity`, `native_pq_to_array`, `native_pq_iterator`, `native_lbq_init(_cap)`, `lbq_ensure_capacity`, `native_lbq_to_array`, `native_lbq_iterator`, `native_cslm_init(_comparator)`, `cslm_ensure_capacity`, `cowal_ensure_lock_and_array` |
| LinkedList | `ll_snapshot_array`, `native_ll_iterator`, `native_ll_list_iterator(_idx)`, `native_ll_spliterator`, `native_ll_to_array(_typed)` |
| TreeSet / executor | `native_ts_clear`, `native_ts_iterator`, `native_ts_descending_iterator`, `native_stpe_init` |

`lhm_resize` is the one that matches the dropped-write signature exactly: it read
the insertion-order `head` through the receiver's **pre-allocation** address,
walked whatever occupied that address as a node chain, wrote
`LinkedHashMap$Node.next` (slot 3) through every "node" it found, and finally
published those addresses into the new bucket array. Its two siblings,
`lhm_init_with_cap` and `map_resize_inner`, had both already been given this
treatment; `lhm_resize` and `native_lhm_clear` were simply missed.

`native_map_init`, `native_map_init_capacity`, `alloc_hs_backing`,
`native_hs_init`, `native_hs_init_capacity` and `native_hs_init_from_collection`
are the same defect, found independently and in parallel on `dev`
(`04396f738` + `ca0ea4299`); their versions are taken verbatim at the merge.
Their commentary justified holding the pins across the reference STORES on the
same false write-barrier premise; the comments are corrected in place and the
extra re-reads kept.

## Defect 2 — the SIGSEGV itself

Both pre-merge arms crash at the *same* RVA, reading an address ~1.4 MB past the
end of the exe image, from a repeating five-frame cycle, immediately after a
burst of

```
old-gen mark: rejecting object at 0x17d01fd6010 with implausible extent 0 …
GC: header kind=RO<garbage> reached the legacy-object sizing arm …
```

That is `warn_non_object_kind_in_object_arm` `Debug`-formatting `header.kind`.
`ObjectKind` is `#[repr(u8)]` with discriminants `0..=2`; the derived `Debug`
indexes a static variant-name table by discriminant, so an out-of-range byte
reads a `&str` from past the end of that table and the formatter walks a wild
pointer. **The function whose whole job is to report a corrupt header was what
turned a recoverable detection into a process kill.** Fixed on `dev` by
`22107d512` (with its own retired report,
[`gc-corrupt-header-diagnostic-debug-formats-invalid-enum-sigsegv-FIXED.md`](../gc-corrupt-header-diagnostic-debug-formats-invalid-enum-sigsegv-FIXED.md)),
which landed after this branch's base and is inherited through the merge.

Two things worth carrying forward from that:

- The report's `gc young-gen actual: 0 moving cycle(s)` line is **not** evidence
  that no young collection ran. `record_moving_young_cycle` is only called when
  `moving_young && has_conservative_roots` — i.e. a moving young cycle with a
  live JIT frame. Under `--nojit` there are none, so the counter reads 0 no
  matter how many collections happened. Do not read it as this doc's earlier
  revisions would have.
- `22107d512` deliberately did not answer *why* an invalid-kind header is
  reachable by the old-gen scan. Defect 1 is one supplier of them; whether it is
  the only one is open.

## Verification

| arm | binary | tree | outcome |
|---|---|---|---|
| A baseline | `cratonvm-mapstale-A-baseline-20260731.exe` | dev tip `32f9db9a2` | **SIGSEGV rc=139 @ 2103 s**, 0 dropped writes logged |
| B defect-1 fix only | `cratonvm-mapstale-B-fix-20260731.exe` | A + this branch's collections fixes | **SIGSEGV rc=139 @ 2312 s** — same faulting RVA, still missing `22107d512` |
| C merged | `cratonvm-mapstale-C-merged-20260731.exe` | B + `origin/dev` (incl. `22107d512`) | *(filled in below)* |

- HotSpot control, same class and classpath: `found=132 started=132 ok=132
  failed=0`, 132 s.
- `RMapGcStress`: fails on baseline in ~90 s; passes on the fix at
  `CRATONVM_DBG_GC_STRESS=65536`, n=400, **both** `--nojit` and JIT-on.
- `regression-suite/run.sh`: **20 passed / 0 failed**, all HotSpot-diffed.
  `RMapResizeGc` and `RMapGcStress` are now in `CORE_CLASSES` — `RMapResizeGc`
  had been added to `src/` by the earlier attempt but never wired into the
  default set, so a plain suite run had never executed it.
- `cargo test -p cratonvm-native-collections`: 166 passed / 0 failed, including
  the new `tests/gc_relocation_collection_stores.rs`. That harness sets
  `MockCtx::set_relocate_pins_on_alloc(true)` — every allocation relocates every
  pinned root and unmaps the old address — and 3 of its 8 cases fail against
  dev's `lib.rs`. The mock now also feeds its pointer map to
  `gc_update_collection_overlay_refs`, matching what the real collectors do for
  the `lhm_overlay` / TreeMap / TreeSet side tables; without that a correctly
  rooted LinkedHashMap looks corrupt in the mock.

## Residual

`probes/audit-unpinned-across-gc.py` reports the functions in this file that
still hold a reference across a GC-capable call with no pin in sight. It is a
deliberately noisy heuristic — a local that only feeds the very call that
produced it is fine — and it still lists ~90 candidates, mostly in the
stream/collector/`CompletableFuture` natives that neither reproducer exercises.
They were **not** converted blind. Run it before adding a native that allocates:

```bash
python probes/audit-unpinned-across-gc.py native-collections/src/lib.rs
```

## Related

- `docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md` — the
  same bug class, found from the other end.
- `HIB-WEAKREF-RECYCLE.1` — a different corrupt writer in the same runs (post-GC
  reference processing); fixed separately, removed 84 bad writes per run, and did
  not stop this one.
- The `reference_native_fastpath_reimplements_library_method_semantics` family: a
  native that owns a library method's whole semantics also owns its GC-safety
  obligations.
