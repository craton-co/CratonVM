# W7-96 — `ConcurrentHashMap.table` is never populated: what that blocks, what it does not, and the mirror that closes the carrier half

Status (2026-08-22, WORKER 2): **§4 is BUILT and RUN**, both §7 nominations are
IMPLEMENTED, and the §8.1 authority move is **REFUSED with a 23-reader audit** —
see §9, which supersedes the status line below and corrects §3.1.

Status: **the feasibility question is ANSWERED — YES, `table` can be maintained**,
and the carrier half is implemented in `native-collections/src/lib.rs`. The
retirement half is **NOT** unblocked and this record is the correction to the one
sentence that was carrying it.

Branch `claude/jdk-only-mode-completion-1351c0`, lane B10, 2026-08-12. Every
number below is measured on `/c/craton/jdkonly-wave2-target/release/cratonvm.exe`
(the wave-2 binary as of this date) against HotSpot 25.0.3+9
(`C:\Program Files\Microsoft\jdk-25.0.3.9-hotspot`), one probe source compiled
once and run on both VMs. **The Rust change in §4 was written but NOT built and
NOT run** — this lane is measurement-and-source only. Everything in §1–§3 and §5
is measured on the pre-change binary and stands independently of whether §4
compiles.

---

## 1. The layout, field by field, empty / small / post-resize

Probe: reflect every declared instance field of `java.util.concurrent.ConcurrentHashMap`,
plus `java.util.AbstractMap`'s two, walking bin chains through `next`. Run with
`--add-opens java.base/java.util.concurrent=ALL-UNNAMED --add-opens java.base/java.util=ALL-UNNAMED`
on both VMs.

| field | | empty | 6 entries | 200 entries (past resize) |
|---|---|---|---|---|
| `table` | HotSpot | `null` | `len=16 used=6 nodes=6` `$Node` | `len=512 used=198 nodes=200` `$Node` |
| | CratonVM | `null` | **`null`** | **`null`** |
| `nextTable` | both | `null` | `null` | `null` |
| `baseCount` | HotSpot | 0 | 6 | 200 |
| | CratonVM | 0 | **0** | **0** |
| `sizeCtl` | HotSpot | 0 | 12 | 384 |
| | CratonVM | 0 | **0** | **0** |
| `transferIndex` / `cellsBusy` | both | 0 / 0 | 0 / 0 | 0 / 0 |
| `counterCells` | both | `null` | `null` | `null` |
| `keySet` / `values` / `entrySet` (CHM's own) | both | `null` | `null` | `null` |
| **`AbstractMap.keySet`** | HotSpot | `null` | **`null`** | `null` |
| | CratonVM | `null` | **`Object[4]` of `AnonymousObject$3`** | `Object[4]` |
| `keySet().iterator()` | HotSpot | `$KeyIterator` | `$KeyIterator` | `$KeyIterator` |
| | CratonVM | `Arrays$ArrayItr` | `Arrays$ArrayItr` | `Arrays$ArrayItr` |
| `keys()` / `elements()` | HotSpot | `$KeyIterator` / `$ValueIterator`, **dual** | same | same |
| | CratonVM | `Collections$3` / `Collections$3`, **NOT an `Iterator`** | same | same |

Control, same run, same probe: `Hashtable` with 6 entries — CratonVM
`table = len=16 used=6 nodes=6`, `count=6 threshold=8 modCount=6`, node type
`cratonvm.synthetic.AnonymousObject$4`, **array type `[Ljava.lang.Object;`** (not
`[Ljava.util.Hashtable$Entry;`). That last cell is the one that lowers the bar:
java.base's own `Hashtable$Enumerator` walks an `Object[]` of untyped nodes
correctly, because `aaload` does not type-check on read and `Entry.key/.value/.next`
resolve against our slots.

### 1.1 The side layout is squatting on `AbstractMap.keySet`

`CHM_FIELD_SEGMENTS = 0` (`native-collections/src/lib.rs:44999`) and slot 0 of a
real `ConcurrentHashMap` is **`java.util.AbstractMap.keySet`**, a `Set`-declared
field — superclass fields come first, and `AbstractMap` declares `keySet` then
`values`. The measurement above confirms it directly: reflecting
`AbstractMap.class.getDeclaredField("keySet")` on a 6-entry CratonVM CHM yields
`Object[4]` whose elements are the segment objects, where HotSpot has `null`.

CHM's real `table` is slot 2 and is a genuinely different slot, so **there is no
conflict between the segmented store and a populated `table`** — they do not
alias. This is the same defect shape `map_buckets_slot` /
`publish_map_table_inner` (`:7101`, `:7143`) already fixed for the `HashMap`
family, applied to CHM's slot 0 and not yet fixed here.

Segment shape: `Object[]` of bare `ClassId(0)` 3-slot objects
(`MAP_FIELD_BUCKETS/SIZE/CAPACITY`), each holding its own `Object[]` bucket array
of 4-slot nodes `hash@0 key@1 val@2 next@3` — **which is exactly
`ConcurrentHashMap$Node`'s declared field order**. Built by `chm_init_segments`
(`:45273`), written only through `native_map_put` on the *segment*; read by
`chm_collect_all_{keys,values,entries}` (`:45149`–`:45270`), which re-sort by a
virtual bucket index to approximate HotSpot's flat-table iteration order.

---

## 2. Can `table` be maintained? **YES — and here is the evidence, not the argument**

The bar set for this lane was not "identical to HotSpot" but "java.base's own
`KeyIterator`/`ValueIterator` can walk it and produce the right answers". That
was measured directly, and it passes.

`probes/ChmDrive.java` (reproduced in §6) builds, **entirely at the Java level**,
a HotSpot-shaped table: a real `ConcurrentHashMap$Node[16]` populated with real
`$Node` objects via the reflective 4-arg constructor, chained by
`(n-1) & spread(hash)`. It writes that array into `table` reflectively, then
constructs java.base's own `KeyIterator` and `ValueIterator` over it through
their real 5-arg constructor and drives both faces.

```text
                                          HotSpot 25   CratonVM --jdk-only
  A4 table readback identity (same==)      true         true
  A6 KeyIterator isIterator/isEnumeration  true/true    true/true
  A7 keys via real KeyIterator             [c0..c5]     [c0..c5]
  A9 MATCH                                 true         true
  B1 values via real ValueIterator         [w0..w5]     [w0..w5]
  B2 MATCH                                 true         true
```

Both faces, **one shared cursor** (one element taken through `nextElement()`, the
rest through `hasNext()/next()`, yielding the table exactly once), correct
multiset. So on CratonVM `--jdk-only`, `Traverser.advance()`, `tabAt()`'s
`U.getReferenceAcquire` **and its checkcast to `$Node`** all execute correctly
over a hand-built table. The reflective write to `table` also sticks and does not
disturb the map.

**Nothing about the shape was blocking. Only the field was empty.** The
`--jdk-only` refusal comment at `make_snapshot_enumeration` (`:48807`–`:48822`)
was right that deleting the natives would answer empty, and wrong only in the
implicit assumption that populating the field was not available.

### 2.1 The trap this lane was warned about, and why it does not fire here

The `Hashtable` lane found that driving a real `Enumerator` over a `Properties`
(side-table backed) yields `[]` on CratonVM and **NPEs on HotSpot** — gate on the
store, not the receiver's class. That is exactly what §4 does: the carrier is
built over an array this code has just materialised and can see the contents of,
and any surprise abandons the whole attempt (§4.2). It never hands a real JDK
iterator a receiver whose store it has not just written itself.

---

## 3. What this does NOT unblock — the correction to the 47 held retirements

`P2-COLLECTIONS-SHADOWS-20260812.md` holds 47 of 68 `java.util` registrations as
"needs-VM-support" with CHM as the headline, on the ground that retiring them
would "answer an EMPTY collection over a populated map". **That premise is
correct, and the inference that populating `table` unblocks the family is not.**

### 3.1 The dial says the premise has never actually been tested

`CRATONVM_ENFORCE_NATIVE_SHADOW` (`vm/src/runtime/env_cache.rs:422`) takes a
prefix list and is the instrument for exactly this question. Armed for CHM on the
current binary, **3/3 runs identical**:

```text
  CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/concurrent/ConcurrentHashMap
  6 puts of distinct keys into a fresh ConcurrentHashMap:
    put returned null (= "fresh insert")   6 of 6
    size()                                 1
    get() hits                             1 of 6
    keySet() iteration                     1  -> [k0]
    table                                  len=16 used=1   baseCount=1
    slot 0 (segments)                      null
```

Five of six entries **silently vanish**, and `put` reports success for every one.

**This is not real CHM bytecode failing.** It is the instrument being
short-circuited. `force_native_over_real_jdk_bytecode`
(`vm/src/runtime/interpreter/native_override.rs:2508`-2542, mirrored at
`vm/src/vm/vm_exec.rs:22199` and
`vm/src/runtime/interpreter/dispatch_virtual.rs:3420`) forces the native for
`ConcurrentHashMap` × 23 method names — `put get remove size keys elements
keySet values entrySet clear …` — and **does not consult
`enforce_shadow_scope()`**. `<init>` is *not* in that list. So arming the dial
retires the constructor and keeps the mutators, producing a CHM with **no
segments**, and `native_chm_put` (`:46490`-`46499`) responds to
`chm_segment_for → None` with:

```rust
None => Ok(Some(Value::Object(None))),
```

— *"there was no previous mapping"*, having stored nothing. That single line
turns a missing store into a successful-looking insert. The one surviving entry
is the first `put`, which reached real bytecode on the cold path before the call
site cached.

Confirmed independently in one run: `Enumeration<String> ce = m.keys()` returned
a real `$KeyIterator` while `Object o = m.keys()` two bytecodes later returned
`Collections$3` — the same `invokevirtual`, at two call sites, taking two
different routes.

**Consequence: the load-bearing sentence under the 47 held retirements rests on
a measurement that cannot currently be taken.** Until the force gate consults the
dial (§7 NOMINATION 1), arming it for any collection receiver measures the gate,
not the VM.

### 3.2 And a mirror is not an authority

Even with the gate fixed, §4's change would not unblock retirement. Retiring
`get`/`putVal` means real bytecode CASing into `table` through
`U.compareAndSetReference`; a table that is *rebuilt from the segments on every
`keys()`* is a **refresh-on-read mirror**, and a writer that mutates the mirror
writes into a copy the segments never see. Retirement needs the authority moved,
which means one flat table replacing the 4/16-way segmentation — and that trades
away the lock striping, on a VM whose CHM is concurrency-critical. That is a
separate, larger piece of work and it is not what §4 does.

**Net for P2: 0 of the 47 move on this record.** What changes is that the reason
they are held is now measured and specific rather than inherited.

---

## 4. The change (`native-collections/src/lib.rs`, WRITTEN, NOT BUILT)

Three additions plus two rewired natives, all in the `keys()`/`elements()` read
path. **No mutator is touched**, so `put`/`get`/`remove`/`compute*`/`size` are
byte-for-byte unchanged and the H2 throughput path is untouched.

* `chm_real_class(ctx, name)` — resolves a real image class and **never a
  fabrication**: `is_class_synthetic_stub` first, then `class_id_by_name`, and
  only then `ensure_class_initialized` with the resolved name verified to match
  (that call fabricates a stand-in rather than failing, so `Ok` alone is not
  evidence).
* `chm_publish_real_table(ctx, this) -> Option<ObjectRef>` — walks the segments,
  builds a real `$Node[]` sized by HotSpot's own rule (start 16, double while
  `n > cap - cap/4`; verified against HotSpot: 6 → 16, 200 → 512), fills it with
  real `$Node` objects, and publishes it into the receiver's real `table` slot
  via `receiver_table_slot`, plus `baseCount` (`long`) and `sizeCtl` resolved
  **by name**.
* `chm_real_dual_iterator(ctx, this, values)` — constructs java.base's own
  `$KeyIterator` / `$ValueIterator` over that array through
  `([L…$Node;IIIL…ConcurrentHashMap;)V` with `(tab, n, 0, n, this)`, the exact
  argument tuple `keys()` uses and the one §2 measured working.

`native_chm_keys` / `native_chm_elements` try the real carrier and fall back to
`make_snapshot_enumeration` verbatim on `Ok(None)`.

### 4.1 Node hashes are masked to `HASH_BITS`

`Traverser.advance()` reads `e.hash < 0` as *"this bin head is a
Forwarding/Tree/Reservation node"* and walks off it. The stored
`NODE_FIELD_HASH` is masked `& 0x7fff_ffff` — the JDK's own `spread()` output
form — so java.base can never mistake one of our nodes for a control node.

Note for anyone extending this: **iteration correctness does not depend on the
bucket index at all** — the traverser visits every bin. The index only matters if
a future change wants real `get()` bytecode to find entries.

### 4.2 The fallback is total, deliberately

No real `$Node` in the image, an over-large table, or a **primitive in a slot a
real `$Node` declares as a reference** (descriptor coercion would silently store
a different value — the `a-defaulting-reader-turns-a-wrong-type-into-a-quiet-wrong-write`
shape) all return `None`, and the caller keeps today's behaviour exactly. A
partially-materialised `table` is a populated-looking lie, which is the one
outcome worse than the `null` it replaces.

### 4.3 Concurrency, stated explicitly

No new sharing, no segment lock taken. The walk is the one
`chm_collect_all_keys` already performs, so it inherits that walk's existing weak
consistency. The array is private until the store to `table` publishes it. An
iterator already handed out does not observe later mutations — which is precisely
what CHM specifies of its weakly-consistent iterators. Two concurrent `keys()`
calls each build their own array and each keep it; the last store to `table`
wins. **What this does not provide is atomicity of the snapshot against a
concurrent writer — no more and no less than the snapshot enumeration it
replaces.**

### 4.4 The one thing to watch when it is first built

`table` stops being `null` on a CHM. Audited before writing: all 18
`map_state(ctx, this)` / `map_buckets_slot(ctx, this)` sites in
`native-collections/src/lib.rs` — `map_collect_keys` (`:8219`),
`map_collect_values` (`:8273`), `map_collect_entries` (`:8317`),
`native_map_size` (`:9055`), `native_map_is_empty` (`:9084`),
`native_map_contains_key` (`:10522`), `native_map_contains_value` (`:10678`),
`native_map_clear` (`:10757`) — **all route a CHM receiver away via
`is_chm_receiver` / `properties_backing_chm` before reading the slot**, and the
remainder (`map_resize_inner`, `native_hashmap_get_*`,
`native_map_put_evict_pinned`, `native_map_remove_pinned`, `try_hm_int_fast_put`,
`native_map_equals`) are HashMap-family paths that a CHM reaches only with the
*segment* as receiver. Not audited and worth a look on first build:
`vm/src/jit/helpers.rs:11152`-11251 (`CONCURRENT_HASHMAP_CLASS_CACHE`,
`jit_concurrent_hashmap_get_direct`).

Also note **both compatibility modes change**: the carrier becomes the real JDK
class in `--real-jdk` too, where it was the fabricated
`cratonvm/internal/SnapshotEnumeration`. That is strictly closer to HotSpot, but
a test that froze the old carrier's class name will move.

---

## 5. `RJdkEnumerations` — not run, and line 374 is not the first red

The fix is unbuilt, so the vector cannot be re-run. On the **current** binary,
`--jdk-only`, the class fails **before reaching line 374**:

```text
  AssertionError: Hashtable.keys()'s carrier must be the JDK's own dual
                  Enumeration/Iterator, got java.util.Collections$3
    at RJdkEnumerations.carriersAreTheJdksOwnDualInterfaceEnumerators(:346)
```

HotSpot: `PASS RJdkEnumerations (70 checks)`.

So **line 346 (`Hashtable`) and line 374 (`CHM`) are two separate reds and both
fixes must land** for the vector to pass. This lane's change addresses 374/377
only.

The sibling `Hashtable` lane's fix **is already in this worktree's source** —
`native-builtins/src/deprecated_util.rs` is +206 lines with
`HASHTABLE_ENUMERATOR_CLASS` / `HASHTABLE_ENUMERATOR_CTOR` and the
`getEnumeration(int)` selectors — but it is **not in the binary**, which still
answers `Collections$3` and logs `deprecated_util.rs:1201` refused on
`java/util/Enumeration$Impl`. One rebuild therefore carries both fixes, and
`RJdkEnumerations` is the joint gate on them: it should be run once after that
build and its verdict attributed to the pair, not to either lane alone.

---

## 6. Reproducing

Probes live in the lane scratchpad, not the worktree (an untracked file in the
worktree trips the source-witness gates). All are single-file, no dependencies:

* **`ChmLayout.java`** — §1's table. Needs both `--add-opens`.
* **`ChmSlots.java`** — §1.1, reads `AbstractMap.keySet` explicitly.
* **`ChmDrive.java`** — §2, the decisive one. Builds a real `$Node[]` and drives
  java.base's own `KeyIterator`/`ValueIterator` over it.
* **`ChmReal.java`** / **`ChmItf2.java`** — §3.1, with
  `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/concurrent/ConcurrentHashMap`.

---

## 7. Nominations (outside this lane's owned files)

**NOMINATION 1 — the force-native gate silently defeats the retirement dial.**
This is the higher-value of the two: it is the instrument the entire Phase 2
migration is supposed to be measured with.

`vm/src/runtime/interpreter/native_override.rs`, in
`force_native_over_real_jdk_bytecode`, the collection cluster ending at line
2542:

```rust
            | "keys"
            | "elements"
    ) {
        return true;
    }
```

→

```rust
            | "keys"
            | "elements"
    ) {
        // §1.4's retirement dial has to win here or it measures this gate
        // rather than the VM. Arming it for `java/util/concurrent/Concurrent\
        // HashMap` today retires `<init>` (absent from this cluster) and keeps
        // `put`, so the map has no segments and `native_chm_put`'s
        // `chm_segment_for -> None` arm drops 5 of 6 entries while returning
        // "no previous mapping" — measured 3/3 identical, W7-96 §3.1.
        return !cratonvm_vm::runtime::env_cache::enforce_shadow_scope().covers(class_name);
    }
```

The same guard belongs on the two mirrors, `vm/src/vm/vm_exec.rs:22199`-22229 and
`vm/src/runtime/interpreter/dispatch_virtual.rs:3420`-3445. (Path/import spelling
is a guess from outside the crate; the `enforce_shadow_scope()` /
`EnforceShadowScope::covers` API at `vm/src/runtime/env_cache.rs:514`/`:507` is
exact.)

**NOMINATION 2 — `native_chm_put` turns a dropped store into a successful
insert.** In this lane's own file, so it is a nomination only in that it is not
part of the carrier fix and deserves its own adjudication.
`native-collections/src/lib.rs:46498`:

```rust
            None => Ok(Some(Value::Object(None))),
```

A CHM with no segments is not a CHM with no previous mapping — it is an
un-constructed receiver, and answering `null` makes `put` claim a fresh insert
for an entry that was never stored. `remove`, `put_if_absent`, `compute*` and
`replace*` share the shape (`:46505` onward). Throwing (or at minimum a
rate-limited diagnostic) would have made §3.1 self-diagnosing instead of looking
like a real-bytecode CAS bug for the first hour.

---

## 8. Residuals

1. **The retirement authority move is untouched.** §3.2. One flat table vs the
   4/16-way striping, on the VM's most concurrency-critical collection. Nothing
   here reduces that work; it only removes the "the shape does not work" fear
   from in front of it (§2).
2. **`AbstractMap.keySet` is still type-punned with an `Object[]` on every
   CratonVM CHM** (§1.1). Not fixed here — moving the segments root off slot 0 is
   a mutator-path change. Real `AbstractMap.keySet()` bytecode reaching a CHM
   would get an `Object[]` where it expects a `Set`; it does not fault today only
   because CHM overrides `keySet()` and that override is natively shadowed. This
   is the `map_buckets_slot` fix (`:7101`) not yet applied to CHM.
3. **`keySet().iterator()` still answers `Arrays$ArrayItr`.** `native_ksv_iterator`
   (`:48166`) was deliberately left alone to keep this diff to the RED. It could
   now build a real `$KeyIterator` the same way, since §4 gives it a populated
   `table`; its `SnapshotItrRoute::SetLike` write-through would need re-deriving
   against `BaseIterator.remove()` first.
4. **`instanceof` spelling divergence, unexplained and possibly nothing.**
   `ChmItf2` F1 vs F2 showed the same `instanceof java/util/Iterator` reading
   `true` and `false` in one run — fully explained by the two `m.keys()` calls
   taking different dispatch routes (§3.1), but the underlying "cold call site
   yields, cached call site does not" asymmetry was not chased to its source and
   is worth confirming is only the force gate.
5. **`counterCells` / `nextTable` / `transferIndex` are left null** by §4. They
   match HotSpot for every size measured in §1, so nothing depends on them today;
   a future authority move will need `counterCells` for contended size counting.
6. **The change is unbuilt.** It must compile and the `--jdk-only` corpus must be
   verdict-neutral apart from `RJdkEnumerations` before any of §4 is believed.

---

# §9 — RE-MEASURED AND ACTED ON, 2026-08-22 (WORKER 2)

This record was written 2026-08-12 against a binary that no longer exists, and
its §4 was **written but not built**. Everything below is measured on
`/data/vm-com2w-nom`, built from the branch tip on
`azureuser@20.80.105.49`, `--jdk-only`, one case per process, against
HotSpot 25.0.3+9. Arms with all of it: **107/107, 107/107, 67/67, rc=0**;
the collection LAW diff against HotSpot is **IDENTICAL, 63 of 63**.

## §9.1 §4 IS BUILT, AND §1's TABLE HAS MOVED

§4's carrier change landed and works. §1's table is re-measured:

| field | | empty | 6 entries | 200 entries |
|---|---|---|---|---|
| `table`, **written only** | HotSpot | `null` | `len=16 used=6` | `len=512 used=198` |
| | CratonVM | `null` ✓ | **`null`** | **`null`** |
| `table`, **after a bulk read** | HotSpot | `null` | `len=16 used=6` | `len=512 used=198` |
| | CratonVM | `null` ✓ | **`len=16 used=6`** ✓ | **`len=512 used=198`** ✓ |
| `baseCount` / `sizeCtl` after a bulk read | both | 0/0 vs 0/**16** | 6/12 ✓ | 200/384 ✓ |
| `keys()` / `elements()` | both | `$KeyIterator` / `$ValueIterator` ✓ | ✓ | ✓ |
| `keySet().iterator()` | HotSpot | `$KeyIterator` | | |
| | CratonVM | **`Arrays$ArrayItr`** | | |

So §1's "CratonVM `null` everywhere" is now true only of a map that has been
WRITTEN AND NEVER BULK-READ. Two rows are new:

* **`sizeCtl` is 16 on an empty CratonVM CHM where HotSpot has 0.** Not in §1's
  original table; harmless (it is the pending table size, which is what HotSpot
  parks there for a capacity-constructed map) but not identical.
* **A read does not survive the next write.** `keySet()` publishes the mirror,
  then one `put`, then reflect: HotSpot 7 entries, CratonVM **6**. That is §3.2's
  *"a mirror is not an authority"* as a measurement rather than an argument, and
  it is the sharpest statement of what the authority move has to fix.

## §9.2 §7 NOMINATION 1 — IMPLEMENTED, and §3.1's conclusion needs correcting

REPRODUCED first on the pre-fix binary, ten days after §3.1, three runs
identical — six `put`s of distinct keys into a fresh CHM:

```text
  dial OFF     putFresh=6/6  size=6  getHits=6/6  keySetIter=6  table=used6
  dial ARMED   putFresh=6/6  size=1  getHits=1/6  keySetIter=1  table=used1
  HotSpot      putFresh=6/6  size=6  getHits=6/6  keySetIter=6  table=used6
```

The dial now wins over all three force paths. **Placed at the TOP of each rather
than on the one cluster §7 names** — every cluster below has the same problem,
and a per-cluster condition would have to be repeated correctly in three files.
`EnforceShadowScope::Off::covers()` is `false`, so an unarmed run does not change
by one dispatch; the arms and the LAW diff confirm that.

**AND §3.1's conclusion is half right.** It says the load-bearing measurement
under the 47 held retirements *"cannot currently be taken"* because the gate
defeats the dial. With the gate fixed, the armed map is still MIXED-ROUTE:

```text
  dial ARMED, after the fix   putFresh=6/6  size=1  getHits=6/6  keySetIter=1
```

`get` finds all six in the native store while `size()`/`keySet()` read the real
`table` and answer 1. **The gate was ONE of the doors, not the door.**
`WORKER-1`'s brief names the rest: `jdk_only_enforce_shadow_for` has exactly one
live call site, inside `resolve_step1_native`, so arming a class arms only its
cold step-1 dispatches — the warm invoke-cache, the force interceptor and
reflective `Method.invoke` are outside it by construction.

**The instrument is one door better, not fixed, and this record must not be read
as having unblocked the 47.** What changed is that arming the dial no longer
DESTROYS DATA while reporting success: the next lane gets an honest wrong answer
instead of a silent one.

## §9.3 §7 NOMINATION 2 — IMPLEMENTED

Eleven sites ended in `None => Ok(Some(Value::Object(None)))`, which on `put`
means "no previous mapping" — a fresh insert. `chm_segment_for_mut` MATERIALISES
the segments instead, which is better than throwing: a segment-less CHM is an
UNINITIALISED object, not a corrupt one, and the JDK's own CHM allocates its
table lazily on first `put` for the same reason.

Routed to the EIGHT inserting sites only. `remove`, `replace`,
`computeIfPresent` and every reader are correct no-ops on an empty map, and
materialising on a read path would allocate where it must not.

MEASURED armed, after: `getHits` 1/6 → **6/6**, `keySet` `[]` → `[k1..k5]`.

## §9.4 THE AUTHORITY MOVE — REFUSED, with the audit §3.2 never ran

§8.1 keeps the authority move as the headline residual and prices it as *"one
flat table vs the 4/16-way striping"*. **That price is too low.** The striping is
not 16 Java monitors; `native-collections/src/lib.rs:3425`-3500 carries a
lock-free-read layer — per-segment `parking_lot::RwLock`s, resize epochs,
mutation epochs and an active-mutation counter, with readers validating against a
snapshot. A flat table has to reproduce all of it with per-bin locking PLUS
forwarding-node semantics, so a reader mid-resize does not miss entries. That is
CHM's own design and it is the part §8.1 does not count.

Against that cost, here is the exposure — **23 readers of a CHM written and never
bulk-read**, one case per process, against HotSpot:

```text
  MISMATCH  reflective read of `table`         HS len=16 used=6   CV null
  MISMATCH  a SUBCLASS reading CHM.table       HS len=16          CV null
  agree     size, isEmpty, get, containsKey, containsValue
  agree     forEach, keySet, values, entrySet, toString
  agree     new HashMap<>(m), new TreeMap<>(m), Map.copyOf, unmodifiableMap
  agree     entrySet().stream(), equals, hashCode
  agree     serialization round trip
  agree     reduceValues, searchKeys, mappingCount, keySet(V), elements()
```

**21 of 23 agree, and both mismatches are the same act — a direct reflective read
of the field.** Every functional reader agrees, including the five CHM-specific
bulk operations (`reduceValues`, `searchKeys`, `mappingCount`, `keySet(V)`,
`elements()`) that walk the table on HotSpot.

So the trade is: re-engineer the lock-free-read layer of the VM's most
concurrency-critical collection, to correct a field that 21 of 23 readers do not
consult, on a corpus whose arms are almost entirely single-threaded — i.e. the
one class of bug this change could introduce is the one the gate cannot see.
**Refused, and the refusal is the same standard `WORKER-2-NOTE-1` §9 applied to
`HashMap`'s Integer-keyed side store: bound the exposure, then decline.**

### What it would take, for whoever does take it

1. `table` becomes the store: `map_carrier_class_for_receiver` answers
   `ConcurrentHashMap$Node` for a CHM, and `map_buckets_slot` already resolves
   `table` by name, so the `HashMap` bucket machinery can back it.
2. Locking moves from 16 segment monitors to the bin head (`synchronized (f)`,
   CHM's own rule), with the empty-bin insert guarded on the map. This is FINER
   striping than today, not coarser — the one place §8.1's pricing is pessimistic.
3. Resize needs forwarding nodes, or the epoch validation at `:3425`-3500
   re-derived for a single array.
4. `baseCount` is a `long`; `set_map_size` writes an `Int`, so `map_size_slot`
   needs a long-aware arm.
5. 31 functions reach the store (`chm_segment_for` / `chm_all_segments`), and 21
   `is_chm_receiver` guards currently route CHM AWAY from `map_state` — §4.4
   audited them in the other direction and that audit has to be re-run inverted.
6. It must be ONE change. A half-migration is two stores that disagree, which is
   the defect this record exists to describe.

**Only then do CHM's and `Properties`' six iterator-class cells become
closable** (`WORKER-2-NOTE-1` §9a.4): a real `$KeyIterator` needs a `remove()`
that writes through, and today it would mutate the mirror.

## §9.5 Residual status after this pass

| §8 item | state |
|---|---|
| 1. authority move | **refused with the audit above**; specified in §9.4 |
| 2. `AbstractMap.keySet` type-punned `Object[]` | still open — MEASURED again, `CHM_FIELD_SEGMENTS = 0` still aliases it; 6 references, so contained, but it dies with the authority move |
| 3. `keySet().iterator()` is `Arrays$ArrayItr` | still open, and now known to be BLOCKED on §9.4 rather than merely undone |
| 4. `instanceof` spelling divergence | explained: §9.2 confirms mixed-route dispatch under an armed dial |
| 5. `counterCells`/`nextTable`/`transferIndex` null | still matches HotSpot at every measured size |
| 6. "the change is unbuilt" | **closed** — built, run, 107/107 ×2 and 67/67 |
