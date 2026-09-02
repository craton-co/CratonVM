# W7-19 — `asCollector` built the wrong container, `bindTo` never refused, and the vector could only report one thing at a time

**Status: DIAGNOSED and FIXED IN SOURCE 2026-08-11. NOT REBUILT.** Every number
below was measured by running the already-built binary at
`C:/craton/CratonVM/target/release/cratonvm.exe` (mtime 19:41:42) and HotSpot
25.0.3 (`Eclipse Adoptium jdk-25.0.3.9-hotspot`). Nothing here claims a source
change works; it claims what was measured before it.

> **VERIFIED AGAINST A BINARY 2026-09-01.** The status above was written by a
> lane that could not build or run Rust, and it stood for roughly three weeks.
> Run on a release binary of `dev`, against HotSpot 25.0.4+7 on the same host:
>
> ```text
> RJdkHandles
>   HotSpot          PASS RJdkHandles (331 checks, 40 steps)
>   CratonVM compat  PASS RJdkHandles (331 checks, 40 steps)      0 differing lines
>   CratonVM strict  PASS RJdkHandles (331 checks, 40 steps)      0 differing lines
> ```
>
> Byte-identical output in BOTH modes, so the source work this record describes
> does what it claimed on a real binary.
>
> `CK RJdkHandles steps=` and the step count on the `PASS` line -- both
> introduced by this record's third pass -- are present and agree with HotSpot.
>
> **The predicted COUNT is superseded: this record expected `RJdkHandles` at 37 steps / 116 checks.** Other
> lanes added to the shared vector across the three weeks. A count written as an
> expectation ages into a falsehood the moment a shared vector grows -- what
> survives verification is the ASSERTIONS, and those match. Do not re-derive a
> defect from a count that merely moved.


Branch: `fix/methodhandles-compatible-residuals-20260811`.
Files changed: `native-builtins/src/lang_invoke.rs`,
`regression-suite/src/RJdkHandles.java`, and this record. Nothing else.

> **THIRD PASS 2026-08-12 (lane A16) — VERIFICATION ONLY, still not rebuilt.**
> Every source claim in this record was re-read against today's tree and all of
> them hold; the one out-of-file line (§5.2.3 / "Out-of-file patch") is **still
> not applied**, and the check/step arithmetic in §5's preamble is now measured
> rather than argued. See **§7** at the end.

> **SECOND PASS 2026-08-12, also NOT REBUILT.** Two of the three residuals this
> record filed in §5 are now fixed in source — `isVarargsCollector()` (§5.2) and
> the getter / array-getter `type()` narrowing (§5.3) — and §5.1 is declined with
> a mechanism instead of a budget (§5.1.1). Same two files, plus **one** new
> out-of-file line reported at the bottom. `RJdkHandles` goes from 37 steps / 116
> checks to **40 steps / 128 checks**; §5's preamble reconciles that against the
> "316" this record printed and the "54" the index prints, both of which are
> wrong.

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

> **REINSTATED 2026-08-12, because the deviation it was red for is now fixed —
> §5.2. Both halves are back: the marking on `asVarargsCollector`'s result, with
> a FRESH-handle negative control, and the `asFixedArity` twin, which is a real
> control now that the flag can be `true`. The reasoning below is why they were
> right to be absent while the flag was hardcoded, and it is the reasoning a
> future lane should apply to §5.1's missing row.**

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

> **REVISITED 2026-08-12. Two of the three are now FIXED IN SOURCE; §5.1 is
> declined with a named mechanism instead of a budget.** Nothing below was
> rebuilt or run — the verdicts are source verdicts and the fixture assertions
> are unverified against a binary.
>
> | residual | 2026-08-12 |
> |---|---|
> | §5.1 `bindTo` does not raise `ClassCastException` | **NOT TAKEN, and the reason is now a mechanism rather than a budget — see §5.1.1.** A same-lane fix would raise FALSE `ClassCastException`s. |
> | §5.2 `isVarargsCollector()` answers `false` | **FIXED IN SOURCE.** `MH_VARARGS` = `MH_BASE + 5`, both allocators widened to `MH_VARARGS + 1`, an `isVarargsCollector` native reads it width-guarded. §5.2.1. |
> | §5.3 `type()` is not narrowed after a getter / array-getter bind | **FIXED IN SOURCE.** The leading-parameter drop now covers `MH_KIND_GETTER`/`SETTER`/`ARRAY_GET`/`ARRAY_SET`. §5.3.1. |
>
> Vector: **`RJdkHandles` moves from 37 steps / 116 checks to 40 steps / 128
> checks.** Three counts for this file are in circulation and two of them are
> wrong, so here is the arithmetic rather than a number to trust.
>
> §4 above says "37 independent steps carrying 316 checks". The 37 is
> right and is still verifiable by counting `step(` call sites. **The 316 is
> not**: the file has 132 `check(` call sites, four of which are
> `check(false, …)` inside a `try` that a correct VM never reaches, so a green
> run counted 116 before this pass. README §2.6's *"expecting `PASS RJdkHandles
> (54 checks)`"* is a third number and is older still — it predates the
> step-based rewrite. Whoever rebuilds should trust the binary's own
> `CK RJdkHandles checks=` line over all three, and correct whichever is wrong.

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

#### 5.1.1 STILL NOT TAKEN 2026-08-12 — and the reason is now a mechanism

The 2026-08-11 reason was *"an unverifiable change to a hot path"*, which is a
budget argument and reads as *"do it when there is time"*. There is a stronger
one, and it names the predicate.

`NativeContext` offers exactly one assignability primitive:
`is_subclass(child: ClassId, parent: ClassId) -> bool`. It does walk interfaces
(`classloading/src/class.rs::is_subclass_of` recurses through the superclass
chain **and** the interface list, with a visited set for the JDK's diamond
shapes), and it routes synthetic lambda-proxy `ClassId`s ≥ `0x8000_0000` through
`lambda_proxy_satisfies`. So on the face of it, it is the right predicate.

It is not, and the counter-example is a species this directory already has a
record for: **a fabricated stand-in declares no interfaces, so every type test
against an interface fails on it.** A `cast` check built on `is_subclass` would
therefore answer "not assignable" — i.e. raise `ClassCastException` — for a
receiver HotSpot casts without complaint, and `bindTo` is on the Groovy-indy /
SpEL-`FunctionReference` / log4j-provider-factory path in this tree, all of which
bind interfaces. The same shape reaches it from the other side:
`class_id_by_name` has no initiating loader, so a leading-parameter class name
that resolves to a different-loader twin gives the same false negative.

That inverts the cost. The residual today is a **wrong answer** on a case nothing
in the corpus exercises; the fix as available would be a **refusal of working
code** on three paths that are exercised constantly — which is the direction this
whole directory is about not going. `aastore_element_assignable` is the shape a
correct fix would need: a predicate whose contract is *"must never produce a
false refusal"*, with the hedges (interface component, `$Proxy`/`AnnotationProxy`
value, synthetic class id, same-named-other-loader component) written into it and
paid for by regressions. There is no `bindTo`-shaped equivalent on
`NativeContext`, and adding one is a `native-api` change, not a `lang_invoke` one.

**So: still no vector row, and the blocker is now stated as a capability gap.**
What would unblock it: either a `cast`-shaped `NativeContext` predicate with
`aastore_element_assignable`'s never-false-refuse contract, or a lane that can
run the Groovy, SpEL and log4j slices against a build.

An in-file pointer to this section now sits beside the guard in
`native-builtins/src/lang_invoke.rs`, so the next reader of the two-line
syntactic test learns why it is only two lines.

### 5.2 `isVarargsCollector()` answers `false`

> **FIXED IN SOURCE 2026-08-12. NOT REBUILT.** Measured red (§4.2) on the
> shipped binary; the fix below is a source change with no run behind it.

Measured (§4.2), and formerly declared in place in `lang_invoke.rs`. The
prescription this record left was: *a sixth synthetic slot
`MH_VARARGS = MH_BASE + 5` with `alloc_method_handle`'s width going to
`MH_BOUND + 2`, set by the `asVarargsCollector` shim and cleared by
`asFixedArity`, plus a registration for `MethodHandle.isVarargsCollector()`
reading it.* That is what landed, with one correction the prescription needed.

#### 5.2.1 The correction: the read had to be width-guarded, and the record did not say so

*"Widening the `MethodHandle` allocation touches every handle in the VM"* is
**false, and falsely reassuring in the dangerous direction.** Widening
`alloc_method_handle` touches only the handles `alloc_method_handle` mints. Three
other populations exist, and a bare `get_field(mh, MH_VARARGS)` is an
out-of-bounds read on all three:

* `MethodHandles.empty` and `MethodHandles.zero` allocate **17** slots
  (`lang_invoke.rs`, two literal `17`s);
* `native-builtins/src/panama.rs` uses a compact layout whose field 0 is the
  native function address, which `asType`'s own comment names;
* `classloader.rs`'s `alloc_method_handle` allocates 21 with a **different field
  order**. Stated here in full 2026-08-12, because the record that found it —
  the retired `W7-13-strict-mh-insert-wrapper` write-up — has left this
  directory and this is now the live home for the row.
  `classloader.rs:9084-9090` declares `MH_BASE = 16` with `MH_KIND`(16),
  `MH_TARGET_CLASS`(17), `MH_NAME`(18), `MH_TYPE`(19), `MH_CLASS_ID`(20), and a
  comment claiming it *"matches the layout used by
  `lang_invoke::alloc_method_handle`"* — it does not: `lang_invoke`'s map is
  `MH_CLASS`(16), `MH_NAME`(17), `MH_DESC`(18), `MH_KIND`(19), `MH_BOUND`(20).
  Only the base matches, so slot 16 holds a `String` reference in one map and an
  `Int` in the other, and slot 20 a reference in one and a `ClassId` `Int` in
  the other. Its only allocator is `classloader::alloc_method_handle`, whose only
  callers are `lk_unreflect`/`lk_unreflect_special`, and §3.3's census answers
  which of the two `unreflect` registrations wins — `type()` reads `(H,int)int`,
  which is `lang_invoke`'s shape, so **the disagreeing map is very probably
  UNREACHED**. That is why it has never corrupted anything, and it is exactly
  the shape that stops being harmless the moment registrar order changes. It
  wants a `--dump-native-registry` diff and then a deletion, not a repair.

That is precisely the defect `MH_KIND_ARRAY_GET`'s doc comment records — `invokeExact`
read `MH_DESC`(18) and `MH_KIND`(19) off the end of a 17-slot object, and the GC
guard logged exactly those two indices with `num_slots=17`. So the marking is
read and written through

```rust
fn mh_is_varargs_collector(ctx: &dyn NativeContext, mh: ObjectRef) -> bool {
    ctx.object_num_fields(mh) > MH_VARARGS && matches!(ctx.get_field(mh, MH_VARARGS), Value::Int(1))
}
```

and a `mh_set_varargs_collector` with the same guard — the idiom
`lang_stackwalker.rs:1158` already uses (`object_num_fields(this) > P59_SF_DECL_MIRROR`).
A handle too narrow to carry the marking is not one, which is the pre-fix answer
for every handle: **the fallback is the old behaviour exactly.**
`alloc_method_handle` and `alloc_string_concat_method_handle` also write
`Int(0)` explicitly, so the read never has to interpret an unwritten slot.

#### 5.2.2 The declared deviation that survives, and why it is the identity shim's, not the marking's

HotSpot's `asVarargsCollector` returns a **new** handle and leaves the receiver
fixed-arity. CratonVM's is the identity — deliberately, and
`register_method_handle_combinator_extras_bridge` explains why at length: the
real bytecode wraps the receiver in a `DelegatingMethodHandle` whose constructor
demands a `LambdaForm` reinvoker that would have to re-enter a `MH_KIND_*` shim
through `invokeBasic`. So there is only ONE handle here, and consequently:

* `h.asVarargsCollector(t).isVarargsCollector()` → `true` (HotSpot: `true`);
* `h.isVarargsCollector()` afterwards → `true` (HotSpot: `false`);
* `h.asFixedArity()` clears the bit on that same object (HotSpot: leaves `h`
  marked and returns an unmarked copy).

Minting a copy instead would have to reproduce all six synthetic slots plus the
`type` field of an arbitrary handle kind, in a shim two other subsystems reach.
Dispatch is unaffected either way — `collect_trailing_varargs` derives varargs
behaviour from arity and never reads this bit — so the copy buys only the
aliasing, at the price of a new allocation on a shimmed path. **The vector
asserts the marking on the RESULT of `asVarargsCollector` and takes its negative
control from a FRESH handle, precisely so it does not assert the deviation.**

#### 5.2.3 One reachability caveat, stated rather than assumed

`MethodHandle.isVarargsCollector()` is **concrete** in the real JDK: the base
class returns `false` and `MethodHandleImpl$AsVarargsCollector` overrides it. Per
`docs/architecture/natives-over-real-jdk-classes.md` §1 a registered native beats
real bytecode on the cold interpreter paths with no list consulted, and this
block's ambient `NativeKind` is `Bridge`, which `--jdk-only` keeps — the sibling
`asVarargsCollector`/`asFixedArity` shims in the same block are green under
`--jdk-only` today, which is the empirical form of that argument. The warm,
cached, reflective and JIT paths reinstate the preference from `vm_exec.rs`'s
`check_override` mirror, and that mirror lists `asCollector`, `asSpreader`,
`asVarargsCollector` and `asFixedArity` but **not** `isVarargsCollector`. Adding
it is a one-line out-of-file edit and is reported as such; until it lands, a
JIT-warm caller may read the base class's `false`. That is the answer the entire
VM gave before this change, so the worst case is the old behaviour on one path,
not a new wrong answer — but it is a cold/warm split of the cached-twin species
and should not be left indefinitely.

### 5.3 `type()` is not narrowed after a getter bind or an array-getter bind

> **FIXED IN SOURCE 2026-08-12. NOT REBUILT.**

Measured in §3.3's census: `findGetter(…)` bound to a receiver still reports
`(H)int` where HotSpot reports `()int`, and `arrayElementGetter(int[])` bound to
an array still reports `([I,int)int` where HotSpot reports `(int)int`. The
`bindTo` native narrowed `type` for `MH_KIND_VIRTUAL`/`SPECIAL`/`STATIC` and for
the second-bind INSERT path, but not for `MH_KIND_GETTER`/`SETTER`/`ARRAY_GET`/
`ARRAY_SET`.

#### 5.3.1 The change, and why the tightening it causes is the point

The `MH_KIND_STATIC` arm's body is already generic — read `type`, drop
parameter 0, rebuild — so the four accessor kinds were added to its condition and
the body is untouched. They take the same drop for the same reason: a getter's
`type` is its raw descriptor `(LH;)I`, because `alloc_method_handle` prepends a
receiver only for VIRTUAL/SPECIAL, so parameter 0 IS the value `bindTo` just
captured, exactly as for STATIC. Static getters and setters (`()I`, `(I)V`) never
reach the arm: the guard at the top of the native already refuses a zero-arity or
primitive-leading target, which is what HotSpot does too.

This record's 2026-08-11 reason for leaving it — *"fixing them would TIGHTEN a
refusal that has not been rebuilt or run once"* — was the right caution and is
answered rather than ignored. §3.3's census found exactly two rows out of
thirteen where CratonVM's `type()` lagged, and **both lagged in the permissive
direction**: CratonVM reported a REFERENCE leading parameter where HotSpot
reported a primitive or none, so the guard under-refused on two shapes and
over-refused on none. Narrowing them makes the guard refuse those two — which is
what HotSpot does. And the behaviour it replaces is worse than a wrong `type()`:
a second `bindTo` on a bound getter fell through to the allocation below, minted
a fresh handle and **overwrote `MH_BOUND`**, silently dropping the first
capture — the same defect the INSERT second-bind path exists to prevent for
static/virtual/special handles. So the tightening converts a silent wrong answer
into HotSpot's `IllegalArgumentException`.

The `type()` narrowing changes no dispatch: `mh_dispatch` keys the accessor arms
off `MH_DESC`, which is untouched, and takes the receiver/array from `MH_BOUND`
(`MH_KIND_GETTER`'s `let receiver = match bound { … }`).

#### 5.3.2 Three new steps, and what each can fail on

`RJdkHandles.binding()` gains three steps / twelve of the new checks:

* **`bindTo narrows type() after a getter bind`** — the unbound arity is 1, the
  bound arity is 0, the return type survives, and the bound handle reads the
  bound receiver's field. Asserted through `parameterCount`/`returnType`, not
  `MethodType.toString()`, because the arity is the claim and §4.3 item 4 already
  names string rendering as the weak form.
* **`bindTo narrows type() after an array-element getter bind`** — the same
  three plus `parameterType(0) == int.class`, which is what distinguishes
  "dropped a parameter" from "dropped the wrong one".
* **`bindTo refuses a second bind on a bound getter`** — the tightening, asserted
  directly rather than left as a consequence.

The two `invoke` assertions are the ones a rebuild should look at first: they are
the only claims here that exercise a bound-accessor **dispatch** rather than its
bookkeeping, and this lane could not run them. If either goes red, the step names
itself and the bookkeeping claims beside it still report independently — which is
what §1 is for.

---

## 6. How to falsify this

Rebuild, then run in this order. The first two are the claims; the third and
fourth are the guards.

```sh
# 0. RECOMPILE FIRST. `regression-suite/build` is a gitignored artifact
#    directory that survives a branch switch, so a direct `-cp
#    regression-suite/build` run will otherwise execute the PREVIOUS
#    RJdkHandles.class and report the old vector's one-bit result as if it
#    were this one's. (run.sh does its own `rm -rf $BUILD`; a hand-run does
#    not.)
javac -d regression-suite/build regression-suite/src/RJdkHandles.java

# 1. Compatible. Was: 13 of 37 steps failed, named in the AssertionError.
cratonvm --real-jdk --java-home "<jdk-25-home>" -cp regression-suite/build RJdkHandles
#    Expect: PASS RJdkHandles (128 checks, 40 steps) after the 2026-08-12 pass.
#    If the oracle disagrees, the oracle is right and this number is stale — the
#    "316" this line used to print is not reproducible from the file (§5 preamble).

# 2. Strict, which additionally exercises W7-13's carrier door.
#    Was: 24 of 37.
cratonvm --jdk-only --java-home "<jdk-25-home>" -cp regression-suite/build RJdkHandles

# 3. The oracle must still agree line for line.
java -cp regression-suite/build RJdkHandles          # 0 of 40, 128 checks

# 4. The whole vector through the harness, which diffs 1 against 3.
ONLY=RJdkHandles bash regression-suite/run.sh
```

**A green `RJdkHandles` is now sufficient evidence for the combinator surface,
which is the point of §1** — it was not before. **After the 2026-08-12 pass it is
also evidence for §5.2 and §5.3, which now have rows: three new checks for the
varargs marking and its two controls, and nine for the accessor-bind narrowing —
twelve in all, taking a green run from 116 checks to 128 and 37 steps to 40.** It
is still NOT evidence for §5.1, which deliberately has none — §5.1.1 says why,
and the reason is now that the fix as available would raise a FALSE refusal, not
that nobody had time.

**Five of the twelve fail on the old behaviour; the rest are controls, and that
split is deliberate.** Falsified by the old behaviour:
`sumAll.isVarargsCollector()` (answered `false`), the bound getter's
`parameterCount() == 0` and the bound array getter's `parameterCount() == 1` and
`parameterType(0) == int.class` (all three read the un-narrowed type), and the
second-bind refusal (was accepted). The other seven are controls or corroboration:
the FRESH-handle and `asFixedArity` negatives — without which a VM answering
`true` unconditionally satisfies the marking check — the two unbound arities and
the return type, and the two `invoke` reads. **The two `invoke` reads may or may
not have passed before this change** (the un-narrowed `type` carried an extra
parameter that some `invoke` arity paths consult) and were not measured either
way; they are here to catch a narrowing that breaks the dispatch, not to
demonstrate the defect.

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

The original two defects, both fixes and the vector are contained in
`native-builtins/src/lang_invoke.rs` and
`regression-suite/src/RJdkHandles.java`. **The 2026-08-12 §5.2 fix adds ONE
out-of-file line**, and it is a completeness edit rather than a correctness one:
the warm/cached/reflective/JIT preference mirror in
`vm/src/vm/vm_exec.rs::invoke_on_class_shared_inner` lists the sibling shims and
not the new reader. Present:

```rust
                        || (class_name == "java/lang/invoke/MethodHandle"
                            && matches!(
                                method_name,
                                "asCollector"
                                    | "asSpreader"
                                    | "asVarargsCollector"
                                    | "asFixedArity"
                            ))
```

Wanted — one added arm, nothing else in the block touched:

```rust
                        || (class_name == "java/lang/invoke/MethodHandle"
                            && matches!(
                                method_name,
                                "asCollector"
                                    | "asSpreader"
                                    | "asVarargsCollector"
                                    | "asFixedArity"
                                    | "isVarargsCollector"
                            ))
```

Without it the `isVarargsCollector` native still wins on the cold interpreter
paths — which is where the vector runs — and a JIT-warm caller falls back to the
base class's `return false`, i.e. to the behaviour that preceded this fix. So the
line is not load-bearing for the vector; leaving it out leaves a cold/warm split
that a later reader will read as a flake. `interpreter/native_override.rs`'s
`force_native_over_real_jdk_bytecode` does **not** list `asVarargsCollector`
either, so no matching edit is wanted there; the two are not kept in sync for
this family today.

`regression-suite/run.sh` needs **no** change: `RJdkHandles` is already in
`JDKONLY_CLASSES`, it takes no per-class arguments from `class_args` or
`class_cv_args`, and the rewrite keeps the `PASS RJdkHandles ` prefix and the
non-zero exit on failure that the harness keys on.

---

## 7. Verification pass 2026-08-12 (lane A16) — what was re-read, and what moved

**Nothing was built or run in this pass either.** Everything below is a source
read against today's worktree, with today's line numbers. It exists because a
record's *hypothesis* can be wrong and not merely stale, and because two of the
three counts this file printed were.

### 7.1 Every source claim in §2, §3, §5.2 and §5.3 is present, at these anchors

| claim | where it is today | verdict |
|---|---|---|
| `MH_VARARGS = MH_BASE + 5` | `native-builtins/src/lang_invoke.rs:6991` | present |
| width-guarded read / write | `:6996`–`:7006` (`object_num_fields(mh) > MH_VARARGS`) | present, exactly as §5.2.1 prescribed |
| both allocators widened to `MH_VARARGS + 1` | `:7553` (`alloc_method_handle`), `:7720` (`alloc_string_concat_method_handle`) | present |
| the slot is never left unwritten | `:7578`, `:7739` — explicit `Int(0)` | present |
| `asVarargsCollector` sets / `asFixedArity` clears | `:6451`, `:6462` | present |
| `isVarargsCollector` registered and reading it | `:6482`–`:6486` | present |
| `bindTo`'s syntactic leading-parameter guard, read from the `type` field only | `:10392`–`:10406` | present; §3.3's safety argument is written in place at `:10366`–`:10375` |
| §5.3's accessor-kind narrowing | `:10471`–`:10476` (`MH_KIND_STATIC \|\| GETTER \|\| SETTER \|\| ARRAY_GET \|\| ARRAY_SET`) | present |
| §5.1's decline, as a **mechanism** | `:10377`–`:10391`, in-file beside the guard | present, and it is the argument §5.1.1 makes, not a budget note |

§5.1 is therefore still correctly open: there is no assignability test anywhere
in the `bindTo` registration, and `NativeContext` still offers no
never-false-refuse `cast`-shaped predicate. **Disposition: still open, blocker
unchanged (capability gap, not time).**

### 7.2 The out-of-file line is STILL NOT APPLIED

`vm/src/vm/vm_exec.rs:23339`–`:23346` still lists four names. The patch under
"Out-of-file patch (not applied)" is reproduced verbatim there and remains the
whole of what is wanted. It is a completeness edit, not a correctness one — the
vector runs on the cold interpreter path where the native already wins — so
this record is **not blocked on it**; leaving it out leaves the cold/warm split
§5.2.3 describes.

### 7.3 The three circulating check counts, settled by counting

Measured today on `regression-suite/src/RJdkHandles.java`:

| | occurrences | minus the definition | green-run value |
|---|---|---|---|
| `step(` | 41 | 40 (definition at `:86`) | **40 steps** |
| `check(` | 133 | 132 (definition at `:65`) | **128 checks** (4 are `check(false, …)` inside a `try` a correct VM never reaches) |

So **"40 steps / 128 checks" is right** and is now arithmetic rather than a
claim. §5's preamble sentence — *"the file has 132 `check(` call sites, four of
which are `check(false, …)` … so a green run counted 116 before this pass"* —
runs an AFTER count into a BEFORE conclusion: 132 − 4 = **128**, which is the
after value; the 116 was measured on the smaller pre-pass file (120 call sites).
Read that sentence as two separate facts. README §2.6's "54 checks" and §4's
"316 checks" are both still wrong and are still not this lane's files.

### 7.4 §5.2.1's `classloader.rs` row: line numbers rotted, the finding did not

The disagreeing `MethodHandle` map is now at
`native-builtins/src/classloader.rs:9089`–`:9107`, not `:9084`–`:9090`. It is
otherwise exactly as described — `MH_BASE = 16` with `MH_KIND`(16),
`MH_TARGET_CLASS`(17), `MH_NAME`(18), `MH_TYPE`(19), `MH_CLASS_ID`(20), under a
comment still claiming it *"Matches the layout used by
lang_invoke::alloc_method_handle"*, which it does not.

**One thing this record did not state and should have: the width guard is
provably sufficient against it.** `classloader.rs`'s allocator asks for
`MH_FIELD_COUNT = MH_BASE + 5 = 21` slots (`:9107`, `:9116`), i.e. valid indices
`0..=20`. `MH_VARARGS` is `21`. `object_num_fields(mh) > MH_VARARGS` is
therefore **false** for every handle that allocator mints, so
`mh_is_varargs_collector` answers `false` and `mh_set_varargs_collector` is a
no-op on them — the pre-fix behaviour, which is the documented fallback. The
same holds for the 17-slot `empty`/`zero` handles. The disagreeing map still
wants a `--dump-native-registry` diff and a deletion; it is not a hazard to
§5.2.

### 7.5 The adjacent live gap, and why it is not this record's

A 33-probe reachability screen run today on the current binary reports
`MethodHandleProxies.asInterfaceInstance` failing with
**`ClassFormatError: ldc: unsupported constant pool entry type at #26`**. That
is a class-file **decoder** gap — the class is refused before any bytecode in it
executes — so it is upstream of every `MH_KIND_*` shim and of §5.2's marking. It
neither causes nor is caused by anything here. The consequence for this record
is narrow and worth writing down: **`MethodHandleProxies` cannot be used as an
end-to-end exerciser of the varargs marking or of `bindTo`**, because the class
never loads. `RJdkHandles` remains the only instrument.

The same screen's passing rows (`Proxy.newProxyInstance`, `ServiceLoader`,
direct `ByteBuffer`, `MXBean`, `AccessController.doPrivileged`) touch none of
this record's claims in either direction. **No claim in this record is
contradicted by them.**
