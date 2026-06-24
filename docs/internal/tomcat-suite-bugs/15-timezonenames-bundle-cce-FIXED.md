# TC0623 — `ResourceBundle` cannot be cast to `TimeZoneNamesBundle` (FIXED)

**Status:** FIXED on branch `fix/tz-names-bundle` (merged to `dev`).
**Test:** `org.apache.catalina.filters.TestExpiresFilter` — **19/19 PASS** (was
HANG with a `ClassCastException`). Two fixes landed on this branch: (1) the
TimeZoneNamesBundle CCE below, and (2) a separate, pre-existing HTTP-connector
hang on bodiless 304 responses (see "Second fix" below).

## Symptom

```
ERROR [org.apache.catalina.core.ContainerBase.[Tomcat].[localhost].[/test].[default]]
  Servlet.service() for servlet [default] ... threw exception
  (java.lang.ClassCastException: java/util/ResourceBundle cannot be cast to
   sun/util/resources/TimeZoneNamesBundle)
```

`ExpiresFilter` formats HTTP `Expires`/`Date` headers, which (when a date with a
zone *name* field `z`/`zzz` is parsed or formatted) drives
`sun.util.locale.provider.TimeZoneNameUtility` → `LocaleData.getTimeZoneNames`.

## Root cause

CratonVM intercepts every `ResourceBundle.getBundle(...)` **and**
`sun.util.resources.LocaleData.getBundle(String, Locale)` with a Rust native
(`native-builtins/src/locale_resources.rs::rb_get_bundle`) that returns a *raw*
synthetic `java/util/ResourceBundle`. But `javap -c LocaleData` shows three
accessors immediately `checkcast` the result to a **concrete** bundle type:

```
getTimeZoneNames → checkcast sun/util/resources/TimeZoneNamesBundle
getCurrencyNames → checkcast sun/util/resources/OpenListResourceBundle
getLocaleNames   → checkcast sun/util/resources/OpenListResourceBundle
```

A bare `java/util/ResourceBundle` is not assignable to `TimeZoneNamesBundle`, so
the cast throws. The bundle was never instantiated as the concrete type the JDK
expects. (`is_synthesized_locale_base` skipped the real class-based bundle path
for **all** `sun.util.resources.*`, so `TimeZoneNames` never got a real class.)

The jimage ships concrete leaf data classes that subclass `TimeZoneNamesBundle`
(`sun.util.resources.cldr.TimeZoneNames`/`TimeZoneNames_en`, plus the non-CLDR
`sun.util.resources.TimeZoneNames` root). The fix routes the `*.TimeZoneNames`
base name through the existing `try_class_bundle` path (same machinery as the
HIB-CV-27 javac `ListResourceBundle` fix), which instantiates the concrete leaf
class. The root data class always exists, so the cast always succeeds.

## Two follow-on data-fidelity defects (same file)

Making the bundle *cast-compatible* was not enough — it then had to be *read*
correctly, since the JDK reads it through `ResourceBundle.getString` /
`containsKey` overrides that CratonVM also natively shortcuts:

1. **`containsKey` read the wrong slot.** `rb_contains_key` only special-cased
   `PropertyResourceBundle`, then fell through to a field-0 map read. On a real
   `OpenListResourceBundle` subclass, field 0 is the inherited `parent` slot, so
   `tzb.containsKey("GMT")` answered `false`. `LocaleResources.getTimeZoneNames`
   gates its lookup on that, so a wrong `false` sent it down the metazone
   fallback → **"UTC"** instead of "GMT". Fix: resolve real subclasses via
   `getContents()` + the parent chain (mirroring `rb_get_object`).

2. **`getStringArray` dropped the zone id.** `TimeZoneNamesBundle.handleGetObject`
   does NOT return the raw `getContents()` value: for a `String[]` it returns a
   NEW `String[]` of length+1 with the lookup key (zone id) prepended at slot 0
   (`javap -c TimeZoneNamesBundle`). `TimeZoneNameUtility.getZoneStrings` depends
   on the 7-column row `[id, longStd, shortStd, longDst, shortDst, longGen,
   shortGen]`. CratonVM's direct `getContents()` read bypassed the override and
   produced a 6-column row. Fix: `maybe_prepend_tz_zone_id` re-applies the
   id-prepend for `TimeZoneNamesBundle` subclasses when the value is a reference
   array (detected via `heap_kind_of`, since `class_name_of_id` reports the
   *element* class for array objects, not the `[L…;` descriptor).

After both, `getStringArray("GMT")` and the `DateFormatSymbols.getZoneStrings()`
GMT row are byte-identical to HotSpot:
`[GMT, Greenwich Mean Time, GMT, Greenwich Mean Time, GMT, Greenwich Mean Time, GMT]`.

## Scope decision

Only `*.TimeZoneNames` is routed to the real class. `CurrencyNames`/`LocaleNames`
share the same latent `checkcast`, but their real display names live in the
`sun.util.resources.cldr.ext.*` package that the simple locale-candidate chain
does not reach; routing them would hand back a partial bundle without improving
their (already code-only, identical pre/post) output, and nothing in the failing
test exercises their cast. Left on the curated `build_bundle` path — verified
byte-identical to baseline (`Currency.getInstance("USD").getDisplayName` →
`"USD"` both before and after; not regressed by this change).

## Verification (debug `cratonvm.exe`, `--nojit`)

| probe | HotSpot | CratonVM (fix) |
|---|---|---|
| `getStringArray("GMT")` | 7 cols, id-prefixed | **==** |
| `DateFormatSymbols.getZoneStrings()` GMT row | `[GMT, …]` | **==** |
| `SimpleDateFormat("…zzz").parse("… GMT")` | parses | parses (no CCE) |
| `FastHttpDateFormat.parseDate(...)` round-trip | ok | ok |

`TestExpiresFilter`: **17/19 PASS** (was HANG + CCE). `DateFormatSymbols(Locale.US)
.getMonths()[0] == "January"` (curated `FormatData` path intact). No regression.

Probes: `scratch/tznames/{TzProbe,TzProbe2,TzProbe3,RegProbe}.java`.

## Cosmetic note — headers say "UTC" not "GMT"

`TimeZone.getDisplayName(...)` is short-circuited by a hardcoded `"UTC"` stub
(`native-builtins/src/lib.rs`, a workaround predating this CCE fix), so the
emitted `Date`/`Expires` headers read `… UTC` rather than `… GMT`. This is
cosmetic only: `FastHttpDateFormat.parseDate("… UTC")` round-trips to a valid
timestamp (verified == the GMT literal), so `validate()`'s
`expiresDate > now` assertion still passes. The stub could be removed now that
the bundle path works, but that has VM-wide blast radius and is out of scope for
this CCE fix.

## Second fix — bodiless 304/204/1xx hung the native HTTP client

With the CCE gone, `testExcludedResponseStatusCode` and `testBug63909` still
failed with `expected:<304> but was:<-1>` (`HttpURLConnection.getResponseCode()`
returns -1 on an unparseable/incomplete response, after a ~200 s hang). Both are
the only tests where the **server returns a 304 (empty-body) response**;
`testExcludedResponseStatusCode` calls `response.setStatus(304)` directly and
contains zero time-zone code. **Reproduced on the baseline dev binary** → a
distinct, pre-existing bug.

Root cause: CratonVM models `java.net.HttpURLConnection` natively
(`native-builtins/src/http_url_connection.rs`); its `read_response` special-cased
only **HEAD** as bodiless. A 304/204/1xx response legitimately omits
`Content-Length` (RFC 9110 §6.4.1: those carry no message body), so the
no-content-length `else` branch read "until EOF" — which never comes on a
keep-alive connection the server holds open → `getResponseCode()` hangs forever.

Fix: extend the bodiless guard to `status == 204 || status == 304 || 1xx` (plus
HEAD). Isolated repro `scratch/tznames/C304v.java` (raw `ServerSocket` returning
a keep-alive 304): pre-fix hangs >60 s; post-fix `rc=304` in <50 ms across the
keep-alive / `Content-Length: 0` / `Connection: close` variants, matching
HotSpot. Rust unit tests `test_read_response_{304,204}_is_bodiless` +
`test_read_response_200_reads_body` guard the rule. Any conditional-GET / 304
test across the Tomcat suite benefits.
