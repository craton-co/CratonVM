# The throwable constructor table: one fixed descriptor list, 62 classes, 97 constructors that do not exist and 15 that do

> **Status: FIXED (registrar data + stub declarations), PREDICTED.**
> Every HotSpot number is **MEASURED** on Microsoft OpenJDK 25.0.3+9-LTS.
> Every CratonVM number is **DERIVED FROM SOURCE** or **PREDICTED** — this lane
> may not build and may not run the VM.
>
> Files changed: `native-builtins/src/lang_misc.rs`,
> `classloading/src/class_manager.rs`, and this record.

Lane E34, 2026-08-13. Answers E23-1 §9 N1 and N3, which had to move together.

---

## 1. What was wrong, in one line

`register_throwable_subclass_natives` registered **the same four `<init>`
descriptors on every class in a 62-entry list**, and
`synthetic_stub_ctor_methods`' `is_throwable_like` arm declared **the same four
for every class name ending in `Exception` or `Error`**. Neither had ever asked
a class what constructors it actually has.

| | count |
|---|---:|
| classes in the registrar's list | **62** |
| of those, resolvable on JDK 25 | **62** (all) |
| registered descriptors that are **not a public constructor** on the real class | **103** |
| of those, descriptors the class **does not declare at all** | **97** |
| **public constructors `javac` emits that were unregistered** | **15** |
| ctor registrations before → after | 249 → **167** (−97 +15) |

---

## 2. The measurement (executed, no build, no VM run)

`scratchpad/e34/`, all HotSpot-only, ~5 s end to end:

| file | what it does |
|---|---|
| `classes.txt` | the class list **parsed out of** `lang_misc.rs`, not hand-typed |
| `Ctors25.java` | JDK 25 reflection → every declared ctor + its visibility (174 rows, 0 misses) |
| `Diff34.java` | the registrar's model transliterated into Java, diffed per class → §3 |
| `Msgs34.java` | the JDK 25 oracle for the 15 missing ctors: message text, cause, `initCause` behaviour |
| `Gen34.java` | emits the corrected table **as the Rust literal** that now ships |

The shipped table was diffed byte-for-byte against `Gen34`'s output and against
its second copy: **0 transcription errors, 62 rows on both sides.**

### 2.1 Three corrections to E23-1's numbers

E23-1 §5.1 is right about the shape and about the 103. Three of its figures do
not survive re-measurement against the working tree:

* **"69 entries of `throwable_classes`"** — the array has **62**. A source parse
  of the array (`grep` on the entry shape, not the comment lines) gives 62;
  E23's 69 appears to have counted quoted strings inside the two long
  "deliberately NOT in this list" comments.
* **"62 of 69 resolve on JDK 25 (7 have no counterpart)"** — **all 62 resolve.**
  `Ctors25.java` reports zero `MISS`.
* **"16 public ctors unregistered"** — **15**.
  `InvocationTargetException.<init>(Ljava/lang/Throwable;Ljava/lang/String;)V`
  was already registered, by the special-case arm that `continue`s out of the
  loop *above* the blanket block. E23's census parsed the blanket loop and did
  not see the arm.

### 2.2 One of E23-1's load-bearing claims is wrong, and it changes the fix

E23-1 §5 says of `AssertionError.<init>(Ljava/lang/String;)V`:

> `(String)V` → **shadows a private JDK method**. No bytecode can call it. Dead.

The first half is right and the second is false. **MEASURED**, `javap -p -c
java.lang.AssertionError`:

```
  public java.lang.AssertionError(java.lang.Object);
    Code:
       0: aload_0
       1: aload_1
       2: invokestatic  #10   // String.valueOf:(Ljava/lang/Object;)Ljava/lang/String;
       5: invokespecial #16   // "<init>":(Ljava/lang/String;)V     <-- the private one
       8: aload_1
       9: instanceof    #19   // java/lang/Throwable
      ...
```

The private ctor is called by `AssertionError`'s **own** bytecode, via
`this(String.valueOf(...))`, on every `AssertionError(Object)` and every
primitive overload. In real-JDK mode that is the live path for every
`assert c : msg`. E23-1 N1's instruction to "drop `(String)V` from it" would
have deleted a native that is reached constantly and answers correctly.

**So the rule this fix uses is not "public ctors only".** A descriptor stays iff
the real class **declares** it and either it is public (`javac` can emit a call)
or it was already registered (six such, all trivial `super(...)` delegations:
`AssertionError(String)` private; `CompletionException()`/`(String)` and
`ExecutionException()`/`(String)` and `InvocationTargetException()` protected).
Everything the real class does not declare at all is gone — **97 descriptors,
and no `javac` on any real JDK could have compiled a call to any of them**,
which is the whole safety argument for the removal.

---

## 3. The per-class diff (the deliverable E23-1 N1 asked for)

Full output in `scratchpad/e34/diff34.txt` and `gen34.txt`. Summary of every
class whose set changed; `−` = the class declares no such ctor unless noted.

| class | added | dropped |
|---|---|---|
| **`java/lang/AssertionError`** | **`(Ljava/lang/Object;)V` `(Z)V` `(C)V` `(I)V` `(J)V` `(F)V` `(D)V`** | `(Throwable)V` |
| `java/io/UncheckedIOException` | `(Ljava/io/IOException;)V` `(Ljava/lang/String;Ljava/io/IOException;)V` | **all four** |
| `java/lang/IndexOutOfBoundsException` | `(I)V` `(J)V` | `(String,Throwable)V` `(Throwable)V` |
| `java/lang/ArrayIndexOutOfBoundsException` | `(I)V` | `(String,Throwable)V` `(Throwable)V` |
| `java/lang/StringIndexOutOfBoundsException` | `(I)V` | `(String,Throwable)V` `(Throwable)V` |
| `java/util/MissingResourceException` | `(String,String,String)V` | **all four** |
| `java/text/ParseException` | `(String,I)V` | **all four** |
| `java/lang/TypeNotPresentException` | — | `()V` `(String)V` `(Throwable)V` |
| `java/lang/MatchException` | — | `()V` `(String)V` `(Throwable)V` |
| `java/util/FormatterClosedException` | — | `(String)V` `(String,Throwable)V` `(Throwable)V` |
| `java/lang/reflect/InvocationTargetException` | — | `(String)V` `(String,Throwable)V` |
| `java/lang/NoClassDefFoundError` | — | `(String,Throwable)V` `(Throwable)V` |
| `java/lang/LinkageError`, `ClassNotFoundException`, `ExceptionInInitializerError` | — | `(Throwable)V` / `(String,Throwable)V` (one each) |
| 33 further classes | — | `(String,Throwable)V` and `(Throwable)V` (two each) |
| 16 classes | — | — (already exactly right) |

`java/lang/Throwable`, `Exception`, `RuntimeException`, `Error`,
`IllegalArgumentException`, `IllegalStateException`, `IOException`,
`NoSuchElementException` and eight others keep all four: they really do declare
all four. **The most-used classes in the family are untouched.**

`UncheckedIOException` is the sharpest row after `AssertionError`: it advertised
four constructors and had none. Nothing could construct it by any descriptor.

---

## 4. `AssertionError`, and why it is 88 fixtures

`AssertionError(String)` is **private** in the JDK. So `javac` compiles both

```java
throw new AssertionError(msg);   // -> <init>:(Ljava/lang/Object;)V
assert cond : msg;               // -> <init>:(Ljava/lang/Object;)V
```

to the `(Object)` overload — **MEASURED**, and E23-1 §4.2 measures the reach:
**88 of 277 regression-suite classes, 157 call sites**, twenty-nine times the
next row. It was not registered and not declared.

The JDK 25 body, and the semantics now implemented (all **MEASURED**,
`scratchpad/e34/Msgs34.java`):

| expression | `getMessage()` | `getCause()` | later `initCause` |
|---|---|---|---|
| `new AssertionError((Object) "boom")` | `"boom"` | null | succeeds |
| `new AssertionError((Object) null)` | `"null"` — the four-char **String** | null | succeeds |
| `new AssertionError((Object) anISE)` | `anISE.toString()` | `anISE` | **throws ISE** |
| `new AssertionError(42)` / `(1.5f)` / `('x')` / `(true)` | `"42"` / `"1.5"` / `"x"` / `"true"` | null | succeeds |

The `(Object)` native writes the sentinel `cause = this` first and only then
adopts a `Throwable` argument, which is what makes the last two rows differ. The
six primitive overloads delegate formatting to `lang_string`'s existing
`String.valueOf` natives rather than re-deriving Java's float/double text rules
— that is `format_float`/`format_double`'s job and a second copy would drift.

The other measured messages now produced: `"Index out of range: 7"`,
`"Array index out of range: 7"`, `"String index out of range: 7"`, and
`new UncheckedIOException(null)` throws `NullPointerException`
(`Objects.requireNonNull` in the JDK body).

---

## 5. Why N1 and N3 had to move together — and which half is load-bearing

E23-1 said the stub arm "must move with N1 or the stub and the registry will
disagree about which ctors exist". Reading the resolution path in the working
tree shows the coupling is real but **asymmetric**, which matters for predicting
the outcome:

`vm/src/vm/vm_exec.rs:24614` — the "final native-registry fallback" — probes
`native_methods.find(&cls.name, method_name, descriptor)` up the **dispatch
class chain** and then the **receiver class chain**, *without requiring the class
to declare the method*. So:

* **Adding** `AssertionError.<init>(Ljava/lang/Object;)V` to the registry is by
  itself enough for the call to be answered. The registrar half is what fixes
  the 88 fixtures.
* **Removing** a descriptor is only effective if BOTH halves drop it. A stub that
  declares an `<init>` as `PUBLIC|NATIVE` with no `Code` and no registration
  behind it resolves and then fails — a different error, not fewer errors.

That is why the 97 removals are in both files or in neither, and why the two
tables are now guarded against each other (§6).

**Domain difference, preserved deliberately:** the registrar's list is 62 named
JDK classes; `is_throwable_like` fires for **any** name ending in
`Exception`/`Error`, including application classes (`…/DbException`) and JDK
throwables outside the measured set. The new arm consults the table first and
falls back to the historical four for everything else. Narrowing a class whose
real constructor set has not been measured would remove answers with nothing to
put in their place — the `[flag≠mode drops it]` shape.

---

## 6. How a JDK bump surfaces as a diff instead of drift

Three mechanisms, in order of strength:

1. **The table is generated, not transcribed.** `scratchpad/e34/Gen34.java`
   reflects over the class names in column 0 of the table itself and prints the
   Rust literal. Regenerating on a new JDK produces a paste-ready diff. The
   command and the rule are in the table's header comment in both files, so the
   next reader does not have to find this record first.
2. **The two copies are guarded against each other by an executed test.**
   `throwable_ctor_table_matches_the_stub_declarations` in
   `classloading/src/class_manager.rs` is a **source witness**: it reads both
   working-tree files, extracts the rows between `THROWABLE-CTOR-TABLE`
   markers, and fails on any disagreement. It is non-vacuous by construction —
   it asserts ≥ 60 rows parsed on each side before comparing, so a broken
   extractor fails loudly instead of comparing two empty lists. The markers are
   assembled from fragments at run time so the test's own text is not what the
   scan finds.
3. **A descriptor with no body cannot be registered silently.**
   `throwable_ctor_native` returns `Option`; the loop `debug_assert!`s on `None`
   rather than falling back to a default body. A future row added to the table
   without an implementation is named, not mis-registered.

What this does **not** do: nothing recomputes the table against a live JDK in
CI, so nothing here would catch JDK 26 adding a constructor. That is **N1**
below — and it is not a gate that has to be built, because a sibling lane has
already landed `scripts/jdk-baseline/` + `scripts/baselines/jdk25-*.tsv` +
`native-builtins/src/jdk_baseline.rs`, whose TSVs already carry `<init>` rows.
The three mechanisms above catch transcription drift, copy drift and
table/dispatcher drift — the failure modes that the hand-written guards in this
tree keep getting wrong — and the baseline files close the fourth.

**Why two copies at all:** the crate dependency runs
`native-builtins → classloading`, so `classloading` cannot import the registry's
table, and the one-table fix needs a `pub use` in `classloading/src/lib.rs`,
which is not this lane's file. N5 nominates it.

---

## 7. The same rule is implemented at FOUR sites; this lane owns two

Grepping for the blanket four-descriptor loop found two more copies, neither
named by E23-1:

| # | site | domain | mine? |
|---|---|---|---|
| 1 | `native-builtins/src/lang_misc.rs` `register_throwable_subclass_natives` | 62 named classes | **yes — fixed** |
| 2 | `classloading/src/class_manager.rs` `synthetic_stub_ctor_methods` | any `*Exception`/`*Error` | **yes — fixed** |
| 3 | `native-builtins/src/lib.rs`, inside `#[cfg(feature = "synthetic-jdk")] register_synthetic_overrides` | 30 classes, incl. `AssertionError` | no — **N2** |
| 4 | `native-builtins/src/lib.rs` `register_exception_extras_natives` | 57 classes | no — **N3** |

Site 4 is a copy-paste twin of site 1: it carries the *same two* long
"deliberately NOT in this list" comments about `PatternSyntaxException` and
`InvalidClassException`, verbatim, with a drifted class list and different ctor
bodies (`native_exception_init_empty`/`_msg` vs `native_exc_init_noargs`/
`_message`). It is called from `register_synthetic_overrides` **and** from
`reflect_annotations.rs:1131`, i.e. in both modes, and there is no unregister
API — so `register` is last-write-wins and sites 3 and 4 keep re-registering
four descriptors on ~55 of my 62 classes.

**Consequence, stated so the prediction is honest:** my 15 additions are
unaffected (no other site registers those descriptors), but a subset of my 97
removals will still be *registered* by sites 3 and 4. Because the stub no longer
declares them, they become genuinely dead registrations rather than a live
divergence — the mode-visible behaviour follows the `class_manager` half. Fully
retiring them needs N2 and N3.

---

## 8. What is predicted to change

* **Synthetic-JDK mode.** `AssertionError(Object)` and the six primitive
  overloads, both `UncheckedIOException` ctors, `ParseException(String,int)`,
  `MissingResourceException(String,String,String)` and the three index-family
  `(int)`/`(long)` ctors become answerable. 97 descriptors that HotSpot also
  refuses stop being answerable, matching HotSpot.
* **Real-JDK mode.** `AssertionError.<init>(Ljava/lang/Object;)V` is now a
  native where it used to run JDK bytecode. This is the one place the fix
  touches real-JDK mode, and it is a *smaller* change than it looks: that
  bytecode's first act was `invokespecial <init>(String)`, which was **already**
  natively shadowed by `native_exc_init_message`. The native now does the whole
  body instead of half of it, with the `instanceof Throwable → initCause` arm
  reproduced and measured. The 97 removals are inert in real-JDK mode: the
  methods do not exist, so nothing could resolve to them.
* **Known partials, named rather than hidden.** `ParseException.errorOffset` and
  `MissingResourceException.className`/`key` are written by name. Real-JDK mode
  reads them back through the real `getErrorOffset()`/`getClassName()`/`getKey()`
  bytecode; synthetic-JDK mode has neither the stub fields nor the accessors, so
  the objects are constructible but that state is not readable. Constructible
  with unreadable extras is strictly better than not constructible at all, but
  it is a gap and it is N6.

### The one sentence asked for

A fixture that executes `assert false : "msg"` under `-ea` now constructs its
`AssertionError` through the registered `<init>(Ljava/lang/Object;)V` and fails
with **`java.lang.AssertionError: msg`** — its own diagnostic — instead of
aborting with `NoSuchMethodError:
java/lang/AssertionError.<init>(Ljava/lang/Object;)V` on the error path.

### The measurement to demand of whoever can run it

In synthetic-JDK mode, a fixture whose failure path is `throw new
AssertionError(msg)` must now print `msg`. If it still reports the
`NoSuchMethodError`, either the registration is being overwritten by site 3/4 or
the resolution path in §5 was read wrong; if it prints something *other* than
`msg` — most likely a bare class name or `null` — then
`native_string_value_of_object` is not reaching the argument's `toString()`, and
§4's table is the oracle to diff against.

---

## 9. Nominations

### N1 — `scripts/jdk-baseline/classes.txt` + `native-builtins/src/jdk_baseline.rs`: this table belongs in the baseline mechanism a sibling lane just built

The strongest anti-drift mechanism this record does **not** have — and it should
not be a new one. A concurrent lane (E32/E25) has landed exactly the right
machinery in the working tree: `scripts/jdk-baseline/generate.py` writes one
`scripts/baselines/jdk25-<class>.tsv` per class from the running JDK's own image,
`native-builtins/src/jdk_baseline.rs` `include_str!`s them, and
`scripts/baselines/README.md` already states the rule this record is an instance
of — *"A row typed by a person is the defect these files exist to remove."*

**Crucially, those TSVs already carry constructors.**
`scripts/baselines/jdk25-java.lang.Character.tsv` contains
`METHOD	<init>	(C)V	public`, so the format needs no change.

The nomination, concretely:

1. Add the 62 class names from this table to `scripts/jdk-baseline/classes.txt`
   (its header asks for the guard each entry serves — the guard here is
   `register_throwable_subclass_natives` / `synthetic_stub_ctor_methods`, and the
   denominator is this record's 103 / 15).
2. Add the 62 `include_str!` rows and `ALL` entries in `jdk_baseline.rs` — its
   own comment says that is "one `include_str!` plus one `ALL` row and nothing
   else", and its `read_dir` cross-check makes an unread baseline a build
   failure.
3. Add one guard asserting that the shipped table's **public** descriptors equal
   the baseline's `METHOD <init> … public` rows, per class.

One documented exception the TSVs cannot express: the six retained non-public
descriptors (§2.2), of which `AssertionError(String)` is private and therefore
absent from a `--public`-shaped baseline. That is a deliberate, argued retention
rather than data, so the guard should compare the public half and carry the six
as a named allow-list.

`scratchpad/e34/Gen34.java` and the source witness in §6 stay useful in the
meantime — they run in seconds with no build — but they are the interim, and
this is the closure. Same gate as E23-1 N5.

### N2 — `native-builtins/src/lib.rs`, `register_synthetic_overrides`: the third copy of the blanket four

*old, verbatim* (the loop header inside `register_synthetic_overrides`):

```
    // --- Exception constructors ---
    // Throwable/Exception/RuntimeException/<init>()V — no-op (fields default to null)
    // <init>(Ljava/lang/String;)V — set field 0 = message
    // <init>(Ljava/lang/String;Ljava/lang/Throwable;)V — set field 0 = message, field 1 = cause
    // <init>(Ljava/lang/Throwable;)V — set field 1 = cause
    for exc_class in &[
```

*new:*

```
    // --- Exception constructors ---
    // Throwable/Exception/RuntimeException/<init>()V — no-op (fields default to null)
    // <init>(Ljava/lang/String;)V — set field 0 = message
    // <init>(Ljava/lang/String;Ljava/lang/Throwable;)V — set field 0 = message, field 1 = cause
    // <init>(Ljava/lang/Throwable;)V — set field 1 = cause
    //
    // E34-1 §7: this is the THIRD of four copies of the "four fixed ctor
    // descriptors on a list of throwables" rule. MEASURED against JDK 25 over
    // the 62-class list in `lang_misc.rs`, that blanket registers 103
    // descriptors the real class does not declare as public and misses 15 it
    // does. `lang_misc.rs::register_throwable_subclass_natives` now carries a
    // per-class table derived from the JDK; every class below that also appears
    // there is re-registered by this loop with the blanket four, so the
    // descriptors this loop adds beyond that table are dead (the synthetic stub
    // no longer declares them). Delete this loop and let the table own the
    // family, or narrow it to the same table.
    for exc_class in &[
```

Plus the code change: delete the loop, or key it off the same table.
`java/lang/AssertionError` is in this list and is the row that matters.

### N3 — `native-builtins/src/lib.rs` `register_exception_extras_natives`: the fourth copy, and a twin that has drifted

*old, verbatim:*

```
fn register_exception_extras_natives(registry: &mut NativeMethodRegistry) {
    // Register constructors for commonly-needed exception types
    // All use the standard Throwable 2-field layout (field 0 = message, field 1 = cause)
    let exceptions = [
```

*new:*

```
fn register_exception_extras_natives(registry: &mut NativeMethodRegistry) {
    // Register constructors for commonly-needed exception types
    // All use the standard Throwable 2-field layout (field 0 = message, field 1 = cause)
    //
    // E34-1 §7: this is a COPY-PASTE TWIN of
    // `lang_misc.rs::register_throwable_subclass_natives` — it carries the same
    // two "deliberately NOT in this list" comments verbatim — with a drifted
    // class list AND drifted bodies (`native_exception_init_empty`/`_msg` here
    // vs `native_exc_init_noargs`/`native_exc_init_message` there). It is
    // called from `register_synthetic_overrides` and from
    // `reflect_annotations.rs`, so it runs in BOTH modes and, being later,
    // last-write-wins over the other registrar for every overlapping class.
    // Which of the two ctor bodies is live for ~55 classes has never been
    // stated. Diff the two bodies, then delete one list.
    let exceptions = [
```

Plus: diff `native_exception_init_empty`/`native_exception_init_msg` against
`native_exc_init_noargs`/`native_exc_init_message` and delete the loser. This is
the `[2twins]` / `[dup nati]` shape and it is not this lane's file.

### N4 — E23-1's own record: three numbers and one mechanism claim

`docs/known-issues/jdk-only/E23-1-synthetic-jdk-nosuchmethoderror-census.md` §5
and §5.1 should be amended for §2.1 and §2.2 above: the list is 62 not 69, all
62 resolve, the miss count is 15 not 16, and `AssertionError(String)` is
private-**and-reachable**, not dead — `AssertionError(Object)`'s own bytecode
calls it. The census's conclusions are unaffected; its N1's instruction to drop
`(String)V` would have been a regression.

### N5 — `classloading/src/lib.rs`: export the table so there is one copy

The durable fix for §6. One line in the existing `pub use class_manager::{…}`
block would let `lang_misc.rs` consume the table instead of mirroring it, and
the source witness could then be deleted.

*old, verbatim* (`classloading/src/lib.rs`, line 78):

```
pub use class_manager::is_bootstrap_appended_class;
```

*new:*

```
pub use class_manager::is_bootstrap_appended_class;
// E34-1 §6: the Throwable `<init>` descriptor table, so
// `native-builtins/src/lang_misc.rs` can consume it instead of keeping a second
// copy guarded by a source-witness test.
pub use class_manager::jdk_throwable_ctor_descriptors;
```

(`jdk_throwable_ctor_descriptors` must also change from `fn` to `pub fn` in
`class_manager.rs`; that half is this lane's and will be taken the moment the
export exists.)

### N6 — the two known partials

`ParseException.getErrorOffset()` and
`MissingResourceException.getClassName()`/`getKey()` have no natives and their
fields are not in `synthetic_stub_fields`, so in synthetic-JDK mode those two
classes are now constructible with unreadable state. Three fields and three
accessors closes it. Low reach (0 fixture callers in E23-1 §4.2), recorded so
it is not rediscovered as a mystery.

---

## 10. Reproducing

```
cd scratchpad/e34
javac -d . Ctors25.java Diff34.java Gen34.java Msgs34.java
java -cp . Ctors25 < classes.txt      # every declared ctor + visibility
java -cp . Diff34                     # the 103 / 15 per-class diff
java -cp . Gen34                       # the shipped table + the add/drop accounting
java -cp . Msgs34                      # the JDK oracle for the 15 new ctor bodies
```

`classes.txt` is produced from the Rust source, so the oracle can never be
measuring a different list than the one that ships:

```
sed -n '/THROWABLE-CTOR-TABLE BEGIN/,/THROWABLE-CTOR-TABLE END/p' \
  native-builtins/src/lang_misc.rs | grep -oE '"[a-zA-Z0-9/$]+", &\[' | ...
```

### What this proves and what it does not

It proves the **data**: given JDK 25.0.3+9-LTS's real constructor tables, the
descriptor list that now ships is exactly what those 62 classes declare, modulo
the six explicitly retained non-public ones — verified by diffing the shipped
literal against the generator's output (0 differences) and the two copies against
each other (0 differences, 62 rows each). It proves the **semantics** of all 15
new constructor bodies against HotSpot, including the three that surprised:
`AssertionError((Object) null)` gives the *text* `"null"`, an `AssertionError`
built from a `Throwable` refuses a later `initCause`, and
`new UncheckedIOException(null)` throws.

It does **not** prove anything about a CratonVM binary — none carrying these
changes exists. The Rust is unrun and uncompiled by this lane. Every "after" in
§8 is predicted from the resolution path read in the source.
