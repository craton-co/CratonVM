# `Character.charCount` lost the sign, and the case tables above the BMP had never been looked at

> **Status: FIXED-UNVERIFIED** in `native-builtins/src/lang_math.rs`. Every
> HotSpot number below is **MEASURED** on Microsoft OpenJDK 25.0.3+9-LTS. Every
> CratonVM "after" is **PREDICTED** — lane E7 may not build and may not run the
> VM, so no binary carrying these edits exists. The predictions are not
> guesses: the new bodies were transliterated back into Java, fed the run
> tables **parsed out of the working-tree `lang_math.rs`**, and diffed against
> HotSpot over all 1,114,112 code points plus the out-of-range `int` domain.
> That witness is `Witness.java` and it is mutation-checked.

Lane E7, 2026-08-13. Files touched: `native-builtins/src/lang_math.rs` only.

---

## 1. The defect the fixture caught

`RJdkIntrinsics2 --only=charcls` fails on the shipping binary at

```
Character.charCount(-1) must be 1 — below MIN_SUPPLEMENTARY, not a panic
```

**It is not a panic, and it never could have been.** The body had no indexing,
no `unwrap`, and no arithmetic that can overflow:

```rust
let cp = match args.first() {
    Some(Value::Int(v)) => *v as u32,     // <-- the whole defect
    _ => return Ok(Some(Value::Int(1))),
};
Ok(Some(Value::Int(if cp > 0xFFFF { 2 } else { 1 })))
```

`*v as u32` widens `-1` to `0xFFFF_FFFF`, which is above `MIN_SUPPLEMENTARY`,
so the method answered **2**. Java's contract is one line with no error case and
a **signed** compare — `codePoint >= MIN_SUPPLEMENTARY_CODE_POINT ? 2 : 1` — so
every negative `int` was wrong. Measured on HotSpot 25.0.3+9:

| input | HotSpot | CratonVM before | after (predicted) |
|---|---|---|---|
| `Integer.MIN_VALUE` | 1 | **2** | 1 |
| `-65536` | 1 | **2** | 1 |
| `-2` | 1 | **2** | 1 |
| `-1` | 1 | **2** | 1 |
| `0` / `0xFFFF` | 1 | 1 | 1 |
| `0x10000` / `0x10FFFF` | 2 | 2 | 2 |
| `0x110000` / `Integer.MAX_VALUE` | 2 | 2 | 2 |

Wrong on all 2^31 negative inputs; right on all 2^31 non-negative ones. The fix
is to stop widening: read the argument as `i32` and compare `cp >= 0x10000`.

`charCount` is not a corner: `String.codePointCount`, `String.offsetByCodePoints`,
`StringBuilder.appendCodePoint` and every `codePoints()` walk are written in
terms of it, so a `2` where the JDK says `1` is an off-by-one that propagates
into index arithmetic rather than into a visible exception.

---

## 2. The family: measured one method at a time, NOT fixed by pattern

The obvious next move — "an unsigned cast on a code point is a bug, sweep the
family" — is wrong, and applying it would have turned two correct methods into
regressions. **The JDK is inconsistent here on purpose and the inconsistency is
the specification.** `CharContract.java` prints one row per
`(method, out-of-range input)` on HotSpot; the full 1,084-row transcript is
`contract.txt` in the lane scratchpad. The contracts:

| method | negative `int` | `> 0x10FFFF` | mechanism in the JDK |
|---|---|---|---|
| `charCount` | **1** | 2 | `codePoint >= MIN_SUPPLEMENTARY` — **SIGNED**, no validation |
| `isBmpCodePoint` | `false` | `false` | `(codePoint >>> 16) == 0` — **UNSIGNED** shift |
| `isValidCodePoint` | `false` | `false` | plane compare on `codePoint >>> 16` — **UNSIGNED** |
| `isSupplementaryCodePoint` | `false` | `false` | signed range test, both ends |
| `toChars` / `toString(int)` | `IllegalArgumentException` | `IllegalArgumentException` | via the two predicates above |
| `highSurrogate` / `lowSurrogate` | arithmetic, no validation | arithmetic | `0xD800 + (cp >> 10)` etc. — `highSurrogate(-1)` is `55231` |
| `isDigit`/`isLetter`/`isWhitespace`/`isSpaceChar`/`isUpperCase`/`isLowerCase`/`isTitleCase`/`isAlphabetic`/`isDefined`/`isMirrored`/`isIdeographic`/`isJava*Identifier*`/`isUnicode*Identifier*`/`isEmoji*` | `false` | `false` | `CharacterData.of` falls to `CharacterDataUndefined` |
| `getType` | `0` (`UNASSIGNED`) | `0` | ditto |
| `getDirectionality` | `-1` | `-1` | ditto |
| `toUpperCase`/`toLowerCase`/`toTitleCase` `(I)I` | the input | the input | every unmapped `int` maps to itself |
| `getNumericValue(int)` / `digit(int,int)` | `-1` | `-1` | ditto |
| `forDigit(int,int)` | `U+0000` | `U+0000` | NUL for any digit outside `0..radix` or radix outside `2..=36`; never throws |

Checked every registered body in `lang_math.rs` against its own row. **Only
`charCount` disagreed.** In particular:

* `native_character_is_bmp_code_point`'s `*v as u32` + `cp <= 0xFFFF` is the
  *same predicate over the same 2^32 inputs* as the JDK's `(codePoint >>> 16) == 0`.
  A "signed" rewrite would make `Character.toChars(-1)` return
  `new char[]{(char) 0xFFFF}` instead of throwing `IllegalArgumentException`, because
  `toChars` is unregistered JDK bytecode that reaches its throw *through this
  native*.
* `native_character_is_valid_code_point` is already `i32`, and agrees with the
  JDK's unsigned form on all 2^32 inputs.
* `isHighSurrogate`/`isLowSurrogate` are registered only as `(C)Z`, so no
  argument can be out of range.
* `digit(CI)I` and `forDigit(II)C` already reject the radix as `i32` **before**
  any `as u32` — the `char::from_digit` panic-above-radix-36 hazard that
  W7-98(a) closed is still closed.
* No `as u8` / `as u16` truncation exists anywhere in `lang_math.rs`'s
  `Character` family. (The `HexFormat.isHexDigit(0x661)` truncation that shape
  is named after lives in `phases_late.rs` and is already fixed there.)

Both correct-by-unsigned-cast bodies now carry a doc comment saying so, and
`charCount`'s says which two must not be changed with it. Guarding against the
next sweep is cheaper than re-deriving the transcript.

**Lesson worth carrying: a cast that is wrong in one member of a family is not
evidence about its siblings.** "Fix them all the same way" is how a correct
member becomes a regression.

---

## 3. The part nothing had ever measured: the supplementary planes

W7-95(C1) drove ten `Character` classifiers to zero divergences by sweeping
**all 65,536 BMP code points** on both VMs. It stopped at `0xFFFF`. Above it:

| | supplementary code points on JDK 25 |
|---|---|
| `isUpperCase` true | **804** |
| `isLowerCase` true | **927** |
| `toUpperCase(cp) != cp` | **282** |
| `toLowerCase(cp) != cp` | **282** |

in DESERET, OSAGE, VITHKUQI, LATIN EXTENDED-F, OLD HUNGARIAN, GARAY, WARANG
CITI, MEDEFAIDRIN, MATHEMATICAL ALPHANUMERIC SYMBOLS, LATIN EXTENDED-G,
CYRILLIC EXTENDED-D, ADLAM and ENCLOSED ALPHANUMERIC SUPPLEMENT. Not one of
them had been compared against HotSpot by anything.

Every one of those answers came out of **Rust's** Unicode database
(`char::is_uppercase`, `char::to_uppercase`), corrected by two override lists
and one "unassigned on JDK 25" list — all three derived from the BMP-only sweep,
so none of them could cover a single code point above `0xFFFF`.

### The two databases are provably not the same version

Measured, not assumed. `rustc --version` on this checkout is **1.96.0
(2026-05-25)**; `java.specification.version` is **25**, and JDK 25 is on
**Unicode 16.0** (confirmed from this side: GARAY `U+10D50`/`U+10D70`, a Unicode
16.0 addition, is `defined=true`, `getType=1/2`, and case-pairs correctly).
Meanwhile HotSpot 25 answers `getType=0 (UNASSIGNED), isDefined=false` for
`U+A7CE`, `U+A7CF`, `U+A7D2`, `U+A7D4`, `U+A7F1` — the five Latin Extended-D
code points the shipping CratonVM binary was measured answering *cased* for.
A toolchain that assigns code points JDK 25 calls unassigned is on a **strictly
newer Unicode than 16.0**, i.e. 17.0. That skew is not a BMP phenomenon; nothing
confines it to `0x0000..0xFFFF`. It simply had never been looked for above it.

### What the old derivation's structural rule did above the BMP

One thing here *is* measurable from this side, and it came out clean, so it is
recorded rather than implied. W7-98(b)'s arity rule ("take
`char::to_uppercase` only if it yields exactly one `char`, else return the
input") is wrong for any code point whose Unicode FULL uppercase is multi-char
but whose Java SIMPLE uppercase still maps. Counting those on HotSpot:

* **27** in the BMP — exactly the ypogegrammeni family
  (`U+1F80..U+1F87`, `U+1F90..U+1F97`, `U+1FA0..U+1FA7`, `U+1FB3`, `U+1FC3`,
  `U+1FF3`), which is precisely what `JAVA_SIMPLE_UPPERCASE_OVERRIDES` listed.
* **0** in the supplementary planes.

So the arity rule was not hiding a second residual up there. The exposure above
the BMP was the version skew alone — and *that* one cannot be measured from a
machine that cannot execute `char::is_uppercase`.

### The fix: stop deriving

Per the method that worked for W7-95(C1): where a small derivation can be
replaced by an explicit enumeration, enumerate. Four run tables, generated by
executing the JDK's own methods over all 1,114,112 code points on OpenJDK
25.0.3+9:

| table | encoding | size |
|---|---|---|
| `JAVA_UPPERCASE_RUNS` | `(first, last)` | 656 runs, 1,978 code points |
| `JAVA_LOWERCASE_RUNS` | `(first, last)` | 675 runs, 2,569 code points |
| `JAVA_TO_UPPER_RUNS` | `(first, last, stride, delta)` | 205 runs, 1,477 mapped code points |
| `JAVA_TO_LOWER_RUNS` | `(first, last, stride, delta)` | 187 runs, 1,460 mapped code points |

The boolean tables reuse the existing `(u32, u32)` shape and the existing
`in_code_point_runs` binary search, exactly like `JAVA_LETTER_RUNS`.

The mapping tables add one concept, and it earns its place: `stride`. Every code
point `first, first+stride, ...` up to `last` maps to itself plus `delta`;
everything else maps to itself. Latin Extended-A and Cyrillic are hundreds of
*alternating* case pairs (`U+0100`/`U+0101`, `U+0102`/`U+0103`, ...), and one
`stride == 2` run swallows a whole block of them. The identical content in plain
`(first, last, delta)` runs needs **690 and 674** rows instead of 205 and 187 —
460 lines of table instead of 140.

Deleted with the derivation: `JAVA_SIMPLE_UPPERCASE_OVERRIDES` (30 rows),
`JAVA_SIMPLE_LOWERCASE_OVERRIDES` (3 rows), `JAVA_UNASSIGNED_ON_JDK25` (5 rows),
`case_override`, the `char::from_u32` / `to_uppercase` / `to_lowercase` calls and
the `ch == 0x0295` special case. The tables subsume all of them.

Two properties come free and are worth naming, because both used to be guards:

* **A lone surrogate survives.** `U+D800..U+DFFF` appear in no run, so the
  lookup returns the input — which is what HotSpot answers. W7-98(c) needed an
  explicit `char::from_u32(..) == None` arm for this; there is no longer a
  `char` in the path to fail on.
* **The function is total.** An `int` outside `0..=0x10FFFF` is in no run and
  maps to itself, which is HotSpot's answer for `0x110000` and for
  `Integer.MIN_VALUE` alike.

`mapped_in_stride_runs` uses `checked_add_signed(..).unwrap_or(cp)` rather than a
cast. No reachable input can overflow; an unreachable one must not panic,
because a Rust panic is not a Java throwable.

---

## 4. Executed evidence for the constants

`Witness.java` — **a source witness: it reads the working tree, not a copy.** It
parses the four `const` literals straight out of
`native-builtins/src/lang_math.rs`, asserts the runs are sorted and disjoint (the
binary-search precondition), transliterates the new Rust bodies, and diffs every
column against HotSpot 25.

```
parsed from working tree: UP=656 LO=675 TU=205 TL=187 runs (sorted, disjoint)

== 0..0x10FFFF (1,114,112 code points), transliterated vs HotSpot 25 ==
  isUpperCase        0 mismatches
  isLowerCase        0 mismatches
  toUpperCase(I)I    0 mismatches
  toLowerCase(I)I    0 mismatches
  charCount          0 mismatches
  isBmpCodePoint     0 mismatches
  isValidCodePoint   0 mismatches
  isISOControl       0 mismatches
  toUpperCase(C)C    0 mismatches
  toLowerCase(C)C    0 mismatches

== out-of-range ints ==
  MIN_VALUE, -2147483647, -1000000, -65536, -2, -1,
  0x110000, 0x110001, 0x200000, 0x7FFFFFFE, MAX_VALUE       all OK

TOTAL MISMATCHES: 0 in-range + 0 out-of-range
```

**Mutation-checked**, because a witness that cannot fail measures nothing.
Shifting `JAVA_UPPERCASE_RUNS[300]` down by 2 turns up 1 mismatch; adding 1 to
`JAVA_TO_UPPER_RUNS[100].delta` turns up 12. `MutAndArity.java` asserts both are
non-zero and aborts if either is not.

### What this proves and what it does not

It proves the **constants** and the **lookup logic**: the predicate the compiled
Rust will compute is byte-identical to HotSpot 25's over the entire code point
space and over the out-of-range `int` domain. It does not prove the Rust
compiles — no build ran — and it does not prove registry ownership. Ownership
was checked by reading: W7-98 §1 recorded that `lib.rs` re-registered all four
case-mapping triples as `Bridge` copies that WON under last-write-wins; that
block is now a tombstone comment at `native-builtins/src/lib.rs` ~:19526 and no
other file registers a `java/lang/Character` triple that `lang_math.rs` also
registers. So unlike W7-98's, these edits are not inert.

---

## 5. Which `--only=charcls` checks flip

`charcls` runs 86 checks and `check()` throws on the first failure, so the
family currently **aborts at check 52** and checks 53–86 have never executed on
any CratonVM binary.

| check | assertion | now | predicted |
|---|---|---|---|
| 1–51 | fixture integrity, `isLowerCase`/`isUpperCase`/`isLetter` incl. the astral rows 36–38, `isISOControl`, `forDigit`, `charCount(U+1F600)`, `charCount(U+FFFF)` | pass | pass |
| **52** | `Character.charCount(-1) == 1` | **FAIL** (answers 2) | **PASS** |
| 53–58 | `isValidCodePoint` ×3, `isBmpCodePoint` ×3 | not reached | pass |
| 59–62 | `isHighSurrogate`/`isLowSurrogate` halves | not reached | pass |
| 63–64 | `toUpperCase(U+10428) == U+10400`, `toLowerCase(U+10400) == U+10428` — **the astral `(I)I` case mappings, first execution ever** | not reached | pass |
| 65–66 | `toUpperCase/toLowerCase(int U+D800) == U+D800` | not reached | pass |
| 67–69 | `toUpperCase(U+1F600)`, `toUpperCase(U+00DF)`, `toUpperCase(-1)` all identity | not reached | pass |
| 70–73 | `digit(char, radix)` at the radix boundaries | not reached | pass |
| 74 | `Character.digit(U+1D7CE, 10) == 0` — **`(II)I`, unregistered** | not reached | pass in real-JDK mode; **see the caveat below** |
| 75–76 | `digit((char) U+0F20, 10)`, `getNumericValue(U+00BC) == -2` | not reached | pass |
| 77–80 | `isEmojiModifier` / `isEmojiModifierBase` | not reached | pass |
| 81–86 | ASCII negative controls, `isLetterOrDigit(U+1D7CE)`, `Character.toString('a')` | not reached | pass |

Expected new line: `CK RJdkIntrinsics2 charcls=86`.

**The one caveat, stated rather than buried.** Check 74 calls
`Character.digit(int, int)`, which is **not** registered — in real-JDK mode it
runs JDK bytecode and is correct. `RJdkIntrinsics2` is in `CORE_CLASSES`, so it
also runs in synthetic-JDK mode, where that bytecode does not exist. Whatever
check 74 does there is a **new discovery**, not a regression: the family aborted
at check 52 in every arm, so nothing beyond it has ever run in either.
`isLetterOrDigit(U+1D7CE)` at check 85 has the same first-execution status but
is registered here and is table-backed (`U+1D7CE` is in
`JAVA_SUPPLEMENTARY_DIGIT_RUNS`), so it does not depend on the arm.

**Do not "fix" check 74 by registering `digit(II)I` against
`native_character_digit`.** That body routes through `java_char_digit`, whose
`JAVA_DIGIT_RUNS` table is BMP-only, so it answers `-1` for `U+1D7CE` — it would
break the check in real-JDK mode, where it currently passes.

---

## 6. What changed in `native-builtins/src/lang_math.rs`

| symbol | change |
|---|---|
| `native_character_char_count` | `*v as u32` + `cp > 0xFFFF` -> `*v` (`i32`) + `cp >= 0x10000`; doc records the transcript and names the two siblings that must NOT follow |
| `native_character_is_bmp_code_point` | **body unchanged**; doc added stating the unsigned cast IS the JDK's `>>> 16` and why a signed rewrite breaks `toChars` |
| `native_character_is_valid_code_point` | **body unchanged**; doc added |
| `native_character_is_upper_case` / `_is_lower_case` | `char::is_uppercase`/`is_lowercase` + `JAVA_UNASSIGNED_ON_JDK25` + the `0x0295` case -> `in_code_point_runs(JAVA_UPPERCASE_RUNS/JAVA_LOWERCASE_RUNS, ..)` |
| `character_case_map` | `char::to_uppercase` arity rule / `to_lowercase().next()` + two override tables -> `mapped_in_stride_runs(JAVA_TO_UPPER_RUNS/JAVA_TO_LOWER_RUNS, ..)` |
| `mapped_in_stride_runs` | **new** — `(first, last, stride, delta)` lookup, total, panic-free |
| `JAVA_UPPERCASE_RUNS`, `JAVA_LOWERCASE_RUNS`, `JAVA_TO_UPPER_RUNS`, `JAVA_TO_LOWER_RUNS` | **new** |
| `JAVA_SIMPLE_UPPERCASE_OVERRIDES`, `JAVA_SIMPLE_LOWERCASE_OVERRIDES`, `JAVA_UNASSIGNED_ON_JDK25`, `case_override` | **deleted** |

No other file was touched. The `(C)C` and `(I)I` overloads keep sharing
`character_case_map`, so they still cannot drift.

---

## 7. Nominations

### N1 — `regression-suite/src/RJdkIntrinsics2.java`: the failure message states a mechanism that is not the mechanism

The fixture's own wording sent this lane looking for a Rust panic. There is
none, and the method cannot panic.

*old, verbatim:*

```java
        check(Character.charCount(OPAQUE_I[2]) == 1,
                "Character.charCount(-1) must be 1 — below MIN_SUPPLEMENTARY, not a panic");
```

*new:*

```java
        check(Character.charCount(OPAQUE_I[2]) == 1,
                "Character.charCount(-1) must be 1 — the compare is SIGNED; widening the"
                        + " argument to unsigned answers 2");
```

### N2 — `docs/known-issues/jdk-only/W7-98-character-unicode.md`: two rows of its result table are now closed, and its stated residual is gone

W7-98's result table still carries `toUpperCase 2,153 -> 27` (the ypogegrammeni
residual) and `isUpperCase / isLowerCase 3 / 3 (Unicode version skew)`. Both are
closed by the enumeration, and the record's §"(b)" residual sentence — "That
code point stays wrong … The exact fix is to stop shadowing the JDK bytecode at
all" — is no longer the state of the tree.

*old, verbatim (the two table rows):*

```
| `toUpperCase` | 2,153 | **27** | needs N1 to take effect |
| `toLowerCase` | 2,051 | **3** | needs N1 to take effect |
| `isUpperCase` / `isLowerCase` | 3 / 3 | 3 / 3 | Unicode version skew |
```

*new:*

```
| `toUpperCase` | 2,153 | **0** | E7-1: enumerated from JDK 25, all planes |
| `toLowerCase` | 2,051 | **0** | E7-1: enumerated from JDK 25, all planes |
| `isUpperCase` / `isLowerCase` | 3 / 3 | **0 / 0** | E7-1: enumerated; the skew list is gone |
```

W7-98's headline recommendation — "the class does not need these natives; it
needs them gone" — still stands and is **not** claimed to be done here. Six
`Character` triples in `lang_math.rs` now answer out of JDK-25-generated tables
rather than Rust's Unicode DB, which makes them correct but does not make them
necessary.

### N3 — `native-builtins/src/case_map.rs` + `native-builtins/src/lang_string.rs`: the same shape, one class over

`String.toUpperCase`/`toLowerCase` still **derive** from Rust's `char` tables and
then subtract a measured skew list (`JDK_UNMAPPED_CASE_CODE_POINTS`) — which is
exactly the arrangement `lang_math.rs` just retired, with the same rustc-1.96
(Unicode 17.0) vs JDK-25 (Unicode 16.0) skew underneath it. `case_map.rs`'s own
module doc already names the cause ("Rust's `char` tables are a DIFFERENT, NEWER
Unicode than the JDK's"). No edit is nominated, because that file is being
actively changed by another lane and because `String`'s mappings are the FULL
ones (multi-character, locale-conditional), so the fix is not a copy of this
one. What is nominated is the **measurement**: sweep
`String.valueOf(Character.toChars(cp)).toUpperCase(Locale.ROOT)` over
`0..0x10FFFF` on HotSpot and compare against the derivation. If the skew list
was also built from a BMP-only sweep, its supplementary half is unmeasured for
the same reason this record's was.

### N4 — no edit, a warning for whoever touches this family next

`Character.digit(int, int)`, `getNumericValue(int)`, `toChars`, `toString(int)`,
`highSurrogate`, `lowSurrogate`, `isSupplementaryCodePoint`, `getType`,
`isSpaceChar`, `toTitleCase`, `isAlphabetic`, `isDefined`, `getDirectionality`,
`isMirrored` and the identifier predicates are **deliberately unregistered** and
run real `CharacterData` bytecode, which W7-98 measured correct on all 65,536
BMP code points including hot. Registering any of them is a regression risk, and
`digit(II)I` specifically would be one today (§5).

---

## 8. Reproducing

Lane scratchpad (`.../scratchpad/e7/`), all HotSpot-only, no CratonVM binary
required:

| file | what it does |
|---|---|
| `CharContract.java` | the 1,084-row out-of-range contract transcript -> `contract.txt` |
| `GenFinal.java` | emits the four table literals from a full `0..0x10FFFF` sweep |
| `GenCase.java` / `GenCase2.java` | the plain vs stride encodings, and the 690/674 -> 205/187 comparison |
| `Witness.java` | parses the tables out of the working-tree `lang_math.rs` and diffs ten columns against HotSpot |
| `MutAndArity.java` | mutation-checks the witness; measures the arity residual (27 BMP / 0 supplementary) |
| `Skew.java` | the `U+A7CE` / GARAY evidence that the two Unicode databases differ |

Re-run `Witness` after any edit to the four tables. It reads the source file, so
it cannot go stale against a copy.
