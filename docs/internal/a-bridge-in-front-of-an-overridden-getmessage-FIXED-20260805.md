# A blanket `getMessage` bridge shadowed `PatternSyntaxException`'s override — FIXED 2026-08-05

**Status:** FIXED.

**Reproducer:** `probes/StringRegexErrorProbe`, the `split("[")` row.

## What was wrong

`"hello".split("[")` threw the right class — `PatternSyntaxException` — with
`getMessage() == null`, where HotSpot returns

```text
Unclosed character class near index 0
[
^
```

The exception object was fine. The *accessor* was wrong.

`native-builtins` registers a blanket set of exception helpers over a list of
~60 throwable classes: `<init>` in four shapes, `getMessage`, `getLocalizedMessage`
and `toString`. `java/util/regex/PatternSyntaxException` was in that list. It is
the one member that **overrides `getMessage()`**: the JDK class stores `desc`,
`pattern` and `index`, leaves `Throwable.detailMessage` null, and assembles the
three-line report on demand. A `Bridge` native in front of that override returned
`detailMessage` — null, correctly, for an object that never had one.

Its real constructor is `(String desc, String regex, int index)`, which is not
among the four `<init>` shapes the loop registers, so *every* registration the
loop added for this class was either dead or actively wrong.

## How it was found

`--dump-native-registry`, not code reading. The census showed the triple twice:

```
java/util/regex/PatternSyntaxException getMessage ()Ljava/lang/String;  inv=0  registered_by lang_misc.rs:2372
java/util/regex/PatternSyntaxException getMessage ()Ljava/lang/String;  inv=7  registered_by lib.rs:36989  overwrote=bridge
```

Two registrations, last-write-wins, and the survivor had been **invoked 7 times**
in the probe run. That is the same shape as
[[reference_dump_native_registry_finds_the_clobbering_duplicate]]: a symptom that
looks like a missing feature is a registration that should not exist, and the
census names the owner while re-reading the source proves nothing.

## The fix, and the second half it exposed

`PatternSyntaxException` is removed from both blanket lists.

That alone fixed `split` (which goes through the JDK's own `Pattern` bytecode, so
the real constructor had already populated the fields) and **broke nothing** —
but it left `matches("[")` / `replaceAll("[")` reporting
`"null near index 0\r\nnull"`. Those go through *our* regex natives, which raised
`RuntimeError::PatternSyntaxException { message }` and let the generic
`create_exception_object` path call `<init>(String)` — a constructor this class
does not have. The object came out with all three fields unset, and the
now-correctly-dispatched override formatted them as `null`.

So the variant carries the **parts** instead of a message:

```rust
PatternSyntaxException { description: String, pattern: String, index: i32 }
```

and `throw_runtime_error` sets `desc` / `pattern` / `index` by name after
construction. Formatting the string Rust-side would have been wrong twice over:
`getMessage()` uses `System.lineSeparator()`, so HotSpot emits `\r\n` on Windows
where a `format!` would emit `\n`, and `getDescription()` / `getPattern()` /
`getIndex()` would still have returned nothing.

## Why the count did not show the regression

Rows 258 and 270 were **already divergent** on message text before this change,
so removing the bridge moved the divergence count *down* (8 → 5) while those two
rows got materially worse in content. Only the "still-divergent rows whose value
changed" check caught it — the same blind spot recorded in
[[reference_a_category_wide_registration_drop_needs_two_sided_pin]] and
[[reference_a_message_diff_can_hide_a_wrong_exception_class]]. A set comparison
plus a value-change check on the intersection is the minimum; a count is not
enough.

Final: 8 → 3 divergences, 5 fixed, **0 regressions, 0 value changes** on the rows
that remain.

## The general shape, worth checking elsewhere

**A blanket registration loop over a class list cannot know which members
override the method it is registering.** Any class in those lists that overrides
`getMessage`, `getLocalizedMessage` or `toString` has the same defect. This one
surfaced because a differential probe read the message; the others would surface
only the same way.

## The sweep, done 2026-08-05

`javap -p` over all 68 classes in the two lists, looking for a declared
`getMessage` / `getLocalizedMessage` / `toString`. Exactly **two** besides
`PatternSyntaxException`:

* **`java.io.InvalidClassException`** overrides `getMessage()` to prepend the
  offending class name. Confirmed broken and **FIXED**: ours returned
  "bad serialVersionUID" where HotSpot returns
  "com.example.Foo; bad serialVersionUID", so a deserialization failure lost the
  one field naming WHICH class failed. Removed from both lists.
* **`java.lang.NullPointerException`** overrides `getMessage()` to compute the
  helpful "Cannot invoke ... because ... is null" text lazily. Checked and
  **already correct**: `probes/ThrowableAccessorOverrideProbe` shows this VM
  producing HotSpot's exact text, `<local1>` placeholder included, for both a
  null method call and a null array length. Left in place; the bridge is not
  hurting it, because the JDK only consults the override when
  `detailMessage` is null and our own throw path fills that in.

`probes/ThrowableAccessorOverrideProbe` now matches HotSpot on every row.
