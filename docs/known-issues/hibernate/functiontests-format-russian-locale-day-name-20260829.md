# `FunctionTests`/`StandardFunctionTests.testFormat` — date `format()` returns the OS's Russian locale day name

## Status
New finding, 2026-08-29, H2 complete-suite run. Same signature in both
classes. Matches this session's established "this specific Windows host's OS
locale leaking into output" pattern (seen previously in Tomcat's
`TestHttp2InitialConnection` `Content-Language` assertion and various socket
exception text) — plausible environmental artifact, not yet confirmed via a
HotSpot A/B on this exact host.

## Symptom

```
org.hibernate.orm.test.query.hql.FunctionTests.testFormat
org.hibernate.orm.test.query.hql.StandardFunctionTests.testFormat

java.lang.AssertionError:
Expected: is "Monday, 25/03/1974"
     but: was "понедельник, 25/03/1974"
```

The test formats a fixed date with a day-name pattern and asserts the
English day name (`"Monday"`); CratonVM produced the Russian day name
(`"понедельник"` = Monday) instead — the date value itself is correct, only
the locale used to render the day name differs.

## Why this is plausibly environmental, not a computation bug

This machine's Windows locale is Russian (the same signature has shown up
repeatedly this session in unrelated contexts — DB error text, HTTP
`Content-Language` headers). If CratonVM's date-formatting path defaults to
the OS/system locale where HotSpot's either doesn't, or resolves a JVM
default locale differently, that would produce exactly this: a correctly
computed date, formatted through the wrong `Locale`.

## Not yet done
- HotSpot A/B on this exact class/host — if HotSpot also prints the Russian
  day name here, this is a shared environment artifact and belongs on a
  "not a CratonVM bug" list instead. If HotSpot correctly prints `"Monday"`
  on the identical (Russian-locale) host, that's a real CratonVM defect in
  locale resolution for date formatting.
- Which code path resolves the locale for this specific `format()` HQL
  function — whether it reads `Locale.getDefault()`, a JVM `-Duser.language`
  equivalent, or something CratonVM populates from the OS differently than
  HotSpot does.

## Repro

```bash
cd apps/hib-suite-runner
cratonvm.exe --java-home <jdk25> -XX:+UseZGC <hibernate-orm classpath+args> \
  JUnitRunner org.hibernate.orm.test.query.hql.FunctionTests
```
