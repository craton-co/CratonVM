# F38-1 — the `Formatter` locale slot, `%tY` is era-relative on BOTH printers, and `getMinusSign` has exactly ONE call site in the whole class

**Date:** 2026-08-13
**Lane:** F38 (`--jdk-only` pool)
**Files changed:** `native-builtins/src/lang_string.rs`, `native-builtins/src/lib.rs`
**Oracle:** HotSpot 25.0.3+9-LTS (`openjdk 25.0.3 2026-04-21 LTS`, Microsoft-13877124).
**Measured under:** `-Duser.timezone=Asia/Kolkata` and `TimeZone.setDefault("Asia/Kolkata")`
for every `%t` row; `Locale.ROOT` unless a row names its locale;
`-Duser.language=tr -Duser.country=TR` for the constructor rows; `lt-LT`,
`et-EE`, `sl-SI`, `sv-SE`, `fi-FI` for the minus-sign rows. Host default locale
`ru_RU`. Every behavioural expectation below was MEASURED before it was written;
the only PREDICTED claims are about which CratonVM checks move, and they are
marked.

Takes F28-1's four nominations (`F28-1-the-t-family-answered-27-fields…`
§5.1–§5.4). **Two of the four had a false premise, and catching them is the
substance of this record.**

---

## 1. The headline: two premises this lane was handed were wrong

| nomination | premise as written | measured |
|---|---|---|
| §5.2 (year) | "`Calendar.YEAR` is already era-relative, so a single expression cannot serve both — fixing `java.time` breaks `Calendar`" | **False for this codebase.** CratonVM never reads `Calendar.YEAR`. One expression serves all ten source shapes. |
| §5.3 (minus) | "`%tF`'s minus, and every negative `%d`/`%f`/`%e`/`%g`, use the ASCII `'-'`; the JDK uses `getMinusSign(l)`" | **The second half is false.** `getMinusSign` has ONE call site in `Formatter.java` and it is `%tF`'s. Every other negative number is ASCII on HotSpot too. |
| §5.1 (locale slot) | premise correct, **but the proposed patch does not compile and does not pin** | see §5.1 |
| §5.4 (lower-caser) | flagged without a live row, correctly | **no live row exists** — 1158 locales swept, zero. Landed anyway as family convergence. |

The §5.3 correction is the one that changed a decision: F28 declined the fix
because it would "put a fourth symbol on the hot path to close one quarter of
one row". Measured, it closes the row **completely**, and the cost is one extra
`()C` virtual call per already-cached `fmt_symbols_for` resolution.

---

## 2. Measured HotSpot 25 rows

### 2.1 The `Formatter` constructor locale slot — §5.1's premise, verified

`-Duser.language=tr -Duser.country=TR`, `d = new Date(1699999999000L)`:

| expression | HotSpot 25 |
|---|---|
| `new Formatter(sb).format("%tb", d)` | `Kas` |
| `new Formatter(sb, (Locale) null).format("%tb", d)` | **`Nov`** |
| `new Formatter().format("%tb", d)` | `Kas` |
| `new Formatter().format("%,d", 1234567)` | `1.234.567` |
| `String.format((Locale) null, "%,d", 1234567)` | **`1,234,567`** |
| `new Formatter(sb).locale()` | `tr_TR` |
| `new Formatter(sb, (Locale) null).locale()` | **`null`** |

The `locale()` pair is the proof that the field really holds a `Locale` and the
decision is not taken late: `Formatter.locale()` is a plain field read. So the
null CratonVM writes is not a lazy encoding of "default", it is the wire
representation of a **different** request.

### 2.2 `Formatter.toString()` on a caller-supplied sink — NOT in any brief

Found while checking whether the "delete the registrations" alternative is
viable. Same host:

| expression | HotSpot 25 | CratonVM before |
|---|---|---|
| `new Formatter(sb).format("%d",42).toString()` | `42` | **`` (empty)** |
| `new Formatter().format("%d",1).toString()` | `1` | `1` |
| `new Formatter().out().getClass()` | `java.lang.StringBuilder` | `java.lang.String` |
| `new Formatter(sb).out().getClass()` | `java.lang.StringBuilder` | `java.lang.StringBuilder` |

`NativeContext::read_string` (`vm/src/vm/vm_exec.rs:11713`) returns `None` for
**any** object whose class is known and is not `java/lang/String` — a deliberate
guard against the structural reader decoding a `StringBuilder`'s whole char[]
capacity. Both `toString()` natives were `read_string(sink).unwrap_or_default()`,
so every non-`String` sink answered the empty string. The second row is why it
survived: the `()V` native writes a real `String` into slot 0, so only the
`Appendable`-taking constructors could expose it. `format` itself was never
affected — its own `read_string` miss falls through to
`invoke_virtual(sink, "append", …)`, so the text really is in the caller's
`StringBuilder` and only the `Formatter`'s own view of it was empty.

The `out()` row is recorded, not fixed — see §5.1.

### 2.3 The year fields, down BOTH of `printDateTime`'s printers

`String.format(Locale.ROOT, …)`, `TimeZone.setDefault("Asia/Kolkata")`. The
`GregorianCalendar`/`Date`/`Long` rows are the **same instant** as the
`LocalDate` row (`-63549548877000L`), which is what makes the `%tF` column the
only disagreement:

| argument | `%tF` | `%tY` | `%ty` | `%tC` | `%tD` |
|---|---|---|---|---|---|
| `LocalDate.of(-44,3,15)` | `-0044-03-15` | `0045` | `45` | `00` | `03/15/45` |
| `LocalDate.of(-1,6,5)` | `-0001-06-05` | `0002` | `02` | `00` | `06/05/02` |
| `LocalDate.of(0,2,29)` | `0000-02-29` | `0001` | `01` | `00` | `02/29/01` |
| `LocalDate.of(1,1,1)` | `0001-01-01` | `0001` | `01` | `00` | `01/01/01` |
| `LocalDate.of(12345,3,4)` | `+12345-03-04` | `12345` | `45` | `123` | `03/04/45` |
| `LocalDate.of(9999,12,31)` | `9999-12-31` | `9999` | `99` | `99` | `12/31/99` |
| `LocalDateTime` / `ZonedDateTime` / `OffsetDateTime` at `-44` | `-0044-03-15` | `0045` | `45` | `00` | `03/15/45` |
| **`GregorianCalendar` ERA=BC YEAR=45** | **`0045-03-15`** | `0045` | `45` | `00` | `03/15/45` |
| **`GregorianCalendar` ERA=BC YEAR=1** | **`0001-01-01`** | `0001` | `01` | `00` | `01/01/01` |
| **`new Date(-63549548877000L)`** | **`0045-03-15`** | `0045` | `45` | `00` | `03/15/45` |
| **`Long.valueOf(-63549548877000L)`** | **`0045-03-15`** | `0045` | `45` | `00` | `03/15/45` |
| **`GregorianCalendar` AD 12345** | **`12345-03-04`** | `12345` | `45` | `123` | `03/04/45` |

`%tc` tails, same instant: `ZonedDateTime` gives
`Thu Mar 15 01:02:03 GMT+09:00 0045`, the `GregorianCalendar`
`Tue Mar 15 01:02:03 GMT+05:30 0045` — **both `0045`**.

And the field each printer actually reads, read in
`java.base/java/util/Formatter.java`:

```java
// TemporalAccessor printer, CENTURY / YEAR_2 / YEAR_4
int i = t.get(ChronoField.YEAR_OF_ERA);
switch (c) { case CENTURY -> i /= 100; case YEAR_2 -> i %= 100; case YEAR_4 -> size = 4; }

// Calendar printer, same three
int i = t.get(Calendar.YEAR);          // already era-relative

// TemporalAccessor ISO_STANDARD_DATE ('F')          Calendar ISO_STANDARD_DATE ('F')
int year = t.get(ChronoField.YEAR);                  print(YEAR_4) '-' print(MONTH) '-' print(DAY)
if (year < 0) { sb.append(getMinusSign(l)); year = -year; }
else if (year > 9999) sb.append('+');
sb.append(localizedMagnitude(year, ZERO_PAD, 4));
```

`ChronoField.YEAR_OF_ERA`, measured directly: `-44 → 45`, `-1 → 2`, `0 → 1`,
`1 → 1`, `12345 → 12345`. So `1 - year` for `year <= 0`, **not** `-year`.

**Why the §5.2 premise does not transfer.** It is a true statement about the
JDK's two printers and a false one about CratonVM, because
`extract_temporal_fields` never asks a `Calendar` for `Calendar.YEAR`: every
epoch-shaped source goes `getTimeInMillis()` → `millis_to_fields` →
`temporal_from_epoch_day`, which yields a **proleptic** year exactly like
`getYear()` does for the `java.time` sources. One expression therefore serves
all ten shapes, and the `%tF` split is the only place the two printers need to
be told apart.

### 2.4 `getMinusSign` — one call site, 59 locales, one conversion

Sweeping `DecimalFormatSymbols.getInstance(l).getMinusSign()` over all
**1158** locales `Locale.getAvailableLocales()` reports:

| value | count | languages |
|---|---|---|
| U+002D HYPHEN-MINUS | 1099 | everything else |
| **U+2212 MINUS SIGN** | **59** | `et` `eu` `fa` `fi` `fo` `gsw` `hr` `ksh` `lt` `nb` `nn` `no` `rm` `se` `sl` `sv` and their regional forms |

Under `lt-LT` (and identically under `et-EE`, `sl-SI`, `sv-SE`, `fi-FI`), dumped
as UTF-16 code units:

| conversion | argument | HotSpot 25 |
|---|---|---|
| `%d` | `-5` | `002D 0035` — **ASCII** |
| `%,d` | `-1234567` | `002D` … — ASCII |
| `%.2f` `%e` `%g` `%a` | `-1234.5` | `002D` … — ASCII |
| `%.2f` | `-0.0` | `002D` … — ASCII |
| `%.1f` | `-Infinity` | `002D` … — ASCII |
| `%d` | `BigInteger("-5")` | `002D 0035` — ASCII |
| `%.2f` | `BigDecimal("-1.5")` | `002D` … — ASCII |
| `%s` | `-5` | `002D 0035` — ASCII |
| `%(d` | `-5` | `0028 0035 0029` — parentheses |
| `%tz` | `OffsetDateTime` at `-03` | `002D 0030 0033 0030 0030` — ASCII |
| **`%tF`** | **`LocalDate.of(-44,3,15)`** | **`2212`** `0030 0030 0034 0034` `002D` `0030 0033` `002D` `0031 0035` |
| `%tF` | `LocalDate.of(12345,3,4)` | `002B` … — **ASCII `'+'`** |
| `%tF` | `GregorianCalendar` BC 45 | `0030 0030 0034 0035` … — no sign at all |
| `%tF` | `LocalDate.of(-44,…)`, `(Locale) null` | `002D` … — ASCII |

Two things in the `%tF` row that a `replace('-', minus)` would get wrong: the
year's sign is localized and **the two date separators in the same field are
not**, and the `'+'` past 9999 stays ASCII even where the minus does not — the
JDK writes it as a literal in the same block it calls `getMinusSign(l)` in.

Confirmed in source: `grep -n getMinusSign Formatter.java` → declaration at
2053, **one** use at 4635, inside the `TemporalAccessor` `ISO_STANDARD_DATE`
arm. Every other sign goes through `leadingSign`, whose body is
`sb.append('(')` / `sb.append('-')` with no locale in scope.

### 2.5 `%tp`'s lower-caser — a measured NEGATIVE

The JDK rule, identical in both printers:

```java
String[] ampm = { "AM", "PM" };
if (l != null && l != Locale.US) { ampm = DateFormatSymbols.getInstance(l).getAmPmStrings(); }
sb.append(ampm[…].toLowerCase(Objects.requireNonNullElse(l, Locale.getDefault(FORMAT))));
```

Two sweeps over all 1158 available locales:

| sweep | divergent rows |
|---|---|
| every locale's own `getAmPmStrings()`, `toLowerCase(loc)` vs `toLowerCase(ROOT)` | **0** |
| the English literals `"AM"`/`"PM"`, `toLowerCase(loc)` vs `toLowerCase(ROOT)`, over every locale as the mapping locale | **0** |

The `tr`/`az`/`lt` rules need a dotted/dotless `I` or a combining-dot sequence
and no CLDR AM/PM string carries one. The other half of the change —
`str::to_lowercase` → `case_map::jdk_to_lowercase` — can only differ on
`JDK_UNMAPPED_CASE_CODE_POINTS`, which is `U+A7CE`, `U+A7CF`, `U+A7D2`..`U+A7D5`;
no AM/PM string is in Latin Extended-D.

**So no row moves.** It is landed anyway, and §3.4 says why.

---

## 3. What changed

### 3.1 `native-builtins/src/lib.rs` — the locale slot (§5.1), fix not delete

**Decision: write the default locale; do NOT delete the registrations.** The
alternative F28 offered ("deleting both registrations would be the smaller, more
principled change… this lane cannot build or run to check slot 0") was
investigated rather than deferred again, and it fails on slot 0 — verified, not
assumed:

* Real `Formatter()` writes `a = new StringBuilder()` into slot 0.
* `read_string` returns `None` for any known non-`String` class (§2.2).
* So with the native gone, `toString()` reads a `StringBuilder`, gets `None`,
  and answers `""` for every `new Formatter().format(…).toString()` — while
  `format`'s appending path keeps working, which makes the failure silent.

Deleting is therefore **not** the smaller change: it is the `toString` fix
**plus** moving every `new Formatter()` onto real bytecode that resolves
`Locale.getDefault(FORMAT)` **and**
`DecimalFormatSymbols.getInstance(l).getZeroDigit()` at construction time, on a
lane that cannot build or run. The `toString` half is landed here (§3.2), so the
deletion now has its precondition; the decision is about what a no-build lane
should ship, not a claim that deletion is wrong. That reasoning is written into
the comment beside the registrations so the next lane does not re-derive it.

New `formatter_default_locale(ctx) -> Value`, three rungs, each degrading to the
one below:

1. `Locale.getDefault(Locale.Category.FORMAT)` — the category the JDK's own
   constructors ask for. `Locale$Category.FORMAT` is READ as a static field and
   the one-argument overload is called **only** if that read produced an object:
   real `Locale.getDefault(Category)` opens with `Objects.requireNonNull`, so
   passing a null would trade a wrong locale for an NPE.
2. `Locale.getDefault()` — the base default.
3. `Value::Object(None)` — exactly the previous behaviour.

`locale_bootstrap` already overrides both `getDefault` overloads with a cached,
stable-identity object resolved from `user.language`/`user.country`, so rung 1
runs a **native**, not the JDK locale-provider adapter chain — which is also why
it cannot re-enter `String.format`.

**Two defects in F28's proposed patch, both fixed here.** The nomination's exact
text was `ctx.set_field(this, 1, fmt_default_locale_value(ctx))`.

* **It does not compile.** Two-phase borrows admit only *shared* uses of the
  receiver during the reservation phase; the argument needs a second `&mut *ctx`
  reborrow, which is `E0499`. Every call site here binds the locale to a local
  first.
* **It does not pin.** `ctx.invoke` runs Java code and can move `this` — this is
  the hazard the `format` native fifty lines below already documents and pins
  for ("resolving a non-null locale runs real JDK bytecode… so the receiver can
  move across the format call"). All four sites now
  `pin_native_root` / `read_native_pin` / `unpin_native_roots` around the
  resolution, and write slot 0 **first** so the freshly created string is
  reachable from the pinned receiver while the bytecode runs.

Four sites, matched on their surrounding lines because the literal
`ctx.set_field(this, 1, Value::Object(None));` appears four times in the file:

| registrar | site |
|---|---|
| real-JDK | `registry.register(f, "<init>", "()V", …)` |
| real-JDK | `registry.register(f, "<init>", "(Ljava/lang/Appendable;)V", …)` |
| synthetic | `native_formatter_init` |
| synthetic | `native_formatter_init_appendable` |

`grep -c 'set_field(this, 1, Value::Object(None))'` on the file is now **0**.

### 3.2 `lib.rs` — `formatter_sink_text`, and the two copies of `toString`

Both registrars' `toString` natives now share one helper: `read_string` first
(the `String` sink, unchanged and allocation-free), then `toString()` on the
sink through `invoke_virtual`, then the empty string. That last rung is the
previous behaviour, so a sink that answers nothing degrades rather than fails.
Fixes §2.2's `42` → `""` row for both modes.

### 3.3 `lang_string.rs` — `year_of_era` (§5.2)

One expression, `if year <= 0 { 1 - year } else { year }`, computed once in
`format_temporal_field` and read by five arms: `%tY`, `%ty`, `%tC`, `%tD`'s year
slot and `%tc`'s tail. It serves **both** printers because CratonVM decodes a
proleptic year for every source (§2.3). `%tj` and the day-of-week keep the
proleptic year; they are not era quantities.

`%tF` is the one arm that has to ask which source it has, so `FmtSupport` gains
`calendar_printer: bool` — **which printer**, not a support question, and
deliberately a stored field rather than a derived one: a `ZonedDateTime` sets
every one of the five support flags and still takes the *other* printer, so it
cannot be computed. The test asserts exactly that.

* `calendar_printer == true` → `format!("{year_of_era:04}")`, the `Calendar`
  arm's era-relative `YEAR_4`, no sign rule and no `'+'`.
* `calendar_printer == false` → `fmt_iso_year(year, sym.minus)`, unchanged
  except for the minus.

**This also closes a divergence F28 introduced.** `fmt_iso_year` was applied to
every source, so `%tF` of a `Date`/`Calendar`/`long` past year 9999 gained a
`'+'` the JDK does not write there — the pre-F28 `{:04}` had that one right.

### 3.4 `lang_string.rs` — `minus` (§5.3) and `fmt_lower_case` (§5.4)

`FmtSymbols` gains `minus: char` (default `'-'`, which is `getMinusSign(null)`),
read in `fmt_symbols_for` alongside the other three. It is resolved eagerly with
them rather than lazily at `fmt_iso_year`, because its only consumer sits inside
`format_temporal_field`, which has no `ctx` re-entrancy budget of its own and is
already handed the resolved symbols. `fmt_localize_digits` maps ASCII digits
only, so a U+2212 survives it and the ASCII date separators survive it too —
asserted.

`fmt_lower_case` is `fmt_upper_case`'s twin: `locale_language_for_case_mapping`
then `case_map::to_lower_case`, with the two non-`Some` `FmtLocale` arms
collapsed onto the default locale exactly as the upper side does (the JDK's
`requireNonNullElse(l, getDefault(FORMAT))` — an explicit null takes the
**default** here, even though the *string* it maps is the English literal in
that case). `%tp` uses it instead of `str::to_lowercase`.

No measured row moves (§2.5). It is landed because the alternative is leaving
one JDK rule with two implementations, one of them fixed — this codebase's most
common defect shape, and the exact mirror of the `W8-F4-1` N3 upper-case defect.

---

## 4. Verified vs. assumed

**Verified (measured on HotSpot 25, or read in `C:\craton\jdk25src`):**

* Every cell in §2.1–§2.5, under the zone/locale each names.
* `Formatter.java`'s two `printDateTime` printers, their `CENTURY`/`YEAR_2`/
  `YEAR_4` arms, both `ISO_STANDARD_DATE` arms, both `AM_PM` arms,
  `leadingSign`/`trailingSign`, and that `getMinusSign` is declared once and
  used once.
* `NativeContext::read_string`'s class guard, in `vm/src/vm/vm_exec.rs:11713`
  — the fact that decides §3.1.
* `locale_bootstrap` registers both `Locale.getDefault` overloads as natives and
  `decode_category` degrades to `Base`; `ObjectRef`, `pin_native_root`,
  `read_native_pin`, `unpin_native_roots`, `class_id_by_name` (`&self`),
  `static_field_index_by_name`, `get_static_field`, `invoke` — all signatures
  read before use.
* `case_map::to_lower_case` exists, is `pub`, and `JDK_UNMAPPED_CASE_CODE_POINTS`
  is `[A7CE, A7CF, A7D2, A7D3, A7D4, A7D5]`.
* `rustfmt --check` on COPIES of both files, with the sibling modules present so
  `mod` resolution succeeds: **no diff hunk falls inside any block this lane
  wrote**. One reflow rustfmt wanted in `formatter_default_locale` was applied.
  Line endings verified LF after every edit (`tr -cd '\r' | wc -c` = 0 for both
  files — note `grep -c $'\r'` lies in this shell).

**Assumed / PREDICTED (no build, no test run, no VM execution — lane constraint):**

* That the crate compiles. Every changed signature's call sites were updated:
  `fmt_iso_year` 1 production + 12 test calls, `FmtSupport` 5 literals + 5 test
  constants, `FmtSymbols` 2 literals + 1 test literal, `formatter_sink_text` 2,
  `formatter_default_locale` 4. **Unverified by a compiler.**
* PREDICTED — checks that **flip** (were wrong, now right):
  * `new Formatter()` / `new Formatter(Appendable)` followed by any name field,
    zone field, separator or grouped number, on a host whose default locale is
    not US — `%tb`, `%tB`, `%ta`, `%tA`, `%tp`, `%tZ`, `%tc`, `%tr`, `%,d`,
    `%.2f`, and the digits under a non-Latin-digit locale. This is F28 §3.4's
    stated residual, and it is now closed rather than traded.
  * `new Formatter(sb).toString()` after any `format` — `""` → the text.
  * `%tY`, `%ty`, `%tC`, `%tD`, `%tc` for any date at or before proleptic year
    0, on **every** source shape.
  * `%tF` of a `Date`/`Calendar`/`long`: BC dates and years past 9999.
  * `%tF` of a `java.time` source with a negative year under any of the 59
    U+2212 locales.
* PREDICTED — checks that **become reachable**: **none.** This lane does not
  widen F28's 79-cell refusal set by one cell. `fmt_temporal_fault_char` is
  untouched, `FmtSupport`'s five support flags are untouched, and
  `calendar_printer` is read by exactly one arm (`%tF`) which was already
  answering.
* PREDICTED — **nothing else moves.** `year_of_era == year` for every year >= 1,
  which is every date any real application formats; `sym.minus == '-'` for 1099
  of 1158 locales and for every explicit-null call; `fmt_lower_case` agrees with
  `str::to_lowercase` on every AM/PM string in CLDR (§2.5).

---

## 5. NOMINATIONS

### 5.1 OPEN — `Formatter.out()` returns a `String`, which is not an `Appendable`

`native-builtins/src/lib.rs`, both registrars: `out()` is declared
`()Ljava/lang/Appendable;` and returns slot 0 verbatim. After the `()V` native
that is a `java.lang.String`, which does **not** implement `Appendable`.
Measured: `new Formatter().out().getClass()` is `java.lang.StringBuilder` on
HotSpot 25 (§2.2). Any caller that does `f.out().append(…)` — or any verifier
that checks the return type — is looking at an object of the wrong type.

Not fixed here because the honest fix is to make slot 0 a real `StringBuilder`
for the `()V` path, which is the deletion of §3.1 by another route and needs a
build. `formatter_sink_text` is the piece that was missing; this is the piece
that is still missing.

*exact literal old text* (real-JDK registrar, and the identical
`native_formatter_out` in the synthetic one):
```rust
    registry.register(f, "out", "()Ljava/lang/Appendable;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        Ok(Some(ctx.get_field(this, 0)))
    });
```
The change is not a one-liner at this site — it is at the `()V` constructor,
which must allocate a `java/lang/StringBuilder` instead of a `String`, and then
`format`'s `read_string` fast path stops firing for it. Whoever takes it should
take §3.1's deletion at the same time; they are the same change.

### 5.2 OPEN — the `java.time` sources still decode by per-class getters

Unchanged from F28's own §6. `extract_temporal_fields` asks
`getYear`/`getHour`/… by name and `invoke_i32` answers `0` for a method the
class lacks; `FmtSupport` *describes* the JDK's partition but does not make the
VM ASK the object. A `TemporalAccessor.isSupported(ChronoField)` route would be
the structurally correct one, and it would also delete `FmtSupport`'s
hand-maintained per-arm table. It costs one virtual call per field decoded where
today there are none, and it is a bigger change than any single lane's brief.
`calendar_printer` would survive that refactor unchanged — it is a printer
question, not a field question.

### 5.3 CLOSED — F28 §5.3's second clause

Recorded so it is not re-opened from the old text: "every negative `%d`/`%f`/
`%e`/`%g` uses `getMinusSign(l)`" is **wrong**, measured under five U+2212
locales (§2.4). `fmt_localize` must NOT gain a minus mapping. If a future lane
sees `%d` of `-5` render as U+2212 on CratonVM, that is a regression, not a fix.

---

## 6. Left undone

* No build, no test run, no VM execution — lane constraint. §4 lists what that
  leaves unverified.
* §5.1 and §5.2.
* The year and minus rules are asserted as pure functions in
  `lang_string.rs`'s test module. There is no end-to-end vector in
  `regression-suite` for `%tF` under `lt-LT` or for a BC `GregorianCalendar`,
  and adding one is a different file.
* `%tF`'s `yearField` is `ChronoField.YEAR` only for an `IsoChronology`
  argument and `YEAR_OF_ERA` otherwise (`Formatter.java`, the
  `t.query(TemporalQueries.chronology()) instanceof IsoChronology` test). Every
  `java.time` type CratonVM decodes is ISO, so the branch is unreachable here;
  a `JapaneseDate`/`HijrahDate` argument would land in
  `extract_temporal_fields`' catch-all and be refused outright, which is a
  different divergence and not one this lane measured.
