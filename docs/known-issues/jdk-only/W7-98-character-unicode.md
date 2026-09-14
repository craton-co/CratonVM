# `java.lang.Character` answered Rust's Unicode, not Java's

> **STATUS 2026-08-12: partially fixed in `native-builtins/src/lang_math.rs`;
> the exact fix is a set of DEREGISTRATIONS this lane could not make.**
>
> The interesting finding is not any single wrong answer. It is that
> `java.lang.Character`'s classification and case-mapping natives were a
> *reimplementation* of the Unicode database, and Rust's database is not
> Java's — it is a different version, exposes different properties, and answers
> a different question for case mapping. Measured over **all 65,536 BMP code
> points** against HotSpot 25.0.3.9, ten of these methods disagree with Java on
> between 3 and 2,153 code points each.
>
> Meanwhile `Character.getType`, `isSpaceChar`, `toTitleCase`, `isAlphabetic`,
> `isDefined`, `getDirectionality`, `isMirrored` and the identifier predicates
> are **not** registered as natives, run the real `CharacterData` bytecode, and
> match HotSpot on **all 65,536** BMP code points — including hot, after 6
> million invocations. The class does not need these natives to be correct; it
> needs them gone.

Measured on `/c/craton/jdkonly-wave2-target/release/cratonvm.exe`
(mtime 2026-08-12 20:18 — a binary that predates every change described here,
so all "before" rows are the true before-state), oracle `java` =
OpenJDK 25.0.3+9-LTS (Microsoft). All comparisons are of `int` values, never
rendered characters.

Line numbers are as of `26e69258a`. This checkout advanced three times during
the lane, so anchor on the symbol names — they are given alongside every
citation.

---

## 1. Which registration actually serves each triple

This has to be settled before any edit, because four of the sixteen triples are
registered **twice** and the `lang_math.rs` copy loses. From
`--dump-native-registry` (schema-v2 census, `owns_slot` + `invocations` +
`overwrote`), on a run that exercised every method:

| triple | registered by | owns slot | invocations |
|---|---|---|---|
| `toLowerCase(C)C` | `lang_math.rs:480` (`intrinsic`) | **false** | **0** |
| `toLowerCase(C)C` | `lib.rs:19526` (`bridge`) | **true** | 32 |
| `toUpperCase(C)C` | `lang_math.rs:474` (`intrinsic`) | **false** | **0** |
| `toUpperCase(C)C` | `lib.rs:19541` (`bridge`) | **true** | 32 |
| `toLowerCase(I)I` | `lang_math.rs:497` (`intrinsic`) | **false** | **0** |
| `toLowerCase(I)I` | `lib.rs:19556` (`bridge`) | **true** | 34 |
| `toUpperCase(I)I` | `lang_math.rs:503` (`intrinsic`) | **false** | **0** |
| `toUpperCase(I)I` | `lib.rs:19572` (`bridge`) | **true** | 32 |

The `lib.rs` closures are **verbatim copies** of the `lang_math.rs` bodies and
they carry `overwrote: intrinsic` — last-write-wins. **A fix to `lang_math.rs`
alone changes nothing observable for the four case-mapping triples.** The fixes
below are still made there (that is the canonical home, and the duplicates are
nominated for deletion), but they are inert until §5 N1 lands.

Every other `java.lang.Character` triple in this record is owned by
`lang_math.rs` (`owns_slot: true`, non-zero invocations), so those edits do take
effect. The full census listing is 46 rows; the four above are the only
duplicated ones.

---

## 2. Before-state: the whole BMP, both VMs

`BmpSweep.java` prints one line per code point `0..0xFFFF` with twelve `int`
columns and is run on both VMs; the table is the count of code points whose
answer differs.

| method | divergent BMP code points (of 65,536) |
|---|---|
| `getType` (**not** registered — real `CharacterData`) | **0** |
| `isUpperCase` | 3 |
| `isLowerCase` | 3 |
| `isWhitespace` | 8 |
| `isDigit` | 360 |
| `digit(c,10)` | 360 |
| `getNumericValue` | 784 |
| `isLetter` | 957 |
| `isLetterOrDigit` | 1,257 |
| `toLowerCase` | 2,051 |
| `toUpperCase` | 2,153 |

The `getType` row is the load-bearing one: it is the same
`Character.f(char) -> Character.f(int) -> CharacterData.of(cp).f(cp)` chain
every method here would take if it were not shadowed, and it is **exact**.

### The three mechanisms

**(a) Different Unicode table.** Rust's predicates are not Java's predicates.

| row | HotSpot 25 | CratonVM (before) |
|---|---|---|
| `isWhitespace(U+00A0)` NBSP | 0 | **1** |
| `isWhitespace(U+0085)` NEL | 0 | **1** |
| `isWhitespace(U+2007)` FIGURE SPACE | 0 | **1** |
| `isWhitespace(U+202F)` NARROW NBSP | 0 | **1** |
| `isWhitespace(U+001C..U+001F)` FS/GS/RS/US | 1 | **0** |
| `isDigit(U+0660)` ARABIC-INDIC ZERO | 1 | **0** |
| `digit(U+0669, 10)` | 9 | **-1** |
| `getNumericValue(U+0669)` | 9 | **-1** |
| `getNumericValue(U+2160)` ROMAN NUMERAL ONE | 1 | **-1** |
| `getNumericValue(U+00B2)` SUPERSCRIPT TWO | 2 | **-1** |
| `isLetter(U+2160)`, `isLetter(U+3007)` (`Nl`) | 0 | **1** |
| `isLetterOrDigit(U+2160)` | 0 | **1** |

`String.isBlank`/`strip` inherit all of it — measured: `" x".strip()`
(`U+2007`) returned length **1** here and **2** on HotSpot, i.e. a character was
deleted; `" ".isBlank()` was `true` here and `false` on HotSpot.

**(b) Different question: full vs simple case mapping.** The lane brief called
this an arity bug — "`to_uppercase()` returns an iterator and `.next()` takes
only the first char". That is half of it, and the half it misses changes the
fix. Rust's `char::to_uppercase` is the Unicode **full** uppercase mapping
(`SpecialCasing.txt`); Java's `Character.toUpperCase` is the **simple** mapping
(`UnicodeData.txt` field 12). They differ whenever the full mapping is
multi-char, and the simple mapping is then *usually* — but not always — the
identity.

Of the **105** non-surrogate `toUpperCase` divergences:

* **78** have `java == input` (`U+00DF` -> `ß`, `U+FB00` -> `ﬀ`, `U+0149`,
  `U+01F0`, `U+0390`, `U+03B0`, `U+1E96`, `U+1F50`, …). We answered the first
  char of the full mapping: `'S'`, `'F'`, `'J'`, `'H'`. **The "return the input
  unchanged" rule fixes exactly these 78.**
* **27** have `java != input`: `U+1F80..U+1FAF`, `U+1FB3`, `U+1FC3`, `U+1FF3`
  (the ypogegrammeni family) have a two-char *full* mapping **and** a
  single-char *simple* mapping — `U+1FB3` -> `U+1FBC`. No Rust std API exposes
  the simple mapping, so the arity rule leaves these at the identity. Still
  wrong, but wrong as the identity rather than as a different letter
  (`U+0391`), which is the safer failure.

The rule is **not symmetric**, and applying it to `toLowerCase` would have
*regressed* a correct row: `U+0130` (İ) has a two-char full lowercase
(`i` + `U+0307`) whose first char **is** Java's simple mapping, so both VMs
answer `105` today. `toLowerCase` has only **3** non-surrogate divergences in
the entire BMP (`U+A7CE`, `U+A7D2`, `U+A7D4` — Unicode *version* skew: JDK 25
reports them `UNASSIGNED`, Rust's tables know them), and none is arity-related.
So lowercase keeps `.next()`.

**(c) Surrogates became NUL — silent data corruption.** Ranked first.
`char::from_u32` answers `None` for `U+D800..U+DFFF` (not scalar values), and
the bodies finished `.unwrap_or('\0')`.

| row | HotSpot 25 | CratonVM (before) |
|---|---|---|
| `toUpperCase((char) 0xD800)` | 55296 | **0** |
| `toLowerCase((char) 0xDFFF)` | 57343 | **0** |
| `Character.toString((char) 0xD800).charAt(0)` | 55296 | **0** |
| `new StringBuilder().append((char) 0xD800)` `.charAt(0)` | 55296 | **65533** |
| `"\uD800A".codePointAt(0)` | 55296 | **65533** |

That is **2,048 code points × 2 methods** on `toUpperCase`/`toLowerCase` alone
— every lone surrogate, which is the normal state of a `char` halfway through a
surrogate pair and of any text chunked on a non-code-point boundary.

The `65533` rows are a **different** and broader defect, and the root cause is
now pinned: `String.codePointAt`'s native body (`lang_string.rs:4991`) is
correct — it re-encodes to UTF-16 and returns the code unit — but it gets its
input from `ctx.read_string(this)`, which hands back a Rust `String`. A lone
surrogate cannot survive that type, so it is already `U+FFFD` before any native
logic runs. `new String(char[])` preserves it (measured), so the *storage* is
fine; the **accessor** is lossy. Same for `StringBuilder.append(char)`
(`lang_string.rs:203`). See §5 N4.

---

## 3. What changed, per mechanism

All in `native-builtins/src/lang_math.rs`.

**(a) `isWhitespace` — exact, and validated over the whole BMP.**
`char::is_whitespace` is the Unicode **White_Space** property,
`Zs ∪ Zl ∪ Zp ∪ {U+0009..U+000D, U+0085}`. Java's rule is the javadoc's:
a `Zs`/`Zl`/`Zp` character that is **not** a non-breaking space
(`U+00A0`, `U+2007`, `U+202F`), **or** `U+0009..U+000D`, **or**
`U+001C..U+001F`. Subtracting `U+0009..U+000D` and `U+0085` from White_Space
leaves exactly `Zs ∪ Zl ∪ Zp` = Java's `isSpaceChar`, so the derivation is
exact rather than a sampled table, and the three excluded code points are the
javadoc's own list. **Verified: 0 mismatches over all 65,536 BMP code points.**
8 -> **0**.

**(b) `toUpperCase(C)C` / `(I)I` — simple-vs-full, via a shared
`character_case_map` helper** so the two overloads cannot drift (the JDK's
`(C)C` is literally `(char) toUpperCase((int) c)`). A mapping that is not
exactly one `char` returns the input. 78 of the 105 non-surrogate divergences
fixed; 27 documented residual. `toLowerCase` keeps `.next()` for the measured
reason above.

**(c) Surrogate preservation** in `character_case_map` (returns the input when
`char::from_u32` is `None`) and in `Character.toString(char)`, which now hands a
surrogate to `String.valueOf(char)` — verified **not** native-registered, so
that is a plain call into real JDK bytecode with no native re-entry, and
measured correct on this VM for every lone surrogate. 2,048 × 2 rows fixed
(pending §5 N1 for the two case-mapping triples).

**(a) `isDigit` / `digit(char,int)` — exact, and the fix was already in the
file.** `java_char_digit(c, radix)` sits ~500 lines above these natives, backed
by `JAVA_DIGIT_RUNS` — 38 runs generated from JDK 25 itself by walking every
code point. It had **exactly one caller**, `java_parse_signed`. So
`Integer.parseInt("٦٦")` answered 66 while `Character.isDigit('٦')` answered
`false`, in the same VM, from the same module, ~500 lines apart. Verified over
all 65,536 BMP code points: `java_char_digit(c,10)` reproduces
`Character.digit(char,10)` and `java_char_digit(c,10).is_some()` reproduces
`Character.isDigit(char)` with **zero** mismatches. 360 -> **0** for both.

**(a) `getNumericValue` — partial.** Same helper at radix 36: 784 -> **372**.
The residual is `Nl`/`No` numeric values (`U+2160` -> 1, `U+00B2` -> 2) and
Java's `-2` sentinel for non-integral values (`U+00BD`), which need the JDK's
numeric table.

**(a) `isLetterOrDigit` — composed, not independently guessed.**
`char::is_alphanumeric` is `Alphabetic ∪ N*`, so on top of `isLetter`'s error it
independently added `No` (`U+00B2`, `U+00BD`). Composing it the way the JDK does
(`isLetter(c) || isDigit(c)`) takes 1,257 -> **957**, which is *exactly*
`isLetter`'s count: this method now contributes no error of its own.

**`isLetter` — not fixed, and deliberately not approximated.** Java's `isLetter`
is `Lu|Ll|Lt|Lm|Lo`; Rust's `is_alphabetic` is the **Alphabetic** property
(`L* ∪ Nl ∪ Other_Alphabetic`), a strictly larger set. All 957 divergences are
in one direction (we say `true`, Java says `false`). Rust std exposes neither
`Nl` nor `Other_Alphabetic`. The closest derivation,
`is_alphabetic() && !is_numeric()`, was measured over the whole BMP and still
misses **892** — it removes `Nl` but not the combining marks (`U+0345`,
`U+0483..`, `U+05B0..`). Not applied: it trades an exact, explainable rule for a
marginally smaller wrong number.

### Result

| method | before | after | note |
|---|---|---|---|
| `isWhitespace` | 8 | **0** | exact |
| `isDigit` (char) | 360 | **0** | exact, BMP |
| `digit(c,10)` | 360 | **0** | exact, BMP |
| `getNumericValue` | 784 | **372** | `Nl`/`No`/`-2` residual |
| `isLetterOrDigit` | 1,257 | **957** | now exactly `isLetter\|\|isDigit` |
| `isLetter` | 957 | 957 | no std route — N2 |
| `toUpperCase` | 2,153 | **0** | E7-1: enumerated from JDK 25, all planes |
| `toLowerCase` | 2,051 | **0** | E7-1: enumerated from JDK 25, all planes |
| `isUpperCase` / `isLowerCase` | 3 / 3 | **0 / 0** | E7-1: enumerated; the skew list is gone |

**These "after" numbers are derived, not executed.** This lane may not build, so
no binary containing them exists yet. The `isWhitespace`, `isDigit` and
`digit` rows are *proved* rather than estimated: the predicate the Rust code now
computes was evaluated against the HotSpot sweep over all 65,536 BMP code points
and matched exactly. The `toUpperCase`/`toLowerCase` rows are counted directly
off the sweep. **They must be re-measured against a real build before this
record is closed.**

---

## 4. The JIT bug that motivated these natives is dead

Four of these natives exist because of `S111r15`: `Character.toLowerCase`'s
bytecode delegates to `(I)I` -> `CharacterData.of` + invokevirtual, and the
compiled code "returned 0 for most inputs after warm-up", corrupting Spring's
`BeanPropertyName.toDashedForm`. Registering natives defeated the JIT path.

That root cause has since been fixed properly, in `jit/src/x64/driver.rs` — a
method whose only inter-method calls are `direct_call`s took a fast entry that
skipped `set_jit_thread`, so the callee's dispatch helper saw a null thread and
returned 0. The comment there names `Character.getType(char)` as the case.

Re-measured today, so the claim is not archaeological. `CharWarm.java` drives
`getType`, `toTitleCase`, `isSpaceChar`, `isAlphabetic` and
`getDirectionality` — all **unshadowed**, all taking the exact
`CharacterData.of(cp).f(cp)` chain — through **6,000,000 invocations each** on
20 probe characters, capturing answers cold and re-verifying them hot:

```
iters=300000 mismatches=0
diff hotspot cratonvm -> IDENTICAL
```

Zero cold->hot mismatches, and byte-identical to HotSpot. `toTitleCase` is the
decisive row: it is `CharacterData.of(cp).toTitleCase(cp)`, the same shape as
`toUpperCase`, and it gets **every** case the natives get wrong right —
`U+00DF` -> `U+00DF`, `U+FB00` -> `U+FB00`, `U+1FB3` -> `U+1FBC`,
`U+D800` -> `U+D800`.

Native calls in this VM are **not** free: this registration block runs after
`registry.set_leaf(false)`, so every one of these pays the full
`safe_native_call` funnel (~120 ns measured elsewhere in-tree). Deleting them
is expected to be a throughput **win**, not a cost.

---

## 5. NOMINATIONS

**N1 — delete the four duplicate closures (`native-builtins/src/lib.rs`).**
Blocking: without it the (b) and (c) fixes are inert. Delete the four
`registry.register("java/lang/Character", …)` calls at **`lib.rs:19526`**
(`toLowerCase(C)C`), **`19541`** (`toUpperCase(C)C`), **`19556`**
(`toLowerCase(I)I`) and **`19572`** (`toUpperCase(I)I`), together with the
`S111r15` comment block above them at `lib.rs:19512-19525`. Each body is a
verbatim copy of the `lang_math.rs` function it shadows, and each carries
`overwrote: intrinsic` in the census. Their stated justification — the
`S111r15` JIT miscompile — is dead (§4). `lang_math.rs` keeps the canonical,
now-fixed registrations.

**N2 — the real fix: stop shadowing, at the real-JDK call site only.**
`Character`'s classification natives should not exist in real-JDK mode. The
call sites are **already mode-split**: `lang_math::register_wrapper_natives`
(which contains the whole `java/lang/Character` block, `lang_math.rs:402-520`
and `1038-1226`) is called from **`lib.rs:14634`**, inside
`register_essential_natives_with_shims` (real-JDK), *and* from
**`lib.rs:23408`**, inside `register_synthetic_overrides` (which `vm_init` runs
only `if config.use_synthetic_jdk`). Split the `java/lang/Character`
classification block out of `register_wrapper_natives` into its own registrar
and call it **only** from `register_synthetic_overrides`. That makes real-JDK
mode exact for `isLetter`, `getNumericValue`, the 27 ypogegrammeni code points,
supplementary digits and the emoji predicates in one move, and leaves the
synthetic class library — which has no `Character` bytecode at all — exactly as
it is today.

Do **not** simply delete the registrations: they are the synthetic library's
only implementation. Keep: `charCount`, `isHighSurrogate`, `isLowSurrogate`,
`isBmpCodePoint`, `isValidCodePoint`, `isISOControl`, `forDigit`, `valueOf`,
`charValue` — closed-form specifications with no Unicode table, all measured
identical to HotSpot.

**N3 — the emoji predicates are hand-rolled range tables and are wrong.**
`lang_math.rs:1161/1182/1187/1200/1213` implement `isEmoji`,
`isEmojiPresentation`, `isEmojiModifier`, `isEmojiModifierBase`,
`isEmojiComponent` as literal `matches!` ranges. Measured on a 10-code-point
sample (`int` values):

| row | HotSpot 25 | CratonVM |
|---|---|---|
| `isEmoji(U+00A9)` | 1 | **0** |
| `isEmojiPresentation(U+231A)` | 1 | **0** |
| `isEmojiPresentation(U+2764)` | 0 | **1** |
| `isEmojiPresentation(U+263A)` | 0 | **1** |
| `isEmojiPresentation(U+1F1E6)` | 1 | **0** |
| `isEmojiComponent(U+1F1E6)` | 1 | **0** |

Six wrong rows out of 60 on a ten-code-point sample; `isExtendedPictographic`
is not registered and is correct. These are JDK 21+ methods with real bytecode
and real `CharacterData` backing. They belong in N2's move, not in a bigger
hand-written table.

**N4 — `ctx.read_string` is lossy for lone surrogates (crate-wide).**
Not a `Character` bug and larger than this lane. `NativeContext::read_string`
returns a Rust `String`, which cannot hold an unpaired surrogate, so every
native that reads a `String` through it sees `U+FFFD` where the Java object
holds `U+D800`. Confirmed: `String.codePointAt`'s body
(`native-builtins/src/lang_string.rs:4991`) is *correct* — it re-encodes to
UTF-16 and returns the code unit — and still answers `65533`, because its input
was already destroyed. `StringBuilder.append(char)`
(`native_sb_append_char`, body at `native-builtins/src/lang_string.rs:1635`,
registered at `:203`/`:209`) has the same shape on the write side.
`new String(char[])` and `String.valueOf(char)`
preserve the surrogate, so the storage is fine and only the native accessors
lose it. Needs a UTF-16-preserving accessor (`read_string_utf16` -> `Vec<u16>`)
and an audit of its callers; until then, treat any native that round-trips a
`String` as surrogate-lossy.

---

## 6. Residuals

* **`isLetter` / `isLetterOrDigit`** — 957 BMP code points, `Nl` and
  `Other_Alphabetic`. No Rust std route. Closed by N2.
* **`getNumericValue`** — 372 BMP code points: `Nl`/`No` values and the `-2`
  sentinel. Closed by N2.
* **`toUpperCase`** — was 27 BMP code points, the `U+1F80..U+1FAF` / `U+1FB3` /
  `U+1FC3` / `U+1FF3` ypogegrammeni family, where the full mapping is multi-char
  but the simple mapping is a *different* single char. **Closed by E7-1**, which
  replaced the derivation with `JAVA_TO_UPPER_RUNS` / `JAVA_TO_LOWER_RUNS`
  enumerated from JDK 25 over all 1,114,112 code points. Not by N2 — the natives
  are still registered.
* **Supplementary digits** — split into two halves that were closed separately.
  `isDigit(int)` / `isLetterOrDigit(int)` are **closed**: W7-95(C1) added
  `JAVA_SUPPLEMENTARY_DIGIT_RUNS` (39 runs, 390 code points) beside the BMP
  table, because the code-POINT overloads really do answer for them.
  `digit(int,int)` is **not a residual at all** — it is unregistered, so real
  `CharacterData` bytecode answers it and answers it correctly; see
  `E17-1-character-digit-int-and-the-fifteen-unregistered.md`, which measures
  what a naive `(II)I` registration against `native_character_digit` would cost
  (**12,246** wrong answers over 390 code points × 35 radices) and records why
  it must not be added. `JAVA_DIGIT_RUNS` stays BMP-only *by design* (its doc
  comment explains why: `Integer.parseInt` walks with `charAt`, so a
  supplementary digit arrives as a surrogate pair and matches nothing —
  extending that table would make `parseInt` more permissive than Java).
* **`isUpperCase` / `isLowerCase`** — was 3 code points each
  (`U+0295`, `U+A7CE`, `U+A7CF`, `U+A7D2`, `U+A7D4`, `U+A7F1`): Unicode
  **version** skew between Rust's tables and JDK 25's, not a rule error.
  **Closed by E7-1**'s `JAVA_UPPERCASE_RUNS` / `JAVA_LOWERCASE_RUNS`, which are
  version-locked to the image by construction. Not by N2.
* **Methods not reached by this lane**: `codePointAt(char[],int)`,
  `codePointCount`, `offsetByCodePoints`, `toChars`, `reverseBytes`,
  `getName`, `codePointOf`, `UnicodeBlock`/`UnicodeScript` lookups, and the
  `String`-level case operations (`String.toUpperCase(Locale)`), which have
  their own locale rules. `Character.getDirectionality`, `isMirrored`,
  `isDefined`, `isJavaIdentifierStart/Part` and `isUnicodeIdentifierStart` were
  measured and are **correct** — they are unshadowed.
* **No "after" measurement exists on a real binary.** This lane may not build.
  Every "after" number in §3 is either proved against the HotSpot sweep by
  evaluating the new predicate over all 65,536 BMP code points, or counted
  directly off that sweep. Re-run `BmpSweep.java` against a build containing
  these changes plus N1 before closing.

## 7. Reproduction

`BmpSweep.java` (all 65,536 BMP code points, twelve `int` columns per row),
`CharProbe.java` (the 755-row targeted differential), `CharWarm.java` (the
6M-invocation hot re-verification of the unshadowed chain), `CharAfter.java`
(the model of the post-N2 behaviour, built only from unshadowed methods) and
`CharToString.java` (the surrogate string round-trip). Run each under `java`
and under `cratonvm.exe -cp . <Class>`, strip `\r`, and diff.
