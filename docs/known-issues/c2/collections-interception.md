# `native-collections/` — natives on inheritance-intercepting base classes

**Status: 🟡 PARTIALLY FIXED 2026-08-01.** One handed-over item was a **false
premise** and is closed by reading (`AbstractSet.hashCode`). The other was
real but not in the way it was described: the interception is load-bearing and
must stay, and the defect was that only *one* of the natives behind it had the
receiver-agnostic fallback the other five claimed in their own comments. The
same trap was then found on the argument side of four bulk operations. Fixed
with nine regression tests. Four residuals are left with a recipe each.

Companion to `docs/known-issues/c2/native-builtins-shim-audit.md`, which handed
over *Cross-crate 1* and *Cross-crate 2* to this crate. **Do not re-derive the
dispatch mechanism** — it is established there and only summarised below.

## The mechanism, in one paragraph

`NativeMethodRegistry::find` is an exact `(class, method, descriptor)` lookup;
the inheritance comes from the caller. When a receiver does not declare a
method, `try_stackless_invoke` climbs the receiver's **superclass** chain and
asks the native registry at each ancestor **before** it asks whether that
ancestor has bytecode (`vm/src/runtime/interpreter/invoke.rs:11292`–`:11341`),
and a registered native wins over real bytecode unconditionally in the default
dispatch mode (`resolve_step1_native`, `:10838`). So **a native on an abstract
class is inherited by every non-overriding subclass — JDK, third-party and user
alike — and beats that base class's real implementation.** Interfaces are not
walked, so an interface registration only fires when the receiver's runtime
class *is* the interface.

`register_collections_natives` (`native-collections/src/lib.rs:1672`) is called
from **both** arms of `vm/src/vm/vm_init.rs` (`:1426`, `:1759`, `:2231`), so
everything below is in the **default build**, not just `synthetic-jdk`.

## Census — every `native-collections` registration on a base its subclasses inherit

Derived from a whole-crate scan of `.register(...)` targets, resolving the
`let c = "…"` bindings and the `SET_CLASSES` / `UNMOD_*` loops. Reproduce with:

```sh
perl -0777 -ne 'while(/\.register(?:_static|_with_kind)?\s*\(\s*"([^"]+)"\s*,/gs){print "$1\n"}' \
  native-collections/src/lib.rs | sort | uniq -c | sort -rn
rg -n 'let c = "|for c in |const .*CLASSES' native-collections/src/lib.rs
```

| registration | registered on | who inherits it | what it reads | correct? | action |
| --- | --- | --- | --- | --- | --- |
| `toArray()[Ljava/lang/Object;` → `native_al_to_array` | `java/util/AbstractCollection` (`:3247`) | **every** Collection in the VM that does not declare `toArray()` | heuristics, then the receiver's real `size()`/`iterator()` | **yes** | kept; fallback moved into the shared helper |
| `toArray([Ljava/lang/Object;)…` → `native_al_to_array_typed` | `java/util/AbstractCollection` (`:3236`) | same set | *was* heuristics **only** → empty array for any layout it could not decode | **NO** | ✅ **FIXED** — now shares the fallback |
| `contains(Ljava/lang/Object;)Z` → `native_al_contains` | `java/util/AbstractCollection` (`:3253`) | same set | *was* heuristics only → a flat `false` for a present element | **NO** | ✅ **FIXED** — now shares the fallback |
| `hashCode()I` → `native_hs_hash_code` | `java/util/AbstractSet` (`:10550`) | `TreeSet`, `LinkedHashSet`, `EnumSet`, `Collections$UnmodifiableSet`, user `extends AbstractSet` | HashSet backing map **only when the receiver really is a HashSet/COWAS**, else `collect_collection_elements_or_real`; sums element hashes via a **virtual** `hashCode()` | **yes** — this *is* the `Set.hashCode` contract | none; premise was wrong, see below |
| `supplier`/`accumulator`/`finisher`/`combiner` → `make_collector_fn` | `java/lang/Object` (`:19487`–`:19505`) | universally | **nothing** — wraps any receiver unconditionally | partly | ⚠️ Residual 1 |
| `<init>`/`size`/`add`/`contains`/`iterator`/`toArray`/`hashCode`/`equals`/… → `native_hs_*` | `java/util/HashSet`, `LinkedHashSet`, `CopyOnWriteArraySet` (`SET_CLASSES`, `:10477`) | user `extends HashSet` etc. | HashSet layout (field 0 = backing map) | yes for HashSet/LinkedHashSet | ⚠️ Residual 2 (COWAS) |
| `native_al_*` on `java/util/Vector`, `java/util/Stack` | concrete | user subclasses | ArrayList slots — but resolved **per receiver** by `al_slots_for` (`:2856`), which reads `elementCount` for a Vector | yes | none |
| ~140 registrations on `java/util/{List,Set,Map,Collection,Queue,Deque,Comparator,Iterator,…}` | **interfaces** | nobody — the walk climbs `superclass` only | mixed | n/a | not swept; see `native-builtins-shim-audit.md` Tier 3 |
| everything else | concrete `java.util` classes (`HashMap`, `TreeMap`, `ArrayDeque`, `PriorityQueue`, `ConcurrentHashMap`, `ScheduledThreadPoolExecutor`, …) | their own subclasses, which share their layout | their own layout | yes | none |

`java/util/AbstractList`, `AbstractSequentialList`, `AbstractQueue` and
`AbstractMap` carry **no** registrations in this crate. That is now pinned by
a test rather than by this sentence.

## What the two handed-over items actually were

### Handed-over 1 — `AbstractSet.hashCode`: premise WRONG, closed by reading

The audit recorded it as "points at `native_hs_hash_code`, a HashSet-layout
reader". It is not one, and has not been one since the HIB-CV-25 fix. Read
`native_hs_hash_code` (`native-collections/src/lib.rs:10647`):

* the HashSet-layout read is behind `hs_backing_map` (`:10214`), which is
  itself behind `is_hashset_native_backed` (`:10199`) — an explicit
  `cid == HashSet || is_subclass(cid, HashSet)` (plus `CopyOnWriteArraySet`)
  test. A `TreeSet`, an `EnumSet` or a user `extends AbstractSet` never reaches
  the layout read;
* everything else goes to `collect_collection_elements_or_real` (`:32509`),
  which falls back to the receiver's own `size()` + `toArray()`;
* the result is `h.wrapping_add(element_hash_code(e))` over the elements, and
  `element_hash_code` (`:5684`) dispatches the element's **virtual**
  `hashCode()`. That is exactly `Set.hashCode`'s specified sum.

The 15-line comment above the function already documents this history — it
names the receiver that broke it (Weld's `ImmutableTinySet$Doubleton`, elements
in `element1`/`element2` fields, no backing map) and the symptom (a `0` hash,
so a `Map<Set<..>,..>` keyed by such a set was unfindable).

**Would deleting it be safe, as the sibling lane's `AbstractMap` deletion was?
No — and the reason is worth keeping.** That deletion was safe because
`AbstractMap.equals`/`hashCode` fall through to `java/lang/Object`'s natives,
which are correct *for identity*. `hashCode` also exists on `Object`, so the
same fallthrough exists here mechanically — but its answer is **not** correct
here: identity hashing a Set breaks the `Set.hashCode` contract (two equal sets
must hash equally), which is precisely the defect the `AbstractMap` deletion was
fixing. Deletion would be correct only in the real-JDK arm, where
`AbstractSet.hashCode`'s own bytecode would run. Keeping a native that computes
the contract answer in both arms is strictly better. **Left as-is.**

### Handed-over 2 — `AbstractCollection.toArray`×2 / `contains`: interception is load-bearing, two of the three were wrong

The in-situ comments (`:3232`–`:3234`, `:3241`–`:3245`) say the interception is
deliberate, and they are right. What it compensates for:

* `EnumSet.allOf(..).toArray()` resolves to `AbstractCollection.toArray()`,
  whose real bytecode loops `iterator()` — and the `Iterable.iterator` native
  only models ArrayList layout, so it would iterate zero elements;
* `ArrayList.toArray(T[])`'s real bytecode calls
  `Arrays.copyOf(elementData, size, a.getClass())`, which NPEs on a synthetic
  ArrayList because `a.getClass()` returns a mirror without the
  array-component-type metadata. Spring Boot's fat-jar
  `Launcher.createClassLoader(Collection)` does `c.toArray(new URL[0])` and
  depends on this.

So refusing is not available: **`toArray` and `contains` do not exist on
`java/lang/Object`**, so dropping the registrations would surface a
`NoSuchMethodError` in the synthetic arm rather than fall through to a correct
implementation. The `AbstractMap` shape does not transfer. The only option is
to answer correctly for the receiver we actually have.

The defect was that only one of them did. All three registrations funnel into
element collection, and `collect_collection_elements` (`:32545`) **must not**
drive `iterator()` — the `iterator()` native snapshots *through* it, so it
would recurse; its closing comment (`:33096`) states this and that an
unmodelled layout therefore "materialise[s] empty". Empty is indistinguishable
from genuinely-empty, so the fallback has to live in the caller. It lived in
exactly one caller:

| native | before | after |
| --- | --- | --- |
| `native_al_to_array` (`:3949`) | heuristics + null-hole guard + real `size()`/`iterator()` fallback, all inlined | unchanged behaviour, logic moved to the shared helper |
| `native_al_to_array_typed` (`:4146`) | **heuristics only** | fallback |
| `native_collection_to_array_generator` (`:4196`) | **heuristics only** | fallback |
| `native_al_for_each` (`:13301`) | **heuristics only** | fallback |
| `native_al_stream` (`:17505`) | **heuristics only** | fallback |
| `native_al_contains` (`:3826`, non-list branch) | **heuristics only** | fallback |

The four middle rows' own doc comments already claimed the fallback
("*whose iterator fallback materialises everything else through the real
`iterator()`*") — they were describing `native_al_to_array`'s inlined copy of
it, not what they did. That is what made this survive: the code read as if it
were already handled.

**Who this actually hurt.** `AbstractCollection`'s documented minimal subclass
contract is "implement `iterator()` and `size()`" — a subclass need expose no
readable element storage at all, which is the exact shape
`collect_collection_elements` cannot decode. For such a receiver, before this
change:

* `coll.toArray(new T[0])` returned a **zero-length array**;
* `coll.contains(x)` returned **`false` for every `x`**, including present ones;
* `coll.forEach(a)` visited **nothing**;
* `coll.stream()` was **empty**;

while `coll.toArray()` on the very same object returned the right elements.
None of these fail loudly. `collect_collection_elements` has accumulated ~15
hand-written per-class special cases (Kafka `ImplicitLinkedHashCollection`,
Jetty `BlockingArrayQueue`, Hibernate `org.hibernate.collection.*`, Kotlin
`ArrayAsCollection`, `RegularEnumSet`, `PriorityQueue`, `ArrayDeque`, MSC's
`IdentityHashSet` …) — each one is a bug report from this family that was fixed
by naming the class rather than by fixing the fallback.

## What changed

All in `native-collections/src/lib.rs`.

1. **`al_or_collection_elements` (`:4113`) is now the layout-agnostic reader.**
   It was a one-line alias for `collect_collection_elements`. It now runs the
   sequence `native_al_to_array` used to inline: heuristic snapshot → if it
   looks suspect (null holes in a non-`List`, `heuristic_snapshot_is_suspect`)
   prefer a re-entrancy-guarded real-iterator walk → if the heuristics found
   nothing, ask the receiver's own `size()` and only then walk its real
   `iterator()`. The receiver is pinned across the virtual calls.
   *Ordering is copied verbatim so the zero-arg `toArray()` path keeps its
   exact behaviour, including its use of the unguarded
   `collect_via_real_iterator` on the empty branch and the guarded
   `collect_via_real_iterator_once` on the suspect branch.*
2. **`native_al_to_array` (`:3949`) now calls that helper** instead of carrying
   its own copy — so the five other natives intercepting the same receivers
   inherit the fallback.
3. **`native_al_contains` (`:3826`)** uses the helper on its non-list branch.
4. Three stale doc comments corrected (`native_al_for_each`, `native_al_stream`,
   `al_or_collection_elements`), each now saying when the claim became true.
5. **The `java/lang/Object` collector-bridge comment (`:19464`)** claimed "the
   callbacks still validate the receiver layout/tag". They do not — see
   Residual 1. The comment now says so.
6. **The same trap on the ARGUMENT side of four bulk operations.**
   `collect_collection_elements_or_real` is the argument-side wrapper (it drives
   the argument's `size()`/`toArray()`), and its own doc listed `removeAll` /
   `retainAll` as callers — they were not. `native_al_remove_all`,
   `native_al_retain_all`, `native_ll_add_all` and `native_ad_add_all` read
   their argument through the bare heuristic reader, so a bulk op against an
   unmodelled collection was a silent no-op that still returned `false`.
   `retainAll` was worse than a no-op: an empty argument means "retain
   nothing", i.e. it **cleared** the receiver. All four now use the wrapper;
   `native_al_add_all` and the copy constructors already did. Its
   recursion-safety paragraph is also corrected — `toArray()` now reaches
   `al_or_collection_elements`, not `collect_collection_elements` directly, so
   the deepest path gained one virtual call but still no cycle.

**Recursion argument (why driving `iterator()` from these six is safe):** the
`iterator()` native (`native_al_iterator`) snapshots through
`collect_collection_elements`, which never drives `iterator()` and never calls
`contains`/`toArray`/`forEach`/`stream`. So the deepest chain is
`toArray → iterator() → native_al_iterator → collect_collection_elements`,
which terminates. The suspect branch additionally uses the thread-local
re-entrancy guard `collect_via_real_iterator_once`. `native_al_iterator`'s own
call to `collect_collection_elements` (`:4368`) was deliberately **not**
switched to the helper — that is the one call site where it would recurse.

**Cost.** The fallback is behind an `is_empty()` check plus a `size()` probe, so
a receiver whose layout is readable (every synthetic collection, every real
`ArrayList`/`HashSet`/`TreeSet`/`LinkedList`/`ArrayDeque`/`PriorityQueue`) pays
nothing and issues no virtual call. A genuinely empty foreign collection pays
exactly one `size()` call. Both are pinned by test.

### Tests

`native-collections/tests/abstract_collection_interception.rs` (new, 9 tests).
The fixture models the minimal `AbstractCollection` subclass: a zero-field
receiver of an application class, with the JDK collection names interned first
(the layout guards are *lenient* for names the class manager does not know, so
a mock that never mentions `java/util/ArrayList` would let the foreign receiver
through the ArrayList probe and stop testing anything).

| test | what it pins |
| --- | --- |
| `to_array_typed_on_an_unmodelled_receiver_uses_its_real_iterator` | **fails before the fix** — the T[] overload returned a zero-length array |
| `contains_on_an_unmodelled_receiver_finds_a_present_element` | **fails before the fix** — `contains` returned `false` |
| `for_each_on_an_unmodelled_receiver_visits_every_element` | **fails before the fix** — `forEach` visited nothing |
| `remove_all_reads_an_unmodelled_argument_collection` | **fails before the fix** — the argument-side gap; `removeAll` removed nothing and reported `false` |
| `to_array_on_an_unmodelled_receiver_uses_its_real_iterator` | the zero-arg path, which already worked, is not regressed by the move |
| `contains_on_an_unmodelled_receiver_still_rejects_an_absent_element` | negative control — "always true" would pass the positive test |
| `an_empty_unmodelled_receiver_is_answered_from_size_alone` | the `size()` guard, by asserting the **absence** of the `iterator()` call; an empty result alone looks identical with or without the guard |
| `a_readable_receiver_is_answered_from_its_layout_without_virtual_calls` | the fallback stays a fallback — a real ArrayList issues no `size()`/`iterator()` dispatch |
| `abstract_collection_carries_exactly_the_three_audited_natives` | the census itself: the three `AbstractCollection` rows and the one `AbstractSet` row exist, `AbstractSet.equals` stays unregistered, and `AbstractList`/`AbstractSequentialList`/`AbstractQueue`/`AbstractMap` stay empty |

Run: `cargo test -p cratonvm-native-collections --test abstract_collection_interception`.

## Residuals

### Residual 1 — the `java/lang/Object` collector bridge validates nothing

`lib.rs:19487`–`:19505` register `supplier()`, `accumulator()`, `finisher()`
and `combiner()` on **`java/lang/Object`**, the universal base, for a stated
reason: "some real-JDK interface dispatch paths can arrive with a tagged
synthetic Collector whose runtime class has collapsed to Object". The comment
used to end "*the callbacks still validate the receiver layout/tag*". They do
not: `make_collector_fn` (`:19610`) is three lines and wraps whatever receiver
it is handed, unconditionally.

Blast radius is narrow but not empty: the walk reaches `Object` only when the
receiver's class and **every superclass** leave the method undeclared — which
includes a receiver whose implementation is an **interface default method**,
because the superclass walk has not consulted interfaces yet. Such a receiver
gets a synthetic SAM object instead of its own default method. The damage
surfaces one call later: the SAM natives (`native_collfn_supplier_get` and
siblings) *do* check `collector_tag_of` and degrade to an empty list/map.

Not fixed here because there is **no way for a native to decline a call** —
`MethodCallResult` (`types/src/error.rs:32`) has `Ok(Some)`, `Ok(None)` and
`Err`, and no "not handled" arm. The only fail-closed options are to throw
(which would break any legitimate default-method receiver) or to drop the four
registrations. **Recipe:** find the "collapsed to Object" Collector path these
were added for (`Stream.collect` against a real-JDK `Collectors$CollectorImpl`),
confirm whether it still occurs, and if it does not, delete all four and the
four `r.find(object, …)` assertions in the in-file registration test
(`lib.rs:~51082`).

### Residual 2 — `CopyOnWriteArraySet` is claimed as HashSet-layout

`is_hashset_native_backed` (`:10199`) reports `java/util/concurrent/CopyOnWriteArraySet`
as HashSet-backed, so `hs_backing_map` reads its **field 0** as a backing
`HashMap`. In the real JDK, `CopyOnWriteArraySet`'s field 0 is `al`, a
`CopyOnWriteArrayList` — not a map. Reading it as one is a layout reader on the
wrong layout: `map_collect_keys` yields nothing or garbage, and
`AbstractSet.hashCode` / `HashSet.*` would answer from it.

It is currently unreachable *by construction*: `SET_CLASSES` also registers
HashSet's `<init>` on `CopyOnWriteArraySet`, and a registered native wins, so
every COWAS the VM builds gets the synthetic backing-map layout in slot 0 and
is internally consistent. The exposure is the half-real-object shape: any COWAS
method **not** in `SET_CLASSES` (`addIfAbsent`, `addAllAbsent`, …) runs real
bytecode that does `al.addIfAbsent(..)` against what is now a `HashMap`.
**Recipe:** decide whether COWAS is modelled synthetically or not, and make it
one or the other — either complete the native surface or drop COWAS from both
`SET_CLASSES` and `is_hashset_native_backed`. Needs a run of the concurrency
suites, which this lane cannot do.

### Residual 3 — `native_hs_hash_code` fails open when it cannot read the set

If the heuristics find nothing **and** the receiver's own `size()`/`toArray()`
cannot be driven, `collect_collection_elements_or_real` returns empty and
`hashCode()` answers `0` for a non-empty set. That is a wrong answer, not a
refusal, and it is the exact failure the HIB-CV-25 fix was about — narrowed,
not eliminated. It is only reachable for a receiver with neither a modelled
layout nor working `size()`/`toArray()` bytecode, i.e. a bare synthetic stub,
which has no elements anyway. **Recipe:** if a real instance shows up, prefer
`identity_hash_code` over `0` for a receiver whose `size()` reports non-empty
but whose elements cannot be read — an identity hash is still contract-violating
but at least is not the same value for every unreadable set.

### Residual 4 — `native_al_stream` collects twice

`native_al_stream` (`:17505`) calls `al_or_collection_elements` once for the
length and again for the elements (deliberately: the array allocation between
them can move every element, so the second snapshot must be taken after it).
With the fallback in place that is now **two** full iterator walks for an
unmodelled receiver. Correct — `Collection.iterator()` is re-iterable — but
O(2n) virtual dispatch. **Recipe:** pin the first snapshot's elements with
`pin_value_slice` and re-read them through the pins after the allocation, the
same shape `native_al_to_array` already uses, instead of re-collecting.

## Cross-file changes this lane needed and could not make

None. Everything in *What changed* is inside `native-collections/`.

One observation for whoever owns the gate: `native-builtins`' new
`shim_inheritance_guard.rs` test
(`the_java_util_abstract_collection_bases_carry_no_natives_at_all`) asserts that
`AbstractList`/`AbstractSet`/`AbstractCollection`/`AbstractSequentialList`/
`AbstractQueue` carry **no** registrations — but it is scoped to the
`native-builtins` registry only, and the statement is **false for the merged
registry the VM actually builds**: `native-collections` puts four natives on
two of those five names, all four intentional. If that gate is ever widened to
the full registry it must gain those four rows, with this document as the
reason. The equivalent assertion for this crate now lives in
`abstract_collection_interception.rs`.

## Related

* `docs/known-issues/c2/native-builtins-shim-audit.md` — the mechanism, and the
  hand-over of *Cross-crate 1* / *Cross-crate 2* that this document answers.
* `docs/synthetic-vs-real-explained.md` — why a synthetic stub can win over a
  real JDK class.
