# `TestScript` — `PARSEDATETIME` could not parse German month names (`Locale 'de'`, pattern `MMMM`)

## Status
**FIXED 2026-08-16**, on branch `h2ki-20260816` off `origin/dev` @ `ecc09d40d`,
Azure host `azureuser@20.80.105.49`. Verified by a same-session differential
against real HotSpot JDK 25 on the same host, same JDK image.

The record below keeps the original report's failure text; everything under
"What it actually was" replaces its hypothesis, which was wrong in an
instructive way.

## The failure, as reported

```
ERROR: org/h2/test/scripts/functions/timeanddate/parsedatetime.sql
line: 13
exp: >> 2001-02-03 00:00:00+01
got: > exception PARSE_ERROR_1
------------------------------
ERROR: script org.h2.jdbc.JdbcSQLDataException: Error parsing "3. FEBRUAR 2001"; SQL statement:
CALL PARSEDATETIME('3. FEBRUAR 2001', 'd. MMMM yyyy', 'de') [90014-249]
Caused by: java.time.format.DateTimeParseException: Text '3. FEBRUAR 2001' could not be parsed at index 3
	at java.time.format.DateTimeFormatter.parseResolved0(DateTimeFormatter.java:2108)
	at org.h2.expression.function.DateTimeFormatFunction.parseDateTime(DateTimeFormatFunction.java:229)
```

## What it actually was — and what the original hypothesis got wrong

The original write-up guessed *"CratonVM's locale-provider implementation
doesn't fully load or expose German month names"*, and pointed at
`ResourceBundle`/`jimage`/`sun.text.resources.de.*`. **The German month names
were there the whole time.** Measured on the pristine `dev` binary, `--nojit`,
real-JDK mode, one probe process:

```
CK de.dfsMonths1=Februar          <- java.text.DateFormatSymbols: correct
CK de.monthFull=2                 <- java.time Month.getDisplayName(FULL, de)
CK de.format=3. Februar 2001      <- hmm: ofPattern("d. MMMM yyyy", de).format
CK de.parseFAIL=DateTimeParseException: ... could not be parsed at index 3
CK de.cdu=null                    <- CalendarDataUtility.retrieveJavaTimeFieldValueNames(gregory, MONTH, LONG, de)
```

`java.text` had German data (W7-80's CLDR reader, which walks the JDK image's
own `sun.text.resources.cldr.ext.FormatData_de`); `java.time` did not, because
**`java.time` never reaches `DateFormatSymbols`.** `DateTimeFormatterBuilder`'s
text fields resolve through `java.time.format.DateTimeTextProvider` →
`sun.util.locale.provider.CalendarDataUtility.retrieveJavaTimeFieldValueName(s)`,
and CratonVM overrides exactly those two statics
(`native-builtins/src/locale_resources.rs`) because the JDK's own
`LocaleServiceProviderPool` chain NPEs in its partial bootstrap.

Those two overrides answered **English only**, by an explicit
`locale_is_english(...)` gate, with the comment *"other languages return null so
the formatter keeps its existing numeric fallback rather than showing English"*.
The fallback is what `de.format=3. Februar 2001` above hides: the FORMAT
direction is not the same code path, so formatting looked right while `MMMM`
parsing had nothing to match. (`Month.getDisplayName(FULL, de)` = `2` is the
same gap with the disguise removed.)

The all-caps detail in the original report — *"both `3. Februar 2001` and
`3. FEBRUAR 2001` fail at index 3, ruling out a case-folding difference"* — is a
correct conclusion. H2 builds its formatter with
`new DateTimeFormatterBuilder().parseCaseInsensitive().appendPattern(format)`
(`DateTimeFormatFunction.getDateFormat`), so on HotSpot the all-caps form parses
and only the missing names can explain a failure of both.

## The fix

`native-builtins/src/locale_resources.rs`. The two `CalendarDataUtility`
overrides now read the requested locale's CLDR `FormatData` — the same
`load_cldr_table` reader W7-80 built for `DateFormatSymbols` — instead of a
hardcoded English table:

* `calendar_resource_key(field, style)` mirrors JDK 25's
  `CalendarNameProviderImpl.getResourceKeyFor` for the CLDR adapter and the
  `gregory` calendar, read out of `src.zip` rather than remembered: ERA ignores
  the standalone bit and spells itself `long.Eras` / `Eras` / `narrow.Eras`;
  MONTH and DAY_OF_WEEK take a `standalone.` prefix and a
  `Names`/`Abbreviations`/`Narrows` suffix; AM_PM takes `narrow.` only for
  narrow.
* `cldr_calendar_name_array` walks the JDK's own four candidate keys in order —
  `java.time.<key>`, `java.time.<key minus standalone.>`, `<key>`,
  `<key minus standalone.>`. The `java.time.`-prefixed rows are real
  (`java.time.long.Eras = [BCE, CE]` sits beside `long.Eras` in root
  `FormatData`), and they are why `retrieveJavaTimeFieldValueNames` exists
  separately from `retrieveFieldValueNames` at all; the unprefixed pair is the
  non-javatime retry `CalendarDataUtility` itself falls back to.
* `calendar_name_entries` reproduces `getDisplayNamesImpl`: skip empty slots,
  value base 1 for `DAY_OF_WEEK`, and **no map at all when the array has
  duplicates** (narrow month/day) unless the field is `AM_PM`, whose day-period
  slots legitimately repeat.
* The curated English tables survive as the fallback for an image with no CLDR
  data at all (synthetic-JDK mode, a jlinked image without `jdk.localedata`),
  and only for English/root — a non-English locale on such an image still gets
  the numeric fallback rather than English names.

One unrelated robustness fix rode along in the same function: the `HashMap` the
plural override builds is now pinned and re-read across the `create_string` /
`Integer.valueOf` / `put` allocations in its own loop.

## Verification

Same host, same JDK image, same probe class, one session (`probes`-style
throwaway `LocaleTimeProbe`, three locales × plain `ofPattern` and H2's exact
`parseCaseInsensitive().appendPattern(...).toFormatter(locale)` construction):

| row | HotSpot 25 | pristine `dev` | fixed |
|---|---|---|---|
| `de.monthFull` (`Month.getDisplayName(FULL, de)`) | `Februar` | **`2`** | `Februar` |
| `de.format` (`ofPattern("d. MMMM yyyy", de)`) | `3. Februar 2001` | `3. Februar 2001` | `3. Februar 2001` |
| `de.parse` | resolves 2001-02-03 | **`DateTimeParseException` @3** | resolves 2001-02-03 |
| `de.h2parseUpper` (`3. FEBRUAR 2001`) | resolves 2001-02-03 | **`DateTimeParseException` @3** | resolves 2001-02-03 |
| `de.cduSize` / `cduKeys` | 12 / German names | **`null`** | 12 / German names |
| the same five rows for `fr` | French | **numeric / null** | French |
| the same five rows for `en` | English | English | English (unchanged) |

Every row of the fixed arm is byte-identical to HotSpot's.

End-to-end, `org.h2.test.scripts.TestScript` on the fixed binary runs to
completion and `functions/timeanddate/parsedatetime.sql` no longer appears in
its error list at all.

## What this record does NOT close

`TestScript` as a whole is still RED on CratonVM for **other, unrelated**
reasons — the fixed-binary run reports 16 errors, none of them
`parsedatetime.sql`: constraint-enumeration ORDER in `testScript.sql:6425`,
`1.0E100` rendered as 101 digits in `datatypes/json.sql`, `SET COLLATION
TURKISH` rejected, `percentile.sql` `1.5` vs `1.50`, `cosh.sql` last-digit
precision, and `btrim.sql` losing a `U&`-escaped astral pair. HotSpot 25 was run
on the same classpath in the same session as the oracle and reports **0 errors,
exit 0**, so all 16 are CratonVM defects — they are simply different ones, with
no relationship to locale month names, and none of them is covered here.

## Repro (for a future regression)

```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home <jdk25-home> --nojit --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.scripts.TestScript
```

or, without H2 at all, three lines of Java:

```java
DateTimeFormatter f = new DateTimeFormatterBuilder().parseCaseInsensitive()
        .appendPattern("d. MMMM yyyy").toFormatter(new Locale("de"));
System.out.println(f.parse("3. FEBRUAR 2001"));            // must resolve, not throw
System.out.println(Month.FEBRUARY.getDisplayName(TextStyle.FULL, new Locale("de")));  // "Februar", never "2"
```

The second line is the cheaper tripwire: it fails with a bare `2` the moment the
`java.time` name path loses its locale again, and it needs no formatter, no
pattern and no H2.
