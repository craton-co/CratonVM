# E26-1 — The reach audit: what eleven GREEN families were not asking

**2026-08-13, lane E26.** Applies E14's N1, then generalises the finding behind
it to every family in `regression-suite/src/RJdkIntrinsics2.java`.

This lane **owns `regression-suite/src/RJdkIntrinsics2.java`** and this file;
everything else is a NOMINATION (§8).

**This lane may not build or run the VM.** Every value asserted here was
MEASURED on this host against `openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)`
(Microsoft build) from `scratchpad/e26/` (`N1Probe.java`, `ProbeA.java` …
`ProbeE.java`), and the fixture is **green on HotSpot at 951 checks** (§6).
Nothing about CratonVM's behaviour is claimed — this is a vector, not a verdict.

The fixture was and remains **LF-only (0 CRLF)**, counted after every edit.

---

## 0. The premise, and why it generalises

E14 landed **five real Base64 defects** — an NPE on *every* encode call, a wrong
wrap shape, non-singleton factories, a decoder synthetic with one slot where the
JDK has two booleans, and a `linemax=0`-vs-`-1` confusion — in a family whose
fixture read **27/27 GREEN**. Its own explanation:

> every item here is in surface the fixture never reaches — which is exactly why
> they survived a green fixture.

A green count is a statement about the rows that exist, not about the class. So
the question for every family is not "does it pass" but **"what is it not
asking"**, along three axes: *methods*, *object states*, *value domains*.

The audit found the same shape in all eleven, and one family — `strictExact` —
is a textbook instance of the third axis: **thirty-two of its thirty-four rows
drove `MIN_VALUE`, `MAX_VALUE`, `-0.0` or `-1`, and not one drove an ordinary
number.** That is precisely the shape in which `Math.pow`'s special values were
correct while its ordinary-input fast path was 44 ulp wrong.

## 1. Verdict

| | |
|---|---|
| E14's N1 (7 rows, b64 27 -> 34) | **verified independently on HotSpot, then applied** (§2) |
| families audited | 11 of 11 |
| rows added | **496** (455 -> 951) |
| families whose reach was limited by METHOD coverage | 11 of 11 |
| families whose reach was limited by an unbuilt OBJECT STATE | 6 of 11 (`hex`, `b64`, `random`, `bounds`, `strfmt`, `uuid`) |
| families whose reach was limited by VALUE DOMAIN | 3 of 11 (`strictExact`, `floatfmt`, `divmod`) |
| oracle mutants planted | 57 |
| oracle mutants **killed** | **57 (100%)**, 0 survived (§7) |
| NOMINATIONS raised | 4 (§8) |

## 2. TASK 1 — E14's N1, verified before applying

E14 could not run the VM and stated its own rows were measured on HotSpot. They
were re-measured here from a standalone probe (`N1Probe.java`) rather than taken
on report, because a nomination's transcript is still someone else's transcript:

```
mime.length()                                      = 82
getEncoder() == getEncoder()                       = true
getMimeEncoder(0,LF) == getEncoder()               = true
getMimeEncoder(20,LF).encodeToString(big).length() = 83
getEncoder().encode(big,dst)                       = 80
getMimeEncoder().encode(big,dst)                   = 82
getMimeDecoder().decode(mimeBytes,dst)             = 60
getEncoder().wrap(bo) -> bo.size()                 = 80
custom lines = [AAECAwQFBgcICQoLDA0O, DxAREhMUFRYXGBkaGxwd,
                Hh8gISIjJCUmJygpKiss, LS4vMDEyMzQ1Njc4OTo7]
```

All seven agree. **No import collision** (every new type is fully qualified) and
**no identifier collision** (`dst`, `bo`, `os`, `e` are unused in `b64()`; `t` is
already declared and N1 does not redeclare it). Applied verbatim;
`--only=b64` then read `b64=34`, `PASS RJdkIntrinsics2 (34 checks)`.

The four extra lines in the custom-linemax transcript are worth keeping: they are
the **shape** E14's §2 says a length assertion cannot see — four 20-character
lines, not one 76-character line and a tail.

## 3. TASK 2 — the per-family reach table

"Surface" counts the public methods a family's target classes actually declare;
"called" counts the distinct ones the pre-E26 rows invoke.

| family | target | called / surface | OBJECT STATES built (before) | VALUE DOMAIN driven (before) | rows |
|---|---|---|---|---|---|
| `charcls` | `java.lang.Character` | **18 / ~60** | none — all statics; the box cache never built | astral + surrogates + radix bounds; the *derivations* (`getType`) never asked | 86 -> **167** |
| `boolparse` | `Boolean/Byte/Short/Integer/Long` | **~22 / ~90** | boxing caches never built | **SIGNED only**; the same 32 bits read unsigned never asked | 57 -> **115** |
| `floatfmt` | `Float`, `Double` | **~18 / ~45** | boxes built, but only at corners | **41 of 47 rows are a corner**; ordinary decimals almost absent | 47 -> **89** |
| `hex` | `java.util.HexFormat` | **13 / 29** | default + 4 configured — but only ever *formatting* | 4 bytes, one alphabet | 26 -> **73** |
| `b64` | `java.util.Base64` | **~14 / 24** | 6 factories + `withoutPadding`; **no ByteBuffer position state** | 3/5/60-byte inputs; residues 1,2,4 unasked | 34 -> **60** |
| `uuid` | `java.util.UUID` | **9 / 14** | hand-built + random only; **no v1 UUID**, so 3 accessors unreachable | a handful of msb/lsb; the sign boundary unasked | 30 -> **51** |
| `random` | `java.util.Random` | **10 / ~30** | fresh + mid-stream; **the PENDING-GAUSSIAN state never built** | seed 42/0/-1/MAX; `nextBytes` at length 7 only | 34 -> **60** |
| `strfmt` | `java.lang.String`, `Formatter` | **~20 / ~80** | ASCII and surrogate strings; **the LATIN-1-not-ASCII coder never built** | ~14 conversions of ~20 | 60 -> **114** |
| `bounds` | `AtomicReferenceArray`, `CharBuffer` | **~12 / ~50** | int-ctor array; **read-only String buffer only — `put` never called at all** | index corners only | 35 -> **91** |
| `strictExact` | `Math`, `StrictMath` | **~15 / ~70** | n/a | **32 of 34 rows are MIN/MAX/-0.0/-1. ZERO ordinary inputs.** | 34 -> **91** |
| `divmod` | `Math`, `StrictMath`, opcodes | **4 / ~14** | n/a | `(-7,2)` and `MIN/-1`; the *truncating* operators on the same operands never asked | 12 -> **40** |

### 3.1 The three sharpest single findings

**(a) `strictExact` had no ordinary inputs at all.** `StrictMath`'s contract is
that it reproduces fdlibm **bit for bit**, so its answers on ordinary arguments
are fully specified and assertable — and none had ever been asked. Nineteen
bit-exact rows now pin them. The complementary row is the one that catches a
44-ulp fast path without over-constraining a conforming implementation:

```java
check(worst <= 1.0, "every Math transcendental must be within ONE ulp of its
        StrictMath twin over the whole ordinary operand set …");
```

Eight functions across the ordinary operand set — **the measured worst gap on
HotSpot is exactly 1 ulp**, so the budget is tight, not slack, and tightening it
to 0 kills the row (mutant 50). `Math.sqrt` gets a separate bit-exact row
because it has *no* error budget.

**(b) `hex` was missing an entire half of its class.** `HexFormat` is a
formatter *and a parser*; 26 rows called the formatter only. Nothing that can
fail on bad input was reached. The parsing half turns out to raise **four
different exception classes** — odd length is `IllegalArgumentException`, a bad
*digit* is `NumberFormatException`, a bad *range* is
`IndexOutOfBoundsException`, and `null` is `NullPointerException` — so a single
"parse errors throw IAE" rule is wrong three ways.

**(c) `random`'s own comment described a test it did not perform.** The row

```java
check(r11.nextInt() == -1170105035, "setSeed(42) must RESET the stream, including
        the Gaussian cache");
```

draws `nextInt()`, which never touches the Gaussian cache. `[nextGaus]` is a
prior finding in exactly this family. The state is now built and the claim
tested three ways, including a row pinning that `nextGaussian()` consumes
exactly the specified number of underlying draws (the following `nextInt()` is
the stream's *fifth* 32-bit draw, `1325939940`, not its second).

### 3.2 Non-uniform contracts the audit surfaced

The task warned against generalising one rule across a family. The audit found
this is the norm, not the exception, and each is now pinned by an explicit
*pair* of rows rather than one row plus an assumption:

| one family, two answers | rows |
|---|---|
| `Character.charCount(0x110000)` = 2 (never validates) vs `toChars(0x110000)` THROWS | both |
| `Integer.parseInt(null)`/`valueOf(null)` -> `NumberFormatException` vs `decode(null)` -> `NullPointerException` vs `Float.valueOf(null)` -> `NPE` | all four |
| `Character.isWhitespace(TAB)`=true / `isSpaceChar(TAB)`=false, and NBSP exactly inverted | both |
| U+2160: `isUpperCase`=T, `isLetter`=F, `isAlphabetic`=T, `isTitleCase`=F, `getNumericValue`=1, `digit`=-1 | six |
| `Character.toUpperCase(U+00DF)` = U+00DF vs `"ß".toUpperCase()` = `"SS"` | both (cross-family) |
| `Math.round(2.5)`=3 (HALF_UP) vs `Math.rint(2.5)`=2.0 (HALF_EVEN) | both |
| `Math.abs(MIN)` wraps vs `Math.absExact(MIN)` throws | both |
| `-7/2`=-3, `floorDiv`=-4, `ceilDiv`=-3; `-7%2`=-1, `floorMod`=1, `ceilMod`=-1 | all six |
| `CharBuffer`: index -> `IndexOutOfBounds`, position -> `IllegalArgument`, write -> `ReadOnlyBuffer`, over-read -> `BufferUnderflow`, no mark -> `InvalidMark` | all five |
| encoding a lone surrogate -> `'?'` (0x3F) vs decoding bad UTF-8 -> U+FFFD | both |

### 3.3 The Base64 decoder trap, measured rather than assumed

The task flagged that `-_-_` has three different correct answers. Confirmed —
and it is worse than "MIME ignores illegal characters":

```
input     basic decoder              MIME decoder               URL decoder
-_-_      IllegalArgumentException   []  (EMPTY)                [-5, -1, -65]
QQ=       IllegalArgumentException   IllegalArgumentException   IllegalArgumentException
A         IllegalArgumentException   IllegalArgumentException   IllegalArgumentException
QQ==X     IllegalArgumentException   IllegalArgumentException   IllegalArgumentException
QQ\n==    IllegalArgumentException   [65]                       IllegalArgumentException
Q*Q==     IllegalArgumentException   [65]                       IllegalArgumentException
+/+/      [-5, -1, -65]              [-5, -1, -65]              IllegalArgumentException
QQ        [65]                       [65]                       [65]
```

The MIME decoder returning an **empty array** for `-_-_` is the cell a plausible
"MIME is lenient, so it decodes more" model gets backwards. Rows were added for
the cells that *discriminate* (the `-_-_` triple, `+/+/` on the URL decoder,
and the three inputs even the lenient decoder rejects), and **not** for the four
cells where all three agree.

`linemax` in `1..3` — which E14 measured as **hanging** the real JDK — is not
driven by any row. `getMimeEncoder(19, …)` is driven instead, because
`19 >> 2 << 2 == 16` makes it the *rounding* probe, and the pair
`(19 -> 84, 16 -> 84)` is what proves the rounding happened.

## 4. What the audit could NOT close with a row

Recorded so the next lane does not re-derive it:

* **`randomUUID` / `new Random()` entropy** — only structural invariants are
  assertable; already covered.
* **Memory-ordering semantics of `getAcquire`/`setRelease`/`getOpaque`** — a
  single-threaded fixture can only show they read and write the right value,
  which is what the new rows do. Real ordering needs a concurrency vector.
* **Locale-dependent case mapping** (Turkish dotted/dotless I). Deliberately
  NOT added: it would make the family depend on CLDR locale data, which is a
  different question from the one this file asks. §8/N3.
* **`HexFormat.of() == HexFormat.of()`** is *measured* identity, not javadoc'd.
  It is asserted anyway, with the comment saying so, because it catches exactly
  the fabricated-per-call-receiver defect E14 found in the Base64 factories.

## 5. Two bugs the audit found IN ITS OWN NEW ROWS

Both were caught by running against HotSpot before landing, which is the reason
the rule exists:

1. **`AtomicReferenceArray.compareAndExchange` compares by `==`, not
   `equals`.** A first draft passed a runtime-concatenated `String` equal to the
   stored one; the CAS silently failed and the write never landed. The rows were
   rewritten to hold the witness reference, and a **new pair of rows now pins
   the identity semantics explicitly** — an equal-but-distinct expected value
   must FAIL and must not write. That pair became mutant 46.
2. **`CharBuffer.wrap(seq, 1, 3)` was written with the wrong opaque index**
   (`OPAQUE_I[5]` is 2, not 3). Caught by the assertion, not by review.

## 6. The fixture as landed — HotSpot transcript

```
$ javac -encoding UTF-8 -d . RJdkIntrinsics2.java
$ java RJdkIntrinsics2            # step lines elided
CK RJdkIntrinsics2 charcls=167
CK RJdkIntrinsics2 boolparse=115
CK RJdkIntrinsics2 floatfmt=89
CK RJdkIntrinsics2 hex=73
CK RJdkIntrinsics2 b64=60
CK RJdkIntrinsics2 uuid=51
CK RJdkIntrinsics2 random=60
CK RJdkIntrinsics2 strfmt=114
CK RJdkIntrinsics2 bounds=91
CK RJdkIntrinsics2 strictExact=91
CK RJdkIntrinsics2 divmod=40
CK RJdkIntrinsics2 checks=951
PASS RJdkIntrinsics2 (951 checks)
```

Every family also runs alone, which is what `--only=` exists for:

```
CK RJdkIntrinsics2 only=charcls     CK RJdkIntrinsics2 charcls=167     PASS RJdkIntrinsics2 (167 checks)
CK RJdkIntrinsics2 only=boolparse   CK RJdkIntrinsics2 boolparse=115   PASS RJdkIntrinsics2 (115 checks)
CK RJdkIntrinsics2 only=floatfmt    CK RJdkIntrinsics2 floatfmt=89     PASS RJdkIntrinsics2 (89 checks)
CK RJdkIntrinsics2 only=hex         CK RJdkIntrinsics2 hex=73          PASS RJdkIntrinsics2 (73 checks)
CK RJdkIntrinsics2 only=b64         CK RJdkIntrinsics2 b64=60          PASS RJdkIntrinsics2 (60 checks)
CK RJdkIntrinsics2 only=uuid        CK RJdkIntrinsics2 uuid=51         PASS RJdkIntrinsics2 (51 checks)
CK RJdkIntrinsics2 only=random      CK RJdkIntrinsics2 random=60       PASS RJdkIntrinsics2 (60 checks)
CK RJdkIntrinsics2 only=strfmt      CK RJdkIntrinsics2 strfmt=114      PASS RJdkIntrinsics2 (114 checks)
CK RJdkIntrinsics2 only=bounds      CK RJdkIntrinsics2 bounds=91       PASS RJdkIntrinsics2 (91 checks)
CK RJdkIntrinsics2 only=strictExact CK RJdkIntrinsics2 strictExact=91  PASS RJdkIntrinsics2 (91 checks)
CK RJdkIntrinsics2 only=divmod      CK RJdkIntrinsics2 divmod=40       PASS RJdkIntrinsics2 (40 checks)
```

`--list` still prints the eleven family names, unchanged. Harness dialect is
untouched: `CK <Class> <family>=<n>`, `CK <Class> checks=<n>`,
`PASS <Class> (N checks)`, and the per-family `step=` breadcrumbs still precede
every panic-candidate — **fifteen new `step=` lines** were added for the new
panic-candidates in `bounds`, `strictExact` and `divmod` (the unsigned
divisions, `absExact`, `divideExact`, the `ceil*Exact` pair and the int
opcodes at `MIN/-1`).

### 6.1 Two structural properties worth stating

* **Every new row provably EXECUTES.** `sectionEnd` asserts an exact
  denominator, so a row that was skipped, folded away or short-circuited would
  change the count and fail the block. The green run at 951 *is* the
  execution-coverage proof; no separate sweep is needed for it.
* **Floats are still compared as raw bits everywhere**, including all 19 new
  `StrictMath` rows and the new `Random` bit rows. The one exception is
  deliberate and documented: `ulpGap()` compares bit patterns *ordinally*
  because `Math`'s javadoc grants a 1-ulp budget, so a literal would be
  over-strict and `==` would be wrong.

## 7. Mutation transcript — 57 oracle mutants, 57 killed

A row nobody has seen fail is a row nobody has tested. Each mutant replaces an
**expected value** in a new row with the answer a *specific plausibly-wrong
implementation* would produce — not a negated condition, which would only prove
the row runs (§6.1 already proves that). Harness: `scratchpad/e26/mutate.py`;
each mutant is compiled and run with `--only=<family>`.

```
KILLED  [charcls    ] isAlphabetic derived from isLetter
KILLED  [charcls    ] getNumericValue aliased to digit()
KILLED  [charcls    ] toTitleCase aliased to toUpperCase
KILLED  [charcls    ] a surrogate reported UNASSIGNED
KILLED  [charcls    ] isSpaceChar aliased to isWhitespace
KILLED  [charcls    ] toChars range check dropped (charCount's rule generalised)
KILLED  [boolparse  ] unsigned parse saturating instead of wrapping
KILLED  [boolparse  ] decode aliased to parseInt (leading zero ignored)
KILLED  [boolparse  ] one shared null-guard across parseInt/valueOf/decode
KILLED  [boolparse  ] rotate distance not masked
KILLED  [boolparse  ] Long.hashCode as identity rather than the xor-fold
KILLED  [boolparse  ] region parse reporting a NUMBER error for a bad RANGE
KILLED  [floatfmt   ] shortest-round-trip printer stopping one digit early
KILLED  [floatfmt   ] 2/3 printed rounded-up instead of shortest
KILLED  [floatfmt   ] f2d widening routed through a decimal string
KILLED  [floatfmt   ] subnormal printed in the NORMAL hex-float shape
KILLED  [floatfmt   ] max() letting NaN lose
KILLED  [floatfmt   ] Float.sum(-0.0,-0.0) giving positive zero
KILLED  [hex        ] one failure class for every parseHex error
KILLED  [hex        ] fromHexDigits saturating instead of filling the int
KILLED  [hex        ] high and low nibble helpers swapped
KILLED  [hex        ] fromHexDigits(seq,from,to) ignoring its range
KILLED  [hex        ] a delimited formatter parsing undelimited input
KILLED  [b64        ] MIME decoder aliased to the URL decoder
KILLED  [b64        ] lineLength taken literally instead of rounded to a multiple of 4
KILLED  [b64        ] encode(ByteBuffer) reading from index 0 without advancing
KILLED  [b64        ] a 1-byte input encoded without padding
KILLED  [uuid       ] compareTo implemented over the text / as unsigned
KILLED  [uuid       ] nameUUIDFromBytes digest off by one nibble
KILLED  [uuid       ] clockSequence taking the wrong bit field
KILLED  [uuid       ] the v1-only accessors throwing IllegalArgument instead
KILLED  [random     ] nextGaussian consuming one draw instead of the specified four
KILLED  [random     ] nextBytes tail loop running on an exact multiple of 4
KILLED  [random     ] nextExponential from a different transform
KILLED  [random     ] ints() stream reseeding rather than sharing the scalar stream
KILLED  [random     ] setSeed not clearing the pending Gaussian partner
KILLED  [strfmt     ] split keeping trailing empty fields
KILLED  [strfmt     ] String.toUpperCase sharing Character's per-char table
KILLED  [strfmt     ] compareTo normalised to -1/0/1 (a Rust Ord-based body)
KILLED  [strfmt     ] lone surrogate encoded as U+FFFD instead of '?'
KILLED  [strfmt     ] strip() treating every Zs as blank (NBSP wrongly stripped)
KILLED  [strfmt     ] %h of null returning the hash of the string "null"
KILLED  [bounds     ] a POSITION error reported as an INDEX error
KILLED  [bounds     ] read-only write refused with UnsupportedOperation
KILLED  [bounds     ] a relative over-read reported as an index error
KILLED  [bounds     ] compareAndExchange comparing by equals() instead of ==
KILLED  [bounds     ] AtomicReferenceArray.toString as an identity hash
KILLED  [strictExact] StrictMath.pow one ulp off (a platform-libm powf)
KILLED  [strictExact] StrictMath.exp(1.0) answered as the Math.E constant
KILLED  [strictExact] the ulp budget tightened to zero - proves the gap is REAL, not vacuous
KILLED  [strictExact] round implemented as floor(x + 0.5)
KILLED  [strictExact] rint aliased to round (HALF_UP instead of HALF_EVEN)
KILLED  [strictExact] absExact wrapping like abs
KILLED  [divmod     ] idiv aliased to floorDiv
KILLED  [divmod     ] divideUnsigned forwarding to a SIGNED divide
KILLED  [divmod     ] ceilMod sign taken from the dividend
KILLED  [divmod     ] remainderUnsigned forwarding to irem

mutants: 57   killed: 57   survived: 0   unusable: 0
```

Three of these are worth calling out because they are the mutants that would
have been *silently survivable* if the rows had been written the obvious way:

* **`StrictMath.pow` off by ONE ulp** dies. A row written as
  `Math.abs(pow - 1.4142135623730951) < 1e-15` would not.
* **`StrictMath.exp(1.0)` answered as the `Math.E` constant** dies — fdlibm's
  `exp(1)` is one ulp *above* `Math.E`, which is the trap a body that special-
  cases `exp(1)` falls into.
* **the ulp budget tightened to 0** dies, which is the control proving the
  1-ulp assertion is tight rather than slack: the measured worst gap really is
  1 ulp, so the row has no headroom to hide a regression in.

## 8. NOMINATIONS

All four are outside this lane's ownership. None is a fixture row — each needs
a Rust change, a new fixture, or another lane's file.

### N1 — `native-builtins/`: seven contracts these rows now assert that no Rust-side test covers

These are the rows most likely to be red on CratonVM, ranked by how cheaply a
plausible implementation gets them wrong. Each is a **prediction**, not a
measurement — this lane cannot run the VM:

1. `Character.getType` (11 rows) — if there is no general-category table, every
   one fails at once. A VM can pass all 18 pre-E26 predicate rows by hard-coding
   the predicates and still have nothing behind `getType`.
2. `HexFormat`'s **entire parsing half** (~30 rows) — 16 registered triples, and
   E10/W8-C3 record that zero were ever invoked.
3. `StrictMath`'s **bit-exact fdlibm answers** (19 rows) — a body dispatching to
   Rust's `f64::powf`/`exp`/`ln` is the platform libm, not fdlibm, and will
   differ in the last bits.
4. `Integer`/`Long`'s **unsigned** parse/format/divide family (~20 rows).
5. `UUID.compareTo` **signed** semantics, `nameUUIDFromBytes` (MD5), and the
   three version-1 accessors.
6. `Random`'s pending-Gaussian reset, the origin-and-bound overloads and the
   three primitive streams.
7. `CharBuffer`'s read-only state and the five distinct exception classes.

### N2 — `classloading/src/class_manager.rs`: synthetic-JDK surface for the newly-reached methods

Unchanged in spirit from E5's N3 and E14's N2, now wider than Base64. In
synthetic-JDK mode the newly-called methods across `HexFormat`, `UUID`,
`Random`, `CharBuffer` and `AtomicReferenceArray` will be `NoSuchMethodError`
unless declared. **Registering natives is not the fix** — a mode-blind
`registry.register` would shadow correct real-JDK bytecode, which is E14 §4.2's
finding and `[flag != mode drops it]`'s. If synthetic mode needs them they need
runtime-mode-gated declarations.

### N3 — a locale fixture, not a row here

`String.toUpperCase(Locale)` under Turkish (`i` -> U+0130, `I` -> U+0131) is the
sharpest case-mapping divergence in Java and Rust's `to_uppercase` is
locale-INDEPENDENT, so it is a real hazard. It was **deliberately not added
here**: it makes the family depend on CLDR locale data, which is a different
question from "is this intrinsic's semantics right". It belongs in a locale
vector alongside the existing `Locale.GERMANY` formatting rows. Measured for
whoever takes it: `"i".toUpperCase(tr)` = `"İ"`,
`"I".toLowerCase(tr)` = `"ı"`, `"I".toLowerCase(ROOT)` = `"i"`.

### N4 — `regression-suite/src/RJdkIntrinsics2.java` header block

The class javadoc still says the file exists to close W7-95's coverage
arithmetic and describes eleven families sized as they were at 448 checks. It is
now 951 and the selection principle has a second axis (states and value domains,
not only untouched triples). This lane owns the file and could have rewritten
the header, but the header is load-bearing documentation that other records cite
by its numbers; changing it is better done once, by whoever next re-derives the
census denominator, rather than twice.

## 8.5 STALE DENOMINATORS — four other lanes' nominations target this file

Before adding anything, this lane swept every `docs/known-issues/jdk-only/`
record that mentions `RJdkIntrinsics2` (17 files), because "a sibling fixture's
rows were nearly added twice" is a live trap. Result: **no row has ever been
added to this fixture by another lane** — the only applied edit is E17-1 §8.1,
which reworded three failure *messages* in place (checks 52, 43, 47; it
subsumes E7-1's N1). Those rewordings are preserved untouched here.

But **four nominations against this file are PENDING and unapplied**, and every
one of them quotes a `sectionEnd` denominator that this lane has just
invalidated. Whoever applies them must recompute:

| nomination | family | denominator it was written against | denominator NOW |
|---|---|---|---|
| E8-1 §7 N1 — `regionMatches` short-circuit (2 rows) | `bounds` | `35 -> 37` | **91 -> 93** |
| E8-1 §7 N2 — a NEW `strnull` family (~31 rows) | new | `sectionEnd("strnull", 31)`, self-flagged as hand-counted | unchanged, but it must also add `"strnull"` to `FAMILIES` and a `runFamily` arm |
| E18-1 §7 N3 — `contentEquals` vs `equals(StringBuilder)` (2 rows) | `strnull` else `bounds` | "+2", no number | **91 -> 93** if it lands in `bounds` |
| E18-1 §7 N6 — `indexOf(int)` code-point family (6 rows) | `strnull` else `bounds` | "six rows", no number | see the overlap note below |

E27-1 re-affirms E18-1's N3 and N6 as still open and still worth adding.

### 8.5.1 One PARTIAL overlap this lane created — read before applying E18-1 N6

E18-1's N6 asserts that `String.indexOf(int)` does **not** narrow to a code unit
and matches a supplementary code point as a surrogate PAIR. This lane's
`strfmt` additions include:

```java
check("a😀b".indexOf(OPAQUE_CP[9]) == 1,
        "indexOf(int) must find an ASTRAL code point at its CODE UNIT index");
```

which asserts **the same contract** on a different operand. It is not the same
row and it does not collide textually, but N6's `mixed.indexOf(0x10437) == 3` is
now **redundant with it**. The other five N6 rows are NOT redundant and remain
the valuable ones — in particular `"abc".indexOf(0x10061) < 0` (which a masking
implementation fails), the lone-low-surrogate scan, and
`"￿q".indexOf(-1) < 0`, which E18-1 itself flags as "the one that rejects a
plausible wrong fix". **Recommendation: land N6 minus its supplementary-pair
row, or land it whole and delete this lane's row — but not both as written.**

This lane did **not** apply N1/N2/N3/N6. They belong to their authors' scope,
and E8-1's N2 in particular restructures `FAMILIES` and the `runFamily`
dispatch, which is a harness-contract change that should be made deliberately
rather than folded into a reach audit.

## 9. What is still not reached, after this

Stated so the next reach audit starts from a real baseline rather than from a
green count:

* **Concurrency.** Every `AtomicReferenceArray` mode accessor is exercised
  single-threaded. The orderings they name are untested by construction.
* **`Formatter`'s date/time conversions** (`%t*`) — an entire conversion family,
  ~20 more registered triples, untouched. It needs a time-zone-stable design,
  which is `RSimpleDateFormatZone`'s problem, not this file's.
* **`Character`'s remaining ~25 statics** — `UnicodeBlock`, `UnicodeScript`,
  `codePointOf`, the `char[]`-taking overloads.
* **`String`'s remaining surface** — `intern`, `getChars`, `regionMatches`'s
  ignore-case arm, `chars()`/`codePoints()` under `parallel()`.
* **Whether ANY of the 496 new rows is red on CratonVM.** This lane may not run
  the VM. The rows are a vector; the verdict is the next run's.
