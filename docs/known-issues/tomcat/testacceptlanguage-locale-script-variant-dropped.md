# `TestAcceptLanguage.bug56848` — `Locale.forLanguageTag` drops the `#Hant` script variant

| | |
|---|---|
| **Status** | OPEN |
| **Severity** | low — single, narrow test method (34 of 35 `TestAcceptLanguage` methods pass) |
| **HotSpot** | PASS (35/35, fresh-verified 2026-08-03) |
| **CratonVM** | FAIL (`bug56848` only) |
| **Discovered** | 2026-08-03, rerunning the 07-31 4-shard FAIL/HANG set after merging `dev` (`c1fe51a24`) |

## Symptom

```
1) bug56848(org.apache.tomcat.util.http.parser.TestAcceptLanguage)
java.lang.AssertionError: expected:<zh_CN_#Hant> but was:<zh_CN>
```

The test parses an `Accept-Language` header containing a Chinese locale with
an explicit script subtag (traditional Han, `Hant`) and expects the resulting
`Locale`'s `toString()` to preserve the script variant as `zh_CN_#Hant`.
CratonVM's locale machinery produces `zh_CN` — the script subtag is silently
dropped somewhere between header parsing and `Locale` construction/rendering.

This is adjacent to, but distinct from, the previously-fixed
[[reference_locale_tostring_returned_empty_for_every_real_locale]] defect (that
one was `toString()` returning empty for *every* locale; this one is a
specific script-variant field being dropped for locales that have one).

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
<cratonvm.exe> --java-home "<real JDK 25>" -cp (Get-Content .suite\cp.txt) `
  org.junit.runner.JUnitCore org.apache.tomcat.util.http.parser.TestAcceptLanguage
```

No server/sockets needed — this is a pure parsing/data-model test, so it's
one of the fastest and easiest of this batch to isolate further (0.04s
runtime).

## Suspected root cause (not yet isolated)

Either `java.util.Locale.Builder`/`forLanguageTag` doesn't retain the script
subtag when constructing a `Locale`, or `Locale.toString()`'s variant/script
formatting doesn't include it (the `#Hant` script suffix in `toString()`'s
output format is BCP-47-adjacent but not identical to the language-tag
form — worth checking whether CratonVM's `Locale` even stores a distinct
script field or only language/country/variant). Not yet checked against
source. No prior known-issue doc covers this exact signature (checked
`docs/internal/fixed-suite-bugs` and `docs/known-issues` — no hits).
