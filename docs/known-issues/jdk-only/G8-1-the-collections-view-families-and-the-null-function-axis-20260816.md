# G8-1 — the collections view families, and the null-function axis

> **RECONCILED 2026-08-17 (lane G40) — the provenance premise "no JDK source was
> read" was avoidable.** `C:\craton\jdk25src` is indeed absent, and this record
> is right about that. But the JDK's sources ship with the oracle itself, at
> `$JAVA_HOME/lib/src.zip` (52,462,198 bytes) — the sources of the exact
> HotSpot 25.0.3+9-LTS build used here, so the view classes' declared fields are
> readable rather than inferable from `javap -p`. Nothing measured in this record
> is invalidated. Since it was written, `G22-1`/`G13-1` proved the
> `LinkedHashMap$LinkedValues` failure to be a **field-slot collision** rather
> than an interface-door problem, and `RJdkMapViews` went green, MEASURED, at
> `783685c34`. See `INDEX.md` §B.3.

**Status:** ORACLE MEASURED / **CRATONVM PREDICTED**.

**Provenance, stated first because it is the thing most easily lost.** Every
row in §2, §3 and §6 was **MEASURED on HotSpot 25.0.3+9-LTS** (`$JAVA_HOME`,
single-file source mode) on 2026-08-16. **Not one number in this record was
measured on a CratonVM binary.** This lane could not build and could not run
the VM — the orchestrator owns the build and a release build was running for
the whole of this lane's window. Every claim about what CratonVM does today is
therefore either **SOURCE-VERIFIED** (I read the body and say so) or
**PREDICTED**, in the exact sense HANDOFF-20260814 §2 warns about. Every "after"
claim in §4 is a **PREDICTION**. Treat all of it as analysis until a binary
agrees with it.

Branch `claude/jdk-only-mode-completion-1351c0`, working tree at `c479a668a`
plus the wave's uncommitted lanes, 2026-08-17. `C:\craton\jdk25src` is absent
on this machine, so no JDK source was read — only behaviour, and `javap -p`
bytecode against the same JDK.

Probes: `scratchpad/g8/NullFn.java`, `scratchpad/g8/Views.java`,
`scratchpad/g8/Order.java`; raw output in `scratchpad/g8/nullfn-out.txt`,
`views-out.txt`, `order-out.txt`. All printed labels are ASCII, deliberately
(HANDOFF §7's em-dash incident). `<NO-MESSAGE>` means `getMessage()` returned
**`null`**, printed distinctly from `""` on purpose.

Picks up the axis `G1-1` §5 measured and deliberately left ("**Not fixed,
measured, recorded**"), and the OPEN collections-view records `C7-1`, `C13-1`,
`C13-2`, `C13-3`, `C7-3`, `W7-1`, `W7-33`, `W7-36`, `W7-96`, `W7-65`.

---

## 0. The headline

* **The null-function axis is one rule, and it is the widest uniform column
  this campaign has measured.** Every `forEach` / `replaceAll` / `removeIf` /
  `computeIfAbsent` / `compute` / `computeIfPresent` / `merge` on every mutable
  receiver answers a **bare, message-less `NullPointerException`** — 199 of 266
  probe rows, with no receiver-specific variation at all. Contrast `Hashtable`'s
  null-key axis, which G1-1 found was two rules with observable ordering.
* **G1-1's six named cells were 22.** It named `computeIfAbsent`/`compute`/
  `computeIfPresent`/`forEach`/`replaceAll` on `HashMap`/`LinkedHashMap`.
  Sweeping the family per HANDOFF §4 found **22 bodies** in
  `native-collections/src/lib.rs` with the identical fabricated-success shape,
  across `TreeMap`, `ArrayList`, `LinkedList`, `Vector`, `HashSet`, `TreeSet`,
  `ArrayDeque`, `ConcurrentHashMap` and the `KeySetView`. All 22 are fixed here.
* **Three traps in that axis, each of which a blanket rule gets wrong.**
  (a) `list.sort(null)` is **not** a null-argument error — a null `Comparator`
  MEANS natural ordering and sorts normally, measured on nine receivers;
  (b) an **immutable** receiver answers `UnsupportedOperationException`, not
  NPE, for every *mutating* method — but still NPE for `forEach`;
  (c) `native_chm_for_each_parallel`'s function is argument **2**, not 1,
  because its descriptor is `(JLjava/util/function/BiConsumer;)V`.
* **The fix does not reach `Properties`, and may not reach
  `ConcurrentHashMap`.** SOURCE-VERIFIED: `properties_sidetable.rs` is
  re-registered **after** `register_collections_natives` behind an explicit
  `LAST-WRITE-WINS BOUNDARY` comment in `vm_init.rs`, so two of the triples this
  lane edited are owned by another crate. `util_concurrent_ext.rs` competes for
  two more and its ordering **differs per VM-init arm**. This is HANDOFF §5's
  failure mode #1 caught before the build, not after. See §5 and §7.
* **`C13-1`'s routing layer is still inert, and now for a stated reason.**
  `vc_route` fires only on `CF_VALUES_VIEW`, which is set by an exact match on
  five class names; none of the five is ever constructed, because `values()` is
  natively registered on all six map classes and returns an `ArrayList` carrier.
  It remains reachable only across the JIT/warm-path bytecode cliff `C13-2` N1
  describes — and that fix is in `vm/**`. **C13-1 cannot close.**
* **One "obvious fix" that measurement stopped.** `C13-1`/`C13-2` both record
  that `Hashtable.values()` returns `Collections$SynchronizedCollection`, and
  `is_values_view_class` names `java/util/Hashtable$ValueCollection` — which
  reads exactly like a phantom entry to delete. It is not:
  `javap -p 'java.util.Hashtable$ValueCollection'` shows the class **exists**,
  with `final java.util.Hashtable this$0` at slot 0, and `Hashtable.values()`
  wraps it. The recognition set is correct. **No code change was made.**

---

## 1. What the axis is, and why "fabricated success" is the right name

The shape, before this lane, at all 22 sites:

```rust
let action = match args.get(1) {
    Some(Value::Object(Some(r))) => *r,
    _ => return Ok(None),          // <-- an explicitly passed null lands here
};
```

`Ok(None)` is `void`. `Ok(Some(Value::Int(0)))` is `false`.
`Ok(Some(Value::Object(None)))` is `null`. In every case the caller is told the
call **succeeded** and that nothing needed doing. The JDK throws.

That is the worst defect class in this campaign's taxonomy, and worse than a
wrong exception, because nothing anywhere fails: no assertion trips, no fixture
reddens, and the application proceeds on an answer that is a lie. `W7-36` says
the same thing about a mistyped refusal — "*a mistyped refusal is worse than a
missing one, because it looks handled*" — and a fabricated success is one step
worse still, because it does not even look handled.

The distinction the fix preserves: `args.get(n) == None` (a **missing**
argument) is a malformed native call inside this VM and keeps its no-op;
`Some(Value::Object(None))` (an **explicitly passed** null) throws. Reporting the
first as a Java NPE would put a VM dispatch bug into an application's catch
block wearing the costume of a program error. `native_map_merge` and
`ht_reject_null_key` already drew this line; this lane reuses it rather than
inventing a second convention.

---

## 2. The oracle table — the null-function axis, MEASURED

`java -version` -> `openjdk 25.0.3 2026-04-21 LTS / Temurin-25.0.3+9`.
Full output: `scratchpad/g8/nullfn-out.txt` (266 rows).

### 2.1 Maps — every cell is a bare NPE

Ten map receivers x thirteen cells. **Every single cell below is
`java.lang.NullPointerException msg=<NO-MESSAGE>`**, except the `putAll` row:

| cell | HashMap | LinkedHashMap | TreeMap | Hashtable | Properties | CHM | CSLM | IdentityHashMap | WeakHashMap |
|---|---|---|---|---|---|---|---|---|---|
| `computeIfAbsent(k,null)` present | NPE | NPE | NPE | NPE | NPE | NPE | NPE | NPE | NPE |
| `computeIfAbsent(k,null)` absent | NPE | NPE | NPE | NPE | NPE | NPE | NPE | NPE | NPE |
| `compute(k,null)` present | NPE | NPE | NPE | NPE | NPE | NPE | NPE | NPE | NPE |
| `compute(k,null)` absent | NPE | NPE | NPE | NPE | NPE | NPE | NPE | NPE | NPE |
| `computeIfPresent(k,null)` present | NPE | NPE | NPE | NPE | NPE | NPE | NPE | NPE | NPE |
| `computeIfPresent(k,null)` **absent** | NPE | NPE | NPE | NPE | NPE | NPE | NPE | NPE | NPE |
| `merge(k,v,null)` present | NPE | NPE | NPE | NPE | NPE | NPE | NPE | NPE | NPE |
| `merge(k,v,null)` absent | NPE | NPE | NPE | NPE | NPE | NPE | NPE | NPE | NPE |
| `forEach(null)` non-empty | NPE | NPE | NPE | NPE | NPE | NPE | NPE | NPE | NPE |
| `forEach(null)` **EMPTY** | NPE | NPE | NPE | NPE | NPE | NPE | NPE | NPE | NPE |
| `replaceAll(null)` non-empty | NPE | NPE | NPE | NPE | NPE | NPE | NPE | NPE | NPE |
| `replaceAll(null)` **EMPTY** | NPE | NPE | NPE | NPE | NPE | NPE | NPE | NPE | NPE |

Two rows are bolded because they are the ones a "check when you get there"
implementation fails: the refusal fires on an **empty** receiver and on an
**absent** key. It is `Objects.requireNonNull` at the top of the method, not a
consequence of reaching an element.

`putAll(null)` is the exception, and it carries a helpful-NPE naming a local —
so it must be **transcribed, never composed**. Four distinct strings:

```text
HashMap / LinkedHashMap / Properties / CHM / IdentityHashMap / WeakHashMap:
    Cannot invoke "java.util.Map.size()" because "m" is null
TreeMap:
    Cannot invoke "java.util.Map.size()" because "map" is null
Hashtable:
    Cannot invoke "java.util.Map.entrySet()" because "t" is null
ConcurrentSkipListMap:
    Cannot invoke "java.util.Map.entrySet()" because "m" is null
```

G1-1 §2 measured the first three independently and agrees exactly. The `TreeMap`
and `ConcurrentSkipListMap` strings are new here. Note this is a **four**-way
split across two axes (method name and local name) — a fifth receiver is a
finding, not a confirmation.

### 2.2 Collections — same rule, plus two message families

Eleven collection receivers. `forEach(null)`, `removeIf(null)`,
`removeAll(null)`, `retainAll(null)`, `replaceAll(null)` — all
`NullPointerException msg=<NO-MESSAGE>`, empty receiver included, on
`ArrayList`, `LinkedList`, `Vector`, `Stack`, `HashSet`, `LinkedHashSet`,
`TreeSet`, `ArrayDeque`, `PriorityQueue`, `CopyOnWriteArrayList` and
`ConcurrentLinkedQueue`.

`containsAll(null)` and `addAll(null)` are **not** message-less — they are
helpful-NPEs, and the two do not agree with each other:

```text
containsAll(null), every receiver measured:
    Cannot invoke "java.util.Collection.iterator()" because "c" is null
addAll(null), ArrayList / LinkedList / Vector / Stack:
    Cannot invoke "java.util.Collection.toArray()" because "c" is null
addAll(null), HashSet / LinkedHashSet / TreeSet / ConcurrentLinkedQueue:
    Cannot invoke "java.util.Collection.iterator()" because "c" is null
addAll(null), ArrayDeque:
    Cannot invoke "java.util.Collection.size()" because "c" is null
addAll(null), CopyOnWriteArrayList:
    Cannot invoke "java.util.Collection.getClass()" because "c" is null
```

Four different `addAll` messages across ten receivers. **Not touched by this
lane** — see §6. Recorded so nobody derives one from another.

### 2.3 The three traps — MEASURED, `scratchpad/g8/order-out.txt`

**Trap 1 — `sort(null)` is not an error.** A null `Comparator` means natural
ordering:

```text
ArrayList | sort(null) nonempty | OK -> "[a, b]"
LinkedList             | sort(null) | OK -> "[a, b]"
Vector / Stack         | sort(null) | OK -> "[a, b]"
CopyOnWriteArrayList   | sort(null) | OK -> "[a, b]"
Arrays.asList("b","a") | sort(null) | OK -> [a, b]
subList                | sort(null) | OK -> [a, b]
Collections.sort(l, null)           | OK -> "[a, b]"
Arrays.sort(arr, null)              | OK -> "[a, b]"
```

A blanket "refuse every null functional argument" sweep would have turned nine
working paths into throws. `native_al_sort_comparator` and
`native_collections_sort_comparator` already route a null comparator to natural
ordering and are **correct as they stand**; this lane did not touch them and
added a test that pins them against exactly that future sweep.

**Trap 2 — immutability wins over the null argument, except for `forEach`.**

```text
List.of("a").removeIf(null)       | UnsupportedOperationException <NO-MESSAGE>
List.of("a").replaceAll(null)     | UnsupportedOperationException <NO-MESSAGE>
List.of("a").sort(null)           | UnsupportedOperationException <NO-MESSAGE>
List.of("a").forEach(null)        | NPE <<Cannot invoke
                                    "java.util.function.Consumer.accept(Object)"
                                    because "action" is null>>
unmodifiableList(l).removeIf(null)   | UnsupportedOperationException
unmodifiableList(l).replaceAll(null) | UnsupportedOperationException
unmodifiableList(l).sort(null)       | UnsupportedOperationException
unmodifiableList(l).forEach(null)    | NPE <NO-MESSAGE>
unmodifiableMap(m).replaceAll(null)  | UnsupportedOperationException
unmodifiableMap(m).forEach(null)     | NPE <NO-MESSAGE>
Map.of("a","b").replaceAll(null)     | UnsupportedOperationException
Map.of("a","b").computeIfAbsent(z,null) | UnsupportedOperationException
Map.of("a","b").forEach(null)        | NPE <<Cannot invoke
                                       "java.util.function.BiConsumer.accept(
                                       Object, Object)" because "action" is null>>
Collections.emptyList().removeIf(null)   | NPE <NO-MESSAGE>
Collections.emptyList().forEach(null)    | NPE <NO-MESSAGE>
Arrays.asList("a","b").replaceAll(null)  | NPE <NO-MESSAGE>
```

The rule is **which class overrides the method**, not "immutable receivers throw
UOE": `Collections.emptyList()` and `Arrays.asList` do NOT override `removeIf`/
`replaceAll`, so the `Collection` default's `requireNonNull` runs first and they
give NPE — while `List.of` and `Collections.unmodifiableList` DO override, and
refuse before looking at the argument. Two of these also produce the
**helpful-NPE naming `action`**, and one produces the message-less form for the
same call on a different receiver. Three shapes for "forEach(null)". This lane
touches none of the immutable receivers.

**Trap 3 — the function is not always argument 1.** SOURCE-VERIFIED against the
registration: `native_chm_for_each_parallel` is registered as
`"forEach", "(JLjava/util/function/BiConsumer;)V"`. The `long` occupies argument
1 and the `BiConsumer` is argument **2**. Its sibling `native_chm_for_each` is
`"(Ljava/util/function/BiConsumer;)V"` and its consumer is argument 1. This is
the "wrong twin" shape HANDOFF §5 names; the guard index was taken from the
index the body's own pre-existing extraction used, per site, never assumed.

### 2.4 Ordering — MEASURED, and it decides where the guard goes

When the key **and** the function are both null:

```text
Hashtable | computeIfAbsent(null,null) | NPE <NO-MESSAGE>
Hashtable | computeIfAbsent(null,F)    | NPE <<Cannot invoke "Object.hashCode()"
                                          because "key" is null>>
Hashtable | compute(null,null)         | NPE <NO-MESSAGE>
Hashtable | compute(null,BF)           | NPE <<...because "key" is null>>
Hashtable | merge(null,v,null)         | NPE <NO-MESSAGE>
Hashtable | merge(null,null,BF)        | NPE <<...because "key" is null>>
```

The **function is refused first**, and the difference is observable through the
message. That is G1-1's RULE V / RULE K ordering extended to the function
argument, and it is why every guard added in §4 sits **ahead** of the
receiver's key handling — and, in `native_tm_for_each` / `native_ts_for_each`,
ahead of the state-resync call, because the JDK's `requireNonNull` is the
method's first statement and the resync re-enters a user `Comparator`.

---

## 3. The oracle table — the view families, MEASURED

`scratchpad/g8/views-out.txt`, 541 rows. Six map receivers x
`values()`/`keySet()`/`entrySet()`.

### 3.1 Class identity and the `instanceof` facts

| receiver | `values()` | `keySet()` | `entrySet()` |
|---|---|---|---|
| `HashMap` | `HashMap$Values` | `HashMap$KeySet` | `HashMap$EntrySet` |
| `LinkedHashMap` | `LinkedHashMap$LinkedValues` | `LinkedHashMap$LinkedKeySet` | `LinkedHashMap$LinkedEntrySet` |
| `TreeMap` | `TreeMap$Values` | `TreeMap$KeySet` | `TreeMap$EntrySet` |
| `Hashtable` | `Collections$SynchronizedCollection` | `Collections$SynchronizedSet` | `Collections$SynchronizedSet` |
| `Properties` | `Collections$SynchronizedCollection` | `Collections$SynchronizedSet` | `Collections$SynchronizedSet` |
| `ConcurrentHashMap` | `CHM$ValuesView` | `CHM$KeySetView` | `CHM$EntrySetView` |

| receiver.view | `List` | `Set` | `Collection` | `Serializable` | `RandomAccess` | `SortedSet` |
|---|---|---|---|---|---|---|
| `HashMap.values` | false | false | true | **false** | false | false |
| `HashMap.keySet` | false | true | true | **false** | false | false |
| `HashMap.entrySet` | false | true | true | **false** | false | false |
| `LinkedHashMap.*` | false | v:false k/e:true | true | **false** | false | false |
| `TreeMap.values` | false | false | true | **false** | false | false |
| `TreeMap.keySet` | false | true | true | **false** | false | **true** |
| `TreeMap.entrySet` | false | true | true | **false** | false | false |
| `Hashtable.*` / `Properties.*` | false | v:false k/e:true | true | **true** | false | false |
| `CHM.*` | false | v:false k/e:true | true | **true** | false | false |

**Nothing in any family is a `List`, and nothing is `RandomAccess`.** That is
`C7-1`'s finding, measured across all eighteen cells rather than one.

Declared interfaces, which is where the families genuinely differ:

```text
HashMap$Values          : (none)      <- inherits AbstractCollection{Collection}
LinkedHashMap$LinkedValues : SequencedCollection
LinkedHashMap$LinkedKeySet : SequencedSet
LinkedHashMap$LinkedEntrySet : SequencedSet
TreeMap$KeySet          : NavigableSet
CHM$ValuesView          : Collection, Serializable
CHM$KeySetView          : Set, Serializable
Collections$SynchronizedCollection : Collection, Serializable
```

### 3.2 Behaviour — where the families agree, and the four places they do not

Uniform across all eighteen cells: `size`, `toString`, liveness (a `put` after
the view is taken is visible: 3 -> 4), and **write-through on every mutator** —
`remove`, `removeAll`, `retainAll`, `removeIf`, `clear`, `iterator().remove()`
all reach the source map. `forEach(null)` / `removeIf(null)` / `removeAll(null)`
are bare NPEs on every view, empty map included. `stream()` reuse throws
`IllegalStateException("stream has already been operated upon or closed")` on
every one of the eighteen — the exact string `W7-65` implemented.

Four divergences, none derivable from the others:

1. **`Properties`' views are not identity-cached.** `m.values() == m.values()` is
   `true` on `HashMap`, `LinkedHashMap`, `TreeMap`, `Hashtable` and `CHM`, and
   **`false`** on all three `Properties` views. `Hashtable` caches its
   `SynchronizedCollection` wrapper in a field; `Properties` re-wraps its side
   map on every call. Same class, opposite identity contract — `C13-2`'s
   "`values()` is cached on the map in every family" is **falsified for
   `Properties`**.
2. **`ConcurrentHashMap.entrySet().add(e)` is supported.** It answers
   `ret=false mapSize=3` (the entry was already present) where every other cell
   in the table throws `UnsupportedOperationException`. `CHM.keySet().add` and
   `CHM.values().add` still throw. A blanket "a view refuses `add`" rule —
   which is what `W7-36` implemented for `keySet.add`/`values.add` — is wrong
   for exactly this one cell.
3. **`Hashtable`'s iterator carries a message.** `iterator().remove()` before
   `next()` is `IllegalStateException msg=<NO-MESSAGE>` on every family except
   `Hashtable` and `Properties`, where it is
   `IllegalStateException msg=<<Hashtable Enumerator>>`. Transcribe; it cannot
   be composed.
4. **`ConcurrentModificationException` is not universal.** Mutating the map
   while iterating a view throws CME on `HashMap`, `LinkedHashMap`, `TreeMap`
   and `Hashtable`, and **does not throw** on `Properties` or `CHM`. `W7-1`'s
   open question about CME blast radius has a smaller denominator than it
   assumed: two of the six families must NOT get one.

### 3.3 The `Serializable` divergence `C13-3` flagged, now measured on one side

`C13-3` records that CratonVM's `keySet()` returns a real `java/util/HashSet`,
which **is** `Serializable`, and flags the divergence as unmeasured. The oracle
half is now measured: `HashMap$KeySet`, `LinkedHashMap$LinkedKeySet` and
`TreeMap$KeySet` are **not** `Serializable`; `Hashtable`/`Properties`/`CHM`
key sets **are**. So the divergence is real for three of six receivers and
absent for the other three — a blanket "our keySet is wrongly Serializable"
statement would be half wrong. The CratonVM half remains unmeasured: I cannot
run the VM. `C13-3` N2 is a probe, not a source change, and stays open.

---

## 4. What was changed — PREDICTED effect, all of it

All edits are in `native-collections/src/lib.rs`. **No other file was touched.**
Nothing here has been compiled or run. Zero registrations were added, removed or
repointed — this lane changes **bodies only**, so the census is unaffected.

### 4.1 One helper

`reject_null_functional(arg: Option<&Value>) -> Result<(), MethodCallFailed>`,
placed next to G1-1's `bare_npe` / `ht_reject_null_key` / `ht_reject_null_value`
and reusing `bare_npe()` rather than adding a parallel error constructor. Its
doc comment carries §2's measurement, including all three traps, so the next
lane meets the `sort(null)` and immutable-receiver rows before it reaches for a
blanket rule.

### 4.2 The 22 bodies

Each guard was inserted individually with the enclosing `fn` signature as the
edit anchor — no scripted edit — and every insertion was then re-derived from
the file and mapped back to its enclosing function to confirm which class it
belongs to (HANDOFF §5's wrong-twin trap). The argument index is the one that
body's own pre-existing extraction used.

| body | arg | receiver family |
|---|---|---|
| `native_map_for_each` | 1 | HashMap, `java/util/Map` door, Properties |
| `native_map_replace_all` | 1 | HashMap |
| `native_map_compute_if_absent` | 2 | HashMap |
| `native_map_compute` | 2 | HashMap |
| `native_map_compute_if_present` | 2 | HashMap |
| `native_lhm_compute_if_absent` | 2 | LinkedHashMap |
| `native_lhm_for_each` | 1 | LinkedHashMap |
| `native_tm_for_each` | 1 | TreeMap |
| `native_tm_compute_if_absent` | 2 | TreeMap |
| `native_tm_merge` | 3 | TreeMap |
| `native_al_for_each` | 1 | ArrayList, Vector, `Collection`/`List` doors |
| `native_al_remove_if` | 1 | ArrayList, Vector |
| `native_al_replace_all` | 1 | ArrayList |
| `native_ll_remove_if` | 1 | LinkedList |
| `native_hs_for_each` | 1 | HashSet, `java/util/Set` door |
| `native_ts_for_each` | 1 | TreeSet |
| `native_ad_for_each` | 1 | ArrayDeque |
| `native_ksv_for_each` | 1 | CHM$KeySetView |
| `native_ksv_remove_if` | 1 | CHM$KeySetView |
| `native_chm_for_each` | 1 | ConcurrentHashMap |
| `native_chm_replace_all` | 1 | ConcurrentHashMap |
| `native_chm_for_each_parallel` | **2** | ConcurrentHashMap |

Four placement decisions that are semantic, not stylistic:

* `native_al_for_each` and `native_hs_for_each` check **before** `ksv_route` /
  `vc_route`, because the answer is the same for a view receiver and rebuilding
  a carrier to reach it is pure cost. `native_ksv_for_each` keeps its own check
  because it is also entered directly by its own registration; the duplicate is
  idempotent.
* `native_tm_for_each` and `native_ts_for_each` check **before**
  `tm_sync_native_state` / `resync_ts_view`, per §2.4.
* `native_chm_replace_all` previously handed the null function to
  `native_map_replace_all` once per segment, each of which returned its own
  silent no-op — so an empty and a populated CHM both answered "done".
* `native_map_merge` was **already correct** (it refuses a null value and a null
  function, message-less) and was not touched. `native_tm_merge` got the
  function arm only, **not** the value arm: `Hashtable.merge(k, null, f)`
  *succeeds* on the oracle (G1-1 §5), so the value rule is per-receiver and this
  lane does not widen it from one row.

### 4.3 Tests

Four tests in the existing `#[cfg(test)] mod tests`:

* `reject_null_functional_separates_a_missing_arg_from_an_explicit_null` — the
  helper's truth table with no VM in the way, pinning the missing-vs-null
  distinction that a "just check for null" rewrite loses.
* `every_null_function_body_refuses_an_explicit_null` — all 22 bodies, each with
  its own argument vector, asserting the **message-less** NPE. It asserts
  `cases.len() == 22` so that a body added to the axis without a row here is a
  compile-time-visible omission rather than a silent regression.
* `a_missing_functional_argument_is_not_reported_as_a_java_npe` — the over-set
  direction.
* `a_null_comparator_means_natural_ordering_and_is_never_refused` — Trap 1,
  pinned against the future blanket sweep.

---

## 5. Which body actually wins — SOURCE-VERIFIED, and it is not all of them

HANDOFF §5's failure mode #1 is "a fix landed in a body with `invocations=0`".
I could not run `--dump-native-registry`, so I did the next best thing: grepped
the whole repo for every `(class, name, descriptor)` triple this lane's bodies
serve, and read the registrar ordering in `vm/src/vm/vm_init.rs`.

**Three other registrars compete for four of the triples.**

`vm/src/vm/vm_init.rs` carries an explicit block comment reading
`LAST-WRITE-WINS BOUNDARY — do not reorder`, and states that everything after it
"deliberately re-registers implementations that `register_collections_natives`
is known to clobber". `properties_sidetable` is in that list, in **all three**
VM-init arms (after the collections calls at lines 1936, 2315 and 2880).

| triple | competing registrar | who wins | consequence |
|---|---|---|---|
| `java/util/Properties.forEach(BiConsumer)V` | `properties_sidetable.rs:3815` -> `native_properties_for_each` | **properties_sidetable** (re-registered after collections in all 3 arms) | my edit is dead here — but that body is **already correct** (it refuses a null action with the message-less `props_null_put_npe()`, which matches the oracle) |
| `java/util/Properties.computeIfAbsent(...)` | `properties_sidetable.rs:3867` -> `native_properties_compute_if_absent` | **properties_sidetable** | my edit is dead here, and that body **has the defect** — it checks argument 1 (the key) and never checks argument 2 (the function). **NOMINATION 1.** |
| `j/u/c/ConcurrentHashMap.forEach(BiConsumer)V` | `util_concurrent_ext.rs:2077` (closure) | **ARM-DEPENDENT** — `register_concurrent_natives` is at 2076 and 2662; collections at 1936, 2315, 2880 | unresolved by reading. **NOMINATION 2** + §7 item 1 |
| `j/u/c/ConcurrentHashMap.computeIfAbsent(...)` | `util_concurrent_ext.rs:1891` (closure) | **ARM-DEPENDENT**, same | unresolved by reading. **NOMINATION 2** + §7 item 1 |

The remaining 18 bodies have no competing registrar anywhere in the repo; their
only registrations are the ones in `native-collections/src/lib.rs` listed in
§4.2, and no `.rs` file outside this crate calls any of them (the three
cross-file hits for `native_map_merge` and `native_al_for_each` are all
comments).

This is the finding I would most want the orchestrator to act on: **a lane can
fix a body correctly, verify it compiles, and still change nothing**, and here
that is provably the case for at least two of the 22 and possibly four.

---

## 6. What this lane did NOT do

Everything below is MEASURED above and deliberately untouched.

* **`addAll(null)` and `containsAll(null)`'s four helpful-NPE messages** (§2.2).
  These are transcribable, but they are a different axis (a null *collection*
  argument, not a null *function*), they touch `native_al_add_all` /
  `native_hs_add_all` and their siblings which are on `RJdkBridge1`'s and
  `RJdkIntrinsics3`'s live paths, and G1-1 already widened `putAll` this wave.
  Two widenings of the bulk-argument axis in one wave with no binary between
  them is exactly what HANDOFF §5's first trap is about. **Recommended as the
  next standalone follow-up**; the oracle answer is in §2.2 in full.
* **The immutable receivers** (`Collections.unmodifiable*`, `List.of`, `Map.of`,
  `Collections.emptyList`, `Arrays.asList`, `subList`). Trap 2 shows the rule is
  per-overriding-class, not per-receiver-kind, and `native_unmod_*` mostly
  delegates to real JDK bodies that would inherit the right behaviour anyway. A
  guard added there could turn a correct `UnsupportedOperationException` into an
  NPE.
* **`native_hibernate_persistent_map_for_each`.** Same fabricated-success shape,
  but its receiver is a Hibernate class with no JDK contract to measure against.
  Left alone rather than guessed at.
* **`native_stream_for_each` and the three primitive-stream `forEach` bodies.**
  Same shape, not measured by this lane's probes, and streams are `W7-65`'s
  surface with a named residual set. Recorded, not touched.
* **`is_values_view_class`'s five-name recognition set.** See §0 — measurement
  stopped a change that reading would have made.
* **`W7-1`'s `sort`/`replaceAll` modCount item.** `W7-1` states an explicit
  ordering constraint: take it *after* the first CME measurement, not with it.
  §3.2 divergence 4 shows why that constraint was right — two of the six
  families must not throw CME at all — but the measurement it asks for is on
  CratonVM, which I cannot run. Left open, deliberately, with the denominator
  now smaller.
* **Anything requiring a registration change**, including all of `C13-2` N2 and
  `C13-3` N1, which are gated on a `vm/**` prerequisite (§7).

---

## 7. What the orchestrator must check at build time

1. **`--dump-native-registry`, and read `owns_slot` + `invocations` for four
   triples specifically** (§5): `java/util/Properties.forEach` /
   `.computeIfAbsent`, and `java/util/concurrent/ConcurrentHashMap.forEach` /
   `.computeIfAbsent`. If `registered_by` is `util_concurrent_ext` or
   `properties_sidetable` for any of them, the corresponding edit in this lane
   is inert and NOMINATION 1 / 2 is the real fix. Flags must precede the main
   class or they are ignored silently, with exit 0 and no file.
2. **Compilation.** One new symbol in `native-collections/src/lib.rs`:
   `reject_null_functional`. Repo-wide grep confirms exactly one definition and
   no name collision. `is_bare_npe` is a new test-module helper, also unique.
   Zero top-level duplicate `fn` names in the file. It takes
   `Option<&Value>` and returns `Result<(), MethodCallFailed>`, called with `?`
   from `MethodCallResult` bodies — the same conversion every existing
   `ht_reject_null_*` call site relies on.
3. **The behaviour changes that can flip a currently-green assertion.** Every
   one of the 22 turns a silent success into a throw. Anything in the corpus or
   in `native-*` that calls one of these methods with a null function currently
   gets away with it and will now raise. `RJdkCollections`, `RJdkViews`,
   `RJdkMapViews` and `RCollections` are the fixtures most likely to surface it.
   The two highest-risk bodies are `native_al_for_each` and `native_map_for_each`,
   because both are also registered on **interface doors**
   (`java/util/Collection`, `java/util/List`, `java/util/Set`, `java/util/Map`)
   and so serve receivers far beyond the classes named in §4.2.
4. **Formatting.** `rustfmt --edition 2021 --check` on
   `native-collections/src/lib.rs` reports **89** diffs — but `git show
   HEAD:native-collections/src/lib.rs` reports **90** under the same rustfmt, so
   the diffs are pre-existing (a rustfmt version skew, not this wave), and this
   lane's edits contribute **zero**. Verified by copying the crate's whole `src`
   tree — copying `lib.rs` alone makes rustfmt abort on the unresolvable
   `mod identity_hash` and report success while writing nothing, which is its own
   small instance of "a green check that proves nothing".
5. **Zero CR bytes** (`tr -cd '\r' < f | wc -c` = 0) and zero conflict markers,
   re-verified after the last edit.

---

## 8. The records this lane touched — can they close?

| record | verdict | evidence |
|---|---|---|
| **`G1-1` §5 residual** | **CLOSABLE IN SOURCE** once a binary agrees | The axis it named is fixed, and at 22 sites rather than 6. It cannot be called closed until §7 item 1 confirms the bodies own their slots, and until a differential runs. |
| **`C7-1`** | **CANNOT CLOSE** | Its open half is class identity, which needs the allocation flip. §3.1 now gives the full eighteen-cell oracle its "HotSpot transcript" section wanted, and confirms its banner: nothing is a `List`, nothing is `RandomAccess`. The rewrite is still net **+17** registrations. |
| **`C13-1`** | **CANNOT CLOSE — and the question it asked is now answered** | The layer is **still inert**. SOURCE-VERIFIED: `vc_route` gates on `CF_VALUES_VIEW`, set only by exact match on five class names in `classify_class`; `values()` is natively registered on all six map classes (`lib.rs` :10062, :31762, :37571, :48376, :49414, :53412, :53514) and each returns an `ArrayList` carrier, so none of the five is ever constructed. The orchestrator's `carrier`-argument merge fix repaired a compile error in a body that still has zero reachable callers. It becomes live only across `C13-2` N1's JIT/warm-path cliff, which is in `vm/**`. |
| **`C13-2`** | **CANNOT CLOSE**, and its arithmetic needs one correction | Still gated on N1 in `vm/**`. Two MEASURED corrections: (a) `values()` is **not** cached on the map in every family — `Properties` mints a fresh wrapper per call (§3.2); (b) the `Hashtable`/`Properties` member of the "five" is reached by callers only as a `Collections$SynchronizedCollection` wrapper, so registrations placed on `Hashtable$ValueCollection` would sit on a class no caller ever holds. Both change what the 25 rows should be. |
| **`C13-3`** | **CANNOT CLOSE**; half of N2 is now done | The oracle half of the `Serializable` divergence is MEASURED (§3.3): real for `HashMap`/`LinkedHashMap`/`TreeMap` keySets, absent for `Hashtable`/`Properties`/`CHM`. The CratonVM half needs a binary. N1 is still sequenced after `C13-2` N2. |
| **`C7-3`** | **CANNOT CLOSE** | Untouched by this lane. Its fixture is green on HotSpot only; that is unchanged. |
| **`W7-1`** | **CANNOT CLOSE** | Its deliberately-open `sort`/`replaceAll` modCount item stays open, per its own ordering constraint. §3.2 divergence 4 supplies part of the measurement it asked for: CME must fire on four families and must **not** fire on `Properties` or `CHM`. |
| **`W7-33` / `W7-36`** | **CANNOT CLOSE** | Still "changed in source, not rebuilt". §3.2 divergence 2 is a **correction to `W7-36`**: its blanket `keySet.add`/`values.add` refusal is right for five receivers and **wrong for `CHM.entrySet().add`**, which the oracle supports. That row should be re-checked before `W7-36` is called finished. |
| **`W7-96`** | **CANNOT CLOSE** | Untouched. Its joint gate (`RJdkEnumerations` + a rebuild) is unchanged. |
| **`W7-65`** | **CORROBORATED, still cannot close** | §3.2 measured the exact string `stream has already been operated upon or closed` on all eighteen view streams, matching `STREAM_LINKED_MSG`. Its residual set (primitive streams, short layouts) is untouched and still needs a binary. |

---

## 9. NOMINATIONS

Everything outside `native-collections/src/lib.rs`.

**NOMINATION 1 — `native-builtins/src/properties_sidetable.rs:2098`,
`native_properties_compute_if_absent`.**
It checks argument 1 (the key) and never checks argument 2 (the function), so
`Properties.computeIfAbsent(k, null)` falls through to
`_ => return Ok(Some(Value::Object(None)))` and answers `null`. MEASURED oracle:
bare `NullPointerException`, `<NO-MESSAGE>`, whether the key is present or
absent. This body **wins the slot over `native_map_compute_if_absent`** (§5), so
this is the only place the fix can land. Change: add, immediately after the
existing key check,

```rust
    if matches!(args.get(2), Some(Value::Object(None))) {
        return Err(props_null_put_npe());
    }
```

`props_null_put_npe()` is already message-less and already the right shape.
Note its sibling `native_properties_for_each` (:3078) is **already correct** and
needs nothing.

**NOMINATION 2 — `native-builtins/src/util_concurrent_ext.rs:1891` and `:2077`,
both inside `register_concurrent_natives`.**
Both closures carry the identical fabricated-success shape:

```rust
// :1891  ConcurrentHashMap.computeIfAbsent(Object, Function)
let func = match args.get(2) {
    Some(Value::Object(Some(f))) => *f,
    _ => return Ok(Some(Value::Object(None))),   // explicit null -> "null"
};
// :2077  ConcurrentHashMap.forEach(BiConsumer)
let action = match args.get(1) {
    Some(Value::Object(Some(a))) => *a,
    _ => return Ok(None),                        // explicit null -> "done"
};
```

MEASURED oracle for both: bare `NullPointerException`, `<NO-MESSAGE>`, empty map
included. Change: guard the explicitly-passed null ahead of each extraction,
matching G1-1's `bare_npe()` convention —
`if matches!(args.get(N), Some(Value::Object(None))) { return Err(...NullPointerException { message: None }.into()); }`
with `N` = 2 and 1 respectively. **Whether this is needed at all depends on
§7 item 1**: if `register_collections_natives` wins in the shipping arms, the
already-fixed bodies in `native-collections` serve these triples and these two
closures are dead code — which is itself worth knowing, and would make the
right change *deleting* them rather than fixing them.

**NOMINATION 3 — `vm/src/vm/vm_init.rs`, documentation only, no behaviour
change.** The `LAST-WRITE-WINS BOUNDARY` comment block lists the registrars it
deliberately orders after `register_collections_natives`, but
`register_concurrent_natives` is ordered *differently in different arms* (2076
vs 2662 against collections at 1936 / 2315 / 2880) and is not mentioned in the
boundary comment at all. A one-line note recording which arm intends which
winner would have saved this lane the ambiguity in §5. I did not edit it.

---

## 10. Regression vector

`RJdkViews`, `RJdkMapViews`, `RJdkCollections`, `RCollections` and
`RChmKeySetView` already exist and are scheduled; **no file was created under
`regression-suite/`.** The vector below is the one this lane would add, as a new
section in `RJdkCollections` — every check paired with the case that must NOT
change, per `W7-36` §6.2.

```java
    // G8-1: the null-function axis. MEASURED on HotSpot 25.0.3+9-LTS.
    // Every row is a BARE NullPointerException -- getMessage() must be null.
    // Paired throughout with the case that must NOT throw, because the
    // dangerous direction of this fix is over-refusal.
    static void nullFunctionAxis() {
        // ---- the refusals: empty receiver included, absent key included ----
        checkBareNpe("HashMap.forEach(null)",        () -> new HashMap<>().forEach(null));
        checkBareNpe("HashMap.replaceAll(null)",     () -> new HashMap<>().replaceAll(null));
        checkBareNpe("HashMap.computeIfAbsent",      () -> new HashMap<>().computeIfAbsent("k", null));
        checkBareNpe("HashMap.compute",              () -> new HashMap<>().compute("k", null));
        checkBareNpe("HashMap.computeIfPresent/abs", () -> new HashMap<>().computeIfPresent("zz", null));
        checkBareNpe("TreeMap.forEach(null)",        () -> new TreeMap<>().forEach(null));
        checkBareNpe("TreeMap.merge(k,v,null)",      () -> new TreeMap<>().merge("k", "v", null));
        checkBareNpe("LinkedHashMap.forEach(null)",  () -> new LinkedHashMap<>().forEach(null));
        checkBareNpe("CHM.forEach(null)",            () -> new ConcurrentHashMap<>().forEach(null));
        checkBareNpe("CHM.replaceAll(null)",         () -> new ConcurrentHashMap<>().replaceAll(null));
        checkBareNpe("Properties.computeIfAbsent",   () -> new Properties().computeIfAbsent("k", null));
        checkBareNpe("ArrayList.forEach(null)",      () -> new ArrayList<>().forEach(null));
        checkBareNpe("ArrayList.removeIf(null)",     () -> new ArrayList<>().removeIf(null));
        checkBareNpe("ArrayList.replaceAll(null)",   () -> new ArrayList<>().replaceAll(null));
        checkBareNpe("Vector.removeIf(null)",        () -> new Vector<>().removeIf(null));
        checkBareNpe("LinkedList.removeIf(null)",    () -> new LinkedList<>().removeIf(null));
        checkBareNpe("HashSet.forEach(null)",        () -> new HashSet<>().forEach(null));
        checkBareNpe("TreeSet.forEach(null)",        () -> new TreeSet<>().forEach(null));
        checkBareNpe("ArrayDeque.forEach(null)",     () -> new ArrayDeque<>().forEach(null));
        // views, all three, on a NON-empty map
        Map<String,String> m = new HashMap<>(Map.of("a","1"));
        checkBareNpe("values().forEach(null)",       () -> m.values().forEach(null));
        checkBareNpe("keySet().removeIf(null)",      () -> m.keySet().removeIf(null));
        checkBareNpe("entrySet().forEach(null)",     () -> m.entrySet().forEach(null));

        // ---- the paired NON-refusals: over-refusal is the dangerous side ----
        // Trap 1: a null Comparator MEANS natural ordering. These must SORT.
        List<String> l1 = new ArrayList<>(List.of("b", "a"));  l1.sort(null);
        check("ArrayList.sort(null) sorts", l1.toString().equals("[a, b]"));
        List<String> l2 = new Vector<>(List.of("b", "a"));     l2.sort(null);
        check("Vector.sort(null) sorts", l2.toString().equals("[a, b]"));
        List<String> l3 = new LinkedList<>(List.of("b", "a")); l3.sort(null);
        check("LinkedList.sort(null) sorts", l3.toString().equals("[a, b]"));
        List<String> l4 = new ArrayList<>(List.of("b", "a"));
        Collections.sort(l4, null);
        check("Collections.sort(l,null) sorts", l4.toString().equals("[a, b]"));

        // Trap 2: an immutable receiver refuses FIRST, with a different type.
        checkThrows("List.of().removeIf(null) is UOE",
                UnsupportedOperationException.class, () -> List.of("a").removeIf(null));
        checkThrows("List.of().sort(null) is UOE",
                UnsupportedOperationException.class, () -> List.of("a").sort(null));
        checkThrows("unmodMap.replaceAll(null) is UOE",
                UnsupportedOperationException.class,
                () -> Collections.unmodifiableMap(new HashMap<>(Map.of("a","b"))).replaceAll(null));
        // ...but forEach on the same receiver is still an NPE, not a UOE.
        checkThrows("unmodList.forEach(null) is NPE",
                NullPointerException.class,
                () -> Collections.unmodifiableList(new ArrayList<>(List.of("a"))).forEach(null));

        // Trap 3: CHM.forEach's PARALLEL overload -- the function is arg 2.
        checkBareNpe("CHM.forEach(long,null)",
                () -> new ConcurrentHashMap<String,String>().forEach(1L, null));
        // ...and the same overload with a real action must still run.
        ConcurrentHashMap<String,String> chm = new ConcurrentHashMap<>(Map.of("a","1"));
        int[] seen = {0};
        chm.forEach(1L, (k, v) -> seen[0]++);
        check("CHM.forEach(long,action) still runs", seen[0] == 1);
    }

    // getMessage() must be null, not "" and not a helpful-NPE. A message here
    // is as wrong as no throw: it means an invented string reached the caller.
    static void checkBareNpe(String label, Runnable r) {
        try {
            r.run();
            check(label + " [must throw]", false);
        } catch (NullPointerException e) {
            check(label + " [bare, getMessage()==null]", e.getMessage() == null);
        } catch (Throwable t) {
            check(label + " [NPE, got " + t.getClass().getName() + "]", false);
        }
    }

    static void checkThrows(String label, Class<? extends Throwable> want, Runnable r) {
        try {
            r.run();
            check(label + " [must throw]", false);
        } catch (Throwable t) {
            check(label, want.isInstance(t));
        }
    }
```

A second vector belongs in `RJdkViews` for §3.2's four divergences —
`Properties` views not identity-cached, `CHM.entrySet().add` supported,
`Hashtable`'s `"Hashtable Enumerator"` message, and CME on four families but not
on `Properties`/`CHM`. I have deliberately not written it: three of those four
are assertions about behaviour this lane did not change, and per `W7-36`,
"**a green suite run is not evidence**" for a row nobody has measured on
CratonVM. They should be added by the lane that can run the differential.

---

## 11. The probes

Reproduced in the working tree at `scratchpad/g8/NullFn.java`,
`scratchpad/g8/Views.java` and `scratchpad/g8/Order.java`, with their raw output
alongside. All three were run as `"$JAVA_HOME/bin/java" <File>.java` (single-file
source mode, no `javac`), `JAVA_HOME=C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot`,
and every printed label is ASCII.

`Views.java` is the one worth re-running as a differential first: it emits the
exact class name, the seven `instanceof` answers, and the outcome of eleven
mutating calls for each of the eighteen view cells, so one run against CratonVM
turns most of §3 from PREDICTED into MEASURED and settles `C13-3` N2 outright.
