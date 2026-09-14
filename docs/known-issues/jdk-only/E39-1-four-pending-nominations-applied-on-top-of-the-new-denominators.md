# E39-1 — Four pending nominations APPLIED, on top of denominators that had just moved

**2026-08-13, lane E39.** Applies E8-1 §7 N1 and N2, and E18-1 §7 N3 and N6
(re-affirmed by E27-1), to `regression-suite/src/RJdkIntrinsics2.java`.

**Status: META / APPLIED.** This is not a defect record. It is the marker that
says those four nominations are **spent**, so nobody applies them a second time
— which is a live trap in this session, not a hypothetical one.

This lane **owns `regression-suite/src/RJdkIntrinsics2.java`** and this file.
Nothing else was edited. **This lane may not build or run the VM**; every value
below was MEASURED on this host against `openjdk 25.0.3 2026-04-21 LTS
(25.0.3+9-LTS)` (Microsoft build), from `scratchpad/e39/E39Probe.java`.
Nothing about CratonVM's behaviour is claimed — this is a vector, not a verdict.

The fixture was and remains **LF-only (0 CRLF)**, 4056 lines, and it now
compiles **silent under `-Xlint:all`**.

---

## 1. Verdict

| | |
|---|---|
| nominations applied | **4 of 4** (E8-1 N1, E8-1 N2, E18-1 N3, E18-1 N6) |
| rows added | **39** (951 -> **990**) |
| families | 11 -> **12** (`strnull` is new) |
| rows dropped as redundant | **1** (E18-1 N6's supplementary-pair `indexOf`, §4) |
| denominators recomputed from the FILE, not from the nomination | 2 (`bounds`, `strnull`) |
| denominators a nomination got wrong | **1** — E8-1's self-flagged `strnull` hand count of 31 is really **30** (§3.2) |
| oracle mutants planted | 40 |
| oracle mutants **killed** | **40 (100%)**, 0 survived, 0 unusable (§7) |
| rows corrected by HotSpot on first run | **0** — but only because every row was measured by a probe BEFORE it was written (§2) |
| NOMINATIONS raised | 3 (§8) |

**New total: 990 checks.**

## 2. Every row was measured before it was written

E26 had two of its own new rows corrected by HotSpot on first run. The cheap
insurance against repeating that is to make the oracle speak first, so this lane
wrote `scratchpad/e39/E39Probe.java` — which asserts nothing and only prints —
and turned its output into assertions afterwards. The transcript:

```
=== E8-1 N1: regionMatches four-term short-circuit ===
"ab".regionMatches(0, null, 0, 1)             THROWS java.lang.NullPointerException
"ab".regionMatches(9, null, 0, 1)             = false
"ab".regionMatches(0, null, -1, 1)            = false
"ab".regionMatches(0, null, 0, -1)            THROWS java.lang.NullPointerException

=== E18-1 N3: equals vs contentEquals across CharSequence types ===
new StringBuilder(3).capacity()               = 3
"abc".equals((Object) sb3)                    = false
"abc".contentEquals(sb3)                      = true
"abc".contentEquals((CharSequence) "abc")     = true

=== E18-1 N6: indexOf(int) / lastIndexOf(int) code points ===
mixed.length()                                = 6
"abc".indexOf(0x10061)                        = -1
"abc".lastIndexOf(0x10061)                    = -1
mixed.indexOf(0x10437)                        = 3
mixed.indexOf(0x0437)                         = 1
mixed.lastIndexOf(0xDC37)                     = 4
mixed.indexOf(0xD801)                         = 3
"￿q".indexOf(-1)                         = -1
"￿q".indexOf(0xFFFF)                     = 0
"￿q".lastIndexOf(-1)                     = -1
mixed.lastIndexOf(0x10437, 2)                 = -1
mixed.lastIndexOf(0x10437, 3)                 = 3
mixed.lastIndexOf(0x10437, 4)                 = 3
mixed.indexOf(0x110000)                       = -1

=== E8-1 N2: strnull ===
"abc".equals(null)                            = false
"abc".equalsIgnoreCase(null)                  = false
String.valueOf((Object) null)                 = null
String.join(",", "a", null, "b")              = a,null,b
"abc".startsWith(null, -1)                    = false
"abc".contains(null)                          THROWS java.lang.NullPointerException
"abc".startsWith(null)                        THROWS java.lang.NullPointerException
"abc".startsWith(null, 0)                     THROWS java.lang.NullPointerException
"abc".endsWith(null)                          THROWS java.lang.NullPointerException
"abc".indexOf((String) null)                  THROWS java.lang.NullPointerException
"abc".lastIndexOf((String) null)              THROWS java.lang.NullPointerException
"abc".compareTo(null)                         THROWS java.lang.NullPointerException
"abc".compareToIgnoreCase(null)               THROWS java.lang.NullPointerException
"abc".concat(null)                            THROWS java.lang.NullPointerException
"abc".split(null)                             THROWS java.lang.NullPointerException
"abc".split(null, 2)                          THROWS java.lang.NullPointerException
"abc".matches(null)                           THROWS java.lang.NullPointerException
"abc".replaceAll(null, "x")                   THROWS java.lang.NullPointerException
"abc".transform(null)                         THROWS java.lang.NullPointerException
"abc".toUpperCase((Locale) null)              THROWS java.lang.NullPointerException
"abc".toLowerCase((Locale) null)              THROWS java.lang.NullPointerException
String.join(null, "a", "b")                   THROWS java.lang.NullPointerException
String.join(",", (CharSequence[]) null)       THROWS java.lang.NullPointerException
String.join(",", (Iterable) null)             THROWS java.lang.NullPointerException
String.copyValueOf((char[]) null)             THROWS java.lang.NullPointerException
String.valueOf((char[]) null)                 THROWS java.lang.NullPointerException
new String((char[]) null)                     THROWS java.lang.NullPointerException
"abc".getChars(0, 1, null, 0)                 THROWS java.lang.NullPointerException
"abc".getChars(0, 9, null, 0)                 THROWS java.lang.StringIndexOutOfBoundsException
"abc".getChars(0, 3, new char[3], 2)          THROWS java.lang.StringIndexOutOfBoundsException
"abc".replace(null, "x")                      THROWS java.lang.NullPointerException
"abc".regionMatches(true, 0, null, 0, 1)      THROWS java.lang.NullPointerException
```

**All 39 nominated values agree with the oracle.** No nomination was wrong about
a *value*; one was wrong about a *count* (§3.2).

### 2.1 Measured cells that did NOT become rows

Recorded so the next lane does not re-derive them:

* `mixed.indexOf(0x0437) = 1` and `mixed.indexOf(0xD801) = 3` — the two halves
  of the pair, found as ordinary code units. Interesting, but each duplicates
  the contract a kept row already pins from the other side.
* `"￿q".indexOf(0xFFFF) = 0` — the positive control for the `indexOf(-1)`
  row. Not added: it asserts that an ordinary BMP scan works, which `strfmt`
  already has three rows for.
* `mixed.lastIndexOf(0x10437, 4) = 3` — `fromIndex` past the pair's start is
  clamped, same answer as `3`. Non-discriminating against `3`.
* `mixed.indexOf(0x110000) = -1` — an out-of-range code point. This is the
  `isValidCodePoint` gate again, and `indexOf(-1)` is the sharper form of the
  same row (E18-1 says so itself), so only the sharper one was taken.
* `"abc".replace(null, "x")` and `regionMatches(true, 0, null, 0, 1)` both throw
  NPE. Neither is in any nomination; left for a future reach audit rather than
  smuggled in under someone else's nomination number.

## 3. What was applied, and the denominator each was recomputed to

E26 §8.5 is right that every denominator these nominations quote is stale.
**None of the numbers below was taken from a nomination; each was recomputed by
running the file and letting `sectionEnd` adjudicate.**

| nomination | rows | landed in | denominator IT quoted | denominator NOW |
|---|---|---|---|---|
| E8-1 §7 N1 — `regionMatches` short-circuit | 2 | `bounds` | `35 -> 37` | **91 -> 93** |
| E8-1 §7 N2 — a new `strnull` family | 30 | `strnull` (new) | `sectionEnd("strnull", 31)` | **30** of the 37 (§3.2) |
| E18-1 §7 N3 — `equals` vs `contentEquals` | 2 | `strnull` | "+2", no number | part of **37** |
| E18-1 §7 N6 — `indexOf(int)` code points | 5 of 6 | `strnull` | "six rows", no number | part of **37** |
| | | | | `sectionEnd("strnull", 37)` |

### 3.1 Why N3 and N6 went into `strnull` and not `bounds`

Both nominations say "`strnull` if E8's N2 lands, else `bounds`". N2 landed, so
they went to `strnull`, and the section comment was widened to say what the
family now is: **String's reference-argument contracts** — the null-argument
half (a), the argument-TYPE half (b), and the code-point search half (c). The
name is narrower than the block; the comment is not, and the comment is what a
reader has in front of them.

The side effect is worth stating because it is what makes §6's proof clean:
**`bounds` moves by exactly N1's two rows and nothing else**, so the harness
restructure can be isolated from every row addition.

### 3.2 The one number a nomination got wrong

E8-1 flagged its own `sectionEnd("strnull", 31)` as hand-counted and asked the
owning lane to re-count. It is **30**: four non-throwing rows, one startsWith
escape hatch, twenty-three `checkNpe` rows, two `getChars` ordering rows. The
count was not re-derived by arithmetic — 31 was written into the file first, the
run said

```
block strnull ran 30 checks, header says 31
```

and the file was corrected to what the VM counted. That is the tripwire working
exactly as its javadoc says it should.

## 4. E18-1 N6's supplementary-pair row: DROPPED, and what replaces its bite

E26 §8.5.1 flagged `mixed.indexOf(0x10437) == 3` as redundant with the `strfmt`
row it had just added, `indexOf(OPAQUE_CP[9]) == 1` on the astral-emoji
receiver. **It was dropped**, and the other five N6 rows landed.

But "redundant" was worth checking rather than accepting, because the two rows
are not the same operand and E26 said so. Measured, they differ in one way that
matters:

* On `"a<emoji>b"`, the masked half `0x1F600 & 0xFFFF == 0xF600` **does not
  occur** in the receiver, so a masking implementation answers `-1`.
* On `mixed`, the masked half `0x10437 & 0xFFFF == 0x0437` **occurs at index 1,
  EARLIER than the pair at 3**, so a masking implementation answers `1`, and a
  *first-hit-wins hybrid* — one that matches either the pair or the masked unit
  and takes whichever comes first — also answers `1`.

Plain masking dies to either row. The **hybrid** only dies to a receiver where
the masked half comes first. So dropping N6's row without a replacement would
have quietly given up a mutant.

It does not, because the receiver survives in the row that WAS kept:

```java
check(mixed.lastIndexOf(0x10437, OPAQUE_I[5]) < 0
                && mixed.lastIndexOf(0x10437, OPAQUE_I[6]) == 3, …);
```

`lastIndexOf(0x10437, 2)` scans backwards from index 2 **over that very
U+0437**. A hybrid answers `1`; the JDK answers `-1`. Planted as mutant 39,
**KILLED** (§7). The comment on the row says this, so the next reader does not
have to re-derive it either.

**One row dropped, zero discrimination lost, and the loss was checked rather
than assumed.**

## 5. Two places the nomination text was improved rather than copied

Both are this lane's changes, both are in service of the nomination:

1. **N6's receivers are built from explicit code units, not written as
   non-ASCII source characters.** N6's text spells the receiver `"xзy𐐷z"` and
   `"￿q"`. `run.sh:466` compiles `src/*.java` with **no `-encoding` flag**, so a
   literal operand is decoded with whatever the platform's native encoding
   happens to be — and these particular rows are *about* exact code units. They
   are now `new String(new char[] { 'x', (char) 0x0437, 'y', (char) 0xd801,
   (char) 0xdc37, 'z' })` and the U+FFFF sibling. Verified: the file compiles
   and runs green **with no `-encoding` flag**, which is the spelling run.sh
   actually uses.

2. **`String.join(",", (Iterable<CharSequence>) NULL_OBJ)` became a typed
   `NULL_ITER` field.** The cast selects the right overload but makes the whole
   file compile with an unchecked warning, and run.sh compiles the suite in ONE
   javac invocation. A pre-existing `unchecked` Note already comes from other
   suite sources, so this was cosmetic rather than load-bearing — but the file
   is now silent under `-Xlint:all` and that is worth keeping.

`checkNpe` was taken as nominated (a `java.util.concurrent.Callable`, because
several bodies are `void` while the rest return values). The file already uses
lambdas — six of them, in `bounds`, which runs *before* `strnull` — so the block
introduces no dependency the run did not already carry.

## 6. The fixture as landed — HotSpot transcript

```
$ javac -d . RJdkIntrinsics2.java     # no -encoding, exactly as run.sh spells it
$ java RJdkIntrinsics2                # step lines elided
CK RJdkIntrinsics2 charcls=167
CK RJdkIntrinsics2 boolparse=115
CK RJdkIntrinsics2 floatfmt=89
CK RJdkIntrinsics2 hex=73
CK RJdkIntrinsics2 b64=60
CK RJdkIntrinsics2 uuid=51
CK RJdkIntrinsics2 random=60
CK RJdkIntrinsics2 strfmt=114
CK RJdkIntrinsics2 bounds=93
CK RJdkIntrinsics2 strnull=37
CK RJdkIntrinsics2 strictExact=91
CK RJdkIntrinsics2 divmod=40
CK RJdkIntrinsics2 checks=990
PASS RJdkIntrinsics2 (990 checks)
```

Every family still runs alone:

```
CK RJdkIntrinsics2 only=charcls     CK RJdkIntrinsics2 charcls=167     PASS RJdkIntrinsics2 (167 checks)
CK RJdkIntrinsics2 only=boolparse   CK RJdkIntrinsics2 boolparse=115   PASS RJdkIntrinsics2 (115 checks)
CK RJdkIntrinsics2 only=floatfmt    CK RJdkIntrinsics2 floatfmt=89     PASS RJdkIntrinsics2 (89 checks)
CK RJdkIntrinsics2 only=hex         CK RJdkIntrinsics2 hex=73          PASS RJdkIntrinsics2 (73 checks)
CK RJdkIntrinsics2 only=b64         CK RJdkIntrinsics2 b64=60          PASS RJdkIntrinsics2 (60 checks)
CK RJdkIntrinsics2 only=uuid        CK RJdkIntrinsics2 uuid=51         PASS RJdkIntrinsics2 (51 checks)
CK RJdkIntrinsics2 only=random      CK RJdkIntrinsics2 random=60       PASS RJdkIntrinsics2 (60 checks)
CK RJdkIntrinsics2 only=strfmt      CK RJdkIntrinsics2 strfmt=114      PASS RJdkIntrinsics2 (114 checks)
CK RJdkIntrinsics2 only=bounds      CK RJdkIntrinsics2 bounds=93       PASS RJdkIntrinsics2 (93 checks)
CK RJdkIntrinsics2 only=strnull     CK RJdkIntrinsics2 strnull=37      PASS RJdkIntrinsics2 (37 checks)
CK RJdkIntrinsics2 only=strictExact CK RJdkIntrinsics2 strictExact=91  PASS RJdkIntrinsics2 (91 checks)
CK RJdkIntrinsics2 only=divmod      CK RJdkIntrinsics2 divmod=40       PASS RJdkIntrinsics2 (40 checks)
```

`--list` prints twelve family names, `strnull` between `bounds` and
`strictExact`. Harness dialect untouched: `CK <Class> <family>=<n>`,
`CK <Class> checks=<n>`, `PASS <Class> (N checks)`. `harness-guard.sh`'s
`extract()` keeps **72 of 72** output lines — nothing this lane added is dropped
by the filter run.sh diffs through. **Two new `step=` lines** were added, both in
`bounds`, both for the new `regionMatches` panic-candidates.

### 6.1 The harness restructure, isolated — E8-1 N2's risk, discharged

N2 is the one that restructures `FAMILIES` and the `runFamily` dispatch, and
E26 deliberately refused to fold it into a reach audit. The requirement for
landing it is that the **eleven existing families behave identically**. That is
not shown by the run above, because `bounds` legitimately moved by N1's two
rows. So it was shown by a **restructure-only isolation build**: the landed
file with the N1 block removed and `sectionEnd("bounds", 91)` restored — i.e.
the harness change and the new `strnull` method, and no edit to any existing
family.

```
=== ISOLATION BUILD: harness restructure ONLY ===
CK RJdkIntrinsics2 charcls=167      CK RJdkIntrinsics2 strfmt=114
CK RJdkIntrinsics2 boolparse=115    CK RJdkIntrinsics2 bounds=91      <-- baseline
CK RJdkIntrinsics2 floatfmt=89      CK RJdkIntrinsics2 strnull=37
CK RJdkIntrinsics2 hex=73           CK RJdkIntrinsics2 strictExact=91
CK RJdkIntrinsics2 b64=60           CK RJdkIntrinsics2 divmod=40
CK RJdkIntrinsics2 uuid=51          CK RJdkIntrinsics2 checks=988
CK RJdkIntrinsics2 random=60        PASS RJdkIntrinsics2 (988 checks)
```

**All eleven pre-existing counts are byte-identical to E26's landed
951-check run**, `bounds` included: `167 115 89 73 60 51 60 114 91 91 40`. The
full `--only` matrix was run against this build too and every one of the eleven
prints its baseline count. The restructure adds a family; it does not perturb
one. **N2 is applied, not left nominated.**

## 7. Mutation transcript — 40 oracle mutants, 40 killed

Each mutant replaces an **expected value** in a new row with the answer a
*specific plausibly-wrong implementation* would give — never a negated
condition. The exact `sectionEnd` denominator already proves every row executes
(E26 §6.1), so a negation would prove nothing new. Harness:
`scratchpad/e39/mutate.py`; each mutant is compiled and run with
`--only=<family>`. Every one of the 39 new rows carries at least one mutant.

```
KILLED  [bounds    ] regionMatches null checked FIRST (the naive fix E8-1 named)
KILLED  [bounds    ] regionMatches term ONE evaluated after the dereference
KILLED  [strnull   ] equals(null) throwing - a null fix applied by SHAPE
KILLED  [strnull   ] equalsIgnoreCase(null) throwing like its compareTo sibling
KILLED  [strnull   ] valueOf((Object) null) as "" - a Rust unwrap_or_default
KILLED  [strnull   ] join rendering a null ELEMENT as "" instead of "null"
KILLED  [strnull   ] startsWith(null, -1) throwing - no short-circuit before the deref
KILLED  [strnull   ] contains(null) answering false
KILLED  [strnull   ] startsWith(null) answering false
KILLED  [strnull   ] startsWith(null, 0) answering false
KILLED  [strnull   ] endsWith(null) answering false
KILLED  [strnull   ] indexOf(null) answering -1 - indistinguishable from a real miss
KILLED  [strnull   ] lastIndexOf(null) answering -1
KILLED  [strnull   ] compareTo(null) answering a number
KILLED  [strnull   ] compareToIgnoreCase(null) answering a number
KILLED  [strnull   ] concat(null) appending ""
KILLED  [strnull   ] split(null) answering a null String[]
KILLED  [strnull   ] split(null, 2) answering a null String[]
KILLED  [strnull   ] matches(null) answering false
KILLED  [strnull   ] replaceAll(null, x) answering the receiver
KILLED  [strnull   ] transform(null) answering the receiver
KILLED  [strnull   ] toUpperCase((Locale) null) falling back to the DEFAULT locale - E18-1 N1's JIT defect
KILLED  [strnull   ] toLowerCase((Locale) null) falling back to the DEFAULT locale
KILLED  [strnull   ] join(null, ...) using "" as delimiter
KILLED  [strnull   ] join(",", (CharSequence[]) null) answering ""
KILLED  [strnull   ] join(",", (Iterable) null) answering ""
KILLED  [strnull   ] copyValueOf(null) answering ""
KILLED  [strnull   ] valueOf((char[]) null) answering "null" like its Object overload
KILLED  [strnull   ] new String((char[]) null) answering ""
KILLED  [strnull   ] getChars(0, 1, null, 0) succeeding silently
KILLED  [strnull   ] getChars(0, 9, null, 0) reporting the DST null before the SOURCE range
KILLED  [strnull   ] getChars dst overflow left to the ARRAY's own bounds check
KILLED  [strnull   ] equals reading the ARGUMENT's value array by slot - no instanceof guard
KILLED  [strnull   ] contentEquals aliased to equals (instanceof-guarded)
KILLED  [strnull   ] indexOf(int) MASKING to a code unit
KILLED  [strnull   ] lastIndexOf(int) MASKING to a code unit
KILLED  [strnull   ] a code-POINT-typed scan that cannot see a lone low surrogate
KILLED  [strnull   ] indexOf narrowing to (char) BEFORE the isValidCodePoint gate
KILLED  [strnull   ] a first-hit-wins hybrid that also matches the MASKED low half at 1
KILLED  [strnull   ] fromIndex treated as the pair's END rather than its START

mutants: 40   killed: 40   survived: 0   unusable: 0
```

Four are worth calling out:

* **`toUpperCase((Locale) null)` falling back to the default locale** dies. That
  is not a hypothetical defect — it is precisely the JIT bug E18-1 §1 describes,
  where the interpreted native threw and the compiled path answered the
  default-locale string. E18-1's N1 fixes the Rust side; this row is the fixture
  that would have caught it, and until now nothing called the method with null.
* **The 23 `checkNpe` rows are mutated one at a time**, not as a block. Mutating
  the shared helper would have modelled "one null-guard for all of String",
  which is a *weaker* claim than "each of these 23 methods individually
  throws" — and the whole point of E8-1's finding is that the family is
  **non-uniform**: four of its members must NOT throw.
* **`equals(sb3)` answering TRUE** dies, which is the mutant that matters for
  E18-1 N3: the JIT's `StringEquals` intrinsic inlines a String-layout decode of
  the argument guarded only against null, and `new StringBuilder(3)` really does
  have capacity 3, so the wrong body reads a match out of it.
* **the first-hit-wins hybrid** dies, which is §4's whole argument: it is the
  mutant the dropped N6 row would have caught, and the kept row catches it.

## 8. NOMINATIONS

### N1 — the four source records should be marked SPENT

This lane cannot edit them. Each still reads as an open nomination against a
file that has now consumed it, and E26 §8.5's table still lists all four as
PENDING with stale denominators. One line each:

* `E8-1-string-null-contracts-and-the-third-copy-of-six-constants.md` §7 N1 and
  N2 — **APPLIED by E39-1.** N2's `sectionEnd("strnull", 31)` was **30**.
* `E18-1-the-jit-facing-string-doors-and-the-fourth-copy-of-one-search-rule.md`
  §7 N3 — **APPLIED by E39-1**, in `strnull`. §7 N6 — **APPLIED by E39-1**,
  five of six rows; the supplementary-pair `indexOf` row was dropped (§4).
* `E27-1-the-jit-indexof-int-intrinsic-was-the-fifth-copy.md` — its re-affirmation
  of E18-1 N3/N6 is discharged.
* `E26-1-the-reach-audit-…md` §8.5 — the table is now history, not a to-do list.

E18-1's **N1, N2, N4, N5** and E27-1's Rust-side nominations are **untouched and
still open**; they are not fixture rows and nothing here closes them.

### N2 — `INDEX.md` has no row for E26-1, E27-1 or this record

`INDEX.md` is a self-declared rotting snapshot taken at `HEAD = 7c00dee66`, and
it predates all three. Not this lane's file. Whoever re-takes the listing should
know these are `META`/`SRC`, not defect records.

### N3 — `RJdkIntrinsics2.java`'s class javadoc, again

E26's N4 stands and this lane did not discharge it either, for the same reason:
the header is load-bearing documentation other records cite by its numbers.
This lane changed exactly one word in it — "eleven families" -> "twelve" — because
that sentence describes the *run order*, which the restructure genuinely
changed, and leaving it would have made the header wrong about behaviour rather
than merely stale about size. The rest (W7-95's coverage arithmetic, the
448-check family sizes) is still stale and should be rewritten **once**, by
whoever next re-derives the census denominator.

## 9. What is still not reached, after this

* **`strnull` is a null-ARGUMENT family, not a null-RECEIVER one.** Every row
  passes a null argument to a live receiver. `((String) null).length()` and its
  kin are a different vector.
* **`String.replace(CharSequence, CharSequence)` and the five-argument
  `regionMatches(boolean, …)`** were measured (§2.1) and left unwritten.
* **`indexOf(int, int)` and `indexOf(String, int)` with a negative or oversized
  `fromIndex`** — the clamping rules, unasked. Only `lastIndexOf(int, int)` got
  a `fromIndex` row here.
* **Whether ANY of the 39 new rows is red on CratonVM.** This lane may not run
  the VM. The rows are a vector; the verdict is the next run's.
