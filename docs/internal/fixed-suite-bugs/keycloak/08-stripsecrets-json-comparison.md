# 08 — StripSecretsUtilsTest: JSON `ComparisonFailure` (empty message)

**Status:** open (needs diff capture)
**Affected:** StripSecretsUtilsTest (`stripRealm`, `stripComponent` — 2/9)
**Symptom:** `org.junit.ComparisonFailure` (expected/actual strings not surfaced in log)
at `StripSecretsUtilsTest.java:275` (stripRealm) / `:170` (stripComponent).

## Analysis
`StripSecretsUtils` walks a realm/component JSON representation masking secret values,
then the test asserts string-equality of the masked JSON. The mismatch is most likely a
JSON field-ordering or value-formatting difference in CratonVM's Jackson serialization
(e.g. map iteration order, masked-value representation, or number formatting), not a
crash. Could share root with the Jackson generics work (report 02) — re-test after that
fix lands; if it persists, capture the actual vs expected JSON to localize.

## Next step
Add a small harness that prints `expected`/`actual` from the two assertions (the test
swallows them into `ComparisonFailure` with an empty message in the captured log).
