# `ArrayStoreException: org.hibernate.sql.results.graph.Initializer` — `Arrays.fill` refused a legal store into a two-dimensional array

**Retired 2026-08-29.** Opened the same day as
`hibernate-reactive-arraystoreexception-initializer-20260829.md`, which had nine
classes, one byte-identical exception, and two undistinguished candidate
explanations. It was a real CratonVM defect — candidate 1 in that page's own
list — and the refusal half of it had ALREADY been fixed on `dev` hours earlier,
by a commit filed against an entirely different symptom. The message half had
not, and this page closes it.

## What was open

Nine hibernate-reactive classes, all in the embeddable/embedded-id mapping
family, all with the same exception:

```
java.lang.ArrayStoreException: org.hibernate.sql.results.graph.Initializer
	at org.hibernate.reactive.sql.exec.internal.StandardReactiveSelectExecutor.doExecuteQuery(...:195)
```

## Reading the message was the whole diagnosis

`org.hibernate.sql.results.graph.Initializer` is an **INTERFACE**. No instance
can ever have an interface as its class, so an `ArrayStoreException` naming one
cannot be reporting a value's type. On a reference array this VM's heap header
holds the **component** class rather than the array's own type — stated on
`vm::runtime::interpreter::typecheck::array_descriptor_of` and again on
`cce_display_class_name`, where the same trap once produced `java.lang.String
cannot be cast to java.lang.String` and cost a session as a supposed
class-identity split. So a raw `class_name_of_id(class_id_of_object(v))` is off
by exactly one array dimension for an ARRAY-valued element and right for
everything else.

That says the value was an `Initializer[]`, which names the store. The
truncated trace in the open page stops at a `CompletableFuture` boundary; the
`Caused by:` frames further down the runner's `raw.log` reach it:

```
Caused by: java.lang.ArrayStoreException: org.hibernate.sql.results.graph.Initializer
	at ...EmbeddableInitializerImpl.fill(EmbeddableInitializerImpl.java:197)
	at ...EmbeddableInitializerImpl.<init>(EmbeddableInitializerImpl.java:116)
```

and `fill` is one line:

```java
private static void fill(@Nullable Initializer<InitializerData>[][] initializers) {
    Arrays.fill( initializers, Initializer.EMPTY_ARRAY );
}
```

`Initializer[][]` filled with `Initializer[]`. A legal store, refused by
`Arrays.fill(Object[], Object)`'s native. Not the `aastore` opcode — the opcode
admits it, and `RArrayStoreLibrary`'s `s27` is the control that proves it.

## Two defects, one cause, fixed a day apart

`Arrays.fill(Object[], Object)`'s store check compared **`ClassId`s**:

```rust
let comp   = ctx.class_id_of_object(arr);   // Initializer[][]  -> [LInitializer;
let actual = ctx.class_id_of_object(v);     // Initializer[]    -> Initializer
if actual != comp && !ctx.is_subclass(actual, comp) && comp is not Object { throw }
```

The two sides can never be equal for an array-valued element, because they are
one dimension apart by construction. That comparison is wrong in both
directions, and it produced:

1. **A false refusal.** Closed on `dev` earlier the same day by `da5638fde`
   ("fix(charset): `Arrays.fill(Object[], Object)` refused a primitive-array
   component"), which was filed against `sun.nio.cs.HKSCS$Encoder.initc2b`'s
   `Arrays.fill(c2b, C2B_UNMAPPABLE)` breaking every Big5-HKSCS/MS950_HKSCS
   charset's static init. Same line, same cause, a completely different
   symptom. That commit added a fallback to the shared `aastore` predicate
   under the `ClassId` comparison, and the shared predicate answers the
   two-dimensional shape correctly.

   **MEASURED on `dev@84a98929e` BEFORE any change of this page's**: the exact
   `EmbeddableInitializerImpl.fill` shape, against the real `Initializer`
   interface on hibernate-core's own classpath, passes on both VMs, and all
   nine classes pass against a live Postgres (table below). What cannot be
   re-checked is the failing run's own binary — it lived at
   `CratonVM/target-tomcat-parallel/release/cratonvm.exe`, which no longer
   exists — so "the 07:24 run predated the fallback" is the explanation the
   code supports, not something this page measured. The claim that IS measured
   is the one that matters for the nine classes: on the tip, they pass.
2. **A wrong message**, which `da5638fde` left standing because it only touched
   the predicate. `Arrays.fill((Object[]) new String[3][], new Integer[0])` is a
   store that SHOULD throw, and this VM named `java.lang.Integer` where HotSpot
   names `[Ljava.lang.Integer;`. That is the same one-dimension error, surviving
   in the refusal path after being fixed in the admission path.

Both are closed here:

* `native-api/src/array_store.rs` gains `aastore_value_external_name`, which
  rebuilds the JVMS descriptor for an array value through
  `NativeContext::object_is_array` / `heap_element_type_of` — heap object-kind
  reads that do not go through the class table, which is exactly what the
  component-vs-array ambiguity needs. This closes the module's one recorded
  KNOWN GAP, whose comment said `NativeContext` "exposes no element-type
  accessor to rebuild the descriptor from". It does.
* `native-collections/src/lib.rs`'s `native_arrays_fill_object` and
  `native-builtins/src/phases_early.rs`'s twin registration both now route
  through `array_store::reject_unstorable(.., StoreRoute::Aastore)` — the one
  place a native's store rule lives. That also removes a second latent defect in
  the collections copy: it consulted the shared predicate as
  `.unwrap_or(false)`, i.e. it failed **closed** on the one answer (`None` — "no
  hierarchy in this context") that must never become a refusal.

## There were TWO registrations of the same triple

`--dump-native-registry` says `java/util/Arrays.fill([Ljava/lang/Object;Ljava/lang/Object;)V`
is owned by `native-collections/src/lib.rs:21479` in compatible / real-JDK mode,
with `overwrote: null`. The `native-builtins/src/phases_early.rs` copy is
reached only through `register_core_stdlib_extras`, which
`register_synthetic_overrides` calls and the compatible path does not — and
registration is LAST-WRITE-WINS, so which body is live is a property of the
boot sequence rather than of either file. It was fixed too. A defect with two
copies is a defect that comes back.

## Falsification

`regression-suite/src/RArrayStoreLibrary.java`, the 106th vector, is the
LIBRARY-METHOD half of the array-store question `RArrayStoreTiers` and
`RArrayStoreInterfaces` ask of the opcode: 28 rows over `Arrays.fill`,
`Arrays.copyOf(T[],int,Class)`, `ArrayList.toArray(T[])` (the `System.arraycopy`
route), `AbstractCollection.toArray(T[])` (the `aastore` route),
`System.arraycopy`, `Array.set`, and the opcode itself as the control. Every row
publishes this VM's own answer for the cross-VM diff, and both balance counts
are published so a door that has degenerated to "allow everything" — which is
what all of these natives did before they were given a check at all — cannot
pass by satisfying the legal majority.

Measured, one execution per shape, JDK 25.0.3+9-LTS:

| | HotSpot | CratonVM `dev@84a98929e` | CratonVM with this change |
|---|---|---|---|
| `fails` | 0 | **1** (`s10` COLD-MESSAGE) | 0 |
| `checks` | 114 | 114 | 114 |
| `illegalRefused` | 10/10 | 10/10 | 10/10 |
| `legalAdmitted` | 18/18 | 18/18 | 18/18 |

The one red row on the pre-change VM is the message defect, and the vector was
red before the change and green after — which is the only thing that
distinguishes a gate from a decoration.

## The nine classes

Re-run on `dev@84a98929e` against a live Postgres via Testcontainers,
`run-hibernate-reactive-suite.sh --shards 1`, 2026-08-29:

| class | 2026-08-29 07:24 run (binary no longer available) | on `dev@84a98929e` |
|---|---|---|
| `EagerElementCollectionForEmbeddableEntityTypeMapTest` | FAIL 12/0 | **PASS 12/12** |
| `EagerElementCollectionForEmbeddableTypeListTest` | FAIL 31/0 | **PASS 31/31** |
| `EagerElementCollectionForEmbeddedEmbeddableMapTest` | FAIL 12/0 | **PASS 12/12** |
| `EagerElementCollectionForEmbeddedEmbeddableTest` | FAIL 31/0 | **PASS 31/31** |
| `EagerOrderedElementCollectionForEmbeddableTypeListTest` | FAIL 31/0 | **PASS 31/31** |
| `EmbeddedIdTest` | FAIL 7/0 | **PASS 7/7** |
| `EmbeddedIdWithManyEagerTest` | FAIL 3/0 | **PASS 3/3** |
| `EmbeddedIdWithManyTest` | FAIL 2/0 | **PASS 2/2** |
| `EmbeddedIdWithOneToOneTest` | FAIL 1/0 | **PASS 1/1** |

`status: PASS=9` in 4m9s. Re-run again on the binary carrying THIS page's
change (`cratonvm-hibloc-fixed.exe`): `status: PASS=9` in 3m49s, same
per-class `found`/`ok`. So the message repair does not disturb the nine, which
is the only thing it could have done to them — they were already green.

## "Does this reproduce on H2 too?" — the open page's fourth question

MEASURED: **no ArrayStoreException appears anywhere in the same day's H2
complete-suite run** — `apps/hib-suite-runner/runs/run-20260829-000921-passed`,
4548 classes, `PASS=4438 FAIL=10 CRASH=1`, `grep -c ArrayStoreException
shard-0/raw.log` = **0**. So in practice it was reactive-only.

The mechanism is not, though, and the distinction matters for anyone who meets
it again. `EmbeddableInitializerImpl` is hibernate-ORM code; the failing call
touches no driver, no connection and no dialect; and the reduced shape
(`RArrayStoreLibrary` `s01`) reproduces with no database at all. What decides
whether a run meets it is which embeddable result graphs the suite happens to
build — and, for these two runs, which binary each used. It is not a property
of Postgres, of Vert.x, or of the reactive executor whose frame the stack
happens to name.

## What the open page got right, and what it cost to not check

Its candidate 1 ("a genuine CratonVM defect in array-store type checking") was
correct, and its instinct that this was adjacent to the `checkcast` /
`KIND_TAGS` family was correct too — same root question, different door. What it
did not do was ask whether `dev` had already moved: the refusal was closed
before the page was written, by a commit whose title mentions a charset. A
suite log is a photograph of one binary, and a page that says "not yet
cross-checked" should cross-check against the tip before it names nine open
classes.
