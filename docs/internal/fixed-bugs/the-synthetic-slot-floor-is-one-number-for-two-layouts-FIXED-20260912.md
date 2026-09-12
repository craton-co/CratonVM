# The synthetic slot floor was ONE number serving TWO layouts — FIXED 2026-09-12

**Status: FIXED.** Opened 2026-09-11 as
`docs/known-issues/perf/the-synthetic-slot-floor-is-one-number-for-two-layouts-20260911.md`,
itself the successor to
`docs/internal/fixed-bugs/jdk-collection-classes-are-padded-to-a-synthetic-stub-floor-FIXED-20260911.md`
§6. All seven rows are closed, the six factories it listed as still pinning
`HashSet` are converted, and the screen it asked for is a gate.

TWO of the seven took a second change the page did not predict, and in both the
floor turned out to be hiding an object that should not have existed:

* §5 -- once the floor came off `CopyOnWriteArraySet`, what was left was not
  padding but a `LinkedHashMap` standing in the slot the real class declares as
  `CopyOnWriteArrayList al`, and `removeIf`, the one method of that class's
  surface this VM does not register, had been raising `NoSuchMethodError` on it.
* §6 -- `Properties` was allocating a 144-byte `Object[16]` in its
  constructor, into a `table` that not one of its methods reads or writes and
  that HotSpot leaves null. §7 is a silent `remove` defect that the probe
  written to make that change safe caught on the way past.

**Verified on:** Windows 11, JDK 25 Temurin `25.0.3+9`, branch
`claude/synthetic-slot-floor-fix-8e9672`, with a baseline worktree built from
the SAME commit for the synthetic-JDK comparison. Twice: once off
`dev@2405991d1`, and again after merging `dev@3e8442e5f` — same numbers, same
verdicts, same 127 `field index OOB`, so nothing in the intervening upstream
work interacts with the exemption.

## 1. The measurement that closes it

`probes/CollectionShapeCause.java`, retained heap per EMPTY instance, 20 000
instances, Generational. HotSpot is `java -Xmx3g --add-opens
java.base/java.util=ALL-UNNAMED`, same probe, same run.

| class | HotSpot | before | after | before | after |
|---|---|---|---|---|---|
| `java.util.Properties` | 120.3 | **544.0** | **80.0** | 4.52x | 0.67x |
| `java.util.concurrent.ConcurrentLinkedDeque` | 48.0 | **120.0** | **72.0** | 2.50x | 1.50x |
| `java.util.concurrent.CopyOnWriteArraySet` | 56.1 | **136.0** | **72.0** | 2.42x | 1.28x |
| `java.util.concurrent.ConcurrentLinkedQueue` | 48.0 | **112.0** | **64.0** | 2.33x | 1.33x |
| `java.util.ArrayDeque` | 112.1 | **232.0** | **184.0** | 2.07x | 1.64x |
| `java.util.HashSet` | 64.0 | **128.0** | **88.0** | 2.00x | 1.38x |
| `java.util.LinkedHashSet` | 80.1 | **152.0** | **112.0** | 1.90x | 1.40x |

All seven land inside the 1.2x-1.7x band the rest of the collections already
occupy, which is reference width and a separate subject.

Two of the seven took a SECOND change to get there, and in both cases what the
floor left behind was not padding but an object that should not have existed.
`CopyOnWriteArraySet` (§5) went 136 -> 112 on the floor and 112 -> 72 once the
`LinkedHashMap` standing in its `al` slot was replaced by the list the class
declares. `Properties` (§8) went 544 -> 224 on the floor and 224 -> **80** once
the `Object[16]` its constructor built — into a `table` that no `Properties`
method reads or writes — was deferred the way HotSpot defers it. It is the one
row that now sits BELOW HotSpot, and honestly so: HotSpot allocates a
`ConcurrentHashMap` in that constructor and this VM keeps its String entries in
a Rust side-table, off the Java heap the probe measures.

Every OTHER row of that table is byte-identical before and after: `ArrayList`
32.0, `HashMap` 64.0, `LinkedHashMap` 88.0, `TreeMap` 80.0, `TreeSet` 24.0,
`IdentityHashMap` 584.0, `Hashtable` 168.0, `ConcurrentHashMap` 104.0,
`CopyOnWriteArrayList` 48.0, `LinkedBlockingQueue` 440.0, and the rest. The
four-entry table moves with the empty one and nowhere else: `ArrayDeque+4`
232 -> 184, `HashSet+4` 464 -> 424, `LinkedHashSet+4` 552 -> 512,
`ConcurrentLinkedQueue+4` 240 -> 192 — and `CopyOnWriteArraySet+4`
**512 -> 120** against HotSpot's 88.3, which is §5 and is the largest single
number on this page. `Properties+4` is new to that table (§8 added the row) and
goes **1128 -> 984**: the same 144 bytes the empty row lost, which is the
measurement proving the array was carrying nothing rather than being moved.

`CRATONVM_DBG_LAYOUT=1` now reports a compact layout for all seven, where it
used to report the refusal:

```text
[layout] java/util/HashSet            cid=65  body=8  refs=1 fields=1
[layout] java/util/LinkedHashSet      cid=129 body=8  refs=1 fields=1
[layout] java/util/concurrent/CopyOnWriteArraySet   cid=147 body=8  refs=1 fields=1
[layout] java/util/ArrayDeque         cid=139 body=16 refs=1 fields=3
[layout] java/util/concurrent/ConcurrentLinkedQueue cid=536 body=16 refs=2 fields=2
[layout] java/util/concurrent/ConcurrentLinkedDeque cid=538 body=16 refs=2 fields=2
[layout] java/util/Properties         cid=132 body=64 refs=6 fields=10
```

The whole-run padded-class count over that probe goes 99 -> 92, and the seven
that left are exactly these seven. Nothing else changed sides.

## 2. The fix: the floor is now asked WHERE, not just HOW MUCH

`synthetic_stub_fields` is one number read in two places that mean different
things. It DEFINES the layout of a fabricated stub — there the natives that
serve the class index it from 0 and nothing else declares those slots — and it
FLOORS the layout of a class defined from real class-file bytes, padding it up
to the width native code was written against.

The second reading is load-bearing for `java.net.InetSocketAddress`: one
declared instance field (`holder`), and a native `<init>` that writes raw
synthetic indices on the REAL class. It was a fiction for these six.

`ClassManager::apply_synthetic_floor` (`classloading/src/class_manager.rs`) is
now the ONE place the floor is applied. Both callers — `define_class_with_options`
and the stub-upgrade path — go through it, so they cannot disagree about the
floor or about who is exempt from it; each used to open-code the same `max`.
A fabricated stub never reaches it at all (`synthesize_stub_class` takes its
fields straight from the table), which is why this change cannot move
synthetic-JDK mode, and why the arm that had to be re-run is the real-JDK one
plus a verdict-neutrality check.

`FLOOR_EXEMPT_CLASSES` beside it carries the six, each with the instance-field
extent the REAL chain declares:

```rust
("java/util/HashSet", 1),                               // `map`
("java/util/LinkedHashSet", 1),                         // inherited, declares none
("java/util/concurrent/CopyOnWriteArraySet", 1),        // `al`
("java/util/concurrent/ConcurrentLinkedQueue", 2),      // `head`, `tail`
("java/util/concurrent/ConcurrentLinkedDeque", 2),
("java/util/Properties", 10),                           // Hashtable's 8 + 2
```

That second number is not used to size anything. It is what
`floor_exempt_real_extent_disagreement` compares the loaded class against, so a
JDK whose shape has moved out from under the screen prints a line naming the
class and both counts instead of silently keeping an exemption that was measured
against a different layout.

**`java.util.ArrayDeque` is deliberately not in that list.** It needed no
exemption, only a correction: its fourth slot held a `size` that `ad_state`
stopped reading on 2026-08-30, when the count started being derived from
`head`/`tail` the way the JDK's own `size()` derives it. Nothing had written
slot 3 since; the only reader left was a `>` receiver-width bound in
`collect_collection_elements`, which now asks for the three slots that exist.
The table declares `elements`/`head`/`tail` — exactly the real class, in the
real order — so `ArrayDeque` is not padded in either mode.

## 3. The six factories, and why they had to go first

The open record named six sites that built an array-backed set by writing raw
absolute slots on a real `HashSet` receiver. Each wrote the MAP layout — element
array at absolute 0, count at 1, sometimes a capacity at 2 — on a class whose
one real instance field is `map` (`Ljava/util/HashMap;`).

**That was a live defect independent of the floor, and in several of these the
defect was the whole point of the method.** Every real `Set` method
dereferences `map`, so the object came back with an `Object[]` where a `HashMap`
belongs, and `size()`/`iterator()`/`contains()` answered for an EMPTY set
whatever the array held:

```text
  native-builtins/src/jmx.rs                       queryNames/queryMBeans fallback
  native-builtins/src/phases_late/net_channels.rs  Selector.selectedKeys()
  native-builtins/src/phases_late/net_channels.rs  Selector.keys()
  native-builtins/src/phases_late/reflect_invoke.rs  ModuleLayer.modules()
  native-builtins/src/util_time.rs                 ZoneId.getAvailableZoneIds()
  native-builtins/src/servlet.rs                   Selector.selectedKeys()/keys()
```

`ModuleLayer.modules()` is the one with a filed history: the OTHER registrar for
that same method was fixed in
`docs/internal/fixed-suite-bugs/CRATONVM_BUGS/BUG-T-modulelayer-modules-synthetic-hashset.md`
(Tomcat's web-fragment scan NPE'd on a null `iterator()`), and this is its twin,
which had kept the shape the whole time.

All six now go through **one** helper,
`native-builtins/src/lib.rs::build_real_hash_set`: allocate the real width, run
the class's own `<init>`, and `add` each element. It is correct in both modes —
in synthetic-JDK mode `native_hs_init` allocates the backing map and
`hs_set_backing_map` places it at the slot `hs_map_slot` names, which is
absolute 0 in both — and it is GC-safe, pinning each element before the first
allocation and re-reading the set across every `add`.

Three further sites asked for a fabricated width and wrote NONE of it, which is
bookkeeping rather than a shape, and the gate reads bookkeeping as a claim:
`system_properties_object` (16 slots of `Properties`, whose state is an
identity-keyed side table) and two Spring empty-set fallbacks (3 slots of
`HashSet`). They ask for the real width now; `alloc_concurrent_synthetic` takes
`max(n, real)`, so a fabricated stub still gets its full count.

The last one was `build_real_layout_string_hashset`'s own fallback arm, which
wrote `(data_array, size)` whenever the real `HashMap` layout could not be
resolved. It delegates to `build_real_hash_set` now. There was no mode left in
which the second shape was right, and it was the last unconditional raw-slot
write on `java/util/HashSet` in the tree.

`wildfly_security::count_carrying_hash_set` is untouched and is the model for
the one arm that may still write the fabricated shape: it asks
`ctx.is_class_synthetic_stub("java/util/HashSet")` first, so the three-slot
shape is reachable only where it IS the layout.

## 4. The screen, as a gate

`t9d_floor_exempt_classes_have_no_oversized_factories` (`vm/tests/tier1_tests.rs`)
is the companion to T9C, running the other way. T9C asserts a fabricated table
is at least as wide as its own factories; T9D asserts a floor-EXEMPT class has
no factory wider than its real layout.

It holds the half of the screen that is mechanical: every literal
`alloc_concurrent_synthetic(_, "cls", n)` site in the tree, whose `n` is the
shape the caller intends to write. A site that has already asked
`is_class_synthetic_stub` is excused, because it knows its receiver is
fabricated. It also refuses a STALE entry — one whose fabricated extent no
longer exceeds the real one pads nothing, so it is a claim about a class rather
than a live exemption.

Written against `dev`, it is RED with exactly the four rows §3 lists as
bookkeeping plus the two remaining `HashSet` factories, and green here. The
second half of the screen — raw `ctx.set_field(obj, N, ..)` on a receiver the
caller did not allocate — is the same grep and is not machine-checkable without
type information; it is covered by the runtime out-of-range reporter in
`gen_heap::set_field` and by §6's probe, and the exemption table's doc states
the rule for anyone adding a class.

`ConcurrentLinkedQueue` is the sharp demonstration that the second half really
is discharged, because it is the one class whose natives would visibly break if
it were not. `native_lbq_init` writes four absolute slots on the receiver and
`native_lbq_size` READS the third; with the floor gone the class has two, so
slots 2 and 3 are off the end and `size()` would answer 0 on a queue holding 40
elements. `CollectionSlotFloor` asserts 40, a full 24-element FIFO drain in
order, and `poll()` on an emptied queue -- and passes. The registrations exist
and do not run: a real `ConcurrentLinkedQueue` is served by its own lock-free
bytecode, and `CollectionShapeCause`'s field dump shows `head` and `tail`
holding real `ConcurrentLinkedQueue$Node` objects, which only that bytecode
builds.

`array_deque_synthetic_table_matches_the_real_class` freezes the §2 narrowing at
three, so it cannot grow back by accident the way `LinkedHashMap`'s floor did.

## 5. `CopyOnWriteArraySet` was backed by the wrong OBJECT, and `removeIf` said so

Taking the floor off left this one class at 2.0x when the other six landed in
the 1.2x-1.7x band, and the reason was not padding. Its object was already
compact (`body=8 refs=1 fields=1`); what it RETAINED was wrong:

```text
  HotSpot   COWAS 56.1  = 16 (object) + 40.1 (CopyOnWriteArrayList + Object[0])
  CratonVM  COWAS 112.0 = 24 (object) + 88.0 (LinkedHashMap)
```

`register_hashset_natives` mirrors the whole `java.util.HashSet` surface onto
`CopyOnWriteArraySet`, on the stated premise that it is one of "HashSet's
real-JDK subclasses that share the same `field 0 = backing map` layout". It is
neither. It does not extend `HashSet`, and the ONE instance field the real class
declares is `private final CopyOnWriteArrayList<E> al` — sitting at exactly that
slot 0. So `native_hs_init` stored a `LinkedHashMap` in a slot declared to hold
a list.

**That is a live defect, not a size one, and the class library itself found it.**
`CopyOnWriteArraySet` declares nineteen methods and this registrar registers
twenty triples; `removeIf` is one it does NOT register, so its real body ran:

```text
  cowSet.removeIf(p)
    -> NoSuchMethodError:
       java.util.LinkedHashMap.removeIf(java.util.function.Predicate)
```

The other eighteen looked correct only because a native stood in front of each
of them. Which methods are silent is therefore a function of which triples this
registrar happens to carry — which is why the guard for it (§6) is the whole
declared surface rather than the one method that broke.

### The fix: the class is served by its own bytecode

`cow_set_route` (`native-collections/src/lib.rs`) sits at the top of each of the
twenty natives — the placement `ksv_route` already uses, so the INTERFACE-level
registrations (`java/util/Set.size()` and friends) are guarded by the same test
as the exact-class ones. When the receiver is a real `CopyOnWriteArraySet` it
runs that receiver's own method, bytecode-only, on its own class.

Real, not a mode flag: `is_real_cow_array_set` asks whether the receiver's class
resolves the field NAME `al`. A fabricated stub declares `_f0`/`_f1` and keeps
the map surface, because there the map surface IS the implementation. It is the
same question `hs_map_slot` and `props_defaults_slot` ask about their own
receivers, and `hs_map_slot` now declines a real one for the same reason —
answering `HS_FIELD_MAP` for it would hand every reader in that file a list to
call `native_map_*` on.

Delegation rather than a second implementation, because the object underneath is
already right: `CopyOnWriteArrayList` writes `lock` and `array` BY NAME
(`cowal_ensure_lock_and_array`), had its `<init>` override deliberately removed
so the JDK constructor runs, and measures 48.0 against HotSpot's 40.0 — the best
ratio in the collection table. The JDK's own `CopyOnWriteArraySet` bodies are
thin forwards to `al`, so they already had a working object to forward to.

One method could not go that way. `stream()` is a `Collection` DEFAULT method
whose body builds a real `java.util.stream` pipeline over a real `Spliterator`,
and this VM's streams are a synthetic carrier — so it takes the ELEMENTS (via
`toArray()`, which IS delegated) and builds the carrier, which is what every
other `*_stream` native in that file does.

### What it bought

```text
  empty         112.0 -> 72.0    against HotSpot 56.1   (2.00x -> 1.28x)
  four entries  512.0 -> 120.0   against HotSpot 88.3   (5.80x -> 1.36x)
```

The filled row is the bigger half and was not visible before this work:
`CollectionShapeCause`'s four-entry table had no `CopyOnWriteArraySet` row at
all, so the class's worst number — a `LinkedHashMap` with four entries, 488
bytes, where HotSpot holds a six-element `Object[]` — had never been measured.
It has a row now.

## 6. `Properties` built a bucket array that no `Properties` method reads

The floor took this class from 544 to 224 (§1) and left it at 1.86x, the last
of the seven still outside the band. As with §5, the remainder was not padding:

```text
  HotSpot   Properties 120.3 = 16 (object) + ~48 (body) + ~56 (ConcurrentHashMap)
  CratonVM  Properties 224.0 = 16 (object) +  64 (body) + 144 (Object[16])
```

Both constructors called `map_init_eager`. `Properties` is deliberately
EXCLUDED from `CF_HASHTABLE_LAYOUT` -- JDK 25 backs it with a side
`ConcurrentHashMap`, not with `Hashtable`'s buckets -- so that call fell through
to the GENERIC arm of `map_init_inner`, which allocates an `Object[16]` into the
receiver's `table`. 16 bytes of header and 128 of references: 144, which is
exactly 224 minus the 80 the object itself costs.

**Nothing ever wrote to it.** `map_carrier_class_for_receiver` had already said
so in a comment, with the measurement attached -- a `Properties` bucket table
reports `occupied=0` with `size=2` -- and this is the change that acts on it.

### The risk the old comment named, and why it does not apply

Both call sites carried the same note: *"leaving it null would make `map_state`
report no buckets on a receiver whose native put path does not go through
`map_resize`."* That is a real hazard in general. It is not this class's, on
three independent counts, and the third is the one that settles it:

* **Nothing writes that array.** `register_properties_sidetable`
  (`native-builtins`) runs AFTER `register_collections_natives` in BOTH
  `vm_init` arms and OVERWRITES every Map method on `java/util/Properties` --
  `put`, `get`, `remove`, `clear`, `size`, `isEmpty`, `containsKey`, `keySet`,
  `values`, `entrySet`, `keys`, `elements`, `contains`. Not one of those bodies
  touches a bucket table: a String->String pair goes to the Rust side-table,
  anything else to the real `map` CHM, which `put_non_string_into_chm` creates
  ON DEMAND.
* **The one path that COULD reach the buckets already handles null.**
  `native_map_put_evict_pinned` opens with
  `if initial_buckets.is_none() || size + 1 > (cap * 3) / 4 { map_resize(..) }`
  -- the branch a JDK-bytecode-constructed `LinkedHashMap` has always taken --
  so even a receiver that somehow reached the generic put would materialise its
  table on the first insert rather than drop the write. `map_state`'s capacity
  fallback reads `threshold` when there is no table, and the lazy arm sets it to
  0, so it answers `MAP_DEFAULT_CAPACITY` -- the same 16 the eager array had.
* **The busiest `Properties` in this VM has ALWAYS had a null table.**
  `system_properties_object` allocates the `System.getProperties()` singleton
  and writes NONE of its slots. `java.home` and every other bootstrap property
  has been read out of a bucket-less `Properties` since that factory was
  written -- including through the `InternalError: null property: java.home`
  incident that `register_properties_sidetable`'s own header records, which was
  a registration-category bug and not a bucket one. This change makes every
  other `Properties` the same shape as the one that was already proving the
  shape works.

HotSpot is the oracle rather than any of the above.
`probes/CollectionShapeCause.java`'s field dump on a real JDK 25 shows
`new Properties()` leaving `table` null, `loadFactor` 0.0 and `threshold` 0,
with the entries in a `ConcurrentHashMap map` -- so the eager array was a
DIVERGENCE as well as a cost. After the change the two VMs agree field for
field on a FILLED one too:

```text
  both VMs, after four setProperty calls:
    map   = java.util.concurrent.ConcurrentHashMap size=4
    table = null
```

### What it bought

```text
  empty         224.0 -> 80.0    against HotSpot 120.3   (1.86x -> 0.67x)
  four entries 1128.0 -> 984.0   against HotSpot 335.4   (3.36x -> 2.93x)
```

The empty row now sits BELOW HotSpot, which is honest rather than suspicious:
HotSpot's constructor allocates the `ConcurrentHashMap` that makes up most of
its 120.3, and this VM allocates it lazily and keeps String entries in a Rust
side-table that is not on the Java heap the probe measures.

The filled row is new -- §1's four-entry table had no `Properties` row, the
same blind spot §5 found for `CopyOnWriteArraySet` -- and it is the measurement
that proves the array was carrying nothing rather than being moved somewhere
else: it falls by the SAME 144 bytes as the empty row, not by less.

### The residual, which is somebody else's number

`Properties+4` is still 2.93x, and it is not a `Properties` defect.

```text
  Properties+4          984.0  vs 335.4  = 2.93x
  ConcurrentHashMap+4   792.0  vs 275.0  = 2.88x
```

On BOTH VMs the per-entry cost of a filled `Properties` is the per-entry cost of
the `ConcurrentHashMap` underneath it (HotSpot: 215.1 per four entries against
the CHM's 210.9; this VM: 904 against 688 plus the CHM object itself). The two
ratios agree because they are the same number. Closing it means narrowing
`ConcurrentHashMap`, which has its own record --
`concurrenthashmap-allocated-its-segments-in-the-constructor-FIXED-20260911.md`
-- and is not in scope here.

## 7. A silent `remove`, which the new probe caught

`probes/PropertiesBacking.java` (§8) was written to make §6 safe, and found a
live defect that predates it. `native_properties_remove` opened with

```rust
let key = read_java_text(ctx, key_obj).unwrap_or_default();
if key.is_empty() { return Ok(Some(Value::Object(None))); }
```

which returns BEFORE `remove_from_properties_backend` for two quite different
keys, and is wrong for both.

`Properties.put` is inherited from `Hashtable` and takes ANY key; only
`setProperty`/`getProperty` are String-typed. `native_properties_put` sends a
non-String key to the real `map` CHM, because the side-table is keyed by a Rust
string. Removing it then did nothing at all, and reported that there had been
nothing to remove:

```text
  p.put(Integer.valueOf(3), "byIntKey");
  p.remove(Integer.valueOf(3))  ->  null   (HotSpot: "byIntKey")
  p.size()                      ->  3      (HotSpot: 2)
```

The entry survived its own removal, and `size`, `keySet` and `containsKey` all
read the CHM, so it went on being reported present. No exception anywhere --
the silent shape the probe's header describes.

The EMPTY String was the second key taking that return, and it needs the
OPPOSITE treatment, which is why the first cut of this fix was still wrong and
the probe caught that too. The two writers disagree about where it goes:
`native_properties_put` sends it to the CHM (its side-table arm is guarded by
`!ks.is_empty()`), while `native_properties_set_property` calls `put_kv_units`
unconditionally and puts it in BOTH. So it is an ordinary key to every reader --
`native_properties_contains_key` looks it up in the side-table like any other --
and takes the ordinary path, which clears both stores. Only a key with no String
form AT ALL may skip the side-table.

The same `unwrap_or_default()` shape appears twice more in that file, in
`native_properties_set_property` and `native_properties_contains_key`. Both were
checked and both are correct: neither returns early, and both fall through to
the CHM.

## 8. How it was validated

Lowering a floor fails SILENTLY — an out-of-range `set_field` is dropped, not
raised — so the real-JDK arm alone proves nothing about the mode the floors
exist for. Both arms were run, against binaries built from the same commit.

* **`probes/CollectionSlotFloor.java` grew the section this needed.** It covered
  `ArrayDeque` and the two `Set` families and nothing else of the seven; it now
  drives `CopyOnWriteArraySet`, `ConcurrentLinkedQueue`/`Deque` (including FIFO
  and deque ORDER, which `size`/`contains` cannot see), `ArrayDeque` past its
  ring-buffer wrap, and `Properties` including the `defaults` chain — the one
  slot on that class a native resolves BY NAME with the fabricated model index
  as its fallback, so it is where a wrong slot shows up first. HotSpot passes
  the extended file.
* **It also stopped hiding its own tail.** Each section runs under `section(..)`,
  which turns a throw into a recorded `ERROR` row instead of the end of the run.
  `LinkedList.indexOf` is absent from the synthetic image and threw at section
  six of fourteen, so the eight sections after it — including every
  floor-exempt family this file exists to cover — were never reached in the arm
  that matters, and their silence read as agreement.
* **Real-JDK:** `PASS CollectionSlotFloor` before and after, with the extended
  sections. The descriptor-coercion census is identical too (total=31,
  `class_id=132 index=8` x30), so the change moved no slot's contents.
  Re-confirmed after §6/§7, on all three probes: `PASS CollectionSlotFloor`,
  `PASS CowSetBacking`, `PASS PropertiesBacking`, with that same census
  (total=31 / x30 for the floor probe, total=16 / x16 for the other two).
* **Synthetic-JDK, the arm that matters:** two binaries, `--features
  synthetic-jdk`, this tree and the UNCHANGED tree at `2405991d1` from its own
  worktree, running the IDENTICAL probe. **Verdict-identical**: the same 14
  outcomes in the same order (the 8 pre-existing mismatches the parent record
  records — `TreeMap.keySet/entrySet`, `IdentityHashMap`, `LinkedHashSet` — plus
  `CopyOnWriteArraySet` and four `ERROR` rows that the new `section` wrapper
  makes visible for the first time in BOTH builds) and the same **127** `field
  index OOB` warnings. Verdict-neutral is the acceptance criterion, not green.
  `CowSetBacking` was run the same way against the same pair and is likewise
  verdict-identical: the same 15 outcomes, which are the fabricated
  `CopyOnWriteArraySet` stub's own gaps and have nothing to do with §5 —
  `cow_set_route` declines a fabricated receiver by construction.
* **§6 and §7 were re-run on the same pair**, rebuilt from `c0bebbde5` — this
  tree and an UNCHANGED worktree at that commit, both `--features
  synthetic-jdk`, all three probes. Verdict-identical on every one, with the
  same `field index OOB` counts on both sides:

  ```text
    PropertiesBacking    35 rows + FAIL      OOB   4 / 4
    CowSetBacking        15 rows + FAIL      OOB   0 / 0
    CollectionSlotFloor  16 rows + FAIL      OOB 127 / 127
  ```

  The 35 are the fabricated `Properties` stub's own gaps and are untouched by
  either change: the stub has no `<init>(int)` and no `list(PrintStream)`, its
  views and enumerations are empty, and its `defaults` chain raises
  `UnsupportedOperationException`. Deferring a bucket table cannot reach any of
  that, and the identical OOB counts are the evidence that it did not.
* **`probes/CowSetBacking.java` is new, and is the §5 half.** Every method
  `javap -p java.util.concurrent.CopyOnWriteArraySet` lists, plus the four it
  inherits, against values a wrong backing cannot produce — insertion ORDER
  throughout, because that is the observable separating a list-backed set from a
  hash-backed one. `CollectionSlotFloor` drives this class through `setFamily`,
  eight checks that a wrong backing passes; this is the file that does not.
  HotSpot passes it unchanged.
* **`vm/tests/cow_array_set_backing.rs`** drives that probe as a gate, and was
  MUTATION-CHECKED rather than assumed: against the pre-§5 binary it FAILS with
  exactly the diagnostic its message describes
  (`NoSuchMethodError: java.util.LinkedHashMap.removeIf`), and against the
  current one it passes. A probe-driving test that has never been seen to fail
  is the vacuous-pass shape `probe_compile_guard.rs` exists about.
* **`probes/PropertiesBacking.java` is new, and is the §6/§7 half.** Every
  method `javap -p java.util.Properties` lists except four the header excludes
  by construction (`loadFromXML`/`storeToXML`, `rehash`, and the two
  package-private serialization hooks), across thirteen sections — including
  one that does nothing but READ a `Properties` whose table was never
  allocated, which is the section that would have failed had §6 broken
  anything. HotSpot passes it unchanged, which is what makes it an oracle.
* **`vm/tests/properties_backing.rs`** drives it as a gate, and like its §5
  counterpart was MUTATION-CHECKED rather than assumed. Against the pre-§6
  binary it FAILS — and the two rows it failed on were not §6's doing but the
  §7 defect, which is how that one came to light. Against the current binary
  it passes.
* **`t9d_floor_exempt_classes_have_no_oversized_factories`:** RED (4 rows) on
  `dev` once the two `HashSet` factories were converted, green here.
  `t9c_synthetic_field_tables_cover_their_factories` stays green throughout.
* **`vm/tests/tier1_tests.rs`:** 58 passed, 0 failed. The selector and
  collection integration tests that cover the converted natives —
  `nio_selector_build_set_gc` (under `CRATONVM_GC_STRESS`), `wave3_c_selector`,
  `collection_view_gated_surface`, `cluster_b_collection_tostring`,
  `map_view_remove_if`, `stub_ratchet`, `synthetic_diff` — all pass.
* **Crate tests (re-run for §6/§7):** `cratonvm-native-collections` 250 / 0
  over 15 targets, `cratonvm-native-builtins` 4254 / 0 plus its four
  integration targets (5 / 8 / 3 / 6), and `cratonvm-vm`'s `tier1_tests` 58 / 0,
  `probe_compile_guard` 2 / 0, `cow_array_set_backing` and `properties_backing`
  1 / 0 each.
* **`regression-suite/run.sh`:** 95 passed, 0 failed, of 95 scheduled.

One gate is RED and was red before this work: `raw_lock_constructions_do_not_grow`
(`native-builtins/tests/lock_discipline_ratchet.rs`) reports 429 raw lock
constructions against a baseline of 428. Reproduced on a pristine worktree at
`dev@2405991d1` and `dev@3e8442e5f` with no local changes, and its own message
forbids the easy disposition ("Do NOT raise the baseline"), so it is filed rather
than touched here. Re-confirmed after §6/§7: still 429 against 428, and the
site it names is `native-builtins/src/classloader.rs:2428`, which neither
section goes near.

One test had to change, and the change is the point rather than an accommodation:
`jboss_jdkspecific::tests::module_get_packages_returns_populated_set_for_java_base`
asserted the element COUNT at `ctx.get_field(set, 1)` — slot 1 of a `HashSet`,
which is off the end of a class with one field, and the fabricated shape
`build_package_set`'s own doc comment explains is wrong. It now declares the real
`HashMap`/`HashMap$Node`/`HashSet` layouts to the mock so the helper takes its
REAL branch, and asserts what that branch promises: `HashSet.map` holds a
`HashMap`, whose `size` is positive and whose `table` is a bucket array.

## 9. Repro

```bash
javac -d out probes/CollectionShapeCause.java probes/CollectionSlotFloor.java
java -Xmx3g --add-opens java.base/java.util=ALL-UNNAMED -cp out CollectionShapeCause 20000
cratonvm -XX:+UseGenerationalGC --Xmx 3g -c out CollectionShapeCause 20000
cratonvm --Xmx 2g -c out CollectionSlotFloor                    # real-JDK: PASS
CRATONVM_DBG_LAYOUT=1 cratonvm --Xmx 2g -c out CollectionShapeCause 100 | grep PADDED
```

```bash
# the arm that matters when touching a floor; needs its own binary, AND the
# unchanged tree built the same way to compare against
cargo build --release -p cratonvm-cli --bin cratonvm --features synthetic-jdk
cratonvm --synthetic-jdk --Xmx 2g -c out CollectionSlotFloor    # expect the 14
```

```bash
javac -d out probes/CowSetBacking.java probes/PropertiesBacking.java
java -cp out CowSetBacking                                      # the oracle
java -cp out PropertiesBacking                                  # the oracle
cratonvm --Xmx 2g -c out CowSetBacking                          # real-JDK: PASS
cratonvm --Xmx 2g -c out PropertiesBacking                      # real-JDK: PASS
```

The §6 numbers need the collector the §1 table was measured under, and the
default is no longer it — without `--XX:UseGc Generational` every row in that
table reads high (`Properties` 320.0 rather than 224.0) and nothing is
comparable to anything:

```bash
cratonvm --XX:UseGc Generational --Xmx 2g -c out CollectionShapeCause 20000
```

```bash
cargo test -p cratonvm-vm --test tier1_tests -- t9c_ t9d_ array_deque_synthetic
cargo test -p cratonvm-vm --test cow_array_set_backing --test properties_backing
CV=<binary> JDK=$(cygpath -m "$JAVA_HOME") bash regression-suite/run.sh
```

A note on running that suite from Git Bash, inherited from the parent record
because it still costs a run: a POSIX `JDK=/c/Program Files/...` is accepted by
the launcher and then rejected behind it, and reports as ENVIRONMENT failures
that look like the change. Pass `JDK=$(cygpath -m ...)`.
