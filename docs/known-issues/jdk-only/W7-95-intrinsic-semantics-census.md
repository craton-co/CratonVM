# W7-95 — the `NativeKind::Intrinsic` category has never had its semantics checked

**Status: OPEN. 39 divergent triples found in a 258-triple sample, six of them
VM-fatal. Both shipping modes are affected; none of this is
`--jdk-only`-specific.**

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

* **645** triples are registered `Intrinsic` in this configuration
  (`--dump-native-registry`, a hello-world run: the registry is populated at VM
  init, so the number does not depend on what the program touches).
* **258** of those 645 were actually invoked by the probe — the census's own
  per-row `invocations` column, not a claim. **40% coverage.**
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

Counted as untested rather than as defects, but note the asymmetry:
`StrictMath.floorDiv(JJ)J` and `StrictMath.floorMod(JJ)J` are registered onto
the *same* `native_math_floor_div_long` / `native_math_floor_mod_long` bodies
that abort the VM for `Math`, so they are near-certainly two more VM-fatal
triples that this probe simply did not call. The nineteen other untested
`StrictMath` integral forms (`addExact`, `multiplyExact`, `negateExact`,
`min`/`max` on `II`/`JJ`, …) share bodies with `Math` twins that measured
**correct**, so those are the reverse case: probably green, unverified.

### VM-fatal: `floorDiv` / `floorMod` at `MIN_VALUE / -1`

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

**387 of the 645 registered `Intrinsic` triples were never invoked**, and the
untested set is *not* a random remainder. Largest families, with the reason each
is worth a lane:

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

## NOMINATIONS

Lane B9 does not build. Every item below is an exact edit for someone who does.

### N1 — VM-fatal. `floorDiv`/`floorMod` must wrap, not panic

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
