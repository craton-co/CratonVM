# W7-44 — the accounting currency pattern, the anonymous enum refusal, and the ULP that was not `Double.toString`

> ## 2026-08-12 (P3-E) — re-verified against the tree; all three sections stand, and P3-E discharges NONE of them
>
> Checked because W7-34/W7-80/W7-44 are one neighbourhood and a record's stated
> hypothesis can be wrong rather than merely stale. Every claim below was
> re-derived from today's source, not inherited:
>
> * **§1, currency pattern.** The `;(¤#,##0.00)` negative subpattern is gone from
>   both copies; `locale_resources.rs:314` carries the standing comment that slot
>   1 must not reacquire one. §1's own inline STALE banner is **correct** —
>   `getNumberPatterns` now reads `cldr_number_strings(&t, "NumberPatterns")` per
>   locale, exactly the staged step it declined. Nothing further to change.
> * **§2, `Enum.valueOf`.** `no_enum_constant_message` and
>   `no_enum_constant_message_for_mirror` are in
>   `native-builtins/src/lang_class.rs` with three unit tests, including the
>   nested-canonical-name and the literal-`null` cases. Stands.
> * **§3, `StrictMath.log`.** The CORRECTION note's claim is the one worth
>   re-testing, because it is the one that says the fix had landed in a body the
>   VM does not run. Re-grepped: **all five** polar sites now read
>   `cratonvm_types::fdlibm::log` — `native-builtins/src/lib.rs:36220`,
>   `securerandom.rs:603/996/1356`, `native-collections/src/lib.rs:31186` — so
>   the last-write-wins winner is covered. `types/src/fdlibm.rs` exists. Stands,
>   correction included.
>
> **What P3-E does NOT discharge here, stated because the two look adjacent.**
> P3-E fixed which `Locale` `java.util.Formatter` picks when the overload has
> none. `java.text.NumberFormat` / `DecimalFormat` — this record's whole §1 — is
> a **different surface** that never had that defect: its no-arg factories are
> real JDK bytecode that already call
> `getInstance(Locale.getDefault(Locale.Category.FORMAT))`, and the patterns and
> symbols they read were made locale-aware by W7-80, not by P3-E. §2 and §3 are
> unrelated to formatting locale entirely. Every residual in this record's
> "What is still open" — the rest of the `StrictMath` transcendental surface, and
> synthetic mode's pattern-ignoring `DecimalFormat.format(D)` (§1's last
> subsection) — is untouched by P3-E and remains open.

**Status: SOURCE LANDED, NOT RE-MEASURED.** No CratonVM binary was built from this branch —
this lane does not run `cargo build`. Every HotSpot column below is a run on
jdk-25.0.3.9-hotspot; every CratonVM column is the state recorded by
W7-40-differential-at-14.md, i.e. *before* the change. The claims about what the source now
does are claims about source. The re-measurement is the last section.

Three of the 14 rows W7-40 left open. They are independent defects that happened to land in
the same probe section, and each turned out to have a population an order of magnitude
larger than its one row.

All three are **HotSpot-parity bug fixes**, so they apply to **both** modes. Compatible mode
(`--real-jdk`) is contractually frozen except for parity fixes; each of these makes
Compatible mode agree with HotSpot where it previously disagreed, which is exactly the
carve-out. Two of the three (currency, `StrictMath.log`) are in fact reachable *only* in
Compatible mode, because the synthetic `java.text`/`java.util.Random` stand-ins do not run
that code at all.

| observable | HotSpot | CratonVM (W7-40) |
|---|---|---|
| `NumberFormat.currencyNegativeUS` | `-$1,234.50` | `($1,234.50)` |
| `Enum.valueOfBadName` | `No enum constant ShadowDifferentialProbe.Color.MAUVE` | `No enum constant MAUVE` |
| `Random.nextGaussian` | `1.1419053154730547` | `1.141905315473055` |

---

## 1. The negative currency pattern: one hardcoded string, wrong for every locale

`NumberFormat.getCurrencyInstance(Locale.US).format(-1234.5)` rendered `($1,234.50)`. The
parenthesised form is a real CLDR pattern — it is the **accounting** pattern — but it is not
any locale's standard currency format.

### Where the pattern comes from

Not from locale data. CratonVM answers
`sun.util.locale.provider.LocaleResources.getNumberPatterns()` from a **hardcoded,
locale-independent 4-slot array** (`native-builtins/src/locale_resources.rs`), because the
real path walks the class-based `jdk.localedata` bundles CratonVM does not surface and the
JDK then indexes 0..3 unconditionally into a null. Slot 1 read:

```text
¤#,##0.00;(¤#,##0.00)
```

The `;` introduces a **negative subpattern**. When one is present `DecimalFormat` uses it
verbatim and never applies its default rule — "negative prefix = minus sign + positive
prefix" — so no minus sign could ever appear, in any locale, for any negative amount.

There was a **second copy** of the same four strings a thousand lines earlier in the same
file, in the synthesised `FormatData` bundle (`put_arr(..., "NumberPatterns", ...)`), with
the same accounting pattern. Finding it took grepping the pattern *string*; grepping
`getNumberPatterns` finds only one of the two. Both are fixed and are now byte-identical.

### The population, measured

`probes/NumberPatternCensusProbe.java` (new) walks `Locale.getAvailableLocales()` on HotSpot
and compares the real CLDR patterns against the hardcoded array.

```text
locales.total=1158
wrong.currency=1158        <- before
wrong.number=4
wrong.percent=234
distinct.currencyForms=27
```

**The accounting string matched 0 of 1158 locales.** Not "wrong for en-US" — wrong
everywhere, and not a single one of the 27 distinct currency forms JDK 25 ships uses
parentheses. So there was no locale for the old value to be right for, and no regression
surface for removing it.

After dropping the subpattern, slot 1 is `¤#,##0.00`:

```text
wrong.currency=797         <- after
```

`¤#,##0.00` is the **exact** CLDR value for **361** locales (the modal form, and the whole
`en` family — en-US, en-GB, en-CA, plus hi-IN, th-TH, tr-TR, ko-KR …). The remaining 797
differ only in symbol **placement**: `#,##0.00 ¤` (333 locales — de, fr, es, it, pl, sv)
and `¤ #,##0.00` (215). Every one of those 797 now gets the correct *negative* rendering,
which is the defect this row named.

### What was deliberately not done — and was DONE four records later

> **STALE 2026-08-12.** Everything in this subsection describes the tree before
> `W7-80-locale-data-stage-two.md`, which took exactly the staged step it asks
> for: `getNumberPatterns`, `getDecimalFormatSymbolsData`, `DecimalFormatSymbols
> .initialize` and the currency tables all read the JDK image's own CLDR data
> per locale now, **together**, in one commit, for the reason this subsection
> gives. The hardcoded arrays survive only as the fallback for an image with no
> `jdk.localedata`. The argument below is why it was correct to decline the
> half-step; it is no longer a description of `locale_resources.rs`.

Making the pattern locale-aware. `getDecimalFormatSymbolsData` in the same file is
**also** hardcoded en (`.` decimal separator, `,` grouping) for every locale. Making the
pattern locale-aware without the symbols would produce a hybrid — `1,234.50 €` for de-DE,
where HotSpot gives `1.234,50 €` — which is *further* from HotSpot than the uniform en
shape. The two tables must move together, and that is a feature, not this parity fix. It is
the same en/de split the sibling `getDateTimePattern` override already carries, so the shape
of the eventual fix is known.

### The sibling surfaces, checked

| surface | slot | right for | note |
|---|---|---|---|
| `getNumberInstance` | 0 `#,##0.###` | 1154 / 1158 | the 4 misses are `#,#0.###` / `#0.######` |
| `getCurrencyInstance` | 1 `¤#,##0.00` | 361 / 1158 exact, 1158 / 1158 for the sign | was 0 / 1158 |
| `getPercentInstance` | 2 `#,##0%` | 924 / 1158 | the 234 misses are the `#,##0 %` no-break-space family (de/fr/es/sv), same en/locale split as the symbols |
| scientific | 3 `#E0` | not measurable | `NumberFormat.getScientificInstance(Locale)` is package-private, so no external probe can read it |

Positive currency, zero, grouping and `getIntegerInstance` were all already correct for the
en family (`$1,234.50`, `$0.00`, `1,234,568`) — the wrong subpattern did **not** travel with
other wrong subpatterns here, because there is only the one table and only its slot 1 was
malformed.

### A separate, pre-existing gap in synthetic mode

`native-builtins/src/phases_late/text_intl.rs` has its own `NumberFormat` factories for
synthetic-JDK mode. They store patterns (`$#,##0.00`, `#,##0%`) but the synthetic
`DecimalFormat.format(D)` body **ignores the pattern's prefix and suffix entirely** — it
applies only fraction digits and grouping. So in synthetic mode
`getCurrencyInstance().format(-1234.5)` yields `-1,234.50` (no `$`) and
`getPercentInstance().format(0.755)` yields `76` (no `%`). That is a different defect with a
different fix, it is not the parenthesis bug, and it is left open here.

---

## 2. `Enum.valueOf`: five sites, one of them anonymous

HotSpot's message is

```java
"No enum constant " + enumType.getCanonicalName() + "." + name
```

CratonVM's `java.lang.Enum.valueOf` native raised a bare `No enum constant MAUVE` —
divergent, and a message that never says which enum refused.

### The population: five sites, one wrong

Grepping the message string across `native-builtins/` finds **five** raising sites:

| site | message | verdict |
|---|---|---|
| `native-builtins/src/lib.rs` — `java.lang.Enum.valueOf` | `No enum constant {name}` | **wrong** — no type at all |
| `phases_early.rs` — `java.time.Month.valueOf` | `No enum constant java.time.Month.{name}` | correct |
| `phases_early.rs` — `java.time.DayOfWeek.valueOf` | correct | |
| `phases_early.rs` — `java.time.temporal.ChronoUnit.valueOf` | correct | |
| `phases_early.rs` — `java.math.RoundingMode.valueOf` | correct | |

The four synthetic `valueOf` bodies were already right, because all four enums are
**top-level** classes whose canonical name is their binary name with dots. One site was
wrong. But five hand-rolled `format!` copies of one message is precisely the idiom this
codebase keeps paying to patch per-site, so all five now go through one helper in
`lang_class.rs`:

```rust
no_enum_constant_message(canonical_type_name, constant)          // pure string
no_enum_constant_message_for_mirror(ctx, enum_mirror, constant)  // resolves the type
```

No call site had to be told the type it could not obtain: `Enum.valueOf` is handed the
enum's `Class` mirror as its first argument, so the mirror path always has it. Nothing is
threaded anywhere and nothing emits a half-qualified name.

### `getCanonicalName()`, not `getName()`

This is the part that is easy to "fix" into a different divergence. The probe's enum is
**nested** — binary name `ShadowDifferentialProbe$Color`, canonical name
`ShadowDifferentialProbe.Color`. Building the message from `getName()` would produce

```text
No enum constant ShadowDifferentialProbe$Color.MAUVE
```

which is not what HotSpot prints. The helper calls
`native_class_get_canonical_name`, which already performs the `$` → `.` rewrite and already
preserves a literal `$` inside a member name. A regression test drives the nested case end
to end through the mirror rather than asserting the pure string, so the `$` → `.` step is
covered and not assumed.

### The one case that cannot be qualified

A **local or anonymous** enum has no canonical name at all (JLS 6.7) and
`Class.getCanonicalName()` returns `null` there. The JDK concatenates that null straight
into the message, yielding a literal `No enum constant null.X`. The helper reproduces that
verbatim rather than substituting the binary name, because the contract being matched is
HotSpot's exact text; substituting something more readable would re-open the divergence in
the opposite direction. This is the only shape where the message does not name a type, and
it is not a case where the type "could not be obtained" — it is a case where the JDK itself
prints `null`.

### GC safety

The refusal path now reads the enum mirror's canonical name **after** the loop that calls
`Enum.name()` on every constant — i.e. after an arbitrary number of Java calls, any of which
can move objects. The mirror is rooted in the same `NativeHandleScope` as the constants
array (which was rooted in 2026-08-04 for exactly this reason) and re-read through its
handle before use. Adding a read of a bare `ObjectRef` at the end of that loop would have
been a fresh instance of the defect the surrounding comment documents.

---

## 3. `Random.nextGaussian`: not `Double.toString`, and not `nextGaussian` either

```text
HotSpot : Random.nextGaussian=1.1419053154730547
CratonVM: Random.nextGaussian=1.141905315473055
```

W7-40 filed this as "a `Double.toString` shortest-representation difference, not a different
number". **That was wrong, and it was the load-bearing question.** A fix to `Double.toString`
would have been inert; a fix to `nextGaussian`'s *algorithm* would have been inert too.

### The determination

`probes/DoubleShortestReprProbe.java` (new) prints `doubleToRawLongBits` beside
`Double.toString`:

```text
1.1419053154730547 -> 3ff2453e82115d86
1.141905315473055  -> 3ff2453e82115d87
```

**Different bits, one ULP apart.** They are two different doubles, and both strings are the
correct shortest-round-trip rendering of their own value — CratonVM's `Double.toString`
(`types/src/float_format.rs`, Rust's shortest digits plus the JLS layout rule) printed
exactly what HotSpot would print for the value it was given. `Double.toString` is innocent.
For completeness the same probe round-trips 200 000 random doubles through
`Double.toString`/`parseDouble` on HotSpot: `roundTripFailures=0`, `nonShortest=0`, so
there is no broad `Double.toString` defect to find here.

### The actual defect

`java.util.Random.nextGaussian()` on JDK 25 is **still** the classic polar method — it does
not go to `RandomSupport.computeNextGaussian`; `java.util.Random` overrides `nextGaussian()`
to preserve its documented seeded stream. The JDK source
(`jdk-25.0.3.9-hotspot/lib/src.zip`, `java.base/java/util/Random.java`) reads:

```java
double multiplier = StrictMath.sqrt(-2 * StrictMath.log(s)/s);
```

Every operation in that line is exactly-rounded IEEE 754 — `*`, `/`, `sqrt` — **except
`log`**. `StrictMath.log`'s contract is not an accuracy bound, it is "the fdlibm result, bit
for bit, on every platform". CratonVM computed `(-2.0 * s.ln() / s).sqrt()` — platform libm.

`probes/GaussianLogUlpProbe.java` (new) recomputes the first variate of `new Random(42)`
both ways on HotSpot:

```text
s.bits=3fd5d9e5352fee22
StrictMath.log(s).bits=bff131ae8f1bf126
Math.log(s).bits=bff131ae8f1bf127      <- differs
g1.viaStrictLog.bits=3ff2453e82115d86  str=1.1419053154730547   <- HotSpot's answer
g1.viaMathLog.bits=3ff2453e82115d87    str=1.141905315473055    <- CratonVM's answer, exactly
log.disagreeRate=73015/1000000
```

The non-fdlibm log reproduces CratonVM's output **bit for bit**. That closes it: the
divergence is `log`, and `nextGaussian`'s structure — polar method, cached partner variate,
LCG advance rate — was already correct.

### The population: `StrictMath`, not one probe row

`native-builtins/src/lib.rs` has carried a comment since the 2026-06-20 review recording that
CratonVM registers the **same** libm-backed bodies for `Math` and `StrictMath`, and that this
"VIOLATES the StrictMath bit-reproducibility contract". This row is that violation arriving
somewhere a user can see it. The measured size, on Temurin 25.0.3+9 / Windows x86-64:

**`Math.log` and `StrictMath.log` return different bit patterns for 73 015 of 1 000 000
uniform draws in (0, 1) — 7.3%.** Every seeded `nextGaussian` stream, on either side of the
`SecureRandom` SHA1PRNG path too, was rolling that 7.3% dice on each draw.

### The fix

`types/src/fdlibm.rs` (new) — a line-for-line port of JDK 25's
`java.lang.FdLibm.Log.compute` (itself FDLIBM 5.3 `e_log.c`), including the integer
high-word surgery and, critically, the exact bracketing of every floating-point expression:
FP addition is not associative, so re-bracketing the polynomial or the reconstruction changes
the last bit and defeats the port. Constants are given by bit pattern rather than decimal,
because Rust has no hex float literal and a decimal transcription is one more place for a
last-ULP mistake.

Wired into:

* `java/lang/StrictMath.log` — and **only** that class. `Math.log`'s contract is a 1-ULP
  bound that platform libm meets; `StrictMath.log`'s is bit-for-bit. Sharing one body was
  what made the strict class no stricter than the loose one, so `register_math_natives` now
  branches on the class name for this one method.
* `rnd_gaussian_pair` in `native-collections/src/lib.rs` (`java.util.Random`).
* Both polar sites in `native-builtins/src/securerandom.rs` — the SHA1PRNG one is seeded and
  therefore observable; the OS-CSPRNG one is not, and is changed anyway so the three sites
  cannot drift.

> **CORRECTION 2026-08-12 — this list named the registrar that LOSES, and the
> row did not move.** `java/util/Random.nextGaussian` is registered twice, and
> registration is last-write-wins: `native-collections`' `register_random_natives`
> — the bullet above — is overwritten by `native-builtins/src/securerandom.rs`'s
> `register`, which `vm/src/vm/vm_init.rs` calls afterwards inside a block
> labelled *"LAST-WRITE-WINS BOUNDARY — do not reorder"*, deliberately, because
> the collections body reads the LCG seed from a synthetic two-field layout and
> answers zero on a real-JDK `Random`. So the fdlibm `log` landed in a body the
> VM does not run, and the body it does run was still on `f64::ln`. There was
> also a **third** copy in `native-builtins/src/lib.rs`, unregistered but
> compiled and tested, that this record never mentions. Nothing caught it
> because the bit-exact seeded-stream test lives in `native-collections` and
> calls that crate's helper directly, never the registry — a test on the wrong
> side of a last-write-wins boundary is not weak evidence, it is *no* evidence.
> Found and fixed by `W7-54-strictmath-fdlibm-family.md` §7; verified in the
> tree 2026-08-12, all three `securerandom.rs` polar sites now read
> `cratonvm_types::fdlibm::log`. **Read §7 of W7-54, not this list, for where
> the fix is.** The rest of §3 — the ULP determination, the 7.3% census, the
> port and its 90 vectors, the tolerance that hid it — was re-checked and
> stands.

Verified bit-exact against **90 golden vectors** captured from HotSpot with
`probes/StrictMathLogVectorProbe.java`: the `s` this defect turns on, the exact powers of
two, both sides of 1.0 (the `|f| < 2^-20` branch), `sqrt(2)/2` and `sqrt(2)` (the
argument-reduction boundary), `MIN_VALUE` / `MIN_NORMAL` / `MAX_VALUE` (the subnormal
rescale path), ±0, negative, ±inf, NaN, and 60 pseudorandom draws spanning the exponent
range. The port reproduces all 90, and reproduces the first 8 draws of
`new Random(42).nextGaussian()` bit for bit.

### The tolerance that hid it

`next_gaussian_matches_jdk_seeded_sequence` in `native-collections/src/lib.rs` compared
against HotSpot's seeded stream with a **1e-12 relative tolerance**, excused in its own
comment:

> the JDK's multiplier goes through `StrictMath.log` (fdlibm) while this uses the platform
> libm, and those may differ in the last ulp. A DIFFERENT sequence cannot come within 1e-12
> of this one, so the tolerance costs the assertion nothing.

The premise was true and the conclusion was wrong. The tolerance was not free: it was sized
exactly to admit the defect, it named the defect while admitting it, and the differential
probe then read that admitted last ULP straight out of `Double.toString`. This is a
tolerance masking the rule it was named after. It is now `assert_eq!` on bits.

### What is still open

Only `log` is ported. The rest of the `StrictMath` transcendental surface — `sin`, `cos`,
`tan`, `asin`, `acos`, `atan`, `atan2`, `exp`, `log10`, `cbrt`, `pow`, `sinh`, `cosh`,
`tanh`, `hypot`, `expm1`, `log1p` — still delegates to platform libm and still violates the
same bit-reproducibility contract. Nothing measured says how visible those are; `log` was
picked because a differential row pointed at it. `Math.log` is deliberately left on libm.

---

## Re-measuring

Nothing here was run against a CratonVM binary. To close:

1. Build this branch and run `probes/ShadowDifferentialProbe.java` under `--real-jdk`
   against HotSpot 25.0.3.9, both pinned to UTF-8 / en-US, as W7-40 did. Expect three of the
   14 rows to go quiet: `NumberFormat.currencyNegativeUS`, `Enum.valueOfBadName`,
   `Random.nextGaussian`.
2. Run `probes/DoubleShortestReprProbe.java` on CratonVM. `gaussian.bits` must now read
   `3ff2453e82115d86`. If it still reads `...d87`, the `StrictMath.log` registration is not
   the one being reached and the real-JDK `java.util.Random` bytecode path should be checked
   before anything else is changed.
3. Run `probes/NumberPatternCensusProbe.java` on CratonVM. Its `CRATON` constant now holds
   the fixed patterns, so `wrong.currency` should print `797` there as it does on HotSpot,
   and the `en-US.currencyNeg` row should read `-$1,234.50`.
4. `cargo test -p cratonvm-types` covers the fdlibm vectors; `cargo test -p
   cratonvm-native-collections` covers the now-exact gaussian stream; the `Enum.valueOf`
   message tests are in `cratonvm-native-builtins`. Note that a `--lib` run does not compile
   `vm/src/vm/tests.rs`; that module is synthetic-JDK-only and unaffected here.

The remaining W7-40 rows — the four vanishing `ArrayDeque` observables, `COW.addAllAbsent`,
the `[SUREFIRE-NPE]` stdout leak, `stream.reuseThrows`, and the five `format.*` exception
subclasses — are untouched by this record.
