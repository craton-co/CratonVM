# `TestConcurrentDateFormat.testFormatReturnsGMTAfterParseCET` — NPE in `SimpleDateFormat.matchZoneString` — FIXED 2026-09-12

Retired from the `known-issues/tomcat` page of the same name (opened 2026-09-12).

## Status
**FIXED.** A CratonVM defect, not a host-locale artifact: HotSpot 25.0.3 on
the same box, classpath and Russian OS locale passes the class (`OK (2
tests)`); CratonVM failed it with the host locale forced to `en_US` too.
Three defects stacked in the locale-data path, plus a fourth, older one in
`TimeZone.getDisplayName` that the same investigation measured and closed.

## Measured
| binary | `TestConcurrentDateFormat` |
|---|---|
| HotSpot 25.0.3 (control, same fixture) | `OK (2 tests)` |
| CratonVM `target-tomcat-20260912` (before) | `Tests run: 2, Failures: 1` — the NPE |
| CratonVM `cratonvm-tlsdf-v1` (fixes 1–3) | `OK (2 tests)` |
| CratonVM `cratonvm-tlsdf-v2` (+ fix 4) | `OK (2 tests)` |

The date/zone-name neighbours on `cratonvm-tlsdf-v2`, all `OK`:
`TestExpiresFilter` (19), `TestCookieProcessorGeneration` (30),
`TestFastHttpDateFormat` (1), `TestAccessLogValve` (94), `TestHttpParser` (25).
Mock test `the_time_zone_name_provider_is_delegated_to_the_real_getter` and the
existing `locale_service_provider_delegation_tests` / `locale_resources` tests
pass.

## Mechanism
`SimpleDateFormat.subParseZoneString` walks `DateFormatSymbols.getZoneStrings()`.
For each row, `matchZoneString` fills an EMPTY name by asking
`TimeZoneNameUtility.retrieveDisplayName`, and calls `.length()` on the answer.
On CratonVM the `GMT` row was `[GMT, Greenwich Mean Time, GMT, "", GMT, "", GMT]`
and `retrieveDisplayName` answered `null`.

### 1. `JRELocaleProviderAdapter.getLocaleServiceProvider` refused `TimeZoneNameProvider`
`locale_bootstrap.rs` shadows this method and answers `null` for every SPI
without an explicit arm (only `DecimalFormatSymbolsProvider` and
`CalendarDataProvider` had arms). So `LocaleProviderAdapter.getAdapter(
TimeZoneNameProvider, loc)` fell through to FALLBACK, whose plain
`TimeZoneNameProviderImpl` serves CLDR's raw rows. CLDR leaves every name it
INHERITS empty; only `CLDRTimeZoneNameProviderImpl.deriveFallbackNames` fills
them. And `retrieveDisplayName` reaches providers through
`LocaleServiceProviderPool` — i.e. through the same refused method — so it had
no provider at all and answered `null`.

```text
                          HotSpot                          CratonVM (before)
getAdapter(TZNP, en_US)   CLDR / CLDRTimeZoneNameProviderImpl   FALLBACK / TimeZoneNameProviderImpl
retrieveDisplayNames(GMT) [GMT, Greenwich Mean Time, GMT, …]    [GMT, null, null, null, null, null, null]
zoneStrings GMT row       fully derived                         "" at DST-long and generic-long
```

The getter itself worked on this VM (`cldr.getTimeZoneNameProvider()
.getDisplayName("GMT", true, LONG, US)` = `Greenwich Mean Time`); only the SPI
lookup refused it. **Fix:** a third arm delegating to
`getTimeZoneNameProvider()`, under the same degrade-to-null contract as the
other two, plus a mock test (`the_time_zone_name_provider_is_delegated_to_the_real_getter`).

### 2. JDK locale data fell back to the DEFAULT locale
`rb_get_bundle` (which answers `LocaleData.getBundle` / `Bundles.of`) applied
`ResourceBundle.Control`'s default-locale fallback to `sun.util.resources.*`.
`Bundles` has no fallback step. The fallback appended the default locale's
candidates to the chain and `try_class_bundle` returns the LAST existing
candidate, so the default locale won:

```text
host default ru_RU:  DateFormatSymbols.getInstance(Locale.US).getZoneStrings()  -> Russian names
host default en_US:  TimeZoneNameUtility.getZoneStrings(ru_RU)                  -> English names
```

On this Russian-locale host that alone would have kept the test's `Locale.US`
formatter on Russian zone names. **Fix:** no default-locale fallback for
`is_synthesized_locale_base` names.

### 3. The zone-id prepend built an `Object[]`
`maybe_prepend_tz_zone_id` re-applies `TimeZoneNamesBundle.handleGetObject`'s
`new String[len + 1]`, but allocated an untyped reference array, which reads
back as `Object[]` (`lr.getTimeZoneNames("GMT")` printed `[Ljava.lang.Object;`).
`TimeZoneNameProviderImpl.getDisplayNameArray` `checkcast`s it to `String[]`.
**Fix:** a `String[]` via `new_ref_array`, with `arr`/`key` pinned across the
allocation (they were not).

After 1–3, on both `-Duser.language=ru` and `en`, `getZoneStrings` rows,
`retrieveDisplayName(GMT, dst, LONG, US)`, `retrieveDisplayNames` and the
`ru_RU` rows match HotSpot exactly, and the probe's `parse("... CET")` succeeds.

### 4. Residual: `TimeZone.getDisplayName` answered "UTC" for names CLDR inherits
Found while checking 1–3, present before them (same output on the old binary).
The `TimeZone.getDisplayName` natives in `lib.rs` read the raw CLDR row and fell
back to a hard-coded `"UTC"`. 14 zones × 5 locales × 4 styles against HotSpot:
143 of 280 answers differed — `America/Buenos_Aires` SHORT `UTC` for
`GMT-03:00` (so `new Date(0).toString()` printed `UTC` on this `-03:00` host),
`Etc/GMT+5` and `Africa/Casablanca` `UTC` in every style, every `Locale.ROOT`
name `Coordinated Universal Time`, and DST-long names of non-DST zones in
English for every locale.

**Fix:** the natives now run `TimeZone.getDisplayName`'s own algorithm —
`TimeZoneNameUtility.retrieveDisplayName` (reachable since fix 1), then the
`GMT±` id verbatim, then `ZoneInfoFile.toCustomID(getRawOffset() [+
getDSTSavings()])` — and keep the old path only when the utility cannot run
(synthetic-JDK images). The same sweep, rows differing from HotSpot:

```text
before (target-tomcat-20260912)   61 of 71
fixes 1-3 (cratonvm-tlsdf-v1)     44 of 71
+ fix 4   (cratonvm-tlsdf-v2)      0 of 71     new Date(0) -> "Wed Dec 31 21:00:00 GMT-03:00 1969", as HotSpot
```

The one remaining line was HotSpot's stderr `WARNING: Use of the three-letter
time zone ID "PST" is deprecated ...`, which JDK 25's
`TimeZone.getTimeZone(String, boolean)` prints for every `ZoneId.SHORT_IDS`
key and the replacing native never did. It prints it now: on
`cratonvm-tlsdf-v3` the sweep's output, stderr included, is byte-identical to
HotSpot's (0 differing lines).

### Probes, before -> after (`diff` lines against HotSpot 25.0.3)
| probe | default mode | `--jdk-only` |
|---|---|---|
| `LocaleDateTzShadowSweep` | 2 -> **0** | 2 -> **0** |
| `L1LocaleProviderWorkload` | 8 -> 6 | 0 -> 0 |
| `L6TlsParamSweep` | 36 -> 22 | 36 -> 22 |

No row in any of the three is new against both HotSpot and the before binary
except `L6TlsParamSweep` row 77's `supportedProtocols`, which reads
`SUPPORTED_PROTOCOL_NAMES` — untouched here; the before binary predates
`TLSv1.1` joining that list, and both differ from HotSpot on it.

## Files
- `native-builtins/src/locale_bootstrap.rs` — `TimeZoneNameProvider` arm + test
- `native-builtins/src/locale_resources.rs` — no default-locale fallback for JDK locale data; `String[]` prepend
- `native-builtins/src/lib.rs` — `TimeZone.getDisplayName` via `TimeZoneNameUtility`

## Repro
```powershell
apps\tomcat-suite-runner\run-one.ps1 -Exe <cratonvm.exe> -Class org.apache.tomcat.util.http.TestConcurrentDateFormat
```
