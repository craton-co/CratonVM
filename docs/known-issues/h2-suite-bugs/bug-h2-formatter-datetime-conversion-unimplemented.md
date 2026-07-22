# `String.format`/`java.util.Formatter` — the entire `%t`/`%T` date-time conversion category is unimplemented and silently passes the format specifier through as literal text

## Status
**OPEN** — new finding, 2026-07-21.

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
