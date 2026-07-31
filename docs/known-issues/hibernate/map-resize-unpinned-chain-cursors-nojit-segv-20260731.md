# Old-generation header corruption kills `DefaultCatalogAndSchemaTest` under `--nojit`

| | |
|---|---|
| **Status** | 🟠 **NARROWED, still OPEN.** Two of the three tangled defects are fixed and the crash has moved twice; the class still SIGSEGVs. The residual is old-gen header corruption of unidentified origin. |
| **ID** | `HIB-MAPRESIZE-STALE.1` |
| **Found** | 2026-07-31, validating the `DefaultCatalogAndSchemaTest` runner accommodation ([`../../internal/fixed-suite-bugs/hibernate/qualfiedtablenaming-runner-timeout-floor-lost-20260731-FIXED.md`](../../internal/fixed-suite-bugs/hibernate/qualfiedtablenaming-runner-timeout-floor-lost-20260731-FIXED.md)). |
| **Repro** | [`probes/hib-mapresize-repro-20260731.sh`](../../../probes/hib-mapresize-repro-20260731.sh) — `org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest`, `--nojit`, `--Xmx 1500m`, real JDK, `-Dcraton.batch=1`. |
| **HotSpot control** | `found=132 started=132 ok=132 failed=0`, 132 s, same class and classpath (re-measured 2026-07-31). |

The title and the whole of this doc's first two revisions named
`map_resize_inner`. That attribution is **retracted** — see "The original
diagnosis was wrong" below. Three separate defects were tangled together here;
telling them apart is what finally produced progress.

## Measured arms

Same command, same host, same fixture.

| arm | tree | outcome | faulting RVA | corrupt-header warnings |
|---|---|---|---|---|
| A baseline | dev tip `32f9db9a2` | SIGSEGV rc=139 @ **2103 s** | `0x166AC79`, read past the exe image | 2 |
| B collections fix | A + `fix/hib-mapresize-put-stale-20260731` | SIGSEGV rc=139 @ **2312 s** | `0x16707D9`, same site | 22 |
| C merged | B + `origin/dev` (incl. `22107d512`) | SIGSEGV rc=139 @ **2611 s** | `0x2B55C3`, reading a **heap** address | 48 |

A and B die in the same place for the same reason (defect 2). C survives that,
logs 48 corrupt headers, re-syncs, runs five minutes longer, and then dies
somewhere else entirely — dereferencing a heap address, not a module one. The
corruption (defect 3) is the residual.

## The original diagnosis was wrong, which is why the first fix changed nothing

Revisions 1–2 blamed `map_resize_inner`'s **reference-typed stores**: each
`set_field`/`set_array_element` of a reference supposedly "can allocate a
remembered-set entry through the write barrier and therefore complete a moving
young GC". `codex/fix-map-resize-stale-cursors-20260731` landed a ~50-line pin
refactor on that premise, merged, and the SIGSEGV survived unchanged.

It survived because the premise is false. `GenerationalHeap::write_barrier`
(`gc/src/gen_heap.rs`) bails out on a non-reference or non-cross-generational
store after two address-range comparisons, and when it does record an edge it
calls `card_table.thread_local_dirty_addr(src)` — a push into a per-mutator Rust
buffer. **It never allocates from the Java heap, so a plain reference store
cannot collect.** A `[SETFIELD-GC]` epoch probe left in
`native_map_put_evict_pinned` had already established exactly this and was
overruled by the comment three lines above it; `ca0ea4299` on `dev` repeated the
same false premise a day later.

The GC points inside a native are only **allocation** (`alloc_ref_array`,
`alloc_synthetic`, `alloc_object`, `alloc_bucket_table`,
`new_object_initialized`, `new_array`) and **Java dispatch** (`map_hash_key` →
the key's `hashCode()`, `map_keys_equal` → its `equals()`, any `invoke_virtual`,
and the GC-safe blocking monitor wait). `map_resize_inner`'s split walk contains
neither below its `alloc_ref_array`, so that refactor was, as measured, inert.
It is harmless and has been kept.

## Defect 1 — collection natives held heap refs across their own allocations — FIXED

A native that captures a heap reference in a bare Rust local, calls something
GC-capable, and then reuses the local is addressing a **pre-move** location.
`safe_native_call` pins a native's object arguments and the collector remaps that
pin — but nothing rewrites the native's own copy of the address. Worse, a
reference reachable **only** from such a local — a table just allocated and not
yet stored, a chain segment the walk has unlinked — is not a root at all, so a
collection there **reclaims** it. That is the zeroed receiver behind

```
gen_heap::set_field: out-of-bounds field write dropped
  index=3 num_slots=0 class_id=ClassId(0) class_name=java/lang/Object
```

(`index=3` is both `HashMap$Node.next` and `LinkedHashMap$Node.next`), and a
write through such a reference is one plausible supplier of defect 3's invalid
old-gen headers.

**Localised** with [`regression-suite/src/RMapGcStress.java`](../../../regression-suite/src/RMapGcStress.java):
colliding keys whose `equals`/`hashCode` **allocate**, so every native chain walk
is guaranteed to span a collection. On dev's tip it fails in ~90 seconds,
deterministically, at n=100 as well as n=600:

```
$ CRATONVM_DBG_GC_STRESS=262144 cratonvm --nojit --Xmx 256m RMapGcStress 300
AssertionError: ConcurrentHashMap/filled: containsKey false for 0
```

`RMapResizeGc`, added by the earlier attempt, deliberately uses an
allocation-free key, so it can never produce the callback half of this bug class
— it stayed green throughout, a **false null**. It had also never been wired into
`regression-suite/run.sh`'s `CORE_CLASSES`, so a plain suite run had never
executed it at all. Both are in the default set now.

**Fixed** on `fix/hib-mapresize-put-stale-20260731`: `rooted_across` /
`rooted_across1` make the shape safe by construction, and ~40 sites were
converted — CHM `containsKey`/`equals`/`newKeySet`; `lhm_resize`,
`native_lhm_clear`, `native_lhm_entry_set`, `native_lhm_compute_if_absent`; the
HashMap / HashSet / ArrayList / ArrayDeque / PriorityQueue /
LinkedBlockingQueue / ConcurrentSkipListMap / COWAL / TreeSet init, clear, grow,
`toArray` and iterator paths; `Map.Entry`'s `setValue`/`hashCode`/`equals`; and
the LinkedList snapshot family. `native_chm_contains_key` had **no pinning at
all** across `chm_key_hash` while `native_chm_get` immediately above it pinned
and re-read — a `containsKey` that reported ABSENT for a key the map held.

`native_map_init`, `native_map_init_capacity`, `alloc_hs_backing`,
`native_hs_init(_capacity)` and `native_hs_init_from_collection` are the same
defect, found independently and in parallel on `dev` (`04396f738`, `ca0ea4299`);
those versions are taken verbatim at the merge.

Guarded by `native-collections/tests/gc_relocation_collection_stores.rs` (mock
harness with `set_relocate_pins_on_alloc(true)`; 3 of its 8 cases fail against
dev's `lib.rs`) and by `probes/audit-unpinned-across-gc.py`.

## Defect 2 — the SIGSEGV in arms A and B was the diagnostic itself — FIXED on `dev`

Both pre-merge arms fault at the same RVA, reading ~1.4 MB past the end of the
exe image, from a repeating five-frame `core::fmt` cycle, immediately after a
burst of `old-gen mark: rejecting object … implausible extent` warnings. That is
`warn_non_object_kind_in_object_arm` `Debug`-formatting `header.kind`.
`ObjectKind` is `#[repr(u8)]` with discriminants `0..=2`; the derived `Debug`
indexes a static variant-name table by discriminant, so an out-of-range byte
reads a `&str` from past the end of that table and the formatter walks a wild
pointer. Fixed on `dev` by `22107d512`
([retired report](../../internal/fixed-suite-bugs/gc-corrupt-header-diagnostic-debug-formats-invalid-enum-sigsegv-FIXED.md)),
inherited here by merge. That commit deliberately did not answer *why* such a
header exists — which is defect 3.

## Defect 3 — old-gen headers acquire garbage — OPEN, this is the residual

Arm C logs 48 of these before dying:

```
GC: header kind=0x3a reached the legacy-object sizing arm (shape=0, class_id=2044330296)
old-gen mark: rejecting object at 0x23a79da0120 with implausible extent 0 (kind=58, array_len=0, num_slots=0)
```

`kind=0x3a` is ASCII `':'`, and it repeats across distinct objects — the header
bytes look like **text written over an old-generation object header**, not a
relocated or zeroed one. (Arm B's kinds were varied garbage; arm C's are
consistently `0x3a`. One run settles nothing here — see the variance caution.)

The one concrete lead: with `CRATONVM_DBG_STALE_OBJREF=1
CRATONVM_DBG_STALE_OBJREF_CYCLES=4` the canary hard-panics at 1952 s on a genuine
stale `ObjectRef` — `NO heap holder found — the stale copy lived only in a
frame/register/native local` — in a native invoked from

```
org/hibernate/boot/registry/classloading/internal/ClassLoaderServiceImpl
    .classForName(Ljava/lang/String;)Ljava/lang/Class;
```

i.e. **defect 1's shape again, in the class-loading natives** rather than the
collection ones. [`regression-suite/src/RForNameGcStress.java`](../../../regression-suite/src/RForNameGcStress.java)
targets the obvious candidate — `native_class_for_name`'s
`invoke_virtual(loader, "loadClass", …)`, driven by a loader whose `loadClass`
allocates so the dispatch is guaranteed to span a collection — and it **passes**,
so the stale local is elsewhere on that path.

### Next steps

1. Symbolized build (`RUSTFLAGS="-Cdebuginfo=2 -Cforce-frame-pointers=yes"`,
   separate `CARGO_TARGET_DIR`) + the canary run, to name the frame. The release
   binary carries no symbols and every frame in the canary's backtrace prints
   `<unknown>`.
2. `CRATONVM_DBG_HEAP_STALE=1` — the deep post-GC verifier walks every live
   object and reports the **referrer** class and field index of any dangling
   reference, which names the missing barrier/root edge rather than a stack. It
   is the documented proven method for this bug class.
3. Audit `native-builtins`' class-loading natives with
   `probes/audit-unpinned-across-gc.py`, the way `native-collections` was.

### Cautions for whoever picks this up

- **Run-to-run variance is large.** Drop counts across runs of this reproducer
  have been 0, 91, 174, 375 and 606; corrupt-header counts 2, 22, 48; crash times
  2103–2611 s. A single run settles nothing subtle. The 2103 → 2611 s progression
  is consistent with real progress but is not, on its own, proof of it.
- **`gc young-gen actual: 0 moving cycle(s)` in the crash report does not mean no
  young GC ran.** `record_moving_young_cycle` is only called when
  `moving_young && has_conservative_roots` — a moving cycle with a live JIT
  frame. Under `--nojit` there are none, so it always reads 0 no matter how many
  collections happened.
- **Do not close this doc on the strength of a fix that has not run the class to
  completion.** That mistake has now been made twice.

## Related

- `docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md` — defect
  1's bug class, found from the other end.
- `HIB-WEAKREF-RECYCLE.1` — a different corrupt writer in the same runs (post-GC
  reference processing); fixed separately, removed 84 bad writes per run, and did
  not stop this one.
- [`gc-overhead-limit-spurious-oom-at-half-full-heap-20260731.md`](gc-overhead-limit-spurious-oom-at-half-full-heap-20260731.md)
  — the same class's JIT-on failure mode.
