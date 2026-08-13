# String concatenation lost an unpaired surrogate to U+FFFD -- FIXED 2026-08-05

**Status:** FIXED 2026-08-05. Found 2026-08-04 while removing the forced-native
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

## The fix

Landed 2026-08-05: option 1 from the original plan, "units throughout".

`execute_string_concat` accumulates a `Vec<u16>` and finishes with a new
`create_string_from_units_or_oom` (same GC/OOM escalation ladder as the `&str`
twin). `read_java_string_units` / `decode_java_string_value_array_units` are the
lossless siblings of the `str` readers, sharing their receiver guards through an
extracted `java_string_value_and_coder` so the two cannot drift.

### It was three loss points, not one

`probes/StringConcatSurrogateProbe` was written to separate them, and it is the
reason this did not ship half-fixed. Concatenation reaches the accumulator by
three paths and only the first goes through the heap:

| # | path | example | fixed by |
|---|---|---|---|
| 1 | ARGUMENT (`TAG_ARG`) | `"x" + lone + "y"` | the units accumulator |
| 2 | RECIPE literal | `"x\uD801y" + n` (javac folds the text into the recipe) | recipe carried as units |
| 3 | CONSTANT (`TAG_CONST`) | `a + "\uD801" + b` | constants carried as units |

The original record located only (1). Fixing it alone would have left (2) and
(3) red while the documented reproducer went green -- the exact shape of a fix
that looks complete and is not. (2) and (3) were fixed by giving `IndyInfo`,
`JitStringConcatSite` and `ResolvedCallSite::StringConcat` `Vec<u16>` /
`Arc<[u16]>` recipes and constants, resolved through
`resolve_concat_constant_units`, which prefers the constant pool's exact-units
side table (`ConstantPool::get_utf8_wide`, already present for ANTLR's
`_serializedATN`) and falls back to `encode_utf16` for the lossless kinds.

The recipe is now walked unit-by-unit against `TAG_ARG_UNIT` / `TAG_CONST_UNIT`
rather than `char`-by-`char`.

### Verification

`probes/StringConcatSurrogateProbe`, 8 assertions, against a HotSpot 25 control:

* **pre-fix binary: 5 of 8 DIFF** -- rows 1a/1b/1c, 2 and 3 all produced U+FFFD.
  Row `split pair rejoins` returned `FFFD FFFD` for a *well-formed* pair split
  across two arguments, which the original record had not noticed.
* **post-fix: 8 of 8 SAME**, unit-for-unit identical to HotSpot, in `--jit`,
  `--nojit` and `--jdk-only` alike. `arg.hashCode()` is 1829648 on both VMs.

The control row (`new String(char[])`) was already SAME before the fix, which is
what made the bug misattributable in the first place.

### Cost

The record required concat-heavy timings before landing. A-B-B-A interleaved,
three rounds, identical checksums per workload (`probes/StringConcatCostProbe`,
medians in ms):

| workload | pre | post | delta |
|---|---:|---:|---:|
| ascii | 198.5 | 203.5 | +2.5% |
| latin1 | 280.5 | 239.5 | **-14.6%** |
| utf16 | 337.0 | 270.0 | **-19.9%** |
| mixed | 156.5 | 159.0 | +1.6% |

Units *removed* a transcode for the non-ASCII cases: a `String` argument used to
be decoded UTF-16 to UTF-8 in and re-encoded UTF-8 to UTF-16 out. The all-ASCII
case pays a widen-to-`u16`-then-narrow-to-`u8` that the old `copy_nonoverlapping`
did not, which is the +2.5%; its distributions overlap heavily and the host was
running other builds, so treat it as "no worse than noise", not as a measured
result in either direction.

`populate_java_string_fields` had to be bulk-written for this to hold at all --
both its LATIN1 and UTF16 branches wrote one boxed `Value` per byte, which on
the newly-hot concat path would have cost far more than the transcode it
replaced.

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
