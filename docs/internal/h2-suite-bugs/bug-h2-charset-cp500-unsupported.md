# H2 — `Unsupported charset: cp500` (jdk.charsets / extended charsets missing)

## Status
**FIXED 2026-07-22** — see
[`../fixed-suite-bugs/bug-h2-charset-cp500-unsupported-FIXED.md`](../fixed-suite-bugs/bug-h2-charset-cp500-unsupported-FIXED.md)
for the fix (a curated IBM500 codec, not the full `sun.nio.cs.ext` provider).
The rest of this file is kept as the original 2026-06-15 sweep report.

## Original status (historical)
~~OPEN~~ — missing charset provider.

## Severity
**MEDIUM** — fails `TestCharsetCollator` at construction; any app requesting an
extended (IBM/EBCDIC/ISO-2022/etc.) charset is affected.

## Affected test classes (mem config)
`org.h2.test.unit.TestCharsetCollator`

## Symptom
```
java.lang.ExceptionInInitializerError
Caused by: java.lang.IllegalArgumentException: Unsupported charset: cp500
    at org.h2.test.unit.TestCharsetCollator.<init>(TestCharsetCollator.java:20)
```
The exception is thrown from the test's static/instance init (it references the
`cp500` charset), so the class never runs — RunOne sees the throwable escape
before `runTest` and the process exits without a result line (classified CRASH).

## HotSpot behavior
PASS — `cp500` (EBCDIC International) is provided by the `jdk.charsets` module.

## Root cause
CratonVM's `Charset.forName`/provider lookup only covers the `java.base`
standard charsets (`UTF-8`, `ISO-8859-1`, `US-ASCII`, `UTF-16*`, …). The
extended charsets that the JDK ships in the **`jdk.charsets`** module
(`sun.nio.cs.ext.*` — IBM/EBCDIC code pages like `cp500`/`IBM500`, the
ISO-2022 family, GB18030, etc.) are not registered, so `forName("cp500")`
throws `UnsupportedCharsetException`/`IllegalArgumentException`.

## Fix options
- Register the extended `sun.nio.cs.ext` charset provider (or a curated subset
  incl. the IBM/EBCDIC code pages) so `Charset.forName` resolves them.
- At minimum, surface a clear `UnsupportedCharsetException` (the JDK's checked
  type) rather than a bare `IllegalArgumentException`, and document which
  charsets are supported.

## Repro
`java -cp temp;ext org.h2.test.RunOne org.h2.test.unit.TestCharsetCollator mem`
or simply `Charset.forName("cp500")`.
