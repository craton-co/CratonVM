# W8-C3-1 — the `Intrinsic` census round 2: closing the 40% that was never called

**Status: OPEN — a vector, plus one measured property of the run. `regression-suite/src/RJdkIntrinsics2.java`
is 448 checks over 173 registered `Intrinsic` triples, green on HotSpot
25.0.3+9. It is not registered in `regression-suite/run.sh` — see NOMINATIONS.**

> **RECONCILED 2026-08-12 (lane C18).** The sentence "never yet run against a
> CratonVM binary" was true when this lane filed it and is **no longer true**:
> the orchestrator has since run this second-generation census on a binary.
> The one result that belongs in every record is **MEASURED: no family aborts
> the VM. Every failure in this census is a Java `AssertionError`, not a Rust
> panic.** Per-check counts are deliberately not reproduced here — this lane
> did not run it and will not restate numbers it did not take. Everything
> below still marked PREDICTED stays PREDICTED.

W7-95 measured `NativeKind::Intrinsic` and found 39 divergent triples, six of
them VM-fatal **at that time — none of the six aborts the VM any more; see the
banner above and `W7-95`'s own reconciliation banner**. Its own numbers say the
sample was partial, and it said so:

> **387 of the 645 registered `Intrinsic` triples were never invoked**, and the
> untested set is *not* a random remainder.

This record is the follow-up. It does not report new divergences — this lane
cannot build or run the VM — it reports the **executable instrument** that will,
and the arithmetic that says how much of the hole it closes.

## The coverage arithmetic

All numbers below are recomputed from a `--dump-native-registry` JSON (schema 4)
taken this wave from a hello-world run, so they are the registry as it stands
after the W7-98 / W7-99 repairs, not W7-95's snapshot. The dump is
`scratchpad/p1/reg.json`; the arithmetic is `scratchpad/c3/cover.py` over
`targets.txt` (every triple this vector drives, written out) and `gen1.txt`
(every triple `RJdkIntrinsics` drives).

| | |
|---|---|
| registry rows with `kind == "intrinsic"` | **645** |
| distinct (class, name, descriptor) triples among them | **614** (31 rows are duplicate registrations) |
| invoked by W7-95's probe, per its own per-row `invocations` column | **258** — 40% |
| **never invoked by anything** | **~387** |

The 645 in W7-95 is a count of *rows*. The distinct-triple number is 614, and
this record uses the triple as the unit throughout, because a triple registered
twice is one piece of semantics — and because W7-95's own §2 shows the
duplicate-registration case where the second row is what actually runs.

What the two committed vectors drive, which is the only *repeatable* coverage
the tree has (W7-95's probe was never committed — its own closing paragraph says
so):

| vector | distinct registered `Intrinsic` triples driven |
|---|---|
| `RJdkIntrinsics` (W7-95's vector) | 36 |
| `RJdkIntrinsics2` (this one) | **173** |
| of those 173, not already driven by `RJdkIntrinsics` | **158** |
| union of the two | **194 of 614 — 32%** |
| still uncovered by any committed vector | **420** |

The 15-triple overlap is deliberate and is listed in `cover.py`'s output:
`Character.digit`, `getNumericValue`, `isLetter(C)Z`, `isLetterOrDigit(I)Z`,
`String.codePointAt/codePointCount/offsetByCodePoints/repeat/regionMatches`,
`StrictMath.floorDiv/floorMod(II)`, `Math.floorMod(JJ)J`, and the three
`parse*` triples. Each is re-entered at an operand W7-95 did not use — an astral
`Nd`, a negative index, a widened `int`, a radix outside `MIN_RADIX..MAX_RADIX`
— so it is a second question to the same native, not a second copy of the same
question.

**How much of the 173 is certainly new relative to W7-95's 258**, and not merely
new relative to `RJdkIntrinsics`: W7-95 did not publish the 258-triple list, so
the honest lower bound is the count of triples in the families its residual
table names as *zero-invoked*, plus the two families it named individually.

| n | family | W7-95's residual row |
|---|---|---|
| 16 | `java/util/HexFormat` | "case, delimiters, malformed input" |
| 10 | `java/util/Random` | "seeded streams must be bit-reproducible" |
| 9 | `java/util/UUID` | listed, no note |
| 7 | `java/util/concurrent/atomic/AtomicReferenceArray` | listed |
| 10 | `java/util/Base64` + `Encoder` + `Decoder` | "padding, MIME line breaks, malformed input" |
| 5 | `java/nio/ByteBufferAsCharBuffer{B,L}` | "endianness" |
| 23 | `java/lang/StrictMath` integral / `Exact` forms | "same bodies as the tested `Math` twins — likely green, unverified" |
| 3 | `String.format` ×2 + `formatted` | "a formatter is a grammar, and this record's finding is that grammars are where these natives fail" |
| **83** | **lower bound on genuinely-new-vs-W7-95 triples** | |

The remaining 90 of the 173 are in classes W7-95 sampled but did not exhaust —
`Character` (17 new triples), `Float`/`Double` (18), `Integer`/`Long`/`Byte`/`Short`
(14), `Boolean` (7), `String` (13), `Math` (4), `CharBuffer` (3), the
`StrictMath.floorDiv/floorMod(JJ)J` pair (2).

## The risk model

W7-95's own conclusion is the prior, and it is worth quoting because it is what
selected these 173 out of the 387:

> Any intrinsic that reaches for a Rust standard-library method with a
> plausibly-matching name is a suspect; any intrinsic that is pure IEEE-754
> arithmetic in the interior of its domain has, in this sample, been right.

Five concrete hazards, in descending severity. Every family in the vector is
chosen for at least one of them, and the family order in the vector is this
list read backwards — least likely to abort the VM first.

1. **Integer division and overflow → a Rust panic, which is not a Java
   throwable.** Rust checks division overflow *unconditionally*, release as well
   as debug. `MIN_VALUE / -1` panics; Java's `idiv`/`ldiv` rule (JVMS 6.5) is
   that the quotient overflows and **wraps**. Families `divmod` and the tail of
   `strictExact`.
2. **Array indexing and slicing.** Rust panics on an out-of-bounds index; Java
   throws, and throws a *specific class* — `ArrayIndexOutOfBoundsException` for
   `AtomicReferenceArray`, `StringIndexOutOfBoundsException` for
   `String.codePointAt`, the plain `IndexOutOfBoundsException` for
   `CharBuffer.get` and `HexFormat.formatHex`. Family `bounds`, which asserts
   the **exact** class, because a body that throws the generic superclass for
   all of them is wrong in a way an `instanceof` test cannot see. (Recorded
   convention: `[subcls≠cls]`.)
3. **Anything taking a `String`.** A Rust `str` cannot hold an unpaired UTF-16
   surrogate, and Rust's parsers implement Rust's grammars. Families
   `boolparse`, `floatfmt`, `b64`, `uuid`, and the `chars()` rows of `strfmt`.
4. **Float and double special values** — NaN, ±0.0, ±Infinity, MAX_VALUE,
   MIN_VALUE, subnormals. Family `floatfmt`, compared by
   `doubleToRawLongBits`/`floatToRawIntBits` throughout.
5. **A Rust standard-library equivalent whose edge semantics differ.** Families
   `charcls`, `random`, `hex`, `strfmt`.

### The specific predictions this vector executes

* **W7-95's own named prediction.** "`StrictMath.floorDiv(JJ)J` and
  `StrictMath.floorMod(JJ)J` are registered onto the *same*
  `native_math_floor_div_long` / `native_math_floor_mod_long` bodies that abort
  the VM for `Math`, so they are near-certainly two more VM-fatal triples that
  this probe simply did not call." Family `divmod` calls both, at
  `Long.MIN_VALUE / -1L`, after the family's non-overflowing rows have already
  reported.
  **RESOLVED (C18, 2026-08-12): the prediction did not hold, and that is good
  news. MEASURED — no family aborts the VM. `floorDiv`/`floorMod` at
  `MIN_VALUE / -1` is fixed and verified on a real binary in both classes, and
  every failure this census reports is a Java `AssertionError`.** The
  ordering-so-a-panic-truncates-only-its-own-block design below is retained on
  purpose: it costs nothing and it is what makes that statement checkable.
* **`Math.abs(Integer.MIN_VALUE)` must wrap.** Not covered by W7-95's inference
  that the `Exact` family's `StrictMath` twins are green, because `abs` is not
  in that family: Java specifies the wrap, and Rust's `i32::abs` panics on that
  input under overflow checks. Four rows at the tail of `strictExact`, each
  behind its own step marker.
* **`Boolean.parseBoolean("TRUE")`.** Rust's `str::parse::<bool>()` accepts only
  lowercase `true`/`false`; Java's comparison is case-insensitive. One row that
  separates a Rust-reflex body from a correct one.
* **`Float.floatToIntBits` must CANONICALISE NaN to `0x7fc00000`**, where its
  registered twin `floatToRawIntBits` must not. Rust's `f32::to_bits()` is the
  raw one. Two registered triples, one Rust method, and only one of them can be
  that method. Same pair for `Double.doubleToLongBits`.
* **`Integer.parseInt("0", 1)` must throw `NumberFormatException`.** Rust's
  `i32::from_str_radix` **panics** on a radix outside `2..=36`. This is hazard 1
  and hazard 3 in the same row.
* **A seeded `java.util.Random` is bit-specified.** Every value in family
  `random` is fixed by javadoc — the 48-bit LCG, the seed scramble, the
  rejection loop in `nextInt(bound)`, the power-of-two special case. The
  `nextGaussian` rows are the sharpest: `[nextGaus]` —
  `random-nextgaussian-discarded-the-cached-partner` — is a **prior finding in
  exactly this family**, and the failure mode it records produces a correct
  first value and a wrong second one, so row #2 is the load-bearing one.
* **`Character.isUpperCase(U+2160) == true` while `isLetter(U+2160) == false`.**
  The same code point, two predicates, opposite answers. A body that answers
  either one from `char::is_alphabetic` cannot get both rows right. This is the
  W7-98 mechanism asked at two triples W7-98 did not measure.
* **`UUID.fromString` is LENIENT**, which is the opposite of the expected
  direction: `"1-2-3-4-5"` parses, and a 35-character form with an 11-digit node
  group parses and is re-padded. Any strict canonical-form parser — which is
  what a Rust `uuid` crate is — rejects both.
* **`Base64`'s basic decoder is lenient about missing padding and about
  non-canonical trailing bits, and strict about the wrong alphabet and about
  trailing data.** Five accept-rows and five reject-rows, because a decoder that
  is uniformly strict or uniformly lenient fails one set or the other.

## The vector

`regression-suite/src/RJdkIntrinsics2.java`. Eleven families, 448 checks,
`PASS` on Microsoft OpenJDK 25.0.3+9, byte-identical over three consecutive
runs.

```
CK RJdkIntrinsics2 charcls=86
CK RJdkIntrinsics2 boolparse=57
CK RJdkIntrinsics2 floatfmt=47
CK RJdkIntrinsics2 hex=26
CK RJdkIntrinsics2 b64=27
CK RJdkIntrinsics2 uuid=30
CK RJdkIntrinsics2 random=34
CK RJdkIntrinsics2 strfmt=60
CK RJdkIntrinsics2 bounds-step=AtomicReferenceArray.get(-1)
        ... 13 more bounds-step lines ...
CK RJdkIntrinsics2 bounds=35
CK RJdkIntrinsics2 strictExact-step=Math.abs(Integer.MIN_VALUE)
        ... 3 more strictExact-step lines ...
CK RJdkIntrinsics2 strictExact=34
CK RJdkIntrinsics2 divmod-step=StrictMath.floorDiv(Long.MIN_VALUE, -1L)
        ... 4 more divmod-step lines ...
CK RJdkIntrinsics2 divmod=12
CK RJdkIntrinsics2 checks=448
PASS RJdkIntrinsics2 (448 checks)
```

Everything the vector prints is on a `CK`/`PASS` prefix, so `extract()` drops
nothing (guard G1), 37 lines survive it carrying real observables (G2), and
`checks=448` is published (G3).

### Every expected value was measured, none remembered

The vector was written in two passes. Pass one is `scratchpad/c3/M1.java` +
`M2.java`, which call every method under test and *print* what HotSpot answers;
pass two transcribes those answers into assertions. The full transcripts are
`scratchpad/c3/m1.out` and the `M2` output, and the interesting rows — the ones
a reader is most likely to think are typos — are reproduced here:

```
Boolean.parseBoolean("TRUE")               = true
Character.isUpperCase(U+2160)/isLetter     = true/false
Character.forDigit(-1,16)                  = 0        (U+0000, not a throw)
Character.toUpperCase(int -1)              = -1       (maps to itself)
Float.floatToIntBits(0x7f800001 as float)  = 7fc00000 (canonicalised)
Float.floatToRawIntBits(same)              = 7f800001 (preserved)
Float.toString(1.0f)                       = 1.0      (Rust prints "1")
Float.parseFloat("1.0f")                   = 1.0f     (type suffix is legal)
Integer.parseInt("ｆｆ", 16)       = 255      (fullwidth hex digits)
Integer.toString(255, 1)                   = 255      (bad radix falls back to 10)
Base64.getDecoder().decode("QQ")           = [65]     (padding not required)
Base64.getDecoder().decode("QR==")         = [65]     (trailing bits discarded)
Base64.getDecoder().decode("-_-_")         = throws IllegalArgumentException
UUID.fromString("1-2-3-4-5")               = 00000001-0002-0003-0004-000000000005
UUID.fromString(35-char form)              = 00112233-4455-6677-8899-0aabbccddeef
UUID.fromString("zzzzzzzz-...")            = throws NumberFormatException
new Random(42).nextInt() x3                = -1170105035, 234785527, -1360544799
new Random(42).nextGaussian() x2           = 0x3ff2453e82115d86, 0x3fed6bca38120847
new Random(42).nextBytes(new byte[7])      = [53,-99,65,-70,-9,-118,-2]
new Random(MAX_LONG).nextInt()             = 1155099827   (== the seed -1 case)
String.format(ROOT,"%.0f",2.5)             = 3         (HALF_UP, not HALF_EVEN)
"".lines().count()                         = 0         (zero lines, not one)
"a".indent(0)                              = "a\n"     (not a no-op)
"abc".replace("","-")                      = -a-b-c-
AtomicReferenceArray.get(-1)               = throws ArrayIndexOutOfBoundsException
new AtomicReferenceArray(-1)               = throws NegativeArraySizeException
CharBuffer.get(9)                          = throws IndexOutOfBoundsException
"ab".codePointAt(2)                        = throws StringIndexOutOfBoundsException
StrictMath.floorDiv(Long.MIN_VALUE,-1L)    = -9223372036854775808
StrictMath.floorMod(Long.MIN_VALUE,-1L)    = 0
Math.abs(Integer.MIN_VALUE)                = -2147483648
```

### The vector was mutation-checked, family by family

A vector that cannot fail is not coverage — the standing lesson of
W7-51/W7-60. One mutation per family, each of them the answer a Rust-reflex body
would give, each compiled and run:

```
charcls: red (rc=1)      hex:    red (rc=1)      bounds:      red (rc=1)
boolparse: red (rc=1)    b64:    red (rc=1)      strictExact: red (rc=1)
floatfmt: red (rc=1)     uuid:   red (rc=1)      divmod:      red (rc=1)
                         random: red (rc=1)      strfmt:      red (rc=1)
```

Eleven for eleven. The mutants are regenerable from `scratchpad/c3/cover.py`'s
sibling script; the table of (family, old, new) is in it verbatim.

## How the orchestrator drives it

**A Rust panic truncates the run, so a single-process full run is not enough.**
The vector is built for three modes.

1. **The suite's mode — no arguments.** All eleven families run, ordered
   ascending by how likely each is to abort the VM rather than fail an
   assertion: `charcls boolparse floatfmt hex b64 uuid random strfmt bounds
   strictExact divmod`. Each family prints its own `CK ...=<n>` line as it
   finishes, so a VM that dies in `divmod` has already reported the other ten.
   This is what `run.sh` does, and it needs no hook — the vector takes no
   arguments from the harness.

   ```
   ONLY="RJdkIntrinsics2" bash regression-suite/run.sh
   ```

2. **Per-family isolation — `--only=<family>`.** The only way to learn anything
   about a family whose predecessor kills the process. `run.sh` passes no
   program arguments, so this is a direct invocation:

   ```
   for f in charcls boolparse floatfmt hex b64 uuid random strfmt \
            bounds strictExact divmod; do
     echo "== $f"
     "$CV" --java-home "$JDK" -cp regression-suite/build RJdkIntrinsics2 --only=$f
     echo "rc=$?"
   done
   ```

   Eleven processes, eleven independent verdicts. `--list` prints the family
   names so the loop can be generated rather than transcribed. Run this **first**
   on any binary that has not seen the vector before; the aggregate run is only
   informative once no family aborts.

3. **Sub-family localisation — the `step` lines.** `bounds`, `strictExact` and
   `divmod` print `CK RJdkIntrinsics2 <family>-step=<call>` *before* each call
   whose hazard is a panic rather than a wrong answer. On a VM that aborts, the
   last line on stdout **names the call that killed it** — 14 markers in
   `bounds`, 4 in `strictExact`, 5 in `divmod`. On a correct VM the sequence is
   deterministic and diffs clean against the oracle, so the markers cost the
   cross-VM comparison nothing.

Expected first result, stated in advance so it is a prediction and not a
post-hoc reading: `--only=divmod` should abort with
`attempt to divide with overflow` at
`CK RJdkIntrinsics2 divmod-step=StrictMath.floorDiv(Long.MIN_VALUE, -1L)`,
**unless** W7-99's `wrapping_div`/`wrapping_rem` repair has landed and been
rebuilt, in which case that family is the gate proving it.

## Residuals — what is still uncovered, and why

**420 of the 614 distinct triples** are driven by neither committed vector. The
list below is the whole remainder, grouped by why it was not taken.

**Covered by W7-95's probe but by no committed vector (re-derivable, not
re-measured).** The largest block, and the cheapest to close: `java/lang/Math`
and `java/lang/StrictMath`'s double-precision forms — 58 and 40 triples — which
W7-95 exercised over 259 rows and found correct except for `pow` and `ulp`.
`RJdkStrictMath` already covers part of this ground from a different angle. A
third-generation vector here is transcription, not investigation.

**Not taken, needs its own lane.**

| n | class | why it needs a lane rather than a block in this file |
|---|---|---|
| 17 | `java/security/SecureRandom` | a security primitive; the interesting properties are distributional and seeding-related, not value-equality, so it needs a different kind of assertion than this file's |
| 19 | `java/math/BigDecimal` | rounding modes × scale × `equals`-vs-`compareTo` is a matrix, not a list |
| 18 | `java/math/BigInteger` | sign/magnitude edges and the four `impl*`/`*Worker` array kernels, which are `jdk.internal`-shaped and only reachable through arithmetic large enough to select them |
| 19 | `java/util/logging/LogRecord` | a mutable bean; the risk is state, and this file tests values |
| 12 | `java/net/InetSocketAddress` | resolution behaviour is host-dependent; needs `createUnresolved` discipline throughout |
| 9 | `java/util/Formatter` | the `Formatter` object itself, as distinct from `String.format` — `out()`, `locale()`, `close()`, `flush()` are lifecycle, not grammar |
| 8 | `java/util/regex/Matcher` + 2 `Pattern` + 3 `Scanner` | a regex engine deserves its own differential |
| 11 | `java/util/Objects` | W7-95 measured the whole family and it agreed; lowest priority in the tree |
| 6 | `jdk/internal/util/ArraysSupport` | **not directly callable** — see below |
| 5 | `java/lang/StringLatin1` | **not directly callable** — see below |
| 6+5 | `java/lang/ThreadLocal`, `InheritableThreadLocal` | value-per-thread, so the interesting question is concurrent and this file is single-threaded |
| 5 | `jdk/internal/util/ClassFileDumper` | writes files; a side-effect surface |
| ~60 | `org/h2/*`, `org/springframework/*`, `sun/security/*` app shims | app-specific, and each needs its app's fixture to reach |

**Structurally unreachable from ordinary Java source, and why.**

* `jdk/internal/util/ArraysSupport.{mismatch,vectorizedMismatch,vectorizedHashCode}`
  and `java/lang/StringLatin1.{compareTo,getChar,inflate,toLowerCase}` are
  package-private or in a non-exported module. They *are* reachable
  **indirectly** — `Arrays.mismatch`, `Arrays.equals`, `String.compareTo`,
  `String.hashCode` route through them — but a check written that way cannot
  prove *which* body answered, so it measures its own reach rather than the
  triple. (`[reach≠defect]`.) The honest instrument for these eleven is a
  registry dump before and after, comparing the `invocations` column — not an
  assertion. `RArraysMismatch` already exercises the `Arrays` surface.
* `Math.random()` / `StrictMath.random()` — nondeterministic by construction,
  excluded from a differential. W7-95 said the same. They need a distributional
  test.
* `java/util/Random.<init>()V` — the unseeded constructor, same reason. The
  seeded one is covered.
* `AtomicReferenceArray.{weakCompareAndSet,weakCompareAndSetPlain}` — specified
  to be allowed to fail spuriously, so no single-shot assertion is sound. Their
  risk is memory semantics, which is a concurrency question.
* `java/util/concurrent/ScheduledExecutorService.{isShutdown,shutdown}`,
  `java/util/Iterator.remove`, `java/io/PrintStream.charset`,
  `java/util/EnumMap.<init>`, `java/text/DateFormat.format` — reachable, but each
  is one triple in a class whose other members are not registered, so they
  belong with whatever vector already owns that surface.
* **Every `Intrinsic` under concurrency.** This vector, like W7-95's, tests
  values on one thread. Nothing in the tree tests any of these natives under
  contention.

**Also untested inside families this vector *did* cover:** `HexFormat.parseHex`
and `fromHexDigits` (not registered here, but the same body will grow them),
`Base64.Decoder.decode([B)[B` and the `wrap`/`decode(ByteBuffer)` forms,
`Character.isEmoji*` beyond the six code points sampled across both vectors, and
`String.format`'s `%t` date conversions.

## NOMINATIONS

Lane C3 owns `regression-suite/src/RJdkIntrinsics2.java` and this file. Every
item below is an exact edit for someone who owns the file it names.

### N1 — register the vector — **REQUIRED, NOT OPTIONAL**

`regression-suite/run.sh:106`, append `RJdkIntrinsics2` to **`CORE_CLASSES`**,
immediately after `RJdkIntrinsics`. Not `JDKONLY_CLASSES`: these are language
semantics, identical in `--real-jdk` and `--jdk-only`, and the natives are
registered in both arms — the same reasoning W7-95's N7 gave for its own vector,
which has since landed.

old (end of the line):

```
 ... RJdkViews RJdkFormatLocale RJdkStrictMath RJdkByteOrder RJdkIntrinsics"
```

new:

```
 ... RJdkViews RJdkFormatLocale RJdkStrictMath RJdkByteOrder RJdkIntrinsics RJdkIntrinsics2"
```

Until this lands, `regression-suite/src/RJdkIntrinsics2.java` is in no class
list, which `run.sh`'s coverage gate reports as a WARNING by default and as a
**failure under `STRICT_COVERAGE=1`, which is what CI runs**. The only
alternative is to park it in `UNREGISTERED_CLASSES` (line 157) with a reason.
Do not leave it in neither list.

No `class_args` or `class_cv_args` hook is needed: the vector takes no launcher
flags and no program arguments in its suite mode.

### N2 — run the eleven families in isolation before trusting the aggregate

Not a file edit; a run. See "How the orchestrator drives it", mode 2. A single
aggregate run of a VM that still panics reports the eight families before
`bounds` and nothing else, and a reader who sees eight green `CK` lines and no
`PASS` has been told almost nothing. The per-family loop is the first
measurement, not the fallback.

### N3 — the two triples W7-95 predicted are now executable

`StrictMath.floorDiv(JJ)J` and `StrictMath.floorMod(JJ)J`. If W7-99's
`wrapping_div`/`wrapping_rem` edit in `native-builtins/src/lang_math.rs` has
landed, `--only=divmod` is its gate and should be run against the rebuilt
binary before W7-99 is closed. If it has not, `--only=divmod` is the
reproduction. Either way the two triples W7-95 could only predict now have a
committed vector, which is what closes that half of its residual list.

### N4 — extend the registry-dump instrument to answer the coverage question directly

W7-95's N8 asked for `--jdk-only-report` to be able to name an `Intrinsic`. This
record adds a cheaper, adjacent ask, because the arithmetic at the top of this
file had to be assembled by hand from a target list a human wrote:

> Run any vector with `--dump-native-registry`, and the `invocations` column
> already says exactly which triples it drove. A ten-line diff of two dumps —
> before and after a vector — is the coverage number, computed rather than
> claimed.

Wiring that into `run.sh` as an optional mode (`CENSUS=1 bash run.sh`) would
turn "173 triples" from a hand-written list into a measurement, and would make
the eleven indirectly-reachable `ArraysSupport` / `StringLatin1` triples
answerable at all. Until it exists, `scratchpad/c3/targets.txt` is a list
somebody has to keep true by hand — which is the same species of defect as the
one W7-95's N8 names.
