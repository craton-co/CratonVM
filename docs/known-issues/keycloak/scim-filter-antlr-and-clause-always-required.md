# SCIM filter parser (ANTLR-generated): grammar always requires a trailing `AND` clause, rejecting every otherwise-valid filter expression

Status: open — genuine, systematic CratonVM ANTLR-runtime bug (23/23 tests fail in the affected class)

Date observed: 2026-07-11 (refresh rerun against non-passed-before classes, branch fix/keycloak-nonpassed-rerun-v2-20260710)

## Summary

`scim/core :: org.keycloak.scim.filter.FilterUtilsTest` fails **all 23 of 23** test methods. Every failure is the
test calling `assertDoesNotThrow(() -> FilterUtils.parseFilter(...))` on a filter string that should parse
successfully, but the parser throws instead — always with the same shape of error, an ANTLR
"mismatched/extraneous input ... expecting AND" pointing at (or just past) the end of the input:

```
=> org.keycloak.scim.filter.ScimFilterException: Invalid filter syntax: position 18: mismatched input '<EOF>' expecting AND
   (testCaseInsensitiveOperators — filter has no AND at all, just a single comparison)

=> ...position 69: mismatched input ']' expecting AND, position 70: mismatched input '<EOF>' expecting AND
   (testValuePathWithSchemaPrefix)

=> ...position 48: extraneous input ']' expecting AND, position 49: mismatched input '<EOF>' expecting AND
   (testValuePathWithLogicalOperators)

=> ...position 16: mismatched input '<EOF>' expecting AND
   (testStringComparison — a plain single string-equality filter, no logical operators at all)
```

## Root cause hypothesis

Every single failure — regardless of what the actual filter content is (single comparisons, value-path
expressions, filters that already contain `AND`/`OR`, filters that don't) — ends with the parser reaching (or
just past) the end of input while still expecting an `AND` token. This is the signature of an ANTLR grammar rule
like `filter: expr (AND expr)*` (a "zero-or-more AND-continuation" loop) where the **zero-repetitions case isn't
being recognized as valid** — i.e. the generated parser's decision logic for "should I keep looping on this
optional/repeated alternative, or exit the rule" incorrectly commits to "there must be at least one more AND"
even when the input is already a complete, grammatically-valid expression on its own.

This matches the general shape of ANTLR-runtime-under-CratonVM issues already tracked elsewhere in this project's
history (multiple prior ANTLR grammar/closure-predicate fixes) — worth checking whether this is the same
underlying ANTLR ATN-execution gap resurfacing in a new grammar (SCIM's filter grammar, presumably a different
`.g4` file than previously-fixed ones), or a distinct instance.

## Impact

100% of `FilterUtilsTest` (23/23 tests) fails — SCIM filter parsing appears to be **completely non-functional**
under this CratonVM build, since even the simplest possible filters (a single string comparison, no logical
operators) fail to parse.

## Next steps

1. Locate the SCIM filter's ANTLR grammar source (`.g4` file, likely under `scim/core/src/main/antlr4/` or
   similar) and find the rule matching `expr (AND expr)*` (or equivalent) to confirm the "optional AND
   continuation" grammar shape hypothesis.
2. Compare against the already-documented/fixed ANTLR closure-predicate issue in this project's history — check
   whether the same fix (or a similar one) applies here, or whether this is a distinct manifestation of ANTLR
   handling under CratonVM.
3. Write a minimal standalone ANTLR repro (a tiny grammar with a `(TOKEN expr)*` loop) to isolate whether *any*
   ANTLR-generated parser with this shape mis-parses zero-repetition input under CratonVM, independent of the
   SCIM-specific grammar.
4. Verify the fix against all 23 tests in `FilterUtilsTest`.

## Repro

```
cd C:\craton\CratonVM-keycloak-nonpassed-v2-20260710
$jdk = '"C:\Program Files\Java\jdk-25"'
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-scim-filter-and -ClassList <(printf 'module\tclass\nscim/core\torg.keycloak.scim.filter.FilterUtilsTest\n') -KeycloakRoot apps\keycloak -Exe target\release\cratonvm-nonpassed-v2-refresh-20260711.exe -JdkHome $jdk
```

## Evidence

`C:\craton\CratonVM-keycloak-nonpassed-v2-20260710\apps\keycloak-suite-runner\.suite\results\nonpassed-before-refresh-shard1\all-jit\logs\scim_core.org.keycloak.scim.filter.FilterUtilsTest.out.log`
(23/23 failed), 2026-07-11 refresh rerun with a binary built from current `dev`.
