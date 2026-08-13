# W8-C4-2 — `Map.of() instanceof Collection` is TRUE: the blanket is a substring, and the guard against it misses by one word

> **STATUS: NOMINATION (one-line source change, outside this lane's owned
> files) + a regression fixture that IS applied at
> `regression-suite/src/RImmutableFactoryTypes.java`.** The defect is located
> exactly and reproduced mechanically without the VM (§3); the "after" column
> is **PREDICTED** — this lane may not run the CratonVM binary.

Lane C4, 2026-08-12. Oracle: Temurin `jdk-25.0.3.9-hotspot`, windows/x64.
Probes: `scratchpad/c4/CollTruth.java` (the Java truth table),
`scratchpad/c4/blanket.rs` (a verbatim, VM-free reproduction of the predicate).
Predecessor: `P4A-SPRING-20260812.md` §5d / N1b, which measured the symptom and
declined to name a patch.

---

## 1. Java's rule, and the oracle

`java.util.Map` does **not** extend `java.util.Collection`. A `Map`'s *views*
do. Full table, `java -cp out CollTruth` on HotSpot 25 (abridged to the columns
at issue; the run also reads `Set`, `List`, `AbstractMap`, `Serializable` and
all three `checkcast` results):

| receiver | `getClass()` | `instanceof Collection` | `instanceof Iterable` | `(Collection)` cast |
|---|---|---|---|---|
| `Map.of()` | `ImmutableCollections$MapN` | **false** | false | CCE |
| `Map.of(k,v)` | `ImmutableCollections$Map1` | **false** | false | CCE |
| `Map.of(k,v,k2,v2)` | `ImmutableCollections$MapN` | **false** | false | CCE |
| `Map.copyOf(…)` | `ImmutableCollections$MapN` | **false** | false | CCE |
| `Map.of().values()` | `AbstractMap$2` | **true** | true | OK |
| `Map.of().keySet()` | `AbstractMap$1` | **true** | true | OK |
| `Map.of().entrySet()` | `ImmutableCollections$Set12` | **true** | true | OK |
| `List.of()` / `List.of(x)` | `ImmutableCollections$ListN` / `$List12` | **true** | true | OK |
| `Set.of(x)` | `ImmutableCollections$Set12` | **true** | true | OK |
| `Arrays.asList(x)` | `Arrays$ArrayList` | **true** | true | OK |
| `Collections.emptyMap()` | `Collections$EmptyMap` | **false** | false | CCE |
| `Collections.singletonMap` | `Collections$SingletonMap` | **false** | false | CCE |
| `Collections.unmodifiableMap` | `Collections$UnmodifiableMap` | **false** | false | CCE |
| `Map.entry(k,v)` | `KeyValueHolder` | **false** | false | CCE |
| `new HashMap()` / `new TreeMap()` | — | **false** | false | CCE |

and the consequence line: `CONSEQUENCE|CCE at the cast (correct)`.

CratonVM `--jdk-only` answers **true** for the first four rows (P4A §5d), and
lets `(Collection) Map.of("k","v")` through — failing four frames later with
`NoSuchMethodError: java.util.ImmutableCollections$Map1.iterator()`. It reached
this project as two Spring `ObjectUtilsTests` assertions, because
`ObjectUtils.nullSafeConciseToString` tests `instanceof Collection` *before*
`instanceof Map` and so rendered a map as `[...]`.

## 2. The code, and why it is a substring

`vm/src/runtime/interpreter/typecheck.rs`, `fn synthetic_implements` — the
**name-based fallback** consulted only after the real hierarchy check has
already failed. It can only *admit*; a `false` from it means "no opinion". So
every defect it can cause is an over-admission, which is exactly the shape
measured.

Lines 927-938:

```rust
    if obj_name.starts_with("java/util/") {
        match target_class_name {
            "java/util/Collection" | "java/lang/Iterable" => {
                if obj_name.contains("List")
                    || obj_name.contains("Set")
                    || obj_name.contains("Queue")
                    || obj_name.contains("Deque")
                    || obj_name.contains("Collection")
                {
                    return true;
                }
            }
```

`"java/util/ImmutableCollections$Map1"` starts with `java/util/`, and it
contains the substring `Collection` — inside the word **`ImmutableCollections`**,
the name of the *container class*, which has nothing to do with the nested
class's supertypes. So the fallback admits `Collection` and `Iterable` for
every `Map` member of that family.

**This exact trap was already found once and fixed once, twenty lines above**,
at line 906:

```rust
    if obj_name.starts_with("java/util/Collections$") {
        return false;
    }
```

Its comment says it in as many words — `"Collections"` itself contains the
substring `"Collection"`, so `Collections$SingletonMap` was misreported, which
broke Groovy's `DefaultTypeTransformation.asCollection`. The guard is correct
and it is **one word short**: `java.util.ImmutableCollections` is a *second*
container class whose name contains `Collection`, and its prefix is
`java/util/ImmutableCollections$`, which `starts_with("java/util/Collections$")`
does not match. Same bug, same file, same failure mode, different prefix — the
"grep the SHAPE, not the name" pattern.

### Why the mode table has two faces

P4A recorded `--jdk-only` over-accepting `Collection`/`Iterable` while
`--real-jdk` *under*-accepts `AbstractMap`. Reading the two paths explains
every cell without further measurement:

* **`--real-jdk` / `Compatible`:** `Map.of(…)` produces a
  `cratonvm/internal/UnmodifiableMap` stamp whose supertypes are declared in
  `vm/src/vm/vm_init.rs:1664` as `&[map_id, serializable_id]` with superclass
  `Object`. That is why `Collection` is correctly false — the blanket's
  `starts_with("java/util/")` guard excludes the name — and why `AbstractMap` is
  wrongly false: the stamp genuinely has no `AbstractMap` in its chain.
  `getClass()` is separately **aliased** to report `ImmutableCollections$Map1`
  (`native-builtins/src/lib.rs`, `GetClassDisplay::CollMap`), which is what makes
  reflection look perfect while the opcodes do not.
* **`--jdk-only`:** `ensure_bootstrap_compat_class` returns `None` under strict
  mode, so no stamp exists and the receiver is a **real**
  `java.util.ImmutableCollections$Map1` with a real hierarchy — hence
  `AbstractMap` correctly true and `Map` true. `Collection` is false in that
  real hierarchy too, and then `synthetic_implements` runs as the last-resort
  fallback and the substring says **true**.

Both faces, six measured cells, one reading. Nothing else needs to change to
explain them.

## 3. Mechanical reproduction, with a mutation check

`scratchpad/c4/blanket.rs` copies the two blocks verbatim into a standalone
program (`rustc -O -o blanket.exe blanket.rs`; **not** cargo, writes into no
target dir) and runs them against HotSpot's answers from §1:

```text
java/util/ImmutableCollections$Map1                  hotspot=false blanket=true  patched=false <-- FIXED
java/util/ImmutableCollections$MapN                  hotspot=false blanket=true  patched=false <-- FIXED
java/util/ImmutableCollections$AbstractImmutableMap  hotspot=false blanket=true  patched=false <-- FIXED
java/util/ImmutableCollections$List12                hotspot=true  blanket=true  patched=true
java/util/ImmutableCollections$ListN                 hotspot=true  blanket=true  patched=true
java/util/ImmutableCollections$Set12                 hotspot=true  blanket=true  patched=true
java/util/ImmutableCollections$SetN                  hotspot=true  blanket=true  patched=true
java/util/Collections$EmptyMap                       hotspot=false blanket=false patched=false
java/util/Collections$SingletonMap                   hotspot=false blanket=false patched=false
java/util/Collections$UnmodifiableMap                hotspot=false blanket=false patched=false
java/util/HashMap                                    hotspot=false blanket=false patched=false
java/util/TreeMap                                    hotspot=false blanket=false patched=false
java/util/ArrayList                                  hotspot=true  blanket=true  patched=true
java/util/Arrays$ArrayList                           hotspot=true  blanket=true  patched=true
java/util/KeyValueHolder                             hotspot=false blanket=false patched=false
cratonvm/internal/UnmodifiableMap                    hotspot=false blanket=false patched=false
over-admissions before=3 after=0
OK
```

The program **asserts `before_wrong > 0`**: if the blanket admitted nothing
HotSpot denies, the probe could not go red and would prove nothing. Exactly
three cells move and no other cell moves in either direction.

## 4. NOMINATION — `vm/src/runtime/interpreter/typecheck.rs`

Not this lane's file. One replacement, at the guard on line 906.

OLD (exact, unique in the file):

```rust
    if obj_name.starts_with("java/util/Collections$") {
        return false;
    }
```

NEW:

```rust
    if obj_name.starts_with("java/util/Collections$")
        // `java.util.ImmutableCollections` is the CONTAINER class of the
        // `Map.of()` / `List.of()` / `Set.of()` family, and its own name
        // contains the substring "Collection" — the same trap the
        // `Collections$` prefix above exists for, missed because the prefix
        // differs by one word. Its Map members reached the
        // `contains("Collection")` term below and were admitted as
        // `instanceof Collection` / `Iterable`, which HotSpot 25 denies
        // (measured: `Map.of("k","v") instanceof Collection` is false, and
        // `(Collection) Map.of("k","v")` throws). CratonVM let the cast through
        // and died four frames later at `ImmutableCollections$Map1.iterator()`.
        //
        // Deliberately narrower than excluding the whole `ImmutableCollections$`
        // family: `List12`/`ListN`/`Set12`/`SetN` are green today in all three
        // arms, they are admitted by this fallback's `List`/`Set` terms rather
        // than by the accident, and moving a green cell is not something to do
        // in a change the authoring lane cannot build.
        // docs/known-issues/jdk-only/W8-C4-2-map-of-instanceof-collection.md
        || obj_name.starts_with("java/util/ImmutableCollections$Map")
        || obj_name == "java/util/ImmutableCollections$AbstractImmutableMap"
    {
        return false;
    }
```

**Predicted effect** (each falsifiable by §5's fixture):

| row | before | after |
|---|---|---|
| `Map.of(k,v) instanceof Collection` (`--jdk-only`) | true | **false** |
| `Map.of(k,v) instanceof Iterable` (`--jdk-only`) | true | **false** |
| `(Collection) Map.of(k,v)` (`--jdk-only`) | succeeds | **ClassCastException** |
| `List.of()/Set.of()/Arrays.asList` everything | correct | **unchanged** |
| `Map.of(k,v) instanceof AbstractMap` (`--real-jdk`) | false (wrong) | **still false — NOT fixed by this** |

### What this nomination does NOT fix, said plainly

The `--real-jdk` face. That one is the `cratonvm/internal/UnmodifiableMap`
stamp's missing `AbstractMap` superclass, and P4A N1b's three candidate designs
(make the receiver real / give the stamp `AbstractMap`'s chain / route the
opcodes through the `getClass()` alias) all still apply. This change closes the
**strict-mode** face — the one that turns a refusal into a wrong answer that
detonates elsewhere — and it closes it with one prefix, which is a different
size of change from any of the three. Do not read it as closing P4A §5d.

### Audit that goes with it, not done here

P4A asks for the other `GetClassDisplay` arms (`CollList`, `CollSet`,
`CollSortedSet`, `CollNavigableSet`, `CollUnmod`,
`native-builtins/src/lib.rs:25385-25605`) to get the same treatment. §1's table
now covers `unmodifiableSortedSet` and `unmodifiableNavigableSet`, which P4A
says were never probed at all — on HotSpot both are
`Collection`/`Iterable`/`Set` true, `Map` false. The fixture in §5 asserts them.

## 5. Scheduled — `regression-suite/src/RImmutableFactoryTypes.java` (APPLIED)

P4A: "the regression vector is four lines and belongs in the suite, because
*nothing currently covers it*." It is more than four lines because a four-line
vector is how this class of bug survives:

* 24 receivers × {`instanceof Collection`, `Iterable`, `Map`} × {the same three
  as `checkcast`} — the opcodes are read **separately** because they are
  separate opcodes and the JIT lowers them separately;
* every row also reads `Class.isInstance` / `isAssignableFrom` as a hard
  `check()` **control**: those are right in this VM today, so if they ever go
  red the defect has moved somewhere else entirely;
* the Map *views* (`values()`, `keySet()`, `entrySet()`) are asserted as their
  own family, because a blanket "collection-ish builtin" answer gets them right
  by accident and a table without them cannot tell the two apart;
* `expect`/`drain`: divergences are printed and counted, then thrown **once**
  at the end. A fixture that dies on its first member gives a taker one member
  per rebuild of this VM — the exact reason W7-37 row 7 stayed wrong.

Verified green on the oracle: `PASS RImmutableFactoryTypes (219 checks)`.
**Mutation-checked** — flipping one expectation (`Map.of(k,v)` to
`isCollection=true`) makes it print four `DIVERGENCE` lines and exit 1, so it
can go red.

### NOMINATION — `regression-suite/run.sh` (not this lane's file)

Register the fixture by adding `RImmutableFactoryTypes` to `CORE_CLASSES`
(line 106). Exact edit — append to the existing value:

OLD (end of the `CORE_CLASSES` line):

```
 RJdkStrictMath RJdkByteOrder RJdkIntrinsics"
```

NEW:

```
 RJdkStrictMath RJdkByteOrder RJdkIntrinsics RImmutableFactoryTypes"
```

**Land this with your eyes open: the fixture is RED on CratonVM today, in both
modes**, by construction — `--jdk-only` fails the `Collection`/`Iterable`/cast
rows and `--real-jdk` fails the `AbstractMap` row. If the suite must stay green
while §4 is pending, hold the registration and run the class directly; do not
weaken the fixture to land it.
