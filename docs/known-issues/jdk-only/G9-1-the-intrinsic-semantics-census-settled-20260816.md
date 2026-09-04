# G9-1 — the `Intrinsic` semantics census, settled where it could be

> **RECONCILED 2026-08-17 (lane G40) — a question this record left open for want
> of sources was answerable.** It names `C:\craton\jdk25src`'s absence as the
> reason. That directory is indeed absent, but the JDK's sources ship with the
> oracle itself, at `$JAVA_HOME/lib/src.zip` (52,462,198 bytes) — the sources of
> the exact HotSpot 25.0.3+9-LTS build used here. Nothing measured in this record
> is invalidated; the open question was cheaper than it looked. See `INDEX.md`
> §B.3.

**Status:** FIXED-UNVERIFIED-ON-CRATONVM. Four defects fixed, all four measured
against the oracle **before** the fix and re-measured after it *through a
standalone `rustc` build of the edited Rust*, never through a CratonVM binary.

> **NOTHING IN THIS RECORD WAS MEASURED ON A CRATONVM BINARY.** The orchestrator
> owned the build for the whole of this lane and no `cargo` command was run.
> Every claim is tagged:
>
> * **MEASURED** — executed. Either on the oracle (Microsoft OpenJDK
>   25.0.3+9-LTS at `$JAVA_HOME`) or on a standalone `rustc -O` binary built
>   from Rust extracted **verbatim** from the working tree, or both and
>   compared. This is the strongest tag available to this lane and it is
>   *still not* a CratonVM measurement.
> * **SOURCE-VERIFIED** — read out of the working tree, cross-checked by a
>   script over every file rather than by eye.
> * **PREDICTED** — neither.
>
> The technique that makes the MEASURED rows real is new here and is the thing
> worth keeping: **`rustc` is available and the workspace `target/` lock is not
> in its way.** A standalone `rustc --edition 2021` build of table constants and
> predicate bodies lifted verbatim out of `lang_math.rs` / `case_map.rs` /
> `intrinsics/record.rs` executes *the shipped algorithm* against the oracle
> over its whole domain, with no VM, no build, and no lock. `W7-95a` said the
> Rust half of its sweep "cannot be executed from here" and used **Python** as a
> proxy; that proxy is what hid fifty of the fifty-six code points in §2. It can
> be executed from here.

Lane G9, 2026-08-16/17. Files changed: `native-builtins/src/case_map.rs`,
`native-builtins/src/intrinsics/record.rs`,
`native-builtins/src/intrinsics/mod.rs`,
`native-builtins/src/intrinsics/math.rs`, and this record.

Probes (scratchpad, not committed): `charsweep.exe` / `CharSweep.java`,
`case1.exe` / `CaseSweep.java`, `case_map_sweep.exe`, `mathsweep.exe` /
`MathSweep.java`, `RecProbe.java`, `NanChk.java`.

---

> **VERIFIED AGAINST A BINARY 2026-09-04, and the vector REFUTES part of what it
> was written to confirm.** Status was **FIXED-UNVERIFIED-ON-CRATONVM**.
>
> **`RJdkG9Skew` is not in the regression suite** — this record writes it out in
> full and it was never added, so `regression-suite/src/RJdkG9Skew.java` does not
> exist and no suite run has ever executed it. Extracted from this record and
> kept as `probes/RJdkG9Skew.java`.
>
> ```text
>                       checks   fails   self-diff over 2 runs
> HotSpot 25              768      0       0
> CratonVM compatible     768      3       0
> CratonVM --jdk-only     768      3       0
> ```
>
> **765 of 768 agree. The three that fail are one defect — FINAL SIGMA:**
>
> ```text
> sig.bulk.final        "AΣ".toLowerCase(ROOT)              want 97,962        got 97,963
> sig.unassigned.a7ce   "AΣ꟎".toLowerCase(ROOT)        want 97,962,42958  got 97,963,42958
> sig.unassigned.16ea0  "AΣ"+U+16EA0 .toLowerCase(ROOT)     want 97,962,55323,56992
>                                                            got 97,963,55323,56992
> ```
>
> 962 is U+03C2 FINAL SIGMA, 963 is U+03C3 medial. A sigma at end-of-word must
> lower-case to the final form; we give the medial form in all three.
>
> **The record's own controls localise it, and they all pass.** Every row that
> WANTS 963 is correct — `sig.assigned.a7d3` (A7D3 is an assigned lowercase
> letter, so the sigma stays medial), `sig.cased.after` (the sigma is not final).
> So the rule is not simply absent. And `sig.bulk.pair` — `"ΣΣ"` → `963,962` —
> **passes**, so final-sigma is produced correctly when the preceding character
> is a sigma and wrongly when it is `A`. That contrast is in the data and is the
> place to start; it is NOT diagnosed here.
>
> **This contradicts a stated premise.** The record's own comment on the
> `sig.bulk.*` rows reads *"No skewed code point: the bulk path, which was
> already right."* `sig.bulk.final` is a bulk-path row and it fails, so the bulk
> path is not right — for this input it never was, or has since regressed.
> Whichever it is, the sentence cannot stand as written.
>
> **What this does NOT verify.** This is one of the vectors this record names,
> not its whole census; the other 765 checks passing says the rest of the
> measured surface holds on these inputs and nothing about inputs the vector does
> not contain. §§ that are source or oracle censuses were not re-derived. The
> three failures are identical in both modes, so nothing here is mode-specific,
> and no claim is made about `--synthetic-jdk`, which was not run.

## 0. The headline

| subject | before | after | how |
|---|---|---|---|
| `W7-95a`'s two `VM ABORT` rows | contradiction open since the INDEX | **both bodies are fixed in the merged tree; neither can panic** | SOURCE-VERIFIED, §1 |
| `W7-98`'s "inert until deregistration" | fixes shadowed by `lib.rs` twins | **the twins are gone; `lang_math.rs` is the only `Character` registrar** | SOURCE-VERIFIED, §1 |
| `java.lang.Character`, 17 registered methods, full domain | — | **0 mismatches** over 1,114,112 code points | MEASURED, §3 |
| `String.to{Upper,Lower}Case(ROOT)`, full domain | **25 + 25** wrong | **0 + 0** | MEASURED, §2 |
| Final_Sigma over the full domain | 259 wrong | **205** wrong, 0 newly broken | MEASURED, §2 |
| record `float`/`double` component hash and equality | 3 wrong rules | **0** | MEASURED, §4 |
| record `boolean` component hash | `0`/`1` | **`1237`/`1231`** | MEASURED, §4 |
| `Math.abs`/`sqrt`/`min`/`max` intrinsics | — | **200,158 rows identical**, bit for bit | MEASURED, §5 |
| `Math.pow`'s `powi` fast path | reported gone by a merge lane | **confirmed gone** | SOURCE-VERIFIED, §5 |

---

## 1. The `W7-95a` VM-ABORT contradiction — answered

The INDEX's contradiction 2 says rows 14 and 38 of the String code-point family
(`offsetByCodePoints(10,-1)`, `indent(-1)` over a leading NBSP) are recorded as
Rust panics, that "no family aborts the VM" is scoped to `floorDiv`/`floorMod`
and the second-generation census, and that **whether these two still abort is
not established**. It is now.

**Answer: neither row can abort. Both bodies were rewritten and the rewrites are
in the merged tree. The `VM ABORT` cells describe a body that no longer exists.
The rows are historically true and presently false.** SOURCE-VERIFIED — this
lane may not run the VM, so this is a reading of the *current* code, not a
re-execution. But it is not a reading of one line: it is the panic shape,
followed to every site that could reach it, plus the registration audit that
says the read body is the one that runs.

### Row 14 — `offsetByCodePoints(10, -1)`

`native-builtins/src/lang_string.rs:5827`. Three independent reasons it cannot
panic now, where the old body had none:

```rust
if index_i32 < 0 || index_i32 > length {
    return Err(cratonvm_types::error::RuntimeError::ioobe_no_message().into());
}
```

* The precondition throws **before** `pos` is formed, so `pos <= chars.len()`
  always. The old body cast `index` straight to `usize`.
* The backward loop is `while x > 0 && pos > 0 { pos -= 1; ... }` — `pos` is
  guarded at every decrement, and `chars[pos]` is only reached after it.
* `Integer.MIN_VALUE` is handled as `i64::from(code_point_offset.unsigned_abs())`
  with an explicit comment naming the overflow. The old `for _ in
  0..(-code_point_offset)` is gone.

### Row 38 — `indent(-1)` of `[00A0, 0061]`

`native-builtins/src/lang_string.rs:5948`. The panic was
`&line[skip..]` slicing a `str` at a byte offset inside a multi-byte character.
The current body never forms a byte offset by arithmetic:

```rust
let remove = n.unsigned_abs() as usize;
let non_ws = line.chars().position(|c| !char_is_java_whitespace(c))
                 .unwrap_or_else(|| line.chars().count());
let skip_chars = remove.min(non_ws);
let byte_off = line.char_indices().nth(skip_chars).map_or(line.len(), |(i, _)| i);
result.push_str(&line[byte_off..]);
```

`char_indices()` can only yield boundaries, `.nth()` past the end yields `None`
and falls back to `line.len()`, and `unsigned_abs` removes the `MIN_VALUE`
negation. It also switched from Rust's whitespace table to
`char_is_java_whitespace`, which is the row's *other* half.

### Which body wins — checked, not assumed

`W7-95a`'s N1 (`codePoints` bound to `native_string_chars`) **has landed**:
`native-builtins/src/lang_math.rs:795` now registers
`("java/lang/String", "codePoints", "()Ljava/util/stream/IntStream;")` onto
`crate::lang_string::native_string_code_points`. A repo-wide scan for the two
triples finds exactly two registrars each:

| triple | registrar | reached? |
|---|---|---|
| `String.offsetByCodePoints (II)I` | `lang_math.rs:837` → `native_string_offset_by_code_points` | yes, the only one |
| `String.indent (I)Ljava/lang/String;` | `lang_math.rs:849` → `native_string_indent` | yes |
| `String.indent (I)Ljava/lang/String;` | `lib.rs:23533` | no — `register_synthetic_overrides`, `#[cfg(feature = "synthetic-jdk")]` |
| `String.codePoints ()…IntStream;` | `lang_math.rs:795` → `native_string_code_points` | yes |
| `String.codePoints ()…IntStream;` | `lib.rs:23473` | no — same gate |

`phases_early.rs`'s inline `repeat` / `codePointAt` closures that `W7-95a` §"Which
registration actually wins" lists as later-and-therefore-winning-if-enabled are
behind that same gate. **SOURCE-VERIFIED.** The one thing this cannot show is
`invocations`; that needs `--dump-native-registry` on the new binary, and it is
in §9's build-time asks.

### `W7-98`'s inertness — also answered

`W7-98` §1 records four `Character` triples registered twice, the `lang_math.rs`
copy losing to a verbatim `lib.rs` bridge closure at `lib.rs:19526/19541/19556/
19572`, and states the fixes are **inert** until those are deleted.

**They have been deleted.** A repo-wide grep for the literal
`"java/lang/Character"` across `native-builtins/src` and `vm/src` (including the
`let ch = "java/lang/Character"` indirection `deprecated_util.rs` uses) finds
registrations in exactly two files: `lang_math.rs` (31 triples) and
`deprecated_util.rs` (3 — `isJavaLetter`, `isJavaLetterOrDigit`, `isSpace`,
disjoint from the 31). `lib.rs` has **no** `java/lang/Character` registration
left. **W7-98's fixes are live**, which is what §3 then measures.

---

## 2. `case_map.rs` — the version-skew table was six code points and the measured set is fifty-six

**MEASURED, both sides, whole domain. This is the largest defect this lane
found, and it is in this lane's own file.**

`JDK_UNMAPPED_CASE_CODE_POINTS` was `[u16; 6]` behind a `cp <= 0xFFFF` guard.
`W7-95a` derived it by dumping the JDK's answers for all 65,536 BMP code units
and diffing them against **Python's** `str.upper()`/`lower()` as a stand-in for
Rust's, then noted, correctly, that the Rust side had never been validated *as a
proxy*.

Executing Rust's side directly — `rustc 1.97.1`, the toolchain this crate builds
with — over all **1,112,064** Unicode scalar values against
`String.to{Upper,Lower}Case(Locale.ROOT)` on OpenJDK 25.0.3+9, and classifying
every disagreement:

```text
   Rust has a mapping, JDK 25 has NONE   56
   JDK has a mapping, Rust has none       0
   both map, to DIFFERENT things          0
```

The 56, as measured runs:

| run | count | JDK 25 `getType` | note |
|---|---|---|---|
| `U+A7CE..U+A7CF` | 2 | 0 (unassigned) | the known ones |
| `U+A7D2..U+A7D5` | 4 | 0, except `A7D3`/`A7D5` = 2 | the known ones |
| `U+16EA0..U+16EB8` | **25** | 0 (unassigned) | **new** |
| `U+16EBB..U+16ED3` | **25** | 0 (unassigned) | **new** |

`U+16EA0..` and `U+16EBB..` are a case-pair block (`+0x1B`) that Rust's Unicode
knows and JDK 25 does not assign at all — the whole span `U+16E9B..U+16EDF`
measures `getType == 0`, `isDefined == false`. Exactly the `A7Cx` shape, one
plane up, and **structurally invisible to the old table**: `[u16; 6]` cannot
hold a supplementary code point and the `cp <= 0xFFFF` guard would have rejected
it anyway. A sweep of the BMP could never have found them, and the record that
described its own sweep as exhaustive was exhaustive over the wrong domain for
the second time.

Observable through the ordinary API, measured, printed as code units:

```text
   "\u{16EBB}".toUpperCase()    HotSpot [55323, 57019] (identity)   was [55323, 56992]
   "\u{16EA0}".toLowerCase()    HotSpot [55323, 56992] (identity)   was [55323, 57019]
```

**Fixed.** `JDK_UNMAPPED_CASE_RUNS` / `JDK_UNASSIGNED_CASE_RUNS` are
`[(u32, u32); 4]` run tables over every plane;
`is_jdk_unmapped_case_code_point` takes the full `u32` domain. The runs are
measured boundaries, not a bracket: `U+A7D0`/`U+A7D1` and `U+A7D6`/`U+A7D7` are
real JDK case pairs inside the same region, and `U+16EB9`/`U+16EBA` split the
supplementary block in two.

**Measured after, on a `rustc` build of the edited file:**

| sweep | before | after |
|---|---|---|
| `String.toUpperCase(Locale.ROOT)`, 1,112,064 scalar values | 25 | **0** |
| `String.toLowerCase(Locale.ROOT)`, same | 25 | **0** |
| `String.toUpperCase(tr)` / `toLowerCase(tr)`, same | 0 | **0** |
| `String.toUpperCase(lt)` / `toLowerCase(lt)`, same | 0 | **0** |

### 2b. The character-wise arm was silently dropping Final_Sigma

Found while widening the trigger set, and it is why the table growing from 6 to
56 mattered more than the 50 wrong answers themselves.

`jdk_to_lowercase` has two arms: a bulk `str::to_lowercase` when no skewed code
point is present, and a character-wise `char::to_lowercase` loop when one is.
`str::to_lowercase` implements Final_Sigma; `char::to_lowercase` **cannot**,
because a lone `char` has no context. So any string that merely *contained* a
skewed code point lost the final sigma everywhere else in it. MEASURED:

```text
   ("A" + U+03A3 + U+A7CE).toLowerCase()   HotSpot [97, 962, 42958]
                                           was     [97, 963, 42958]
```

`962` is `U+03C2` FINAL SIGMA, `963` the medial `U+03C3`. Invisible while the
trigger set was six code points nobody types; extending the table to 56 would
have widened it.

**Fixed**: the character-wise arm now routes `U+03A3` through `is_final_cased`,
the JDK's own `ConditionalSpecialCasing` rule already in this file for the
`tr`/`az`/`lt` path — so the two arms finally answer the same way.

### 2c. The residual, stated: 205 code points, and the fix that made it WORSE

`("A" + U+03A3 + X).toLowerCase()` over all 1,112,064 scalar values:

| | mismatches vs HotSpot |
|---|---|
| before | 259 |
| after §2 + §2b | **205** — 54 fixed, **0 newly broken** |

The 205 are one root cause with two faces, and it is not the version skew:
**the JDK's `ConditionalSpecialCasing.isCased` is a frozen hardcoded list and
Rust consults the live `Cased` property, and Rust additionally skips
`Case_Ignorable` characters where the JDK stops at a `BreakIterator` word
boundary.** Enumerated:

```text
U+00AA, U+00BA, U+0295, U+02B0..U+02B8, U+02C0..U+02C1, U+02E0..U+02E4,
U+0345, U+037A, U+1D2C..U+1D61, U+24B6..U+24E9,
U+1F130..U+1F149, U+1F150..U+1F169, U+1F170..U+1F189
```

**The obvious fix was measured and rejected.** Routing *every* string containing
`U+03A3` through the character-wise arm — so `is_cased`/`is_final_cased` decide
every sigma rather than only the ones next to a skewed code point — was built
and swept:

```text
   current fix                       205 mismatches
   sigma also routes char-wise       329 mismatches
      would fix    6 runs   (U+02B0..U+02B8, U+02C0..U+02C1, U+02E0..U+02E4,
                             U+0345, U+037A, U+1D2C..U+1D61)
      would break 19 runs   (U+10FC, U+1D62..U+1D6A, U+1D78, U+1D9B..U+1DBF,
                             U+2071, U+207F, U+2090..U+209C, U+2C7C..U+2C7D,
                             U+A69C..U+A69D, U+A770, U+A7F1..U+A7F4,
                             U+A7F8..U+A7F9, U+AB5C..U+AB5F, U+AB69,
                             U+10780, U+10783..U+10785, U+10787..U+107B0,
                             U+107B2..U+107BA, U+1E030..U+1E06D)
```

`is_cased` is Rust-derived and therefore wrong in a *different* set of places
than `str::to_lowercase` is; swapping one for the other trades 6 runs for 19.
This is the handoff's "do not generalise a contract from three rows" with a
number attached. **The complete fix is to transcribe the JDK's frozen
`isCased` list** — the enumeration above is what it disagrees with — and that is
a `case_map.rs` change a later lane can make from this measurement without
running anything. Left open deliberately; it is class (c) at the far edge of
reachability (it needs a capital sigma adjacent to one of 205 code points).

Two further property divergences the mapping tables do not cover, MEASURED and
recorded so the next lane does not rediscover them: `char::is_lowercase` vs
`Character.isLowerCase` disagrees at **U+0295** (JDK says lowercase, Rust does
not) and **U+A7F1** (Rust says lowercase, JDK does not), on top of the 26 that
are in the skew runs.

---

## 3. `java.lang.Character` — 17 registered methods, the full domain, zero mismatches

**MEASURED.** `JAVA_LETTER_RUNS`, `JAVA_UPPERCASE_RUNS`, `JAVA_LOWERCASE_RUNS`,
`JAVA_TO_UPPER_RUNS`, `JAVA_TO_LOWER_RUNS`, `JAVA_DIGIT_RUNS`,
`JAVA_SUPPLEMENTARY_DIGIT_RUNS`, `JAVA_NUMERIC_VALUE_RUNS`,
`JAVA_NUMERIC_VALUE_NEG2_RUNS` and the six predicates over them were extracted
verbatim from `lang_math.rs` by script, compiled standalone, and run against
HotSpot 25.0.3+9 over the whole domain. The transliteration includes the
argument decoding (`*v as u32` and friends), not just the table lookup.

`(I)` overloads, all **1,114,112** code points:

| method | mismatches |
|---|---|
| `isDigit`, `isLetter`, `isLetterOrDigit`, `isWhitespace` | **0** each |
| `isUpperCase`, `isLowerCase` | **0** each |
| `toUpperCase(I)I`, `toLowerCase(I)I` | **0** each |
| `charCount`, `isBmpCodePoint`, `isValidCodePoint`, `isISOControl` | **0** each |

`(C)` overloads, all **65,536** `char` values:

| method | mismatches |
|---|---|
| `isDigit`, `isLetter`, `isLetterOrDigit`, `isWhitespace` | **0** each |
| `isUpperCase`, `isLowerCase`, `toUpperCase(C)C`, `toLowerCase(C)C` | **0** each |
| `getNumericValue(C)I` | **0** |
| `isHighSurrogate`, `isLowSurrogate` | **0** each |
| `digit(c,10)`, `digit(c,36)`, `digit(c,16)`, `digit(c,2)` | **0** each |

`forDigit(d, r)` over the whole grid `d, r ∈ -3..=40` (1,936 cells) plus
`digit(c, r)` for `r ∈ {MIN_VALUE, -1, 0, 1, 37, 40, MAX_VALUE}` and
`charCount`/`isBmpCodePoint`/`isValidCodePoint`/`isISOControl`/`toUpperCase`/
`toLowerCase` over the 11 extreme `int`s: **0 mismatches, 1,989 rows.**

**W7-98 is closed by this**, and so is E7-1's code-point contract table. The ten
methods W7-98 measured at 3 to 2,153 divergent BMP code points each are at zero
over a domain seventeen times larger.

### The one non-zero row, and why it is not a defect

`getNumericValue` over the supplementary planes: **1,177 mismatches** (e.g.
`U+10107` AEGEAN NUMBER ONE, HotSpot `1`, table `-1`). `JAVA_NUMERIC_VALUE_RUNS`
is BMP-only by design and **`getNumericValue` is registered for `(C)I` only** —
a `char` argument cannot exceed `U+FFFF`, and `getNumericValue(int)` is
unregistered and runs real `CharacterData` bytecode. So the 1,177 are
unreachable.

**They are a loaded gun, not a defect.** Registering `getNumericValue(I)I` onto
`native_character_get_numeric_value` — a one-line change that would look like
completing a family — turns 1,177 code points wrong instantly. This is the exact
shape of E17-1's `Character.digit(int,int)` decision, on a second method E17-1
did not name, and it is now measured rather than argued. **Do not register it.**

---

## 4. `intrinsics/record.rs` — three measured defects in the generated record bodies

`record.rs` is not a delegate: a record's `hashCode`/`equals` is an
`invokedynamic` against `java.lang.runtime.ObjectMethods.bootstrap` that this VM
services from Rust, and this module is the single body for both the interpreter
intrinsic and the `invokedynamic` executor. So there is no second implementation
to diff against — only the oracle. `RecProbe.java` on OpenJDK 25.0.3+9,
**MEASURED**:

| call | HotSpot | CratonVM (before) |
|---|---|---|
| `record Z(boolean b)`; `new Z(false).hashCode()` | `1237` | **`0`** |
| `new Z(true).hashCode()` | `1231` | **`1`** |
| `record F(float v)`; `new F(intBitsToFloat(0x7F800001)).hashCode()` | `2143289344` | **`2139095041`** |
| `record D(double v)`; `new D(longBitsToDouble(0x7FF0000000000001L)).hashCode()` | `2146959360` | **`2146435073`** |
| `new F(intBitsToFloat(0x7F800001)).equals(new F(Float.NaN))` | `true` | **`false`** |

Three rules, all class (c) wrong value:

* **`Float.hashCode` is `floatToIntBits`, not `floatToRawIntBits`.** It collapses
  every NaN to `0x7FC00000`; `f32::to_bits` keeps the payload. Same for
  `double`, canonical `0x7FF8000000000000`. Reachable from ordinary bytecode
  through `Float.intBitsToFloat` / `Double.longBitsToDouble`.
* **The generated `equals` compares components with `Float.compare(a,b) == 0`,
  which also canonicalises.** So two NaNs with *different payloads* are equal
  components on HotSpot and were not here. The raw-bit form got the
  `+0.0 != -0.0` rule right and the NaN rule only half right — measured
  controls: `F(0.0f).equals(F(-0.0f))` is `false` on both, `F(NaN).equals(F(NaN))`
  is `true` on both.
* **A `boolean` component hashes `1231`/`1237`, not `1`/`0`.** `Value::Int`
  carries `boolean`, `byte`, `char`, `short` and `int` alike, and the wrapper
  hash agrees for four of the five — `Byte`/`Short`/`Character`/`Integer
  .hashCode` are all the value itself. `Boolean.hashCode` is not.

**Fixed**, all three. The boolean fix needs the declared type, which is not on
the value; `NativeContext::record_components` supplies it but takes the
class-manager read lock and clones a `String` pair per component, which would
destroy the reason this intrinsic exists (it replaced a ~3 µs `invokedynamic`
path that made a Hibernate flush plan hang). So it is memoised per `ClassId` and
**consulted only when a component reads back as `Int(0)` or `Int(1)`** — the
only two values at which `Boolean.hashCode` and `Integer.hashCode` can differ,
and a `boolean` field cannot hold any other. Ordinary components never touch it.

**Blast radius, stated because it is smaller than it looks and larger than it
looks in different directions.** A wrong-but-self-consistent hash does not break
a `HashMap` *within one process*, so this is not a container-corruption bug. It
does break (a) any expectation that a record hashes the same here as on HotSpot,
and (b) any run where some calls take this body and others take a real
`java.lang.runtime.ObjectMethods` chain — which in `--jdk-only` mode is a real
class. That second one is a genuine hazard and is in §9.

---

## 5. The `Math` intrinsics, and `Math.pow`'s fast path

`intrinsics/math.rs` is the one group in the table that does not delegate — it
computes inline, on the stated grounds that "the inline arithmetic IS the real
implementation". **SOURCE-VERIFIED**: every `compute_*` is textually the same
operation as its `lang_math::native_math_*` counterpart
(`wrapping_abs`, `std::cmp::{min,max}`, `f64::{abs,sqrt}`), and `long_arg`'s
`Double`-tagged-`Long` recovery is reproduced.

**MEASURED**: `f64::sqrt` and `f64::abs` on this toolchain against `Math.sqrt`
and `Math.abs` on HotSpot 25.0.3+9 over 200,000 pseudorandom `double` bit
patterns plus 14 hand-picked specials, and `wrapping_abs`/`std::cmp` against
`Math.abs`/`min`/`max` over the 8×8 `int` and `long` extreme grids:
**200,158 rows, byte-identical, zero divergences** — including NaN payloads,
which `is_nan()`-style assertions cannot see:

```text
   Math.abs(longBitsToDouble(0xFFF0000000000001))   7FF0000000000001   both
   Math.sqrt(longBitsToDouble(0x7FF0000000000001))  7FF8000000000001   both
   Math.sqrt(-0.0)                                  8000000000000000   both
```

A regression test pinning those bit patterns is added — the file previously
asserted only `.is_nan()`, which a payload change walks straight past.

**`Math.pow`'s `a.powi(b as i32)` fast path is GONE.** SOURCE-VERIFIED on the
merged tree: `native_math_pow` (`lang_math.rs:1878`) contains the *comment*
describing the old path and the ulp table that condemned it, and the only
surviving specialisation is `b == 2.0 → a * a`, which is a single correctly
rounded multiply. A repo-wide grep for `.powi(` finds two live uses, neither in
`pow`: a test expectation at `lang_math.rs:7427` and a subnormal rescale at
`lang_string.rs:9930`. **The merge lane's report is correct; do not repeat the
claim that it is still there.**

Per the handoff: **`Math.ulp` was not touched and `Math.floorDiv`/`floorMod` at
`MIN_VALUE / -1` was not touched.**

---

## 6. `intrinsics/mod.rs` — a prefilter whose doc comment was false

`might_have_method_descriptor` is documented as: *"A `false` result is
definitive: no intrinsic entry has this `(method_name, descriptor)` pair on any
class."*

**SOURCE-VERIFIED: that is false.** `lookup` resolves 30 triples; the prefilter's
list omits two of them — `Thread.onSpinWait ()V` and `Thread.currentThread
()Ljava/lang/Thread;`. So the function answers `false` for two live intrinsics.

It is **sound today**: the only caller is
`vm/src/runtime/interpreter/dispatch_virtual.rs` (three sites), both omissions
are `invokestatic` targets, and the static path does not consult it. But a
future caller on the static path would have lost both fast paths silently, with
a green build — the handoff's trap in its purest form, sitting in a doc comment
that invited exactly that.

**Fixed by narrowing the claim to what is true and pinning it with tests**, not
by adding the two entries (adding them would be a behaviour change to a hot
virtual path for no benefit). Four new tests over a table that is a deliberate
second copy of `lookup`'s arms:

* `staticness_agrees_with_the_table` — `is_static` must agree with `lookup` for
  all 30; the interpreter picks its argument-pop helper from it, so a wrong
  answer reads the receiver as `param0`.
* `every_instance_entry_is_admitted` — the invariant that IS true: the prefilter
  may drop a static entry, never an instance one.
* `the_prefilter_blind_spot_is_exactly_the_two_thread_statics` — so a third
  omission fails a test instead of being absorbed.
* `the_table_is_complete` — 30 arms; `RecordHashCode`/`RecordEquals` are
  deliberately not in `lookup` (they are produced per class by
  `dispatch_static::record_object_intrinsic`).

### The intrinsic table is a second registrar, and it was audited as one

The whole point of the handoff's `--dump-native-registry` discipline is that a
triple can be served by a body other than the one you read.
`intrinsics::lookup` is exactly such a second registrar, one layer above the
native registry: at IC-fill time the interpreter caches `callback_for(kind)` and
the native registry is never consulted again for that site. So every one of the
30 triples was matched against what the registry binds, by script over every
`.rs` in `native-builtins/src` and `vm/src`, including the parameterised
registrars (`register_math_natives(registry, class)`,
`register_string_builder_natives(registry, class)`) that a literal-string grep
misses. **SOURCE-VERIFIED result: all 30 agree.** Notes worth keeping:

* `String.charAt (I)C`, `Integer.parseInt`, `Long.parseLong` are each registered
  **twice**, at different sites, to the **same** function. Harmless, and now
  known rather than assumed.
* `Thread.onSpinWait ()V` has **no** registry entry at all — the intrinsic is
  the only implementation. It is a no-op plus `std::hint::spin_loop`, which is
  what the method is.
* `StringBuilder.appendCodePoint (I)L…;` is registered twice **to two different
  functions** inside `register_string_builder_natives` itself
  (`native_sb_append_codepoint`, then `native_sb_append_code_point`,
  last-write-wins). Not an intrinsic triple, already documented at
  `lang_string.rs:2753`, restated here because a scripted audit found it and a
  reader would not.

---

## 7. What this lane did NOT do

* **Nothing was run on a CratonVM binary.** Not one row of this record is a
  CratonVM measurement. Every "after" is either the oracle, or a standalone
  `rustc` build of the edited Rust, or both compared — and a standalone build of
  a lifted function is not the VM either: it proves the *algorithm* agrees, not
  that the VM reaches it.
* **No `cargo build`, `cargo check` or `cargo test` was run**, per the lane
  brief. The edited files were type-checked and their unit tests executed by
  building them standalone with `rustc --test` and a shim for the one external
  dependency (`unicode_normalization::char::canonical_combining_class`, stood in
  by a table generated from Python's UCD 16.0.0). **That shim is itself a proxy
  and this record will not repeat W7-95a's mistake of not saying so**: it affects
  only `in_word`/`scan_back`/`is_more_above`/`is_before_dot`, i.e. the
  `tr`/`az`/`lt` context conditions, and the `tr`/`lt` sweeps came back 0/0
  against the oracle, which is the best evidence available that it is faithful
  over the reachable domain. It is not proof.
* **The 205-code-point Final_Sigma residual was measured, not fixed** (§2c),
  with the rejected alternative measured too.
* **`getNumericValue(I)I` was not registered** and the 1,177 supplementary rows
  were not repaired — deliberately, §3.
* **`Character.digit(int,int)` was not registered.** E17-1's decision stands and
  §3 is independent evidence for it.
* **`lang_string.rs` and `lang_math.rs` were not edited** — not this lane's
  files. Everything wanted there is in §8.
* **No `regression-suite/` file was created or run**, per the brief. §10 has the
  vector as a code block.
* **The `Object.hashCode` / `System.arraycopy` / `String.charAt` intrinsics were
  audited for *routing*, not swept for *semantics*** — they are pure delegates to
  `lang_*` bodies this lane does not own, and a semantic sweep of them belongs
  with the file's owner.
* **`W8-C3-1`'s 614-triple arithmetic and `E10-1`'s 292 were not re-derived.**
  They need a registry dump.

---

## 8. NOMINATIONS

**N1 — `Character.getNumericValue(I)I` must stay unregistered, and the reason
should be next to the body.** Same shape as E17-1's `digit(int,int)` decision,
measured here at 1,177 code points.

*File:* `native-builtins/src/lang_math.rs`, the doc comment on
`native_character_get_numeric_value` (≈:6114). Its last paragraph currently says
*"BMP is the whole domain: only `(C)I` is registered, so no argument can exceed
`U+FFFF`."* — true, and it does not say what happens if someone changes that.
*Add:* `G9-1 measured it: `JAVA_NUMERIC_VALUE_RUNS` disagrees with HotSpot 25 on
**1,177** supplementary code points (first `U+10107` AEGEAN NUMBER ONE, HotSpot
1, here -1). Registering `(I)I` onto this body turns all 1,177 wrong on the same
day. Do not. Same decision as E17-1 §1 for `digit(int,int)`.`

**N2 — the JDK's `isCased` is a frozen list; transcribe it.** Closes §2c's 205.

*File:* `native-builtins/src/case_map.rs` — **this lane's own file**, so this is
a follow-up rather than a boundary crossing, but it is stated as a nomination
because it wants the JDK source (`java.lang.ConditionalSpecialCasing.isCased`,
which `C:\craton\jdk25src` would have given and which is **ABSENT** on this
host). The enumeration in §2c is what a transcription has to reproduce; the
measured trap is that swapping wholesale to `is_cased` costs more than it buys
(205 → 329).

**N3 — give `record.rs` the component kinds without a lock.** The boolean fix in
§4 memoises `record_components` in a `RwLock<HashMap<u32, Box<[bool]>>>` inside
`record.rs`, consulted only for `Int(0|1)`. The right home is the memo the VM
already computes per class.

*File:* `native-api/src/registry.rs`, ≈:1043 — `object_method_fast_path` returns
`(u8 bits, first_field_index, num_record_components)`.
*Change:* widen it (or add a sibling `record_boolean_components(class_id) -> u64`)
so bit *i* says component *i* is declared `Z`.
*File:* `vm/src/vm/vm_exec.rs`, the `object_method_fast_path` impl (≈:7995) —
`class.record_components` is already in hand there; compute the mask once.
*Then:* `record.rs` drops `BOOLEAN_COMPONENTS`, `is_boolean_component` and the
`std::sync` imports.

**N4 — `String.chars()`/`codePoints()` still cannot see a lone surrogate from
the VM layer.** `W7-95a` N3, restated because it is still true:
`vm/src/vm/vm_object.rs:758`'s `read_java_string_units` (the documented lossless
twin of `read_java_string`) has one caller in the tree
(`vm/src/runtime/invokedynamic.rs:1918`) and is **not on the `NativeContext`
trait**, so no native can reach it. `lang_string.rs` works around this with its
own `read_string_chars`. *File:* `native-api/src/registry.rs` — add
`fn read_string_units(&self, obj: ObjectRef) -> Option<Vec<u16>>`.

**N5 — three stale prose references to a renamed constant.** §2 renamed
`case_map::JDK_UNMAPPED_CASE_CODE_POINTS` to `JDK_UNMAPPED_CASE_RUNS` (the type
changed from `[u16; 6]` to `[(u32, u32); 4]`). The only cross-file *code*
reference is the function `is_jdk_unmapped_case_code_point`, whose signature is
unchanged, so nothing breaks; but three comments now name a constant that does
not exist.

*File:* `native-builtins/src/lang_string.rs`
*lines:* ≈5171, ≈6665, ≈11101, ≈11132, ≈11141, ≈11177, ≈11207
*exact literal old text:* `JDK_UNMAPPED_CASE_CODE_POINTS`
*exact literal new text:* `JDK_UNMAPPED_CASE_RUNS`

**N6 — `lang_string.rs`'s per-char case helpers are BMP-shaped and the skew is
not.** `java_char_to_upper_case` / `java_char_to_lower_case`
(`lang_string.rs:11177`, `:11207`) call `is_jdk_unmapped_case_code_point(u32::
from(ch))`. That call is now correct over every plane for free — but only if
`ch` is a `char` rather than a `u16`. *Ask:* the owner should confirm the
parameter type, and if it is `u16`, say in the doc that the supplementary half
of the skew is handled by the String-level path only. **PREDICTED, not read
closely — this is a request to check, not a defect claim.**

---

## 9. What the orchestrator must check at build time

Ordered by how badly a wrong answer would mislead.

1. **Prove the fixes DID something rather than merely compiled.** A green build
   proves nothing here; three of the four fixes are invisible to any test
   written over ordinary text. The cheapest positive controls, each a one-liner
   whose *before* answer is recorded above:

   ```bash
   ./target/release/cratonvm.exe --java-home "$JAVA_HOME" --jdk-only \
       -cp regression-suite/build RJdkG9Skew
   ```
   with §10's vector. Row for row, the `before` column of §2 and §4 is what a
   binary built *without* these changes prints, so a run that reproduces the
   old values is a fix that did not land.

2. **`--dump-native-registry`, and read three things.** §1 is SOURCE-VERIFIED
   only; `owns_slot` and `invocations` are what turn it into a measurement.
   * `("java/lang/String", "offsetByCodePoints", "(II)I")` and
     `("java/lang/String", "indent", "(I)Ljava/lang/String;")` — expect exactly
     one row each, `registered_by` naming `lang_math.rs`, `owns_slot=true`, and
     `invocations` non-zero on a run that calls them. If `overwrote` is
     non-empty, §1's answer is wrong and the abort rows may be live.
   * `("java/lang/String", "codePoints", "()Ljava/util/stream/IntStream;")` —
     expect `native_string_code_points`, not `native_string_chars`. That is
     `W7-95a` N1 and §1 claims it landed.
   * every `("java/lang/Character", …)` row — expect **31** from `lang_math.rs`
     plus **3** from `deprecated_util.rs`, and **zero** `overwrote` values. If
     any `Character` row reports `overwrote`, `W7-98` is still inert and §3's
     zeros describe a body that does not run.

3. **`Character.getNumericValue` must have exactly one registration and it must
   be `(C)I`.** If a `(I)I` row appears, 1,177 code points are wrong (§3).

4. **The record intrinsic vs the real `ObjectMethods`.** In `--jdk-only` mode
   `java.lang.runtime.ObjectMethods` is a real class. If any path ever
   materialises its `MethodHandle` chain while another takes `record.rs`, the
   two produce *different hashes for the same record* and any `HashMap` keyed by
   one is corrupt across the boundary. §4's fixes narrow the gap to zero for
   `float`/`double`/`boolean`; whether the second path exists at all is a
   question only a run can answer. `vm/tests/intrinsic_diff.rs:557` already
   exercises intrinsics-on vs `CRATONVM_DISABLE_INTRINSICS=1` for the record
   kinds — **run it**, and extend it with a boolean and a NaN component.

5. **`cargo test -p cratonvm-native-builtins`** for the four edited files.
   Standalone `rustc --test` runs are recorded below as passing, but a shim
   stood in for `unicode_normalization` and only the real build settles it:
   * `case_map.rs` — 12 tests, incl. 2 new (`the_supplementary_half_of_the_skew
     _is_identity_without_a_locale`, `the_character_wise_arm_still_finds_the
     _final_sigma`).
   * `intrinsics/record.rs` — 2 new
     (`a_nan_component_hashes_and_compares_by_canonical_bits`,
     `boolean_component_hash_constants`).
   * `intrinsics/mod.rs` — 4 new (§6).
   * `intrinsics/math.rs` — 1 new
     (`nan_payloads_survive_abs_and_sqrt_bit_for_bit`).

6. **Formatting and line endings are already parity-checked.** Each edited file
   has the same `rustfmt --edition 2021 --check` diff count as its `HEAD`
   version (`case_map.rs` 9/9, `record.rs` 4/4, `math.rs` 1/1, `mod.rs` 1/1 —
   all pre-existing), 0 CR bytes, 0 conflict markers, no duplicate `fn` names.

7. **`RJdkIntrinsics3` — what this lane can say about it.** Its `boxid` family
   drives `Character.isLetterOrDigit` at `'a'`, `U+2160` ROMAN NUMERAL ONE,
   `U+00B2` SUPERSCRIPT TWO and `'_'` (lines 712–717). §3 measured that method
   at **0 mismatches over all 65,536 `char` values and all 1,114,112 code
   points**, so **those four checks are not why it is red** — a driver working
   the vector one assertion at a time can skip past them rather than re-derive
   the Unicode tables. It carries no `record` and no case-mapping family, so
   nothing else in this lane bears on it.

---

## 10. The regression vector this lane would add

Not created — the brief forbids writing under `regression-suite/`. Every
expectation below is the **measured** OpenJDK 25.0.3+9 answer, and every
"before" is in §2/§4. Labels are ASCII only (handoff §7: a single em-dash once
failed a differential with every assertion passing).

```java
import java.util.Locale;

/**
 * G9-1: the Unicode version skew above the BMP, the final sigma the
 * character-wise case arm used to drop, and the three record-component rules.
 *
 * Every expectation is HotSpot 25.0.3+9, captured 2026-08-16. Values are
 * printed as UTF-16 code units, never as rendered characters, and every label
 * is ASCII: a non-ASCII label goes through the Windows console code page
 * differently on the two VMs and fails the differential with every assertion
 * passing.
 */
public class RJdkG9Skew {
    static int checks = 0, fails = 0;

    static void ck(String label, String actual, String want) {
        checks++;
        if (!actual.equals(want)) {
            fails++;
            System.out.println("FAIL " + label + " got=" + actual + " want=" + want);
        }
    }

    static String u16(String s) {
        StringBuilder b = new StringBuilder();
        for (int i = 0; i < s.length(); i++) { if (i > 0) b.append(','); b.append((int) s.charAt(i)); }
        return b.toString();
    }

    /** The four measured runs where Rust has a case mapping and JDK 25 has none. */
    static final int[][] SKEW = { {0xA7CE, 0xA7CF}, {0xA7D2, 0xA7D5},
                                  {0x16EA0, 0x16EB8}, {0x16EBB, 0x16ED3} };

    static void skew() {
        int n = 0;
        for (int[] run : SKEW) {
            for (int cp = run[0]; cp <= run[1]; cp++, n++) {
                String s = new String(Character.toChars(cp));
                // Identity in EVERY locale, both directions. Was Rust's pairing
                // for the 50 supplementary members before G9-1.
                for (String tag : new String[] {"und", "tr", "az", "lt", "en", "el"}) {
                    Locale loc = Locale.forLanguageTag(tag);
                    ck("skew.up." + Integer.toHexString(cp) + "." + tag,
                            u16(s.toUpperCase(loc)), u16(s));
                    ck("skew.lo." + Integer.toHexString(cp) + "." + tag,
                            u16(s.toLowerCase(loc)), u16(s));
                }
                // The discriminator a table that PAIRS them answers true to.
                // Rust's partner: XOR 1 inside the A7Cx pairs, +/- 0x1B across
                // the two supplementary runs. A table that pairs them says true.
                int partner = cp < 0x10000 ? (cp ^ 1)
                        : (cp <= 0x16EB8 ? cp + 0x1B : cp - 0x1B);
                ck("skew.eqic." + Integer.toHexString(cp),
                        String.valueOf(s.equalsIgnoreCase(
                                new String(Character.toChars(partner)))), "false");
            }
        }
        ck("skew.count", String.valueOf(n), "56");
        // Controls: real JDK case pairs inside the same spans, and the two
        // code points that split the supplementary block. A range test fails here.
        ck("skew.ctl.a7d1", u16("\uA7D1".toUpperCase(Locale.ROOT)), u16("\uA7D0"));
        ck("skew.ctl.a7d7", u16("\uA7D7".toUpperCase(Locale.ROOT)), u16("\uA7D6"));
        ck("skew.ctl.sharps", u16("\u00DF".toUpperCase(Locale.ROOT)), "83,83");
    }

    static void sigma() {
        // The character-wise arm runs when a skewed code point is present, and
        // char-at-a-time lowercasing has no context. 962 = U+03C2 FINAL SIGMA,
        // 963 = U+03C3 medial. Every row measured.
        ck("sig.unassigned.a7ce", u16("A\u03A3\uA7CE".toLowerCase(Locale.ROOT)), "97,962,42958");
        ck("sig.unassigned.16ea0", u16(("A\u03A3" + new String(Character.toChars(0x16EA0)))
                .toLowerCase(Locale.ROOT)), "97,962,55323,56992");
        // A7D3 IS an assigned lowercase letter on JDK 25, so the sigma stays MEDIAL.
        // This is the control that says the rule is Final_Cased, not "always final".
        ck("sig.assigned.a7d3", u16("A\u03A3\uA7D3".toLowerCase(Locale.ROOT)), "97,963,42963");
        ck("sig.cased.after", u16("\uA7CEA\u03A3A".toLowerCase(Locale.ROOT)), "42958,97,963,97");
        // No skewed code point: the bulk path, which was already right.
        ck("sig.bulk.final", u16("A\u03A3".toLowerCase(Locale.ROOT)), "97,962");
        ck("sig.bulk.pair", u16("\u03A3\u03A3".toLowerCase(Locale.ROOT)), "963,962");
    }

    record Z(boolean b) {}
    record F(float v) {}
    record D(double v) {}
    record Mixed(boolean flag, int n) {}

    static void recs() {
        // Boolean.hashCode, not the int value. Was 0 and 1.
        ck("rec.bool.false", String.valueOf(new Z(false).hashCode()), "1237");
        ck("rec.bool.true", String.valueOf(new Z(true).hashCode()), "1231");
        // 0 and 1 are the ONLY int values at which the two wrappers differ, so
        // an int component holding them must NOT move.
        ck("rec.mixed.f0", String.valueOf(new Mixed(false, 0).hashCode()),
                String.valueOf(1237 * 31));
        ck("rec.mixed.t1", String.valueOf(new Mixed(true, 1).hashCode()),
                String.valueOf(1231 * 31 + 1));
        // floatToIntBits, not floatToRawIntBits: every NaN collapses.
        float oddNan = Float.intBitsToFloat(0x7F800001);
        ck("rec.float.nan.payload", String.valueOf(new F(oddNan).hashCode()), "2143289344");
        ck("rec.float.nan.canon", String.valueOf(new F(Float.NaN).hashCode()), "2143289344");
        double oddDNan = Double.longBitsToDouble(0x7FF0000000000001L);
        ck("rec.double.nan.payload", String.valueOf(new D(oddDNan).hashCode()), "2146959360");
        // Two NaNs with DIFFERENT payloads are equal components. Was false.
        ck("rec.float.nan.equals", String.valueOf(new F(oddNan).equals(new F(Float.NaN))), "true");
        ck("rec.double.nan.equals",
                String.valueOf(new D(oddDNan).equals(new D(Double.NaN))), "true");
        // Controls: the non-NaN rules must not have moved with them.
        ck("rec.float.zeros", String.valueOf(new F(0.0f).equals(new F(-0.0f))), "false");
        ck("rec.double.zeros", String.valueOf(new D(0.0).equals(new D(-0.0))), "false");
        ck("rec.float.negzero.hash", String.valueOf(new F(-0.0f).hashCode()), "-2147483648");
        ck("rec.float.plain", String.valueOf(new F(1.5f).hashCode()), "1069547520");
    }

    static void charfam() {
        // Spot rows from the 1,114,112-code-point sweep, chosen where Rust's
        // tables and the JDK's disagree, so a body that derives from Rust fails.
        ck("chr.isletter.2160", String.valueOf(Character.isLetter(0x2160)), "false");
        ck("chr.isalpha.2160", String.valueOf(Character.isAlphabetic(0x2160)), "true");
        ck("chr.isdigit.0660", String.valueOf(Character.isDigit(0x0660)), "true");
        ck("chr.isdigit.1D7CE", String.valueOf(Character.isDigit(0x1D7CE)), "true");
        ck("chr.isws.001C", String.valueOf(Character.isWhitespace(0x001C)), "true");
        ck("chr.isws.00A0", String.valueOf(Character.isWhitespace(0x00A0)), "false");
        ck("chr.isupper.16EA0", String.valueOf(Character.isUpperCase(0x16EA0)), "false");
        ck("chr.islower.16EBB", String.valueOf(Character.isLowerCase(0x16EBB)), "false");
        ck("chr.islower.0295", String.valueOf(Character.isLowerCase(0x0295)), "true");
        ck("chr.islower.A7F1", String.valueOf(Character.isLowerCase(0xA7F1)), "false");
        ck("chr.toupper.1FB3", String.valueOf(Character.toUpperCase(0x1FB3)), "8124");
        ck("chr.charcount.neg", String.valueOf(Character.charCount(-1)), "1");
        ck("chr.numval.2160", String.valueOf(Character.getNumericValue('\u2160')), "1");
        ck("chr.numval.00BD", String.valueOf(Character.getNumericValue('\u00BD')), "-2");
        // The unregistered (I)I overload runs real CharacterData and must stay right.
        ck("chr.numval.10107.int", String.valueOf(Character.getNumericValue(0x10107)), "1");
        ck("chr.fordigit.oob", String.valueOf((int) Character.forDigit(5, 40)), "0");
        ck("chr.digit.oob", String.valueOf(Character.digit('5', 40)), "-1");
    }

    public static void main(String[] a) {
        skew(); sigma(); recs(); charfam();
        System.out.println("@@RESULT checks=" + checks + " fails=" + fails);
    }
}
```

`chr.numval.10107.int` is the row that would go red the day someone registers
`getNumericValue(I)I` onto the BMP table (§3, N1). It is in the vector precisely
because it passes today for a reason nobody has written down anywhere else.

---

## 11. Records that can now close, and on what evidence

| record | disposition | evidence |
|---|---|---|
| `W7-98-character-unicode` | **CLOSE.** Its ten divergent methods measure 0 over a domain 17x its own, and the deregistrations it was blocked on have landed | §1 (registration audit), §3 (full-domain sweep) |
| `E7-1-character-int-code-point-contracts` | **CLOSE.** Every `(I)` contract it transcribed measures 0, including the extreme-`int` domain | §3 |
| `E17-1-character-digit-int-and-the-fifteen-unregistered` | **KEEP OPEN as a decision record, its decision CONFIRMED.** Independent evidence: the same trap exists on `getNumericValue` and is now measured | §3, N1 |
| `W7-95a-string-code-point-family` | **The VM-ABORT contradiction is CLOSED** (INDEX contradiction 2). The record's other rows still need a binary; its "the Rust side cannot be executed from here" premise is **falsified** | §1, §2 |
| INDEX contradiction 2 | **RESOLVED** — rows 14 and 38 cannot abort; the cells describe a body that no longer exists | §1 |
| INDEX contradiction 1 (`Math.ulp` NaN) | **UNTOUCHED**, per the handoff. Not this lane's to decide | — |
| `W7-95-intrinsic-semantics-census` | still open — its String code-point triples need the binary | — |
| `W8-C3-1`, `E10-1` | still open — their arithmetic needs a registry dump | §7 |

**The durable lesson, which is the same one W7-95a drew and got wrong by one
step.** That record concluded: *derive the table from the JDK, or enumerate it,
but do not compute it from Rust's tables at runtime.* Correct, and it is why §3
is a page of zeros — every `Character` table generated from the oracle is exact.
The step it missed is that a table you cannot *check* against Rust will drift
anyway: `case_map.rs` still derives its case mapping from `char::to_uppercase`,
which is the right call (the JDK has ~1,400 BMP mappings and enumerating them is
not free), and the six-entry exception list guarding that derivation was 12% of
the real number because the Rust half had never been run. **`rustc` is on this
host and takes no lock.** Any claim of the form "Rust's tables say X" in this
directory can be executed, and until this lane none had been.
