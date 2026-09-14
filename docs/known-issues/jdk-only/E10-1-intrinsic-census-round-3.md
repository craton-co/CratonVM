# E10-1 — the `Intrinsic` census round 3: 292 of the last 420, and a `SimpleDateFormat` memo collision

**Status: OPEN — two vectors, both green on Microsoft OpenJDK 25.0.3+9, neither yet
run against a CratonVM binary.**

1. `regression-suite/src/RJdkIntrinsics3.java` — NEW. **1,011 checks over 16 families,
   driving 292 registered `Intrinsic` triples that no committed vector drove.** Not
   registered in `regression-suite/run.sh`; see N1, which is REQUIRED.
2. `regression-suite/src/RSimpleDateFormatZone.java` — a 7th family `memo`, applying
   lane E1's N2. **109 → 115 checks.** That file is already in `CORE_CLASSES`, so this
   half needs no `run.sh` edit.

Everything this record reports about HotSpot is measured. Nothing in it is a claim
about CratonVM: this lane cannot build or run the VM, and does not.

---

# Part 1 — `RJdkIntrinsics3`

## 1. The coverage arithmetic

Recomputed from the same schema-4 `--dump-native-registry` dump W8-C3-1 used
(`scratchpad/p1/reg.json`), in the same unit — the distinct triple. The script is
`scratchpad/e10/cover3.py` and it **computes** the residual rather than restating a
hand-kept list: generation 3's target set is derived as *residual minus an explicitly
excluded set*, and every exclusion carries its reason in the source of the script.

| | |
|---|---|
| registry rows with `kind == "intrinsic"` | 645 |
| distinct `(class, name, descriptor)` triples | **614** (31 rows are duplicate registrations) |
| `RJdkIntrinsics` (generation 1) | 36 |
| `RJdkIntrinsics2` (generation 2) | 173 |
| union of generations 1 and 2 | 194 — **32%** |
| driven by neither | **420** |

| | |
|---|---|
| **`RJdkIntrinsics3` (generation 3)** | **292 — 70% of the 420** |
| **union of all three vectors** | **486 of 614 — 79%** |
| still driven by no committed vector | **128** |

The 292 are new by construction, not by estimate: the set is computed as a difference
against the two committed vectors' own target lists, so a triple either appears in
`scratchpad/e10/gen3.txt` or it does not.

## 2. The risk model, and what selected these families

W8-C3-1's five hazards, in descending severity. The family order in the vector is that
list read backwards, so a VM that dies in the most dangerous family has already reported
the fifteen before it.

1. **Integer overflow and division → a Rust panic, which is not a Java throwable.** Rust
   checks division overflow unconditionally and `checked_*().unwrap()` aborts; Java
   specifies `ArithmeticException` with a fixed message, and specifies that the plain
   operators WRAP. Family `mathexact`, plus the `setScale` rows of `bigdec`.
2. **Array indexing and slicing.** Family `bufslice`, plus the `group(int)` rows of
   `regex`. Every such row asserts the **exact class name**, never `instanceof` —
   the three failures on one `CharBuffer` are three *different* classes
   (`BufferOverflowException`, `BufferUnderflowException`, `IndexOutOfBoundsException`),
   and a body that funnels them into one superclass is wrong in a way an `instanceof`
   test cannot see. (`[subcls≠cls]`.)
3. **Anything taking a `String`.** The `toString` rows throughout `boxid`, `bigdec`,
   `bigint`, and the lone-surrogate rows in `boxid` and `misc`.
4. **Float and double special values.** Families `strictd`, `mathd`, `boxconv`.
5. **A Rust standard-library equivalent with different edge semantics.** Families
   `objects`, `boxid`, `bitops`, `tlocal`.

### The "do not test only the corners" constraint, and how it is met

The standing lesson is that a sampled special-value census reported `java.lang.Math`
correct while `Math.pow`'s fast path was **44 ulp wrong on ordinary inputs**. A
special-value census and an interior census are different questions, so every
transcendental in `strictd` and `mathd` is driven at BOTH — the specified special values
AND a spread of ordinary arguments (`0.1`, `0.7`, `PI`, `E`, `123456.789`, `1e17`,
`1.0000000000000002`).

That forced a comparator decision. `java.lang.Math`'s transcendentals are specified only
to within **1 ulp** (2 for `pow` and `atan2`), so asserting their exact bits would assert
more than the spec says and could fail a conforming VM. Those rows therefore use
`ckU`/`ckUF`, which compare the bit patterns **as a total order** and report the ULP
DISTANCE — tight enough to catch 44 ulp, loose enough to be legitimate. A special-value
class mismatch (NaN vs finite, infinite vs finite) is never inside the tolerance, because
those *are* exactly specified.

`java.lang.StrictMath` has no such licence — it is bit-exact by contract — so `strictd`
uses exact raw-bit assertions throughout and is the sharper of the two families. This
asymmetry is the point of having both: 39 triples asked exactly, and their 40 twins asked
to the letter of a looser spec.

## 3. The families

| # | family | checks | triples | hazard |
|---|---|---|---|---|
| 1 | `objects` | 31 | 11 | 5 |
| 2 | `boxid` | 69 | 28 | 3, 5 |
| 3 | `boxconv` | 58 | 29 | 4, 5 |
| 4 | `bitops` | 33 | 10 | 5 |
| 5 | `strictd` | 245 | 39 | 4 |
| 6 | `mathd` | 211 | 40 | 4 |
| 7 | `bigdec` | 56 | 19 | 1, 3 |
| 8 | `bigint` | 38 | 18 | 3 |
| 9 | `logrec` | 35 | 23 | state |
| 10 | `tlocal` | 26 | 11 | 5 |
| 11 | `fmtobj` | 22 | 9 | state |
| 12 | `inet` | 34 | 13 | 3 |
| 13 | `regex` | 42 | 13 | 2, 3 |
| 14 | `misc` | 21 | 5 | mixed |
| 15 | `bufslice` | 33 | 7 | **2** |
| 16 | `mathexact` | 57 | 17 | **1** |
| | **total** | **1011** | **292** | |

Four families are ones W8-C3-1 explicitly deferred to "their own lane". Each is taken
here for a stated reason rather than in defiance of that judgement:

* **`bigdec` / `bigint`** — "a matrix, not a list" is true of an *exhaustive* treatment.
  It is not a reason for 37 registered natives to have **zero** committed callers. These
  blocks take the edges, not the matrix.
* **`logrec`** — "the risk is state, and this file tests values" is the reason to test it.
  A native-backed getter whose setter writes elsewhere is invisible to a value-only
  census (`[nat hidden]`), so every getter here is asked **twice**: once for its
  constructed default, once after its setter.
* **`tlocal`** — the memory semantics do need a concurrent vector, but the per-thread
  STORAGE contract is deterministic with one child thread and a `join`, and it is exactly
  the half a process-global map gets wrong (`[vmscope]`).
* **`inet`** — "host-dependent" is closed mechanically: **nothing in the family performs
  a DNS lookup.** Every address is `createUnresolved`, a numeric literal, the wildcard, or
  the loopback constant, so the expected strings reproduce on a machine with no network.

## 4. Every expected value was measured, none remembered

The fixture ships a **`--measure` mode**. It prints every observable the file asserts, as
a Java literal, on a `CK`-prefixed line. The committed expectations were produced by
running exactly that mode on HotSpot 25.0.3+9 and substituting its output mechanically
(`scratchpad/e10/subst.py`, 995 substitutions). **No expected value in the file was typed
by a human**, and the substitution script:

* refuses to run if a label is ambiguous or occurs more than once;
* type-checks each literal against the sentinel it replaces;
* **deletes the sentinel declarations afterwards**, so a value it failed to substitute is
  a compile error rather than a check that silently asserts zero.

Re-derive on any JDK with:

```
java -cp regression-suite/build RJdkIntrinsics3 --measure
```

That mode is not merely a build convenience; it is the standing answer to "how do you
know none of these came from memory".

**It also caught this lane in the act.** A draft comment on `StrictMath.copySign` asserted
that it takes the raw sign bit, "so a negative NaN is a NEGATIVE sign". The `--measure`
run answered `0x3ff0000000000000` — **+1.0**. `StrictMath.copySign` *requires* a NaN sign
argument to be treated as positive; `Math.copySign` is explicitly relieved of that
requirement for performance. The comment was wrong, the measurement was right, and the
committed file now says so and asserts the value only on the twin that has no licence to
differ. (`mathd` asserts only the *magnitude* on the same input.)

### The rows a reader is most likely to think are typos

All measured; none remembered.

```
Objects.hash()                              = 1
Objects.hash((Object[]) null)               = 0        (the same method, the other answer)
Objects.hash(new Object[]{null})            = 31
Integer.valueOf(5).equals(Long.valueOf(5))  = false    (equals is TYPE-sensitive)
Character.isWhitespace(0x00a0 NBSP)         = false    (Rust says true)
Character.isWhitespace(0x001c FILE SEP)     = true     (Rust says false)
Character.valueOf(0xd800).toString()        = a 1-char String holding a LONE SURROGATE
Float.valueOf(NaN).hashCode()               = 2143289344  (0x7fc00000, canonicalised)
Integer.compare(Integer.MIN_VALUE, 1)       = -1       (naive a-b overflows to positive)
Double.compare(-0.0, 0.0)                   = -1
Double.valueOf(+Inf).intValue()             = 2147483647  (SATURATING)
BigDecimal("1E+20").intValue()              = 1661992960  (NARROWING, the opposite rule)
BigInteger.ONE.shiftLeft(100).intValue()    = 0        (narrowing again)
StrictMath.rint(-2.5)                       = -2.0     (HALF-EVEN; Rust's round gives -3.0)
StrictMath.ceil(-0.5)                       = -0.0     (raw bits 0x8000000000000000)
StrictMath.IEEEremainder(5.0, 3.0)          = -1.0     (where 5.0 % 3.0 is 2.0)
StrictMath.copySign(1.0, -NaN)              = +1.0     (NaN sign is POSITIVE here)
StrictMath.hypot(+Inf, NaN)                 = +Infinity (Inf BEATS NaN)
StrictMath.atan2(0.0, -0.0)                 = PI
Math.round(0.49999999999999994)             = 0        (not 1; floor(x+0.5) is wrong)
Math.round(0.49999997f)                     = 0
new BigDecimal(0.1).toString()              = 0.1000000000000000055511151231257827021181583404541015625
BigDecimal.valueOf(0.1).toString()          = 0.1      (the SAME class, the other conversion)
new BigDecimal("2.5").setScale(0)           = throws ArithmeticException
new BigDecimal("2.5").setScale(0, 4)        = 3        (HALF_UP)
new BigDecimal("2.5").setScale(0, 6)        = 2        (HALF_EVEN)
new BigDecimal("1E+400").doubleValue()      = +Infinity
InetSocketAddress.createUnresolved(...)     = "example.invalid/<unresolved>:8080"
new InetSocketAddress(65536)                = throws IllegalArgumentException
Matcher.group(99)                           = throws IndexOutOfBoundsException
Matcher.start(2) on a group that did not match = -1
CharBuffer view .get() at the limit         = throws BufferUnderflowException
CharBuffer view .charAt(-1)                 = throws IndexOutOfBoundsException
CharBuffer.put(char) past the limit         = throws BufferOverflowException
Math.addExact(MAX_VALUE, 1)                 = throws ArithmeticException: "integer overflow"
Math.toIntExact(2147483648L)                = throws ArithmeticException: "integer overflow"
new EnumMap(RoundingMode.class) iterates    = {CEILING=c, HALF_EVEN=he}   (ORDINAL order)
Formatter.out() after close()               = throws FormatterClosedException
```

## 5. HotSpot transcript

`javac` clean; the run is **byte-identical over three consecutive runs** (md5
`f2fdd119ae8937bf9fcb5a2cac56b33e`), and **every line printed is on a `CK` or `PASS`
prefix** — 0 lines survive `grep -vE '^(CK|PASS) '`, so `extract()` drops nothing (G1),
50 lines carry real observables (G2), and `checks=1011` is published (G3).

```
CK RJdkIntrinsics3 objects=31
CK RJdkIntrinsics3 boxid=69
CK RJdkIntrinsics3 boxconv=58
CK RJdkIntrinsics3 bitops=33
CK RJdkIntrinsics3 strictd=245
CK RJdkIntrinsics3 mathd=211
CK RJdkIntrinsics3 bigdec=56
CK RJdkIntrinsics3 bigint=38
CK RJdkIntrinsics3 logrec=35
CK RJdkIntrinsics3 tlocal=26
CK RJdkIntrinsics3 fmtobj=22
CK RJdkIntrinsics3 inet=34
CK RJdkIntrinsics3 regex=42
CK RJdkIntrinsics3 misc=21
CK RJdkIntrinsics3 bufslice-step=ByteBufferAsCharBufferB.get()
        ... 17 more bufslice-step lines ...
CK RJdkIntrinsics3 bufslice=33
CK RJdkIntrinsics3 mathexact-step=Math.addExact(Integer.MAX_VALUE, 1)
        ... 16 more mathexact-step lines ...
CK RJdkIntrinsics3 mathexact=57
CK RJdkIntrinsics3 checks=1011
PASS RJdkIntrinsics3 (1011 checks)
```

**All sixteen families also pass in SIXTEEN SEPARATE PROCESSES**, and the isolated check
counts sum to exactly 1011 — so no family depends on another's side effects, which is the
property that makes the per-family loop in §8 a sound substitute for the aggregate run.

```
objects:rc=0   boxid:rc=0    boxconv:rc=0  bitops:rc=0
strictd:rc=0   mathd:rc=0    bigdec:rc=0   bigint:rc=0
logrec:rc=0    tlocal:rc=0   fmtobj:rc=0   inet:rc=0
regex:rc=0     misc:rc=0     bufslice:rc=0 mathexact:rc=0
```

### No `invokedynamic` from a lambda

The vector originally used lambdas for three `Thread` bodies and two `Supplier`s. They are
gone, replaced by named/anonymous classes: `javap` reports the only remaining
`invokedynamic` bootstraps in the fixture are `StringConcatFactory.makeConcatWithConstants`
(27 of them, the unavoidable cost of building assertion messages — `RJdkIntrinsics2` has
59). **A census vector for the intrinsics must not be able to red a family because
`LambdaMetafactory` is weak on the VM under test**, because that failure would be
indistinguishable from the defect it is hunting.

## 6. The vector was mutation-checked, family by family

A vector that cannot fail is not coverage. One mutation per family, each of them the
answer a plausible Rust-backed body would give, each compiled and run. The table of
(family, old, new, why) is `scratchpad/e10/mut3.py` verbatim; it refuses to emit a mutant
whose target text is not unique.

**Sixteen for sixteen, all red (rc=1):**

```
objects   rc=1  hash((Object[]) null): expected 1, got 0
boxid     rc=1  Character.isWhitespace(0x00a0 NBSP): expected true, got false
boxconv   rc=1  Integer.compare(MIN,1): expected 1, got -1
bitops    rc=1  Integer.numberOfLeadingZeros(0): expected 64, got 32
strictd   rc=1  rint(-2.5): expected raw bits 0xc008000000000000 (-3.0), got 0xc000...
mathd     rc=1  round(-2.5): expected -3, got -2
bigdec    rc=1  new BigDecimal(0.1).toString: expected "0.1", got "0.100000000000..."
bigint    rc=1  2^100 intValue: expected 2147483647, got 0
logrec    rc=1  getResourceBundleName() after set: expected null, got "some.bundle.Name"
tlocal    rc=1  ITL child inherited: expected "parent-init", got "from-parent"
fmtobj    rc=1  toString() after close: expected none, got java.util.FormatterClosedException
inet      rc=1  createUnresolved toString: expected "example.invalid:8080", got "...<unresolved>..."
regex     rc=1  unmatched start(2): expected 0, got -1
misc      rc=1  PrintStream(UTF-16BE).charset().name(): expected "UTF-8", got "UTF-16BE"
bufslice  rc=1  put(char) past the limit: expected java.lang.IndexOutOfBoundsException, got
                java.nio.BufferOverflowException
mathexact rc=1  negateExact(MIN) throws: expected none, got java.lang.ArithmeticException
```

Each mutant is a specific wrong theory, not a random edit: `bitops` mutates to the answer
of a body that widens the `int` to `u64` before counting; `bufslice` to a body that
funnels every bounds failure into one class; `mathexact` to a body that WRAPS on overflow,
which is what release-mode Rust integer negation does; `tlocal` to a process-global map
that never inherits into the child.

## 7. Residuals — the 128 nobody drives, and why

Computed, grouped by reason, by `cover3.py`.

| n | what | why not taken |
|---|---|---|
| **84** | `org/h2/*` (74, of which 24 are the eight `Token$*` subclasses), `org/springframework/*` (6), `sun/security/*` (3), `org/junit/*` (1) | app shims; each is reachable only through its own application's fixture |
| 17 | `java/security/SecureRandom` | a security primitive: the interesting properties are distributional and seeding-related, so value-equality is the wrong instrument |
| 6 | `jdk/internal/util/ArraysSupport` | package-private in a non-exported module — reachable only INDIRECTLY, so a check measures its own reach (`[reach≠defect]`) |
| 5 | `java/lang/StringLatin1` | same |
| 5 | `jdk/internal/util/ClassFileDumper` | writes files; a side-effect surface |
| 3 | `jdk/internal/reflect/Reflection` | `registerFieldsToFilter` mutates process-global filter state |
| 2 | `ScheduledExecutorService.{shutdown,isShutdown}` | a concurrency question |
| 2 | `AtomicReferenceArray.weakCompareAndSet*` | SPECIFIED to be allowed to fail spuriously, so no single-shot assertion is sound |
| 2 | `Math.random()`, `StrictMath.random()` | nondeterministic by construction |
| 1 | `java/util/Random.<init>()V` | the UNSEEDED constructor; the seeded one is generation 2 |
| 1 | `String.<init>(AbstractStringBuilder, Void)` | package-private; ordinary Java source cannot name the parameter type |
| **128** | | |

**Two observations for whoever takes the next round.**

* **The reachable-by-a-vector surface is now essentially closed.** Of the 128, only the
  84 app shims and the 17 `SecureRandom` triples are reachable from ordinary Java at all,
  and both need a different *kind* of instrument (an app fixture; a distributional test)
  rather than more rows. Set those two blocks aside and the denominator is 513, of which **486 are driven —
  95%**; the 27 that remain are the structurally unreachable and the nondeterministic.
* **W8-C3-1's N4 is now the binding constraint, not a nicety.** The eleven indirectly
  reachable `ArraysSupport`/`StringLatin1` triples and the five `BigInteger` array kernels
  cannot be settled by an assertion at all. The only sound instrument is a registry dump
  before and after a vector, diffing the `invocations` column. `bigint`'s kernel rows are
  written as round trips and invariants precisely because they cannot prove *which* body
  answered, and this record will not pretend otherwise.

**Also still untested inside families this vector DID cover:** `Math`/`StrictMath` over
the whole ordinary domain rather than a spread of eight arguments (an interior sweep is a
different instrument again); `BigDecimal`'s rounding-mode × scale matrix; `Matcher`'s
named groups and region API; every one of these 292 natives **under concurrency** —
this vector, like both before it, tests values on one thread.

## 8. How the orchestrator drives `RJdkIntrinsics3`

**A Rust panic truncates the run, so a single-process full run is not the first
measurement.** Run the per-family loop FIRST on any binary that has not seen this vector.

1. **Per-family isolation — run this first.** Sixteen processes, sixteen independent
   verdicts. `--list` prints the names so the loop can be generated rather than
   transcribed.

   ```
   for f in $("$CV" --java-home "$JDK" -cp regression-suite/build RJdkIntrinsics3 --list \
              | sed 's/.*family=//'); do
     echo "== $f"
     "$CV" --java-home "$JDK" -cp regression-suite/build RJdkIntrinsics3 --only=$f
     echo "rc=$?"
   done
   ```

   Measured on HotSpot: all sixteen rc=0, and the isolated counts sum to the aggregate
   1011 — so on the VM under test, a family that reds or aborts is a finding about that
   family and not an artefact of ordering.

2. **The suite's mode — no arguments.** All sixteen families in ascending order of how
   likely each is to abort the VM. Informative **only once no family aborts**.

   ```
   ONLY="RJdkIntrinsics3" bash regression-suite/run.sh
   ```

3. **Sub-family localisation — the `step` lines.** `bufslice` (16 markers) and
   `mathexact` (18) print `CK RJdkIntrinsics3 <family>-step=<call>` *before* each call
   whose hazard is a panic rather than a wrong answer. On a VM that aborts, **the last
   line on stdout names the call that killed it.** On a correct VM the sequence is
   deterministic and diffs clean against the oracle, so the markers cost the cross-VM
   comparison nothing.

4. **Re-deriving the expectations** — `--measure`, per §4. Needed only when the oracle
   JDK changes.

**Read the ulp rows correctly.** A `ckU` failure reports the ULP DISTANCE. A distance of
1 (or 2 on `pow`/`atan2`) is *not* reported at all, because that is inside the spec. Any
number it does print is a real divergence, and a large one on an ORDINARY argument is the
`Math.pow` 44-ulp shape recurring.

## 9. NOMINATIONS

This lane owns `regression-suite/src/RJdkIntrinsics3.java`,
`regression-suite/src/RSimpleDateFormatZone.java`, and this file. Every item below is an
exact edit for someone who owns the file it names. Each OLD block was counted against the
working tree and occurs **exactly once**.

### N1 — register the vector — REQUIRED, NOT OPTIONAL

`regression-suite/run.sh:164`, append `RJdkIntrinsics3` to the end of **`CORE_CLASSES`**.
Not `JDKONLY_CLASSES`: these are language and class-library semantics, identical in
`--real-jdk` and `--jdk-only`, and the natives are registered in both arms — the same
reasoning that put `RJdkIntrinsics` and `RJdkIntrinsics2` there.

REPLACE (the end of line 164):

```
 RJdkStringCodePoints RFsSingleton RJdkOptionalShape RSimpleDateFormatZone"
```

WITH:

```
 RJdkStringCodePoints RFsSingleton RJdkOptionalShape RSimpleDateFormatZone RJdkIntrinsics3"
```

Until this lands, `RJdkIntrinsics3.java` is in no class list, which `run.sh`'s coverage
gate reports as a WARNING by default and as a **failure under `STRICT_COVERAGE=1`, which
is what CI runs**. The only alternative is `UNREGISTERED_CLASSES` (line 243) with a
reason. Do not leave it in neither list.

**No `class_args` or `class_cv_args` hook is needed.** The vector takes no launcher flags
and no program arguments in its suite mode; `--only`, `--list` and `--measure` are for
direct invocation only.

### N2 — run the sixteen families in isolation before trusting the aggregate

Not a file edit; a run. §8 mode 1. A single aggregate run of a VM that still panics
reports the families before the aborting one and nothing else, and a reader who sees
fourteen green `CK` lines and no `PASS` has been told almost nothing.

### N3 — `bufslice` and `mathexact` are the two gates worth watching

If W7-99's `wrapping_div`/`wrapping_rem` repair and its neighbours are sound, `mathexact`
is green and its 17 step markers all print. If any `*Exact` body reaches for a Rust
`checked_*().unwrap()` or a debug-mode arithmetic op, the run stops at a named step and
that step is the whole diagnosis. Stated in advance so it is a prediction: **`mathexact`
should abort at `CK RJdkIntrinsics3 mathexact-step=Math.negateExact(Integer.MIN_VALUE)`
or not at all**, because that is the one row whose Rust-reflex implementation
(`i32::neg` / `i32::abs` under overflow checks) panics rather than returns.

### N4 — W8-C3-1's N4 is now load-bearing

Repeated because this round hit its limit rather than merely noting it. Sixteen triples
(11 `ArraysSupport`/`StringLatin1`, 5 `BigInteger` array kernels) **cannot be settled by
any assertion**, and no further vector will change that. A `CENSUS=1` mode on `run.sh`
that diffs a `--dump-native-registry` `invocations` column before and after a vector
would turn "292 triples" from a computed-but-hand-seeded number into a measurement, and
would make those sixteen answerable at all.

---

# Part 2 — `RSimpleDateFormatZone`, the `memo` family

Lane E1's N2, applied. The fixture goes **109 → 115 checks** — exactly the figure N2
predicted.

## The defect it catches

The `SimpleDateFormat` fast path memoizes verified shapes keyed by `(formatter, shape)`,
and **`setTimeZone` invalidates nothing**, so a shape verified under one zone is then
SERVED under a different zone with no cross-check. The other format blocks in that file
cannot see it: a fresh formatter's first format declines the fast path and the second is
cross-checked against bytecode. `memo` defeats both by warming ONE formatter under a zone
the VM answers correctly for, then swapping in a `SimpleTimeZone` with the same offset and
a contradicting id — the shape key is unchanged, the memo hits, no cross-check runs.

Non-vacuous because Tokyo's true offset (32,400,000) differs from the 10,800,000 supplied,
which the block asserts through `notVacuous` rather than assuming.

## One deliberate deviation from N2

N2's body contained **five** checks but declared `sectionEnd("memo", 6)`. Rather than edit
the number down, this lane added a sixth check that closes a real vacuity hole:

```java
check(f.getTimeZone().getRawOffset() == 10800000,
        "memo: setTimeZone must have TAKEN -- the formatter's zone must now report"
                + " rawOffset 10800000, got " + f.getTimeZone().getRawOffset());
```

Without it, a VM whose `setTimeZone` silently failed to take would leave the formatter
holding `Etc/GMT-3`, which answers `+0300` **correctly on every VM** — and the family's
load-bearing assertion would pass for the wrong reason. N2's declared count of 6 was
right; its body was one check short.

## HotSpot transcript

```
$ java -cp build RSimpleDateFormatZone --only=memo
CK RSimpleDateFormatZone only=memo
CK RSimpleDateFormatZone memo-after-zone-swap=2021-01-15 15:00:00 +0300
CK RSimpleDateFormatZone memo=6
CK RSimpleDateFormatZone checks=6
PASS RSimpleDateFormatZone (6 checks)
rc=0

$ java -cp build RSimpleDateFormatZone      # all seven families
... control=8  fmtdate=20  routes=30  roundtrip=30  parse=10  dstrule=11  memo=6 ...
CK RSimpleDateFormatZone checks=115
PASS RSimpleDateFormatZone (115 checks)
rc=0
```

## Mutation transcript — including E1's built-in check, made executable

E1 noted that "removing the two warm-up formats makes this family go green on a broken
VM, which is its own built-in mutation check", but could not run it: HotSpot is green
either way, so the claim needs a *model* of the broken VM to be testable at all. This
lane built one.

**MUT-A** — the expected post-swap string replaced with the broken answer.

```
AssertionError: memo: after setTimeZone(new SimpleTimeZone(10800000, "Asia/Tokyo")) the
SAME formatter must still answer from the caller's rawOffset, got
"2021-01-15 15:00:00 +0300" -- a per-(formatter, shape) memo served a zone it never checked
rc=1
```

**MUT-B** — the defect modelled at the ONE seam. `fmt()` is replaced by a per-formatter
memo that is cross-checked for the first two formats and, from the third on, resolves the
receiver's ID field against tzdb — which is what `zone_rules_cached` does one layer below
dispatch:

```java
static int fmtCalls;

static String fmt(SimpleDateFormat f, Date d) {
    if (++fmtCalls <= 2) {
        return f.format(d);
    }
    SimpleDateFormat g = new SimpleDateFormat(f.toPattern(), Locale.US);
    g.setTimeZone(TimeZone.getTimeZone(f.getTimeZone().getID()));
    return g.format(d);
}
```

```
CK RSimpleDateFormatZone memo-after-zone-swap=2021-01-15 21:00:00 +0900
AssertionError: memo: ... got "2021-01-15 21:00:00 +0900" ...
rc=1
```

`2021-01-15 21:00:00 +0900` is **exactly** the broken answer E1 measured in its §3a. The
model and the diagnosis agree without either being fitted to the other.

**MUT-C** — MUT-B's broken VM, PLUS E1's own mutation check applied to the fixture: the
two warm-up formats deleted.

```
CK RSimpleDateFormatZone memo-after-zone-swap=2021-01-15 15:00:00 +0300
CK RSimpleDateFormatZone memo=4
PASS RSimpleDateFormatZone (4 checks)
rc=0
```

**Green on the same broken VM that MUT-B reds.** The two warm-up formats are load-bearing,
and E1's built-in mutation check is now a transcript rather than a prediction.

The mutants are `scratchpad/e10/mut{A,B,C}/`.

## What still needs running

`memo` is a HotSpot-green vector against a defect nobody has yet re-measured on a binary.
E1's §8 item 4 — "N2's `memo` family, once added: this is the only vector that
distinguishes before from after" — is now runnable, and is the gate on E1's own patch.
Run it with stderr captured: `grep -c 'date-format fast path DISABLED'` must be 0.

## Files

| path | state |
|---|---|
| `regression-suite/src/RJdkIntrinsics3.java` | NEW, 1011 checks, 16 families |
| `regression-suite/src/RSimpleDateFormatZone.java` | +`memo`, 109 → 115 checks |
| `scratchpad/e10/cover3.py` | the coverage arithmetic; writes `gen3.txt` |
| `scratchpad/e10/gen3.txt` | the 292 triples, one per line |
| `scratchpad/e10/subst.py` | measure → expectation substitution, 995 values |
| `scratchpad/e10/measure.out` | the `--measure` transcript the expectations came from |
| `scratchpad/e10/mut3.py` | the 16 mutants, with the wrong theory each encodes |
| `scratchpad/e10/mut{A,B,C}/` | the three `RSimpleDateFormatZone` mutants |
