# W7-95a — `java/lang/String`'s code-point family read Java text through a Rust `str`

**Status: FIXED in `native-builtins/src/lang_string.rs` and
`native-builtins/src/charset.rs`, EXCEPT the surrogate-PAIR case of
`codePoints()`, whose fix is written but not reachable until one registration
line changes in a file this lane does not own — NOMINATION N1. Not
`--jdk-only`-specific: these natives are registered in both shipping modes, so
every row below reproduces in the default compatibility mode too.**

This is the `java/lang/String` follow-up to
[W7-95](W7-95-intrinsic-semantics-census.md), which measured seven divergent
`String` triples in a differential run against HotSpot and left them as its N6.
W7-95's diagnosis was right and stopped one step short: it said "`U+FFFD`
appearing in a `codePointAt` answer means the value crossed a
UTF-8/scalar-value boundary — find that conversion, it is the bug". The
conversion is `NativeContext::read_string`, every one of the seven bodies
called it, and **the tree already contains the lossless reader they should have
called**, a few hundred lines up in the same file.

## The root cause, in one sentence

`ctx.read_string(obj)` returns a Rust `String`; a Rust `str` cannot represent an
unpaired surrogate; so `String::from_utf16_lossy` substitutes `U+FFFD` — and a
Java `String` is UTF-16 and is *allowed* to contain unpaired surrogates.

**Measured on the shipping binary, not inferred.** The second-generation
intrinsic census fixture `regression-suite/src/RJdkIntrinsics2.java` (448
checks, green on HotSpot 25) fails its `strfmt` family at exactly this line:

```
AssertionError: chars() over an UNPAIRED high surrogate must be 55296, not U+FFFD
```

`55296` is `0xD800`. That check is `"x\ud800y".chars().toArray()[1]`, and
`String.chars()` was `ctx.read_string(this).encode_utf16()`. So the conversion
is demonstrated guilty on at least one member of the family, and every other
caller of it is guilty until shown otherwise rather than the reverse. Reproduce
with `<cratonvm> --jdk-only -cp <out> RJdkIntrinsics2 --only=strfmt`.

**Why this survives ordinary testing: the corruption is SILENT and LOSSY IN
ONE DIRECTION.** A lone surrogate goes in, `U+FFFD` comes out, and no exception
is raised anywhere on the path — not by the native, not by the VM, not by the
caller. A program that round-trips a `String` through one of these bodies gets
a *different* `String` back with no indication that anything happened, and it
cannot get the original back afterwards because `U+FFFD` is a perfectly
ordinary character that no later step has any reason to question. That is
strictly worse than a throw: a throw stops the program at the defect. And no
test written over well-formed text can see it at all, which is why a family of
seven triples carried it into a shipping binary — every input anyone had tried
was text a Rust `str` could hold.

`lang_string.rs`'s own `read_string_chars` (via `decode_string_chars`, which
handles all three storage layouts: legacy `char[]`, compact LATIN-1 `byte[]`,
compact UTF-16 `byte[]`) decodes the receiver's `value` array straight to
`Vec<u16>` and never constructs a `str`. It had **12** callers in this file
before this change — `charAt`, `substring`, `indexOf`, `contains`,
`startsWith`, `compareTo` and friends — and **none of the seven methods whose
entire subject is code points**. That is the `[1 of 10 callsites]` shape again:
the correct helper existed, its doc comment said exactly what it was for, and
the family that needed it most used the other one.

The VM has the same pair one layer down, with the same asymmetry:
`vm/src/vm/vm_object.rs:758`'s `read_java_string_units` is documented as the
"lossless twin" of `read_java_string` — "Use this whenever the destination is
another Java `String` rather than Rust text" — and has exactly **one** caller
in the tree, `vm/src/runtime/invokedynamic.rs:1918`. It is not on the
`NativeContext` trait at all, which is why no native can reach it. See N3.

Two further root causes, because a fix that addressed only surrogates would
have left rows 9–11, 17–20 and 22–37 below still red:

* **Java's whitespace table is not Unicode's.** `str::trim` and
  `char::is_whitespace` are Unicode `White_Space`; `Character.isWhitespace`
  *excludes* every non-breaking space and *includes* `U+001C`..`U+001F`.
  `isBlank`, `strip`, `stripLeading` and `stripTrailing` were all `str::trim`.
  `String.trim()` is a **third** rule — code units `<= U+0020`, a raw
  comparison that predates Unicode-aware whitespace in Java — and was also
  `str::trim`.
* **Java's case mapping is 1:1; Rust's is the full one.** `char::to_uppercase`
  yields the SpecialCasing expansion (sharp s becomes `"SS"`), while
  `Character.toUpperCase(char)` returns the input unchanged when the mapping
  does not fit in one `char`. `regionMatches(true, ...)` compared the two
  iterators.
* **Rust's Unicode tables are NEWER than the JDK's**, which is not a Unicode
  subtlety but a version-skew hazard — and it is the one that got past a
  measurement I had called exhaustive. See
  [the version-skew section](#the-hazard-that-got-past-an-exhaustive-sweep).

## Bounds: three methods returned a number where HotSpot throws — and one aborted the VM

`offsetByCodePoints` cast `index` straight to `usize`, so any `index >
length()` survived; the backward branch then did `pos -= 1` followed by an
unguarded `chars[pos]`. `"a<U+1F600>b".offsetByCodePoints(10, -1)` indexes a
4-element `Vec` at 9.

**A Rust panic is not a Java throwable — it terminates the VM.** One line of
ordinary application bytecode, no flags, default compatibility mode. Same
function, second panic: `for _ in 0..(-code_point_offset)` negates
`Integer.MIN_VALUE`, which overflows `i32` and panics under any
debug-assertions profile.

That outranks every wrong answer in the table below, and is why this is its own
record rather than a line in W7-95.

**A third panic, found by auditing rather than by the census.** After the
orchestrator asked for a guarantee that no path this change touches can abort,
`native_string_indent` was read for the same shape and has it:

```rust
let remove = (-n) as usize;                               // panics on Integer.MIN_VALUE
let spaces = line.len() - line.trim_start().len();        // BYTES, Rust's whitespace table
let skip = remove.min(spaces);
result.push_str(&line[skip..]);                           // slices a str at an arbitrary byte
```

`indent(-1)` of the two-character string `[00A0, 0061]`: `U+00A0` is two UTF-8
bytes, so `trim_start` reports `spaces == 2`, `-n` is 1, and `&line[1..]`
slices inside the character —
**"byte index 1 is not a char boundary" is a panic, and a panic kills the VM.**
Java's rule is `s.substring(Math.min(-n, s.indexOfNonWhitespace()))`: a count
of CHARACTERS over `Character.isWhitespace`, which does not even remove a
leading `U+00A0` (measured: `indent(-1)` of `[00A0, 0061]` is
`[160, 97, 10]`). Every multi-byte leading whitespace character is a way in.
Fixed here, and pinned by the `ind.*` rows of the vector.

## Measured before / predicted after

**HotSpot column: measured.** Microsoft OpenJDK 25.0.3+9, this host, the
committed vector `regression-suite/src/RJdkStringCodePoints.java`, 186 checks,
`@@RESULT checks=186 fails=0`. No expectation in that file was written from
memory: the whole file was run against HotSpot first, and the rows I had
predicted wrong — `repeat`'s check ORDER, and the exact exception classes and
message texts, which differ between two neighbouring methods — were corrected
from the transcript rather than from recall.

**CratonVM-before column.** Rows marked *(W7-95)* come from that record's real
differential run. Rows marked *(read)* are derived from the old body by reading
it: this lane could not execute the binary (a build was in flight), and a body
read is not a measurement. They are listed so a reviewer can check them against
the diff, not because they were observed.

**CratonVM-after column: PREDICTED, every single row.** This lane may not run
the VM. `RJdkStringCodePoints` is the instrument that turns these predictions
into measurements; it is green on HotSpot and has never been run on CratonVM.

| # | call | HotSpot (measured) | CratonVM before | after (PREDICTED) |
|---|------|--------------------|-----------------|-------------------|
| 1 | `"a<U+1F600>b".codePoints()` | `[97,128512,98]` | `[97,55357,56832,98]` *(W7-95)* | `[97,128512,98]` **only with N1** — the surrogate-PAIR case is the one thing `chars()` and `codePoints()` disagree about, so it is the one row N1 gates |
| 2 | `"x" U+D800 "y" .codePoints()` | `[120,55296,121]` | `[120,65533,121]` *(W7-95)* | `[120,55296,121]` — fixed WITHOUT N1: `codePoints` still lands on `native_string_chars`, and that body is now lossless |
| 3 | `U+DC00 "a" .codePoints()` | `[56320,97]` | `[65533,97]` *(W7-95)* | `[56320,97]` — same reason as #2 |
| 4 | `"x" U+D800 "y" .chars()` | `[120,55296,121]` | `[120,65533,121]` — **MEASURED on the shipping binary**: `RJdkIntrinsics2` `strfmt`, "chars() over an UNPAIRED high surrogate must be 55296, not U+FFFD" | `[120,55296,121]` |
| 5 | `"x" U+D800 "y" .codePointAt(1)` | `55296` | `65533` *(W7-95)* | `55296` |
| 6 | `"a" U+D800 .codePointAt(1)` | `55296` | `65533` *(W7-95)* | `55296` |
| 7 | `"a<U+1F600>b".codePointAt(1)` | `128512` | `128512` *(read — already right)* | `128512` |
| 8 | `"a<U+1F600>b".codePointAt(4)` | `StringIndexOutOfBounds: Index 4 out of bounds for length 4` | same *(read — already right)* | same |
| 9 | `"a<U+1F600>b".codePointCount(3,1)` | `IndexOutOfBounds: Range [3, 1) out of bounds for length 4` | no throw, `0` *(W7-95)* | `IOOBE`, same text |
| 10 | `"a<U+1F600>b".codePointCount(0,9)` | `IndexOutOfBounds: Range [0, 9) ...` | no throw, `3` *(read)* | `IOOBE`, same text |
| 11 | `"a<U+1F600>b".codePointCount(-1,2)` | `IndexOutOfBounds: Range [-1, 2) ...` | no throw, `0` *(read)* | `IOOBE`, same text |
| 12 | `"a<U+1F600>b".offsetByCodePoints(0,9)` | `IndexOutOfBounds`, **null** message | no throw, `4` *(W7-95)* | `IOOBE`, null message |
| 13 | `"a<U+1F600>b".offsetByCodePoints(0,-1)` | `IndexOutOfBounds` | no throw, `0` *(read)* | `IOOBE` |
| 14 | `"a<U+1F600>b".offsetByCodePoints(10,-1)` | `IndexOutOfBounds` | **VM ABORT** — panic, `chars[9]` on a 4-element vec *(read)* | `IOOBE` |
| 15 | `"abc".offsetByCodePoints(0, Integer.MIN_VALUE)` | `IndexOutOfBounds` | panic under debug-assertions; wraps to `0` in release *(read)* | `IOOBE` |
| 16 | `"a<U+1F600>b".offsetByCodePoints(4,-2)` | `1` | `1` *(read — already right)* | `1` |
| 17 | `"ab".repeat(-1)` | `IllegalArgumentException: count is negative: -1` | no throw, `""` *(W7-95)* | `IAE`, same text |
| 18 | `"".repeat(-1)` | `IllegalArgumentException` — count is checked FIRST, before the empty-string short circuit | no throw, `""` *(read)* | `IAE` |
| 19 | `"ab".repeat(Integer.MAX_VALUE)` | `OutOfMemoryError: Required length exceeds implementation limit` | multi-GB `str::repeat`, capacity-overflow abort *(read)* | `OOME`, same text |
| 20 | `"abc".repeat(1000000000)` | `OutOfMemoryError`, same text | as #19 *(read)* | `OOME`, same text |
| 21 | `"ab".repeat(3)` | `"ababab"` | `"ababab"` *(read — already right)* | `"ababab"` |
| 22 | `isBlank()` of `[00A0]` NBSP | `false` | `true` *(W7-95)* | `false` |
| 23 | `isBlank()` of `[0085]` NEL | `false` | `true` *(read)* | `false` |
| 24 | `isBlank()` of `[2007]` FIGURE SPACE and `[202F]` NARROW NBSP | `false` | `true` *(read)* | `false` |
| 25 | `isBlank()` of `[001C]`..`[001F]` file/group/record/unit separators | `true` | `false` *(read)* | `true` |
| 26 | `isBlank()` of `[2028]` LINE SEPARATOR (negative control) | `true` | `true` *(read)* | `true` |
| 27 | `"ABC".regionMatches(0,"abc",0,-1)` | `true` | `false` *(W7-95)* | `true` |
| 28 | `"ABC".regionMatches(0,"abc",0,Integer.MIN_VALUE)` | `true` | `false` *(read)* | `true` |
| 29 | `"ABC".regionMatches(Integer.MAX_VALUE,"abc",0,1)` | `false` | `false` *(read — already right)* | `false` |
| 30 | `[0130].regionMatches(true,0,"i",0,1)` | `true` | `false` *(read)* | `true` |
| 31 | `[00DF].regionMatches(true,0,"S",0,1)` | `false` | `false` *(read — already right)* | `false` |
| 32 | `[D800].regionMatches(true,0,[DC00],0,1)` | `false` | `true` — both sides read back as U+FFFD *(read)* | `false` |
| 33 | `trim()` of `[00A0, 0078, 00A0]` | `[160,120,160]` | `[120]` *(read)* | `[160,120,160]` |
| 34 | `trim()` of `[0000, 0078, 0000]` | `[120]` | `[0,120,0]` *(read)* | `[120]` |
| 35 | `strip()` of `[2007, 0078, 2007]` | `[8199,120,8199]` | `[120]` *(read)* | `[8199,120,8199]` |
| 36 | `strip()` of `[001C, 0078, 001C]` | `[120]` | `[28,120,28]` *(read)* | `[120]` |
| 37 | `strip()` of `[2028, 0078, 2028]` | `[120]` | `[120]` *(read — already right)* | `[120]` |
| 38 | `indent(-1)` of `[00A0, 0061]` | `[160,97,10]` | **VM ABORT** — `&line[1..]` inside a 2-byte NBSP *(read)* | `[160,97,10]` |
| 39 | `indent(-1)` of `[2028, 0061]` | `[97,10]` | `[97,10]` *(read — right by accident: both tables call U+2028 whitespace)* | `[97,10]` |
| 40 | `indent(-1)` of `[001C, 0061]` | `[97,10]` | `[28,97,10]` *(read)* | `[97,10]` |
| 41 | `"  a".indent(Integer.MIN_VALUE)` | `[97,10]` | panic under debug-assertions *(read)* | `[97,10]` |
| 42 | `"  a".indent(-1)` | `[32,97,10]` | `[32,97,10]` *(read — already right)* | `[32,97,10]` |
| 43 | `[A7CF].regionMatches(true,0,[A7CE],0,1)` | `false` | `true` — Rust pairs them, the JDK does not *(read)* | `false` |
| 44 | `[A7D3].regionMatches(true,0,[A7D2],0,1)` | `false` | `true` *(read)* | `false` |
| 45 | `[A7D5].regionMatches(true,0,[A7D4],0,1)` | `false` | `true` *(read)* | `false` |
| 46 | `toUpperCase()` of `[A7D3]` | `[42963]` (identity) | `[42962]` *(read)* | `[42963]` |
| 47 | `[A7D1].regionMatches(true,0,[A7D0],0,1)` (control — a real JDK case pair inside the same span) | `true` | `true` *(read)* | `true` |
| 48 | `toUpperCase()` of `[00DF]` (control — the full mapping must NOT become 1:1) | `[83,83]` | `[83,83]` *(read)* | `[83,83]` |

Rows 38–42 were found by auditing this file for the panic shape after the
orchestrator asked for an abort guarantee, not by any census. Rows 33–37 were
not among W7-95's seven either. They are here because `trim` and the
`strip*` family share `isBlank`'s single root cause and live in the same file;
fixing `isBlank` and leaving them is the `[no-op w/ excuse]` shape — a family
fix that does not cover every member of the family. `trim()` and `strip()` are
also two *different* contracts that had been collapsed onto one Rust call, and
every row where they disagree with each other is a row `str::trim` gets wrong
for at least one of them.

## The hazard that got past an exhaustive sweep

The lane that owned `lang_math.rs` reviewed this file and found six code points
missing from its case-exception table. **Verified here directly against
OpenJDK 25.0.3+9 before adopting** — and the measurement corrects the
nomination's framing in a way that matters:

```text
         Character.toUpperCase  toLowerCase  isDefined  getType
U+A7CE   U+A7CE                 U+A7CE       false      0  (UNASSIGNED)
U+A7CF   U+A7CF                 U+A7CF       false      0  (UNASSIGNED)
U+A7D2   U+A7D2                 U+A7D2       false      0  (UNASSIGNED)
U+A7D3   U+A7D3                 U+A7D3       true       2  (LOWERCASE_LETTER)
U+A7D4   U+A7D4                 U+A7D4       false      0  (UNASSIGNED)
U+A7D5   U+A7D5                 U+A7D5       true       2  (LOWERCASE_LETTER)
```

**Four** are unassigned, not five: `U+A7D3` (LATIN SMALL LETTER DOUBLE THORN)
and `U+A7D5` (LATIN SMALL LETTER DOUBLE WYNN) are assigned lowercase letters
that simply have no uppercase partner in the JDK's Unicode version. A newer
Unicode added the capitals, which is exactly why Rust pairs them. So the
framing is not only "Rust's tables are newer than the JDK's" — it is also
"both sides know the character and only one of them has a mapping for it".
Both shapes need the same fix and both will recur.

All six were confirmed observable through the real `String` API, not just
`Character`: `toUpperCase`/`toLowerCase` are identity, and
`regionMatches(true, …)` / `equalsIgnoreCase` against the Rust-side partner are
`false`. That last one is the discriminator — a table that pairs them answers
`true`.

**Why I adopted this without being able to execute Rust.** I cannot verify
Rust's side of the claim from here. I do not need to: the six arms return the
JDK's measured answer, which is identity. If Rust also maps them to
themselves the arms are no-ops; if Rust pairs them the arms fix a real bug.
Correct under both hypotheses, so the change lands without resolving the
question.

### How it got past a sweep that was described as exhaustive

This is the part worth keeping. The original exception table was not sampled —
it was derived by dumping `Character.toUpperCase((char) c)` and `toLowerCase`
for **all 65,536** BMP code units from the JDK and diffing them against a model
of "Rust's full mapping, multi-char falls back to identity". The diff found 28
differences, all 28 were encoded, and the sweep reported success.

The model was **Python's** `str.upper()`/`str.lower()`, standing in for Rust's.
Measured just now: Python here is on UCD 16.0.0 and **agrees with the JDK at
all six** — `U+A7CE` is unassigned in Python's tables too. So the diff was
genuinely empty at those code points and genuinely reported no work to do.

The Java side of that measurement was real and exhaustive. The Rust side was a
proxy that I never validated *as a proxy*. `[setup lies]` — a probe's setup is
code that can be wrong, and **an exhaustive sweep against the wrong oracle is
still exhaustive**. Sweeping all 65,536 code units bought precision about a
question that was subtly the wrong question.

The durable conclusion, which is why it changed more than one function here:
**derive the table from the JDK, or enumerate it, but do not compute it from
Rust's tables at runtime.** Applied twice —

* the six arms are now constants;
* `java_char_is_whitespace` was **converted from a derivation to an explicit
  enumeration**, even though the derivation measured perfect (see below). It
  consulted `char::is_whitespace` at runtime, which is the same class of
  dependency, on a set small enough to just write down.

The case tables cannot be enumerated as cheaply — the JDK has ~1,400 BMP case
mappings — so `char::to_uppercase` remains the fast path there, with a
documented exception list. That residual is stated rather than hidden: it will
need re-measuring whenever either side's Unicode version moves, and the
`skew.*` rows of the vector are what will say so.

## Both tables are now pinned exhaustively, not by sample

Adopting the other lane's technique — transliterate the Rust back into Java and
run it against HotSpot over the whole domain:

**Whitespace, all 1,114,112 code points.** `WsSweep` transliterates both the
old derivation (modelling `char::is_whitespace` as
`Character.isSpaceChar(cp) || cp in {09..0D, 85}`, which is the definition of
Unicode `White_Space`) and the new enumeration, and compares each against
`Character.isWhitespace`:

```
proposed mismatches=0
derived  mismatches=0
proposed-vs-derived disagreements=0
code points swept=1114112
```

So the derivation was never wrong — it was *fragile*, and the enumeration is
the same function with the dependency removed. `Character.isWhitespace` is true
for exactly **25** code points on JDK 25, so the committed vector pins the
whole set rather than sampling it: one row asserting all 25 together, 25 rows
asserting each alone (a conjunction hides a single wrong member), and four rows
for the entire Unicode-vs-Java disagreement (`U+0085`, `U+00A0`, `U+2007`,
`U+202F`).

**What this technique does NOT do**, stated because it would be easy to
overclaim: transliterating into Java proves the constants in the repo encode
the JDK's answers. It cannot execute `char::is_whitespace` or
`char::to_uppercase`, so it says nothing about Rust's side — which is precisely
where the six code points went wrong. It is a strong witness for enumerations
and a weak one for anything still deriving from Rust.

## The middle of the domain, not just the corners

W7-95's `Math.pow` finding — special values correct while ordinary inputs were
44 ulp wrong, because the census sampled edges — applies directly here. This
record's rows are almost all surrogate corners and boundary throws. The
"ordinary input" for the code-point family is *any text at all*, so the vector
now walks 14 real Unicode blocks (Latin, Cyrillic, Hebrew, Thai, Latin
Extended Additional, Hiragana, CJK, Hangul, the `A7Cx` span itself, and four
supplementary blocks including emoji and Plane 16), 64 consecutive code points
each, and at **every position** checks that `codePointAt`, `codePointCount`,
`offsetByCodePoints` (forward *and* backward), `codePoints()` and
`regionMatches` all agree with each other and with the code point that was put
there.

Two properties make those rows non-vacuous:

* **The expectations come from arithmetic, not from the VM.** `charCount` is a
  native this suite is auditing, so the sweep uses a local
  `cc(cp) = cp >= 0x10000 ? 2 : 1` instead. Otherwise a `charCount` wrong in
  the same direction as `codePointAt` would make every row agree vacuously.
* **The fixture is validated before the subject.** `StringBuilder
  .appendCodePoint` is also a native; if it misbuilds the string the row
  reports `FIXTURE length=…`, not a `codePointAt` failure.

Mutation-checked rather than assumed: breaking `cc()` to `return 1` turns the
four supplementary blocks red with `FIXTURE length=128 want 64`, which is both
a failure and a correctly attributed one.

## Which registration actually wins

Checked rather than assumed, because W7-95's own §2 is a case of a duplicate
registration silently overwriting an `Intrinsic` one and making a fix invisible.

The seven triples are registered at **three** sites:

| site | enclosing registrar | reached in the shipping binary? |
|---|---|---|
| `native-builtins/src/lang_math.rs`, `register_wrapper_natives` (called from `register_essential_natives_with_shims`, `lib.rs:14634`) | — | **yes; this is the one that wins** |
| `native-builtins/src/lib.rs`, `register_synthetic_overrides` | — | no: `#[cfg(feature = "synthetic-jdk")]`, and both real-JDK arms of `vm_init` call only `register_essential_natives_with_shims` |
| `native-builtins/src/phases_early.rs:1988` (`repeat`) and `:2060` (`codePointAt`), inline closures registered LATER and therefore last-write-wins | `register_core_stdlib_extras` ← `register_enterprise_final_natives` ← `register_synthetic_overrides` | no, same gate |

So every winning registration points at a `lang_string.rs` body, which is why
this fix is one file — **except `codePoints`**, which both the winning site and
the synthetic one bind to `native_string_chars` with the comment "Same as chars
for BMP". That comment is true and is not the contract: a String holding one
surrogate *pair* yields four elements from `chars()` and three from
`codePoints()`, so `codePoints().count()` was wrong for every emoji, not just
the values. `native_string_code_points` now exists in `lang_string.rs` and
nothing calls it. See N1.

## What changed

`native-builtins/src/lang_string.rs`:

* Three new private predicates: `java_char_is_whitespace` (an explicit
  25-code-point enumeration, swept against all 1,114,112), and
  `java_char_to_upper_case` / `java_char_to_lower_case`, whose exception table
  holds **34** entries: 27 Greek vowels with ypogegrammeni whose simple
  uppercase is the *titlecase* character (`U+1F80` maps to `U+1F88`), `U+0130`
  on the lowercase side, and the six `JDK_UNMAPPED_CASE_CODE_POINTS`. The first
  28 were derived by dumping all 65,536 BMP code units from the JDK; the last
  six were found by review after that derivation missed them, for the reason
  set out above. The `lang_math.rs` lane independently derived the same 27
  ypogegrammeni exceptions — genuine cross-validation of the part that was
  right.
* `String.toUpperCase`/`toLowerCase` keep the FULL mapping (`ß` really does
  become `"SS"` at String level — only `Character.toUpperCase(char)` is 1:1),
  with the six skew code points passed through unchanged. The character-wise
  path runs only when one is actually present, so ordinary text keeps the
  single bulk call, and the ASCII fast path never reaches it. `skew.sharps.up`
  is the control that this did not quietly turn the String-level method into
  the 1:1 mapping.
* `codePointAt`, `codePointCount`, `offsetByCodePoints`, `chars`, `repeat`,
  `isBlank`, `trim`, the `strip*` family and both `regionMatches` overloads now
  read `read_string_chars` instead of `ctx.read_string(..).encode_utf16()`.
* `codePointCount` throws `IndexOutOfBoundsException` — the SUPERCLASS, with
  `checkFromToIndex` wording — instead of clamping `end` and casting a negative
  `begin` to a huge `usize`. `offsetByCodePoints` transcribes
  `Character.offsetByCodePoints`'s two bounded loops plus its shortfall test,
  and throws `IndexOutOfBoundsException` with a **null** message. The two
  neighbours throw different classes and different message shapes on purpose;
  both were read off a HotSpot run, not guessed.
* `repeat` gets the JDK's check ORDER (`count < 0` first, before the
  empty-string short circuit), the JDK's `Integer.MAX_VALUE / count < len`
  guard on the same quantity the JDK measures (`value.length`, i.e. two bytes
  per char once the UTF-16 coder is needed, which is why the coder is
  reconstructed rather than using the char count), and a `try_reserve_exact` so
  an allocation failure surfaces as `OutOfMemoryError` instead of aborting.
* `regionMatches` is one shared `region_matches_impl` for both overloads,
  carrying the JDK's `long`-widened bounds test verbatim — including the
  consequence that a negative `len` PASSES it and the comparison loop then runs
  zero times, which is why the answer is `true`.
* `code_unit_eq_ignore_case` transcribes `StringUTF16.regionMatchesCI`,
  including that the JDK lower-cases the **upper-cased** forms rather than the
  originals. That composition is what makes `U+212A` KELVIN SIGN match `k`.
* `chars()` and the new `codePoints()` share one `int_stream_of`, so the
  synthetic-stream layout notes — paid for twice already, once by a 1-field
  allocation and once by a reference-array allocation — exist in one place.

`native-builtins/src/charset.rs`:

* `read_string_utf16` delegates to `lang_string::read_string_chars`. Its only
  caller is `Charset.encode(String)`, where an unpaired surrogate is
  *malformed input* that a real `CharsetEncoder` must report — and U+FFFD is a
  perfectly encodable character, so the malformed unit was being laundered into
  three valid UTF-8 bytes before the encoder ever saw it.

Both files were checked with `rustfmt --check` against their pre-change
selves: the change adds **zero** new formatting diffs (28 before and after in
`lang_string.rs`, 10 in `charset.rs` — the repo is not rustfmt-clean and this
change does not make it worse).

### The reader half is fixed; the writer half draws a hard line through the family

Worth stating explicitly, because it decides which of these methods are done
and which are only half-done, and the split is not the one you would guess
from the defect list:

* **Methods whose answer is a number, a boolean, or an `int[]`** —
  `codePointAt`, `codePointCount`, `offsetByCodePoints`, `isBlank`,
  `regionMatches` (both), `chars`, `codePoints` — are **fully fixed**. Nothing
  on their path constructs a Java `String`, so once the *reader* stops going
  through `str`, an unpaired surrogate survives end to end.
* **Methods whose answer is a `String`** — `repeat`, `trim`, `strip*`, and
  `lines` — are fixed for their *predicate* behaviour (which characters count
  as whitespace, where the boundaries are, which exceptions are thrown) and
  remain lossy for *content*, because `NativeContext` has no
  `create_string(&[u16])`. `String::from_utf16_lossy` at the end of those
  bodies is not an oversight; it is the only constructor that exists. N3's
  writer half is the fix for all four at once, and until it lands, adding a
  lossless read to `lines()` would buy nothing observable — the surrogate would
  die on the way out instead of on the way in.

### No path this change touches can panic

Stated because the orchestrator's calibration run showed every current
`RJdkIntrinsics2` failure to be a Java `AssertionError` rather than a Rust
panic, and turning one of those into an abort would be a regression from the
current state even while fixing a wrong answer. Audited, arm by arm:

* every `chars[i]` in `codePointAt` / `codePointCount` / `offsetByCodePoints` /
  `codePoints` / `regionMatches` sits behind an explicit bound that the arm
  itself established, and the two backward-walking arms guard `pos > 0` before
  the decrement rather than after;
* `-code_point_offset` is gone, replaced by `code_point_offset.unsigned_abs()`
  widened to `i64` — that was the `Integer.MIN_VALUE` negation panic;
* `repeat`'s only division has a divisor proven `>= 2` by the two early
  returns above it, its length arithmetic is `checked_mul`, and its allocation
  is `try_reserve_exact`, so an out-of-memory becomes
  `java.lang.OutOfMemoryError` rather than a Rust capacity-overflow abort;
* `indent`'s negative arm converts a CHARACTER count back to a byte offset
  through `char_indices().nth(..)`, which can only ever land on a boundary,
  and takes `n.unsigned_abs()` rather than `-n`;
* every out-of-range input now leaves through `Err(RuntimeError::…)`, which the
  interpreter turns into a Java throwable, and no arm reaches `unwrap`,
  `expect`, a bare slice index, or unchecked signed arithmetic.

The one abort path deliberately left alone is `indent`'s POSITIVE arm, which
pushes `n` spaces per line and will exhaust memory for a large `n` — that is
what HotSpot does too (`OutOfMemoryError`), it predates this change, and
turning a Rust allocation failure into a Java `OutOfMemoryError` there needs
the same `try_reserve` treatment `repeat` just got. Noted, not done.

## The vector

`regression-suite/src/RJdkStringCodePoints.java` — 110 checks, measured green
on Microsoft OpenJDK 25.0.3+9, never yet run on CratonVM.

It is pure ASCII: every fixture is a backslash-u escape, because a lone
surrogate cannot survive a source-encoding round trip at all and a file holding
`U+00A0` as a byte is one `-encoding` default away from holding a space. This
is not hypothetical — two drafts of this record and one draft of the fixture
had exactly that happen to them in-flight, which is why both are now
name-only. Every answer prints as an `int`, a boolean, or `EX:<SimpleName>`; a
code point printed as text reports the console encoder, not the VM. The
fourteen `fx.*` rows assert the fixtures themselves before any subject row
runs, so "this classifier says NO about U+00A0" cannot pass vacuously. `repeat`
is reported by length plus `hashCode`, so a six-character answer that is the
wrong six characters cannot pass. There are no lambdas and no
`Arrays.toString`: the subject is `String`, and a probe that also leans on
invokedynamic reports the union of two subsystems.

`repeat`'s two overflow rows are the ones to watch on the first CratonVM run.
They are cheap on HotSpot — the guard precedes any allocation — and they were
the VM-abort path here, so if the process dies at `rep.ab.max` that is the
guard not landing, not the test being slow.

## NOMINATIONS

### N1 — REQUIRED, or `codePoints()` stays wrong for surrogate pairs

`native-builtins/src/lang_math.rs`, the `codePoints` registration inside
`register_wrapper_natives` (the string `"codePoints"` is at line 737 as of this
writing, but other lanes are editing that file — match on the text, not the
line). This is the site that wins in the shipping binary.

old:

```rust
    registry.register(
        "java/lang/String",
        "codePoints",
        "()Ljava/util/stream/IntStream;",
        native_string_chars, // Same as chars for BMP
    );
```

new:

```rust
    registry.register(
        "java/lang/String",
        "codePoints",
        "()Ljava/util/stream/IntStream;",
        // NOT `native_string_chars`. The comment this replaces said "Same as
        // chars for BMP", which is true and is not the contract: a String
        // holding one surrogate PAIR yields four elements from `chars()` and
        // three from `codePoints()`, so `codePoints().count()` was wrong for
        // every emoji, not merely the values. See W7-95a.
        native_string_code_points,
    );
```

and add `native_string_code_points` to the `use crate::lang_string::{...}` list
at the top of `native-builtins/src/lang_math.rs` (line 10–17), next to the
`native_string_code_point_count` that is already there.

The same one-word change applies to the synthetic-jdk twin in
`native-builtins/src/lib.rs` (line 23234 as of this writing), whose
registration reads `native_string_chars, // Same as chars() for BMP
characters`. That file has `use lang_string::*;` (`lib.rs:4596`), so it needs
no import. **Change both, or the synthetic-jdk gate and the shipping binary
disagree** — the `[dup-fix]` shape, which is what W7-95 §2 was written about
for this very class.

### N2 — the two `phases_early.rs` inline closures are worse than what they shadow

`native-builtins/src/phases_early.rs:1988` (`repeat`) and `:2060`
(`codePointAt`) are inline closures registered after the `lang_string.rs`
bodies and therefore win — but only inside `register_synthetic_overrides`,
which is `#[cfg(feature = "synthetic-jdk")]` and does not run in the shipping
binary. In a synthetic-jdk build they ARE what executes, and both carry defects
the `lang_string.rs` bodies no longer have:

* `repeat` is `args.get(1)...unwrap_or(0).max(0)` — the exact
  negative-count-swallowing this record just removed — plus an unbounded
  `val.repeat(count)`, i.e. the abort in row 19.
* `codePointAt` is `val.chars().nth(idx)`: a code-POINT index where the
  argument is a code-UNIT index, over a lossy `ctx.read_string`, with
  `.unwrap_or(0)` where the spec requires `StringIndexOutOfBoundsException`.
  It is wrong for every string containing any non-BMP character, not only at
  the boundaries.

**Delete both closures.** The `lang_string.rs` registrations already cover both
triples, and `register_core_stdlib_extras` runs after them, so removing the
closures restores the fixed bodies rather than leaving a hole.

### N3 — `NativeContext` exposes neither a lossless String reader nor a lossless String writer

Two halves. The second still gates a residual.

* **Reader.** `vm/src/vm/vm_object.rs:758`'s `read_java_string_units` is the
  documented lossless twin of `read_java_string`, and has exactly one caller
  in the tree. It is not on the `NativeContext` trait, which is why every
  native wanting code units either re-implements the decode (as
  `lang_string::decode_string_chars` does) or silently accepts U+FFFD (as
  every other native in the tree still does). Add
  `fn read_string_units(&self, obj: ObjectRef) -> Option<Vec<u16>>` to
  `native-api/src/registry.rs`'s `NativeContext`, defaulted to
  `self.read_string(obj).map(|s| s.encode_utf16().collect())` so no
  implementor breaks, and override it in `vm/src/vm/vm_exec.rs` with
  `read_java_string_units`.
* **Writer.** There is no `create_string` taking `&[u16]`; the only
  constructor is `create_string(&str)`. So `String.repeat` on a receiver
  containing an unpaired surrogate **cannot** be made lossless by any native —
  the fixed body still ends in `String::from_utf16_lossy`. The unit COUNT is
  preserved (a lone surrogate and U+FFFD are both one UTF-16 unit), so every
  length and bound in this record is right either way; the residual is confined
  to the CONTENT of a repeated lone surrogate. `create_java_string` in the VM
  already writes the compact `byte[]` + `coder` pair from units, so exposing
  `create_string_from_utf16(&mut self, units: &[u16]) -> ObjectRef` is the same
  shape as the reader half. Row 32 is fixed by the reader half alone; `repeat`
  of a lone surrogate needs the writer half.

### N4 — register the vector. REQUIRED, not optional.

`regression-suite/run.sh`, `CORE_CLASSES` (around line 106) — **not**
`JDKONLY_CLASSES`, for the same reason W7-95's N7 gives for `RJdkIntrinsics`:
these are language semantics, identical under `--real-jdk` and `--jdk-only`,
and the natives are registered in both arms. Append to the end of that list:

```
 ... RJdkStrictMath RJdkByteOrder RJdkIntrinsics RJdkStringCodePoints"
```

Until this lands, `regression-suite/src/RJdkStringCodePoints.java` is in no
class list, which `run.sh`'s coverage gate reports as a WARNING by default and
as a **failure under `STRICT_COVERAGE=1`, which is what CI runs**. Do not leave
it in neither list; if it must not run yet, park it in `UNREGISTERED_CLASSES`
(around line 157) with a reason.

### N5 — `Character.isWhitespace` is now implemented twice, and the case helpers should not become a third copy

`lang_string.rs::java_char_is_whitespace` and
`lang_math.rs::native_character_is_whitespace` (fixed by W7-98a) are the same
JDK definition transcribed twice, in two files, in one crate. That is the
`[reflect≠opcode]` shape — one rule implemented twice, which drifts. Hoist it
to one `pub(crate) fn` (`lang_math.rs` is the natural home, since the
`Character` natives live there) and have both the native and the `String`
family call it. Not done here because `lang_math.rs` is outside this lane's
file list and a cross-file move is not a nomination-sized edit.

The same argument applies with more force to `java_char_to_upper_case` and
`java_char_to_lower_case`. W7-95's N4 asks a lane to fix
`native_character_to_upper_case` / `to_lower_case`, whose defect is precisely
what these two helpers work around. **That lane should adopt these two
functions rather than write a third copy of the rule** — the 28-entry exception
table in their doc comments is the derivation, it took a full BMP dump against
HotSpot to produce, and it is not obvious.

### N7 — `case_map.rs` has the same Unicode version skew, on the locale path

`native-builtins/src/case_map.rs` is the Turkish/Lithuanian/Azeri branch that
`string_case_impl` takes when `is_locale_dependent(&lang)`, so it bypasses the
fix landed here entirely: `toUpperCase(new Locale("tr"))` of the one-character
string `[A7D3]` is still wrong.
Its own module doc already says it implements "Unicode's" rules, which
is the tell.

Three sites, all falling through to Rust's tables for any character without a
locale-specific rule:

* `case_map.rs:118` — `return s.to_lowercase();`
* `case_map.rs:126` — `return s.to_uppercase();`
* `case_map.rs:141`–`142` — `None if lowercasing => out.extend(c.to_lowercase()), None => out.extend(c.to_uppercase()),`

The fix is the same guard this record added to `string_case_impl`: pass
`JDK_UNMAPPED_CASE_CODE_POINTS` through unchanged. Since that constant now
lives in `lang_string.rs`, either make it `pub(crate)` and import it, or take
N5's hoist and give both modules one home for JDK-vs-Unicode table facts. The
second is better — this is the third file in the crate that needs the same six
constants.

Also at `case_map.rs:283`–`284`: `c.to_uppercase().next() != Some(c) && c.to_lowercase().next() != Some(c)`
is a "does this character have case" test built from Rust's tables, so it
answers `true` for all six where the JDK says `false`. Not measured through a
Java API by this lane — flagged as a likely third instance rather than a
confirmed one.

### N6 — one-line cross-reference into W7-95

`docs/known-issues/jdk-only/W7-95-intrinsic-semantics-census.md` is owned by
another lane this wave, so this is an edit request rather than an edit. In its
`### N6 — `String`'s code-point family` section, old:

```
### N6 — `String`'s code-point family
```

new:

```
### N6 — `String`'s code-point family — **DONE, see
[W7-95a](W7-95a-string-code-point-family.md)**
```

The body of that section can stay as it is: it is an accurate statement of the
problem, and W7-95a's opening quotes it. The point of the line is that a reader
arriving at N6 should not start the search over.

## Residuals

* **Every "after" in the table is PREDICTED.** Nothing here was observed on a
  CratonVM binary. `RJdkStringCodePoints` is the instrument; its first CratonVM
  run is the measurement, and rows that come back red there are real.
* **`codePoints()` over a surrogate PAIR is not fixed until N1 lands.** The
  two lone-surrogate rows are fixed without it, which makes the remaining gap
  easy to mistake for "fixed" if only rows 2 and 3 are checked.
* **`repeat` of a string containing an unpaired surrogate** still substitutes
  U+FFFD in the CONTENT — no native can build such a String today. N3's writer
  half. Lengths and bounds are unaffected.
* **Not measured here:** `String.equalsIgnoreCase`, `toUpperCase(String)`,
  `toLowerCase(String)`, `compareToIgnoreCase`, `indexOf(int codePoint)`,
  `codePointBefore`, `String.valueOf(char[])`. `native_string_copy_value_of` is
  visibly in the same family and is worse than its neighbours: it does
  `char::from_u32(c)` and pushes only on `Some`, so it **silently DROPS** every
  surrogate element rather than replacing it — `new String(new char[]{ D800 })`
  loses a character rather than corrupting one. Whichever lane takes N3 should
  take that too, since the writer half is its fix.
* **`Charset.encode` of malformed input.** Now visible for the first time:
  with `read_string_utf16` lossless, an unpaired surrogate reaches
  `encode_with_charset` as `0xD800`. Whether that path reports MALFORMED or
  replaces per the charset's `CodingErrorAction` was not measured — before this
  change the question could not even be asked, because the surrogate never
  arrived.
* **`String.indent()`'s negative arm is fixed** (the panic and the whitespace
  table); its **line-terminator handling is not**. Both `indent` and `lines`
  iterate Rust's `str::lines`, which splits on `\n` and strips a trailing `\r`
  — Java's `lines()` also treats a **bare CR** as a terminator, so
  `"a\rb".lines().count()` is 2 on HotSpot and 1 here. `RJdkIntrinsics2`'s
  `strfmt` family already pins that row and is owned by another lane, so
  `RJdkStringCodePoints` deliberately does not duplicate it: a vector that goes
  red for a defect this record did not fix makes this record's signal
  unreadable.
* **`String.lines()` content** is still read through `ctx.read_string`. Not
  fixed, and fixing only its reader would change nothing observable — it
  returns `String`s, so it is in the second bucket above, blocked on N3's
  writer half.
* **Concurrency.** As W7-95 said of its own scope: this record tested values on
  one thread.
* **Overlap with `RJdkIntrinsics2`.** That fixture's `strfmt` family already
  pins the two `chars()` surrogate rows (#4 above) and is owned by another
  lane; `RJdkStringCodePoints` re-asks them so this record's vector stands
  alone, and adds the 117 rows `strfmt` does not cover. No case needs to be
  added to `RJdkIntrinsics2` on this record's account.
