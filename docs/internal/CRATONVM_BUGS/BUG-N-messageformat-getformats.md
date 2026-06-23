# BUG-N — `BigDecimal.toString()` expanded scientific notation to plain ("100" vs "1E+2")

**Test:** `org.apache.el.util.TestMessageFactory` (`testFormatNone`).
HotSpot: PASS. **Status: FIXED.**

> The original hypothesis (`MessageFormat.getFormatsByArgumentIndex()` wrong)
> was **incorrect** — that method returns `[null]` for `{0}`, matching HotSpot.
> The real cause is `BigDecimal.toString()`.

## Symptom

`messageFactory.getInternal("messageFactory.formatNone", new BigDecimal("1E+2"),
ZERO)` must return `"1E+2"`; CratonVM returned `"100"`.

`MessageFactory.getInternal` converts each `Number` arg to a `String` (via
`toString()`) when its `{n}` slot has no explicit `NumberFormat`. For `"{0}"`
that conversion fires, so the result is `new BigDecimal("1E+2").toString()`.
CratonVM rendered that as `"100"` instead of `"1E+2"`.

## Root cause

`new BigDecimal("1E+2")` has unscaledValue=1, scale=−2 (correct in CratonVM).
But the native `BigDecimal.toString()` (`native_bd_to_string`, also wrongly
registered for `toPlainString`) always expanded to plain decimal — a negative
scale became trailing zeros (`"1" + "00"` → `"100"`). Per
`java.math.BigDecimal.toString`, **scientific notation** must be used when the
scale is negative OR the adjusted exponent (`digits − 1 − scale`) is `< −6`.

## Fix

`native-builtins/src/lib.rs`: add `bd_layout_chars` implementing the canonical
`toString` layout (plain only when `scale ≥ 0 && adjusted ≥ −6`, else
scientific with a signed exponent). Route `toString()` through it via
`bd_read_canonical`, and give `toPlainString()` its own always-plain native
(`native_bd_to_plain_string` → `bd_read`/`apply_scale`). The internal
round-trippable arithmetic form (`apply_scale`) is unchanged, so BigDecimal
arithmetic and positive-scale rendering (e.g. H2 `DECIMAL(p,2)` → `"0.00"`) are
unaffected. Verified: `BigDecimal("1E+2").toString()` → `"1E+2"`,
`TestMessageFactory` 4/4.
