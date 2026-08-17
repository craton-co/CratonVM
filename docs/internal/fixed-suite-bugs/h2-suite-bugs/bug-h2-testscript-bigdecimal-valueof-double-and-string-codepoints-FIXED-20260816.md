# `TestScript` — `BigDecimal.valueOf(double)` was not `new BigDecimal(Double.toString(v))`, and `String.codePoints()` was `chars()`

## Status
**FIXED 2026-08-16**, on branch `fix/h2-testscript-sql-divergences-20260816`
off `dev` @ `496bc3c2c`, worktree `/data/cvm-h2sql-20260816`, Azure host
`azureuser@20.80.105.49`. Verified by a same-session differential against real
HotSpot JDK 25 on the same host, same JDK image, same classpath.

Two unrelated root causes, one commit, because they were found in the same
census and each closes part of the same run. Between them they take
`org.h2.test.scripts.TestScript` from 15 errors to 10 (a third root cause, fixed
2026-08-17, has since taken it to 9). The remainder are recorded in
[`../../../known-issues/h2/testscript-sql-divergences-20260816.md`](../../../known-issues/h2/testscript-sql-divergences-20260816.md).

## The failures, as measured

```
ERROR: org/h2/test/scripts/datatypes/json.sql
line: 46
exp: >> 1.0E100
got: >> 10000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000
------------------------------                       (and the same at line: 49)

ERROR: org/h2/test/scripts/functions/aggregate/percentile.sql
line: 451
exp: >> 1.50
got: >> 1.5
------------------------------                       (and the same at line: 457)

ERROR: org/h2/test/scripts/functions/string/btrim.sql
line: 22
exp: >> "_ABC_ "
got: >> U&"\+01f600\+01f604\+01f600_ABC_ \+01f600\+01f604"
```

HotSpot JDK 25: 0 errors, exit 0.

---

## 1. `BigDecimal.valueOf(double)` — json.sql:46, :49 and percentile.sql:451, :457

`java.math.BigDecimal.valueOf(double val)` is specified as
`new BigDecimal(Double.toString(val))`. `native_bd_value_of_double` in
`native-builtins/src/math_bignum.rs` did neither half of that:

```rust
let s = format!("{}", d);
let scale = s.find('.').map(|p| (s.len() - p - 1) as i32).unwrap_or(0);
let result = bd_alloc(ctx, &s, scale);
```

Rust's `Display` for `f64` is not `Double.toString`. It never uses E-notation,
and it prints `2.0` as `2`. So:

```
                              HotSpot                       CratonVM (before)
BigDecimal.valueOf(1e100)     1.0E+100  scale -99  uns 10   1000…000 (101 digits)  scale 0
BigDecimal.valueOf(2.0)       2.0       scale 1    uns 20   2                      scale 0
BigDecimal.valueOf(1e-7)      1.0E-7    scale 8    uns 10   1E-7                   scale 7
BigDecimal.valueOf(1e7)       1.0E+7    scale -6   uns 10   10000000               scale 0
```

Everything *else* in `BigDecimal` was already right — `new BigDecimal("1.0E100")`,
`BigDecimal.valueOf(long, int)` and `BigDecimal.toString`'s scientific-notation
rules all matched HotSpot exactly, before and after. The bug was one factory.

Both H2 failures run straight through it:

* `ValueDouble.getBigDecimal()` is `BigDecimal.valueOf(value)`, and
  `Value.convertToJson` renders `REAL`/`DOUBLE`/`NUMERIC`/`DECFLOAT` by calling
  `ValueJson.get(getBigDecimal())`, which is `number.toString()`. Hence 101
  literal digits instead of `1.0E100`.
* `Percentile.interpolate` for `REAL`/`DOUBLE` is
  `interpolateDecimal(BigDecimal.valueOf(v0.getDouble()), BigDecimal.valueOf(v1.getDouble()), factor)`.
  With `valueOf(1.0)` and `valueOf(2.0)` coming back at scale 0 instead of
  scale 1, the interpolated median loses a digit of scale: `1.5` instead of
  `1.50`.

**Fix.** Format with `format_double` — the shared `Double.toString`
implementation (`cratonvm_types::java_double_to_string`) — and turn the mantissa
digits and exponent straight into the exact `(unscaled, scale)` pair, rather
than handing a string back to `bd_alloc`. The string path has no E-notation
handling, and its negative-scale branch strips significant trailing zeros; going
through `bd_alloc_bigint` avoids both.

```rust
let (unscaled, scale) = bd_parts_of_java_double_string(&crate::lang_string::format_double(d));
let result = bd_alloc_bigint(ctx, &crate::bigint::BigInt::from_decimal(&unscaled), scale);
```

`Double.toString` output is always `[-]<digit>.<digits>[E[-]<exp>]`, so the
scale is "digits after the point, less the exponent" — `1.0E100` → (`10`, `-99`),
`2.0` → (`20`, `1`), `1.0E-7` → (`10`, `8`). All ten doubles in the probe now
satisfy `valueOf(d).equals(new BigDecimal(Double.toString(d)))` on both VMs.

## 2. `String.codePoints()` — btrim.sql:22

`native-builtins/src/lib.rs` and `native-builtins/src/lang_math.rs` both
registered `codePoints()` as an alias of `chars()`:

```rust
registry.register("java/lang/String", "codePoints", "()Ljava/util/stream/IntStream;",
                  native_string_chars, // Same as chars() for BMP characters
);
```

The comment is true and the conclusion is wrong: `native_string_chars` emits
`s.encode_utf16()`, so above the BMP a supplementary character came back as its
two surrogate halves.

```
"\u{1F600}\u{1F603}\u{1F604}".codePoints().toArray()
    HotSpot   [128512, 128515, 128516]
    CratonVM  [55357, 56832, 55357, 56835, 55357, 56836]
```

Everything adjacent was correct — `codePointAt`, `codePointBefore`,
`codePointCount`, `Character.charCount` all agree with HotSpot — which is
exactly what makes the failure mode nasty: H2's
`StringUtils.trim(String s, boolean, boolean, String characters)` builds the
trim set one way and tests membership the other:

```java
HashSet<Integer> set = new HashSet<>();
characters.codePoints().forEach(set::add);      // surrogate halves on CratonVM
test = set::contains;
...
while (begin < end && test.test(cp = s.codePointAt(begin)))   // real code points
```

so for a trim set of three or more code points nothing ever matched and
`BTRIM(U&'…', U&'\+01F600\+01F603\+01F604')` returned its input unchanged. (The
one- and two-code-point cases take a different branch built from `codePointAt`,
which is why only line 22 of `btrim.sql` failed and not the lines above it.)

**Fix.** A real `native_string_code_points` that iterates the Rust `String`'s
own `chars()` — already code-point-wise — with the `IntStream` construction
factored into a shared `string_int_stream` so `chars()` is byte-for-byte
unchanged.

## Verification

Same binary, same classpath, from `apps/h2database/h2`:

```
                                  errors
HotSpot JDK 25                        0   (exit 0)
CratonVM dev @ 496bc3c2c             15
CratonVM with these two fixes        10
```

The five that went green are exactly json.sql:46, json.sql:49,
percentile.sql:451, percentile.sql:457 and btrim.sql:22. No previously-passing
script regressed.

## Repro (against a binary without the fixes)

```bash
source /data/toolchain/env.sh
cd /data/cratonvm/apps/h2database/h2
<cratonvm-bin> --java-home /data/toolchain/jdk-25 --nojit --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.scripts.TestScript
```

Neither root cause needs H2 to show itself:

```java
System.out.println(BigDecimal.valueOf(1e100));                        // want 1.0E+100
System.out.println(BigDecimal.valueOf(2.0).scale());                  // want 1
System.out.println(Arrays.toString("😀".codePoints().toArray())); // want [128512]
```
