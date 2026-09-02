# G69-1 — 82 rows where the type was right and the sentence was wrong

**Status:** MEASURED throughout; fixed, and one fix nobody was looking for.
**Provenance:** every row on both VMs. Oracle HotSpot 25.0.3+9-LTS; CratonVM
`C:/craton/target-rel12` (before) and `target-rel13` (after), `--jdk-only`.
Probes `scratchpad/g71/{PD,PE,PF,PG}.java`, ASCII and deterministic.

---

## 0. Why this is not a cosmetics record

`G68-1` N1 deferred the `IllegalAccessException` family with four measured
rows and the note that they were "a start, not the table". They were. The
table is 82 rows, and it says something the four could not:

> **The exception types and the precedence lattice were already exact. All 82
> rows agreed on both. All 82 disagreed on the text.**

That is an unusual shape and it is worth naming, because it inverts the usual
priority. Normally a wrong message is the least of it. Here there was nothing
else wrong — so the message was the entire remaining defect, and a reflective
write that fails is diagnosed by reading exactly this sentence and nothing
else. `Field.setInt` on a `static final int` said

```text
Can not set static final field via Field.set: Field typed setter
```

where HotSpot says

```text
Can not set static final int field PD$H.I to (int)9
```

— an internal operation label where the field belongs. On the ranks that
printed a type at all it was the raw JVM descriptor (`I`,
`Ljava/lang/String;`), which no JDK message has ever used.

## 1. Five grammars, and they are not uniform

```text
1  Can not set[ static][ final] <ty> field <Q> to <attempted>     ranks 3,5,6 + generic rank 4
2  Can not set[ static][ final] <ty> field <Q> on <recvClass>     typed setter, rank 4
3  Can not get[ static][ final] <ty> field <Q> on <recvClass>     every getter, rank 4
4  Attempt to get <ty> field "<Q>" with illegal data type conversion to <target>
5  NullPointerException with a NULL message
```

Three asymmetries, each of which a tidier renderer would have erased and each
of which cost a probe round to find:

* **Grammar 4 carries no modifiers.** Measured on a `public static final int`:
  `Attempt to get int field "PD$H.I" ...` — neither `static` nor `final`.
  Grammars 1–3 carry both.
* **Grammar 2/3 *does* carry `final`.** A final instance field is the one case
  where a `final` can reach rank 4 at all, and it prints
  `Can not get final int field PG$H.fin on PG$Other`. I probed this
  specifically because I could not derive it, and the answer was the opposite
  of grammar 4's.
* **Grammar 5 has one exception out of five entry points.** The generic
  `Field.set` answers HotSpot's helpful NPE,
  `Cannot invoke "Object.getClass()" because "o" is null`; the typed setters
  and all the getters answer a null message on the same input.

And one quirk nobody derives:

```text
f.set(new Other(), "ARG")   ->   Can not set int field PF$H.nf to PF$Other
```

The generic setter given a wrong-typed **receiver** names it after `to` — the
sentence position every other rank-6 row fills with the **value**. Confirmed
against a distinctive `String` argument and against a `null` one; both still
print `PF$Other`.

## 2. The value rendering is not the value's type

```text
setByte(9)   into an int field   ->  (int)9
setChar('z') into an int field   ->  (int)122      the NUMBER
setChar('z') into a char field   ->  (char)z       the CHARACTER
setLong(9L)  into an int field   ->  (long)9       IllegalArgumentException
```

A **legal** widening is refused at rank 5, after conversion, and reports the
FIELD's type and the widened value. An **illegal** one is refused at rank 3,
before conversion, and reports the SETTER's type. Same field, same call shape,
two different types in the sentence and two different exception classes.
Neither follows from the other; both are transcribed.

## 3. `argument type mismatch` was right, for a different caller

Rank 6 answered `argument type mismatch` for every field refusal. That string
is JDK-faithful — the comment above it is correct, cites Hibernate's HHH-20261
appending it verbatim, and is worth keeping. It is right for `Method.invoke`
and `Constructor.newInstance`. It is not right for `Field.set`, which has its
own sentence naming the field and the value.

**A correct contract applied one caller too widely.** That is the third time
in two days: `G66-1`'s guard asserting a bytecode guarantee inside the native
that replaced the bytecode, and `G68-1` §2's comment stating
`InstantiationException` while the code threw something else. The remedy here
was a message override rather than an edit to the shared helper, so the caller
it was written for keeps it.

## 4. The fix nobody was looking for: an array is not its own class

After the rewrite, 79 of 82 rows matched. The last three:

```text
                     HotSpot                CratonVM
int[] value          [I                     java.lang.Object
String[][] value     [[Ljava.lang.String;   [Ljava.lang.String;
```

An array's `class_id_of_object` is **not its own class**: for a reference
array it is the COMPONENT's id, and for a primitive array it is `ClassId(0)`.
So `class_name_of_id` on an `int[]` answers `java.lang.Object`, and on a
`String[][]` it silently drops a dimension.

`Object.getClass()` has always known this — it computes the array descriptor
itself, and `new int[1].getClass().getName()` was already exact on both VMs.
The reflection path was asking a different question and getting a wrong
answer. That computation is now `lang_class::array_class_internal_name`,
called from both, so the two cannot disagree. Its `Cow` is preserved: the
comment protecting the eight allocation-free primitive branches was making a
real point about a hot path, and lifting the code kept it.

**This was found by three rows out of 82, on a probe written for something
else.** It is the argument for wide probes stated as plainly as it gets.

## 5. What is guarded

27 rows in a new `refmsg` family in `RJdkIntrinsics3`, **all verified PASS on
HotSpot before the fix was built** — including the array-value rows and the
three asymmetries of §1, which are precisely the rows a plausible wrong fix
would have passed.

Three unit tests pin the parts that are guessable, at the code:

* `descriptor_get_name` is `getName()`, not the `int[]` speller — and the test
  asserts `array_descriptor_to_type_name("[I") == "int[]"` alongside it, so
  the fork stays visibly a fork. `G68-1` §3a made exactly this mistake in
  `NoSuchMethodException`, and the wrong helper is still one import away.
* `render_prim_as` follows the type NAMED in the message, not the value's tag.
* grammar 4 quotes the name and drops the modifiers, asserted against grammar
  1 on the *same* `FieldIdent` so the asymmetry is the assertion.

## 6. What did NOT change

Not one exception type, and not one rank of the precedence lattice. The
lattice banner in `lang_class.rs` was already right, already measured, and is
the reason this was a one-day job: everything was in the right place and
saying the wrong thing.

Arms at `6aa23080b`: `--jdk-only` **100 of 100**. `SUITE=all` 95 of 100 and
`SUITE=core` 61 of 62 — both at baseline, failure sets identical **by name**,
which is what a change to `Object.getClass()` owes the other two modes.

## 7. A harness trap that cost twenty minutes

`run.sh` exports `MSYS_NO_PATHCONV=1`, so a `/c/...` path reaches the Windows
executable verbatim. `JDK=/c/Program Files/...` therefore produces a garbage
`--java-home` and **every vector fails** — 0 of 100, with per-vector errors
that look like the VM is broken. `JDK=C:/Program Files/...` is required.

Two-binary attribution is what identified it in one step: the *pre-change*
binary failed identically, so the harness was implicated and the change was
not. Recorded because "0 of 100" is the most alarming possible output and the
cause is one slash.

## 8. NOMINATIONS

**N1 — the array-class-id trap, everywhere else.** §4 is one symptom of
`ctx.class_name_of_id(ctx.class_id_of_object(o))`, a pattern with **306
occurrences across 100 files** in `native-builtins`. It is not wrong in
general — most sites are dispatching on a receiver that cannot be an array.
It is wrong wherever the receiver CAN be an array and the answer is used as an
identity or printed. Nobody has separated the two, and the grep is the easy
half.

**N2 — `G68-1` N2 is unmoved.** `int.class.newInstance()` still answers
`UnsupportedOperationException` because a primitive `Class` mirror carries no
class id. Untouched here; §4 is a different hole in the same wall.

**N3 — the exception-contract axis, third instalment.** `G68-1` N3 listed
`Thread` interrupt/join, lock and condition contracts, class-loading and
resource-resolution failures, and charset decode/encode error actions. None
were touched. This record covered only what a reflective FIELD access says.
`Method.invoke` and `Constructor.newInstance` have their own message
families and were only sampled, never swept.

**N4 — the rank-6 override is a one-caller fix.** `coerce_arg_strict_msg`
gives `Field.set` its sentence. Whether any OTHER caller of
`coerce_arg_strict` also inherits a message written for `Method.invoke` is
unmeasured — the call sites were not enumerated, because this record's
evidence only ever reached the field path.
