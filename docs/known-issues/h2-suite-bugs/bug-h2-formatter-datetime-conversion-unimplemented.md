# `String.format`/`java.util.Formatter` — the entire `%t`/`%T` date-time conversion category is unimplemented and silently passes the format specifier through as literal text

## Status
**FIXED** — 2026-07-22, `dev@<merge-commit>` (branch `fix/h2-formatter-datetime-20260722`).

## Severity
**HIGH** — silent, no-exception data corruption for a widely-used part of
`java.util.Formatter`'s public API (every `%tY`, `%tm`, `%td`, `%tb`, `%tB`,
`%tH`, `%tM`, `%tS`, `%tF`, `%tT`, `%tc`, … conversion), not specific to H2.

## Affected test class
`org.h2.test.db.TestFunctions` (`testToCharFromDateTime`) — also fails on
the HotSpot JDK25 baseline, but for a **different, unrelated** reason
(`AssertionError: Expected: true got: false`, an earlier sub-assertion in
the same long `test()` method); this specific `%tb` failure is CratonVM-only
and occurs at a later point HotSpot never has trouble reaching.

## Symptom
```
java.lang.AssertionError: Expected: @4 12-%<*>TB-79 08.12.34.560000000 AM (31)
                             actual: 12-NOV-79 08.12.34.560000000 AM (31)
	at org/h2/test/db/TestFunctions.testToCharFromDateTime(TestFunctions.java:1399)
```
The test computes its own expected value at runtime as a cross-check against
H2's SQL `TO_CHAR`:
```java
String expected = String.format("%tb", timestamp1979).toUpperCase();  // TestFunctions.java:1397
```
`%tb` is the `Formatter` conversion for a date/time argument's
locale-abbreviated month name (real JDK: `"Nov"`, upper-cased by the test to
`"NOV"`). Under CratonVM this call does not raise an exception and does not
perform the conversion — it returns the literal, unprocessed format string
`"%tb"` (upper-cased to `"%TB"`, visible mixed into the assertion diff as
`%<*>TB` because `String.format`'s own `%` sigil collides with `TestBase`'s
diff-formatting of the failure message). H2's actual `TO_CHAR(X)` SQL
function (a completely independent, hand-written month-name table in H2's
own source, not `java.util.Formatter`) correctly produces `NOV` — confirming
the bug is specifically in `Formatter`'s `%t*` handling, not in date/time
value computation.

## Root cause
`native-builtins/src/lang_string.rs`'s `native_string_format` (the shared
Rust engine backing both `String.format` and `java.util.Formatter.format`,
per the comment in `lib.rs`: `"Delegate to the full String.format
implementation"`) parses each `%[flags][width][.precision]conversion`
specifier and dispatches on the single conversion character:
```rust
match spec {
    's' | 'S' | 'd' | 'f' | 'x' | 'X' | 'c' | 'C' | 'b' | 'B' | 'e' | 'E' | 'g'
    | 'G' | 'o' | 'h' | 'H' | 'a' | 'A' => { /* consume an argument, format it */ }
    _ => {
        // unrecognized conversion: echo the specifier back as literal text
        result.push('%');
        ...
        result.push(spec);
    }
}
```
**`'t'`/`'T'` are absent from the recognized-conversion list entirely.**
Real `java.util.Formatter` treats `t`/`T` as a *prefix* introducing a whole
second character (the actual date/time conversion, e.g. `b`/`Y`/`m`/`H`) —
this two-character convention isn't handled at all here; the parser doesn't
even know `t`/`T` need a lookahead character, so any `%t?` specifier falls
straight into the `_ =>` "unknown conversion, treat as literal" branch —
silently, with no exception, no argument consumed, and no diagnostic of any
kind.

## Verification (2026-07-21)
Standalone, no H2:
```java
java.util.Calendar c = java.util.Calendar.getInstance();
c.set(1979, java.util.Calendar.NOVEMBER, 12, 8, 12, 34);
java.util.Date d = c.getTime();
String.format("%tb", d);   // expect "Nov"
String.format("%tY", d);   // expect "1979"
```
- **HotSpot JDK25**: `Nov`, `1979` (etc. — all `%t*` conversions work).
- **CratonVM** (`cratonvm-h2-fail-triage-20260721 --java-home
  /home/victor/jdk25`): `%tb`, `%tY` (and every other `%t*`/`%T*` tried)
  come back completely unchanged — the literal input string, not even
  partially processed. Reproduces identically with `--nojit`.

## Fix direction
Add the `t`/`T` conversion category to `native_string_format`'s dispatch:
consume a second character after `t`/`T` to select the specific date/time
field (`H`,`M`,`S`,`L`,`N`,`p`,`z`,`Z`,`s`,`Q`,`B`,`b`,`h`,`A`,`a`,`C`,`Y`,`y`,
`j`,`m`,`d`,`e`,`R`,`T`,`r`,`D`,`F`,`c`), convert the consumed argument
(`Date`/`Calendar`/`TemporalAccessor`/`Long` are all legal argument types per
the `Formatter` spec) accordingly, and apply the same upper-case rule real
`Formatter` applies for the uppercase `T` prefix.

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.db.TestFunctions
```
or the standalone `String.format("%tb", new java.util.Date())` snippet above.

## Fix (2026-07-22)
Added the `t`/`T` conversion category to `native_string_format`'s dispatch in
`native-builtins/src/lang_string.rs`:

- A new `'t' | 'T'` match arm consumes the second (field) character and
  upper-cases the whole result when the prefix is `'T'`, matching real
  `Formatter` semantics.
- `extract_temporal_fields()` decodes the consumed argument's wall-clock
  `(year, month, day, hour, minute, second, nanos)` fields. Supported
  argument types: `Long`/boxed `java.lang.Long` (epoch millis), any
  `java.util.Date` subclass (`getTime()`), any `java.util.Calendar`
  subclass (`getTimeInMillis()`), `java.time.Instant`, `LocalDate`,
  `LocalTime`, `LocalDateTime`, `ZonedDateTime`, `OffsetDateTime` (via their
  standard `getYear`/`getMonthValue`/.../`getNano` accessors, dispatched
  through `invoke_virtual` so real-bytecode and native-synthetic instances
  both work). An unsupported argument type throws
  `IllegalArgumentException` (real `Formatter` throws the more specific
  `IllegalFormatConversionException`, a subclass — not reproduced here,
  consistent with the rest of this format engine's simplified error
  taxonomy).
- `format_temporal_field()` implements the field conversions:
  `H k I l M S L N p z Z s Q B b h A a C Y y j m d e R T r D F c`
  (time, date, and composite conversions). `Date`/`Calendar`/`Long` epoch
  millis are broken down with **no timezone shift** — this matches
  CratonVM's existing Calendar/Date model (`cal_to_epoch_millis`/
  `cal_from_epoch_millis` in `phases_early.rs`), which already treats epoch
  millis as raw wall-clock fields VM-wide; `'z'`/`'Z'` report a fixed
  `+0000`/`UTC` identity consistent with that. The epoch-day/calendar-math
  helpers are self-contained duplicates of `util_time.rs`'s equivalents
  rather than reusing them, because `util_time` sits behind the
  `synthetic-jdk` feature while `String.format` is a core native available
  regardless of feature flags.

### Verification
- Standalone repro (18 distinct `%t*`/`%T*` conversions against a
  `Calendar`-derived `Date`, plus a boxed-`Long` epoch-millis argument):
  byte-for-byte match against the HotSpot JDK25 baseline for every
  conversion tried, including `%tb`→`Nov`, `%tB`→`November`,
  `%TB`→`NOVEMBER`, `%tA`→`Monday`, `%tF`→`1979-11-12`, `%tr`→
  `08:12:34 AM`, `%tj`→`316`, etc. A non-Date argument to `%tz` throws
  (`IllegalArgumentException` under CratonVM vs. HotSpot's
  `IllegalFormatConversionException` — see caveat above). `%%` and `%s`
  continue to work unaffected.
- `org.h2.test.db.TestFunctions#testToCharFromDateTime`'s own
  `String.format("%tb", timestamp1979)` cross-check (line 1397) now
  produces `"NOV"`, matching H2's independent `TO_CHAR` implementation —
  confirmed with a minimal standalone JDBC repro
  (`TO_CHAR(X)` → `12-NOV-79 08.12.34.560000000 AM`, matching HotSpot
  exactly) run against `org.h2.Driver` directly.
- Full `TestFunctions` class: the `%tb` `AssertionError` at line 1399 is
  gone; `test()` now runs past `testToCharFromDateTime()` (called at line
  135) all the way to `testAnnotationProcessorsOutput()` (line 143, a
  later, unrelated sub-test — dynamic in-process javac/annotation-processor
  SQL function compilation, nothing to do with date/time formatting) before
  failing. That residual failure is tracked separately; see
  `bug-h2-testannotationprocessorsoutput-*.md` (spawned as a follow-up
  investigation, not yet filed as of this writing).
- No other `%t`/`%T` usages were found anywhere under `apps/` (H2, Spring,
  etc. test suites) via `grep -rE '"%[-+0 #(,<0-9.]*[tT][a-zA-Z]'`, so this
  fix has no other known regression surface within the suites already
  checked into this repo.
- `cargo test -p cratonvm-native-builtins --lib lang_string`: all 82
  pre-existing tests in this module still pass (no dedicated unit tests
  were added for the new dispatch — coverage here is the black-box
  HotSpot-parity repro above plus the real H2 integration test).
