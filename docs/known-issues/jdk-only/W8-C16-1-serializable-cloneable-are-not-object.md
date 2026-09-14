# `Serializable[]` and `Cloneable[]` are not `Object[]` — the kept-and-wrong arm, closed as a pair

**Status: FIXED IN SOURCE (lane C16, this record's own commit). NOT EXECUTED —
this lane may not build or run CratonVM, so every CratonVM "after" is
PREDICTED. The HotSpot rows are EXECUTED and pasted verbatim.** 2026-08-12/13.

Oracle: HotSpot 25.0.3, Microsoft build 25.0.3+9-LTS, windows/x64, run under
`-Xint` **and** with the JIT. Probes: `scratchpad/c16/Arm3.java`,
`scratchpad/c16/W.java`, `scratchpad/c16/GenJrt.java` (generates a 3,462-row
table straight out of `jrt:/`), `scratchpad/c16/score.rs`,
`scratchpad/c16/synthcheck.rs` (compiles the LANDED predicate body under plain
`rustc`). No cargo, no CratonVM binary.

Closes `W8-C10-1` §3, which measured this arm wrong and deliberately KEPT it.

---

## 1. What the arm was

`vm/src/runtime/interpreter/typecheck.rs`, `aastore_element_assignable`, arm 3
of 15:

```rust
// Object[] (and Serializable[]/Cloneable[]) accept any reference element.
if component == "Ljava/lang/Object;"
    || component == "Ljava/io/Serializable;"
    || component == "Ljava/lang/Cloneable;"
{
    return true;
}
```

Only the first third is a rule. Every reference IS an `Object`, so an
`Object[]` component can never make a store illegal. `Serializable` and
`Cloneable` are ordinary marker interfaces and `aastore` checks them like any
other interface.

## 2. The full HotSpot truth table

`scratchpad/c16/Arm3.java`, run `java -Xint -cp . Arm3`. Identical with the JIT
on (both transcripts taken). Every cell is a real `aastore` executed on the
oracle:

```text
java.vm.version = 25.0.3+9-LTS
java.vm.name    = OpenJDK 64-Bit Server VM

component                  value runtime class                isSer  isClo  isMrk  store result
Ljava/lang/Object;         java.lang.Object                   false  false  false  OK
Ljava/lang/Object;         java.lang.Integer                  true   false  false  OK
Ljava/lang/Object;         java.lang.String                   true   false  false  OK
Ljava/lang/Object;         [I                                 true   true   false  OK
Ljava/lang/Object;         [Ljava.lang.Integer;               true   true   false  OK
Ljava/lang/Object;         [[Ljava.lang.String;               true   true   false  OK
Ljava/lang/Object;         java.util.ArrayList                true   true   false  OK
Ljava/lang/Object;         java.util.HashMap                  true   true   false  OK
Ljava/lang/Object;         Arm3$Plain                         false  false  false  OK
Ljava/lang/Object;         Arm3$Ser                           true   false  false  OK
Ljava/lang/Object;         Arm3$Clo                           false  true   false  OK
Ljava/lang/Object;         Arm3$$Lambda/0x000000000e040c48    false  false  false  OK
Ljava/lang/Object;         $Proxy0                            true   false  true   OK

Ljava/io/Serializable;     java.lang.Object                   false  false  false  ArrayStoreException | java.lang.Object
Ljava/io/Serializable;     java.lang.Integer                  true   false  false  OK
Ljava/io/Serializable;     java.lang.String                   true   false  false  OK
Ljava/io/Serializable;     [I                                 true   true   false  OK
Ljava/io/Serializable;     [Ljava.lang.Integer;               true   true   false  OK
Ljava/io/Serializable;     [[Ljava.lang.String;               true   true   false  OK
Ljava/io/Serializable;     java.util.ArrayList                true   true   false  OK
Ljava/io/Serializable;     java.util.HashMap                  true   true   false  OK
Ljava/io/Serializable;     Arm3$Plain                         false  false  false  ArrayStoreException | Arm3$Plain
Ljava/io/Serializable;     Arm3$Ser                           true   false  false  OK
Ljava/io/Serializable;     Arm3$Clo                           false  true   false  ArrayStoreException | Arm3$Clo
Ljava/io/Serializable;     Arm3$$Lambda/0x000000000e040c48    false  false  false  ArrayStoreException | Arm3$$Lambda/0x000000000e040c48
Ljava/io/Serializable;     $Proxy0                            true   false  true   OK

Ljava/lang/Cloneable;      java.lang.Object                   false  false  false  ArrayStoreException | java.lang.Object
Ljava/lang/Cloneable;      java.lang.Integer                  true   false  false  ArrayStoreException | java.lang.Integer
Ljava/lang/Cloneable;      java.lang.String                   true   false  false  ArrayStoreException | java.lang.String
Ljava/lang/Cloneable;      [I                                 true   true   false  OK
Ljava/lang/Cloneable;      [Ljava.lang.Integer;               true   true   false  OK
Ljava/lang/Cloneable;      [[Ljava.lang.String;               true   true   false  OK
Ljava/lang/Cloneable;      java.util.ArrayList                true   true   false  OK
Ljava/lang/Cloneable;      java.util.HashMap                  true   true   false  OK
Ljava/lang/Cloneable;      Arm3$Plain                         false  false  false  ArrayStoreException | Arm3$Plain
Ljava/lang/Cloneable;      Arm3$Ser                           true   false  false  ArrayStoreException | Arm3$Ser
Ljava/lang/Cloneable;      Arm3$Clo                           false  true   false  OK
Ljava/lang/Cloneable;      Arm3$$Lambda/0x000000000e040c48    false  false  false  ArrayStoreException | Arm3$$Lambda/0x000000000e040c48
Ljava/lang/Cloneable;      $Proxy0                            true   false  true   ArrayStoreException | $Proxy0

LArm3$Marker;              java.lang.Object                   false  false  false  ArrayStoreException | java.lang.Object
LArm3$Marker;              java.lang.Integer                  true   false  false  ArrayStoreException | java.lang.Integer
LArm3$Marker;              [I                                 true   true   false  ArrayStoreException | [I
LArm3$Marker;              java.util.ArrayList                true   true   false  ArrayStoreException | java.util.ArrayList
LArm3$Marker;              $Proxy0                            true   false  true   OK
```

(The `Marker[]` block is the control: an ordinary user interface, four of the
same rows, all refused except the proxy created FOR that interface. If
`Serializable`/`Cloneable` behaved specially, this block would not match them.)

Read it in three parts:

* **The rule.** `Object[]` accepts all thirteen values. Nothing else does.
* **The three rows arm 3 got wrong.** `Serializable[] <- Object`,
  `Cloneable[] <- Object`, `Cloneable[] <- Integer` — all
  `ArrayStoreException`, all admitted by CratonVM.
* **The legal rows that must keep working.** `Serializable[] <- String`,
  `Serializable[] <- Integer`, `Cloneable[] <- ArrayList`, and — the one that
  is easy to miss — **every ARRAY implements both**
  (JVMS §4.10.1.2 / JLS §10.7): `Cloneable[] <- int[]`,
  `Serializable[] <- int[]`, `Serializable[] <- Integer[]`,
  `Cloneable[] <- String[][]` are all `OK`. Get that wrong and ordinary
  array-of-array code starts throwing.

The `Integer` pair is the whole argument in one line: `Integer` **is**
`Serializable` and **is not** `Cloneable`. No blanket over both interfaces can
express that.

Two rows are worth naming because CratonVM answers them by a different arm and
they must not be mistaken for regressions:

* `Serializable[] <- lambda` throws on HotSpot. CratonVM fails open on
  synthetic ids `>= 0x8000_0000`. Under-refusal, pre-existing, by design.
* `Cloneable[] <- $Proxy0` throws on HotSpot (a `Proxy` is `Serializable`, not
  `Cloneable`). CratonVM fails open on the `$Proxy` name test. Same.

## 3. Why it was kept, and why that reasoning was right

`W8-C10-1` §3 kept it because the machinery below would answer these rows
correctly **only where real class bytes exist**. `synthetic_implements` — the
last-resort fallback for classes with no real interface data — had no
`Serializable` or `Cloneable` arm at all, and this predicate's documented
contract is that it must never produce a FALSE `ArrayStoreException`.

That risk is real and this lane measured its size. In synthetic-JDK mode a
class's interfaces are whatever `classloading/src/class_manager.rs`'s
`jdk_interfaces` table says. That table names `java/lang/Cloneable` for
**exactly two** classes (`Hashtable`, `Properties`). HotSpot has it on **81**
classes in `java.base`'s `java.util`/`java.lang`/`java.io`/`java.math`/
`java.text`/`java.time`/`java.net`/`java.security`/`java.nio` packages alone
(MEASURED: `scratchpad/c16/GenJrt.java` walks `jrt:/modules/java.base` and reads
`Cloneable.class.isAssignableFrom(c)` for each of 3,462 classes). Deleting arm 3
on its own would have turned `Cloneable[] <- fabricated ArrayList` into a
spurious `ArrayStoreException`.

So the fix is a PAIR, exactly as that record said. Landing half of it is worse
than landing none.

## 4. The fix as applied

`vm/src/runtime/interpreter/typecheck.rs`, three changes, all in this lane's
file.

**(a) Arm 3 keeps only the rule.**

```rust
if component == "Ljava/lang/Object;" {
    return true;
}
```

The array rows stay right without an arm here: an ARRAY value falls into the
block immediately below, and `array_is_assignable_to_impl` **opens** with the
JLS §10.7 rule (`target_name == "java/io/Serializable" || "java/lang/Cloneable"`
→ `true`). A plain object now goes to `is_subclass_of`, which recurses into the
superclass and walks ITS interfaces — read, not assumed:
`classloading/src/class.rs` `is_subclass_of_inner` recurses on `self.superclass`
before iterating `self.interfaces`, so `Integer → Number → Serializable`
resolves.

**(b) A fabricated-class hatch for exactly these two components**, at the bottom
of the function beside the `$Proxy` hatch, i.e. AFTER `is_subclass_of` and
`is_assignable_to_name` have both declined:

```rust
if (comp_name == "java/io/Serializable" || comp_name == "java/lang/Cloneable")
    && vn != "java/lang/Object"
    && cls.origin.is_compatibility_stub()
{
    return true;
}
```

`ClassOrigin::CompatibilityStub` is the VM's own admission that it fabricated
the class because the real bytes were not found (`classloading/src/class_origin.rs`;
the same predicate `--jdk-only` rejects on). It is not a re-run of the interface
blanket `W7-101` deleted: it is scoped to two component types, to values the VM
says it fabricated, and to the point after the hierarchy has already declined.

**`java/lang/Object` is excluded, and that exclusion is the point.** A
fabricated `Object` is not a case of missing information — `java.lang.Object`
implements no interfaces, which is the definition of the root type, not a fact
about a class file. Without that clause synthetic-JDK mode would keep admitting
`Serializable[] <- new Object()` (in that mode `java/lang/Object` is itself a
stub), and the two rows that motivated the change would be closed in only one
of the two modes.

**(c) `synthetic_implements` grows the two marker arms**, so the relationship is
declarable by name like every other relationship in that function — which also
fixes `instanceof Serializable` / `instanceof Cloneable` for fabricated classes,
where it is reached from `checkcast`/`instanceof` in both the interpreter
(`opcodes.rs`) and the JIT (`helpers.rs`), not only from `aastore`.

Placement matters and is stated in-code: the arms go ABOVE the
`java/util/Collections$` exclusion, which returns `false` for **every** target
and not just the collection ones. A `Collections$SingletonList` is genuinely
`Serializable` and must not be denied by a guard written about the substring
"Collection".

The arms carry an array clause first (`obj_name.starts_with('[')` → `true` for
both), because this function is also reached with a raw array class name from
paths that do not go through `array_is_assignable_to_impl`.

### The lists are measured, and measuring them changed them

Every name in both lists was checked against the generated table;
`scratchpad/c16/score.rs` **asserts** that each listed name reads `true` on
HotSpot and fails otherwise. That assertion threw out two entries this lane had
written from recall:

```text
SER list: 71 names, 71 matched a table row, 1 NOT Serializable on HotSpot
  WRONG (not Serializable): java/util/WeakHashMap
CLO list: 28 names, 28 matched a table row, 1 NOT Cloneable on HotSpot
  WRONG (not Cloneable): java/util/AbstractMap
```

Confirmed independently (`scratchpad/c16/W.java`):

```text
java.util.WeakHashMap        Serializable=false  Cloneable=false  ifaces=[interface java.util.Map]
java.util.HashMap            Serializable=true   Cloneable=true   ifaces=[interface java.util.Map, interface java.lang.Cloneable, interface java.io.Serializable]
java.util.AbstractMap        Serializable=false  Cloneable=false  ifaces=[interface java.util.Map]
Serializable[] <- WeakHashMap -> ASE | java.util.WeakHashMap
Cloneable[]    <- WeakHashMap -> ASE | java.util.WeakHashMap
```

`WeakHashMap` is not Serializable and `AbstractMap` declares `clone()` without
declaring `Cloneable`. Both are the kind of fact that survives any amount of
confident recall. **A list written from memory would have shipped two
over-admissions into a predicate whose whole job is to be conservative.**

63 of the 70 `Serializable` names and 23 of the 27 `Cloneable` names are names
that `classloading/src/class_manager.rs` actually mentions, so the lists are
aimed at the fabricable population rather than at the JDK in general.

## 5. What this lane could and could not check without a build

* The landed `synthetic_implements` body was extracted **mechanically** out of
  `typecheck.rs` and compiled under plain `rustc` with a stub signature
  (`scratchpad/c16/synth.rs` + `synthcheck.rs`). It compiles clean — so the two
  long `matches!` blocks have no syntax or type error — and answers 15 spot
  rows correctly, including the three rows this record is about and the array
  clause. Mutation-checked: deleting `"java/util/ArrayList"` from the
  `Cloneable` list makes it fail with
  `assertion left == right failed: java/util/ArrayList / java/lang/Cloneable`.
* The `aastore` hatch itself cannot be extracted (it reads the class store), so
  it was checked by reading, and the three claims it rests on were each checked
  at their source rather than assumed: `is_subclass_of_inner` recurses through
  superclass interfaces; `array_is_assignable_to_impl` opens with the array
  marker rule; `ClassOrigin::is_compatibility_stub`
  (`classloading/src/class_origin.rs:123`) is already called exactly this way
  elsewhere in `vm/src` — `jit/helpers.rs:8253`, `interpreter/invoke.rs:3624`,
  `interpreter/native_override.rs:6594` — so `cls.origin` is in scope on the
  `get_class` result with no new import.

## 6. Vector, and what to expect

`regression-suite/src/RArrayStoreInterfaces.java` (landed with `W8-C10-1`)
already carries every row:

| row | shape | HotSpot | CratonVM before | PREDICTED after |
|---|---|---|---|---|
| `s08` | `Serializable[] <- Object` | ArrayStoreException | no-throw | **refused, both modes** |
| `s09` | `Cloneable[] <- Object` | ArrayStoreException | no-throw | **refused, both modes** |
| `s10` | `Cloneable[] <- Integer` | ArrayStoreException | no-throw | **refused with real class bytes; still admitted in synthetic-JDK mode** |
| `s21` | `Serializable[] <- Integer` | OK | OK | unchanged |
| `s22` | `Serializable[] <- Integer[]` | OK | OK | unchanged |
| `s23` | `Cloneable[] <- Integer[]` | OK | OK | unchanged |

`s10`'s split is deliberate and is the price of the contract: in synthetic-JDK
mode `java/lang/Integer` is a fabricated stub and the hatch fails open. It
shrinks whenever the marker lists grow. **If the orchestrator's run shows `s10`
green in real-JDK mode and red under the synthetic-JDK gate, that is this design
working, not a partial fix.**

Everything else in that fixture must not move — in particular `s25` (a dynamic
proxy into an array of its own interface) and `s26` (an annotation proxy into
`Annotation[]`), the fail-open populations `W7-101` was careful to keep, and the
`DEGENERATE` guard must stay silent.

Run both tiers; the split matters:

```sh
javac -d <outdir> regression-suite/src/RArrayStoreInterfaces.java
<cratonvm>         -cp <outdir> RArrayStoreInterfaces
<cratonvm> --nojit -cp <outdir> RArrayStoreInterfaces
```

## 7. Nominations

### N1 — `classloading/src/class_manager.rs`: `WeakHashMap` is not `Serializable`

`jdk_interfaces` groups it with the `HashMap` family, so synthetic-JDK mode
answers `weakMap instanceof Serializable` **true** where HotSpot answers
**false**, and would serialise-by-type-test down a path the real JVM never
takes. MEASURED above, twice.

OLD (exact literal):

```rust
        "java/util/HashMap"
        | "java/util/LinkedHashMap"
        | "java/util/TreeMap"
        | "java/util/IdentityHashMap"
        | "java/util/WeakHashMap"
        | "java/util/EnumMap"
        | "java/util/concurrent/ConcurrentHashMap"
        | "java/util/concurrent/ConcurrentSkipListMap" => {
            &["java/util/Map", "java/io/Serializable"]
        }
```

NEW:

```rust
        // `WeakHashMap` is NOT here: MEASURED on HotSpot 25.0.3, its
        // `getInterfaces()` is `[java.util.Map]` alone — it implements neither
        // `Serializable` nor `Cloneable`, unlike every other member of this
        // family. Declaring it Serializable made `instanceof Serializable`
        // answer true where the real JVM answers false.
        // docs/known-issues/jdk-only/W8-C16-1-serializable-cloneable-are-not-object.md
        "java/util/WeakHashMap" => &["java/util/Map"],
        "java/util/HashMap"
        | "java/util/LinkedHashMap"
        | "java/util/TreeMap"
        | "java/util/IdentityHashMap"
        | "java/util/EnumMap"
        | "java/util/concurrent/ConcurrentHashMap"
        | "java/util/concurrent/ConcurrentSkipListMap" => {
            &["java/util/Map", "java/io/Serializable"]
        }
```

Note the new arm must come FIRST or the `|` list still matches it.

### N2 — `classloading/src/class_manager.rs`: the `Cloneable` gap, if it is ever worth closing

`jdk_interfaces` declares `java/lang/Cloneable` on two classes; the real JDK has
it on 81 in these packages. The list in `synthetic_implements` now covers the
`instanceof`/`aastore` question by name, so this is **not** blocking; it matters
only for anything that reads `Class.getInterfaces()` on a stub. Left as an
observation on purpose — moving a table that the verifier consults is a change
that wants its own measurement.

### N3 — `regression-suite/src/RArrayStoreInterfaces.java`: add the PRIMITIVE-array rows

The fixture's `AN_INT_ARRAY` is `new Integer[1]` (a reference-component array),
so `s22`/`s23` never exercise the primitive path — and that is a genuinely
different route through CratonVM: `array_descriptor_of` returns `"[I"` from
`element_type_of` without touching the class store, where a reference array goes
through `class_id_of` + a name lookup. The JLS §10.7 rule the record warns about
is exactly the one a primitive array tests.

OLD (exact literal):

```java
    static void s22() { SERIALIZABLES[0] = AN_INT_ARRAY; }   // arrays are Serializable
    static void s23() { CLONEABLES[0]    = AN_INT_ARRAY; }   // arrays are Cloneable
```

NEW:

```java
    static void s22() { SERIALIZABLES[0] = AN_INT_ARRAY; }   // arrays are Serializable
    static void s23() { CLONEABLES[0]    = AN_INT_ARRAY; }   // arrays are Cloneable
    // A PRIMITIVE-component array takes a different route through the VM than a
    // reference-component one (`array_descriptor_of` answers "[I" straight from
    // the element type and never consults the class store), and JLS §10.7
    // applies to it identically. MEASURED OK on HotSpot 25.0.3,
    // `scratchpad/c16/Arm3.java`.
    static void s28() { SERIALIZABLES[0] = A_PRIM_ARRAY; }
    static void s29() { CLONEABLES[0]    = A_PRIM_ARRAY; }
```

with `static final Object A_PRIM_ARRAY = new int[1];` beside `AN_INT_ARRAY`, two
new `"s28 Serializable[]  <- int[]"` / `"s29 Cloneable[]     <- int[]"` labels,
and two `""` (no-throw) entries appended to `EXPECTED_KIND`. Both are LEGAL, so
they raise the `legal stores admitted: N/15` denominator, not the illegal count.

Whoever lands it should re-run the fixture's own mutation check (`W8-C10-1` §5)
afterwards, since the degenerate-blanket emulation must still produce a
`DEGENERATE` divergence.
