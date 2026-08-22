# WORKER-5 NOTE 10 — `RTreeRangeGc`'s compatible-mode failure is DETERMINISTIC, needs three DIFFERENT view kinds, and is not any of the six things it looks like

**Status: OPEN — NOT FIXED.** Lane WORKER-5, 2026-08-22, on
`C:/craton/cratonvm-w5b.exe` (a Windows release build of the integrated tree).
Oracle: HotSpot 25.0.3+9. `native-collections` / `gc` are other lanes' surfaces;
this record hands over a reproduction and six eliminations, not a diagnosis.

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

* **No root cause, and no fix.** Six mechanisms are eliminated; the seventh is
  not identified.
* **Why a MIX of view kinds is required is unexplained.** Three same-kind views
  with three different bound sets pass. Nothing here explains the asymmetry, and
  it is the most specific fact available — whoever takes this should start
  there.
* **`regression-suite/probes/TreeViewGcProbe.java` does NOT reproduce it.** It
  is committed as the NEGATIVE CONTROL: nine view shapes, walked under real
  collection pressure with the vector's own eager-message allocation habit, all
  surviving. It says where the bug is not.
* **The `Mini1`..`Mini6` bisect harness is mechanical**, generated by truncating
  `main`; it is described in §2 rather than committed, because it is six
  near-copies of a vector that already exists.
* **Only one host and one binary.** Everything here is `cratonvm-w5b.exe` on
  this Windows box.

## 6. NOMINATIONS

* **N1 — owner needed** in `native-collections` / `gc`. The reproduction is one
  command and 100% reliable; §2's table says which three calls are needed and
  §3 says which six explanations are already spent.
* **N2 — count the prune against the collection count** (§4). If
  `gc_prune_dead_collection_overlays` is genuinely reachable on only one of two
  collection paths, that is a defect regardless of whether it is THIS defect,
  and it is cheap to assert.
* **N3 — `RTreeRangeGc` remains the one red cell.** With the `getDefinedPackage`
  fix (`WORKER-5-NOTE-8`) the arms are **107/107 strict, 106/107 compatible,
  66/67 core**, and this vector is the whole of the difference.

---

### INDEX ROWS (for H0 to move into `INDEX.md`)

* `WORKER-5-NOTE-10` — `RTreeRangeGc`'s compatible-mode failure reproduced
  **100% deterministically** (`--Xmx 64m --nojit`), bisected to needing
  **`headMap` + `tailMap` + `subMap` together** (subMap alone passes; three
  headMaps with different bounds pass with MORE checks), and **six mechanisms
  eliminated with measurements** — JIT roots, pin-time staleness, false-dead
  overlay prune, an unpruned view table, raw-address keying, and bound-loss in
  the range-view constructor. Best remaining lead: the overlay prune runs ONCE
  for TWO collections. **NOT FIXED**; negative-control probe committed as
  `TreeViewGcProbe.java`.
