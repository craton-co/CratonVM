# String concatenation loses an unpaired surrogate to U+FFFD

**Status:** OPEN. Found 2026-08-04 while removing the forced-native
`java/lang/String` policy
([`FIXED` record](../internal/forced-native-string-policy-two-lists-that-disagree-FIXED-20260804.md)).

**Reproducer:** `probes/StringUtf16HashProbe` (clean) versus
`probes/StringPolicyMatrixProbe` (corrupt) — the two build the same string two
different ways, which is what isolates this.

## What is wrong

```java
String a = new String(new char[] { 'x', '\uD801', 'y' });   // survives
String b = "x" + new String(new char[] { '\uD801' }) + "y"; // becomes "x�y"
```

`a` reads back through `charAt` as `0078 D801 0079`, exactly as on HotSpot.
`b` reads back as `0078 FFFD 0079`.

An unpaired surrogate is not a Unicode scalar value, so it cannot round-trip
through a Rust `String`/`&str`. Any path that decodes a Java `String` to UTF-8
and re-encodes it replaces the surrogate with U+FFFD. Concatenation is on such
a path; `new String(char[])` is not.

This is what makes it easy to misattribute: the same probe corpus looks corrupt
or clean depending only on how its constants were built, and a `hashCode` or
`substring` differential over the corrupt one points at the wrong subsystem.

## Where it is

Located 2026-08-05, no longer a "where to look".

`vm/src/runtime/invokedynamic.rs::execute_string_concat` accumulates into a
**Rust `String`**:

```rust
let s = value_to_string(shared, Some(thread), &arg_val, arg_type);
result.push_str(&s);
...
let str_ref = crate::runtime::interpreter::create_string_or_oom(shared, thread, &result)?;
```

A Rust `String` is UTF-8 by construction, so it cannot hold an unpaired
surrogate: the loss happens on the way *in* (`value_to_string` decoding the
argument) and is sealed on the way *out* (`create_string_or_oom` re-encoding
it). Every `"a" + b` site in the VM goes through this function, which is why the
symptom is universal for concatenation and absent for `new String(char[])`.

## The fix, and the reason it is not a one-liner

Accumulating `Vec<u16>` code units instead of a `String` is the correct shape —
the VM already stores compact strings as little-endian UTF-16 pairs and has
unit-level constructors (`init_string_from_units`). The care needed is that
this is a hot path: every string concatenation in every workload runs it, and
`push_str` on a `String` is not the same cost as pushing units.

Two options, in preference order:

1. **Units throughout.** Change the accumulator to `Vec<u16>` and give
   `value_to_string` a unit-returning sibling for the `String` case (the
   non-`String` cases go through `toString()` and are already lossless as
   UTF-8). Measure against the concat-heavy probes before landing.
2. **Lossless fast path with a units fallback.** Keep the `String`
   accumulator, but detect an argument that is not losslessly representable
   and redo that concat through the unit path. Cheaper to land, but it adds a
   check to the hot path and leaves two code paths that can drift — the shape
   this feature has been burned by repeatedly.

Do not land either without the concat-heavy timings; the equivalent change on
the `java/lang/String` natives turned out to make its workload FASTER, which is
not something to assume in either direction.

The general shape — a native that reads a `String` through `read_string` and
writes it back through `create_string` cannot carry an unpaired surrogate — was
the cause of a dozen divergences in the forced-native `String` surface, all of
which went away when those natives were dropped in favour of real bytecode.
Concat is the residue: it is not one of those natives.

## Blast radius

Narrow but real: text that legitimately contains unpaired surrogates —
truncated UTF-16 input, a `substring` that splits a surrogate pair, some
Windows filesystem and clipboard paths, fuzz corpora. Data is silently
replaced, not rejected, so it surfaces later as a mismatch rather than an
exception.

## Verification when fixed

`probes/StringPolicyMatrixProbe` rows `charAt*(LONE)`, `substring1(LONE,*)`,
`trim(LONE)`, `strip(LONE)`, `toLowerCase-ROOT(LONE)`, `concat(SUPP,LONE)`,
`getBytes(LONE,UTF-8)`, `roundtrip(LONE,UTF-8)`, `hashCode(LONE)` — all of
which currently differ from HotSpot **only** because `LONE` is built with `+`.
Building it with `new String(char[])` instead already passes today, which is
the control.
