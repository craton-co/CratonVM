# P2 — the `java.util` collections shadows: 8 of 68 registrations are retirable

> ## RECONCILED 2026-08-12 (lane C18) — the direction of this campaign is the
> opposite of what the title suggests
>
> **SOURCE-VERIFIED reading, not a measurement on a binary.**
>
> * **The census direction is backwards as usually quoted.** Retiring the 8
>   `ArrayList` rows this record recommends *while adding the ~25 real
>   view-class rows that the corrected `Map.values()` shape requires* (`C7-1`,
>   `C13-2`) is a **NET +17 registrations**. Any record or brief framing the
>   collections work as *shrinking* the shadow count should say plainly that
>   **correctness here COSTS registrations.** The 8 are still worth retiring;
>   the reason is fidelity, not arithmetic.
> * **§2.2's line range for `register_interface_natives` is stale twice over.**
>   `28697–28896` truncated the function mid-block (`C7-2` §1); in the current
>   tree it spans **28773–28991** with **38 registrations** (`C13-1` §5.1).
>   Its own in-file header comment saying "these 23" is stale too.
> * **There is a second interface registrar this record does not mention:**
>   `register_queue_deque_interface_natives`, **`:38096`, 23 rows** — not
>   `:37787` and not 18. Four of the 23 come out of a `for` loop, so a count
>   taken by grepping `registry.register(` call sites undercounts, and five of
>   the 23 are on concrete `$Itr` classes rather than interfaces.

**What this is.** A per-triple adjudication of the `java.util` collections slice
of the 226 `native-shadows-bytecode` rows a `--jdk-only --explain-jdk-only` run
reported as *actually taken*. It answers, per family, the only question that
decides a §1.4 retirement: **is the class's state real?** — because
`native-api/src/retired_shadow.rs`'s module doc fixes the order and it is not
this lane's choice: *"a class's state has to become real before its shadow can be
retired."*

**This lane could not build, run cargo, or take a census.** Everything below is
read from the tree, from the frozen `scripts/baselines/jdk-only-kind-map-25-linux.tsv`,
and from doc comments that record earlier measurements. Where a claim is a
reading rather than a measurement it says so. §6 is the measurement recipe,
which needs nobody's permission and no source edit at all.

> **UPDATE 2026-08-12, a later lane with a current binary.** §6 has been RUN and
> **the eight are landed** in `RETIRED_SHADOW_TRIPLES`, with the discriminator
> widened to `java/util/`. §8 is the transcript. Read it before §5's "nothing
> was retired": the hold in §5.1 was never doubt about the eight, and the three
> frozen artefacts it names still have to be re-frozen from one Linux census in
> the landing commit — that part is unchanged and is the open obligation.
> §6's arm C as written is also **not sufficient on its own**: the `ArrayDeque`
> control comes back verdict-neutral against the `RJdk*` corpus and needs a
> workload that calls `delete(i)` before it goes red. §8.2.

---

## 0. Headline

| | triples | registrations |
|---|---:|---:|
| measured slice (this lane) | 62 | **68** |
| **RETIRED 2026-08-12** (was "recommended"; §8) | 7 | **8** |
| KEEP — state is not real (needs-VM-support) | 43 | 47 |
| KEEP — the native is load-bearing *for* real bytecode | 1 | 2 |
| KEEP — entangled with `Map.values()` | 7 | 8 |
| KEEP — pending measurement (fabricated carrier) | 2 | 2 |
| RECLASSIFY `Bridge` → `Intrinsic`, do not retire | 1 | 1 |

**Three findings that change how this cluster should be planned.**

1. **The measured "collections are green under `--jdk-only`" evidence does not
   transfer.** Every probe in that screen ran *with the shadow in place*. What
   it establishes is that the natives work, not that the real bytecode would.
   For four of the fifteen classes here the real bytecode demonstrably would
   **not**: their state is in a process-global Rust side table that no
   `getfield` can reach.
2. **The brief's strongest argument is stale in the tree.** The
   `Collections.synchronizedList` data loss is attributed to
   *"the identity-stub-is-not-atomic defect"*. That stub is **gone** — commit
   `e8a7caba4` (2026-07-01, ancestor of HEAD) and its `synchronizedList` sibling
   both construct the real `Collections$SynchronizedMap` /
   `SynchronizedRandomAccessList` through the real constructor. Retiring
   `synchronizedMap` therefore fixes nothing, because there is nothing left in it
   to fix. **The measured data loss has an unidentified cause** — see §7.1. That
   is a finding, not a quibble: it is the row the campaign is using to justify the
   whole cluster.
3. **`ArrayList` is the one family whose §1.4 precondition is already met**, and
   it is *still* mostly not retirable — because `Map.values()` returns a
   `java/util/ArrayList` with the source map stashed in a trailing capacity slot,
   so `ArrayList.size`/`get`/`iterator`/`toArray`/… are not `ArrayList` methods
   here, they are the implementation of `Map.values()`. Retiring them freezes
   every `values()` view. §3.2.

---

## 1. Method, and what the instrument can and cannot say

**Population.** The 62 triples named in the `--jdk-only-report` slice. Their
registration multiplicity is read from
`scripts/baselines/jdk-only-kind-map-25-linux.tsv`, whose unit is **one
registration** and whose column 4 is the registration ordinal — so
`java/util/ArrayList.<init>(I)V` at ordinals 0 and 1 is two registrations of one
triple, and a retirement moves both.

**The predicate.** `NativeMethodRegistry::register`
(`native-api/src/registry.rs:5782-5798`) re-tags a registration
`Bridge → SyntheticStub` when
`retired_shadow::triple_is_retired_shadow(class, method, descriptor)` answers
true, and `--jdk-only` refuses every `SyntheticStub` at the door. So a retirement
is a re-tag of **every `Bridge` registration of that exact triple**, and it says
nothing about a registration of the same triple under an ambient `Intrinsic`.
That is the `LogRecord.<init>` inert-entry shape and it applies here — see §5.3.

**Three things the frozen artefacts cannot tell this lane.**

* The kind map is keyed `25/linux` and is **already flagged stale** — its
  sibling `jdk-only-bridge-ratchet.json` carries a "PENDING RE-FREEZE, NOT
  APPLIED, 2026-08-12" note in its own body. Every count in §4 is a delta on a
  stale base and must be re-taken, not added.
* `invocations` is per-run. A row absent from the 226 is not proven dead.
* Registration **order** decides which of two implementations owns a slot, and
  the kind map records the ordinal but not the file. Two triples in this slice
  have two *different* implementations (§2.1).

---

## 2. The structural finding: the shadow is not keyed where you would look

Three doors serve the same call, and `RETIRED_SHADOW_TRIPLES` closes only the
first. Any retirement plan that ignores the other two will measure as inert.

### 2.1 Door 1 — the concrete-class registration, sometimes twice, with two bodies

`java/util/ArrayList.iterator()Ljava/util/Iterator;` has **two** registrations
with **different** implementations:

* `native-collections/src/lib.rs:4280` → `native_al_iterator`, which builds a
  `java/util/ArrayList$Itr` (real class, real field names — §3.1);
* `native-builtins/src/lib.rs:16607` → `native_arraylist_list_iterator`, which
  builds a `java/util/ArrayList$ListItr` **snapshot** served by
  `native_snapshot_itr_*`.

Both are inside `register_essential_natives_with_shims`
(`native-builtins/src/lib.rs:7103`, the first entry of the boot sequence) and
`register_arraylist_natives` respectively, so the later one owns the slot. The
same shape holds for `ArrayList.<init>(I)V` (`native-builtins/src/lib.rs:16535`
writes `elementData`/`size` **by name**; `native-collections/src/lib.rs:4206`
writes them through `al_slots_for`). Retiring the triple re-tags both, which is
correct — but a reader who greps one crate concludes the wrong multiplicity.

### 2.2 Door 2 — `register_interface_natives`

`native-collections/src/lib.rs:28697-28896`, ambient `Bridge` at `:28699`,
registers the *same native functions* on the interfaces:

| interface triple | native | line |
|---|---|---|
| `java/util/Collection.iterator ()Ljava/util/Iterator;` | `native_al_iterator` | `:28701` |
| `java/util/List.iterator ()Ljava/util/Iterator;` | `native_al_iterator` | `:28742` |
| `java/util/List.{get,size,add,isEmpty,stream,toArray}` | `native_al_*` | `:28730`–`:28761` |
| `java/util/Set.iterator` | `native_hs_iterator` | `:28792` |
| `java/util/Map.{get,put,containsKey,keySet,values,entrySet,size,forEach}` | `native_map_*` | `:28802`–`:28850` |

and the frozen kind map confirms **`java/util/Iterator.hasNext()Z` and
`.next()Ljava/lang/Object;` each carry two `Bridge` registrations of their own**
(the second at `native-builtins/src/lib.rs:16549`, an explicitly fabricated
`cursor=slot 1, total=slot 2` layout).

`ksv_route`'s doc (`native-collections/src/lib.rs:47894-47903`) states these
interface rows **win over the exact-class registrations** when the receiver's
class declares no such method — which is the whole reason that helper exists.

**Consequence, and it is the load-bearing one:** the 226-row report attributes
these dispatches to the concrete class (`java/util/ArrayList.iterator`), so on
*that* run the concrete key is what dispatched. But retiring the concrete triple
leaves the interface key registered and live. Whether a given call site then
falls to the interface native or to real bytecode is a dispatch question this
lane cannot answer by reading. **It is answerable in one command** — §6.

### 2.3 Door 3 — `force_native_over_real_jdk_bytecode`

`vm/src/runtime/interpreter/native_override.rs:2442`. Its own comment (`:2483-2489`)
calls it *"the ONLY force-native gate consulted by the
reflective/megamorphic/`invokespecial`/interface-default dispatch path"*. It
names, among others:

* `java/util/ArrayList` × `{size,isEmpty,get,contains,iterator,toArray,indexOf,lastIndexOf,toString,hashCode,equals}` (`:2464-2479`)
* `{HashMap,LinkedHashMap,Hashtable,ConcurrentHashMap}` × 22 methods (`:2508-2540`)
* `{Iterable,Collection,Set,EnumSet}.iterator` (`:3292-3298`)
* `java/util/Iterator.{hasNext,next,remove}` (`:3314-3316`)
* `java/util/Collections.emptyList()Ljava/util/List;` (`:2863-2868`)

This list cannot *resurrect* a retired triple — under `--jdk-only` the
`SyntheticStub` never registers, so there is no callback to force — but it is
**evidence**: every entry is a place where somebody measured real bytecode losing
to the native and wrote down why. Two of those reasons are decisive here (§3.2
and §3.6).

The sibling `real_protected_stub_class_common` (`:6606-6672`) is the inverse
list, twelve classes both dispatch paths yield to real bytecode. **None of the
fifteen classes in this slice is on it.**

---

## 3. Per-family adjudication

### 3.1 `ArrayList` and `ArrayList$Itr` — state IS real. The only family whose precondition is met.

`al_slots` (`native-collections/src/lib.rs:3736-3746`) resolves the **real**
fields by name and falls back to the synthetic pair only when the real class is
absent:

```rust
let data = ctx.resolve_field_index("java/util/ArrayList", "elementData");
let size = ctx.resolve_field_index("java/util/ArrayList", "size");
```

`al_mod_count_slot` (`:4055`) resolves `AbstractList.modCount`; `al_set_size`
(`:4074`) bumps it on every structural change. `al_itr_slots` (`:13778`),
`al_itr_last_ret_slot` (`:13800`) and `al_itr_expected_mod_count_slot` (`:13814`)
resolve `cursor` / `lastRet` / `expectedModCount` / `this$0` by name, and
`try_alloc_synthetic` (`:2632`) loads the **real** `java/util/ArrayList$Itr`
class before allocating. So in real-JDK mode both halves read and write exactly
the fields the JDK's own bytecode reads and writes, in both directions.

Two residual divergences, both in the safe direction: `modCount` deliberately
under-reports (`sort`/`replaceAll` bump it in the JDK and not here — `:4088-4092`),
and `native_al_init` seeds a 10-element `Object[]` where the JDK seeds the
shared `DEFAULTCAPACITY_EMPTY_ELEMENTDATA` sentinel. Neither survives a
retirement; both improve.

### 3.2 …and `ArrayList` is still mostly not retirable, because of `Map.values()`

`force_native_over_real_jdk_bytecode:2448-2463`, quoted because it is the whole
argument:

> `Map.values()` (native_map_values in native-collections/src/lib.rs) returns a
> plain `java/util/ArrayList` that stashes its source map in a spare trailing
> capacity slot so a later `Map.put`/`remove` on the source is reflected on read
> (`resync_values_view`, called from `native_al_size`/`native_al_is_empty`/
> `native_al_get`/`native_al_contains`/`native_al_iterator`/`native_al_to_array*`
> etc.). Real `ArrayList` bytecode declares its own `size()`/`isEmpty()`/`get()`/
> etc., so without this force-entry the receiver-has-own-bytecode rule picks real
> bytecode over the registered native, skipping the resync entirely and freezing
> the returned Collection at whatever the source map held at `values()` call
> time (H2 `TestAlter.testAlterTableDropIdentityColumn`).

So `ArrayList.{size,isEmpty,get,iterator}` — and by the same route
`ArrayList$Itr.{hasNext,next,remove}`, which iterate that view — are **not
retirable while `Map.values()` piggybacks on the ArrayList layout**. The fix is
to give `values()` a real view class, which is a `native-collections` change, not
a table entry. This is the clearest "retiring turns a working path into a broken
one" case in the slice, and it was already measured once, against H2.

**What is left, and it is real:** the five triples that are *not* on that list
and touch no view — `<init>()V`, `<init>(I)V`, `<init>(Ljava/util/Collection;)V`,
`add(Ljava/lang/Object;)Z`, `clear()V`. Their real bodies route allocation
through `Arrays.copyOf(Object[],int)` and `Arrays.copyOf(Object[],int,Class)`,
both of which **stay** (§3.3), and `jdk/internal/util/ArraysSupport.newLength`,
which is unregistered arithmetic. They are safe in either order (a retired
`<init>()` leaving the JDK's zero-length sentinel is grown correctly by the
surviving native `add`; a surviving native `<init>` leaving `Object[10]` is
appended to correctly by real `add`), so this is **not** an all-or-none set —
unlike `LogRecord`'s source pair. Retire them together for coherence, not for
safety.

`ArrayList.sort(Comparator)` and `ArrayList.stream()` are held separately:
`sort` is plausible (real `Arrays.sort(T[],int,int,Comparator)` is unregistered)
but unmeasured, and `native_al_stream` (`:20581`) allocates an instance of
**`java/util/stream/Stream`, which is an interface** — retiring it hands the real
`ReferencePipeline`/`ArrayListSpliterator` stack a job this VM has never
exercised. See W2-1.

### 3.3 `Arrays.copyOf(Object[],int)` — KEEP, and it is load-bearing *for* the retirements above

`native-builtins/src/phases_early.rs:1031-1038`, on the 3-arg sibling:

> Real-JDK `ArrayList.toArray(T[])` calls this 3-arg overload … **Without this
> native the call falls through to bytecode that dereferences unsupported
> `arrayClass` reflection internals and NPEs.**

The 2-arg overload's own comment (`:981-984`) records that the real JDK body
allocates via `Array.newInstance(original.getClass().getComponentType(), n)` —
the identical reflection path. And `alloc_ref_array`
(`native-collections/src/lib.rs:2728`) is `ctx.new_ref_array(ClassId::new(0), n)`,
so every backing array this VM hands `ArrayList` has component class id 0.

`ArrayList.grow()`, `ArrayList.toArray()` and `ArrayList(Collection)` in real
JDK 25 bytecode all funnel through these two overloads. **Retiring
`Arrays.copyOf([Ljava/lang/Object;I)` would break the very real bytecode the rest
of this document is trying to reach.** Hard keep, and it belongs in the
"needs-VM-support" column: the VM support it is waiting on is array-class
mirrors that carry a component type.

`Arrays.copyOfRange([BII)[B` is the opposite: a stateless primitive
`byte[]`→`byte[]` copy with no reflection on either side. That is what §1.3 calls
an **`Intrinsic`**, and the census is explicit that intrinsics are not roadmap
work. Reclassify; do not retire. (Note a disagreement to settle: the frozen kind
map records it `bridge`, while the nearest `set_category` above its registration
at `native-builtins/src/phases_early.rs:884` is `Intrinsic`. One of the two
readings is wrong and the re-tag only fires on an effective `Bridge`, so this
must be resolved before it is counted.)

### 3.4 `TreeMap`, `TreeSet` — pure Rust side tables. Not retirable at any price.

`tm_get_slot` / `tm_set_slot` (`native-collections/src/lib.rs:38989`, `:39010`):
*"The object's own fields are never consulted"* / *"never touched — the
side-table is the sole authoritative store"*. The state lives in
`tm_array_table()` / `tm_fast_table()` / `ts_array_table()`, keyed by
`tm_obj_key` (`:38213`) = the receiver's identity hash. Only `size` and
`comparator` are mirrored to real fields, write-only, and only so real
`writeObject` works (`:39032-39063`); **`root` and the `TreeMap$Entry` graph are
never written**. `TreeSet` has no mirror at all.

The proof that this is not a theory is `tm_materialize_deser_if_needed`
(`:38276`), which exists because real `readObject` bytecode populates `root` and
the natives cannot see it:

> `TreeMap.readObject` (real bytecode) rebuilds the red-black tree … but it never
> touches our `tm_fast_table`/`tm_array_table` side-tables, so every native read
> op (`get`/`size`/`entrySet`/…) sees an empty map.

Retiring any of these six triples yields an empty map or set.

### 3.5 `ArrayDeque` — fabricated four-slot layout over a three-field class.

`native-collections/src/lib.rs:34896-34899` hard-codes
`AD_FIELD_DATA/HEAD/TAIL/SIZE = 0/1/2/3` and `ad_state` (`:34949`) reads them
with raw `ctx.get_field`. **There is no `resolve_field_index` anywhere in the
ArrayDeque block.** Real `java.util.ArrayDeque` declares `elements`, `head`,
`tail` — three fields, no `size` — so slot 3 is off the end of a real receiver.
The registrar says so itself (`:35091-35094`): without these natives the call
*"falls through to the real JDK `delete(...)` bytecode, which leaves our
synthetic `size` slot stale and silently corrupts the deque (… the H2 SYS-lock
leak)"*. Not retirable.

### 3.6 `LinkedList` — real fields exist and are mirrored; the blocker is two writers.

`ll_set` (`:31607`) mirrors `head`/`tail`/`size` into the real `first`/`last`/`size`
by name, and `ll_get` (`:31582`) reads the **overlay first**, real fields only on
a miss. `native_ll_iterator`'s own comment (`:33134-33158`) is the adjudication,
already done and already measured:

> The obvious alternative was to drop the interception and let real `LinkedList`
> bytecode run, and unlike the other collections here that is genuinely
> available … It is still the wrong answer, and one run says why. Drive a real
> `ListItr.remove()` … and the two owners disagree: real `unlink` decrements the
> real `size` …, `ll_get` reads the OVERLAY first and still answers the old size,
> and the NEXT real iteration walks off the end — `NullPointerException` …
> Handing iteration to real bytecode would put a second writer on state the
> natives own.

Retirable **only** together with deleting `ll_get`'s overlay-first arm. That is a
collections reclassification, which is what W7-16 already concluded.

### 3.7 The hash family — one is close, three are not.

* **`ConcurrentHashMap` (+ `KeySetView` contents).** `CHM_FIELD_SEGMENTS = 0`
  (`:44991-45005`), raw slot, no name resolution. Absolute slot 0 on a real CHM is
  `AbstractMap.keySet`, and the natives put an `Object[] segments` there. The
  file states the consequence outright (`:45312-45318`): the entries are
  *"NOT in the real JDK's `table` field (which stays null for the object's whole
  life)"*, which is why the JDK's own `writeObject` serialised every CHM as
  empty. Ten registrations, none retirable.
* **`Hashtable`.** Slots line up by accident (Dictionary declares no fields), but
  entries are `java/util/HashMap$Node`, never `Hashtable$Entry` — so real
  `Hashtable.clone()`'s `checkcast` throws (`native-builtins/src/deprecated_util.rs:1034-1043`).
  Not retirable.
* **`LinkedHashSet`.** Backed by a real-class `LinkedHashMap` whose **contents
  live in the `lhm_overlay` Rust side table** (`:9028`, `:33293-33306`;
  `identity_hash.rs:4-11` names it as one of four process-global overlays). Not
  retirable until the LinkedHashMap overlay is.
* **`HashSet`.** `hs_backing_map` (`:12403`) reads hard-coded slot 0, which
  happens to be the real `HashSet.map`. Layout-compatible, but `iterator()` and
  `toArray()` are **snapshots** minting `java/util/HashMap$KeyItr` — a name no JDK
  declares and which is already on `NO_IMAGE_JDK_RECEIVERS`. Retiring
  `HashSet.iterator` would be an improvement in principle and is blocked in
  practice by the `Set.iterator` interface door (§2.2). Ten registrations, held.
* **`HashMap`** is the closest of the four to retirable — real `table`/`size`
  resolved by name on the receiver's own class (`receiver_table_slot`, `:7064`),
  real `HashMap$Node` in real JDK field order — and is still blocked by three
  things: the `hm_int_fast_shards` overlay (`:1489-1582`) is the **authoritative
  store** for a fresh `HashMap<Integer,?>` while the heap map's `table` is empty
  and `size` is 0 (`:8651-8656`); bucket arrays are `ClassId(0)` `Object[]`, not
  `HashMap$Node[]`; and `keySet`/`entrySet` return snapshot `HashSet`s over a
  16-slot marker-carrying backing (`:11438-11440`), not `HashMap$KeySet`/`EntrySet`.
  `map_buckets_slot`'s doc (`:7080-7096`) records the HotSpot A/B —
  `keySet=null` there, `keySet=ARRAY[Object]` here — and adds the sentence that
  settles this whole family: *"It does not fault today only because the natives
  shadow every reader."* Seven registrations, held.

### 3.8 `Arrays$ArrayList.iterator` and `Collections.synchronizedMap` — retirable, and near-inert.

`arrays_array_list_backing` (`:14408`) reads the real declared field `a` by name,
and `make_iterator_from_array` (`:1918`) already prefers the **real**
`java/util/Arrays$ArrayItr` with its fields resolved by name. The native and the
real bytecode do the same thing over the same field.

`native_collections_synchronized_map` (`:51778-51790`) is
`ctx.new_object_initialized("java/util/Collections$SynchronizedMap", "(Ljava/util/Map;)V", …)`
— it *is* the real bytecode's answer, built by the real constructor. Retiring it
is behaviour-preserving by construction. §7.1 is why it is also not the fix
anybody thinks it is.

### 3.9 `Collections.emptyList` — retirable only as a pair with a `vm/**` deletion.

`native_collections_empty_list` (`:15627`) reads the real static `EMPTY_LIST`,
which `native_collections_clinit` (`:15543`) seeds with a real
`java/util/Collections$EmptyList` instance. So the real `emptyList()` body —
`return EMPTY_LIST;` — would answer correctly today.

What blocks it is `force_native_over_real_jdk_bytecode:2859-2868`, whose stated
reason is *"During the Brave bootstrap that slot can retain a polluted
ArrayList"*. **That reason reads stale**: the pollution mechanism it describes
(`ensure_collections_empty_singletons`, `:15555-15570` — a mutable synthetic
`ArrayList` singleton that kotlin-reflect's shaded protobuf mutated) was closed
when the seed became a real immutable `Collections$EmptyList`. Reads stale is not
measured stale, so this is a nomination with a probe attached, not a deletion.

---

## 4. The arithmetic, and its derivation

**Recommended retirement = 7 triples, 8 registrations.** Multiplicity from the
frozen kind map, ordinals shown:

| triple | regs | sites |
|---|---:|---|
| `java/util/ArrayList.<init>()V` | 1 | `native-collections/src/lib.rs:4205` |
| `java/util/ArrayList.<init>(I)V` | **2** | `native-builtins/src/lib.rs:16535`; `native-collections/src/lib.rs:4206` |
| `java/util/ArrayList.<init>(Ljava/util/Collection;)V` | 1 | `native-collections/src/lib.rs:28925` |
| `java/util/ArrayList.add(Ljava/lang/Object;)Z` | 1 | `native-collections/src/lib.rs:4216` |
| `java/util/ArrayList.clear()V` | 1 | `native-collections/src/lib.rs:4220` |
| `java/util/Arrays$ArrayList.iterator()Ljava/util/Iterator;` | 1 | `native-collections/src/lib.rs:14229` |
| `java/util/Collections.synchronizedMap(Ljava/util/Map;)Ljava/util/Map;` | 1 | `native-collections/src/lib.rs:51643` (live); 3 further sites, §5.3 |
| **total** | **8** | |

### 4.1 `native-builtins/tests/stub_ratchet.rs`

The ratchet counts `SyntheticStub` **registrations** in its own boot-path replay.
All eight sites are inside registrars that replay reaches
(`register_essential_natives_with_shims` is `VM_INIT_SEQUENCE[0]`;
`register_collections_natives` is the registrar whose 364 stub rows the
2026-08-05 scope fix added), and the ambient category at each is `Bridge`
(`native-collections/src/lib.rs:4202`, `:14201`, `:28923`, `:51561`; the two
`native-builtins` rows are recorded `bridge` in the kind map). So the retag fires
on all eight and the count rises by **+8**.

**This is a prediction to check a diff against, not a number to paste.** Two
reasons, both already written into the file being predicted:

* the ratchet's replay *"registers some triples a real boot does not and misses
  others"*, so a census-derived multiplicity is a bound on it, not an identity;
* the constants are **already stale**. `stub_ratchet.rs:450` predicts 1259
  (no-management) against a frozen 1253, and `STUB-CENSUS-20260812.md` §6 records
  an **observed 1261** = 1259 + 2 Cipher. So the arithmetic is:

```
no-management:  last observed 1261  + 8  =  1269      (frozen constant: 1253)
management:     no observed number in tree;
                frozen 1263 + the same +6 stale drift + 2 Cipher = 1271, + 8 = 1279
```

The management figure compounds two unmeasured terms and is the weaker of the
two. **Take both from the printed `stub-ratchet: const <NAME>: usize = <N>;`
line**, in each configuration, as `stub_ratchet.rs:81-86` requires. Anything
beyond +8 is a finding to attribute, not slack to absorb.

### 4.2 `scripts/baselines/jdk-only-bridge-ratchet.json`

All eight shadow bytecode by construction — that is why they are in the 226-row
report — so every counter moves by the same 8:

```
bridge.rows                       9711 -> 9703
bridge.shadows_bytecode           4457 -> 4449
bridge.shadows_bytecode_anywhere  6066 -> 6058
bridge.without_acc_native         8912 -> 8904
registrations.bridge              9711 -> 9703
registrations.synthetic-stub      1262 -> 1270
total_rows                       11636 -> 11636   (a retag moves no row)
```

`bridge.stated_shadows_bytecode` (24) does **not** move: a re-tagged row is no
longer a bridge, so it leaves that population rather than joining it.

**This baseline is already stale by its own note** ("PENDING RE-FREEZE, NOT
APPLIED, 2026-08-12"), which expects `without_acc_native` 8922 and
`shadows_bytecode_anywhere` 6067 before this change. The deltas above are
therefore deltas on a base nobody has taken. Re-freeze from **one** census with
`sh regression-suite/bridge-ratchet.sh --update-baseline`, which re-freezes the
kind map from the same measurement because they are two readings of one census.

### 4.3 `scripts/baselines/jdk-only-kind-map-25-linux.tsv`

Exactly **eight** rows change, column 5 `bridge` → `synthetic-stub` and column 6
`kind_stated` `0` → `1` (the central re-tag sets `kind_stated`, which is the
point of applying it centrally):

```
java/util/ArrayList        <init>          ()V                                        0
java/util/ArrayList        <init>          (I)V                                       0
java/util/ArrayList        <init>          (I)V                                       1
java/util/ArrayList        <init>          (Ljava/util/Collection;)V                  0
java/util/ArrayList        add             (Ljava/lang/Object;)Z                      0
java/util/ArrayList        clear           ()V                                        0
java/util/Arrays$ArrayList iterator        ()Ljava/util/Iterator;                     0
java/util/Collections      synchronizedMap (Ljava/util/Map;)Ljava/util/Map;           0
```

That is the complete predicted diff. **A ninth row is a finding.**

### 4.4 `native-api/src/retired_shadow.rs`'s own tests

* `the_table_is_not_empty` — floor is `>= 80`; 88 → 95. Holds.
* `the_table_is_sorted_and_unique` — every new key sorts **before** every
  `java/util/logging/` row (`'A' < 'C' < 'H' < 'c' < 'l'`), so the seven entries
  go at the **head** and no existing row moves.
* `every_entry_is_reachable_through_the_predicate` — **fails unless
  `triple_is_retired_shadow`'s prefix discriminator is widened** from
  `"java/util/logging/"` to `"java/util/"`. One prefix covers this slice and the
  existing 88 rows. That test is the guard the `java/io/Print*` section warns
  about, and it already works; no new test is needed for it.

---

## 5. Why nothing was retired *here* — superseded in part by §8

§5.2's condition has been met: the measurement was taken, and the eight landed
(§8). §5.1's arithmetic obligation is **not** discharged — the three `25/linux`
artefacts still have to be re-frozen from one Linux census, and no Windows lane
can do it. §5.3's inert-entry check was run: all seven triples dispatch today
(§8.4).

### 5.1 The same reason `java/io/PrintWriter` is held

`retired_shadow.rs:159-180`: adding entries alone moves three artefacts frozen at
`25/linux` — `BASELINE_SYNTHETIC_STUBS_*` (`SLACK = 0`), `bridge_shadows_bytecode`
and the kind map — and turns three gates red for a change that is otherwise
correct. **Land them together or not at all.** On a non-Linux host both gate
scripts exit **2** ("REFUSING"), so this lane could not discover whether it got
the numbers right even if it had run them.

### 5.2 …and one reason the PrintWriter section does not have

Two of the seven triples are **behaviour-neutral by construction** and five are
**not measured at all**. §6 is a measurement that costs one binary and no source
edit, and it should run before the table is touched. A retirement landed ahead of
its own measurement is exactly the shape `retired_shadow.rs:94-118` calls
"necessary and NOT sufficient".

### 5.3 The inert-entry trap, live in this slice

`Collections.synchronizedMap` is registered at **four** sites:
`native-builtins/src/phases_early.rs:158` (ambient **`Intrinsic`**, `:111`),
`native-builtins/src/phases_early.rs:2241`,
`native-builtins/src/phases_late/collections.rs:49` (ambient `Bridge`), and
`native-collections/src/lib.rs:51643` (ambient `Bridge`). The kind map records
**one** row, so only one of the four is on the shipping boot path and it is a
`Bridge` — the retag will fire. But the re-tag arm fires **only on an effective
`Bridge`**, so if boot order ever changes and the `Intrinsic` site wins, the
table entry goes inert and silent. That is precisely how
`LogRecord.<init>(Level,String)` sat inert from 2026-08-11 to 2026-08-12. The
disambiguating instrument is the census kind, not `CRATONVM_DBG_DROPPED_STUBS`:
`synthetic-native-registered` is a refusal record, `native-shadows-bytecode` is a
live dispatching shadow.

---

## 6. The measurement this lane could not run — and it needs no source edit

`CRATONVM_ENFORCE_NATIVE_SHADOW` (`vm/src/runtime/env_cache.rs:422-524`) is a
**prefix list** of internal class names, consulted at dispatch under `--jdk-only`
by `resolve_step1_native` (`vm/src/runtime/interpreter/native_override.rs:7129-7135`)
against the *same* class name the registration is keyed on, and it yields on
exactly the §1.4 predicate a retirement uses (`step1_dispatch_has_code`). It is
therefore a per-family simulation of retirement with **zero edits and zero
rebuilds**, and it is the instrument the `java/util/logging` retirement was
accepted on.

Acceptance criterion is **verdict-neutral, not green** — the same criterion
`retired_shadow.rs:33-37` states — because the strict corpus has pre-existing
failures that have nothing to do with collections.

```sh
SUITE=jdk-only CRATONVM_ARGS=--jdk-only bash regression-suite/run.sh          # baseline

# A — the recommended set, plus the interface doors it cannot close by itself
CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/ArrayList,java/util/Arrays$ArrayList,java/util/Collections \
  SUITE=jdk-only CRATONVM_ARGS=--jdk-only bash regression-suite/run.sh

# B — the interface doors alone (§2.2): does closing door 1 do anything at all?
CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/List,java/util/Collection,java/util/Iterator \
  SUITE=jdk-only CRATONVM_ARGS=--jdk-only bash regression-suite/run.sh

# C — the negative controls. Each of these SHOULD go red; if one does not,
#     this document's state-model reading is wrong for that family.
CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/TreeMap,java/util/TreeSet   ...
CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/ArrayDeque                  ...
CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/concurrent/ConcurrentHashMap ...
```

**Read arm A as an upper bound, not as the retirement.** The dial covers *every*
shadowing method on the prefix, so it also yields `ArrayList.size`/`get`/
`iterator`/`stream`/`sort`, which §3.2 says are entangled. A green A is therefore
strictly stronger evidence than the 8 rows need. A red A must be narrowed, and
the dial cannot go finer than a class name — which is the point at which the
table, not the dial, becomes the instrument.

**Arm C is the part that must not be skipped.** Three negative controls that go
red are what turn "we read the source" into "we measured it", and this directory
already has a record of a probe reporting its own reach rather than the defect.

> **RUN 2026-08-12 — and the last sentence came true about arm C itself.** Two
> of the three controls go red against this corpus; `ArrayDeque` does **not**,
> because the corpus never calls `delete(i)`. A prefix alone does not make a
> negative control — each family needs a workload that drives the path its §3
> entry names. §8.2 has the one that turns `ArrayDeque` red.

---

## 7. Residuals

### 7.1 The `synchronizedList` data loss is UNATTRIBUTED — and it is the campaign's headline

`STUB-CENSUS-20260812.md` §5.2 measures `Collections.synchronizedList` returning
6534 and 4697 of 8000 in compatible mode and exactly 8000 under `--jdk-only`, and
attributes it to *"the `Collections.synchronized*` identity-stub-is-not-atomic
defect"*. **That stub does not exist in this tree.** `e8a7caba4` (2026-07-01)
fixed `synchronizedMap` at all four sites; the `synchronizedList` sibling in both
live registrars (`native-collections/src/lib.rs:51795`,
`native-builtins/src/util_concurrent_ext.rs:7424`) constructs a real
`Collections$SynchronizedList` / `SynchronizedRandomAccessList`. Both are
`Bridge`, so **both modes build the same real wrapper**, and the compatible-only
loss must come from something the strict mode refuses *inside* the wrapper's
`synchronized(mutex) { list.add(e) }` — a `SyntheticStub` on the inner `add`
path. §2.2's interface registrations are the obvious suspects and the hypothesis
is **untested**.

Consequence for planning: **retiring `Collections.synchronizedMap` will not fix
the measured defect**, and the defect is the reason this lane exists. It needs
its own record and its own probe — a compatible-mode run with
`--dump-native-registry` and the `invocations` column read along the
`SynchronizedList.add` path would name the culprit in one run.

### 7.2 `Arrays.copyOfRange([BII)[B` — two readings disagree on its ambient kind

The frozen kind map says `bridge`; the nearest `set_category` above
`native-builtins/src/phases_early.rs:1140` is `Intrinsic` at `:884`. The re-tag
fires only on an effective `Bridge`, so this decides whether the row is even
movable. Settle it from a census before counting it.

### 7.3 `force_native_over_real_jdk_bytecode`'s `emptyList` arm reads obsolete

`native_override.rs:2859-2868`, §3.9. Its stated cause was closed when the
singleton became a real `Collections$EmptyList`. Nominated for deletion **as a
pair** with the table entry; neither half alone is useful and the deletion is a
`vm/**` edit.

### 7.4 The two lists in §2.2 and §2.3 disagree with each other

`java/lang/Iterable.iterator` is on the force list (`:3292`) and is deliberately
**not** registered as an interface native (`native-collections/src/lib.rs:28765-28772`);
`java/util/List.iterator` is registered (`:28742`) and is **not** on the force
list. Both directions are anomalies. Neither is this lane's to fix, but a
collections retirement will walk into both.

### 7.5 A `CV-TIMEOUT` is a wall-clock verdict on a host that cannot support one

The corpus runner's own header says the `ms` column is non-metric, and its
`--timeout` is the only reason it can say `CV-BROKEN` at all. On this shared
Windows host, with three sessions running, the same 14-class H2 run produced
five `CV-TIMEOUT` rows at a 200 s cap while HotSpot itself needed 86 s and 74 s
on two of them; two of those five (`TestCluster`, `TestCompatibility`) then
completed standalone well inside 200 s of CPU with nothing changed. A
`CV-TIMEOUT` therefore has to be re-taken standalone before it enters any
before/after comparison. See §8.3.

### 7.6 `run-corpus.sh` reports `rundir: unbound variable` on any non-zero run

Observed twice, verbatim:

```text
corpus=h2 mode=jdk-only  AGREE=9 DIVERGE=0 CV-BROKEN=5 UNADJUDICATED=0
results: .../results.tsv
regression-suite/corpus/run-corpus.sh: line 549: rundir: unbound variable
```

`rundir` is `local` to `cmd_run` (`:420`) and the reported line is `return 1`
(`:549`), so a run that legitimately exits 1 because something was `CV-BROKEN`
ends with a shell error on the way out. It does not change the verdict, and it
does make an honest non-zero exit read like a harness fault. Not this lane's
file to edit.

### 7.7 `org.h2.test.db.TestBackup` is flaky on CratonVM independently of anything

ABBA from a private working directory (`rm -rf data` before each run), one
binary, `--jdk-only`:

```text
  A1 dial-OFF  rc=0            B1 dial-ON  rc=1  MVStoreException: Chunk 2 not found
  B2 dial-ON   rc=0            A2 dial-OFF rc=1  MVStoreException: Chunk 2 not found
```

Both arms produce both outcomes, so the failure is attributable to neither. It
needs its own record; a one-shot corpus run will keep attributing it to
whatever change happens to be under test.

### 7.8 Not adjudicated here

`ArrayList.sort(Comparator)` and `ArrayList.stream()` (held for measurement,
§3.2); the `HashSet.iterator` improvement blocked only by door 2; the
`Map.values()` view-class rewrite that would unblock eight more ArrayList
registrations; and the `hm_int_fast` / `lhm_overlay` retirements that would
unblock `HashMap` and `LinkedHashSet`. Each is a `native-collections` change, and
the census's own lane table puts that file in L5 — *"do last, alone"*.

---

## 8. The measurement, taken 2026-08-12 — and the eight are landed

One prebuilt binary (`jdkonly-wave2-target/release/cratonvm.exe`), Windows host,
JDK 25.0.3, only `CRATONVM_ENFORCE_NATIVE_SHADOW` differing between arms. No
rebuild, no source edit, exactly as §6 says.

### 8.1 Arms A and B are verdict-neutral; C1 and C3 are red

`SUITE=jdk-only CRATONVM_ARGS=--jdk-only bash regression-suite/run.sh`:

| arm | scope | result |
|---|---|---|
| baseline | (dial off) | **32 passed / 4 failed** |
| A | `ArrayList,Arrays$ArrayList,Collections` | **32 / 4**, identical failing set |
| B | `List,Collection,Iterator` (§2.2 doors) | **32 / 4**, identical failing set |
| C1 | `TreeMap,TreeSet` | **10 / 51** RED |
| C2 | `ArrayDeque` | 32 / 4 — see §8.2 |
| C3 | `concurrent/ConcurrentHashMap` | **28 / 12** RED |

The failing set in the baseline and in A and B is `RJdkProxyIface`,
`RJdkForeign`, `RJdkEnumerations` (+ the `RJdkEnumerations` harness row). C3
adds `RJdkModule`, `RJdkSecurity`, `RJdkX509Intercept`, `RJdkLogging`.

Arm B answering neutral is worth stating on its own: closing the §2.2 interface
doors alone changes nothing on this corpus, so the doors are not silently
carrying these dispatches today.

### 8.2 §6's arm C is NOT sufficient as written — the ArrayDeque control is vacuous

C2 came back verdict-neutral, which §6 says would mean "this document's
state-model reading is wrong for that family". It is not. **The probe's reach is
wrong.** The `RJdk*` corpus only pushes and pops a deque's ENDS, and real
`ArrayDeque.addLast`/`pollFirst` never call `delete(i)` — the method §3.5 names.
A probe that does (40 `addLast`s past the initial capacity, a middle
`remove(Object)`, four `Iterator.remove`s, then an alternating drain and a
reuse), same binary, same dial, `--jdk-only`:

```text
                          dial OFF        dial=java/util/ArrayDeque   HotSpot
  size after 40 addLast   40              39                          40
  size after It.remove×4  35              39                          35
  toArray length          35              34                          35
  drained count           35              41                          35
  pollFirst after reuse   "z"             null                        "z"
```

Five wrong answers, and the shape is exactly the two-writers-on-`size`
corruption §3.5 predicts. **All three negative controls go red once each is
driven on the path its own §3 entry names.** The same probe under arm C1 gives
`TreeMap.firstKey` = `k1` for a map whose first key is `k0` and a `TreeSet` that
kept 1 of 2 elements; under C3, `ConcurrentHashMap.size` = 1 for a two-entry map
with `containsKey` false for a key that is present. Under arm A every ArrayList,
`Arrays.asList` and `synchronizedMap` line is HotSpot-identical.

The general lesson is already in this directory: a narrow probe reports its own
reach, not the defect. **§6's arm C should be re-written to name a workload per
family, not just a prefix.**

### 8.3 The H2 corpus moved nothing

`bash regression-suite/corpus/run-corpus.sh run h2 --classes-from <14>
--cv <binary> --mode jdk-only --timeout 200`, dial off then dial at arm A:

```text
  BEFORE (dial off)   AGREE=9  DIVERGE=0  CV-BROKEN=5   14/14 classes
  AFTER  (arm A)      AGREE=8  DIVERGE=1  CV-BROKEN=3   12/14 (arm was cut short)
```

Per class over the 12 both arms adjudicated, **every verdict is identical except
`TestBackup`** — and `TestBackup` is a pre-existing flake, proven by ABBA
(§7.7): the same `MVStoreException: Chunk 2 not found` appears with the dial
**off**. The two classes the cut-short arm missed were re-run standalone in both
arms: `TestCluster` passes in 161–183 s in both (its corpus `CV-TIMEOUT` was
host contention), and `TestCompatibility` exceeds even a doubled 400 s cap in
both (`rc=124` at 406 s dial-off, 403 s dial-on) — pre-existing either way.

This BEFORE does not reproduce the AGREE=10 DIVERGE=2 CV-BROKEN=2 the campaign
carried for today, and the reason is §7.5: it ran alongside another arm on a
three-session host, HotSpot itself needed 86 s and 74 s on two of these classes,
and five 200 s caps fired. **The comparison that survives is the per-class one
between two arms, not either arm's totals.**

### 8.4 The census says all seven are live, and confirms the `Arrays.copyOf` keep

`--jdk-only --explain-jdk-only --jdk-only-report` on an ordinary collections
workload reports every one of the seven triples as an actually-taken
`native-shadows-bytecode` row — the kind that means a dispatching shadow, not
the `synthetic-native-registered` refusal record. None is an inert entry.

Arming arm A makes **`java/util/Arrays.copyOf([Ljava/lang/Object;I)` appear** in
the taken-shadow set, where the dial-off run never reaches it. That is real
`ArrayList.grow` funnelling through it the instant the native constructors
yield — §3.3's "load-bearing *for* the retirements" confirmed by experiment
rather than by reading, and the reason it must stay a `Bridge`.

One instrument caveat, because it inverts the obvious reading: with the dial
armed the yielding row is **still** recorded `native-shadows-bytecode` (the
record is written on the yield path too), so the per-run census cannot tell you
whether the dial engaged. Only the behavioural probe can. After the table lands,
those rows leave the population for a different reason: a `SyntheticStub` is
refused at the door and never reaches step 1.

### 8.5 What landed, and what did not

Landed in `native-api/src/retired_shadow.rs`: the seven triples at the head of
`RETIRED_SHADOW_TRIPLES` (88 → 95 entries), the discriminator widened from
`java/util/logging/` to `java/util/`, and two new tests — one asserting the
seven are reachable, one asserting the held families and `Arrays.copyOf` are
**not** retired, which is what earns the wider prefix.

Not landed, and it is the same obligation §5.1 names: the three `25/linux`
artefacts. They cannot be re-frozen from a Windows host — both gate scripts exit
2 ("REFUSING") — and the record is explicit that a hand-derived count landing
too high widens a slack-free ratchet. The predicted diff, to check against a
real Linux census rather than to paste:

```text
  stub ratchet          +8   (from the OBSERVED 1261 no-management: 1261 + 8 = 1269;
                              the management figure compounds two unmeasured terms)
  bridge.rows                       9711 -> 9703
  bridge.shadows_bytecode           4457 -> 4449
  bridge.shadows_bytecode_anywhere  6066 -> 6058
  bridge.without_acc_native         8912 -> 8904
  registrations.bridge              9711 -> 9703
  registrations.synthetic-stub      1262 -> 1270
  total_rows                       11636 -> 11636      (a retag moves no row)
  bridge.stated_shadows_bytecode      24 ->   24       (unchanged)
  kind map: EXACTLY 8 rows, bridge -> synthetic-stub, kind_stated 0 -> 1
```

All eight kind-map rows were verified present as `bridge` / `kind_stated=0`
before the edit (`:6241-6244`, `:6246`, `:6248`, `:6314`, `:6360`), and
`NativeMethodRegistry::register:5782-5798` was verified to set both the kind and
`next_kind_stated`. **A ninth row is a finding.** The base itself is stale by its
own note, so these are deltas on a base nobody has taken.
