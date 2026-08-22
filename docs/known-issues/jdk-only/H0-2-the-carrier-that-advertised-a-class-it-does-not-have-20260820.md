# H0-2 — the carrier that advertises a class it does not have, and the 219-check vector that asked one of twelve

**Status: OPEN — MEASURED, no source change.** Both columns below are real runs
on this host, 2026-08-20: HotSpot 25.0.3+9 at
`C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot`, and
`C:/craton/target-jdkonly-h2/release/cratonvm.exe` built from this branch. The
mechanism in §3 is read from the tree, not inferred from the numbers.

> ## CORRECTION, same day, measured — the sentence that was here was wrong
>
> The original text read: *"the `--jdk-only` arm is 104/104 with all twelve
> cells below still wrong, because strict mode does not take this path."*
> **The second half of that was never measured.** §4's probe was run in the
> default mode only, and the claim about strict mode was inferred from the arm
> being green rather than from asking it. Lane H4 pointed out that it is also
> self-contradictory: `RImmutableFactoryTypes` is in `CORE_CLASSES`
> (`run.sh:177`) and therefore *in* the strict arm, so a green strict arm and a
> wrong `AbstractMap` cell cannot both be true.
>
> **Re-run under `--jdk-only`, 22 receivers × 9 type tests, diffed whole
> against the HotSpot column: IDENTICAL. All twelve cells are already right in
> strict mode.**
>
> That inverts this record's conclusion. Strict mode refuses the
> `cratonvm/internal/Unmodifiable*` producers outright, so `Map.of(...)` there
> IS a real `ImmutableCollections$Map1`, `is_subclass_of` walks a real chain,
> and there is nothing to fix. The defect is **compatible-mode only, on both
> faces**, and §5's framing of this record as "a specification for the wave-2
> cluster retag" does not survive: a retag cannot improve strict mode (already
> exact) and, per `H4-1` §3, cannot improve compatible mode either, because
> `allowed_in(Compatible)` is unconditionally true and the kind is discarded
> there. **The twelve cells are not acceptance criteria for a retag. They are a
> compatible-mode defect whose fix has to be something else.**
>
> The lesson is this directory's oldest one and I walked into it while writing a
> record about somebody else walking into it: *a green gate is evidence about
> the question it asked.* I had the probe, the binary and the flag in hand, and
> inferred instead of spending one command. See `HANDOFF-20260819.md` §3.

**This is a `--real-jdk` (compatible-mode) defect. It does not move strict
mode** — measured: under `--jdk-only` every cell in §4 already matches HotSpot.
Per `HANDOFF-20260819.md` §1, do not read anything here as `--jdk-only`
progress.

Lane H0 (orchestrator), 2026-08-20.

---

## 1. What started it

`RImmutableFactoryTypes` is the only `SUITE=core` failure and one of the five
standing `SUITE=all` failures. Its whole complaint is one line out of 219
checks:

```text
DIVERGENCE  Map.of(k,v) must be instanceof AbstractMap (got false);
            getClass()=java.util.ImmutableCollections$Map1
CK RImmutableFactoryTypes checks=219
```

Two records already explain this. **Both explanations are wrong about the
mechanism**, and one is wrong about the receiver.

* `W8-C4-2` §4's predicted table says the `--real-jdk` face is *"the
  `cratonvm/internal/UnmodifiableMap` stamp's missing `AbstractMap`
  superclass"* and points at P4A N1b's three candidate designs.
* `vm/src/runtime/interpreter/typecheck.rs:1530`'s in-tree comment says the same:
  *"Under `--real-jdk` the VM stamps a `cratonvm/internal/UnmodifiableMap`,
  which the `java/util/` prefix below already excludes."*

Both read as "the carrier is the wrong class, give it the right superclass".
That remedy cannot work, and §3 says why.

## 2. The measurement that names the mechanism

`Map.of("k","v")`, one receiver, four doors:

| door | CratonVM | HotSpot |
|---|---|---|
| `getClass().getName()` | `java.util.ImmutableCollections$Map1` | same |
| `getClass().getSuperclass().getSuperclass()` | `java.util.AbstractMap` | same |
| `AbstractMap.class.isInstance(m)` | **true** | true |
| `m instanceof AbstractMap` (the opcode) | **false** | true |

and the class identity is *not* split — `viaChain == AbstractMap.class ==
Class.forName("java.util.AbstractMap")` is `true` on both VMs, same
`identityHashCode` across all three. `new HashMap() instanceof AbstractMap` is
`true` on CratonVM, so the opcode is not broadly broken.

So: **the reflective door and the bytecode door disagree about one receiver**,
and the reflective one is right. That is the
*reflective-native-and-its-opcode-are-twins-that-drift* shape, and here the
twins do not merely drift — they read different state.

## 3. The mechanism, from the tree

`Map.of(...)` and `Collections.unmodifiableMap(...)` allocate **the same class**:

```rust
// native-collections/src/lib.rs
const UNMOD_MAP_CLASS: &str = "cratonvm/internal/UnmodifiableMap";

fn alloc_immutable_wrapper(ctx, class_name, backing) -> Result<ObjectRef, _> {
    let wrapper = alloc_unmod_wrapper(ctx, class_name, backing)?;
    ctx.set_field(wrapper, UNMOD_FIELD_IMMUTABLE, Value::Int(1));   // <-- the ONLY difference
    Ok(wrapper)
}
```

and `getClass()` tells them apart by reading that marker field:

```rust
// native-builtins/src/lib.rs :: GetClassDisplay::CollMap
let name = if getclass_immutable_marker(ctx, this) {
    if getclass_collection_size(ctx, this) == 1 { ".../ImmutableCollections$Map1" }
    else                                        { ".../ImmutableCollections$MapN" }
} else { "java/util/Collections$UnmodifiableMap" };
```

`INSTANCEOF` does not read that marker. It reads
`shared.mem.heap.class_id_of(obj_ref)` and walks
`ClassManager::is_subclass_of`, which sees `cratonvm/internal/UnmodifiableMap`
— superclass `Object`.

**Therefore the published remedy cannot work.** Giving the carrier
`java/util/AbstractMap` as its superclass fixes `Map.of(...)` and
simultaneously *breaks* `Collections.unmodifiableMap(...)`, for which HotSpot
answers **false** (`Collections$UnmodifiableMap extends Object`). One class,
two required answers, discriminated by a field. A static superclass link is the
wrong shape of fix, and that is why five months of "give the stamp
`AbstractMap`" has never been done.

## 4. The vector asks one question out of twelve

The single divergence is not the size of the defect; it is the size of the
question the vector asks. Probe:
`scratchpad/probe/AbstractChain.java` — 22 receivers × 9 type tests, diffed
whole against the HotSpot column.

**Twelve divergent cells, all the same mechanism, one of them reported:**

| receivers | test | HotSpot | CratonVM |
|---|---|---|---|
| `Map.of()`, `Map.of(1)`, `Map.of(3)`, `Map.copyOf` | `instanceof AbstractMap` | true | **false** |
| `List.of()`, `List.of(1)`, `List.of(3)`, `List.copyOf` | `instanceof AbstractCollection` | true | **false** |
| `Set.of()`, `Set.of(1)`, `Set.of(3)` | `instanceof AbstractCollection` | true | **false** |
| `List.of()`, `List.of(1)`, `List.of(3)`, `List.copyOf`, `Collections.unmodifiableList` | `instanceof RandomAccess` | true | **false** |

Every other cell — `Collection`, `List`, `Set`, `Map`, `AbstractList`,
`AbstractSet`, and every `getClass()` name including
`Collections$UnmodifiableRandomAccessList` — matches HotSpot exactly.

### The `RandomAccess` row is the one that costs something

The other rows are answers to questions applications rarely ask. `RandomAccess`
is a **dispatch switch inside `java.util.Collections` itself**:
`binarySearch`, `reverse`, `shuffle`, `fill`, `copy` and `swap` all branch on
`list instanceof RandomAccess` and fall back to an iterator/`ListIterator`
algorithm when it is false. `Collections.binarySearch` on a `List.of(...)`
therefore runs the **linear** ListIterator walk instead of the indexed one, and
`Collections.reverse` the O(n²) one.

Note the shape: CratonVM's `getClass()` reports
`java.util.Collections$UnmodifiableRandomAccessList` — the class whose *name is
the assertion* — while the opcode denies the interface that name promises.
**A cosmetic-looking `getClass()` alias is answering a question with a
performance contract attached.**

## 5. Why I did not fix it here

The obvious patch is to add an object-aware arm to `INSTANCEOF` — it already
calls `proxy_instance_satisfies_target(shared, obj_ref, …)`, so the plumbing to
read the marker exists. **That would be the second copy of one rule**, and this
directory's history is largely a record of what a second copy costs: `E18-1`
found the fourth copy of one search rule, `E27-1` the fifth, `W8-E11-1` a third
`aastore` twin. The display rule and the type rule would then have to be kept
in step by hand forever, and the next person to add a receiver family would
update one of them.

The principled fix is the one `G88-1` §5 already measured: **make these
containers real**. Retagging the whole map/set ownership cluster together took
`RCollections` (53 checks), `RJdkMapViews` (74) and `RChmKeySetView` from
broken to green in one build, because real JDK bytecode CAN own these
containers — what fails is a PARTIAL retag. When `Map.of(...)` returns a real
`ImmutableCollections$Map1`, `is_subclass_of` walks a real chain and all twelve
cells come right with no rule to keep in step, because there is only one rule.

**WITHDRAWN, measured 2026-08-20 (see the correction banner at the top).** The
paragraph that stood here said this work was "now unblocked" by the wave-2
authorisation and that §4's twelve cells were "the acceptance criteria for that
cluster change". Both halves are false:

* the twelve cells are **already right under `--jdk-only`**, so a strict-mode
  change has nothing to earn there;
* `H4-1` §2 measured that `native-builtins/src/lib.rs` contains **zero**
  `java/util/Properties` or `java/util/Hashtable` registrations — the 67 are in
  `properties_sidetable.rs` (35), `deprecated_util.rs` (12),
  `deprecated_io_util.rs` (7) and `wildfly_naming.rs` (3). Re-verified here:
  that file has two occurrences of either name and neither is a registration.
  **`G88-1` §6 conflated the crate with the file.** Contract §8 names one file,
  and the Properties/Hashtable cluster was never inside it — so §8 was never
  the blocker for that row, and lifting it did not unblock this.

What remains true is §3: the display rule and the type rule read different
state, and a static superclass link cannot serve both receivers. What is now
open is **which** mechanism should fix the compatible-mode face, given that
`NativeKind` demonstrably cannot — `H4-1` §3 shows the kind is discarded in
Compatible mode entirely.

## 6. What this corrects

* **`W8-C4-2`** — its `STATUS: NOMINATION` banner is stale; the
  `typecheck.rs` patch it publishes as pending **landed** (`typecheck.rs:1534`,
  `git log -S'java/util/ImmutableCollections$Map'` → `b7e24364f`). Its
  §4 predicted table is right that `AbstractMap` stays false, and wrong about
  why.
* **`typecheck.rs:1530`'s comment** — accurate that the receiver is a
  `cratonvm/internal/UnmodifiableMap`, misleading in implying a superclass link
  would settle it. The marker is the reason it cannot.
* **`P4A` N1b** — its three candidate designs are (a) make the receiver real,
  (b) give the stamp `AbstractMap`'s chain, (c) route the opcodes through the
  `getClass()` alias. **(b) is now disproved** by the `unmodifiableMap` row: it
  would trade a false negative for a false positive. (a) and (c) survive, and
  (a) is `G88-1` §5's measured path.

## 7. NOMINATIONS

* **N1 — extend `RImmutableFactoryTypes` by the two axes it is missing**:
  `instanceof AbstractCollection` and `instanceof RandomAccess`, over the same
  24 receivers it already builds. It is nine lines in a vector that already has
  the receivers, the `expect`/`drain` machinery and the HotSpot column. Until
  then the arms cannot see eleven of the twelve cells, and a cluster change that
  fixed four of them would read as "still failing".
* **N2 — the `RandomAccess` row deserves its own check with a cost attached**:
  assert `Collections.binarySearch(List.of(…), k)` returns the right index
  *and* that the receiver reports `RandomAccess`, so a future regression is
  reported as the dispatch bug it is rather than as a slow test.
* **N3 — audit the other `GetClassDisplay` arms the same way.** `CollList`,
  `CollSet`, `CollSortedSet`, `CollNavigableSet`, `CollUnmod` and `CollMap` all
  advertise a class name whose supertypes the carrier does not carry.
  `Collections$UnmodifiableSortedSet` and `…NavigableSet` were never probed at
  all (`P4A` says so); §4's table does not cover them either, because
  `AbstractChain.java` does not build those receivers. **Stated so it is not
  read as covered.**
* **N4 — check the JIT.** §4 is an interpreter measurement. `instanceof` and
  `checkcast` are lowered separately by the JIT (`W8-E11-1`, `E27-1`), so a
  cluster fix verified only in the interpreter has not been verified.
