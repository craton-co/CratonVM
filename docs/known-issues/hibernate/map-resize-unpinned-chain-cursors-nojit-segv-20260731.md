# The collector leaves reference fields UN-FORWARDED — `DefaultCatalogAndSchemaTest` silently loses a third of its tests

| | |
|---|---|
| **Status** | 🟠 **SIGSEGV FIXED, correctness residual OPEN.** The class now runs to completion (`rc=0`, 5110 s) instead of crashing at 2103–2611 s. It still does **not** match HotSpot: `found=99` vs `132`. The collector still leaves reference fields UN-FORWARDED (defect 3), which is the live defect. |
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

And the deep verifier still reports **≥40 UN-FORWARDED fields per major GC** on F
(the report caps at 40), against `pointer_map size=3295701`, with the same
referrer shapes as on C.

So there is **one** defect left, not two: fix the un-forwarded edges and the
`found` count should follow.

So `c3dbb011a` removed the crash, not the corruption. Defect 3 is unchanged and
is now a **silent wrong answer** — a third of the class's tests quietly vanish —
which is the worse failure mode of the two.

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

## Defect 3 — the collector leaves reference fields UN-FORWARDED — OPEN, this is the residual

Arm C logs 48 of these before dying:

```
GC: header kind=0x3a reached the legacy-object sizing arm (shape=0, class_id=2044330296)
old-gen mark: rejecting object at 0x23a79da0120 with implausible extent 0 (kind=58, array_len=0, num_slots=0)
```

`kind=0x3a` is ASCII `':'`, and it repeats across distinct objects — the header
bytes look like **text written over an old-generation object header**, not a
relocated or zeroed one. (Arm B's kinds were varied garbage; arm C's are
consistently `0x3a`. One run settles nothing here — see the variance caution.)

### The corruptor is an UN-FORWARDED old-to-young edge

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

**`UN-FORWARDED` is the verifier's most actionable class**: the target address is
still a KEY in the collector's own `pointer_map`, i.e. the object *was* moved and
this referrer's field was never rewritten. Not a native local, not a missing
root — a **missed referrer edge in the collector**.

That closes the causal chain end to end: an un-forwarded field keeps pointing at
the pre-move address; once that young space is reused the address holds
arbitrary bytes; a later old-gen scan reads those bytes as an object header and
gets `kind=0x3a` (ASCII, i.e. text) — the corrupt headers above — and eventually
a dereference lands on unmapped memory.

Two details worth keeping:

- The 17 `NumericIdentifierAttributeImpl field[2]` lines are 17 **distinct
  referrer objects all pointing at the same un-forwarded target**, and likewise
  for `ListAttributeImpl field[3]`. One moved object, many referrers, none
  updated — so this is a scan gap over a *set* of referrers, not a one-off.
- `pointer_map size=3436289` — 3.4 M objects moved in that cycle. Whatever the
  gap is, it survives a full compaction.

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
bogus extent, and walk into unmapped memory — and a bogus mark is equally a good
explanation for the UN-FORWARDED edges above, since the compaction's notion of
what is live and where it moved comes from that same mark.

**Measured since:** arm F (built from the merge that includes `c3dbb011a`) no
longer crashes — `rc=0`, 5110 s. So the validation does fix the *fault*. It does
**not** fix the un-forwarded edges: the same `CRATONVM_DBG_HEAP_STALE=1` run on F
still reports the report-cap of 40 stale fields in a major collection, with the
same referrer shapes. Whatever leaves those fields un-rewritten is still there;
`c3dbb011a` stopped the mark from *walking into unmapped memory* on a bogus
address, which is a different thing from making the mark complete.

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

### Where to look first — `update_refs_in_object` skips everything outside old gen

`OldGen::update_refs_in_object` (`gc/src/old_gen.rs`, reached from
`OldGen::compact`) is the pass that rewrites an old-gen object's own reference
slots after compaction. Its closure returns `None` — i.e. **leaves the slot
untouched** — for any target outside the compacted region:

```rust
let r = ref_ptr as usize;
if r < data_start || r >= data_end {
    return None; // reference outside the compacted old-gen region
}
```

That is correct *only* if every old→young edge is rewritten by the young
collector instead, via the card table / remembered set. Which puts the whole
weight on that set being accurate — and the card table is indexed by
`(addr - base) / CARD_SIZE`, so a compaction that **relocates the referrers
themselves** invalidates every recorded dirty-card index. A composed cycle
(young + major in one `collect_garbage_inner`) is exactly where that ordering
can bite.

The sibling pass `fixup_young_old_refs` (`gen_heap.rs`) already carries a
hardening comment naming this whole failure mode for the *other* direction —
"then `break` and leave the rest of from-space's old-gen refs un-fixed-up after a
compaction → dangling pointers" — and falls back to a conservative word rewrite
over any stretch it cannot parse. The old→young direction has no equivalent
backstop.

This is a hypothesis from reading, **not** a measurement. Confirm it before
changing anything: the reported referrers' addresses versus the old-gen bounds,
and whether their cards were dirty, will settle it.

### Next steps

1. Chase the `UN-FORWARDED` edge above — that is the actual corruptor, and it is
   a collector bug, not a native-rooting one.
2. Symbolized build (`RUSTFLAGS="-Cdebuginfo=2 -Cforce-frame-pointers=yes"`,
   separate `CARGO_TARGET_DIR`) + the canary run, to name the remaining stale
   frame outright. The release binary carries no symbols and every frame in the
   canary's backtrace prints `<unknown>`.

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

## Related

- `docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md` — defect
  1's bug class, found from the other end.
- `HIB-WEAKREF-RECYCLE.1` — a different corrupt writer in the same runs (post-GC
  reference processing); fixed separately, removed 84 bad writes per run, and did
  not stop this one.
- [`gc-overhead-limit-spurious-oom-at-half-full-heap-20260731.md`](gc-overhead-limit-spurious-oom-at-half-full-heap-20260731.md)
  — the same class's JIT-on failure mode.
