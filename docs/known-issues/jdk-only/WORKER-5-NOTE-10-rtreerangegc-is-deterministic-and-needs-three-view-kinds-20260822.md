# WORKER-5 NOTE 10 — `RTreeRangeGc`'s compatible-mode failure is DETERMINISTIC, needs three DIFFERENT view kinds, and is not any of the six things it looks like

**Status: FIXED, MEASURED.** Lane WORKER-5, 2026-08-22. Oracle: HotSpot
25.0.3+9. Found on `cratonvm-w5b.exe`, diagnosed with an instrumented build
(`cratonvm-w5tv.exe`), fixed and verified on `cratonvm-w5fix.exe` — all Windows
release builds of the integrated tree made by this lane.

> **§7 is the cause and the fix.** A TreeMap content native passed an UNPINNED
> `this` across `tm_sync_native_state`, which allocates; a moving collector
> relocated the view mid-call and the caller then keyed the side table on a
> from-space address. `TreeMap.size()` answered **0** on a view nothing had
> mutated, and `entrySet()` hitting the same window is the
> `ClassCastException`.
>
> **All three arms are now green: 107/107 · 107/107 · 67/67.**
>
> §1–§6 are kept as written — the reproduction and the six eliminations are what
> made the diagnosis reachable, and three of the theories they kill are the ones
> a reader would otherwise try first.

---

## 1. The reproduction, which is the useful part

```bash
cratonvm --java-home "$JDK" --Xmx 64m --nojit -cp <dir> RTreeRangeGc
```

**100% failure.** `--nojit` is not a workaround — it makes the bug MORE
deterministic (6/6 with it, 5/6 without), which is also what exonerates the JIT.

```text
ClassCastException: class RTreeRangeGc$V cannot be cast to class java.util.Map$Entry
    at RTreeRangeGc.checkMap(RTreeRangeGc.java:121)      // Map.Entry e = it.next();
```

The entrySet iterator returned a **value object where an Entry belongs** — an
address that was reclaimed and reused. Alongside it, one `gc::guard` ERROR:

```text
obj=0x…  site="checkcast"  in_published_snapshot=true  published_roots=886
last_publish_at_collection=1  collections_now=2  last_publish_pc=17
holder=<not found in frames>  in_blocked_region=false  frames=2
top_frame=RTreeRangeGc.checkMap pc=44
```

**Strict mode passes 6/6.** Under `--jdk-only` the collection natives decline
and real JDK bytecode runs, so the native view-materialisation path is
implicated — which also means this is NOT one of the COMPATIBLE-mode defects
that "go green when the natives are fixed"; it is the natives.

## 2. The bisect — it needs THREE DIFFERENT VIEW KINDS

`RTreeRangeGc` asserts 14,014 things about six views, so it cannot say which.
Truncating `main` after each view (`Mini1`..`Mini6`, `if (true) return;`):

| views walked | result |
|---|---|
| 1 — `headMap` | ok, 1802 checks |
| 2 — `+ tailMap` | ok, 3603 checks |
| **3 — `+ subMap`** | **ClassCastException** |
| 4, 5, 6 | ClassCastException |

And the three controls that say what it is NOT:

| control | checks | result |
|---|---:|---|
| `subMap` **alone** | 1202 | **ok** — subMap is not itself broken |
| `headMap` ×3, same bounds | 5404 | **ok** — and that is MORE checks than the failing case |
| `headMap` ×3, **different** bounds | 4684 | **ok** — not simple spec aliasing either |

**So it is not the subMap code, not allocation volume, and not "three distinct
view specs".** It needs a MIX OF KINDS — `headMap` + `tailMap` + `subMap` —
live against one source map.

## 3. Six eliminations, each measured

1. **The JIT root scanner.** `--nojit` makes it worse (6/6 vs 5/6), so
   `jit/conservative_roots.rs` is not it.
2. **Pinning an already-stale value.** `CRATONVM_DBG_BLOCKGC=1` arms a pin-time
   canary that fires when a caller hands `pin_native_root` an already-forwarded
   address. **Zero hits on a failing run** — nothing stale is being pinned, so
   the bad reference is used with no pin in the picture.
3. **A false-dead overlay prune.** The documented hazard (2026-08-01,
   `ROverlaySystemGcStress`, "a populated TreeMap reading back as size 0") would
   fit the symptom exactly. `CRATONVM_DBG_OVERLAY_PRUNE=1` on a failing run
   condemns two addresses and **both are `in_heap=false`** — genuinely not live
   objects. The prune is not condemning live views.
4. **`tm_view_table` being missed by the prune.** It is not: the prune covers
   LinkedList, LinkedHashMap/Set, TreeMap array+fast, `tm_force_array_set`,
   TreeSet, CSLM comparators, the snapshot-iterator backings AND the
   navigable-view specs. Read, not assumed.
5. **A raw address key colliding after a recycle.** `widened_obj_key` is not a
   raw address — it is an identity-hash + generation scheme with an explicit
   class-id disambiguator and a documented recycle path. The obvious version of
   this theory is wrong.
6. **`tm_refuse_reversed_bounds` / `tm_new_range_view` losing a bound across a
   collection.** Both pin and re-read `this`, `from`/`lo` and `to`/`hi` through
   pins before use, and `tm_set_slot` / `tm_set_force_array` are table-only (no
   allocation), so `source` cannot go stale between them.

## 4. The one unexplained observation, and it is the best lead

**The overlay prune runs ONCE for TWO collections.** On a failing run,
`CRATONVM_DBG_MIRRORPIN=1` reports exactly one
`gc_prune_dead_collection_overlays CALLED` while the guard reports
`collections_now=2`.

`gc_prune_dead_collection_overlays` is called from
`vm/src/runtime/interpreter/gc_and_alloc.rs`, deliberately NOT from
`update_all_roots` — that function early-returns on an empty `pointer_map`, so
it would miss the non-moving sweep. If some *other* collection path reaches the
collector without going through that call site, its dead overlay entries survive
to be aliased. That is the shape `ovl×4` ("overlay side tables need rooting in
every collector path") already records for the ROOTING half.

**This is a lead, not a finding.** Nothing here shows that the second collection
is the one that corrupts, or that a surviving entry is what the iterator reads.

## 5. What this record does NOT establish

* ~~**No root cause, and no fix.**~~ §7.
* **Why the failing COMBINATION was what it was is still unexplained**, and the
  fix does not depend on it: `ss` failed while `s`, `hs` and three same-kind
  views with different bounds passed. The move that corrupts is timing, so which
  call sequence happens to relocate the receiver is not a property worth
  chasing now — but nothing here derived it, and §2's table is retained rather
  than rationalised after the fact.
* **The audit of §7 NOMINATION 4 is not done.** Only `tm_sync_native_state`'s
  callers were fixed.
* **`regression-suite/probes/TreeViewGcProbe.java` does NOT reproduce it.** It
  is committed as the NEGATIVE CONTROL: nine view shapes, walked under real
  collection pressure with the vector's own eager-message allocation habit, all
  surviving. It says where the bug is not.
* **The `Mini1`..`Mini6` bisect harness is mechanical**, generated by truncating
  `main`; it is described in §2 rather than committed, because it is six
  near-copies of a vector that already exists.
* **Only one host and one binary.** Everything here is `cratonvm-w5b.exe` on
  this Windows box.

## 7. THE CAUSE, AND THE FIX

### 7.1 What the instrument said

§4's "best lead" (the prune running once for two collections) was **not** it.
The answer came from instrumenting `tm_resync_view_inner` itself
(`CRATONVM_DBG_TMVIEW`, declared and kept). Five identical `size()` calls on one
unmutated `subMap`:

```text
  q sizes: 200 200 0 200 200        expect=200      <- HotSpot: 200 x5

[TMVIEW] resync view=0x203352ed650 source=0x203339f23b8 pairs=400 kept=200   (x4)
[TMVIEW] resync view=0x20333a99870 source=0x203339f23b8 pairs=400 kept=200   (x2)
```

Two facts settle it. **Every resync computed `kept=200`** — the rebuild was
never wrong. And **the view's address changes**, across exactly the call that
answered `0`.

### 7.2 The mechanism

```rust
tm_sync_native_state(ctx, this)?;                    // ALLOCATES; `this` may move
let size = tm_get_slot(ctx, this, TM_FIELD_SIZE);    // reads the OLD address
```

`tm_sync_native_state` rebuilds a view's storage — boxing keys, dispatching
`compareTo` into interpreted Java, allocating the backing array — so a moving
collector can relocate the receiver inside it. Every caller passed a bare
`ObjectRef` and kept using its own copy.

`tm_get_slot` / `tm_collect_pairs` key the side table on `widened_obj_key(this)`,
which starts with `identity_hash_code(this)`. Off a from-space address that
reads a stale header, lands in a different bucket, and mints a FRESH slot — so
the lookup finds **no entry and answers the default**. Hence `size() == 0` while
`isEmpty()` (a later call, after the move had settled) said `false` and
`firstKey()` was right: a self-contradiction inside one object.

`entrySet()` does the same thing one step further — `tm_collect_pairs` on the
stale receiver — and the list it builds is what hands a `V` to a `Map.Entry`
checkcast.

### 7.3 The fix, and why the signature changed

The pin now lives inside `tm_sync_native_state`, once, and the refreshed
reference is written back through `&mut`:

```rust
fn tm_sync_native_state(ctx: &mut dyn NativeContext, this: &mut ObjectRef)
    -> Result<(), MethodCallFailed>
```

`&mut` rather than a returned `ObjectRef` **deliberately**: a returned value can
be dropped with `?;` and silently keep the bug, while `&mut` makes every one of
the **34 call sites a compile error** until converted. rustc reported 35 errors,
then 31 mutability suggestions which were applied from its own JSON output
rather than by hand. `grep` afterwards confirms **zero** call sites of the old
shape remain.

The one site that could not take `&mut` directly — `spec.source` behind a shared
`&TmViewSpec` — goes through a local, and the authoritative re-read there is
still the pin that already existed.

### 7.4 MEASURED

| | before | after |
|---|---|---|
| `size()` x5 on one `subMap` | `200 200 0 200 200` | **`200 200 200 200 200`** |
| `ss` (two subMaps) | 0/4 pass | **4/4** |
| `sss`, `hts`, `MM` | 0% | **100%** |
| `RTreeRangeGc`, `--nojit` | 0/6 | **6/6** |
| `RTreeRangeGc`, JIT on | 1/6 | **6/6** |
| `SUITE=all CRATONVM_ARGS=--jdk-only` | 106/107 | **107 / 107** |
| `SUITE=all` | 105/107 | **107 / 107** |
| `SUITE=core` | 65/67 | **67 / 67** |

`RTreeRangeGc`'s CK values match HotSpot exactly (`319200 / 79800 / 199500`,
`14014 checks`).

### 7.5 One self-inflicted detour, recorded because it cost a run

Between the fix and the arms, a sweep reported **11 failures in a set that
changed between runs**, each with a `harness:` entry. That is the environment
signature trap 8 describes, and it was mine: `git checkout -- regression-suite/build/`
while a suite was compiling into that directory. `git clean -Xf` on the
generated classes (the 8 TRACKED ones left alone) and a clean re-run gave
107/107. **A failure set that changes between runs is not a verdict** — the
two vectors it named passed standalone on both binaries.

## 6. NOMINATIONS

* ~~**N1 — owner needed.**~~ **DONE — §7.**
* **N2 — the prune still runs ONCE for TWO collections** (§4). It was NOT this
  defect, but nothing showed it is correct either, and it is cheap to assert.
  Left open deliberately rather than closed by association.
* ~~**N3 — `RTreeRangeGc` is the one red cell.**~~ **All three arms are green.**
* ~~**N4 — audit the SAME SHAPE elsewhere.**~~ **DONE — `WORKER-5-NOTE-11`.**
  Seventeen funnels in `native-collections` match the shape, five have callers
  that reuse the receiver, and **one** (`resync_view_set`, 8 of 10 sites) still
  had the bug; it is fixed with the same `&mut` pattern but is LATENT — no
  reproduction. `resync_ts_view` and `resync_values_view` were already correct
  at all 19 of their call sites. The audit is scoped to that one crate.

---

### INDEX ROWS (for H0 to move into `INDEX.md`)

* `WORKER-5-NOTE-10` — `RTreeRangeGc`'s compatible-mode failure reproduced
  **100% deterministically** (`--Xmx 64m --nojit`), bisected to needing
  **`headMap` + `tailMap` + `subMap` together** (subMap alone passes; three
  headMaps with different bounds pass with MORE checks), and **six mechanisms
  eliminated with measurements** — JIT roots, pin-time staleness, false-dead
  overlay prune, an unpruned view table, raw-address keying, and bound-loss in
  the range-view constructor. **FIXED (§7):** a TreeMap content native passed an
  UNPINNED `this` across `tm_sync_native_state`, which allocates; a moving
  collector relocated the view and the caller then keyed the side table on a
  from-space address, so `size()` answered 0 on an unmutated view and
  `entrySet()` threw. The pin now lives in the funnel and the refreshed ref is
  written back through `&mut` — chosen so all 34 call sites are COMPILE ERRORS
  until converted. **All three arms green: 107/107 · 107/107 · 67/67.**
