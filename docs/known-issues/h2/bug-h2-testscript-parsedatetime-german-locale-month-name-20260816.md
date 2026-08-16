# `TestScript` — `PARSEDATETIME` fails to parse German month names (`Locale 'de'`, pattern `MMMM`)

## Status
**OPEN, confirmed CratonVM-specific** — found 2026-08-16 on a GC-sweep rerun
(`origin/dev` @ `f80a4b775`, Azure host `azureuser@20.80.105.49`).
Differential-verified against real HotSpot JDK 25: **HotSpot PASSes in 5.6s**,
same classpath, same H2 checkout, same `run-h2-suite.sh hotspot` harness.

## The failure
```
ERROR: org/h2/test/scripts/functions/timeanddate/parsedatetime.sql
line: 13
exp: >> 2001-02-03 00:00:00+01
got: > exception PARSE_ERROR_1
------------------------------
ERROR: script org.h2.jdbc.JdbcSQLDataException: Error parsing "3. FEBRUAR 2001"; SQL statement:
CALL PARSEDATETIME('3. FEBRUAR 2001', 'd. MMMM yyyy', 'de') [90014-249]
	...
Caused by: java.time.format.DateTimeParseException: Text '3. FEBRUAR 2001' could not be parsed at index 3
	at java.time.format.DateTimeFormatter.parseResolved0(DateTimeFormatter.java:2108)
	at java.time.format.DateTimeFormatter.parse(DateTimeFormatter.java:1936)
	at org.h2.expression.function.DateTimeFormatFunction.parseDateTime(DateTimeFormatFunction.java:229)
```
Both the mixed-case form (`3. Februar 2001`) and the all-caps form
(`3. FEBRUAR 2001`) fail at the same index (3 — right where the month name
starts, immediately after `"3. "`), ruling out a simple case-folding
difference and pointing at the month name itself not being recognized at
all.

## What this is
`PARSEDATETIME(text, format, locale)` (`DateTimeFormatFunction.parseDateTime`)
builds a `java.time.format.DateTimeFormatter` with pattern `"d. MMMM yyyy"`
and `Locale.forLanguageTag("de")`, then calls `.parse(text)`. The `MMMM`
pattern field is a full month name lookup against the formatter's locale —
resolved through `java.time.format.DateTimeTextProvider`, which in turn reads
locale-specific month names from the JDK's CLDR-derived locale data
(`java.time.format.DateTimeFormatterBuilder$LocaleStore`, ultimately backed
by `sun.util.locale.provider.CalendarDataUtility` / the CLDR locale
resources bundled in the JDK image). If CratonVM's locale-provider
implementation doesn't fully load or expose German month names for this
lookup — while English (the suite's dominant locale, silently exercised
constantly elsewhere) works fine — every `MMMM`-pattern parse against a
non-English locale would fail exactly like this.

## Why it matters beyond this one line
`PARSEDATETIME`/`FORMATDATETIME` with an explicit locale are the only place
in H2's own script test corpus that exercises non-English month-name
resolution through `java.time.format`. A CratonVM gap here would affect
*any* Java code doing locale-specific date/time text parsing or formatting
(`DateTimeFormatter.ofPattern(..., locale)`), not just this one H2 function —
worth checking beyond just German, since the gap (if it's "German locale
month data missing/incomplete") could plausibly affect other non-English
locales identically.

## Next steps
* Minimal repro outside H2: `DateTimeFormatter.ofPattern("MMMM",
  Locale.GERMAN).parse("Februar")` (or `.format()` the other direction) —
  isolate whether the gap is in parsing, formatting, or the locale data
  itself.
* Check which locales' month-name tables CratonVM's locale-provider natives
  actually populate — likely a `native-builtins` locale/calendar-data native
  that only ships English (or only the JVM's default locale) rather than the
  full CLDR set the real JDK provides.
* Once isolated, compare against the JDK's own `sun.util.locale.provider`
  classes running as ordinary bytecode (this environment runs `--java-home`
  real-JDK mode) to see whether CratonVM intercepts/shims the locale data
  lookup with a native, or whether the real JDK bytecode path itself hits an
  incomplete CratonVM primitive underneath (e.g. resource-bundle loading from
  the JDK image, `ResourceBundle.getBundle` for `sun.text.resources.de.*` or
  the CLDR provider's `.jimage` entries).

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home <jdk25-home> --nojit --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.scripts.TestScript
```
Fails deterministically and fast (well under the 900s cap) at
`functions/timeanddate/parsedatetime.sql:13`.
