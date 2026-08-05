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

## Where to look

`StringConcatFactory` / the `invokedynamic` string-concat bridge, and the
`indy` concat helper the JIT binds (`INDY_STRING_CONCAT_FN`). The narrowing
question is whether the concat path goes through `read_string` /
`create_string` (UTF-8, lossy) rather than assembling the two backing arrays as
code units.

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
