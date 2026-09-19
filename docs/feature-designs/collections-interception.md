# Natives on inheritance-intercepting collection base classes

**Status:** Shipped (default on, in the plain real-JDK build) — this is load-
bearing behaviour, not a legacy path.

## What it does today

`register_collections_natives` is called from **three** places in
`vm/src/vm/vm_init.rs`: the synthetic arm, the real arm inside
`#[cfg(feature = "synthetic-jdk")]`, **and** the
`#[cfg(not(feature = "synthetic-jdk"))]` block — i.e. the default
`cratonvm-cli` build. So `AbstractCollection.toArray` / `.contains`,
`AbstractSet.hashCode`, the `SET_CLASSES` loop and the `java/lang/Object`
collector functions are all live in default real-JDK mode. The only
feature-gating inside `native-collections` is on internal helpers, not on the
registrar.

The correctness gate is separate from registration:
`set_drop_real_layout_synthetic(true)` runs in the real arms *before*
registration and removes specific families (StringJoiner, Cleaner,
Pattern/Matcher, EnumSet, StringReader/Writer, the `Executors` pool factories,
`java/lang/String` bridges).

**The interception must stay.** The recurring defect in this area is not that
a native intercepts an inherited call — it is that only *some* of the natives
behind the interception carry the receiver-agnostic fallback their own
comments claim, on the receiver side and on the argument side of bulk
operations alike.

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

## Related

* `native-builtins/tests/shim_inheritance_guard.rs` — the executable inventory
  of which shims exist and which base classes they intercept. The gate, not a
  document, is the live list.
* `docs/synthetic-vs-real-explained.md` — why a synthetic stub can win over a
  real JDK class.
