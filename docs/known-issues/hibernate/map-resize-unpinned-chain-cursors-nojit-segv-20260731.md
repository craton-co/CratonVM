# The in-place old-gen sweep frees LIVE promoted objects — `DefaultCatalogAndSchemaTest` still short of HotSpot

| | |
|---|---|
| **Status** | 🟠 **Four defects fixed; a residual, still-unattributed OLD-GEN HEADER CORRUPTION remains and is the last thing blocking retirement.** `found=99..128` against HotSpot's `132`; the gap is OPEN and needs a per-test comparison rather than a `found` count. Defect 4 — the in-place old-gen sweep returning a LIVE promoted object's block to the free list — is landed and attributed (`20cab92aa`). 2026-08-03 (see Follow-up 4 below): on top of dev's tip plus the defect-5 diagnosis, `CRATONVM_DBG_STALE_OBJREF` needed two of its own bugs fixed (it was crashing on the very corruption it exists to report) before it could survive long enough to attribute anything — with both fixed it reproducibly names a native reached from `TestPlan.getTestIdentifier`, with "NO heap holder found" both times, which **rules out a Family-1 unpinned-native-local bug** (every map-get native on that path was re-audited and is already correctly pinned) and confirms this is genuine header corruption, not a pinning gap. `CRATONVM_NO_OLDGEN_COALESCE=1` did **not** prevent the crash (it changed the failure from SIGSEGV to SIGILL inside `OldGen::compact()` itself), which is a real negative result against the coalescer hypothesis for THIS lane specifically — under `--nojit` there are no JIT frames, so `old_gen_gc` almost always takes the **compacting** path, not the in-place-sweep-plus-coalesce path the coalescer hypothesis was built on. An intermittent SIGSEGV remains that is present with every fix here reverted and whose rate **tracks HOST LOAD** (~1 in 3 on a quiet box, ~4 in 5 under heavy concurrent load) — i.e. timing- or concurrency-sensitive. **Four earlier framings are RETRACTED: the `UN-FORWARDED` collector hypothesis (a verifier artefact), `map_resize_inner` (a false premise about write barriers), the arm table under defect 4 (an unpinned, load-sensitive reproducer), and — as of 2026-08-03 — the coalescer as the `--nojit` lane's mechanism (see Follow-up 4).** |
| **ID** | `HIB-MAPRESIZE-STALE.1` |
| **Found** | 2026-07-31, validating the `DefaultCatalogAndSchemaTest` runner accommodation ([`../../internal/fixed-suite-bugs/hibernate/qualfiedtablenaming-runner-timeout-floor-lost-20260731-FIXED.md`](../../internal/fixed-suite-bugs/hibernate/qualfiedtablenaming-runner-timeout-floor-lost-20260731-FIXED.md)). |
| **Repro** | [`probes/hib-mapresize-repro-20260731.sh`](../../../probes/hib-mapresize-repro-20260731.sh) — `org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest`, `--nojit`, `--Xmx 1500m`, real JDK, `-Dcraton.batch=1`. |
| **HotSpot control** | `found=132 started=132 ok=132 failed=0`, 132 s, same class and classpath (re-measured 2026-07-31). |

The title and the whole of this doc's first two revisions named
`map_resize_inner`. That attribution is **retracted** — see "The original
diagnosis was wrong" below. Four separate defects were tangled together here;
telling them apart is what finally produced progress.

## Measured arms

Same command, same host, same fixture.

| arm | tree | outcome | faulting RVA | corrupt-header warnings |
|---|---|---|---|---|
| A baseline | dev tip `32f9db9a2` | SIGSEGV rc=139 @ **2103 s** | `0x166AC79`, read past the exe image | 2 |
| B collections fix | A + `fix/hib-mapresize-put-stale-20260731` | SIGSEGV rc=139 @ **2312 s** | `0x16707D9`, same site | 22 |
| C merged | B + `origin/dev` (incl. `22107d512`) | SIGSEGV rc=139 @ **2611 s** | `0x2B55C3`, reading a **heap** address | 48 |
| **F** | C + `origin/dev` (incl. **`c3dbb011a`**) | **rc=0 @ 5110 s**, `found=99 started=97 ok=97 failed=0` | — | 0 |
| F again (`CRATONVM_DBG_HEAP_STALE=1`) | same binary | **rc=0 @ 4724 s**, `found=99 started=96 ok=96 failed=0` | — | 0 |

A and B die in the same place for the same reason (defect 2). C survives that,
logs 48 corrupt headers, re-syncs, runs five minutes longer, and then dies
somewhere else entirely — dereferencing a heap address, not a module one.

**F does not crash at all.** `c3dbb011a`'s mark-worklist validation is what stops
it: the symbolized frame (below) is `scan_object_for_old_refs` on an unvalidated
worklist address, which is exactly what that commit screens.

But F is **not** a pass. HotSpot gets `found=132 started=132 ok=132`; F gets
`found=99 started=97 ok=97`.

**That shortfall is the same corruption, not a separate discovery bug.** The
count comes from `SummaryGeneratingListener`, which reads the `TestPlan`; and the
`TestPlan` is losing entries out of its own map at run time:

```
org.junit.platform.commons.PreconditionViolationException: No TestIdentifier with
unique ID [[engine:junit-jupiter]/[class-template:…DefaultCatalogAndSchemaTest]]
has been added to this TestPlan.
    at org.junit.platform.launcher.TestPlan.getTestIdentifier(TestPlan.java:204)
    at …ExecutionListenerAdapter.executionFinished(ExecutionListenerAdapter.java:57)
```

Those four exceptions are **interleaved, at the same millisecond, with a burst of
101 dropped out-of-bounds field READS** plus 6 dropped writes — the `num_slots=0`
(reclaimed-receiver) guard, at `index=5`/`4`/`1`, i.e. `LinkedHashMap$Node`'s
`after`/`before`/`key`. The reads start at log line 6244, the first TestPlan
exception at 7269, and they continue together to the end. A map whose nodes have
been reclaimed returns null for a key it holds; JUnit's `Preconditions.notNull`
turns that into the exception, and the summary undercounts.

So there is **one** defect left, not two: stop the premature frees and the
`found` count should follow.

So `c3dbb011a` removed the crash, not the corruption. The residual is now a
**silent wrong answer** — a third of the class's tests quietly vanish — which is
the worse failure mode of the two.

### Every one of those dropped accesses is a reclaimed object, not a bad index

The guard's own message says "class layout is correct; the bug is in the
caller's slot computation". For this workload that is **wrong**, and reading it
literally cost a round of investigation. Classifying all 107 events in F's log
by receiver:

```
$ grep "out-of-bounds field read dropped" mapstale-F-oldgenfix.log | …
     48 idx=0 ns=0 cid=0 cls=java/lang/Object
     28 idx=1 ns=0 cid=0 cls=java/lang/Object
     18 idx=3 ns=0 cid=0 cls=java/lang/Object
      3 idx=5 …   2 idx=8 …   1 idx=4 …   1 idx=2 …
```

**101 of 101 reads and 6 of 6 writes have `num_slots=0 class_id=0`** — a zeroed
header. There is not a single genuine wrong-index event in the run. Every one is
a dereference of an object that was **freed while still referenced**. The slot
index is whatever field the caller legitimately wanted; the receiver is gone.

The interpreter's own receiver check names three of the victims outright:

```
WARN …invoke: Stale pointer detected in invokevirtual receiver (ptr=0x1caa2c76270, all-zero header)
     — falling back to CP class org/junit/jupiter/engine/extension/MutableExtensionRegistry$Entry
WARN …invoke: … (ptr=0x1caa2bbb200, …) — org/junit/platform/engine/support/hierarchical/ThrowableCollector
WARN …invoke: … (ptr=0x1caa50c5190, …) — java/util/concurrent/RunnableScheduledFuture   (×278)
```

JUnit's own execution machinery, reclaimed underneath the run. That is the
`TestPlan` losing its entries.

**All of it — the 8 GC warnings below, the 107 dropped accesses, and all 281
stale-receiver fallbacks — falls in one three-second window,
`02:15:13.569` to `02:15:16.353`.** One collection did this.

Two independent F runs agree on `found=99` and on 6 drops, so the discovery gap
is stable, not run-to-run noise (which for this reproducer is otherwise large —
see the cautions). Both completed; neither crashed.

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
write through such a reference is one plausible supplier of defect 3a's invalid
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
header exists — which is defects 3a and 3b.

## Defect 3a — the old-gen mark walked unvalidated worklist addresses — FIXED on `dev`

Arm C logs 48 of these before dying:

```
GC: header kind=0x3a reached the legacy-object sizing arm (shape=0, class_id=2044330296)
old-gen mark: rejecting object at 0x23a79da0120 with implausible extent 0 (kind=58, array_len=0, num_slots=0)
```

`kind=0x3a` is ASCII `':'`, and it repeats across distinct objects — the header
bytes look like **text written over an old-generation object header**, not a
relocated or zeroed one. (Arm B's kinds were varied garbage; arm C's are
consistently `0x3a`. One run settles nothing here — see the variance caution.)

### RETRACTED: the `UN-FORWARDED` evidence was a verifier artefact

Revisions 3–4 of this doc built their whole open hypothesis on this, and it does
not hold. Read the next few paragraphs before reusing any `[heap-stale]` output.

`CRATONVM_DBG_HEAP_STALE=1` (the deep post-GC verifier: walks every live object
and classifies each reference field) fires on this reproducer. In one major
collection:

```
[heap-stale] UN-FORWARDED OBJ org/hibernate/metamodel/model/domain/internal/SingularAttributeImpl$NumericIdentifierAttributeImpl field[2] -> 0x23d68482738
[heap-stale] UN-FORWARDED OBJ org/hibernate/metamodel/model/domain/internal/ListAttributeImpl field[3] -> 0x23d684cd5c8
[heap-stale] UN-FORWARDED OBJ org/hibernate/metamodel/model/domain/internal/EntityTypeImpl field[19] -> 0x23d6847d6d8
[heap-stale] UN-FORWARDED OBJ org/hibernate/type/descriptor/sql/internal/DdlTypeImpl field[4] -> …
[heap-stale] UN-FORWARDED OBJ org/assertj/core/api/ObjectArrayAssert field[7] -> …
[heap-stale] UN-FORWARDED OBJ cratonvm/synthetic/AnonymousObject$4 field[2] -> …
[heap-stale] ^ 40 stale field(s) this GC (pointer_map size=3436289)
```

The reasoning was: `UN-FORWARDED` means the target address is still a KEY in the
collector's own `pointer_map`, so the object *was* moved and this referrer's
field was never rewritten — a missed referrer edge in the collector.

**That inference is invalid, because `verify_heap_object_fields` was missing the
recycled-destination filter its sibling `verify_no_stale_refs` documents at
length.** An address that is BOTH a key and a value in `pointer_map` was vacated
by one object and handed out again as the *destination* of another; a slot the
remap rewrote **correctly** then points at a map key and gets reported. On the
major-GC path that is not a corner case: `pointer_map` there is the composition
of the young map with `OldGen::compact`'s, and a **sliding** compactor moves
survivors down into space its predecessors just vacated, so key∩value overlap is
the normal case. With `pointer_map size≈3.4 M` the report cap of 40 says
essentially nothing.

The filter is added (`vm/src/memory/gc.rs`), so the instrument can be trusted
from here on. Everything derived from those 40 lines — including the
"17 distinct referrers, one un-forwarded target" argument and the
`update_refs_in_object` lead that used to close this doc — is withdrawn.

**The lesson is the one this codebase keeps re-learning: a diagnostic that
cannot distinguish its own false positives will confidently invent a defect.**
The real corruptor was in the log the whole time, in plain `WARN` lines.

### The faulting frame, symbolized

A build with `RUSTFLAGS="-Cdebuginfo=2 -Cforce-frame-pointers=yes"` (separate
`CARGO_TARGET_DIR`, so the ordinary `target/` cache is untouched) plus the
report's own offline mode resolves the crash. Note the symbolizer needs the
binary sitting **next to its `.pdb`** — copying just the `.exe` elsewhere yields
`<unresolved>` for every frame:

```
$ CRATONVM_SYMBOLIZE="0x2B64B3,0x2ADC6A,0x1048F01" target-dbg/release/cratonvm.exe X
0x2B64B3  cratonvm_gc::gen_heap::GenerationalHeap::old_gen_gc+0x12B3   [gen_heap.rs:8562]
0x2ADC6A  cratonvm_gc::gen_heap::GenerationalHeap::collect_garbage_inner  [gen_heap.rs:5412]
0x1048F01 cratonvm_vm::runtime::interpreter::maybe_gc                  [interpreter.rs:1463]
```

`gen_heap.rs:8562` is `Self::scan_object_for_old_refs(obj_ptr, …)` inside the
old-gen mark **BFS**, dereferencing a pointer just popped off the worklist. The
lines immediately above it are one of the worklist push sites: an
`old_gen.contains(overlay_ptr)` bare range check followed by a blind
`gc_flags |= GC_FLAG_MARKED` and a push.

That is exactly the code `dev`'s `c3dbb011a` hardens — "seven of nine
mark-worklist push sites validated nothing … a bare `old_gen.contains()` range
check followed by a blind `gc_flags |= GC_FLAG_MARKED` RMW and a push", plus
`is_addr_live` reporting freed old-gen blocks as live. An unvalidated address on
that worklist makes `scan_object_for_old_refs` read a bogus header, compute a
bogus extent, and walk into unmapped memory.

**Measured since:** arm F (built from the merge that includes `c3dbb011a`) no
longer crashes — `rc=0`, 5110 s. So the validation does fix the *fault*, and
defect 3a is closed. It does not fix the premature frees, which are defect 3b
below — `c3dbb011a` stopped the mark from *walking into unmapped memory* on a
bogus address, which is a different thing from making the mark complete.

### The class-loading lead (separate, and fixed)

With `CRATONVM_DBG_STALE_OBJREF=1
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

Step 3 below was done: `cl_load_class_base_delegation_inner` (synthetic) and
`cl_real_load_class_base` (real-JDK — the one this workload takes, since the
repro passes `--java-home`) both dispatched the parent's `loadClass` and the
receiver's `findClass` override and then kept using the pre-dispatch `this` /
name. Hibernate's `AggregatedClassLoader` is `super(null)` and overrides
`findClass` to iterate its scoped child loaders, so that arm is the hot one here.
Both are rooted now. Whether that was the canary's exact site is unconfirmed —
it is the same defect shape on the named path, found by audit, not by a red test.

## Defect 3b — a same-cycle major GC frees collection-overlay objects — FIXED here

The eight `WARN` lines in F's log that nobody had decoded are the thread to
pull:

```
old-gen mark: rejecting external-overlay(BFS owner) candidate 0x1ca84911920 —
  not a plausible object base (aligned=true, w0=0xcc453e9000000004 …)
```

`ObjectHeader` is `class_id:u32 | kind:u8 | element_type:u8 | gc_age:u8 |
gc_flags:u8` in its first word, so `w0=0xcc453e9000000004` decodes as
`class_id=4, kind=0x90, element_type=0x3e, gc_age=0x45, gc_flags=0xcc`. Only
three `gc_flags` bits are defined and `kind` is `0..=2`; this is garbage, and
the screen is right to reject it. All eight decode the same way — one is a raw
**heap pointer** (`0x1ca8e2695f8`) sitting where a header belongs.

So an external-root provider — the collection overlays — is handing the major
GC's marker addresses that are not object bases. Which is exactly what happens
when a provider's stored address is one collection cycle out of date.

### The mechanism

Overlay-backed collections (`LinkedHashMap`, `LinkedList`, `TreeMap`, `TreeSet`)
keep their backing state in process-global Rust side tables, not Java heap
slots, so neither a root slot nor a dirty card can describe the edge. The
collector reaches them through `crate::external_roots`.

In a moving young cycle, **Phase 1a** forwards every overlay-held young object —
and, when the promotion policy says so, *promotes* it into old gen. It
deliberately does **not** repoint the side tables; its own comment says so:

> Only forwarding is needed here: the side tables themselves are repointed
> afterwards by `remap_external_roots` from `pointer_map`.

That "afterwards" is the VM's post-GC pass, which runs **after
`collect_garbage_inner` returns**. But **Phase 5 can run a major GC inside the
same call**, and `old_gen_gc` seeds its mark worklist from those very side
tables:

- `external_roots_for_matching_owners(|o| young_from.contains(o))` tests a
  pre-copy address against the **post-swap** from-space, so it matches nothing
  and that owner's refs are never seeded at all;
- `external_roots_for_owner(..)` in the BFS hands back the **pre-copy** address
  of an object this cycle just promoted, so `old_gen.contains()` is false, the
  promoted copy is never marked, and the compaction frees it — while the
  overlay still holds the only reference to it.

The freed payload is then read back through the relocation-invariant
identity-hash key and comes out as a zeroed header. Hence
`class_id=0 num_slots=0 class_name=java/lang/Object` on 107 accesses, the 281
stale-receiver fallbacks, and — one cycle later, once the provider is still
holding an address that was *freed* rather than *moved*, so the post-GC remap
has nothing to rewrite it to — the eight garbage-header rejections above.

`run_non_moving_young_cycle` has published at this boundary all along, and its
comment names this failure mode almost word for word:

> Waiting for the VM's ordinary post-GC remap is therefore too late: the old
> marker would consult a provider that still points at the forwarded young
> source, fail to seed the promoted destination, and immediately reclaim it.

The moving path never had that call. It does now — one line, immediately before
`Self::major_gc`, idempotent with the VM's later pass (at that point
`pointer_map` holds only young→to-space / young→old-gen entries, whose keys and
values live in disjoint arenas).

### The guard test

`gen_heap.rs::moving_cycle_publishes_relocation_to_external_roots_before_major_gc`
registers a test `ExternalRootProvider` that owns the **only** reference to a
payload object, pushes old gen past the 75 % threshold so the promoting cycle
also compacts, and asserts the payload is still allocated afterwards.

Without the fix it fails with

```
the major GC returned to the free list an object the external-root provider
holds the only reference to
```

and with it, passes. The whole `cratonvm-gc` lib suite is 929/929.

Two things it deliberately does **not** do: it does not use the process-global
`System.gc()` request flag (a concurrently running test could consume it), and
it asserts its own preconditions — old-gen occupancy ≥ 75 %, and the payload
actually in old gen — so a promotion- or sizing-policy change makes it fail
loudly rather than pass vacuously.

### The sibling registries, checked and left alone

`loader_pin`, `mirror_pin` and `metadata_pin` are consulted by the same BFS and
are likewise only rebuilt post-GC, so they have the same *shape* of exposure.
They are left unchanged, on the argument that they are supplementary pins over
objects that are already precise roots (loader singletons via
`gc_scan_loader_singleton_roots`, mirrors via the class-mirror cache), whereas
the collection overlay is genuinely the sole owner — which is why it, and only
it, broke. That is an argument, not a measurement; if a premature free ever
implicates a mirror or a loader, this is the first place to look.

**Measured since:** the fix is real but **inert for this reproducer**, and that
was predictable rather than a surprise. `System.gc()` sets `explicit_full_gc`,
which diverts `collect_garbage_inner` to the NON-moving young sweep — so the
`System.gc()` path never reaches the moving Phase 5 at all. And under `--nojit`
neither `gc_quiescence::is_active()` nor `unregistered_jit_frame_on_stack()` is
ever true, so the conditional overlay-root scan is never skipped and the precise
root set already covers what the major GC's owner walk would have missed. Arm G
(this fix) returns `found=99`, unchanged from F.

Where it IS live is a JIT-on run, where `is_active()` makes
`scan_collection_overlays` skip the precise scan by design. Kept for that.

## Defect 4 — the in-place old-gen sweep frees LIVE promoted objects — FIXED 2026-08-01 (`20cab92aa`)

> Landed after a reverted first attempt. Read
> "[Re-attempted and LANDED](#re-attempted-and-landed-2026-08-01-20cab92aa)"
> below for the paired 0/8-vs-8/8 evidence and for why the earlier attempt
> failed — and **pin `--Xmx` before re-running anything in this section**; the
> unpinned reproducer is load-sensitive and produced the noisy arm table below.

This is the one with a seconds-long reproducer, and it is a genuine
use-after-free rather than a bookkeeping loss.

[`regression-suite/src/ROverlaySystemGcStress.java`](../../../regression-suite/src/ROverlaySystemGcStress.java)
— JIT **on**, real JDK, `System.gc()` per round, collections kept live across
rounds so the old ones get PROMOTED — fails deterministically at round 5:

```
TMDIAG bundle=0 size=0 isEmpty=true get(k0.0)=null containsKey=false iterCount=0 identity=100
```

On `dev`'s own tip it fails harder still, with
`ClassCastException: class java.lang.Object cannot be cast to Bundle` — the same
block after another allocation has been handed it.

### The mechanism

`sweep_young_non_moving` commits selective promotions (young→old) and records
each in `result.0.pointer_map`, but leaves every root on its PRE-promotion young
address. `sweep_old_gen_non_moving` then marks from exactly that slice, and
`old_gen_gc`'s seed loop drops any address `old_gen.contains()` rejects. So a
stale young address seeds **nothing**, the object at its new old-gen home is
never marked, and the sweep returns a **live** object's block to the free list.

The `CRATONVM_OLD_SWEEP_JIT` gate's own comment predicted this exactly — "the
young sweep survives an imperfect root set via conservative over-marking and
side-mark containment, but this old sweep frees purely on `GC_FLAG_MARKED`, so
any root-set gap frees a LIVE promoted object". The gap was self-inflicted, one
statement earlier in the same function.

Fixed by passing the promotion map into `sweep_old_gen_non_moving` and applying
it to `root_shadow` — the private copy that already exists because this mode
must not disturb the caller's roots.

### The three false trails, each killed by a measurement

Worth recording, because each was plausible and each cost a build:

- **the overlay prune** — `dead_keys=10` immediately precedes the loss, so it
  looked causal. It is the messenger: `[overlay-prune] CONDEMNED … region=old-gen
  old_gen_allocated=false` shows it reacting correctly to a block that is
  already on the free list;
- **the identity-hash key** — `[objkey]` stayed silent and
  `probes/IdentityHashStabilityProbe.java` shows the hash stable across the
  failing window;
- **a dangling side-table ref** — `[overlay-stale]` stayed silent on this probe.

Also ruled out: JIT tier-up. `CRATONVM_JIT_THRESHOLD` of 1e3, 1e5 and 1e8 fail
identically while `--nojit` passes, so what matters is the JIT being ENABLED and
the root/quiescence decisions that follow, not any method being compiled.

### The first version of this fix was a regression — don't repeat it

Rewriting the CALLER's root slice (rather than the sweep's shadow) fixed the
probe and the regression suite, and SIGSEGVed `DefaultCatalogAndSchemaTest` 3
runs out of 3. That slice is the VM's root snapshot and outlives the collector
call.

| arm | `rc` across runs | crashes |
|---|---|---|
| G — no root fixup | 0, 0, 0 | 0/3 |
| M — caller's roots rewritten | 139, 139, 139¹ | 3/3 |
| N — fixup on the sweep's shadow only | 0, 139, 139 | 2/3 |
| P — fixup REVERTED | 0, 139 | 1/2 |
| Q — P + a later `origin/dev` merge | 139, 0 | 1/2 |

¹ the third M run had `CRATONVM_OLD_SWEEP_JIT=0`.

### …and then the baseline moved, which invalidated that comparison

**P and Q are functionally identical to G** — the fixup is reverted in both, and
everything else added since is gated off by default. G crashed 0 times in 3
runs; P and Q crashed 2 times in 4. So the true baseline crash rate for this
class is roughly one run in three, **not zero**, and G's clean sweep was luck.

That matters, because the decision to revert was made on "0/3 baseline versus
3/3 and 2/3". Against a ~1-in-3 baseline, 3/3 and 2/3 are not significant.
**The revert stands as the conservative default — an unproven change to the
collector is not worth shipping — but it should not be read as evidence that the
fixup is harmful.** Settling that needs a properly powered comparison, on the
order of eight runs per arm, ideally paired on the same host so background load
cannot skew one arm (the G-vs-M runs were not paired, and the load differed).

### Corroboration: a second, independent 8-pair interleaved run

`20cab92aa` landed defect 4's fix on `dev` with its own paired 0/8-vs-8/8
attribution. The run below was made in parallel, against a slightly different
variant of the same fix (destinations seeded without the explicit
`old_gen_mark_candidate_plausible` screen the landed version uses) and under
much heavier host load, so its absolute rates are worse on BOTH arms. It is
kept because it independently reproduces the direction and because it is what
established the load sensitivity below.

Run with [`probes/hib-paired-ab-20260801.sh`](../../../probes/hib-paired-ab-20260801.sh),
which alternates the arms run by run so a shift in background load cannot land
on one side:

```
baseQ (baseline): 6/8 crashed  [0 139 139 139 139 0 139 139]
fixR  (the fix):  4/8 non-zero [0 139 139 139 0 0 1 0]
```

| | baseline | fix |
|---|---|---|
| SIGSEGV (`rc=139`) | 6/8 | **3/8** |
| runs that completed | 2 | **5** |
| best `found` (HotSpot: 132) | 99 | **128** |

The one `rc=1` is not a crash: that run completed with
`found=128 started=118 ok=99 failed=19`. More of the test plan survived, so real
test failures surfaced instead of tests silently vanishing — which is the
correct behaviour appearing, not a new problem.

Nothing here is significant on its own (Fisher ≈ 0.15), but the direction is
consistent on every axis, and the claim the revert rested on — that the fix
makes this class worse — is refuted. The fix is restored.

**The crash rate tracks HOST LOAD.** Early runs took ~1000–1400 s and crashed
about 1 in 3; these took ~1800–2500 s under heavy concurrent load from other
agents and crashed 4 in 5, in BOTH arms. That is the whole explanation for the
false signal: arm G was measured on a quiet host and arm M on a busy one, and an
unpaired design cannot tell that apart from a code difference. It also says
something about the residual SIGSEGV itself — a fault whose probability rises
sharply with machine load is timing- or concurrency-sensitive, which is the
first thing the next investigation should exploit.

Anyone re-attempting defect 4 should start there rather than trusting the table
above.

### Re-attempted and LANDED, 2026-08-01 (`20cab92aa`)

Done as this section asked: eight runs per arm, paired and interleaved on one
host, with the heap PINNED (see the reproducer caution below — the unpinned form
is what made the original table noise).

| arm | binary | pass | fail |
|---|---|---|---|
| A | pristine `dev` (`5e4b50b8d`) | **0** | 8 — `tm size 0 != 24` every run |
| B | A + promotion-destination seeding | **8** | 0 |

`probes/overlay-paired-ab-20260801.sh`, `--Xmx 256m`, `ROverlaySystemGcStress`.
0/8 versus 8/8 interleaved is not a baseline-drift artefact.

Two things differ from the reverted attempt, and both matter:

* **the shadow only, and additively.** The caller's root slice is never touched
  (that part of the original was a genuine error — it outlives the call), and
  destinations are APPENDED to the private shadow, so the change can only mark
  MORE. Over-retention is the safe direction for a sweep that frees purely on
  `GC_FLAG_MARKED`.
* **the destinations are now screened.** The reverted attempt's comment reached
  the right conclusion — "at least one address in `promotions` is not the valid
  old-gen object base this seed loop assumes" — but had no filter for it. Since
  `c3dbb011a` every mark-worklist push site goes through
  `old_gen_mark_candidate_plausible`, so such an address is rejected instead of
  blindly `gc_flags`-RMW'd. That commit is what makes this attempt viable and
  the earlier one destabilising; re-attempting before it would have failed the
  same way.

Every destination is seeded, not just those a root names: a promotion
destination is live by construction, and root-matching alone misses a promoted
object held only by a heap slot whose rewrite happens in the VM's post-GC
fixup — after this sweep has already freed it.

**Why every collector-side detector was silent on this.** The victim's referrers
were promoted in the SAME cycle, so they are condemned alongside it. A
freed-while-referenced assertion over heap→heap edges asks "does a LIVE heap
object point into the doomed set?", and when a whole subgraph goes together the
answer is legitimately no. The five clean detectors in follow-up 3 below are
therefore **not** evidence that the mark was complete — they could not have seen
this, and they never covered the young→old direction at all. Guarded now by
`gen_heap.rs::old_sweep_seeds_from_this_cycles_promotion_destinations`, which
fails with the seeding disabled ("returned a LIVE promoted object's block to the
free list"). 931/931 `cratonvm-gc` tests, 0 ignored.

The sweep does not zero what it reclaims, which is why this presented as silent
data loss rather than a crash: the freed object still looks intact to Java,
while the overlay prune correctly reads its block as free and drops the side
table — a populated collection reading back as `size 0`.

### Next steps

1. **`found` is not a stable signal.** Two runs of the same clean binary gave
   `found=99` and `found=110`. Any future claim about this class needs several
   runs, and a per-test comparison against HotSpot's 132 rather than a count.
2. The residual gap (99–110 vs 132) is still open and still unexplained. The
   `[overlay-stale] lhm-overlay` reports are the strongest remaining lead, but
   see the caution below before trusting the count.

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
- **`[heap-stale] UN-FORWARDED` was reporting false positives** on every
  major-GC cycle until the recycled-destination filter was added. If you are
  reading an older log, discount it entirely.
- **The out-of-bounds field guard's message misattributes this shape.** It says
  the caller's slot computation is wrong; when `num_slots=0 class_id=0` the
  receiver is simply a freed object and the index is fine. Always classify the
  events before believing the text.
- **`[overlay-stale]` over-reports, and cannot self-correct.** A genuinely DEAD
  `LinkedHashMap`'s overlay entries legitimately point at reclaimed memory from
  the moment it dies until the prune removes them — and the prune's own
  liveness predicate is the thing under investigation. So the 600 (arm G) /
  1886 (arm M) `ZEROED(reclaimed) lhm-overlay` reports are an **upper bound**,
  possibly all benign. What makes such a report real is the collection still
  being reachable from Java, which a heap walk cannot establish and
  `ROverlaySystemGcStress` can.
- **`found` moves run to run on a clean binary** (99 and 110 on the same
  build). It is a weak signal; do not bisect on it.
- **`ROverlaySystemGcStress` on the DEFAULT heap is not a controlled
  experiment** (measured 2026-08-01). Identical command, same binary, same
  class files: one run made ZERO young→old promotions across all 40
  collections, another made 8670 (`CRATONVM_DBG_PROMO_SEED=1`) — the default
  heap size derives from system RAM, which decides how many collections run at
  all. The verdict follows: 4/4 FAIL in one window, 8/8 PASS an hour later,
  with the intervening source change disproven by bisect. **Its published
  `CRATONVM_OLD_SWEEP_JIT=0` A/B therefore flips on its own**, and a run that
  promotes nothing cannot express a promotion defect at all. Always pass
  `--Xmx 256m`, and interleave arms with
  [`probes/overlay-paired-ab-20260801.sh`](../../../probes/overlay-paired-ab-20260801.sh)
  rather than running one arm now and the other later.
- **A freed-while-referenced assertion over heap→heap edges cannot see a
  wholly-condemned subgraph.** If the victim's referrers were promoted in the
  same cycle they are doomed with it, so no LIVE heap object points into the
  doomed set and the detector is legitimately silent. Silence from that class
  of detector is not evidence the mark was complete — defect 4 was invisible to
  all five of them.

## Follow-up 2 — 2026-07-31: relocation is RULED OUT as the mechanism

Went after `native_map_put_evict_pinned` as this doc directed. The most useful
result is a **negative** one, and it invalidates the framing used above.

**The corrupt receiver is `tail_node`.** Every drop from this site reports
`index=3`, and `NODE_FIELD_NEXT == 3`, so the failing store is the tail-append
`set_field(t, NODE_FIELD_NEXT, Some(new_node))` — `t` is the `tail_node` the
chain walk produced.

**But `tail_node` is not stale.** A full failing run (`CRATONVM_DBG_BLOCKGC=1`,
SIGSEGV rc=139, 93/132 tests) recorded **zero** `[blockgc] PIN-STALE` hits.
That canary fires whenever *any* `pin_native_root` receives an already-forwarded
address — precisely the "pinned a stale value, so the pin preserved the
staleness" failure hypothesised above. Not one fired, anywhere in the process,
across the whole run.

That rules out the entire stale-`ObjectRef` family for this crash:

- `tail_node` is pinned before `alloc_object` and re-read from that pin
  immediately before the store, and `native_pin_roots` is a genuine GC root, so
  it cannot be collected *after* the pin either.
- With no forwarding recorded anywhere, it was not silently pre-stale when
  pinned.

**So `tail_node` was already a dead, zeroed node when the walk read it out of
the chain.** `native_map_put_evict_pinned` is a *victim* site, not the source:
something reclaims a node that is still linked into a live bucket chain, and
this store is merely where the damage first becomes visible. Chasing this
function further is chasing a symptom.

**Where to look next.** The source is a *rooting/liveness* defect, not a
staleness one, so the tooling has to change:

- `CRATONVM_DBG_BLOCKGC`'s pin-time canary only sees relocation — wrong
  detector.
- A `CRATONVM_DBG_GC_STRESS` probe is also wrong unless moving-young actually
  engages: one built for this reported `moving_young: cycles=0
  coverage_fallbacks=0`, i.e. the mechanism never ran, so its passing on an
  *unfixed* binary proved nothing (kept as `regression-suite/src/RMapResizeGc.java`
  for its HotSpot-diffed coverage, but it is NOT a reproducer).
- What is wanted is a liveness/ownership assertion: mark nodes at link time and
  check at sweep time that nothing reachable from a live bucket array is being
  reclaimed.

**Also landed (hardening, NOT the fix): `HIB-MAPPUT-PINORDER.1`.**
`native_map_put_evict` and `native_hashmap_put_exact` copy `key_val`/`value` out
of `args` into bare locals, then call `materialize_hm_int_fast` — which re-puts
every side-stored entry through `native_map_put_evict_pinned`, allocating a node
each, so it can collect — and only pin them *afterwards*. Pinning after a
collection roots an already-stale address. Given the canary result this is
**latent, not live** on the collector that actually runs today, but it becomes
live the moment moving-young engages (its own open doc). Fixed by pinning across
the materialisation and re-reading after it.

## Follow-up 3 — 2026-08-01: the victim has NO heap referrer at sweep time

A freed-while-referenced assertion (`CRATONVM_DBG_SWEEP_LIVENESS`, on
`codex/gc-sweep-liveness-assert-20260731`) now covers every heap→heap edge into
a block a sweep is about to reclaim, and all of them come back clean on this
reproducer. Five detectors, five negatives, each from a full failing run:

| # | detector | covers | result |
|---|---|---|---|
| 1 | `CRATONVM_DBG_BLOCKGC` PIN-STALE | any pin of an already-forwarded address | 0 hits |
| 2 | `SWEEP-LIVENESS` (old-gen sweep) | marked old-gen -> doomed old-gen | 0 hits |
| 3 | `SWEEP-LIVENESS young` | old-gen -> doomed young span | 0 hits |
| 4 | `SWEEP-LIVENESS young` | young survivor -> doomed young span | 0 hits |
| 5 | all of the above armed together | — | SIGSEGV 94/132 anyway |

**What that eliminates.** "The mark missed a heap edge" is now excluded in both
generations and both directions. At the moment a block is reclaimed, no live
heap object points at it — so from the collector's point of view the reclaim is
CORRECT.

**What that leaves.** The only referrer is something that is neither a heap
object nor a GC root: a native-side Rust local, a side table, or a cache. The
node genuinely became garbage while native code was still using it — the same
family as `HIB-MAPRESIZE-STALE.1` and `HIB-MAPPUT-PINORDER.1`, both fixed on
this branch, which says at least one more unpinned holder is still out there.
That reframes the hunt a third time: not "which root did the mark miss" but
"which native local is the last reference".

**Caveat, stated plainly.** Each row is ONE run of a high-variance failure —
this reproducer has produced drop counts of 0, 91, 174, 375 and 606 and crashed
anywhere between 74 and 104 tests. Run 5 produced no dropped writes at all, so
the observable corruption signal was absent even though the process still died.
A clean detector on one run is evidence, not proof; these detectors are cheap to
re-arm and worth re-running before treating the eliminations as final.

**Next probe, concretely.** Stop asking the collector and start asking the
allocator: record every node address the map natives allocate together with its
holder, and have the sweep report when it reclaims one whose holder has not
released it. That turns "some native local" into a named call site, which is
what three rounds of collector-side detectors could not do.

## Follow-up 4 — 2026-08-03: the diagnostic tooling itself needed fixing before it could attribute anything; the corruption is real, in old gen, and not the coalescer

Worked on `fix/hib-mapresize-chain-cursor-retire-20260803` (branched from `dev`
tip `a9241eedf`, defect-4's fix already included). Ported forward two commits
from an earlier, unmerged session (`f977c0788`, `cb9e4520b`, originally on
`fix/hib-reclaimed-live-roots-20260801`): a class-identity check on the old-gen
marker's owner→overlay edge (`gc_overlay_roots_for_collection`), landed but
**disabled by default** (`CRATONVM_OWNER_CLASS_FILTER`) because enforcing it
drops thousands of live objects' roots — the class-mismatch signal it keys off
is dominated by false positives (a still-live collection whose *own* header
reads `class_id=0` because it was corrupted, not recycled), not genuine address
reuse. Left disabled; this is diagnostic infrastructure, not a fix.

### The stale-ObjectRef canary was crashing on the very corruption it exists to report

`CRATONVM_DBG_STALE_OBJREF=1` (the quarantine-based canary that turns a stale
native-local read into a hard, attributed panic — see
`docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md`) was
tried against this reproducer on the theory that it is the generic tool for
exactly this bug shape. First run: **SIGSEGV, not a panic** — the canary itself
faulted, reading `0xFFFFFFFFFFFFFFFF`. Manually symbolizing the raw frame
addresses against the built binary (`CRATONVM_SYMBOLIZE=0x<rva>` — NOT
`exe+0x<rva>`, and note the flag is now grouped-spelling-gated, so a bare
`CRATONVM_SYMBOLIZE=` set directly prints a one-line rejection notice but still
works) placed the fault inside `get_header_diagnostics` itself
(`gc/src/gen_heap.rs`): it dereferenced `header.forwarding_address()`
unconditionally whenever `is_forwarded()` — a bare `!ptr.is_null()` check — was
true. A header corrupted in a way that leaves `forwarding_ptr` non-null but
garbage (the same corruption class `old_gen_mark_candidate_plausible` screens
for elsewhere) made the diagnostic dereference unmapped memory instead of
reporting what it found.

**Fixed** (`1ec76ae41`): route `fwd_ptr` through `is_object_address` — the same
lock-free bounds/alignment/tag-validated check every other "is this byte
pattern a real object" call in `gen_heap.rs` already uses — and report an
implausible target as `<implausible-forward 0x...>` data instead of crashing.

Rebuilt, re-ran: **SIGSEGV again**, different address
(`0x0000000100000010`), same symbolized entry point
(`get_header_diagnostics`, RVA unchanged because the crash lands in an inlined
callee at essentially the same offset). This time the full raw-frame chain
symbolized cleanly enough to place it exactly:

```
chm_seg_get (native-collections/src/lib.rs:40870, get_node_hash)
  -> get_field -> get_header -> get_header_diagnostics -> for_each_ref_slot
```

`get_header_diagnostics`'s holder scan calls `for_each_ref_slot(optr, oh, ...)`
on every `old.walk_objects()` result to find anything still pointing at the
stale address. `for_each_ref_slot` (`gc/src/gen_heap.rs`) blindly trusts
`header.num_slots()` / `array_length()` with **zero** bound checking — unlike
`walk_objects`/`scan_region`, which validate a candidate's implied size fits
its free-list-bounded region before ever including it in the walk. A header
that reads differently on this second, lock-free pass (the panic path holds no
GC-exclusive lock; it takes `old_gen.try_lock()` only around the scan itself)
than it did during the walk that produced it made the slot loop stride off
into unmapped memory.

**Fixed** (same commit): re-derive each walked object's total size via
`gen_object_total_size(oh)` and skip it (`continue 'oldscan`) if that disagrees
with the size `walk_objects` already validated for that slot, rather than
trusting the header a second time. `cratonvm-gc`'s full test suite passed after
each of the two fixes, independently.

### With both fixed, the canary survives and reproducibly names one site

Two full runs (2335 s and — the third, `CRATONVM_NO_OLDGEN_COALESCE=1` combined
— 2116 s before a different crash, see below) both produced a **clean Rust
panic** instead of a crash:

```
CRATONVM_DBG_STALE_OBJREF: stale ObjectRef detected at 0x1e7395a50a0 — this
object was evacuated by a moving GC to 0x1e7a7f2bb10 (class_id=4294967295
kind=<implausible-forward 0x1e7a7f2bb10>), but native/interpreter code
dereferenced the OLD address.
Holder scan:
  NO heap holder found — the stale copy lived only in a frame/register/native
  local (root-remap gap), or its holder was itself already collected (native
  invoked from org/junit/platform/launcher/TestPlan.getTestIdentifier(...))
```

Two things this establishes:

1. **The "forwarding target" is itself implausible** (`<implausible-forward>`,
   not a real object). The suspect header's `forwarding_ptr` field is not a
   genuine relocation record — it is corrupted. This is the same shape as the
   `num_slots=33554433` / `kind=0xe7` / `kind=0x3a` (ASCII `:`) corruption
   logged elsewhere in the very same runs (see below): different header
   fields, same underlying "garbage bytes sitting where a header belongs"
   phenomenon.
2. **"NO heap holder" both times, from the same Java call site,
   `TestPlan.getTestIdentifier`.** A genuine Family-1 unpinned-native-local bug
   was the leading hypothesis reaching this doc — but every native on this call
   path (`native_map_get` → `native_chm_get`/`native_lhm_get`/
   `native_hashmap_get_exact` → `chm_seg_get`/`lhm_find_node`) was re-read line
   by line this session and each already pins `this`/the search key/the
   bucket-chain node around every `hashCode()`/`equals()` dispatch, refreshing
   from the pin before every subsequent dereference — including
   `chm_seg_get`'s snapshot-the-whole-chain-then-pin-every-entry dance, which
   looks elaborate but is correct. **This is not a missed pin.** The object
   being read was already corrupted before the native ever touched it; the
   native/`get_field`/`get_header` chain is just where the corruption is first
   *observed*, the same role `native_map_put_evict_pinned` played for defect 1
   before that investigation retracted it as "a victim site, not the source."

### The coalescer is not the (or not the only) mechanism for this lane

The sibling JIT-on doc
([`defaultcatalogandschema-late-phase-instability-20260801.md`](defaultcatalogandschema-late-phase-instability-20260801.md))
logs the *identical* corruption signature (`num_slots=33554432`-ish, rejected
`external-overlay(BFS owner)` candidates) and its leading, untested hypothesis
was `OldGen::coalesce_free_blocks` merging a free block over live promoted
data. Tested here: `CRATONVM_NO_OLDGEN_COALESCE=1` (still with the canary on).
Result: **still crashes**, but differently — `EXCEPTION_ILLEGAL_INSTRUCTION`
(SIGILL, not SIGSEGV) at `old_gen_gc+0x5371`, symbolized cleanly inside
`OldGen::compact()` itself (`gen_heap.rs:10375`), no Rust panic message (so a
genuine trap/bad jump, not `panic!`/`assert!` — release profile here is
`panic = "unwind"` with default `overflow-checks = false`, so an `unwrap`/
`assert` failure would have printed, and didn't). Two `old-gen mark: rejecting
external-overlay(BFS owner)` warnings fired 2 seconds before the crash, and the
crash report's shadow-stack dump nearby contains the literal ASCII bytes
`"TestTask"` (`0x6B73615474736554` read as a `u64`) sitting where a
pointer-sized slot should be — a second, independent instance of "text written
over memory that should hold a pointer," the same phenomenon the ORIGINAL
version of this doc found in arm C (`kind=0x3a`, ASCII `:`).

**Why this is a real negative result, not just "n=1 didn't help":**
`CRATONVM_NO_OLDGEN_COALESCE` only gates `OldGen::coalesce_free_blocks`, which
is called from exactly two places — `OldGen::alloc`'s last-ditch retry, and the
in-place (non-compacting) old-gen sweep's post-free coalesce pass. `--nojit`
means there is never a live JIT frame, so `has_conservative_roots` is false and
`old_gen_gc` takes the **compacting** branch essentially every time (the
in-place-sweep branch this doc spent most of its history on is the
`--nojit`-when-a-JIT-frame-is-live case, which cannot arise under `--nojit`
proper). `OldGen::compact()` "rebuilds the free list as a single trailing block
and so has never needed" coalescing (its own doc comment). So disabling the
coalescer could not have touched the code path that actually crashed here —
the negative result is expected, not surprising, and it means **the coalescer
hypothesis from the JIT-on doc does not explain this `--nojit` lane**, even
though the corruption *signature* the two docs log is identical. Either the
two lanes share a root cause reachable through a different intermediate
mechanism (most likely: something written during `old_gen.compact()`'s own
slide/fixup phases, or during a moving-young promotion that feeds it), or they
are two independent writers producing coincidentally similar garbage. Not yet
distinguished.

### What is established, and what is not

Established:
- The residual corruption is real, reproducible, and lives in old-gen object
  headers (not a GC-root/rooting gap — the marker's owner-class check, the
  freed-while-referenced sweep-liveness assertion, and now the stale-ObjectRef
  canary have all been pointed at it and none found a rooting explanation).
- It is not a Family-1 unpinned-native-local bug on the `TestPlan` /
  `ConcurrentHashMap`/`LinkedHashMap` get-path — that path is correctly pinned
  throughout.
- It is not (solely, or not at all, for this lane) the old-gen coalescer.
- `SWEEP-LIVENESS` (the freed-while-referenced assertion) does not fire on
  this reproducer, consistent with the JIT-on doc's own finding: whatever
  writes this garbage is not "the sweep reclaimed a block something still
  points at."

Not yet established: the actual writer. Two concrete leads for whoever
continues this:
1. **The literal ASCII bytes appearing where pointers belong** (`kind=0x3a`
   originally, `"TestTask"` this session) are the strongest single clue in the
   whole investigation and have never been chased to a source. Something is
   writing STRING/text data into memory that a pointer-typed slot or an
   object header later reads. Grep for raw byte-level writes into old-gen
   memory that do not go through `set_field`/the allocator (a `String`
   backing-array write, a JNI byte-copy, a `Vec<u8>` resize that reuses old
   gen's backing buffer) — this smells like a **type confusion / wrong-stride
   copy**, not a GC-rooting bug, and the doc's whole history of GC-side
   detectors (five of them, all clean) is consistent with that: none of them
   were built to look for a writer outside the GC's own protocol.
2. **`OldGen::compact()`'s own phases** (forwarding-address assignment,
   reference-slot rewrite, the slide, free-list rebuild) are the one thing
   provably on the call stack of the SIGILL crash and have not yet been
   audited this session the way `sweep_old_gen_non_moving` was for defect 4.
   Start there before `coalesce_free_blocks` again.

## Related

- `docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md` — defect
  1's bug class, found from the other end.
- `HIB-WEAKREF-RECYCLE.1` — a different corrupt writer in the same runs (post-GC
  reference processing); fixed separately, removed 84 bad writes per run, and did
  not stop this one.
- [`gc-overhead-limit-spurious-oom-at-half-full-heap-20260731.md`](gc-overhead-limit-spurious-oom-at-half-full-heap-20260731.md)
  — the same class's JIT-on failure mode.
