# WORKER-2-NOTE-1 — the collection object model past `HashMap`, measured end to end

**Status: LANDED, BUILT AND RUN.** Unlike `H4-1`, `H16-2` and `H23-2` — which
could not build — every number here was measured on this lane's own binaries
against HotSpot 25.0.3+9 on the same host, `--jdk-only`, **one case per
process**. Claims that are ARGUED say so.

Lane WORKER-2, 2026-08-21. `native-collections/src/lib.rs` only. Acts on
`WORKER-2` §5 rows 1-4, `H23-2` §7.1/§7.2/§7.3, `H4-1` §2.

---

## 0. The baseline the brief states is not the baseline this host has

`WORKER-2` §1 and §7 give the acceptance bar as `105/105`, `104/105`, `65/65`.
MEASURED on a **pristine build of the branch tip** (`22cb4338d`, unmodified),
`TIMEOUT=600`:

```text
  CRATONVM_ARGS=--jdk-only   104 / 105     RJdkOptionalShape
  SUITE=all                  103 / 105     RJdkOptionalShape RJdkFunctionCombinators
  SUITE=core                  64 /  65     RJdkOptionalShape
```

`RJdkOptionalShape` fails **on the pristine build, in all three arms.** It is
not in this file and not in this cluster: the vector's own javadoc attributes it
to nine sites in `native-builtins/src/http2.rs` that model an `Optional` as
`(flag, payload)` and write an `int` presence flag into the slot the real class
uses for `value`. Anyone measuring this tree here should read **104/103/64 as
green** and a fourth failure as theirs.

That is the standing trap applied to a brief rather than to a lane, and it cost
me one cycle of believing I had reddened a vector that was already red. The
control settled it in one run, and the control is cheap — build the unmodified
tip once and keep the binary.

**It is not harness contention and it is not a launch failure. Both were tested
rather than assumed, against H0's own two instruments.**

* `d4ca67040` PID-scopes `regression-suite/.guard-tmp` — exactly the fixed
  shared path trap 8 warns about — so the first hypothesis was a concurrent
  sweep starving my oracle. Merged, rebuilt, re-run: **unchanged, all three
  arms.** Two further discriminators: the second `run.sh` visible in `ps`
  during a sweep is that run's own CHILD (`ppid` matches), not a foreign
  session, and `.guard-tmp` is per worktree rather than per host.
* `dab993033` makes a launch/config failure NAME ITSELF instead of rendering
  as an assertion failure — H0 wrote it after discovering that trap 1's
  *"the VM dies in argument parsing"* was wrong twice over and that the real
  death was JDK-image validation behind a bare `cratonvm rc=1`. That is the
  best available instrument for "is this really an assertion?". Merged and
  re-run: **the vector still classifies as a genuine `AssertionError` thrown by
  CratonVM inside the test**, not as `unclassified:` and not as `no output`.

Three independent reasons to believe the failure, and one of them is the tool
built specifically to catch the alternative.

**ARGUED, not measured, and left for H0:** `ff262645d` records the corpus as
`105/105, 105/105, 65/65`, all five standing failures closed. That measurement
and this one cannot both be of the same thing. The difference this lane cannot
rule out from here is the PLATFORM — H0 measures on Windows against
`cratonvm-r10.exe`; every number in this record is Linux, `azureuser@20.80.105.49`,
built from the source it describes. A per-platform `Optional` shape in
`http2.rs` would explain both results and nothing else I can see would.

**Also standing, also not mine:** `cargo clippy --all-targets -- -D warnings`
cannot pass anywhere in this workspace. `jfr/src/jdk_chunk.rs:432` trips
`clippy::redundant_guards`, and `-p cratonvm-native-collections` does not exempt
it because the flag reaches the dependency build. A lane that treats a red
clippy as its own will chase a file it does not own.

---

## 1. What moved — the whole table, before and after

MEASURED. `before` is the branch tip; `after` is this lane's final binary;
`oracle` is HotSpot 25.0.3+9. A row is **GREEN** when after == oracle.

| shape | field | before | after | oracle |
|---|---|---|---|---|
| `Hashtable` 3 entries | `table` | `[Ljava.lang.Object;` len 16 | `[Ljava.util.Hashtable$Entry;` len **11** | same **GREEN** |
| | node | `HashMap$Node` | `Hashtable$Entry` | same **GREEN** |
| `Hashtable` empty | `table` | `[Ljava.lang.Object;` len 16 | `[Ljava.util.Hashtable$Entry;` len **11** | same **GREEN** |
| `Hashtable` 40 entries | `table` | `[Ljava.lang.Object;` len 64 | `[Ljava.util.Hashtable$Entry;` len **95** | same **GREEN** |
| `Hashtable` a,b,c,d | `toString` | `{c,d,a,b}` | `{b,a,d,c}` | same **GREEN** |
| | `keySet`/`values`/`entrySet` iter | ascending | descending | descending **GREEN** |
| | `keys()` enumeration | descending | descending | descending (was already right) |
| | serialization round trip | `{b,c,d,a}` | `{a,d,c,b}` | same **GREEN** |
| `HashMap.keySet()` | `this$0` | **NULL** | the map | the map **GREEN** |
| `HashMap.entrySet()` | `this$0` | **NULL** | the map | the map **GREEN** |
| `HashMap.values()` | `this$0` | the map | the map | the map (was already right) |
| `LinkedHashMap.keySet()` | `this$0` | **NULL** | the map | the map **GREEN** |
| `LinkedHashMap.entrySet()` | `this$0` | **NULL** | the map | the map **GREEN** |
| `LinkedHashMap.values()` | `this$0` | the map | the map | the map (was already right) |
| `CHM` 2 entries, after any bulk read | `table` | **null** | `[LCHM$Node;` len 16 | same **GREEN** |
| `TreeMap` 20 puts + 1 remove | `modCount` | **0** | 21 | 21 **GREEN** |

### 1a. The gate, on the merged tree

MEASURED on the final binary, which is the merge of this lane's three commits
with `H0`'s `d4ca67040`, `TIMEOUT=600`:

```text
                     pristine 22cb4338d     final     final + dab993033
  --jdk-only         104 / 105              104 / 105  104 / 105
  SUITE=all          103 / 105              104 / 105  104 / 105   <- H0's comparator fix
  SUITE=core          64 /  65               64 /  65   64 /  65
```

The third column is the same binary re-run after merging H0's improved
signature extraction — the harness changed, the verdicts did not.

Every arm is at or above the baseline and the ONLY failure in any of them is
`RJdkOptionalShape` (§0). `RChmKeySetView` — the vector this lane REDDENED
mid-flight and then repaired, §4a — **PASSES in all three.**

Census on the strict arm, for whoever tracks the denominator:
`native-shadows-bytecode 1403 native-won · 471 bytecode-won`,
`synthetic-native-registered 1610`, `compatibility_classes 0`, no saturation.

Controls that had to stay still, and did: `HashMap`/`LinkedHashMap` table class,
length and node class; `HashSet`/`LinkedHashSet` backing tables;
`IdentityHashMap` (`[Ljava.lang.Object;` — correct on HotSpot too);
`WeakHashMap` (real bytecode, never ours); `HashMap` iteration order;
`Properties` `toString`/`keySet`/`size`/`store`+`load`; `TreeMap`/`TreeSet`
20-key navigation, `clone`, and both serialization round trips.

---

## 2. `Hashtable` — `H23-2` §3a's decline, spent

`H23-2` declined typing `Hashtable.table` and gave the reason precisely: the
nodes were `HashMap$Node`, `Hashtable$Entry.isAssignableFrom(HashMap$Node)` is
false on both VMs, and typing the array first arms the hybrid. **The decline was
right and the answer was to move both halves at once.**

`map_carrier_class_for_receiver` is now the single decision;
`map_node_class_for` (the node) and `bucket_table_component` (the array) both
delegate to it, so a family added to one is added to the other **by
construction**. That is the structural fix for the shape `H23-2` had to work
around: two functions that could disagree by omission.

The layouts made it a class-identity change and not a slot migration (`javap
-p`, JDK 25) — `final int hash; final K key; V value; <Node> next` on both, so
`NODE_FIELD_*` and `get_node_key`'s `Int@0 = JDK layout` sniff are untouched.

### 2a. The FOURTH allocation site

`H23` typed three sites and missed the no-arg `HashMap()` constructor. The same
species, one family over: `native_map_init` has a **dedicated
`uses_native_hashtable_layout` arm** that spells its allocation `alloc_ref_array`
and RETURNS before the generic path, so it is the only site a `new Hashtable()`
ever takes.

It was measured live rather than reasoned: with the node half moved and the
three known sites routed, a 3-entry `Hashtable` still read
`[Ljava.lang.Object;` while a 40-entry one — which had been through
`map_resize` — read `[Ljava.util.Hashtable$Entry;`. **A family whose small case
is wrong and whose large case is right is telling you which allocation site you
missed**, because the large case has been through the resize path and the small
one has only been through the constructor.

### 2b. The capacity, and why 34 call sites did not need touching

`new Hashtable()` is `this(11, 0.75f)` and `rehash()` is
`(oldCapacity << 1) + 1`: 11, 23, 47, 95. This VM had 16 and doubling.

`map_bucket_index` has 34 call sites in this file and none outside it. Making
them receiver-aware was the obvious plan and the wrong one. **The general index
rule IS the Hashtable's** — `(hash & 0x7FFFFFFF) % len` — and for a power-of-two
length it is *arithmetically identical* to `hash & (len - 1)`, because `len - 1`
has no bit 31 set so masking the sign bit first cannot change a bit the AND
keeps. So one function now takes the modulus from the table's **actual length**,
the power-of-two branch is a division-elimination fast path, and no call site
changed.

That is not only smaller, it is safer: `alloc_bucket_table`'s low-memory
fallback silently substitutes `MAP_DEFAULT_CAPACITY` for the requested size, so
a receiver-keyed rule and the array it indexes could disagree. A length-keyed
rule cannot.

`Hashtable(int)` now honours its argument verbatim (0 rounded to 1) — that is
`new Entry[initialCapacity]`, not `HashMap`'s `tableSizeFor`. `Hashtable(Map)`
came free: its JDK body delegates to the registered `(IF)V`.

The existing `threshold` mirror already wrote `(cap*3)/4`, which is
`(int)(cap*0.75)` at every one of 11/23/47/95. The two constants were already
consistent; only the length was wrong.

### 2c. The ordering bug the capacity fix UNCOVERED

`Hashtable$Enumerator` sets `index = table.length` and DECREMENTS. Four keys
`a b c d` land at buckets 9, 10, 0, 1 in an 11-bucket table:

```text
                   HotSpot 25.0.3+9      before               after
  toString()       {b=2, a=1, d=4, c=3}  {c=3, d=4, a=1, b=2}  matches
  keySet()         [b, a, d, c]          [c, d, a, b]          matches
  values()         [2, 1, 4, 3]          [3, 4, 1, 2]          matches
  entrySet() iter  b,a,d,c               c,d,a,b               matches
  keys()           b,a,d,c               b,a,d,c               unchanged
```

**The last row is why this is a bug report and not a preference.**
`native_ht_keys` was ALREADY descending and right, so one `Hashtable` presented
**two enumeration orders that disagreed with each other** — and only the three
bulk collectors were wrong. `map_bucket_scan_order` is now the one place that
decides.

It was invisible until §2b landed. With a 16-bucket table the keys occupy
different indices entirely, so the orders differed for a reason that swamped
this one. **Matching a capacity can make an ordering rule measurable that no
amount of staring at the old numbers would have separated** — and it closed a
divergence nobody had filed: a `Hashtable` serialization round trip now reads
back `{a=1, d=4, c=3, b=2}` on both VMs where CratonVM produced
`{b=2, c=3, d=4, a=1}`.

### 2d. Functional surface, measured

30 keys: size, get-back count, `keySet`/`entrySet`/`values` sizes, entrySet
iteration count, `remove`, `containsKey`, `contains`, copy-construction, and a
serialization round trip — **identical to HotSpot on every one.**

---

## 3. The view carriers — `H4-1` §2's containment, removed from the other side

`H4-1` §2 says a carrier is minted with `this$0` DELIBERATELY null and that
arming the producing registrar removes the containment. MEASURED, the defect is
narrower and sharper than "the carriers have a null `this$0`":

```text
  HM.keySet    this$0 = NULL              HotSpot: java.util.HashMap
  HM.entrySet  this$0 = NULL              HotSpot: java.util.HashMap
  HM.values    this$0 = java.util.HashMap HotSpot: java.util.HashMap   <-- already right
```

Two of three, and the same split for `LinkedHashMap` — because only the
`values()` mint path called `store_view_carrier_backref`.
`store_set_view_backref` is its missing twin.

**FOUR mint sites, not three.** `LinkedHashMap` has its OWN entrySet native that
reaches neither `make_view_set_of` nor `native_map_entry_set`, and with the
other three fixed it was the one carrier still reporting NULL. That is §2a
again, in a different function, in the same session. **When a family has a
per-family override native, "I fixed the generic path" is a claim about the
generic path only.**

`TreeMap`'s and `Hashtable`'s carriers were already right and are untouched
(`Hashtable`'s are `Collections$Synchronized*` wrappers on both VMs, which
declare no `this$0` at all).

**What this changes about arming.** `H4-1` §2 requires the split to be per
carrier family, keyed on whether the PRODUCING registrar moved, because
refusing a carrier's rows while its producer is still a bridge lets real
`HashMap$KeySet.size()` bytecode dereference the null. Populating the field
removes that coupling for this half of the family: the carrier is now correct
whether or not its rows are refused. It does **not** remove the coupling `H4-1`
§3 describes — a `Hashtable$KeySet` still mints a `HashMap$KeyIterator` — and
nothing here touches that.

---

## 4. `ConcurrentHashMap.table` — `H23-2` §7.3, and the regression it caused

```text
  CHM  2 entries    table=null   HotSpot [LCHM$Node; len=16 occupied=2
  CHMR 40 entries   table=null   HotSpot [LCHM$Node; len=64
  CHME empty        table=null   HotSpot null                       (agreed)
```

`chm_publish_real_table` was correct wherever it ran and ran from exactly ONE
caller, `chm_real_dual_iterator` — i.e. only from `keys()`/`elements()`. It is
now refreshed from `keySet()`, `values()` and `entrySet()` too. Each ALREADY
walks every entry, so the rebuild is a constant factor on an already-linear
operation. Hanging it on `put` would make every insert O(N); hanging it on
`size()` would make an O(1) query O(N). Both are worse than the defect.

MEASURED after: `entrySet()` iteration, `keySet()` and `keys()` each leave
`table` byte-identical to HotSpot's.

### 4a. TWO PRODUCERS, ONE SLOT — and the arm that caught it

The first version of this change **reddened `RChmKeySetView`** in all three
arms, and the mechanism is worth the space because it is a species, not an
accident.

`sizeCtl` has two writers in this file:

* `chm_record_initial_table` writes the requested **TABLE SIZE**, because
  `ConcurrentHashMap(int)` ends with `this.sizeCtl = cap` and
  `chm_reorder_by_virtual_bucket` needs that number later to reproduce
  HotSpot's flat-table bucket order; and
* the tail of `chm_publish_real_table_pinned` writes the 0.75 **THRESHOLD**,
  because that is what HotSpot's `sizeCtl` holds once the table exists.

Both are faithful to HotSpot, which reuses the field for both meanings at
different points in a map's life. **`chm_initial_table` cannot tell them apart**
— it applies `next_power_of_two` to whatever it finds.

`new ConcurrentHashMap<>(16)` records `sizeCtl = 32` (HotSpot's
`tableSizeFor(16 + 8 + 1)`), so a 3-entry map built that way has
`table.length == 32`. The mirror sized its own table from `DEFAULT_CAPACITY`,
published 16, wrote `sizeCtl = 12`, and `chm_initial_table` then answered 16.
`keySet()` came back `[youralias, thirdalias, myalias]` against HotSpot's
`[thirdalias, myalias, youralias]`.

The fix reconciles the two producers at the CONSUMER rather than picking a
winner: the mirror's table starts at `chm_initial_table(this)` rather than at 16.
That makes the published length match HotSpot for capacity-constructed maps —
a fidelity gain, not just a repair — and it makes the round trip exact, **and
not by luck**: the threshold written is `cap - (cap>>2)` = `0.75*cap`, which is
strictly greater than `cap/2`, so `next_power_of_two` of it is `cap` for every
`cap >= 4`. Repeated refreshes are idempotent and the reorder mask is
unchanged by a refresh.

**The general lesson.** Adding a *second call site* to an existing helper is not
a no-op even when the helper is unchanged and correct: it moves that helper's
side effects onto a new path. The side effect here was a field write that
another part of the same file reads as an input. `RChmKeySetView` was the only
vector in 105 that could see it, and it saw it in all three arms.

### 4b. Residual, stated

A CHM that has been **written and never bulk-read** still presents
`table = null`. Closing that needs `table` to be the authority a real `putVal`
CASes into rather than a mirror
(`W7-96-chm-table-never-populated.md`), which is a different change. There is
no hook for "someone is about to reflect on this field", so a mirror cannot be
made unconditional without paying O(N) on a write or on an O(1) read.

---

## 5. `TreeMap` — the diagnosis `WORKER-2` §3 asked for, and a decline with evidence

`H0-5` §6 measured that mechanism A (null `this$0`) reaches `LinkedHashMap` and
`Hashtable` but **not** `TreeMap`, and concluded `TreeMap` needs its own
diagnosis. Here it is.

**`TreeMap` does not have a `this$0` problem at all.** MEASURED: its
`entrySet()` and `values()` carriers both carry the map, and its `keySet()`
carrier declares no `this$0` on either VM. `H0-5` §6 is right that mechanism A
does not reach it, and the reason is that there is nothing there to reach.

What `TreeMap` has instead is that **its entire structure lives in a Rust
address-keyed side table** (`tm_array_table`: a sorted interleaved `Object[]`,
or a `BTreeMap` on the fast path). Three real JDK fields are consequently
unpopulated, and they are NOT one defect:

| field | before | oracle | verdict |
|---|---|---|---|
| `size` | correct | correct | already mirrored |
| `comparator` | correct | correct | already mirrored |
| `modCount` | **0** | 21 after 20 puts + 1 remove | **FIXED here** |
| `root` | **null** | a `TreeMap$Entry` tree | **DECLINED, see below** |
| `entrySet()` element class | `AbstractMap$SimpleEntry` | `TreeMap$Entry` | **DECLINED** |
| `TreeSet.m` | **null** | the backing `TreeMap` | **DECLINED** |

### 5a. `modCount`, fixed

A size change is exactly a structural modification, and the correspondence
holds in both directions — which is why the bump is gated on the size actually
CHANGING and not on the setter being called. `put` of an existing key changes
neither on HotSpot; `put` of a new key, `remove` of a present key and `clear`
of a non-empty map change both. A redundant same-value write through
`tm_set_slot` must not inflate the count, so the test is on the value.

### 5b. `root` — the consumer search, and why I did not build the mirror

The mirror is buildable. `TreeMap.buildFromSorted` is a pure function of a
sorted sequence, `computeRedLevel` gives a genuinely valid red-black colouring,
and the layout is known (`key@0 value@1 left@2 right@3 parent@4 color@5`,
`javap -p`). What stopped me was the **reach audit**, run rather than assumed:

```text
                          HotSpot            CratonVM
  TreeSet serialization   [a, b, c]          [a, b, c]        agree
  TreeMap serialization   {a=1, b=2}         {a=1, b=2}       agree
  TreeMap.clone().root    a=1                a=1              agree  (!)
  20-key navigation       first/last/headMap/tailMap/higher/floor/
                          remove/descendingKeySet/iteration     all agree
  TreeSet 20-key          first/last/headSet/contains/remove    all agree
```

**Nothing consumes `root`.** Serialization walks `entrySet()`, not the tree.
And `TreeMap.clone()` returns a map whose `root` IS populated **on CratonVM
too** — because clone runs real `buildFromSorted` bytecode — which is a
standing existence proof that the shape is walkable here, and simultaneously
evidence that the field is not load-bearing for anything this VM currently does.

The cost of the mirror is the reason to refuse it. The only available triggers
are the bulk read natives, and `native_tm_entry_set` **already allocates N
entries**; adding N `TreeMap$Entry` allocations would roughly double the
allocation cost of every `TreeMap` iteration in the VM. For `ConcurrentHashMap`
I accepted exactly that trade because there the mirror is **load-bearing** —
java.base's own `KeyIterator` walks it, and `keys()`/`elements()` cannot return
the JDK's dual carrier without it. For `TreeMap` the mirror would be
decorative. **A mirror with no consumer is a per-iteration tax with no
falsifiable benefit**, and if a consumer is found the specification is two
paragraphs up.

`TreeSet.m` and the `entrySet()` element class are declined for the same
reason and with the same evidence: no consumer found, and both would need a
real object graph maintained beside the authoritative one. The element class
additionally needs the 3-field live-entry carrier (`key@0 value@1 source@2`) to
move its `source` to an undeclared slot, because on a real `TreeMap$Entry`
slot 2 is `left` — storing a `TreeMap` there would hand real bytecode a
`TreeMap` where it expects an `Entry`, which is strictly worse than the current
detached entry.

---

## 6. `H23-2` §7.1's invariant, written

> A table may be typed only if every node that can enter it is real.

`map_node_class_for` holds it in one direction and cannot hold it in the other:
a mapping whose key or value is a non-`Object` `Value` keeps the untyped
carrier, because a real `$Node` declares both slots `Ljava/lang/Object;` and
`coerce_field_value_by_descriptor` would null the primitive. So a Rust-private
producer can in principle head a typed table with a fabricated node.

`H23-2` §4b measured that case at **0 in 14 shapes** from Java and §7.1 then
declined to write the repair, on the grounds that an unexercised GC-sensitive
reallocation is worse than the residual. That judgement was right about the
risk and wrong about the ceiling — `0 in 14` is not a proof and the remaining
producers are the 42 direct `native_map_put_pub` call sites that are not
obliged to store an object.

`degrade_bucket_table_to_untyped` retypes the TABLE down rather than typing the
node up, and **the risk reads differently now**: the worst case of this path is
a table that reverts to `Object[]`, which is the state every map in this VM was
in before `H23` — a known-good configuration, not a novel one. It cannot lose a
mapping (every chain head is copied, the links are untouched) and it cannot
loop (the replacement array carries the sentinel id, so the `== want` test is
false forever after). The guard itself is one array-header read on the hot path,
because `class_id_of_object` on a reference array answers the component class.

---

## 7. `Properties` — declined, and the reason is structural not cosmetic

```text
  PROP  table = [Ljava.lang.Object; len 16 occupied 0   HotSpot: null
```

JDK 25 backs `Properties` with a private `ConcurrentHashMap map` and leaves the
inherited `Hashtable.table` null. This VM backs it with a Rust side-table and
its bucket array holds nothing — `occupied=0` with `size=2`.

It is not fixable by declining an allocation. `MAP_FIELD_BUCKETS` is slot 0, and
on a real `java/util/Properties` **slot 0 IS `Hashtable.table`** — the VM stores
its empty bucket array in exactly the field HotSpot's null lives in. Null it and
`map_state` reports no buckets, `native_map_put` calls `map_resize` to
materialise one, and the field is repopulated. Moving `Properties`' native
bucket state elsewhere is a different change in a different file's territory.

The observable consequence of the divergence is nil in both directions: an empty
`Object[]` and a null both read as "no entries" to any walker, and MEASURED,
`Properties` `toString`, `keySet`, `size`, `store` and `load` are identical to
HotSpot. Recorded as a divergence, refused as a repair.

---

## 8. NEW FINDING — `HashMap`'s byte-for-byte match holds for OBJECT keys only

`WORKER-2` §4 opens with *"`HashMap`'s internal representation now matches
HotSpot byte-for-byte in the default configuration"*, over a two-entry
`String`-keyed map. MEASURED, an `Integer`-keyed one does not:

```text
  HashMap<Integer,String>, 40 puts
    HotSpot    table [Ljava.util.HashMap$Node; len=64 occupied=40   size=40
    CratonVM   table [Ljava.util.HashMap$Node; len=16 occupied=0    size=40
```

**`occupied=0` with `size=40`.** The map's entire contents are in the
`hm_int_fast` Rust side store; the bucket array is present, correctly typed, and
empty. `H23-2` §4b saw the same thing from the other end (*"J1 shows `size=1`
with `occupied=0` — an all-`Integer`-keyed map has an empty table"*) and read it
as evidence for the fabricated-node question it was asking. Read as a fidelity
question it is the largest remaining `HashMap`-family divergence, and it is
strictly worse-shaped than the pre-`H23` `Object[]`: the object is
**internally inconsistent**, with `size` and `table` disagreeing, where the old
defect was merely mistyped.

Not fixed here, and not declined lightly — the trigger problem is `TreeMap`'s
(§5b) with a worse constant: materialising on a bulk read would defeat the
optimisation the side store exists for, on the hottest map shape in the VM. §9
carries the reach audit that bounds the exposure.

---

## 9. Reach audit for §8 — what the green arms were not asking

Thirteen readers of an `Integer`-keyed 40-entry `HashMap`, one case per process,
against HotSpot. This is `WORKER-2` trap 5 run as an instrument rather than
quoted: *"a green arm is evidence about the question it asked."*

| reader | HotSpot | CratonVM | |
|---|---|---|---|
| reflective `table` read | len 64, occupied 40 | **len 16, occupied 0** | **MISMATCH** |
| `forEach` | visited 40 | visited 40 | agree |
| `new HashMap<>(m)` | 40 | 40 | agree |
| `new TreeMap<>(m)` | 40 | 40 | agree |
| `Map.copyOf(m)` | 40 | 40 | agree |
| `unmodifiableMap(m).entrySet()` | 40 | 40 | agree |
| `entrySet().stream()` | 40 | 40 | agree |
| `equals` / `hashCode` vs a `LinkedHashMap` | true / true | true / true | agree |
| `merge` + `computeIfAbsent` + `replaceAll` | 41, `V7!`, `NEW`, `V0` | identical | agree |
| `entrySet().iterator().remove()` | removed 20, size 20 | identical | agree |
| `toString` / `keySet` | `{0=v0 … 5=v5}` | identical | agree |
| `clone()` | 40, `v7` | 40, `v7` | agree |
| **a user SUBCLASS of `HashMap` reading `HashMap.table`** | **len 64** | **len 64** | **agree** |

**Twelve of thirteen agree, and the thirteenth is the discriminator.** A user
subclass of `HashMap` gets a fully materialised 64-bucket table on BOTH VMs —
so the side store is not taken for a subclass receiver, and the divergence is
confined to a receiver whose exact class is `java.util.HashMap` observed by a
**direct reflective read of the field**. Every functional reader, including the
five that walk entries through real JDK bytecode (`TreeMap` copy-construction,
`Map.copyOf`, `unmodifiableMap().entrySet()`, the stream, `clone`), agrees.

That is what bounds the exposure, and it is also why no arm was ever going to
find it: the corpus asks about behaviour, and this is the one shape where the
behaviour is right and the representation is not. It took a thirteen-case
reflective probe to separate them — the same instrument `H23`'s inert table fix
needed.

**Not fixed, refused with the reason.** The trigger problem is `TreeMap`'s
(§5b) with a worse constant: materialising the side store on a bulk read would
defeat the optimisation it exists for, on the hottest map shape in the VM, to
correct a field that twelve of thirteen readers do not consult. The one honest
change available — making `size` and `table` stop *disagreeing* by materialising
eagerly — is the whole optimisation.

---

## 10. What every lane after this one should take

1. **Verify the brief's baseline before trusting it** (§0). Two of the three
   acceptance numbers in `WORKER-2` are unreachable on this host for reasons
   that predate every lane reading it.
2. **The small case wrong and the large case right names your missed allocation
   site** (§2a). The large case has been through `resize`; the small one has
   only been through the constructor.
3. **A per-family override native means "I fixed the generic path" is a claim
   about the generic path** (§3). `LinkedHashMap` has its own entrySet native
   and it was the last carrier standing.
4. **Prefer deriving a rule from the DATA over teaching every call site the
   receiver** (§2b). One length-keyed `map_bucket_index` beat 34 receiver-aware
   ones and is immune to the allocator substituting a different size.
5. **Adding a second call site to a correct helper moves its SIDE EFFECTS onto
   a new path** (§4a). Audit what the helper writes, not only what it returns —
   `sizeCtl` had two producers with two meanings and one consumer that could
   not tell them apart.
6. **Fixing a capacity can make an ordering rule measurable** (§2c). Do not
   close the family when the class and the length match; re-diff the orders.
7. **Run the reach audit before building a mirror** (§5b). `TreeMap.root` has
   no consumer in serialization, clone, or twenty navigation methods, and the
   mirror would have taxed every `TreeMap` iteration in the VM for it.

---

## 11. Index rows for `INDEX.md` (H0 to move; nobody but H0 edits that file)

```text
| WORKER-2-NOTE-1 | native-collections/src/lib.rs | Hashtable's nodes, table type, 11/23/47/95 capacity and descending enumeration order; the view carriers' this$0 across four mint sites; CHM's real table on every bulk read; TreeMap's modCount; H23-2 §7.1's invariant enforced | LANDED, BUILT, RUN |
```
