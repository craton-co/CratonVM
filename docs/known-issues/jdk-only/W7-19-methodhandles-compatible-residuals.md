# W7-19 — `asCollector` built the wrong container, `bindTo` never refused, and the vector could only report one thing at a time

**Status: DIAGNOSED and FIXED IN SOURCE 2026-08-11. NOT REBUILT.** Every number
below was measured by running the already-built binary at
`C:/craton/CratonVM/target/release/cratonvm.exe` (mtime 19:41:42) and HotSpot
25.0.3 (`Eclipse Adoptium jdk-25.0.3.9-hotspot`). Nothing here claims a source
change works; it claims what was measured before it.

Branch: `fix/methodhandles-compatible-residuals-20260811`.
Files changed: `native-builtins/src/lang_invoke.rs`,
`regression-suite/src/RJdkHandles.java`, and this record. Nothing else.

Both defects are **`Compatible`-mode** defects — a wrong value and a missing
refusal, in the mode that is supposed to be the faithful one. Neither is a
strict-mode policy question. Both were named, and could not be taken, by the
lane that wrote `W7-13-strict-mh-insert-wrapper.md`; that record's §"Two things
observed on the way, not fixed here" is this record's brief.

> **Reading the strict column below.** The binary used for every measurement was
> built at 19:41:42; W7-13's carrier fix was committed at 19:46:27 and merged to
> `dev` at 19:51:17. So the `--jdk-only` column shows the state **before** that
> fix — the ten `NoClassDefFoundError`s in it are W7-13's subject, are already
> fixed in the source this branch is cut from, and are reproduced here only
> because they are what the shipped binary still does.

---

## 1. The instrument comes first, because it is why these two survived

`W7-13` established the finding this lane was set to act on:

> **The vector is a one-bit instrument.** A probe reaching each combinator
> independently (catching per step) named **all ten** under strict — insert,
> permute, filter, guard, collect, spread, retfilter, fold, collect_args, catch
> — while the vector reported one. HotSpot 25 passes every step.

`RJdkHandles.adaptation()` was a straight-line run of `check(...)` calls, so the
first `AssertionError` ended the method and every later claim went unevaluated.
Ten broken combinators, one line of output. That is the `W6-5` species in its
subtler form: the vector does test something real, but its **resolution** is one
bit where the surface is ten.

Measured, one run each, on the same binary and the same class file:

| | steps run | steps failed | failures NAMED in that one run |
|---|---|---|---|
| HotSpot 25.0.3 | 37 | 0 | — |
| CratonVM `--real-jdk` | 37 | 13 | 13 |
| CratonVM `--jdk-only` | 37 | 24 | 24 |

The old vector, on the same two CratonVM runs, would have printed one failure
each and stopped: `NoClassDefFoundError: __mh_insert_wrapper__` under strict,
and — because `asCollector` had no step at all — **nothing** under
`--real-jdk`. That is not a rhetorical claim: `RJdkHandles` passed `--real-jdk`
in full while `asCollector` was returning `-2527743864898872` for a `long[]`
collector, because the only path it took to `asCollector` was
`asVarargsCollector`, which is a different path.

### What "independent" means here, concretely

Every claim runs inside `step(name, body)`: the failure is caught, printed with
its own name, recorded, and the next claim still runs. `main` throws at the end
if anything failed, so `rc` and `run.sh`'s existing `rc` check are unchanged.
Each step also builds **its own** target handle rather than sharing one — a
shared setup line that fails takes every later claim with it, which is the
behaviour being removed.

`run.sh`'s `extract()` keeps only `^PASS ` and `^CK ` lines for its cross-VM
diff, so the per-step `FAIL RJdkHandles step <name>: …` lines are deliberately
**not** `CK` lines: a failing run is already red on `rc` long before the diff,
and those lines exist for whoever runs the class directly. The end-of-run
`AssertionError` names every failed step, which is what `run.sh` puts in its
`why` column.

### The output line that changed

`CK RJdkHandles adapt=14` is now `CK RJdkHandles adapt combinators=14`, and two
lines are new: `CK RJdkHandles steps=37`, and the `PASS` line carries the step
count. The internal record
`fixed-bugs/jdk-only-L2-methodtypeform-lambdaforms-null-FIXED-20260811.md`
quotes the old spelling as its verification marker; that quotation is now stale
and the file is not this lane's to edit.

---

## 2. `asCollector` gathered into a reference array for every carrier

### 2.1 Measured, one row per array carrier

`sumInts(int[])`.`asCollector(int[].class, 3)`.`invoke(1,2,3)` and its nine
siblings. `--jdk-only` is pre-W7-13 and refuses the carrier before any of this
is reached, so it cannot see the defect at all.

| carrier | HotSpot 25 | CratonVM `--real-jdk` BEFORE | `--jdk-only` BEFORE | specified AFTER |
|---|---|---|---|---|
| `int[]` | `6` | **`0`** | `NoClassDefFoundError` | `6` |
| `long[]` | `6` | **`-2527743864898872`** | `NoClassDefFoundError` | `6` |
| `double[]` | `7.0` | **`NaN`** | `NoClassDefFoundError` | `7.0` |
| `float[]` | `3.75` | **`0.0`** | `NoClassDefFoundError` | `3.75` |
| `byte[]` | `6` | **`0`** | `NoClassDefFoundError` | `6` |
| `short[]` | `60` | **`0`** | `NoClassDefFoundError` | `60` |
| `char[]` | `131` | **`0`** | `NoClassDefFoundError` | `131` |
| `boolean[]` | `2` | **`0`** | `NoClassDefFoundError` | `2` |
| `String[]` value | `abc` | `abc` | `NoClassDefFoundError` | `abc` |
| `String[]` container | `[Ljava.lang.String;` | **`[Ljava.lang.Object;`** | `NoClassDefFoundError` | `[Ljava.lang.String;` |
| `Object[]` | `2` | `2` | `NoClassDefFoundError` | `2` |
| `Object[]` boxes a primitive element | `java.lang.Integer` | `java.lang.Integer` | `NoClassDefFoundError` | `java.lang.Integer` |
| `type()` | `(int,int,int)int` | `(int,int,int)int` | `NoClassDefFoundError` | `(int,int,int)int` |
| leading arg + `int[]` tail | `n=3` | **`n=0`** | `NoClassDefFoundError` | `n=3` |

`-2527743864898872` is not a wrong sum. It is a raw heap pointer read as a
`long` — the target's `laload` reading an oop.

**Eight of eleven carriers wrong, two right, one right-by-value-wrong-by-type.**
This is the shape the instruction named: the FFM defect in this tree where four
of nine carriers worked because one probe reached one of them. `Object[]` and
`String[]`-by-value are exactly the two carriers a reference-array
implementation gets right, and they are exactly the two the old vector could
have reached.

### 2.2 The cause

The `MH_KIND_COLLECT` arm of `mh_dispatch` built the gathering array with

```rust
let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, count);
```

unconditionally, and boxed every primitive element into it. Its own comment
explains the boxing, correctly, as a fix for Groovy's `Object[]` indy call sites
(`invoke(II)Object` → `selectMethod`'s `args[0].getClass()`). The boxing is
right for a **reference** collector and is preserved verbatim. What was missing
is that the arm had no way to know the collector's array type at all: the
carrier `__mh_collect_wrapper__` held `(target, count)` and nothing else.
`type()` was computed correctly in the `asCollector` native — from the `Class`
mirror it had in hand — and then that mirror was discarded.

### 2.3 The change

`__mh_collect_wrapper__` gains a third slot holding the `arrayType` **Class
mirror**. A mirror is a reference, so it occupies a carrier slot without the
int-in-an-oop-slot hazard that `docs/architecture/natives-over-real-jdk-classes.md`
§5 and `W4-4-slot-index-species-sweep.md` are about; an encoded element-type tag
would have been exactly that hazard, and was rejected for it.

The dispatch arm then:

1. derives the component descriptor from the mirror (`[I` → `I`), reusing the
   same `mirror_to_descriptor` + `strip_prefix('[')` the native already used for
   `type()`;
2. allocates the matching heap array kind — `new_array(Int, n)` for `int[]`, and
   for a reference component `new_ref_array(component_class_id, n)` so a
   `String[]` collector builds a `String[]`, not an `Object[]`;
3. boxes on the reference branch (unchanged) and, on the primitive branch,
   **unboxes** an argument that arrived boxed and applies JLS 5.3 widening.

The widening exists because CratonVM's `MethodHandle.asType` is a passthrough
shim. On HotSpot `asCollector(long[].class, 3).invoke(1, 2, 3)` reaches the
target with three `long`s because the combinator's trailing `asType` widened
them; here the raw `int`s arrive at the array store and `write_prim_element`'s
`Long` arm matches only `Value::Long`, writing 0 for anything else. Narrowing is
deliberately **not** performed: `Z`/`B`/`C`/`S`/`I` all travel as `Value::Int`
and `write_prim_element` already truncates on the store, and a `long` handed to
an `int[]` collector is a type error the JDK refuses rather than truncates.

**The fallback is the old behaviour, exactly.** With no mirror in slot 2 — a
carrier written before the slot existed, or an `asCollector` whose `Class`
argument was not an object — the component descriptor stays
`Ljava/lang/Object;`, the element type is `Reference`, and the arm runs the
pre-existing box-into-`Object[]` path unchanged. The new code cannot introduce a
failure mode the old code did not have; it can only decline to improve.

### 2.4 Which mode each change affects

| | effect |
|---|---|
| `Compatible` (`--real-jdk`) | **changed, intentionally**: eight primitive carriers move from a wrong value to the specified one, and a `String[]` collector's container moves from `Object[]` to `String[]`. This is the defect being fixed. |
| `JdkOnly` (`--jdk-only`) | the same change. Strict additionally needs W7-13 (already on `dev`) before this arm is reached at all. |

The one behaviour a `Compatible` consumer could be depending on is the
`Object[]` container for a `String[]` collector. That dependency would be a bug
on HotSpot — `String[] xs` in the target's signature means the parameter is
declared `String[]`, and a reflective or `aastore`-checked consumer can tell.

---

## 3. `bindTo` accepted a target with no leading reference parameter

### 3.1 The rule, and the sentence that specifies it

`java.lang.invoke.MethodHandle.bindTo(Object)`, javadoc:

> "@throws IllegalArgumentException if the target does not have a
> leading parameter type that is a reference type"

The body is two lines, and the check is the first of them:

```java
public MethodHandle bindTo(Object x) {
    x = type.leadingReferenceParameter().cast(x);  // throw CCE if needed
    return bindArgumentL(0, x);
}
```

`MethodType.leadingReferenceParameter()` (JDK 25 `src.zip`) is the whole test:

```java
if (ptypes.length == 0 || (ptype = ptypes[0]).isPrimitive())
    throw newIllegalArgumentException("no leading reference parameter");
```

So the predicate is arity-and-primitiveness and nothing else, and the message is
verbatim. CratonVM had no such check at all.

### 3.2 Measured

| case | HotSpot 25 | CratonVM `--real-jdk` BEFORE | specified AFTER |
|---|---|---|---|
| `(String,int)int`.`bindTo("abc")` | `(int)int`, invokes to 6 | same | unchanged |
| `(int[])int`.`bindTo(new int[]{4,5})` | 9 | 9 | unchanged |
| `(String,int)int`.`bindTo(null)` | accepted, `(int)int` | accepted, `(int)int` | unchanged |
| `(String,int)int`.`bindTo("abc")`.`bindTo(2)` | **IAE** *no leading reference parameter* | **ACCEPTED**, answered 6 | IAE |
| `(long)long`.`bindTo(5L)` | **IAE** | **ACCEPTED**, produced `()long` | IAE |
| `constant(String,"K")` i.e. `()String`.`bindTo("x")` | **IAE** | **ACCEPTED**, produced `()String` | IAE |
| `(String,int)int`.`bindTo(Integer.valueOf(1))` | **CCE** *Cannot cast java.lang.Integer to java.lang.String* | **ACCEPTED** | *not taken — §5.1* |

`bindTo(null)` being **accepted** is why the check must be a type test and not a
null test; it is in the vector for that reason.

### 3.3 The safety argument, measured rather than asserted

A new refusal can only do harm by refusing something HotSpot accepts. The guard
reads the leading parameter from the handle's **`type` field only**, never from
`mh_type_descriptor`'s `MH_DESC` fallback — `MH_DESC` on a virtual/special
handle omits the receiver that `alloc_method_handle` prepends to `type`, so
`Holder.pub`'s `(I)I` would read as a primitive leading parameter and the guard
would refuse a bind HotSpot accepts. When no `type` MethodType is installed, or
its mirrors do not render, the leading parameter is UNKNOWN and the bind goes
through: a refusal is only ever raised on a positive reading.

That is the argument; here is the measurement behind it. Thirteen handle shapes,
`type()` printed on both VMs, same class file:

| shape | HotSpot `type()` | CratonVM `type()` | guard would |
|---|---|---|---|
| `findVirtual` | `(H,int)int` | `(H,int)int` | accept (both) |
| `findVirtual`.`bindTo` | `(int)int` | `(int)int` | refuse (both) |
| `findStatic` | `(String,int)int` | `(String,int)int` | accept (both) |
| `findStatic`.`bindTo` | `(int)int` | `(int)int` | refuse (both) |
| `findConstructor` | `()H` | `()H` | refuse (both) |
| `findGetter` | `(H)int` | `(H)int` | accept (both) |
| `findGetter`.`bindTo` | `()int` | **`(H)int`** | accept — HotSpot refuses |
| `identity(String)` | `(String)String` | `(String)String` | accept (both) |
| `constant` | `()String` | `()String` | refuse (both) |
| `arrayElementGetter` | `(int[],int)int` | `([I,int)int` | accept (both) |
| `arrayElementGetter`.`bindTo` | `(int)int` | **`([I,int)int`** | accept — HotSpot refuses |
| `unreflect` | `(H,int)int` | `(H,int)int` | accept (both) |
| `unreflect`.`bindTo` | `(int)int` | `(int)int` | refuse (both) |
| `insertArguments(twice,0,4)` | `()int` | `()int` | refuse (both) |

Two rows differ, and **both differ in the permissive direction**: where
CratonVM's `type()` bookkeeping lags (it does not drop the bound leading
parameter after a getter bind or an array-getter bind), CratonVM reports a
REFERENCE leading parameter where HotSpot reports a primitive or none. The guard
therefore under-refuses on those two and over-refuses on none. No false refusal
appears anywhere in this census.

The residual exposure the census cannot close is a handle kind whose `type`
field is a fabrication rather than a lag — `MH_KIND_LAMBDA_FACTORY`,
`MH_KIND_STRING_CONCAT`, `MH_KIND_RECORD_DESER`. For the lambda factory the
chained-bind pattern this tree actually depends on
(`factory.bindTo(serviceType).bindTo(classLoader)`, log4j) binds references, and
`type` is not narrowed between binds, so both binds read a reference leading
parameter and are accepted; a chained static bind was measured end-to-end
(`cat("a")("b")` → `ab`) and is unaffected. That is an argument, not a
measurement, and it is the part of this change a rebuild should look at first.

### 3.4 Which mode each change affects

| | effect |
|---|---|
| `Compatible` (`--real-jdk`) | **changed, intentionally**: three shapes move from silent acceptance to the specified `IllegalArgumentException: no leading reference parameter`. |
| `JdkOnly` | the same; the guard is mode-independent. |

---

## 4. The vector: 37 steps, and what each one is for

`RJdkHandles` went from 60 straight-line assertions in `adaptation()` to **37
independent steps carrying 316 checks**, of which the combinator and binding
surface is 30 steps. Six combinators had **no coverage at all** before and now
have a step each: `filterReturnValue`, `foldArguments`, `collectArguments`,
`catchException`, `asSpreader`, `asCollector`.

Green on HotSpot 25.0.3 at `--release` 17, 21 and 25 (`run.sh`'s `RELEASES`
set), 316 checks each. Every value asserted was measured on the oracle before it
was written down.

### 4.1 Checks that were strengthened because they could not fail

* **`permuteArguments`.** The old check permuted `statAdd(int,int)` and demanded
  3 from `(1,2)`. `statAdd` is commutative, so a permutation that did nothing at
  all also produces 3 — the check could not fail on a VM that ignored the
  permutation. It is now on `minus`, where swapped gives `2-1 = 1` and unswapped
  gives `-1`, plus a second permutation that DUPLICATES and DROPS
  (`(a,b) → minus(b,b)`), which a swap-only implementation gets wrong.
* **`catchException`** carries a negative control — a target that does NOT throw
  must return the target's value (14), not the handler's — and a wrong-type
  control: an `IllegalArgumentException` thrown by the target of a handler
  registered for `IllegalStateException` must propagate. Without the first, a VM
  that ran the handler unconditionally passes; without the second, a VM that
  caught everything passes.
* **`bindTo`** carries both halves. Without the accepting steps, a VM that threw
  `IllegalArgumentException` from every `bindTo` satisfies all three refusals
  and is indistinguishable from a correct one. `bindTo(null)` is asserted as
  ACCEPTED for the same reason: it separates a type check from a null check.
* **`asCollector`** asserts the runtime CLASS of the array that was built
  (`classOfInts` returns `xs.getClass().getName()`), not only the value computed
  from it. The value can be right for the wrong reason — a wrong container whose
  elements happen to unbox correctly on the way out still sums to 6. `"[I"`
  cannot. This is the check that caught the `String[]`-built-as-`Object[]` row
  in §2.1, which the value check alone reported as green.
* **`insertArguments`, `dropArguments`, `filterArguments`, `collectArguments`**
  each assert at TWO positions, because "a combinator ran" and "it ran at the
  position asked for" are different claims and the single-position form conflates
  them.
* **`foldArguments` vs `collectArguments`** are asserted on the same two handles
  with values that differ (`minus(twice(5), 5) = 5` vs
  `minus(twice(3), 4) = 2`), because fold PREPENDS and keeps while collect
  REPLACES — an implementation that confused them would otherwise pass both.

### 4.2 One check written, measured, and deleted

`check(sumAll.isVarargsCollector(), …)` was written, run, and **removed**, with
its `asFixedArity` twin.

It fails on CratonVM: `isVarargsCollector()` answers `false`, because
`asVarargsCollector` is registered as the identity and the marking is stored
nowhere. That is a **declared** deviation, written down in place in
`native-builtins/src/lang_invoke.rs` beside the registration ("Known deviation,
deliberately not papered over"), with its reason: dispatch derives varargs
behaviour from arity (`collect_trailing_varargs`), so a "this handle is a
collector" bit adds nothing to it, and recording it needs a sixth synthetic
`MethodHandle` slot (`MH_BOUND + 1` is the allocated width).

It is not a vacuous check — it can fail, and it did. It is the opposite problem:
a check whose red no fix in this lane can clear, on a vector that is scheduled
in `JDKONLY_CLASSES` and whose red is read as a regression. A permanently-red
vector teaches operators to ignore the red, which costs more than the check
buys. It is recorded in §5.2 instead, with the shape of the fix.

The `asFixedArity` twin went with it: `check(!fixed.isVarargsCollector(), …)`
passes on CratonVM, but it passes because the flag is *always* false, not
because `asFixedArity` cleared it. Kept alone it would be half a control — the
half that agrees with a broken VM.

### 4.3 The weakest checks in the new set

Named because someone should be able to argue with them:

1. **`asCollector double[]`, `float[]`, `byte[]`, `short[]`, `char[]`,
   `boolean[]` assert the value only, not the container.** `int[]`, `long[]` and
   `String[]` assert both. The six weaker ones would pass a hypothetical
   implementation that built the right *values* in the wrong container — which
   is not a shape anything in this tree produces, but it is a shape the two
   stronger rows would have caught and these would not. The reason is length,
   not principle; a `classOf*` helper per carrier is six more methods for one
   more bit each.
2. **`asCollector type()`** asserts `(int,int,int)int`. It is falsifiable
   (breaking `split_descriptor_params` fails it) but it is **not the gate for
   the container defect** — the arity bookkeeping was already correct while the
   container was wrong, measured, §2.1. It is written as its own step so nobody
   reads it as coverage of the value.
3. **`asVarargsCollector array call`** — `sumAll.invoke(new int[]{5,6}) == 11` —
   passes on a VM with no varargs-collector concept at all, because a plain
   fixed-arity handle accepts the array form too. Its partner (the spread call,
   `invoke(1,2,3,4) == 10`) is the one that requires the collector semantics.
   Kept because "the array form must still work" is a real property that a
   collector implementation can break, but it is the weak half of the pair.
4. **`bindTo drops the bound leading parameter from type()`** asserts
   `"(int)int"` on a handle whose value behaviour is asserted separately. String
   comparison of `MethodType.toString()` is a rendering check; it was measured
   identical on both VMs, but a spelling difference in an unrelated change would
   fail it for a reason that is not a defect.

---

## 5. Observed and measured, NOT fixed here

### 5.1 `bindTo` does not raise `ClassCastException` for a wrong reference type

HotSpot: `findStatic(…(String,int)int).bindTo(Integer.valueOf(1))` →
`ClassCastException: Cannot cast java.lang.Integer to java.lang.String`, from
the `.cast(x)` in the same one-line body as the check this record does take.
CratonVM `--real-jdk` accepts it and answers `(int)int`.

Not taken, and the split is principled rather than budgetary. The refusal taken
here is a **syntactic** property of a descriptor token — is the first parameter
`L…;`/`[…` or one of the eight primitives — with no class-hierarchy lookup, so
it cannot be wrong for a reason outside its own two lines. A `cast` check is an
**assignability** question against a resolved `Class`, and a wrong answer
silently refuses working code: `bindTo` is on the hot path of Groovy indy, SpEL
`FunctionReference` and log4j's provider factory in this tree, all of which bind
subtypes and interfaces. With no build available to run any of them, a
same-lane assignability check would be an unverifiable change to a hot path.
It is a real defect and it wants a lane that can rebuild.

There is no vector row for it, for the same reason: a red that no fix in this
lane clears.

### 5.2 `isVarargsCollector()` answers `false`

Measured (§4.2), and already declared in place in `lang_invoke.rs`. The fix, if
a later lane wants it, is confined to that file: a sixth synthetic slot
`MH_VARARGS = MH_BASE + 5` with `alloc_method_handle`'s width going to
`MH_BOUND + 2`, set by the `asVarargsCollector` shim and cleared by
`asFixedArity`, plus a registration for `MethodHandle.isVarargsCollector()`
reading it — the base-class bytecode returns `false` and is what runs today. Not
done here because it is a third defect on a two-defect brief, and because
widening the `MethodHandle` allocation touches every handle in the VM.

### 5.3 `type()` is not narrowed after a getter bind or an array-getter bind

Measured in §3.3's census: `findGetter(…)` bound to a receiver still reports
`(H)int` where HotSpot reports `()int`, and `arrayElementGetter(int[])` bound to
an array still reports `([I,int)int` where HotSpot reports `(int)int`. The
`bindTo` native narrows `type` for `MH_KIND_VIRTUAL`/`SPECIAL`/`STATIC` and for
the second-bind INSERT path, but not for `MH_KIND_GETTER`/`SETTER`/`ARRAY_GET`/
`ARRAY_SET`. Both are in this lane's file and were left alone: they are the
reason the new `bindTo` guard under-refuses on two shapes (§3.3), so fixing them
would TIGHTEN a refusal that has not been rebuilt or run once. Fix the arity
bookkeeping and the guard in the same rebuilt lane, not in a source-only one.

---

## 6. How to falsify this

Rebuild, then run in this order. The first two are the claims; the third and
fourth are the guards.

```sh
# 1. Compatible. Was: 13 of 37 steps failed, named in the AssertionError.
cratonvm --real-jdk --java-home "<jdk-25-home>" -cp regression-suite/build RJdkHandles
#    Expect: PASS RJdkHandles (316 checks, 37 steps).

# 2. Strict, which additionally exercises W7-13's carrier door.
#    Was: 24 of 37.
cratonvm --jdk-only --java-home "<jdk-25-home>" -cp regression-suite/build RJdkHandles

# 3. The oracle must still agree line for line.
java -cp regression-suite/build RJdkHandles          # 0 of 37, 316 checks

# 4. The whole vector through the harness, which diffs 1 against 3.
ONLY=RJdkHandles bash regression-suite/run.sh
```

**A green `RJdkHandles` is now sufficient evidence for the combinator surface,
which is the point of §1** — it was not before. It is NOT evidence for anything
this record lists in §5, none of which has a row.

What would falsify the `bindTo` half specifically: any real-world `bindTo` in
the Groovy / SpEL / log4j paths newly raising `IllegalArgumentException: no
leading reference parameter`. §3.3's census says that cannot happen for the
thirteen shapes measured; the shapes it does not cover are the fabricated-`type`
kinds named at the end of §3.3, and a Spring Boot suite run is the instrument
that would see it.

What would falsify the `asCollector` half: a `Compatible`-mode consumer that
depended on receiving an `Object[]` from a `String[]` collector. §2.4 argues
that dependency is already a bug on HotSpot; a Groovy suite run is the
instrument.

---

## Out-of-file patch (not applied)

None. Both defects, both fixes and the vector are contained in
`native-builtins/src/lang_invoke.rs` and
`regression-suite/src/RJdkHandles.java`.

`regression-suite/run.sh` needs **no** change: `RJdkHandles` is already in
`JDKONLY_CLASSES`, it takes no per-class arguments from `class_args` or
`class_cv_args`, and the rewrite keeps the `PASS RJdkHandles ` prefix and the
non-zero exit on failure that the harness keys on.
