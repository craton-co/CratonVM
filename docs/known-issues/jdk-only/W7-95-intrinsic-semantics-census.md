# W7-95 — the `NativeKind::Intrinsic` category has never had its semantics checked

> ## RECONCILED 2026-08-12 (lane C18) — READ THIS BEFORE THE HEADLINE BELOW
>
> Four numbers in this record are stale. They are corrected here rather than
> deleted, because the *original* readings were real measurements and the
> before/after pairing is the evidence.
>
> 1. **No family aborts the VM any more. MEASURED on a binary.** The headline's
>    "six of them VM-fatal" is the **pre-fix** state. Every failure in the
>    second-generation census (`RJdkIntrinsics2`, `W8-C3-1`) is a Java
>    `AssertionError`, not a Rust panic. Read "six VM-fatal" as history
>    throughout this record; §"VM-fatal: `floorDiv` / `floorMod`" and N1 are
>    the *cause*, not a live defect.
> 2. **`floorDiv`/`floorMod` at `MIN_VALUE / -1` is FIXED and VERIFIED on a
>    real binary. MEASURED.** No record should call it open or VM-fatal.
> 3. **`Math.ulp` is NOT a defect and must come off the divergent list.
>    MEASURED**, and by proof rather than sample: the shipped body was run
>    against the JDK's own exponent form over **all 4,294,967,296 `float` bit
>    patterns** and the two are equivalent. The `+Infinity` rows below are the
>    pre-fix state. §"`Math.ulp` is EXECUTED-fixed" carries the sweep; the
>    NaN-payload row in the closure table is a javadoc-permitted difference,
>    not a divergence. *Unresolved detail, stated rather than guessed:* that
>    section reports 16,777,212 NaN patterns differing in payload while the
>    later exhaustive result is quoted as equivalence over all 2^32; the two
>    reconcile only if payload is excluded from "equivalent", which is what
>    the javadoc permits. Do not quote either number without saying which.
> 4. **`Math.pow` is TWO items, not one.** (a) The five special-value rows:
>    FIXED and verified, MEASURED. (b) The **fast path on ordinary inputs**:
>    `a.powi(b as i32)` binary exponentiation, measured at **1.4 ulp at
>    |b| = 2 and 44.3 ulp at |b| = 63 against a 1-ulp contract**. A record
>    that treats "pow" as one closed item is wrong. See §"`Math.pow`: the
>    special values were the smaller half".
> 5. **"645 triples" is a count of registry ROWS.** The correct figures are
>    **645 registry rows = 614 DISTINCT triples** (31 duplicate
>    registrations) — `W8-C3-1` §"The coverage arithmetic", recomputed from a
>    `--dump-native-registry` schema-4 dump. Coverage arithmetic stated
>    against 645-as-triples is off: 258 / 614 is **42%**, not 40%, and the
>    never-invoked remainder in distinct triples is **356**, not 387.
>
> Nothing else in this record was upgraded. Every value still marked
> **PREDICTED** below stays PREDICTED.

**Status: PARTIALLY CLOSED. 39 divergent triples found in a 258-triple sample.
Six of them were VM-fatal; none is any longer — see the reconciliation banner
above. Both shipping modes are affected; none of this is
`--jdk-only`-specific.**

**Closed and re-measured on a binary:** `floorDiv`/`floorMod` (the six formerly
VM-fatal triples), `Math.pow`'s **special values only**, `Math.ulp`/
`StrictMath.ulp` ×4 (and `ulp` is now shown to be no defect at all), and the
nine `parse*` triples. **Closed but not yet re-measured on a binary:** the
sixteen `Character` triples and the `Math.pow` **fast-path accuracy** defect
this lane found that the original census did not sample. **Still open:** the
seven `java.lang.String` code-point triples. See [Closure](#closure-lane-c1).

`NativeKind::Intrinsic` means, per `native-api/src/registry.rs`'s own doc
comment, "a correct fast-path for a hot method". Two claims: *fast*, and
*correct*. The tree measures the first and asserts the second.

W7-94 found `Math.min(1.0, NaN) == 1.0` where Java requires `NaN`. This record
is the follow-up question — **is `Math.min` special, or is the category
untested?** — and the answer is the second one.

## The measurement

Every row below is `/c/craton/jdkonly-wave2-target/release/cratonvm.exe
--jdk-only` against Microsoft OpenJDK 25.0.3+9, same host, same class file, one
process per block. Floating point is compared as
`Double.doubleToRawLongBits` / `Float.floatToRawIntBits`, never `==`: `-0.0 ==
0.0` is `true` and `NaN != NaN`, so an equality-shaped check passes against
exactly the defects being hunted.

* **645** registry ROWS are registered `Intrinsic` in this configuration
  (`--dump-native-registry`, a hello-world run: the registry is populated at VM
  init, so the number does not depend on what the program touches). **Corrected
  2026-08-12 (C18): 645 is rows, not triples. 31 of them are duplicate
  registrations, so the distinct-triple count is 614** — `W8-C3-1` §"The
  coverage arithmetic". Every "of the 645" in this record is "of the 645 rows".
* **258** distinct triples were actually invoked by the probe — the census's own
  per-row `invocations` column, not a claim. **42% coverage of the 614**
  (the "40%" this record originally printed divided by the row count).
* **776** differential rows over those 258 triples.
* **39 distinct triples diverge**, across nine classes. **Six of them are
  VM-fatal** — a Rust panic, which is not a Java throwable and cannot be
  caught.

The 39, so the number is checkable rather than asserted:

```
java/lang/Math          floorDiv (II)I  floorDiv (JJ)J  floorMod (II)I  floorMod (JJ)J
                        pow (DD)D  ulp (D)D  ulp (F)F
java/lang/StrictMath    floorDiv (II)I  floorMod (II)I  ulp (D)D  ulp (F)F
java/lang/Character     isWhitespace (C)Z  isWhitespace (I)Z  isDigit (C)Z  isDigit (I)Z
                        isLetter (C)Z  isLetter (I)Z  isLetterOrDigit (C)Z
                        isLetterOrDigit (I)Z  digit (CI)I  getNumericValue (C)I
                        toUpperCase (C)C  toLowerCase (C)C  toUpperCase (I)I
                        toLowerCase (I)I  isEmojiPresentation (I)Z  isEmojiComponent (I)Z
java/lang/String        codePoints ()Ljava/util/stream/IntStream;  codePointAt (I)I
                        codePointCount (II)I  offsetByCodePoints (II)I
                        repeat (I)Ljava/lang/String;  isBlank ()Z
                        regionMatches (ILjava/lang/String;II)Z
java/lang/Integer       parseInt (Ljava/lang/String;)I
java/lang/Long          parseLong (Ljava/lang/String;)J
java/lang/Short         parseShort (Ljava/lang/String;)S
java/lang/Double        parseDouble (Ljava/lang/String;)D
java/lang/Float         parseFloat (Ljava/lang/String;)F
```

> **The four `ulp` entries in that list are no longer a divergence
> (C18, 2026-08-12, MEASURED).** `Math.ulp (D)D`, `Math.ulp (F)F`,
> `StrictMath.ulp (D)D` and `StrictMath.ulp (F)F` were the pre-fix
> `+Infinity`-at-the-top rows; the shipped body has since been proved
> equivalent to the JDK's own exponent form over all 4,294,967,296 `float`
> bit patterns. The list above is preserved as the census's original finding —
> **it is history, not a live defect list.** Live status per family is the
> Closure table below plus the reconciliation banner at the top.

Counted as untested rather than as defects, but note the asymmetry:
`StrictMath.floorDiv(JJ)J` and `StrictMath.floorMod(JJ)J` are registered onto
the *same* `native_math_floor_div_long` / `native_math_floor_mod_long` bodies
that abort the VM for `Math`, so they are near-certainly two more VM-fatal
triples that this probe simply did not call. **(C18, 2026-08-12: moot — the
shared bodies were fixed and no family aborts the VM any more. The asymmetry
argument stands as method; the prediction of two more aborts does not.)** The nineteen other untested
`StrictMath` integral forms (`addExact`, `multiplyExact`, `negateExact`,
`min`/`max` on `II`/`JJ`, …) share bodies with `Math` twins that measured
**correct**, so those are the reverse case: probably green, unverified.

## Closure (lane C1)

Every "after" below is labelled either **EXECUTED** — measured by running the
`cratonvm.exe` binary — or **PREDICTED**, which means the fix is written and
argued but the binary carrying it has not been run. The distinction is not
decoration: this record exists because a comment that *asserted* a semantic was
believed for months.

| area | before | after | how |
|---|---|---|---|
| `floorDiv`/`floorMod` ×6 | VM ABORT | correct | **EXECUTED**; re-verified on a real binary 2026-08-12 — **no longer VM-fatal** |
| `Math.pow` special values ×5 | `1.0` | NaN | **EXECUTED** — this row is *only* the special values |
| `Math.ulp`/`StrictMath.ulp` ×4 | `+Infinity` | `0x7ca0…`/`0x73800000` | **EXECUTED**; and since proved equivalent over all 2^32 `float` patterns — **not a defect** |
| `Math.pow` fast path on ORDINARY inputs (NEW) | 1.4 ulp at \|b\|=2, 44.3 ulp at \|b\|=63, against a 1-ulp contract | ≤1 ulp | **PREDICTED** — separate item from the special values above |
| `Math.ulp` NaN payload (NEW) | canonical NaN | payload kept | **PREDICTED**; javadoc-permitted either way, not a divergence |
| `Character` over the BMP | 3837 → 1294 code points | 0 | 3837→1294 **EXECUTED**, 1294→0 **PREDICTED** |
| `Character.isDigit` supplementary | 390 wrong | 0 | **PREDICTED** |
| emoji predicates ×5 | 2746 wrong | 0 | **PREDICTED** |
| the nine `parse*` triples | Rust's grammar | Java's | **EXECUTED** (see below) |

### The `Character` number is the one to read carefully

A full sweep of all 65,536 BMP code points, eleven columns each, run on the
binary against HotSpot 25 — **both arms executed, neither predicted**:

```
BEFORE (the Bridge duplicates in lib.rs winning):   3837 code points diverge
AFTER  (those four deleted, intrinsics winning):    1294 code points diverge
```

So deleting the duplicates (commit `4866ad9b4`) was worth 2543 code points, and
it is now *measured* rather than argued. The residual 1294 attributes cleanly,
which is what made it fixable:

| column | code points | cause |
|---|---|---|
| `isLetter` | 957 | Rust's `Alphabetic` (`L* u Nl u Other_Alphabetic`) vs Java's `L*` |
| `isLetterOrDigit` | 957 | the same 957, inherited |
| `getNumericValue` | 372 | the digit table reused for a *numeric value* |
| `toUpperCase` | 30 | simple-vs-full case mapping + a version skew |
| `isUpperCase` | 3 | toolchain Unicode version skew |
| `isLowerCase` | 3 | toolchain Unicode version skew |
| `toLowerCase` | 3 | toolchain Unicode version skew |

(1294 distinct code points, not 2325 — `isLetter` and `isLetterOrDigit` fail on
the same set.)

**957 ≠ 949 is a finding, not rounding.** JDK 25's own `isAlphabetic \ isLetter`
over the BMP is **949**. The census measured **957** against the binary. The
eight-code-point gap is Rust's Unicode tables disagreeing with JDK 25's, and it
is why the obvious fix — `is_alphabetic() && !DELTA` — was rejected: it would
have left a residual that nothing on the Rust side can enumerate. Six of those
eight surface directly in the table above (`U+A7CE`, `U+A7CF`, `U+A7D2`,
`U+A7D4`, `U+A7F1` are `UNASSIGNED` on JDK 25 and assigned in the toolchain;
`U+0295` is the reverse). The fix is therefore a table generated **from JDK 25
itself**, the same provenance as the existing `JAVA_DIGIT_RUNS`.

### Why these have to be fixed as a SET, not one method at a time

`RJdkIntrinsics2`'s `charcls` family names the trap in one line: **`U+2160`
ROMAN NUMERAL ONE**. Measured on JDK 25:

| classifier | `U+2160` | why |
|---|---|---|
| `Character.isLetter` | **false** | its category is `Nl`, and `isLetter` is `L*` only |
| `Character.isUpperCase` | **true** | it carries `Other_Uppercase` |
| `Character.isLetterOrDigit` | **false** | not `L*`, not `Nd` |
| `Character.isAlphabetic` | **true** | `L* u Nl u Other_Alphabetic` |
| `char::is_alphabetic` (Rust) | true | it *is* Unicode `Alphabetic` |

Four classifiers that all read like "is this a letter", four different answers,
one code point. Any body that answers *any* of them from a Rust `char` method
gets at least one row wrong, and a fix that repairs `isLetter` by broadening
`isUpperCase` trades a red row for a red row. That is why the fix here is one
generated table per Java predicate rather than one clever derivation shared
between them.

The long tail behind that row, by general category — `isAlphabetic && !isLetter`
over every code point on JDK 25:

| category | trapped | of the category |
|---|---|---|
| `Mn` (non-spacing mark) | 927 | 2020 |
| `Mc` (spacing mark) | 438 | 468 |
| `Nl` (letter number) | 236 | **236 — the whole category** |
| `So` (other symbol) | 130 | 7376 |
| **total** | **1731** | |

`Me` and `Cf` are *not* in the trap (`U+20DD`, `U+200D` are `isAlphabetic=false`
on both sides), which is worth stating because they are the categories one would
expect to be there by analogy. `No` is not either — `U+00B2` SUPERSCRIPT TWO is
`isAlphabetic=false` — but it *was* a defect through a different door:
`isLetterOrDigit` used `char::is_alphanumeric`, which is `Alphabetic u N*` and so
took in `No` on its own account. That door is closed by composing
`isLetter || isDigit` the way the JDK does.

**How the tables were verified without a build.** The run tables were extracted
back out of `native-builtins/src/lang_math.rs` as text, the exact composition
each Rust body performs was replayed in Java, and the result was compared to
HotSpot 25 for **every code point `0..=0x10FFFF`**:

```
isLetter 0   isDigit(int) 0   isLetterOrDigit(int) 0   isEmoji 0
isEmojiPresentation 0   isEmojiModifier 0   isEmojiModifierBase 0
isEmojiComponent 0      getNumericValue(char) 0   case mapping + predicates 0
TOTAL TABLE MISMATCHES AGAINST HOTSPOT 25: 0
```

That proves the *bytes in the repo* are the JDK's answers. It does not prove
they compile or that the registrations reach them, which is why the rows above
still read PREDICTED.

### A supplementary-plane defect no BMP sweep can see

`RJdkIntrinsics` fails on the binary with:

```
AssertionError: Character.isDigit(U+1D7CE MATHEMATICAL BOLD ZERO) must be true
```

`U+1D7CE` is above the BMP, so the `(C)Z` char-taking form cannot reach it —
only `(I)Z` can. A sweep that walks `0..0xFFFF` reports green on this forever,
and the 65,536-point sweep above did exactly that. **390 supplementary decimal
digits** were wrong, and the two digit tables must stay separate: `parseInt`
walks UTF-16 code *units*, so a supplementary digit correctly matches nothing
there (measured on JDK 25:
`Integer.parseInt(new String(Character.toChars(0x104A0)))` throws even though
`Character.digit(0x104A0, 10) == 0`). Merging them would make the parser *more*
permissive than the JDK.

### `Math.pow`: the special values were the smaller half

The census sampled special values, found the five C99-vs-JLS rows, and those are
now fixed and EXECUTED-correct. Sampling special values is exactly why it missed
the larger defect: **the fast path was wrong on ordinary numbers.**

`Math.pow` took `a.powi(b as i32)` for every integral `|b| < 64`. That is binary
exponentiation — up to eleven chained multiplications — and `Math.pow` promises
"within 1 ulp of the exact result". Measured against the exact power
(`BigDecimal.pow` at 120 digits), 20,000 random bases per exponent:

| \|b\| | worst error (ulp) | cases over the 1-ulp bound |
|---|---|---|
| 2 | 0.500 | 0 / 20000 |
| −2 | 1.439 | 412 / 20000 |
| 3 | 1.228 | 150 / 20000 |
| −3 | 2.242 | 1304 / 20000 |
| 4 | 1.852 | 2787 / 20000 |
| 8 | 4.943 | 10107 / 20000 |
| 16 | 10.420 | 14939 / 20000 |
| 32 | 20.814 | 17360 / 20000 |
| 63 | 44.321 | 18682 / 20000 |

HotSpot 25 is within **0.503 ulp** on every one of those same inputs, so each is
also a plain differential divergence. The old comment called the window
"HotSpot-style"; HotSpot's C2 specialises `pow(x, 2)` and `pow(x, 0.5)`, not a
63-wide range.

**Should `Math.pow` delegate to fdlibm instead?** No, and the file already
contains the argument at its own head: `Math.f` promises 1 ulp, `StrictMath.f`
promises the fdlibm *bits*, and the doc block says in terms "do NOT simplify
this by pointing both classes at the fdlibm bodies … libm already satisfies
`Math`'s contract." Measured here, that block is right: `Math.pow` and
`StrictMath.pow` differ bitwise on 9.73% of random finite inputs, and both are
legal. The defect was never that `powf` was used — it was that `powi` was used
*instead of* `powf`. So the fix removes the bypass rather than replacing the
backend, keeping exactly the one case that is provably exact: `b == 2.0` is
`a * a`, a single correctly-rounded multiply (0.500 ulp worst, 0 violations),
and it needs no guard on `a` because `±inf * ±inf == +inf` and
`-0.0 * -0.0 == +0.0` are already the JLS's answers.

### `Math.ulp` is EXECUTED-fixed, and the fixture is not vacuous

`RJdkIntrinsics` reports `CK ulp=9` passing. That is only evidence if the nine
checks include the defect, so it was checked rather than assumed:
`ulpAtTheTop()` asserts `Math.ulp(Double.MAX_VALUE) == 2^971` and
`StrictMath.ulp(Float.MAX_VALUE) == 2^103` by raw bits. The pass is real.

Separately, the algorithm was proved rather than sampled: both the shipped
bit-increment form and the JDK's own exponent form were transliterated to Java
and run over **all 4,294,967,296 `float` bit patterns**. Every non-NaN pattern —
4,278,190,082 of them — agrees with `Math.ulp` exactly, and the two algorithms
are *equivalent*, so rewriting the working body from the exponent would have
been churn. The 16,777,212 that disagreed were all NaN: `Math.ulp` is
`Math.abs(d)`, which keeps a NaN's payload and only clears its sign, where the
body returned canonical NaN. Measured: `Math.ulp(0x7ff0000000000001)` is
`0x7ff0000000000001` on HotSpot. Payloads are not a contract — the javadoc
promises only "is NaN" — but `v.abs()` is the JDK's own expression, is shorter,
and is strictly closer, so there is no reason to write anything else.

### The `parse*` family: the claim VERIFIED, and one record row corrected

Commit `26e69258a` claims eight parsers were moved off Rust's grammar. The claim
holds. The acceptor was transliterated from the Rust back into Java and diffed
against HotSpot's own accept/reject decision:

```
Integer.parseInt named rows                     divergences 0
Double.parseDouble named rows                   divergences 0
float-grammar fuzz, 400,000 generated tokens    divergences 0
integer-grammar fuzz, 400,000 tokens x radix    divergences 0
```

That covers every item this lane was asked to check: leading `+` is accepted,
leading/trailing whitespace and underscores are rejected by the integer grammar,
trailing `d`/`f`/`D`/`F` is accepted by the floating one, and hex literals
(`0x1.8p3`) parse.

**One row in W7-99 is wrong and should not be copied forward.** It lists
`Double.parseDouble(" 1.0")` as throwing. It does not — measured on JDK 25 it
returns `1.0`, because the grammar's `[\x00-\x20]*` really does skip an ASCII
space, which is what `java_trim` implements. The rejection that *does*
distinguish `java_trim` from Rust's `str::trim` is a NON-ASCII space:
`Double.parseDouble("\u00a01.0")` and `("1.0\u00a0")` both throw, while
`(" 1.0")`, `("\u000b1.0")` and `("\u00001.0")` are all accepted -- the
grammar's bound is the byte value `0x20`, not the `White_Space` property. The
Rust is correct either way; only the record's example was wrong.

### VM-fatal: `floorDiv` / `floorMod` at `MIN_VALUE / -1` — **FIXED, MEASURED ON A REAL BINARY 2026-08-12. This section is the pre-fix measurement.**

```
                                       HotSpot                CratonVM
Math.floorDiv(Integer.MIN_VALUE, -1)   -2147483648            VM ABORT
Math.floorDiv(Long.MIN_VALUE, -1L)     -9223372036854775808   VM ABORT
Math.floorMod(Integer.MIN_VALUE, -1)   0                      VM ABORT
Math.floorMod(Long.MIN_VALUE, -1L)     0                      VM ABORT
StrictMath.floorDiv(MIN_VALUE, -1)     -2147483648            VM ABORT
StrictMath.floorMod(MIN_VALUE, -1)     0                      VM ABORT
```

```
thread 'main-vm' panicked at native-builtins\src\lang_math.rs:2335:13:
attempt to divide with overflow
[cratonvm] main-vm run() returned Err: Error in thread "main"
    internal error: native method panic: attempt to divide with overflow
```

The bodies are `let d = a / b; let r = a % b;` on `i32`/`i64`. **Rust checks
division overflow unconditionally — in release as well as debug**, so this is
not a debug-only hazard and not a `--jdk-only` hazard: it reproduces in the
default compatibility mode too, from ordinary application bytecode, with no
flags. Java's contract is the `idiv` opcode's (JVMS 6.5): the quotient
overflows and *wraps*.

The `b == 0` guard above these lines is the tell. Someone thought about the
one divisor that makes `/` illegal in Java, and did not think about the one
that makes it illegal in Rust.

### `Math.pow` uses C99's special-value table, not the JLS's

```
                          HotSpot   CratonVM
Math.pow(1.0, NaN)        NaN       1.0
Math.pow(1.0, +Infinity)  NaN       1.0
Math.pow(1.0, -Infinity)  NaN       1.0
Math.pow(-1.0, +Infinity) NaN       1.0
Math.pow(-1.0, -Infinity) NaN       1.0
Math.pow(NaN, 0.0)        1.0       1.0       <- correct (exponent-zero wins)
Math.pow(2.0, 10.0)       1024.0    1024.0    <- correct
StrictMath.pow(1.0, NaN)  NaN       NaN       <- correct, different body
```

The JLS says `pow(x, NaN)` is `NaN` and `pow(x, ±Infinity)` is `NaN` when
`|x| == 1`. C99's `pow` says `1.0` for all five, and Rust's `f64::powf` *is*
that `pow`.

This one is a `[premise=guard]` instance in its purest form. The body's own
comment says:

> `- Gated on a.is_finite() && b.is_finite() so NaN/±infinity edge cases fall
> through to powf, preserving Java/JLS special-value semantics (e.g. pow(NaN,
> 0) == 1, pow(±0, neg) == ±inf, pow(1, ±inf) == NaN per JLS, etc.).`

The comment names the exact case that is wrong, states the exact rule Java
requires, and asserts that falling through to `powf` preserves it. It does
not. `StrictMath.pow` routes through the ported fdlibm in
`types/src/fdlibm.rs` and gets all five right — two bodies for one function,
in one file, and only one of them was ever measured.

### `Math.ulp` overflows at the top of the range

```
                                HotSpot                 CratonVM
Math.ulp(Double.MAX_VALUE)      0x7ca0000000000000      +Infinity
Math.ulp(Float.MAX_VALUE)       0x73800000              +Infinity
StrictMath.ulp(Double.MAX_VALUE) 0x7ca0000000000000     +Infinity
StrictMath.ulp(Float.MAX_VALUE)  0x73800000             +Infinity
```

`next - abs` where `next = from_bits(abs.to_bits() + 1)`: at `MAX_VALUE`,
`bits + 1` *is* the infinity pattern. Java defines `ulp` from the exponent, and
it is finite for every finite input. Everywhere else in the domain the
bit-increment trick is exact, which is why this survived.

### `java.lang.Character` is Rust's Unicode, not Java's

Every classifier delegates to a Rust `char` method. These implement the
**Unicode** definitions; Java's are deliberately different, and the two
disagree on whole blocks, not on corner cases.

```
                                             HotSpot   CratonVM   Rust method
Character.isWhitespace(U+00A0 NBSP)          false     true       is_whitespace
Character.isWhitespace(U+0085 NEL)           false     true       is_whitespace
Character.isWhitespace(U+2007 FIGURE SPACE)  false     true       is_whitespace
Character.isWhitespace(U+202F NNBSP)         false     true       is_whitespace
Character.isWhitespace(U+001C FILE SEP)      true      false      is_whitespace
Character.isDigit(U+0660 ARABIC-INDIC ZERO)  true      false      is_ascii_digit
Character.isDigit(U+0966 DEVANAGARI ZERO)    true      false      is_ascii_digit
Character.isDigit(U+FF10 FULLWIDTH ZERO)     true      false      is_ascii_digit
Character.isDigit(U+1D7CE MATH BOLD ZERO)    true      false      is_ascii_digit
Character.digit(U+FF10, 10)                  0         -1         to_digit
Character.digit(U+0660, 16)                  0         -1         to_digit
Character.getNumericValue(U+FF21)            10        -1         (same table)
Character.getNumericValue(U+2160)            1         -1         (same table)
Character.getNumericValue(U+00BC ONE QTR)    -2        -1         (same table)
Character.isLetter(U+2160 ROMAN NUM ONE)     false     true       is_alphabetic
Character.isLetter(U+3007 IDEO NUM ZERO)     false     true       is_alphabetic
Character.isLetterOrDigit(U+2160)            false     true       is_alphanumeric
Character.toUpperCase(U+00DF sharp s)        U+00DF    'S'        to_uppercase
Character.toUpperCase(U+FB00 ff ligature)    U+FB00    'F'        to_uppercase
Character.toUpperCase(U+1F88 titlecase)      U+1F88    U+1F08     to_uppercase
Character.toLowerCase(U+D800 high surrogate) U+D800    U+0000     from_u32→None
Character.toUpperCase(U+D800)                U+D800    U+0000     from_u32→None
Character.toLowerCase(U+DC00 low surrogate)  U+DC00    U+0000     from_u32→None
Character.isEmojiPresentation(U+2764)        false     true       (own table)
Character.isEmojiPresentation(U+261D)        false     true       (own table)
Character.isEmojiPresentation(U+1F1E6)       true      false      (own table)
Character.isEmojiComponent(U+1F1E6)          true      false      (own table)
```

Three separate mechanisms, worth separating because they need different fixes:

1. **Different table.** `is_whitespace` is Unicode's `White_Space`;
   `Character.isWhitespace` deliberately *excludes* every non-breaking space
   and *includes* `U+001C`–`U+001F`. `is_alphabetic` is `Alphabetic`, which
   takes in `Nl` and `Other_Alphabetic`; `Character.isLetter` is exactly the
   five `L*` categories. `is_ascii_digit`/`to_digit` are ASCII-only where Java
   accepts every `Nd`.
2. **Different mapping arity.** `char::to_uppercase()` yields the *full* case
   mapping and the body takes `.next()` — the first character of a multi-char
   expansion. `Character.toUpperCase(char)` is the 1:1 mapping and returns the
   input unchanged when the full mapping does not fit. So `ß` became `S`.
3. **Surrogates are not scalar values.** `char::from_u32` answers `None` for
   `U+D800`–`U+DFFF`, and the `.unwrap_or('\0')` turns that into NUL. Java maps
   every unmapped char to *itself*. Half of a surrogate pair passed through
   `toLowerCase` becomes `U+0000`, which is a silent data corruption in exactly
   the code that handles text char by char.

`String.isBlank()` inherits (1) — `" ".isBlank()` is `true` on CratonVM
and `false` on HotSpot.

### `String`'s code-point family loses astral characters

```
                                       HotSpot              CratonVM
"a😀b".codePoints()          [97, 128512, 98]     [97, 55357, 56832, 98]
"x\uD800y".codePoints()                [120, 55296, 121]    [120, 65533, 121]
"\uDC00a".codePoints()                 [56320, 97]          [65533, 97]
"x\uD800y".codePointAt(1)              55296                65533
"a\uD800".codePointAt(1)               55296                65533
"a<U+1F600>b".codePointCount(3, 1)     IndexOutOfBounds     no throw
"a<U+1F600>b".offsetByCodePoints(0, 9) IndexOutOfBounds     no throw
"ab".repeat(-1)                        IllegalArgument      no throw
"ABC".regionMatches(0,"abc",0,-1)      true                 false
```

Two bugs in one family. `codePoints()` never pairs the surrogates, so a
`String` containing one emoji yields four code points instead of three — and
`codePointAt` on a *lone* surrogate answers `U+FFFD`, which means the value is
going through a UTF-8 / Unicode-scalar pipe somewhere and being lossily
replaced. `65533` in a code-point answer is a diagnosis, not a coincidence.
The missing bounds checks are the separate half: three methods specified to
throw that return silently.

`regionMatches` with a negative length is not a typo in the table above: the
JDK's bounds test passes (`toffset > length() - len` is false when `len` is
negative) and `while (len-- > 0)` runs zero times, so the answer is `true`.
Code that passes a computed length depends on it.

### The `parse*` family is Rust's grammar, not Java's

```
                                  HotSpot              CratonVM
Integer.parseInt("  1")           NumberFormatEx       1
Integer.parseInt("1 ")            NumberFormatEx       1
Integer.parseInt("1\n")           NumberFormatEx       1
Long.parseLong("1 ")              NumberFormatEx       1
Short.parseShort("1 ")            NumberFormatEx       1
Integer.parseInt("१२")  12                   NumberFormatEx
Long.parseLong("१२")    12                   NumberFormatEx
Double.parseDouble("nan")         NumberFormatEx       NaN
Double.parseDouble("inf")         NumberFormatEx       +Infinity
Double.parseDouble("infinity")    NumberFormatEx       +Infinity
Float.parseFloat("nan")           NumberFormatEx       NaN
Double.parseDouble("0x1p3")       8.0                  NumberFormatEx
Float.parseFloat("0x1p3")         8.0f                 NumberFormatEx
Double.parseDouble(null)          NullPointerEx        NumberFormatEx
Float.parseFloat(null)            NullPointerEx        NumberFormatEx
Double.parseDouble("  1.5  ")     1.5                  1.5        <- correct
Integer.parseInt(null)            NumberFormatEx       NumberFormatEx  <- correct
```

`Integer.parseInt` is `text.trim().parse::<i32>()`. The `.trim()` is wrong —
`Integer.parseInt` does **not** accept surrounding whitespace, while
`Double.parseDouble` explicitly does. Two contracts on the same-shaped
argument, and the integer side borrowed the floating side's. The `parse::<i32>`
is the other half: it accepts only ASCII, where `Integer.parseInt` accepts any
`Character.digit`.

`parse_double_string` has explicit `"NaN"` / `"Infinity"` arms — correct — and
then falls through to `numeric.parse::<f64>()`, whose grammar is *also*
case-insensitively lenient about `nan`/`inf`/`infinity` and knows nothing about
Java's hex significands. The explicit arms make the function look like it
enumerated the tokens. It enumerated the ones Rust would have rejected.

## Two reasons the instrument never saw any of this

### 1. `--jdk-only-report` is blind to the whole category, by construction

Witness, measured:

```
$ cratonvm --jdk-only --jdk-only-report rep.json --dump-native-registry cen.json MathWitness
# MathWitness calls Math.min/max/pow/ulp/floorDiv 1000 times each

census:  java/lang/Math intrinsic invocations = 5000
report:  rows mentioning "java/lang/Math"     = 0
report:  counts.intrinsic_invocations         = 5000
```

The registrars open with an ambient `set_category(NativeKind::Intrinsic)`;
`Intrinsic` is exempt from shadow retirement and is not the census's
`native-shadows-bytecode` kind. So the report knows the number 5000 and cannot
name a single one of the calls. The project's best instrument reports the
*volume* of the category and nothing about its *content*.

### 2. An `Intrinsic` registration silently overwritten by a `Bridge` duplicate

```
$ cratonvm --jdk-only --dump-native-registry tiny.json Tiny   # calls toLowerCase once
toLowerCase (C)C kind=intrinsic invocations=0 registered_by=native-builtins/src/lang_math.rs:480
toLowerCase (C)C kind=bridge    invocations=1 registered_by=native-builtins/src/lib.rs:19526
```

`native-builtins/src/lib.rs:19526` re-registers all four `Character` case
forms as inline closures whose bodies are **verbatim copies** of the
`lang_math.rs` intrinsics, under the `Bridge` category, and wins. Two
consequences:

* A reader counting `Intrinsic` triples counts four rows that never execute.
* Both copies carry all three Character bugs above, and a fix applied to
  `lang_math.rs` alone would change nothing observable — the classic
  `[dup-fix]` shape, except here the duplicate is in the same crate.

The `Bridge` copy at least earns a `native-shadows-bytecode` violation row, so
the report *can* see `toLowerCase`. It cannot see `isWhitespace` or `digit`,
which are the same class of defect one category away.

## Does "mostly legitimate acceleration — not roadmap work" survive?

**Not as written.** Evidence, stated at the strength it supports:

* Of **258** `Intrinsic` triples actually exercised, **39 diverge** — 15%. That
  is not "mostly illegitimate", and the roadmap's *direction* is defensible: a
  large majority of the sample agreed with HotSpot exactly — 677 of 755
  measured rows, including **259** `Math`/`StrictMath` rows over NaN, ±0.0,
  ±Infinity and overflow
  boundaries, the whole `Objects` family, `Integer`/`Long` bit-twiddling
  (`bitCount`, `numberOfLeadingZeros(0)`, `reverse`, `reverseBytes`,
  `nlz`/`ntz` at zero), the `Exact` overflow family, `Math.round` at halfway
  points and at `±Infinity`/NaN, `Double.toString` across its formatting
  breakpoints, `Double.compare`/`hashCode` over NaN and `-0.0`, and
  `String.split`'s trailing-empty rules.
* What does not survive is the *conclusion drawn from it* — "not roadmap
  work". A category with a 15% defect rate in its own measured sample,
  including six ways to abort the VM from one-line application code, is
  roadmap work whatever the majority does.
* The failures are not random. They cluster on exactly three seams: **Java's
  Unicode tables vs Rust's**, **Java's numeric grammars vs Rust's `str::parse`**,
  and **Java's defined overflow vs Rust's checked arithmetic**. Any intrinsic
  that reaches for a Rust standard-library method with a plausibly-matching
  name is a suspect; any intrinsic that is pure IEEE-754 arithmetic in the
  interior of its domain has, in this sample, been right.

One row deliberately *not* counted as a defect: `Math.cosh(1.0)` is one ULP
from HotSpot (`0x3ff8b07551d9f550` vs `...51`). `Math`'s contract allows 2.5
ULP for `cosh`, so libm satisfies it; `StrictMath.cosh(1.0)` matches
bit-for-bit, which is the row that would have been a defect and is not.

**A prior residual that did not reproduce.** `DoubleStream.min()/max()` over
`{1.0, NaN, 2.0}` was carried as a known surviving instance of W7-94's defect
(`OptionalDouble[1.0]` where HotSpot answers `OptionalDouble[NaN]`). Measured
here it agrees — as do `DoubleStream.sum/average`, `DoubleSummaryStatistics`
over NaN and over the empty stream, and `DoubleStream.reduce(Double::min)`. The
`min`/`max` natives in `native-collections/src/lib.rs:28160`/`:28166` are
registered on the **interface** `java/util/stream/DoubleStream` and are
`Bridge`, not `Intrinsic`; the census shows zero invocations for them on this
path, so `DoubleStream.of(...).min()` ran as real bytecode over `Double.min`,
which W7-94 fixed. The natives themselves are unexercised, not proven correct —
whoever reaches them through interface dispatch should re-measure.

## The regression vector

`regression-suite/src/RJdkIntrinsics.java` — 132 checks, green on HotSpot
25.0.3+9, red on the shipping binary at the first check. It pins the divergences
above by raw bits, with negative controls in every block, and it validates its
own fixtures (the code points in `OPAQUE_C` are asserted, because "this
classifier says NO about this character" passes vacuously if the source
encoding ever folds `U+00A0` to a space).

`floorDivModOverflowMustNotAbortTheVm()` runs **last**, deliberately: on a VM
that still panics there, nothing after the first line of that block reports.

It is not registered in `regression-suite/run.sh` — see NOMINATIONS.

## Residuals — what is still untested

**387 of the 645 registered `Intrinsic` ROWS were never invoked** — **356 of the
614 distinct triples**, corrected 2026-08-12 (C18); see the reconciliation
banner — and the untested set is *not* a random remainder. Largest families,
with the reason each is worth a lane:

| n | class | why it matters |
|---|---|---|
| 34 | `java/security/SecureRandom` | untested intrinsics over a security primitive |
| 22 | `java/util/Random` | seeded streams must be bit-reproducible; `[nextGaus]` is a prior finding in exactly this family |
| 19 | `java/math/BigDecimal` | rounding modes, scale, `equals` vs `compareTo` |
| 18 | `java/math/BigInteger` | sign/magnitude edges, `MIN_VALUE` round trips |
| 19 | `java/util/logging/LogRecord` | |
| 16 | `java/util/HexFormat` | case, delimiters, malformed input |
| 12 | `java/net/InetSocketAddress` | |
| 9 | `java/util/Formatter` + 6 `java/lang/String.format` | locale, `%e`/`%g` rounding, `-0.0` |
| 9 | `java/util/UUID` | |
| 9 | `java/util/concurrent/atomic/AtomicReferenceArray` | memory semantics, not values |
| 6 | `java/util/Base64` (+5 `Encoder`/`Decoder`) | padding, MIME line breaks, malformed input |
| 10 | `java/nio/ByteBufferAsCharBuffer*` | endianness |
| 19 | `StrictMath` integral/`Exact` forms | same bodies as the tested `Math` twins — likely green, unverified |
| 5 | `java/lang/StringLatin1` | `compareTo([B[BII)I`, `inflate`, `toLowerCase(String,[B,Locale)` |
| ~40 | H2 / Spring / BouncyCastle app shims | app-specific, lower general risk |

Also untested inside families this record *did* cover:

* `Character.isEmoji*` beyond the four code points sampled — the emoji
  predicates already show two disagreements out of five properties tested, so
  the tables are probably wrong across the board rather than at two points.
* `Character.toTitleCase`, `isSpaceChar`, `isMirrored`, `getType`,
  `isJavaIdentifier*` — not registered in this configuration, but the same
  Rust-`char` reflex would produce the same class of bug if they ever are.
* `String.format`/`formatted` (registered `Intrinsic`, four descriptors, never
  invoked here) — a formatter is a grammar, and this record's finding is that
  grammars are where these natives fail.
* `Math.random()`/`StrictMath.random()` — nondeterministic, excluded from a
  differential by construction; needs a distributional or seeding test instead.
* Every `Intrinsic` under concurrency. This record tested values on one thread.

The probe that produced all of this is not committed; it is a single Java file
that prints one `label=rawbits` line per call and is diffed between the two
VMs, one section per process so a VM-fatal panic truncates only its own block.
Reconstructing it from the triple list is a morning's work, and the triple list
is one `--dump-native-registry` away.

### What lane C1 could not close

Stated at the strength the evidence supports, because a residual that is not
named is a residual nobody looks for.

* **Nothing in the Closure section has been run in a binary yet** except the
  rows marked EXECUTED. The tables are proved correct *as data* — replayed out
  of the source against HotSpot over every code point, 0 mismatches — which is
  not the same as proved correct *as a program*. `C1Verify.java` (276 checks,
  green on HotSpot 25) is the vector for settling that in one run.
* **`isUpperCase`/`isLowerCase` above the BMP are unmeasured.** The six pinned
  code points came from a 65,536-point sweep, and no sweep has covered
  `0x10000..0x10FFFF` for these two. `C1Verify` checks the population counts
  (1978 / 2569 on HotSpot 25) precisely so that, if a supplementary skew exists,
  the next run reports its size instead of hiding it.
* **`toUpperCase`/`toLowerCase` above the BMP are unmeasured**, same reason —
  the `(I)I` overloads are registered and reach code points no BMP sweep sees,
  which is exactly the shape that hid the 390 supplementary digits.
* **The `U+A7CE` family pins a toolchain Unicode version against JDK 25's.** It
  is the only place in this file that encodes "these two tables are at different
  Unicode versions" rather than "Java differs from Unicode". It stays correct
  when the toolchain moves — the pinned answer is JDK 25's either way — but it
  goes stale when the *JDK* moves, along with every other table here.
* **`java.lang.String`'s seven code-point triples are untouched** (N6). The
  `U+FFFD` in a `codePointAt` answer still says a value is crossing a
  UTF-8/scalar-value boundary somewhere upstream.
* **`Character.getType` is unshadowed and was measured correct** on all 65,536
  BMP code points. It is the natural shared source of truth for this whole
  family, and nothing here uses it; the tables are per-predicate instead. That
  is the right call while these are `Intrinsic` (a general-category lookup plus
  a per-predicate mask is slower than a range probe), but it is the reason the
  table count is eleven rather than one.

## NOMINATIONS

Lane B9 does not build. Every item below is an exact edit for someone who does.

### N1 — (was VM-fatal) `floorDiv`/`floorMod` must wrap, not panic — **LANDED AND VERIFIED ON A BINARY 2026-08-12; kept for the reasoning, not as an open nomination**

`native-builtins/src/lang_math.rs`, four bodies. Rust checks division overflow
in *every* profile, so `a / b` and `a % b` on `i32`/`i64` must never be reached
with `b == -1`.

**2334–2337** (`native_math_floor_div_int`), old:

```rust
    // Java floorDiv: rounds toward negative infinity
    let d = a / b;
    let r = a % b;
    let result = if (r != 0) && ((r ^ b) < 0) { d - 1 } else { d };
```

new:

```rust
    // Java floorDiv: rounds toward negative infinity.
    //
    // `wrapping_div`/`wrapping_rem`, not `/` and `%`. Rust checks DIVISION
    // overflow unconditionally — release as well as debug — so a bare
    // `Integer.MIN_VALUE / -1` panics, and a panic inside a native is not a
    // Java throwable: it terminates the VM. Java's rule is the `idiv`
    // opcode's (JVMS 6.5): the quotient overflows and WRAPS to MIN_VALUE,
    // with a zero remainder, so `floorDiv(MIN, -1) == MIN` and
    // `floorMod(MIN, -1) == 0`. The `b == 0` guard above already removed the
    // only input on which the wrapping forms would themselves panic.
    let d = a.wrapping_div(b);
    let r = a.wrapping_rem(b);
    let result = if (r != 0) && ((r ^ b) < 0) { d.wrapping_sub(1) } else { d };
```

**2360–2362** (`native_math_floor_div_long`), old:

```rust
    let d = a / b;
    let r = a % b;
    let result = if (r != 0) && ((r ^ b) < 0) { d - 1 } else { d };
```

new (same two calls; see the comment on the `i32` twin):

```rust
    let d = a.wrapping_div(b);
    let r = a.wrapping_rem(b);
    let result = if (r != 0) && ((r ^ b) < 0) { d.wrapping_sub(1) } else { d };
```

**2385–2387** (`native_math_floor_mod_int`), old:

```rust
    // Java floorMod: a - floorDiv(a,b) * b
    let r = a % b;
    let result = if (r != 0) && ((r ^ b) < 0) { r + b } else { r };
```

new:

```rust
    // Java floorMod: a - floorDiv(a,b) * b. `wrapping_rem` for the same
    // reason `native_math_floor_div_int` uses `wrapping_div`: `%` panics on
    // `MIN % -1`, and Java answers 0.
    let r = a.wrapping_rem(b);
    let result = if (r != 0) && ((r ^ b) < 0) { r.wrapping_add(b) } else { r };
```

**2410–2411** (`native_math_floor_mod_long`), old:

```rust
    let r = a % b;
    let result = if (r != 0) && ((r ^ b) < 0) { r + b } else { r };
```

new:

```rust
    let r = a.wrapping_rem(b);
    let result = if (r != 0) && ((r ^ b) < 0) { r.wrapping_add(b) } else { r };
```

While there: grep the crate for every other bare `/` and `%` on a signed
integer whose operands came from `args`. This is a *shape*, not a site — the
same reflex that put `text.trim().parse::<i32>()` in `parseInt` put an
unguarded `/` here.

### N2 — `Math.pow` must apply the JLS special-value table before `powf`

**DONE and EXECUTED-verified** (`26e69258a`). Kept below for the reasoning. But
see [Closure](#closure-lane-c1): the special values were the smaller half, and
the `powi` fast path this nomination did not question was up to **44 ulp** out
on ordinary numbers. That half is fixed and PREDICTED.

`native-builtins/src/lang_math.rs`, in `native_math_pow` (declared at 1673),
insert immediately before the existing fast-path
`if a.is_finite() && b.is_finite() && b.fract() == 0.0 && b.abs() < 64.0 {`
(currently line 1692):

```rust
    // The JLS special-value table is NOT C99's, and `f64::powf` IS C99's
    // `pow`. Three rules where they disagree, in the JLS's own precedence
    // order (exponent-zero outranks NaN, which is why `pow(NaN, 0.0)` is 1.0):
    //
    //   pow(x, ±0.0)      = 1.0 for every x, NaN included
    //   pow(x, NaN)       = NaN
    //   pow(±1.0, ±inf)   = NaN
    //
    // C99 answers 1.0 for the last two. Measured: `Math.pow(1.0, NaN)` came
    // back 1.0 where HotSpot answers NaN. `StrictMath.pow` is unaffected — it
    // routes through `cratonvm_types::fdlibm::pow`, which has the table.
    if b == 0.0 {
        return Ok(Some(Value::Double(1.0)));
    }
    if b.is_nan() || (b.is_infinite() && a.abs() == 1.0) {
        return Ok(Some(Value::Double(f64::NAN)));
    }
```

And delete the `pow(1, ±inf) == NaN per JLS` clause from the comment above the
fast path, which asserts that `powf` already does this. It is the reason
nobody re-derived it.

### N3 — `Math.ulp` must not overflow at `MAX_VALUE`

**DONE and EXECUTED-verified** (`5196e3aed`; `RJdkIntrinsics`'s `ulpAtTheTop()`
asserts the `2^971` / `2^103` bits, so `CK ulp=9` is not a vacuous pass). The
backward-step form below was additionally proved *equivalent to the JDK's
exponent form* over all 4,294,967,296 `float` bit patterns, so no rewrite is
owed. One refinement landed on top: the NaN arm is now `v.abs()`, the JDK's own
expression, which keeps a NaN payload.

`native-builtins/src/lang_math.rs:2678–2680` (`native_math_ulp_double`), old:

```rust
        let abs = v.abs();
        let next = f64::from_bits(abs.to_bits() + 1);
        next - abs
```

new:

```rust
        // `bits + 1` at MAX_VALUE IS the +Infinity pattern, so the forward
        // difference overflows exactly once, at the top of the finite range.
        // Java's ulp is defined from the exponent and is finite for every
        // finite input; step BACKWARD there instead. Everywhere else the
        // forward difference is exact, which is why this survived.
        let abs = v.abs();
        let next = f64::from_bits(abs.to_bits() + 1);
        if next.is_infinite() {
            abs - f64::from_bits(abs.to_bits() - 1)
        } else {
            next - abs
        }
```

`native_math_ulp_float` at **2699–2701** is the same three lines on `f32`:

```rust
        let abs = v.abs();
        let next = f32::from_bits(abs.to_bits() + 1);
        if next.is_infinite() {
            abs - f32::from_bits(abs.to_bits() - 1)
        } else {
            next - abs
        }
```

Both classes are fixed by these two edits — `StrictMath.ulp` shares the bodies.

### N4 — the `Character` family needs Java's tables, and needs its duplicate removed

**DONE.** The duplicate deletion is EXECUTED-verified (3837 → 1294 diverging BMP
code points); the tables are written and PREDICTED (0 mismatches against
HotSpot 25 over every code point when replayed out of the source, but not yet
run in a binary). Two of this nomination's recommendations were **not** taken,
and the reasons are measurements rather than preferences:

* *"Simplest correct route: call the real JDK's `CharacterData` through
  bytecode."* Rejected. These natives are registered in **both** modes, and
  synthetic-JDK mode has no `CharacterData` to call — narrowing a registration
  to the mode that has one drops it from the mode that does not, with no
  fallback. Generated tables serve both.
* *"`is_alphabetic() && !is_numeric()` … deliberately NOT applied."* Correct
  call, and now quantified: even the *exact* delta
  (`isAlphabetic && !isLetter`, 949 BMP) is 8 short of the measured 957, because
  Rust's `Alphabetic` table and JDK 25's differ. Any derivation from Rust's
  tables leaves an unnameable residual.

Not a one-line fix, and it should be one lane, not seven patches:

* `native_character_is_whitespace` (`lang_math.rs:3520`),
  `native_character_is_letter` (3508), `native_character_is_digit` (3496),
  `native_character_is_letter_or_digit` (3622), `native_character_digit`
  (4204), `native_character_get_numeric_value` (4255) must stop delegating to
  `char::is_whitespace` / `is_alphabetic` / `is_ascii_digit` / `is_alphanumeric`
  / `to_digit`. These are Unicode's definitions; Java's are different by
  design.
* `native_character_to_upper_case` (3556) / `to_lower_case` (3570) and the
  `_int` twins must (a) return the input unchanged when
  `char::to_uppercase()` yields more than one char, and (b) return the input
  unchanged for a surrogate instead of `unwrap_or('\0')`. **(b) alone is worth
  landing immediately** — `Character.toLowerCase(U+D800) == 0` is silent data
  corruption, and the fix is replacing `.unwrap_or('\0')` with a fallback to
  the input.
* **Delete** the four inline closures at `native-builtins/src/lib.rs:19526`,
  `:19541`, `:19556`, `:19572`. They are verbatim copies of the `lang_math.rs`
  bodies re-registered as `Bridge`, they *win* over the `Intrinsic`
  registrations, and they mean any fix to `lang_math.rs` alone is invisible.
  Their comment says they exist to defeat a JIT miscompile of the
  `CharacterData` chain — the `lang_math.rs` registrations already do that,
  which is presumably why nobody noticed the duplication.

Simplest correct route for the classifiers: call the real JDK's
`java.lang.CharacterData` through bytecode (these are `Intrinsic`, so they may
yield) rather than porting Unicode tables into Rust. Measure first — the
category's whole justification is speed, and if the bytecode is fast enough
these natives should not exist.

### N5 — the `parse*` family must use Java's grammars

**DONE and VERIFIED** (`26e69258a`). The acceptor was transliterated back out of
the Rust and diffed against HotSpot over the named rows plus **800,000 fuzzed
tokens**: 0 divergences. One caveat carried forward — `Character.digit` was
handed to the `Character` lane by W7-99 and is now also done, but the two digit
tables must stay separate; see [Closure](#closure-lane-c1).

`native-builtins/src/lang_math.rs:3035` (`native_integer_parse_int`), old:

```rust
    match text.trim().parse::<i32>() {
```

`Integer.parseInt` does **not** trim (`Double.parseDouble` does — do not merge
the two contracts), and accepts any `Character.digit`, not just ASCII. The
`.trim()` must go and the digit set must widen; the same two changes apply to
`native_long_parse_long` (3350) and the `Byte`/`Short` forms that funnel
through them.

`parse_float_string` (4678) / `parse_double_string` (4693): the fallthrough
`numeric.parse::<f64>()` accepts `nan`, `inf`, `infinity` in any case, which
Java rejects — the explicit `"NaN"`/`"Infinity"` arms above it look like an
enumeration and are not one. Reject anything whose first non-sign byte is not
a digit, `.`, or `0x`/`0X`, then add the hex-significand form
(`0x1p3` → `8.0`) that Java's grammar includes and Rust's does not.

`Double.parseDouble(null)` and `Float.parseFloat(null)` must throw
`NullPointerException`, not `NumberFormatException` — the `_ =>` arms of
`native_float_parse_float` (declared 4707) and `native_double_parse_double`
(declared 4724), both of which currently `return Err(… NumberFormatException {
message: "null" })`. (`Integer.parseInt(null)` really *is*
`NumberFormatException`; the two contracts differ, and both float arms are
copies of the integer one.)

### N6 — `String`'s code-point family

`U+FFFD` appearing in a `codePointAt` answer means the value crossed a
UTF-8/scalar-value boundary. Find that conversion — it is the bug, and it is
upstream of all four `String` code-point rows. Separately, `codePoints()` must
pair surrogates, and `codePointCount`, `offsetByCodePoints` and `repeat` must
perform their specified bounds checks.

### N7 — register the vector (not lane B9's file to edit) — **REQUIRED, NOT OPTIONAL**

`regression-suite/run.sh:106`, append `RJdkIntrinsics` to **`CORE_CLASSES`**,
not `JDKONLY_CLASSES`: these are language semantics, identical in
`--real-jdk` and `--jdk-only`, and the natives are registered in both arms.

```
 ... RJdkViews RJdkFormatLocale RJdkStrictMath RJdkByteOrder RJdkIntrinsics"
```

Until this lands, `regression-suite/src/RJdkIntrinsics.java` is in no class
list, which `run.sh`'s coverage gate reports as a WARNING by default and as a
**failure under `STRICT_COVERAGE=1`, which is what CI runs**. Lane B9 was
scoped out of `run.sh`, so this one line is a hard handoff: whoever picks this
record up must land it in the same change, or park the file in
`UNREGISTERED_CLASSES` (line 157) with a reason. Do not leave it in neither
list.

### N8 — make the instrument able to see this category at all

The deepest item, and the reason W7-94 and this record both had to be found by
hand. `--jdk-only-report` can currently say "5000 intrinsic invocations" and
cannot name one of them. Either:

* emit a per-triple `intrinsic-invoked` row (the census already carries the
  invocation counts per row — the report just does not join them), or
* make `Intrinsic` mean something checkable: require every `Intrinsic`
  registration to carry a differential vector, and have the census fail a
  ratchet on any triple that has none.

Until one of those exists, "reviewed `NativeKind::Intrinsic`" is a claim with
no reviewer.
