# W7-91 — `%t`/`%T` rendered its NAMES from hard-coded English tables, and the other half of `RJdkLogging`'s four bytes

> # RETIRABLE 2026-08-12 (third pass, lane A24). §5 — the one thing holding this
> # record open — is DISCHARGED IN SOURCE, and its successor residual is now
> # written at its own source site. See §9 for the reading and the one condition
> # that is source-level rather than measured. The second-pass banner below is
> # kept because its measurement is the headline's evidence.
>
> # ---- second pass, superseded in part by §9 ----
>
> # NOT RETIRED 2026-08-12 (second pass). The headline IS discharged; §5 is not,
> # and §5 is now the whole of this record.
>
> **Discharged, by measurement rather than by a `PASS`.** Re-run on the wave
> binary with the pristine-dev control beside it and HotSpot as the oracle:
>
> ```
>                      streamBytes  handlerLevelGate  defaultZoneRawOffsetMs
> HotSpot 25                   177                88               -10800000
> f8      --jdk-only           177                88               -10800000
> control --jdk-only           175                87                       0
> ```
>
> The month name is the whole of the `175 → 177` movement, which is this
> record's defect, closed.
>
> **§8's falsifier is SUPERSEDED — do not work from it.** It predicts
> `175 → 177` against HotSpot's `179`, i.e. still RED by one character per
> record. Both VMs read **177** in the same session, for two reasons that
> compound: W7-92 landed the hour half, and at 14:xx local the unpadded 12-hour
> field is one digit on every VM, so `H=1` for the broken VM and the fixed one
> alike. The arithmetic in §1 is right; the constants were a function of the
> clock, exactly as §6 warned. The judge is the live same-session diff.
>
> **What holds this record open is §5, "The numeric half, deliberately not
> moved"** — `String.format("%,.2f", x)` with no `Locale` localizes against ROOT
> where a real `Formatter` uses `Locale.getDefault(FORMAT)`. This record calls
> it *"probably wants taking — but it wants its own measurement"*, which is a
> deferral for want of a measurement, not a refusal on the merits, and
> RETIREMENT-20260812.md §3 rules that shape a LIVE row. Unlike W7-67's
> residual it is **not** written at its source site, so retiring the record
> would lose it. Its natural successor if anyone wants it closed rather than
> carried is W7-34-formatter-family-residuals.md, which owns the
> `Formatter`-receiver locale. See RETIREMENT-20260812B.md §3.2.

**Status: HEADLINE FIXED AND MEASURED 2026-08-12; §5's numeric half is the live
residual. Source landed for the name tables.** The measurement in §1 below was
taken against a pristine `dev` binary before the fix; the banner above is the
after.
The measurement in §1 is a real suite run against a pristine `dev` binary
(44044c7e2, no lane edits) handed to this lane by the orchestrator; everything
below it about CratonVM is read from source, and the JDK columns are read from
`jdk25src` (JDK 25 `src.zip`, on this host) rather than remembered.

---

## 1. The measurement, and what the four bytes are

`RJdkLogging` FAILS under `--jdk-only` on pristine `dev`. Both VMs print 63
checks and both pass their own assertions; the failure is the cross-VM diff, and
exactly two `CK` lines differ:

```text
HotSpot  : CK RJdkLogging streamBytes=179 marks=2
CratonVM : CK RJdkLogging streamBytes=175 marks=2
HotSpot  : CK RJdkLogging handlerLevelGate=ok bytes=89
CratonVM : CK RJdkLogging handlerLevelGate=ok bytes=87
```

Four characters over two records, two over one: **a uniform two per LOG
RECORD**, at both arities.

### 1.1 `%n` is refuted, quantitatively

The obvious reading — `SimpleFormatter`'s default pattern carries two `%n` per
record, so a `%n` emitting `\n` where Windows wants `\r\n` would be exactly two
per record — is **wrong**, and it is worth refuting by arithmetic rather than by
inspection, because both readings fit the headline number.

Source first. `native-builtins/src/lang_string.rs` has two `%n` arms, the
undecorated fast path and the decorated one, and both are
`if cfg!(windows) { "\r\n" } else { "\n" }`. `W7-71-jca-exception-types-and-line-separator.md`
§4.2 already checked all three sites in the tree and found the same thing; its
census named the five hard-coded-`\n` sites, and `%n` is not among them.

Now the arithmetic. `SimpleFormatter`'s default pattern is, verbatim from
`jdk/internal/logger/SimpleConsoleLogger.java`'s `Formatting.DEFAULT_FORMAT`:

```text
%1$tb %1$td, %1$tY %1$tl:%1$tM:%1$tS %1$Tp %2$s%n%4$s: %5$s%6$s%n
```

so one record is

```text
(month) ' ' (day 2) ', ' (year 4) ' ' (hour12) ':' (min 2) ':' (sec 2) ' ' (AM/PM 2) ' '
(source) CRLF (level) ': ' (message) (throwable) CRLF
```

`RJdkLogging.formattedOutputIsRealBytes` logs `WARNING`/`BYTES-MARK-ONE` and
`INFO`/`BYTES-MARK-TWO` from source `RJdkLogging formattedOutputIsRealBytes`
(38 chars); `handlerLevelIsASecondGate` logs one `SEVERE`/`HL-SEVERE-MARK` from
source `RJdkLogging handlerLevelIsASecondGate` (37). Writing `M` for the month
length and `H` for the unpadded 12-hour length:

| | HotSpot | CratonVM |
|---|---|---|
| `streamBytes` | `2M + 2H + 167` | `2M + 2H + 167` |
| `handlerLevelGate` | `M + H + 83` | `M + H + 83` |

Solving each VM's two measurements: **HotSpot `M=4, H=2`; CratonVM `M=3,
H=1`.** Both solve to integers in range, from two independent equations each,
and the four `CRLF`s are *inside* those constants — a CratonVM emitting `\n`
would have made them 163 and 81, i.e. 171 and 85, not 175 and 87. So `%n` is
correct on both sides and the deficit is two different one-character defects:

1. **`M`: the month name.** HotSpot on this ru_RU host renders `%1$tb` as
   `авг.` (4). CratonVM renders `Aug` (3). That is this record.
2. **`H`: the hour.** HotSpot's unpadded 12-hour field is two digits and
   CratonVM's is one — a three-hour shift on a host at UTC+3, i.e. CratonVM is
   rendering the instant in **UTC**. That is §4, and it is *not* fixed here.

`авг.` is `MonthAbbreviations[7]` of `sun.text.resources.cldr.ext.FormatData_ru`
in this JDK image, read from `jdk25src`. Russian `AmPmMarkers` is `{"AM","PM"}`
in the same file, which is why `%1$Tp` contributes nothing, and there is no
`sun/util/logging/resources/logging_ru`, which is why `%4$s` stays `WARNING` on
both and the vector's own `contains("WARNING")` passes on HotSpot.

**This corrects `W7-80-locale-data-stage-two.md` §3 ("Why the gap is exactly two
bytes") and its §7 expectation that `RJdkLogging` reaches 177 and goes green.**
W7-80 was right that `%1$tb` is worth one character per record and right that
`DateFormatSymbols` for ru now answers `авг.`; it assumed HotSpot's number was
177 without measuring it. It is 179, and W7-80's own premise — that
`java.util.Formatter` renders `%tb` from `getShortMonths()` — is true of HotSpot
and was **false of CratonVM**, where `%t` never reached `DateFormatSymbols` at
all. That is the defect below.

## 2. The mechanism

`format_temporal_field` (`native-builtins/src/lang_string.rs`) carried four
`const` arrays — `MONTHS_ABBR`, `MONTHS_FULL`, `DAYS_ABBR`, `DAYS_FULL` — and an
`ampm` computed as `if hour < 12 { "am" } else { "pm" }`. Every `%t` conversion
that renders a name read one of them, unconditionally, with no locale in scope:
the function did not take one.

The census — every conversion affected, against JDK 25's
`Formatter$FormatSpecifier.print(Formatter, StringBuilder, TemporalAccessor,
char, Locale)`:

| conversion | JDK 25 reads | was |
|---|---|---|
| `%tB` | `DateFormatSymbols.getMonths()[m-1]` | `MONTHS_FULL` |
| `%tb`, `%th` | `getShortMonths()[m-1]` | `MONTHS_ABBR` |
| `%tA` | `getWeekdays()[dow]` | `DAYS_FULL` |
| `%ta` | `getShortWeekdays()[dow]` | `DAYS_ABBR` |
| `%tp`, `%Tp` | `getAmPmStrings()[ampm]`, lower-cased with the locale | literal `am`/`pm` |
| `%tr` | its own `AM_PM` sub-print, upper-cased | literal `AM`/`PM` |
| `%tc` | its own `NAME_OF_DAY_ABBREV` + `NAME_OF_MONTH_ABBREV` sub-prints | both tables |

Seven specifier spellings, five arrays, one omission: the locale never reached
the function. Everything else in the `%t` family is a number, and numbers were
already right.

The reach is wider than a differential probe row. `String.format` is registered
`Intrinsic` by `register_string_format_real_jdk_natives`
(`native-builtins/src/lib.rs`, reached from `register_essential_natives_with_shims`),
and `Intrinsic` is the one kind `--jdk-only` does not yield back to bytecode —
so this native is what runs in **both** shipping modes, for every caller. The
one that reaches a user without asking for it is
`java.util.logging.SimpleFormatter`, whose default pattern opens `%1$tb`: every
JUL line on a non-English host dated its month in English.

## 3. The fix

`fmt_date_name(ctx, locale, getter, index)` reads one element of one
`java.text.DateFormatSymbols` array, and the seven arms above call it with the
getter the JDK calls. On `None` the four tables answer, exactly as before.

Three things in it are load-bearing:

* **The re-entrancy latch, copied from W7-34's.** `FMT_DATE_NAMES_RESOLVING` is
  the `DateFormatSymbols` twin of `FMT_SYMBOLS_RESOLVING` in the same file.
  Resolving symbols runs the locale provider chain, `ResourceBundle`, and —
  since W7-80 — CLDR bundle classes out of the JDK image, any of which may call
  `String.format` itself and re-enter this native asking for the same locale
  whose symbols are mid-construction. While the latch is set the inner call
  answers `None` and prints the English name, which is what the JDK's own
  null-locale branch prints and cannot recurse. Two latches rather than one
  shared latch, deliberately: a `%d` inside date-symbol resolution should still
  get its separators, and vice versa.
* **`None` means the NO-LOCALE overload, and resolves through
  `DateFormatSymbols.getInstance()`** — whose body is
  `getInstance(Locale.getDefault(Locale.Category.FORMAT))`, i.e. exactly the
  `Locale` a real `java.util.Formatter` built by `String.format(String,
  Object...)` carries. Asking the JDK rather than composing a `Locale` here is
  what keeps this from becoming a fourth opinion about what the default locale
  is, next to `vm_init::derive_host_locale`, `locale_bootstrap` and the
  `user.*` properties.
* **Failure is `None`, not a refusal.** Synthetic-JDK mode has no
  `java.text.DateFormatSymbols.getInstance`, a jlinked image can be missing
  `jdk.localedata`, and both arrays carry an empty trailing entry. Every one of
  those falls back to the English table. The `Err` from `invoke` is dropped for
  the reason `fmt_raise`'s speculative `load_class` drops its own: the caller is
  owed a formatted string, not this lookup's failure, and a thrown exception
  travels in the return value here rather than in thread state, so nothing is
  left pending.

Two corrections came with it, both read out of the JDK rather than judged:

* **`%tc`'s day was space-padded.** The JDK's `DATE_TIME` composite uses
  `DAY_OF_MONTH_0`, i.e. zero-padded (`Sat Nov 04 …`, its own javadoc example).
  `{:2}` was `%te`'s rule. Now `{:02}`.
* **`%tp` lower-cases what it read**, rather than emitting a literal. For a
  locale whose markers are already `AM`/`PM` — German and Russian both — this
  changes nothing, which is why it is not visible in §1's arithmetic.

## 4. What this does NOT close: the hour, and it is the other two bytes

`RJdkLogging` is still expected RED after this change, at `streamBytes=177`
against HotSpot's `179`. **Do not read a 177 as this fix having failed** — it is
this fix having landed and §1's second defect still being open.

`SimpleFormatter.format` builds its argument as
`ZonedDateTime.ofInstant(record.getInstant(), ZoneId.systemDefault())`, so the
wall clock is fixed before `String.format` ever sees it, and
`extract_temporal_fields`' `ZonedDateTime` arm reads the already-zoned
`getHour()`. The shift is in `ZoneId.systemDefault()`. Three producers, all
answering `UTC` on Windows, in dependency order:

1. `native-builtins/src/lib.rs`, `native_timezone_get_system_id`
   (`TimeZone.getSystemTimeZoneID`) — `let tz_id = "UTC";` with the comment *"A
   more complete implementation would query the OS"*. This is the **live**
   one: it is a real JDK `native` method, so it is registered on the essential
   path, and real `TimeZone.setDefaultZone()` bytecode calls it whenever
   `user.timezone` is empty.
2. `vm/src/vm/vm_init.rs` seeds `user.timezone` from `$TZ` alone, which Windows
   does not set — so it is empty, and (1) decides.
3. `native-builtins/src/util_time.rs`, `os_default_zone_id` — its
   `#[cfg(windows)]` arm computes nothing and returns `"UTC"`, saying so in its
   own comment. It is only `jvm_default_zone_id`'s fallback for when the real
   `TimeZone` round-trip fails, and that round-trip succeeds here, so **fixing
   this one alone is inert** — a marker comment is left there rather than a
   half-fix.

A real fix reads `HKLM\SYSTEM\CurrentControlSet\Control\TimeZoneInformation`'s
`TimeZoneKeyName` and maps it through `<java.home>/lib/tzmappings`, which is
what `java_props_md.c` does and what `getSystemTimeZoneID` is handed `javaHome`
*for*. That is a feature with a registry dependency, not a line, and it is in
another lane's files. It wants its own record and its own owner.

It is also worth stating what it is worth: the two bytes in `RJdkLogging` are
cosmetic, but a VM whose default time zone is UTC on every Windows host renders
**every** `java.util.Date`, `Calendar` and `ZonedDateTime.now()` three hours off
here, and that is not cosmetic.

## 5. The numeric half, deliberately not moved

`String.format("%,.2f", x)` with no `Locale` still localizes against ROOT
(`FmtSymbols::default()`), where a real `Formatter` would use
`Locale.getDefault(FORMAT)`. It is the same one-line question — what does a
`None` locale mean — answered differently on purpose:

* The **name** half has a bounded blast radius: seven specifiers nothing in the
  corpus formats except `SimpleFormatter`, and it is the measured red.
* The **numeric** half moves every unpinned `%f`, `%e`, `%g` and `%,d` in the
  corpus and in every application, on any non-en host, in both shipping modes.
  It is a HotSpot-parity fix too and probably wants taking — but it wants its
  own measurement, and it is not what the four bytes are.

Note the asymmetry is at least *stated* rather than accidental: the file's
existing treatment of a `None` locale — `'0'`/`'.'`/`','` — is precisely
HotSpot's **explicit-null** semantics (`getZero(null)` and friends), so it is
right for `String.format((Locale)null, …)` and wrong only for the no-locale
overloads. The name half now resolves `None` the other way, so an explicit
`String.format((Locale)null, "%tb", d)` gets the default FORMAT locale's month
where HotSpot gets `Locale.US`'s. That case cannot be told apart at this layer
today — `Formatter`'s no-locale constructors store null in slot 1 as well — and
the reading that serves the common caller was taken.

## 6. Coverage

`regression-suite/src/RStrings.java` (`CORE_CLASSES`), **+7 checks, 39 → 46**.
Each is an equality between `String.format(Locale.GERMANY, "%tX", …)` and the
same locale's `DateFormatSymbols` array, never against a pinned `Mär`: both
sides read one array, so a platform without German data cannot manufacture a
red, and the instant is a fixed `LocalDateTime.of(2026, 3, 4, 5, 6, 7)`, so
nothing follows the calendar. `LocalDateTime` and not `Date`/`Calendar` because
those go through §4's UTC model and would import a second defect into this
check.

Three of the seven earn their place separately:

* the no-locale overload against `DateFormatSymbols.getInstance()` — the
  `SimpleFormatter` shape, and the one that moves the measured red;
* an explicit `Locale.US` row, which fails an implementation that answered the
  default locale for an explicit one, or German for everything;
* `CK RStrings deMonthAbbrLen=… deWeekdayLen=… deNames=…`, the antecedent —
  lengths and not the names, because that line goes through the harness's
  byte-for-byte cross-VM diff and printing `Mär` in it would put the two VMs'
  stdout encodings on trial inside a row about month names. If
  `deNames=false` this platform's German names *are* the English ones and the
  implications passed vacuously — green on the old behaviour too, and visible
  rather than silent. That is W7-34's discipline for its
  `DecimalFormatSymbols.getInstance(Locale.GERMANY)` guard, copied.

No expectation here is a byte count and none is date-dependent. **`streamBytes`
itself is deliberately not asserted anywhere** — 179 is a property of August on
a ru_RU host in this JDK's CLDR (`мая` is three characters, so the same correct
VM prints a smaller number in May); the suite judges that row by a live
same-session diff against HotSpot, which is why it caught this at all.

## 7. Compatible mode

`--real-jdk` is frozen except for HotSpot-parity fixes, and this is one: it
replaces a value that disagreed with HotSpot with the value HotSpot reads, out
of HotSpot's own data. On an en_US host nothing moves — CLDR `en`'s
`MonthAbbreviations`/`DayNames`/`AmPmMarkers` are the four tables, entry for
entry — except `%tc`'s day padding, which was wrong in en too. On a non-en host
every row above moves toward HotSpot.

No new `CRATONVM_*` env var, so no `flag_groups.rs` / `flag-surface.txt` /
`flag-tokens.md` / `flag-inventory.md` churn. No new registration, so no
`bridge_shadows_bytecode`, `stub_ratchet` or kind-map movement: every edit is
inside an existing native body.

## 8. Re-measuring

```
CRATONVM_ARGS=--jdk-only SUITE=all bash regression-suite/run.sh
```

unpinned, on this ru_RU host, with a binary built from this branch:

1. `RStrings` must print `PASS RStrings (46 checks)` on **both** VMs, and
   `CK RStrings deNames=true`. A `deNames=false` means the five German
   implications measured nothing.
2. `CK RJdkLogging streamBytes=` must move `175 → 177` against HotSpot's `179`,
   and `handlerLevelGate bytes=` `87 → 88` against `89`. **Those exact
   residual gaps are the falsifier**: one character per record left, and it is
   §4's hour. Anything else — no movement, or full agreement — means something
   other than this change happened.
3. `probes/DefaultLocaleProbe.java`'s `simpleFormatterHead` row must stop
   reading `Aug` on the ru arm.

A pinned run (`-Duser.language=en -Duser.country=US` on both sides) stays
byte-identical, before and after, and is not evidence either way.

---

## 9. §5 is discharged, §4 is discharged, and what replaces them (2026-08-12, lane A24)

Doc-only lane: nothing here was built or run. Every claim is read from
`native-builtins/src/lang_string.rs` and names its site. No file outside this
record was edited.

### 9.1 §5's numeric half — DISCHARGED

§5 deferred `String.format("%,.2f", x)` with no `Locale` for want of a
measurement, and the second-pass banner correctly ruled that a LIVE row. It has
since been taken, and taken in the shape §5's own last paragraph said was needed
— **the ambiguity is removed rather than resolved to one side.**

`lang_string.rs:5765` declares

```rust
enum FmtLocale {
    /// The overload has no `Locale` parameter, so the JDK supplies
    /// `Locale.getDefault(Locale.Category.FORMAT)`.
    DefaultFormat,
    /// The overload carries an explicit `Locale` argument, possibly `null`.
    Given(Option<cratonvm_types::ObjectRef>),
}
```

and the two consumers now agree instead of drifting:

* `fmt_symbols_for` (`:5827`) — `Given(None)` returns `FmtSymbols::default()`
  (the root constants, which is `Formatter.zero(Locale)`'s literal behaviour for
  a null locale, quoted at the site); `DefaultFormat` resolves through the
  **no-arg** `DecimalFormatSymbols.getInstance()`, whose body is
  `getInstance(Locale.getDefault(Locale.Category.FORMAT))`;
  `Given(Some(l))` is unchanged.
* `fmt_date_name` (`:5932-5948`) — the same three arms, with
  `DefaultFormat | Given(None)` sharing the `DateFormatSymbols.getInstance()`
  route for the reason §9.3 gives.
* `format_impl` is entered as `FmtLocale::DefaultFormat` from the no-`Locale`
  overload (`:6018`) and as `FmtLocale::Given(locale)` from the explicit one
  (`:8157`).

The declaration's own doc names this record: *"Collapsing the two onto one `None`
is the defect this type removes (W7-91 §5, and the last open `format` row of
W7-34-formatter-family-residuals.md)"*. Both re-entrancy latches survive —
`FMT_SYMBOLS_RESOLVING` (`:5782`) and `FMT_DATE_NAMES_RESOLVING` (`:5885`) —
which is what §3 said was load-bearing and what W7-34 named as the worst hazard
on exactly this path.

**§5's asymmetry paragraph is discharged with it.** §5 closed by admitting that
`String.format((Locale) null, "%tb", d)` "cannot be told apart at this layer
today". It can now: that is the whole point of `Given(None)` versus
`DefaultFormat`.

### 9.2 §4's hour — DISCHARGED by W7-92, verified in source

§4 named `native_timezone_get_system_id` as the live producer of "the system zone
is UTC", quoting its body as `let tz_id = "UTC";`. That body is gone. The
function at `native-builtins/src/lib.rs:29425` now takes `javaHome` and queries
the platform, and its doc opens *"W7-92: this used to be `let tz_id = "UTC";`"* —
naming the second producer (`timezone_default_ref`, what `TimeZone.getDefault()`
resolves to in Compatible mode) as the other half, and stating that unresolvable
hosts deliberately keep answering `"UTC"` rather than the JDK's `null`. §4's
prescription (read the registry key, map through `tzmappings`) is what landed.
This is a source reading; the second-pass banner's live same-session diff is the
measurement.

### 9.3 What §5's discharge does NOT close — and it is now written at its source site

A narrower residual replaces §5, and it is **recorded in the code**, at
`lang_string.rs:5906-5919`, under the heading *"`Given(None)` deliberately does
NOT take the JDK's `Locale.US` branch"*:

> `Formatter.printDateTime` opens every name field with
> `Locale lt = ((l == null) ? Locale.US : l)`, so a literal
> `String.format((Locale) null, "%tB", d)` renders English on HotSpot, where this
> renders the default FORMAT locale. That divergence is kept ON PURPOSE, because
> in this VM `Given(None)` is not only an explicit null: **`new Formatter()` and
> `new Formatter(Appendable)` reach `format` through natives in
> `native-builtins/src/lib.rs` that write `null` into the receiver's locale
> slot**, and for THOSE the JDK's answer is the FORMAT default, not English.

So one of the two readings is wrong until that constructor writes what the real
`java.util.Formatter()` constructor writes. The comment routes the fix to W7-34's
residuals "with the constructor patch that closes it", and W7-34 is the record
that owns the `Formatter`-receiver locale — which is exactly the successor the
second-pass banner nominated.

**That is what makes this record retirable rather than merely quieter.** The
second-pass banner's stated reason for holding it open was: *"Unlike W7-67's
residual it is **not** written at its source site, so retiring the record would
lose it."* That premise no longer holds. The successor residual is written at
its source site, in full, with its owner named — and a guard scoped by a stated
premise is only as good as the premise, so the premise was the thing to re-check.

### 9.4 A defect found and fixed in passing that this record would have caught

The same lane found `printf(Locale, …)` was **dropping its locale** — i.e. the
explicit-`Locale` overload behaved as though no locale had been given. Under the
old code that was invisible, because the no-`Locale` path also localized against
ROOT; the moment §9.1's fix makes the two paths differ, it becomes a live wrong
answer on any non-en-US host.

`regression-suite/src/RJdkHello.java:99` is the vector, and it is worth naming
because it is nearly a false negative:

```java
ps.printf(Locale.ROOT, " [%s|%d|%05.2f]", "x", 7, 1.5);
...
check(got.equals("Hello, world! 42 3.5 true ab [x|7|01.50]"), "PrintStream body: " + got);
```

The explicit locale is `Locale.ROOT`, so on an **en-US host the row is green
either way** — dropping a ROOT locale and defaulting to ROOT are the same answer.
On this ru_RU host `%05.2f` would have rendered `01,50` and the pinned string
comparison would have gone red. `RJdkHello` is in `run.sh`'s `JDKONLY_CLASSES`,
so that row **is** scheduled — which is why the fix is covered. Recorded here
because it is the same one-line question (*what does an absent `Locale` mean*)
answered at a third site, and because it is a clean instance of a pinned fixture
that discriminates only on a non-default host.

### 9.5 Disposition

**RETIRABLE.** Not marked RETIRED by this lane, for two honest reasons:

1. **§9.1 and §9.2 are source readings, not measurements.** This lane cannot
   build or run. The headline's discharge is measured (second-pass banner); §5's
   is not. A run of `CRATONVM_ARGS=--jdk-only SUITE=all bash
   regression-suite/run.sh` on a ru_RU host with `RStrings` at 46 checks and
   `CK RStrings deNames=true` is what converts this to RETIRED, and §8's steps 1
   and 3 are still the right instrument for it (step 2 is superseded — see the
   second-pass banner).
2. **Retirement here is a process with an owner.** `RETIREMENT-20260812B.md` §3.2
   already carries this record's row; moving it belongs to the lane that owns
   those files, not to this one. What this lane owes that lane is the finding
   that §5 is discharged and its successor is now source-sited, which is §9.3.

**Coverage that is scheduled**, so the next reader does not re-derive it:
`RStrings` (§6's +7 checks) and `RJdkHello` (§9.4) are both in `run.sh` word
lists — `CORE_CLASSES` and `JDKONLY_CLASSES` respectively. `RJdkLogging` (§1's
measurement) is in `JDKONLY_CLASSES`. `probes/DefaultLocaleProbe.java` (§8 step
3) is **not** scheduled: the string `probes` occurs zero times in `run.sh` at any
`SUITE=` value.

No nominations from this section — everything it describes has landed, and the
one open item is owned by W7-34.
