# H2 — `StringBuilder.repeat(int,int)` → `ArrayStoreException` (date/time formatting)

## Status
**FIXED** (worktree `fix/h2-suite-loop`, `native-builtins/src/lang_string.rs`).

## Severity
**HIGH** — ~17 H2 test classes FAIL outright; the method is on the `java.time`
formatting path so it touches almost any date/timestamp output.

## Symptom
```
java.lang.ArrayStoreException: arraycopy: incompatible array element types (src=Char, dest=Byte)
    at java.util.Arrays.copyOf(Arrays.java:3538)
    at java.lang.AbstractStringBuilder.ensureCapacityNewCoder(AbstractStringBuilder.java:282)
    at java.lang.AbstractStringBuilder.repeat(AbstractStringBuilder.java:2092)
    at java.lang.AbstractStringBuilder.repeat(AbstractStringBuilder.java:2136)
    at java.lang.StringBuilder.repeat(StringBuilder.java:456)
    at java.time.format.DateTimeFormatterBuilder$NumberPrinterParser.format(...)
    ...
    at org.h2.expression.function.DateTimeFormatFunction.formatDateTime(...)
```

## HotSpot behavior
PASS — all affected classes pass on HotSpot.

## Root cause
CratonVM represents `StringBuilder`/`StringBuffer` with a **synthetic layout**
(`char[] value` at field slot 0, `int count` at slot 1) and intercepts the
common mutators (`append(String|char|char[]|int|long|...)`) natively so they
operate on that layout.

`StringBuilder.repeat(int codePoint, int count)` (JDK 21+) was **not**
intercepted. It therefore ran the *real* `java.lang.AbstractStringBuilder`
bytecode, which assumes the real JDK compact-string layout (`byte[] value` +
`byte coder`). On a capacity grow it calls
`ensureCapacityNewCoder(...)` →
`value = Arrays.copyOf(value, newCapacity << coder)`. `value` is declared
`byte[]`, so javac emitted `Arrays.copyOf([B,I)[B`, whose body does
`System.arraycopy(value, 0, new byte[n], 0, …)`. At runtime `value` is the
synthetic **`char[]`**, so the bulk copy is `char[] → byte[]` →
`ArrayStoreException`.

`java.time.format.DateTimeFormatterBuilder$NumberPrinterParser` uses
`StringBuilder.repeat('0', n)` to left-pad numbers, so every formatted
date/time field with padding hit it.

## Fix
Intercept `repeat(int,int)` for both `StringBuilder` and `StringBuffer`
(`native_sb_repeat_codepoint`), implementing it directly against the synthetic
`char[]` layout:
- `count < 0` → `IllegalArgumentException` (JDK contract);
- `count == 0` → no-op;
- expand the code point to UTF-16 units (BMP → 1 unit, incl. lone surrogates
  appended verbatim to match `repeat((char)cp,n)`; supplementary → surrogate
  pair; out-of-range → `IllegalArgumentException`);
- append the repeated units via the existing `sb_append_chars` helper.

This matches the established interception pattern already used for `append`,
`insert`, `replace`, etc. (the synthetic layout requires every reachable
mutator to be intercepted).

## Affected test classes (mem config)
TestListener, TestCompatibilityOracle, TestCompatibilitySQLServer,
TestLinkedTable, TestMultiThreadedKernel, TestSequence, TestSelectTableNotFound,
TestAlterTableNotFound, TestZloty, TestManyJdbcObjects,
TestDatabaseEventListener, TestStringUtils, and others reaching date/time
formatting (~17 ArrayStoreException failures total).

## Repro
`StrBug`/`StrBug2` isolated probes did **not** reproduce it (the intercepted
mutators avoid the path); the trigger is specifically `StringBuilder.repeat`
via `DateTimeFormatter`. Minimal: `new StringBuilder().repeat('0', 4)` after a
content that forces a grow, or any `DateTimeFormatter.format` with zero-padding.
