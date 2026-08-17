# `Character.digit(int,int)` stays unregistered, and the "15 deliberately unregistered" methods are 57

> **Status: NO CODE CHANGE to `native-builtins/src/lang_math.rs`.** The
> conclusion of this lane is that the correct action is to *not* register
> `Character.digit(int,int)`, and the record exists so the next lane does not
> re-open it. Two text corrections were made (§8).
>
> Every HotSpot number below is **MEASURED** on Microsoft OpenJDK 25.0.3+9-LTS.
> Every claim about the Rust bodies is measured by a **source witness** that
> parses the tables out of the working-tree `lang_math.rs` and transliterates
> the bodies — it is mutation-checked (§10). Every CratonVM "after" is
> **PREDICTED**: this lane may not build and may not run the VM.

Lane E17, 2026-08-13. Follows E7-1 §5 and §7 N4. Files touched:
`regression-suite/src/RJdkIntrinsics2.java`,
`docs/known-issues/jdk-only/W7-98-character-unicode.md`, and this record.

---

## 1. The decision, up front

**`Character.digit(int,int)` must stay unregistered.** Not because the correct
table is hard to build — it is already in the file — but because registering it
buys nothing in either mode and costs a shadow in the mode whose whole purpose
is to run the real image.

* **In real-JDK / `--jdk-only` mode it already answers correctly.** The registry
  census records `java/lang/Character` as `loaded: true, declared: true,
  has_code: true` — every native registered on this class is shadowing working
  bytecode, and `digit(II)I` is one of the few that is not.
* **Registering it against `native_character_digit` would be a regression**, and
  the size of it is now measured rather than asserted: **12,246** wrong answers
  over 390 code points × the radices that admit them (§3).
* **Registering it against a NEW, exact body would be correct but pointless.**
  Such a body was written and verified to **0 mismatches** over all 1,114,112
  code points × all 35 radices plus the out-of-range `int` and radix domains
  (§10, column C). It still would not make `charcls` pass in synthetic-JDK mode,
  because check **84** calls `isISOControl(char)`, which is *also* unregistered
  (§6). Landing one of the two would move the failure from check 74 to check 84
  — the "fix that moves failures deeper" shape — while adding two more shadows
  to a class W7-98 already recommends stripping.
* **The synthetic-JDK gap is not a `lang_math.rs` gap.** The synthetic
  `java/lang/Character` declares **zero methods**
  (`classloading/src/class_manager.rs`, `synthetic_stub_ctor_methods` has no arm
  for it; the stub's own doc says "no methods (all handled by native
  registry)"). 57 of the class's 96 public methods therefore have no
  implementation at all in that mode (§5). Papering over two of them with
  natives does not change that, and hides it.

---

## 2. The contract of `digit(int,int)`, measured over the whole domain

`DigitContract.java`, HotSpot 25, all 1,114,112 code points:

| property | measurement |
|---|---|
| `digit(cp, r) == (v < r ? v : -1)` where `v = digit(cp, 36)` | **0 violations** over 1,114,112 cp × 35 radices |
| radix outside `2..=36` (incl. `0`, `1`, `-1`, `37`, `MIN_VALUE`, `MAX_VALUE`) | always `-1`, never throws |
| `codePoint` outside `0..=0x10FFFF` (incl. `MIN_VALUE`, `0x110000`, `MAX_VALUE`) | always `-1` |
| lone surrogates `U+D800..U+DFFF` | always `-1` |
| `MIN_RADIX` / `MAX_RADIX` | `2` / `36` |

So the whole `(II)I` contract is **one value per code point, gated by the
radix**. There is no radix-dependent value anywhere in Unicode. The value table
over the full range is **80 runs / 864 code points**:

| | runs | code points |
|---|---|---|
| BMP | 41 | 474 |
| supplementary | 39 | 390 |
| total | 80 | 864 |

The BMP half is already `JAVA_DIGIT_RUNS` (38 runs) plus the three ASCII runs
the fast path handles inline (`0-9`, `A-Z`, `a-z` = 62 code points):
38 + 3 = 41 runs, 412 + 62 = 474 code points. **Exact.**

The supplementary half is already `JAVA_SUPPLEMENTARY_DIGIT_RUNS`. Diffed
literally against the JDK-generated list: **39 runs, identical, in order.**
Every supplementary run is base `0` and length `10` — they are all `Nd` decimal
runs, so the value is `cp - first` with no extra column needed.

**Both halves of the table this lane would have needed already exist in
`lang_math.rs`.** That is a reason not to add a native, not a reason to add one:
the data is there, so a future lane that decides differently has no derivation
to do.

### The two overloads are genuinely different functions

`digit((char) cp, r)` and `digit(cp, r)` agree on all 65,536 BMP code points
(**0 mismatches**) and disagree on exactly the 390 supplementary digits. The
fixture's own row is the load-bearing example:

```
digit(0x1D7CE, 10)        == 0      // (II)I — the code point
digit((char) 0x1D7CE, 10) == -1     // (CI)I — the char, which is U+D7CE
```

`(char) 0x1D7CE` is `U+D7CE`, a Hangul syllable. The `char` overload is not a
worse version of the `int` overload; it is a correct function over a smaller
domain, and `Integer.parseInt` depends on it staying that way
(`JAVA_DIGIT_RUNS`' own doc comment records the measurement:
`Integer.parseInt(new String(Character.toChars(0x104A0)))` throws on JDK 25 even
though `digit(0x104A0, 10) == 0`).

### In the JDK there is only ONE implementation, and it is the `(II)I` one

`javap -c java.lang.Character` on JDK 25:

```
public static int digit(char, int);
   0: iload_0
   1: iload_1
   2: invokestatic  #248   // Method digit:(II)I      <-- just a widening
   5: ireturn

public static int digit(int, int);
   0: iload_0
   1: invokestatic  #158   // Method java/lang/CharacterData.of:(I)LCharacterData;
   4: iload_0
   5: iload_1
   6: invokevirtual #252   // Method java/lang/CharacterData.digit:(II)I
   9: ireturn
```

So the JDK's `(CI)I` is a one-line widening of `(II)I`, and the whole
BMP-vs-supplementary distinction is an artefact of `char` being 16 bits — not a
second algorithm. CratonVM has it the other way round: `(CI)I` is a native over
a BMP table and `(II)I` is the bytecode. The two arrangements agree because a
widened `char` can never exceed `U+FFFF`, which is exactly the domain
`JAVA_DIGIT_RUNS` covers.

**This is an argument for deleting the `(CI)I` native, not for adding a `(II)I`
one.** If `digit(CI)I` were deregistered, `Character.digit(char,int)` would fall
straight through to the `(II)I` bytecode and keep answering correctly, W7-98's
recommendation would advance by one triple, and the BMP-only table would have
one caller left (`java_parse_signed`, which is the caller it was written for).
The same holds for `isISOControl(I)Z`, whose `(C)Z` sibling is likewise a bare
widening (`iload_0; invokestatic isISOControl:(I)Z; ireturn`) — which is why
check 84 lands on the registered native in real-JDK mode.

---

## 3. The trap, quantified

E7-1 §5 warned in words: do not register `digit(II)I` against
`native_character_digit`, whose table is BMP-only. The size of that mistake:

| body | domain | mismatches vs HotSpot 25 |
|---|---|---|
| `native_character_digit` as **`(CI)I`** — the registration that exists | 65,536 cp × 35 radices | **0** |
| `native_character_digit` re-used as **`(II)I`** — the trap | 1,114,112 cp × 35 radices | **12,246** |
| a code-point-aware body using both tables | 1,114,112 cp × 35 radices | **0** |
| unregistered (real JDK bytecode) | all | **0** |

12,246 is not a round number and it should not be: a supplementary digit of
value `v` is wrong at every radix `> v`, so each 10-long `Nd` run contributes
`35+35+34+33+…+27 = 314`, and `314 × 39 = 12,246`. The first wrong code point is
`U+104A0` (OSMANYA DIGIT ZERO), not the `U+1D7CE` the fixture happens to name.

Note what this row would have looked like on a check-74-only measurement: the
naive registration answers `-1` where HotSpot answers `0`, so it fails the
fixture immediately. **The fixture would have caught it.** What the fixture
would *not* have shown is that it is 390 code points wide, or that
`U+1D7CE` is not even the first one.

---

## 4. The two siblings

### `forDigit(int,int)` — no BMP limitation, and it cannot have one

Checked because the task shape ("BMP-only table") looked like it should apply.
It does not, and the reason is worth stating so nobody checks again:
**`forDigit`'s first argument is a digit VALUE in `0..radix`, not a code point,
and its result is always ASCII** — `'0'..'9'` or `'a'..'z'`, maximum `U+007A`.
There is no table and nothing to be BMP-only about.

Measured: `forDigit(d, r) == (0 <= d < r && 2 <= r <= 36 ? lowercase digit :
U+0000)` with **0 violations** over `d, r ∈ -40..80` plus the `MIN_VALUE` /
`MAX_VALUE` extremes. The registered `native_character_for_digit` reproduces
that with **0 mismatches** (§10, column F), including the negative-digit path
where `*v as u32` widens `-1` to `0xFFFF_FFFF` and `char::from_digit` correctly
returns `None`.

**No action. It is registered, it is exact, and it is not a member of the
BMP-only family.**

### `getNumericValue` — the SAME trap, a different table, 1,177 wide

`getNumericValue` is registered **only as `(C)I`**, and its
`JAVA_NUMERIC_VALUE_RUNS` / `JAVA_NUMERIC_VALUE_NEG2_RUNS` are BMP-only. Over
the BMP that body is exact (**0 mismatches**, §10 column D), and the `(C)I`
domain cannot exceed `U+FFFF`, so today it is correct by construction.

`getNumericValue(int)` is unregistered. If someone registers `(I)I` against the
same body:

| | measurement |
|---|---|
| supplementary code points with `getNumericValue != -1` | **1,177** |
| of which return the `-2` sentinel | **66** |
| non-negative values observed | `0` .. `100,000,000` |
| mismatches a naive `(I)I` registration would produce | **1,177** |

Worked examples the `(C)I` body answers `-1` for and HotSpot does not:
`getNumericValue(0x1D7CE) == 0`, `getNumericValue(0x10107)` (AEGEAN NUMBER ONE)
`== 1`, `getNumericValue(0x12432)` (CUNEIFORM NUMERIC SIGN SHAR2 TIMES GAL PLUS
DISH) `== 216,000`. That last one also shows why this table cannot be derived
from the digit runs at all: 216,000 is not a digit in any radix.

Out-of-range `int`s are safe either way: HotSpot answers `-1` for
`MIN_VALUE`, `-1`, `0x110000` and `MAX_VALUE`, and so does the Rust body.

**No action. `(C)I` stays registered; `(I)I` stays unregistered, for the same
reason as `digit(II)I` and with a bigger number behind it.**

---

## 5. The record section: what is deliberately unregistered, and what breaks if you register it

E7-1 §7 N4 listed ~15 methods as "deliberately unregistered, running real
`CharacterData` bytecode correctly". Verified against the registry dump
(`--dump-native-registry`, `class == java/lang/Character`, `registered_by` /
`owns_slot`) and against JDK 25 reflection. **The list is right in spirit and
wrong in two ways that matter.**

### 5.1 It is 57, not 15

`java.lang.Character` on JDK 25 has **96** public methods. CratonVM registers
**39** unique `(name, descriptor)` triples for it. **57 are unregistered.**

Registered (39): `charCount(I)I`, `charValue()C`, `digit(CI)I`,
`equals(Ljava/lang/Object;)Z`, `forDigit(II)C`, `getNumericValue(C)I`,
`hashCode()I`, `isBmpCodePoint(I)Z`, `isDigit(C)Z`, `isDigit(I)Z`,
`isEmoji(I)Z`, `isEmojiComponent(I)Z`, `isEmojiModifier(I)Z`,
`isEmojiModifierBase(I)Z`, `isEmojiPresentation(I)Z`, `isHighSurrogate(C)Z`,
`isISOControl(I)Z`, `isJavaLetter(C)Z`, `isJavaLetterOrDigit(C)Z`,
`isLetter(C)Z`, `isLetter(I)Z`, `isLetterOrDigit(C)Z`, `isLetterOrDigit(I)Z`,
`isLowSurrogate(C)Z`, `isLowerCase(C)Z`, `isLowerCase(I)Z`, `isSpace(C)Z`,
`isUpperCase(C)Z`, `isUpperCase(I)Z`, `isValidCodePoint(I)Z`,
`isWhitespace(C)Z`, `isWhitespace(I)Z`, `toLowerCase(C)C`, `toLowerCase(I)I`,
`toString()Ljava/lang/String;`, `toString(C)Ljava/lang/String;`,
`toUpperCase(C)C`, `toUpperCase(I)I`, `valueOf(C)Ljava/lang/Character;`.

Unregistered (57), grouped by why they are unregistered:

| group | methods | why registering is a risk |
|---|---|---|
| **`CharacterData`-backed classifiers** | `getType(C)I` `getType(I)I` `getDirectionality(C)B` `getDirectionality(I)B` `isDefined(C)Z` `isDefined(I)Z` `isMirrored(C)Z` `isMirrored(I)Z` `isSpaceChar(C)Z` `isSpaceChar(I)Z` `isTitleCase(C)Z` `isTitleCase(I)Z` `toTitleCase(C)C` `toTitleCase(I)I` `isAlphabetic(I)Z` `isIdeographic(I)Z` `isExtendedPictographic(I)Z` `isIdentifierIgnorable(C)Z` `isIdentifierIgnorable(I)Z` `isJavaIdentifierStart(C)Z` `isJavaIdentifierStart(I)Z` `isJavaIdentifierPart(C)Z` `isJavaIdentifierPart(I)Z` `isUnicodeIdentifierStart(C)Z` `isUnicodeIdentifierStart(I)Z` `isUnicodeIdentifierPart(C)Z` `isUnicodeIdentifierPart(I)Z` `getNumericValue(I)I` `digit(II)I` | Any Rust reimplementation is **Rust's Unicode database**, which is a *newer* version than the image's (E7-1 §3: rustc 1.96 assigns `U+A7CE`/`U+A7CF`/`U+A7D2`/`U+A7D4`/`U+A7F1`, JDK 25 calls them `UNASSIGNED`). W7-98 §4 drove `getType`, `toTitleCase`, `isSpaceChar`, `isAlphabetic`, `getDirectionality` through 6,000,000 invocations each, cold and hot, against HotSpot: **0 mismatches**. They are correct *because* nothing shadows them. |
| **BMP-only table would be re-used** *(the same two rows again — not additional methods)* | `digit(II)I` `getNumericValue(I)I` | Quantified above: **12,246** and **1,177** wrong answers. |
| **UTF-16 arithmetic over other methods** | `codePointAt` ×3 `codePointBefore` ×3 `codePointCount` ×2 `offsetByCodePoints` ×2 `toChars(I)[C` `toChars(I[CI)I` `toCodePoint(CC)I` `toString(I)Ljava/lang/String;` `highSurrogate(I)C` `lowSurrogate(I)C` `isSurrogate(C)Z` `isSurrogatePair(CC)Z` `isSupplementaryCodePoint(I)Z` | These reach their contracts *through* the registered natives. E7-1 §2 records the concrete coupling: `toChars(-1)` throws `IllegalArgumentException` only because the unregistered bytecode consults the registered `isBmpCodePoint`/`isValidCodePoint`, whose unsigned casts must therefore stay unsigned. Registering the caller too would let the pair drift. |
| **plain value methods** | `compare(CC)I` `compareTo(Ljava/lang/Character;)I` `compareTo(Ljava/lang/Object;)I` `hashCode(C)I` `reverseBytes(C)C` `describeConstable()Ljava/util/Optional;` `isISOControl(C)Z` | Nothing to gain; each is 1–3 bytecodes and would pay the ~120 ns `safe_native_call` funnel to save less. `isISOControl(C)Z` is a bare widening onto the registered `(I)Z` (§2), so in real-JDK mode it already answers out of the native. |
| **Unicode name/database lookups** | `getName(I)Ljava/lang/String;` `codePointOf(Ljava/lang/String;)I` | There is no Rust-side Unicode name table at all. A stand-in here fabricates. |

Group sizes, so the table can be checked rather than trusted:
**29 + 19 + 7 + 2 = 57**, the second row re-listing two of the first row's.

### 5.2 "runs real `CharacterData` bytecode correctly" is true of ONE mode

This is the correction that matters, because it changes what the list means.

| mode | what an unregistered `Character` method does |
|---|---|
| **real-JDK / `--jdk-only`** | Runs the image's own bytecode. Measured correct (W7-98 §4). **N4's claim holds here and only here.** |
| **synthetic-JDK** | **`NoSuchMethodError`.** There is no bytecode and the synthetic `java/lang/Character` declares no methods. |

Traced, not assumed. In synthetic-JDK mode `ClassManager::load_class` falls
through to `create_synthetic_stub`, whose doc says "The stub has no methods (all
handled by native registry)", and `synthetic_stub_ctor_methods` has **no arm for
`java/lang/Character`** — so its method list is empty. The resolution path is
exact on the descriptor at **every** step: `execute_invokestatic`'s registry
probe, `try_stackless_invoke`'s probes, `Class::find_method`
(`m.name == name && m.descriptor == descriptor`), and the terminal recovery
walk. The registry's only descriptor fallback
(`find_with_descriptor_quirks`) rewrites whitespace and a missing return-type
`;` and **never rewrites argument types**, so the registered `digit(CI)I` can
never be picked up by a `digit(II)I` call site. `NativeKind::SyntheticStub` is a
*label on registrations that exist*, not a body-minting mechanism — nothing in
the tree mints a body for an unregistered triple. The throw is
`NoSuchMethodError`, not `AbstractMethodError` (that one needs a method to be
found with no `Code`) and not a fabricated `0`.

**So the sentence to carry forward is: these methods are deliberately
unregistered because in the mode that has a class library they are correct, and
in the mode that does not, the answer is a missing synthetic class library — not
a missing native.**

### 5.3 Four arity gaps, and they are not accidents

Where CratonVM registers one overload of a pair and not the other:

| registered | unregistered sibling |
|---|---|
| `digit(CI)I` | `digit(II)I` |
| `getNumericValue(C)I` | `getNumericValue(I)I` |
| `isISOControl(I)Z` | `isISOControl(C)Z` |
| `toString(C)Ljava/lang/String;` | `toString(I)Ljava/lang/String;` |

The first two are the BMP-only trap and must stay as they are. The second two
are the reverse shape — the registered arity is the *wider* one — and are
harmless in real-JDK mode for the same reason. All four are `NoSuchMethodError`
in synthetic-JDK mode.

### 5.4 One stale row in the registry dump, named so nobody re-derives it

`scratchpad/p1/reg.json` was captured **before** E7-1's prerequisite landed. It
shows four rows — `toUpperCase(C)C`, `toLowerCase(C)C`, `toUpperCase(I)I`,
`toLowerCase(I)I` — as `owns_slot: false` in `lang_math.rs`, overwritten by
`bridge` copies at `lib.rs:19526/19541/19556/19572`. **Those four
re-registrations are gone**; `native-builtins/src/lib.rs` now carries a
tombstone comment in their place, verified by reading the working tree. The
`lang_math.rs` bodies own their slots today. Every other row in the dump was
re-checked against the source and agrees.

---

## 6. `charcls` has TWO unregistered call sites, not one

Every `java/lang/Character` call site in `charcls` was extracted from the
compiled fixture with `javap -c` and cross-referenced against the 39 registered
triples. 23 distinct triples are called. Two are unregistered:

| check | call | descriptor | real-JDK | synthetic-JDK |
|---|---|---|---|---|
| **74** | `Character.digit(U+1D7CE, 10)` | `digit(II)I` | passes (bytecode) | `NoSuchMethodError` |
| **84** | `Character.isISOControl(' ')` | `isISOControl(C)Z` | passes (bytecode) | `NoSuchMethodError` |

Check indices are **mechanically** confirmed, not counted by hand: an
instrumented copy of the fixture printing `CHECK#n` per assertion puts
`charCount(-1)` at **52** (E7-1's abort point), `digit(U+1D7CE, 10)` at **74**
and `isISOControl(' ')` at **84**.

`isISOControl(char)` is a first-execution discovery of this lane: `charcls`
calls `isISOControl` six times, five of them on `int` (checks 39–43, registered)
and once on `char` (check 84, not). Both land on correct answers in real-JDK
mode — the JDK's `(C)Z` is literally `isISOControl((int) c)`, and the registered
`native_character_is_iso_control` reads its argument as a signed `i32` and range
-tests it, which is exact for every `char`.

---

## 7. Which `charcls` checks this lane's work affects

**None of the assertions changed.** `charcls` is still 86 checks, three failure
*messages* were reworded (§8), and no check was added, removed or reordered.

| | prediction |
|---|---|
| HotSpot 25 oracle | **MEASURED, now**: `CK RJdkIntrinsics2 charcls=86`, `PASS RJdkIntrinsics2 (86 checks)` — including the reworded messages, so the fixture still compiles and still passes on the oracle. |
| CratonVM real-JDK, with E7-1's `charCount` fix | **PREDICTED** `CK RJdkIntrinsics2 charcls=86`. Checks 74 and 84 pass on real bytecode; this lane changes nothing about them. |
| CratonVM synthetic-JDK, with E7-1's `charCount` fix | **PREDICTED**: aborts at check **74** with `NoSuchMethodError: java/lang/Character.digit(II)I`. That is the abort point moving from 52 to 74 — **21 more checks executing than today**, not a regression. Check 84 is the next one behind it. |
| CratonVM either mode, without E7-1 | aborts at check 52; 74 and 84 never reached. |

The measurement to demand of whoever runs this: in synthetic-JDK mode, `charcls`
should reach **73** and then throw. If it reaches 86 there, something is
answering `digit(II)I` and this record's §5.2 mechanism is wrong.

---

## 8. Text corrections made

### 8.1 `regression-suite/src/RJdkIntrinsics2.java` — three messages that guessed at a mechanism

E7-1 §7 N1 nominated one. Reviewing the block turned up two more of the same
shape, and the principle is the point: **a fixture message is read as evidence.**
The `charCount` one cost a lane a hunt for a Rust panic that never existed (the
real cause was `*v as u32` making `-1` unsigned). All three now state the
observable.

| check | was | now |
|---|---|---|
| 52 | `"… must be 1 — below MIN_SUPPLEMENTARY, not a panic"` | `"… must be 1 — the compare is SIGNED; widening the argument to unsigned answers 2"` |
| 43 | `"Character.isISOControl(-1) must be false, not a panic"` | states that the range test is signed and that widening still answers false, so a `true` means the *range* is wrong |
| 47 | `"Character.forDigit(-1, 16) must be U+0000, not a panic"` | states that a negative digit is outside `0..radix` and widening it must not make it a digit |

Checks 43 and 47 could not panic either. The `forDigit` panic hazard W7-98(a)
closed was `char::from_digit` **with a radix above 36** — which is check **49**
(`forDigit(0, 37)`), whose message never mentioned a panic. The claim was on the
wrong row.

Verified: the edited fixture compiles (`javac`, exit 0) and passes on HotSpot 25
with `charcls=86`.

### 8.2 `docs/known-issues/jdk-only/W7-98-character-unicode.md`

Three result rows updated to `0` per E7-1 §7 N2 (`toUpperCase` 27 → 0,
`toLowerCase` 3 → 0, `isUpperCase`/`isLowerCase` 3/3 → 0/0), and the matching
§6 residual bullets reconciled so the document does not contradict its own
table. The supplementary-digits bullet was rewritten with this lane's numbers.

**W7-98's headline claim is NOT closed and is not marked closed.** "The class
does not need these natives; it needs them gone" still stands: 39 triples are
still registered on a class the census reports as `has_code: true`. This lane
adds evidence *for* that recommendation — it declined to add a 40th and a 41st.

---

## 9. Nominations

### N1 — `docs/known-issues/jdk-only/E7-1-character-int-code-point-contracts.md` §7 N4: the list is 57, and its correctness claim is mode-scoped

Owned by lane E7. The warning is right; the count and the scope are not.

*old, verbatim:*

```
### N4 — no edit, a warning for whoever touches this family next

`Character.digit(int, int)`, `getNumericValue(int)`, `toChars`, `toString(int)`,
`highSurrogate`, `lowSurrogate`, `isSupplementaryCodePoint`, `getType`,
`isSpaceChar`, `toTitleCase`, `isAlphabetic`, `isDefined`, `getDirectionality`,
`isMirrored` and the identifier predicates are **deliberately unregistered** and
run real `CharacterData` bytecode, which W7-98 measured correct on all 65,536
BMP code points including hot. Registering any of them is a regression risk, and
`digit(II)I` specifically would be one today (§5).
```

*new:*

```
### N4 — no edit, a warning for whoever touches this family next

**57** of `java.lang.Character`'s 96 public methods are unregistered — the
partial list here (`digit(int,int)`, `getNumericValue(int)`, `toChars`,
`toString(int)`, `highSurrogate`, `lowSurrogate`, `isSupplementaryCodePoint`,
`getType`, `isSpaceChar`, `toTitleCase`, `isAlphabetic`, `isDefined`,
`getDirectionality`, `isMirrored`, the identifier predicates) is a sample of it.
The full enumeration, the per-group reason each must stay unregistered, and the
measured cost of registering the two that reuse a BMP-only table are in
`E17-1-character-digit-int-and-the-fifteen-unregistered.md` §5.

They run real `CharacterData` bytecode, which W7-98 measured correct on all
65,536 BMP code points including hot — **in real-JDK mode**. In synthetic-JDK
mode there is no bytecode and the synthetic `Character` declares no methods, so
each of the 57 is a `NoSuchMethodError`; that is a missing synthetic class
library, not a missing native, and must not be closed by registering natives.
Registering any of them is a regression risk, and `digit(II)I` specifically
would cost 12,246 wrong answers today (§5, E17-1 §3).
```

### N2 — the deprecated trio is registered, so "the identifier predicates are unregistered" needs a boundary

`isJavaLetter(C)Z`, `isJavaLetterOrDigit(C)Z` and `isSpace(C)Z` — the deprecated
1.0 predicates — **are** registered, as `bridge` from
`native-builtins/src/deprecated_util.rs:2076/2077/2083`, each overwriting a
duplicate registration of the same triple in
`native-builtins/src/deprecated_io_util.rs:933/944/955` (`overwrote: bridge`,
`owns_slot` false on the loser). The modern `isJavaIdentifierStart/Part`,
`isUnicodeIdentifierStart/Part` and `isIdentifierIgnorable` are the unregistered
ones. Nominated for the owner of those two files: **three triples registered
twice from two files is a duplicate-registrar shape** — the losing copies in
`deprecated_io_util.rs` are dead and should be deleted, or the pair reconciled,
before a future edit "fixes" the wrong one. No edit made here; neither file is
this lane's.

### N3 — `classloading/src/class_manager.rs`: the synthetic `Character` gap is measurable and unmeasured

`synthetic_stub_ctor_methods` has no `java/lang/Character` arm, so the class
carries **0** declared methods against 96 public ones on the real class, of
which 39 are covered by natives. Nominated as a **measurement**, not an edit:
the same ratio can be computed for every synthetic stub, and `Character`'s 57
is unlikely to be the largest. Whoever owns the synthetic-JDK gate should
enumerate `(public methods on the real class) − (registered triples) −
(declared stub methods)` per synthetic class; that number is the exact list of
`NoSuchMethodError`s that mode can produce, and it is derivable today from
`--dump-native-registry` plus reflection on the image, with no VM run.

### N4 — no edit: `native-builtins/src/lang_math.rs`

This lane owns the file and deliberately changed nothing in it. `digit(II)I`,
`getNumericValue(I)I` and `isISOControl(C)Z` stay unregistered; §1 and §4 are
the reasons. If a later lane reverses this, the table it needs is already in the
file (`JAVA_DIGIT_RUNS` + `JAVA_SUPPLEMENTARY_DIGIT_RUNS`, §2) and the body it
needs is verified in `Witness17.java` column C — but it must also handle
`isISOControl(C)Z` in the same change, or it will only move `charcls`'s
synthetic-mode abort from check 74 to check 84.

---

## 10. Reproducing

Lane scratchpad (`.../scratchpad/e17/`), all HotSpot-only, **no CratonVM binary
required**:

| file | what it does |
|---|---|
| `DigitContract.java` | the whole-domain oracle: radix independence over 1,114,112 cp × 35 radices, out-of-range radices and code points, the 80-run value table, the supplementary `getNumericValue` count, the `forDigit` model -> `contract.txt`, `digit_runs_full.txt`, `digit_runs_supp.txt` |
| `Witness17.java` | **source witness** — parses `JAVA_DIGIT_RUNS`, `JAVA_SUPPLEMENTARY_DIGIT_RUNS`, `JAVA_NUMERIC_VALUE_RUNS`, `JAVA_NUMERIC_VALUE_NEG2_RUNS` out of the working-tree `lang_math.rs`, asserts sorted+disjoint, transliterates the bodies, diffs six columns |
| `Unregistered.java` | JDK 25 reflection × the 39 registered triples -> the 57-method split |
| `num/` | the instrumented fixture copy that prints `CHECK#n`, pinning 52 / 74 / 84 |

Witness output against the current working tree:

```
parsed from working tree: DIGIT=38 SUPP=39 NUM=123 NEG2=9 runs
  sorted+disjoint: DIGIT=true SUPP=true NUM=true NEG2=true

== 0..0x10FFFF x radix 2..36, transliterated vs HotSpot 25 ==
  A digit(char,r)   REGISTERED (CI)I      0 mismatches
  B digit(int,r)    NAIVE (II)I re-use    12246 mismatches (first cp U+104A0, 390 code points wrong at r=36)
  C digit(int,r)    code-point-aware body 0 mismatches
  D getNumericValue(char) REGISTERED (C)I 0 mismatches
  E getNumericValue(int)  NAIVE (I)I      1177 mismatches (first cp U+10107)
  F forDigit(int,int)     REGISTERED      0 mismatches
  out-of-range ints/radices                 0 mismatches
```

**Mutation-checked**, because a witness that cannot fail measures nothing.
`--mutate-supp` shifts `JAVA_SUPPLEMENTARY_DIGIT_RUNS[28]` (the `U+1D7CE` run)
up by 1 and column C turns up **322** mismatches; `--mutate-digit` adds 1 to
`JAVA_DIGIT_RUNS[0].value` (`U+0660`) and columns A and C each turn up **314**.
Both non-zero, on different columns, from different tables.

### What this proves and what it does not

It proves the **contract** and the **tables**: `Character.digit(int,int)`'s
answer over the entire `int` × `int` domain, that both halves of the table it
would need are already in the tree and are exactly the JDK's, and that the
registered `(CI)I`, `(C)I` and `(II)C` bodies agree with HotSpot 25 everywhere
they can be called. It does not prove anything about a CratonVM binary — none
carrying E7-1's changes exists — and the synthetic-JDK predictions in §5.2 and
§7 are read off the resolution path in the source, not off a run.
