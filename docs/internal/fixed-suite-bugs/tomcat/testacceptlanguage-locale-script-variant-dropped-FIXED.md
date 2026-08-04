# `TestAcceptLanguage.bug56848` — `Locale.forLanguageTag` dropped the `#Hant` script variant

| | |
|---|---|
| **Status** | ✅ **FIXED** (2026-08-03) |
| **Severity** | low as filed — one narrow test method; the underlying defect was wider (see below) |
| **HotSpot** | PASS (35/35, fresh-verified 2026-08-03) |
| **CratonVM** | PASS (35/35) on `cratonvm-localescript-20260803-003.exe` |
| **Discovered** | 2026-08-03, rerunning the 07-31 4-shard FAIL/HANG set after merging `dev` (`c1fe51a24`) |
| **Fixed in** | branch `fix/tomcat-locale-script-variant-20260803` |

## Symptom

```
1) bug56848(org.apache.tomcat.util.http.parser.TestAcceptLanguage)
java.lang.AssertionError: expected:<zh_CN_#Hant> but was:<zh_CN>
```

`TestAcceptLanguage.bug56848` parses `zh-hant-CN;q=0.5,zh-hans-TW;q=0.05` and
compares each result against a `Locale.Builder`-built locale with the same
script. The *expected* side (`Locale.Builder`) was already correct on CratonVM;
only the `Locale.forLanguageTag` side lost the script.

## Root cause

`java/util/Locale.forLanguageTag` was natively overridden by a hand-rolled
BCP-47 split in `native-builtins/src/locale_bootstrap.rs`. It had three
independent defects, and the side table it wrote into had no slot for a script
at all:

1. **Script dropped.** The parser recognised a 4-letter subtag only well enough
   to `continue` past it, so it would not be mistaken for the region. Nothing
   kept it — and `locale_data`, the ObjectRef-keyed side table every accessor
   reads, stored only `(language, country, variant)`. `getScript()` therefore
   answered `""` for every locale CratonVM built, and `toString()`'s `_#Hant`
   suffix never appeared.
2. **Extension singletons filed as variants.** A one-character subtag opens a
   BCP-47 extension (`-u-…`) or private-use sequence (`-x-…`). The parser had no
   notion of them, so `en-US-u-ca-japanese` produced a locale whose *variant*
   was the string `"u"`, and `zh-Hant-TW-x-java` one whose variant was `"x"`.
3. **`und` treated as a language.** `forLanguageTag("und-DE")` returned language
   `"und"` (`toString()` → `und_DE`) where HotSpot returns an empty language
   (`_DE`).

Two further natives were built on the assumption that a CratonVM `Locale` could
never carry extensions, and became wrong once it could:

4. `Locale.toString()` omitted the extension suffix entirely — HotSpot renders
   `en_US_#u-ca-japanese` and `zh_TW_#Hant_x-java`.
5. `Locale.stripExtensions()` was an **identity stub**, so a locale with
   extensions stripped to itself.

## The fix

`forLanguageTag` now runs the **real JDK's own** `forLanguageTag` body from the
native — `LanguageTag.parse` → `InternalLocaleBuilder.setLanguageTag` →
`getBaseLocale`/`getLocaleExtensions` → `Locale.getInstance`, plus the
`getCompatibilityExtensions` step for legacy variant spellings. That machinery
is plain bytecode CratonVM already executes correctly: `Locale.Builder
.setLanguageTag(...)`, which shares the exact same chain, was byte-for-byte
identical to HotSpot on all 12 probe tags *before* any change here. There was
never a reason to re-implement BCP-47 in Rust.

The hand-rolled split survives as `split_language_tag`, used only when the
delegation is unreachable — i.e. the synthetic-JDK build, which has no
`sun.util.locale` package. It was fixed for (1), (2) and (3) above so that mode
is also correct as far as its data model reaches (it cannot represent
extensions, and says so via a `tracing::debug!` when it takes over). Eight unit
tests pin its behaviour.

Supporting changes:

- `locale_data` (`native-builtins/src/lib.rs`) now stores
  `(language, script, country, variant)`. Putting the script in the **existing**
  table rather than a new one matters: that table is already a GC root and is
  already rebuilt with relocated keys by `gc_update_locale_refs`, so the script
  inherits both for free instead of needing its own root/remap wiring.
- `locale_populate_full` sets `BaseLocale.script`, and `getScript()` consults the
  side table before the `baseLocale` field (the synthetic build has no
  `BaseLocale` class, so the field route alone loses the script there).
- `toLanguageTag`'s side-table branch emits the script and splits multi-subtag
  variants, matching the real-`Locale` branch beside it.
- `toString()` gained the extension suffix, transcribed condition-for-condition
  from `java.util.Locale.toString` (including that a script and an extension
  share one `#`).
- `stripExtensions()` actually strips.
- `locale_populate` now roots its `Locale` across the five allocations it makes
  while building the `BaseLocale`, and returns the forwarded reference — the
  Family-1 stale-native-local shape, latent there before.

## Verification

`probes/LocaleScriptProbe.java` (added with this fix) makes 26 assertions over
`toString` / `toLanguageTag` / `getLanguage` / `getScript` / `getCountry` /
`getVariant` / `hashCode` / `equals` / `stripExtensions` / `getExtension`,
across 12 language tags plus the `Locale.Builder` round-trips `bug56848` itself
compares against. Every expectation is HotSpot's own answer, so the probe is
self-checking — it prints `bad=0` and exits 0 on a correct VM, and on HotSpot by
construction (verified: `bad=0` there).

| | baseline (dev `36f1157ad`) | fixed |
|---|---|---|
| `TestAcceptLanguage` | 35 run, **1 failure** | **OK (35 tests)** |
| `LocaleScriptProbe` | **bad=16**, rc=1 | **bad=0**, rc=0 |
| `LocaleScriptProbe` on HotSpot | bad=0 | bad=0 |

The 16 baseline failures span all five defects above, not just the one the test
method caught.

No-regression set — every Tomcat test class that mentions `Locale`, run on the
same host before and after, all `OK` on both:

`TestStringManager` (6), `TestWebXml` (29), `TestB2CConverter` (7),
`TestCharsetCache` (1), `TestConcurrentDateFormat` (2),
`TestAccessLogValveDateFormatCache` (1), `juli.TestDateFormatCache` (8),
`TestHttpServlet` (13), `TestAddCharSetFilter` (8), `TestCorsFilter` (83),
`TestLockoutRealm` (4), `TestResponse` (81), `TestRequest` (40).

Rust tests: `cargo test -p cratonvm-native-builtins --lib` — 3246 passed, 0
failed (8 of them new, pinning `split_language_tag`).
`cargo test -p cratonvm-vm --lib --features synthetic-jdk locale` — 6 passed,
including a new `locale_for_language_tag_keeps_script` that exercises the
fallback parser and the side table end-to-end in the mode where the real BCP-47
machinery does not exist.

## Notes for the next reader

- **The first attempt at this fix silently did nothing.** It called
  `LanguageTag.parse(String, ParseStatus)` — the pre-JDK-21 signature. In JDK 25
  the method is `parse(String, java.text.ParsePosition, boolean)` and
  `sun.util.locale.ParseStatus` does not exist (`Class.forName` on it throws
  `ClassNotFoundException` on **HotSpot too**). The failed resolution is
  indistinguishable from "synthetic JDK", so the delegation fell straight
  through to the fallback parser and the build looked *almost* right — script
  subtags fixed, extensions still missing. `javap -s` the real JDK before
  writing a descriptor.
- The related-but-distinct 2026-08-01 defect — `toString()` returning the empty
  string for *every* locale, because the native read only the side table that
  the JDK-built constants never fill — is a different bug in the same native.
  That one was the whole rendering; this one was one subtag inside it.
- Two stale comments elsewhere still claim "`forLanguageTag` does NOT populate
  the base-locale fields without CLDR data"
  (`locale_bootstrap.rs`'s `AVAILABLE_LOCALES`, and the Quarkus
  `LocaleConverter` override in `lib.rs`). That rationale is now obsolete —
  `forLanguageTag` populates everything — but the workarounds they justify are
  harmless and untouched here, so their comments were left alone rather than
  changed without a run of the suites that motivated them.
