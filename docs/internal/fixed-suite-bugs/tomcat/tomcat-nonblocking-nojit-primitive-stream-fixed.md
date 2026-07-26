# Tomcat `TestNonBlockingAPI` primitive stream backing (fixed)

## Failure

In no-JIT mode, `ApplicationHttpRequest.<clinit>` evaluated:

```java
specialsMap.keySet().stream().mapToInt(String::length).min().getAsInt()
```

as an empty stream, leading `testNonBlockingReadWithDispatch` to return HTTP
500 instead of 200.

## Root cause

The native `mapToInt` bridge invoked its `ToIntFunction` mapper correctly, but
`make_int_stream` wrote the resulting `Value::Int` values into an `Object[]`.
Reference-array stores coerce primitive values to null, so later primitive
stream terminals observed zeroes. `make_long_stream` and `make_double_stream`
had the same backing-array error.

## Fix

The three synthetic primitive stream constructors now allocate `int[]`,
`long[]`, and `double[]`, respectively. Their existing terminal operations use
the same generic array accessors and therefore preserve the native values.

## Verification

The focused `HashMapClinitProbe` reports the mapped twelve values, `min=31`,
and `mappedSum=414` in both JIT and no-JIT modes; direct Int/Long/Double stream
arrays are also preserved. A release executable then passed all 44 methods of
`org.apache.catalina.nonblocking.TestNonBlockingAPI` locally on 2026-07-23:

- no-JIT: 473.5 seconds
- JIT: 455.5 seconds

The same release build was validated against the Azure real-JDK/Tomcat fixture
with `-Xmx2g`:

- no-JIT: 44/44 in 124.3 seconds;
- JIT: 44/44 in 127.6 seconds with GC accounting enabled, then three clean
  uninstrumented repetitions (44/44 in 128.7, 162.5, and 133.6 seconds).

The initial JIT OOM observed while investigating this change did not recur in
these exact-fixture runs, including the high-callback async-read path, so it is
not retained as a CratonVM residual.
