# W7-80 — stage 2: the locale data was in the JDK image all along

> ## 2026-08-12 (P3-E) — this record's data is now reachable from a THIRD consumer, and §7's "No test was weakened" was re-checked rather than inherited
>
> **Verified against today's source, not assumed.** `load_cldr_table`
> (`native-builtins/src/locale_resources.rs:585`) and the six surfaces §2 lists
> are all present; `getNumberPatterns` reads
> `cldr_number_strings(&t, "NumberPatterns")` per locale, so §4's table is a
> description of the tree as it stands. Nothing in this record was found stale.
>
> **What changed above it.** `String.format(String, Object...)` — the overload
> with no `Locale` — used to localize against `Locale.ROOT`, so it never reached
> this data at all. It now resolves through the no-arg
> `DecimalFormatSymbols.getInstance()`, i.e. through
> `Locale.getDefault(Locale.Category.FORMAT)` → `getInstance(Locale)` → the
> native this record rewrote. W7-34 §"What is left" bullet 4 is the row; the fix
> is in `native-builtins/src/lang_string.rs`.
>
> That makes §3's `de_DE` "chimera" observation load-bearing in a new place: on a
> non-en host, **plain `String.format("%.2f", x)` anywhere in the VM now reads
> this table.** Every claim in §6 about what does and does not move on an en_US
> host still holds (CLDR en `NumberElements` is byte-identical to the old
> hardcoded values), but the *population* of callers that can see a wrong answer
> just grew from "explicit-`Locale` callers" to "every formatting call in the
> process". If a locale row in §2's coverage table is wrong, it is now much more
> visible. Re-running `probes/DefaultLocaleProbe.java` per §8 is worth more than
> it was.
>
> **§7 "No test was weakened", re-checked under the NEW rule.** That bullet's
> argument was that the formatting vectors pin `Locale.ROOT`/`Locale.US`, so the
> host locale cannot reach them. Under the old rule the no-`Locale` calls were
> *also* safe, because they were ROOT by construction; under the new rule they
> are not, so the grep had to be redone. Result, over all of
> `regression-suite/src/*.java`:
>
> | site | verdict |
> |---|---|
> | `RJdkViews`, `RStrings`, `RJdkHello` numeric formatting | every localizing conversion pins `Locale.ROOT` or `Locale.US` — unaffected |
> | `RStrings:131` `new Formatter(fsink, Locale.GERMANY).format("%,.2f", …)` | receiver locale, not the no-`Locale` rule — unaffected |
> | `RStrings:195` `String.format("%tb", …)` vs `DateFormatSymbols.getInstance()` | already asserts the FORMAT-default rule, on the date helper that was already correct — unaffected, and now the numeric helper agrees with it |
> | `RJdkLogging:715` `String.format("%d:%02d:%02d", …)` | no locale, but its own comment says it is built through `String.format` precisely so it moves with the formatter. Both sides of that `contains` go through the same native, before and after. Digits only change at all for a FORMAT default whose zero digit is not ASCII `'0'` (ar-EG's U+0660); de/ru/fr are ASCII |
> | **`RJdkHello:99` `ps.printf(Locale.ROOT, " [%s\|%d\|%05.2f]", …)`** | **BREAKS**, and not because of this record: `native_printf_locale` in `native-builtins/src/lib.rs` DROPS its `Locale` and delegates to the no-`Locale` entry, so on a non-ROOT-default host this now renders the host's separators against a pinned `" [x\|7\|01.50]"`. The two-line fix is in the P3-E lane report and must land in the same change |
>
> One site at risk, one fix, both named. On an en-US CI box nothing moves at all
> — which is the same hiding place the defect being fixed had used.

**Status: SOURCE LANDED, NOT RE-MEASURED ON A CRATONVM BUILD FROM THIS BRANCH.**
This lane does not run `cargo build`. Every HotSpot column below is a live run on
jdk-25.0.3.9-hotspot on this host (Windows 11, **ru_RU**). Every "CratonVM today" column
is a live run of `C:/craton/CratonVM/target/release/cratonvm.exe` (built 2026-08-12 06:04,
current-dev-ish, i.e. *with* W7-67's reporting fix and *without* this change). The
reachability finding in §1 is a live CratonVM measurement, not source reasoning.

W7-67 §4 staged the work: (1) report the host locale — landed; (2) patterns + symbols +
currency table **together**, because W7-44 refused to move the patterns alone; (3)
`DateFormatSymbols` per locale, "the large one and probably wants real CLDR data rather
than curation".

This record does (2) and (3) in one change, because the thing that made (3) look large
turned out not to be true.

---

## 1. The premise every comment in `locale_resources.rs` rested on was false

Eleven comments in that file, and more in `native_override.rs`, `vm_exec.rs`,
`reflect_annotations.rs` and `phases_late/text_intl.rs`, say some version of:

> the JDK's per-locale CLDR data lives in `jdk.localedata`, which CratonVM doesn't surface

Measured, `--jdk-only`, before a line of this change was written
(`probes/` — a throwaway `Class.forName` + `getContents()` walk):

```text
LOAD ok sun.text.resources.cldr.FormatData        super=java.util.ListResourceBundle
LOAD ok sun.text.resources.cldr.FormatData_en     super=java.util.ListResourceBundle
LOAD ok sun.text.resources.cldr.ext.FormatData_ru super=java.util.ListResourceBundle
FAIL sun.text.resources.cldr.ext.FormatData_ru -> java.lang.reflect.InaccessibleObjectException:
     module jdk.localedata does not "opens sun.text.resources.cldr.ext" to unnamed module
```

**The classes load.** The only failure was `setAccessible` — a JPMS check on the
*reflective caller*, which is exactly right for an unnamed module and which a Rust native
does not go through. Re-run with `--add-opens`, same binary:

```text
  NEW ok sun.text.resources.cldr.ext.FormatData_ru
  CONTENTS rows=415
  MonthAbbreviations=[янв., февр., мар., апр., мая, июн., июл., авг., сент., окт., нояб., дек., ]
```

Resource lookup — the mechanism `try_class_bundle` already uses to decide a candidate
exists — works too, on the same binary, and correctly misses a locale that is not there:

```text
FOUND sun/text/resources/cldr/ext/FormatData_ru.class
FOUND sun/util/resources/cldr/ext/CurrencyNames_ru.class
MISS  sun/text/resources/cldr/ext/FormatData_zz.class
```

So the answer to "curated table versus general mechanism" is not a judgement call. The
data is in the image this VM already boots from — 1157 locales of it — behind a public
no-arg constructor and a `getContents()` that returns `Object[][]`. **No curated table was
built.** The ones already in the file survive only as a fallback.

Why it looked unreachable for so long: the *stock JDK path* to that data really is
unreachable here. `ResourceBundle.getBundle` walks caller-module checks that NPE in
CratonVM's partial bootstrap, and `Bundles`/`LocaleProviderAdapter` add a ServiceLoader
chain on top. Every one of those comments is a true statement about the JDK's own loader
and a false statement about the class files. Nobody had tried loading the class directly.

### The bundle layout, enumerated from the live jimage rather than assumed

| family | ROOT + `_en` (java.base) | every other locale (jdk.localedata) |
|---|---|---|
| `FormatData` | `sun.text.resources.cldr.*` | `sun.text.resources.cldr.ext.*` |
| `CurrencyNames` / `LocaleNames` / `TimeZoneNames` | `sun.util.resources.cldr.*` | `sun.util.resources.cldr.ext.*` |
| `CalendarData` | `sun.util.resources.cldr.CalendarData` (ROOT only) | — |

## 2. What was implemented

A CLDR reader and a process-wide cache in `native-builtins/src/locale_resources.rs`:
`load_cldr_table(base, lang, country)` walks ROOT ▸ `_<lang>` ▸ `_<lang>_<country>`,
probing both the base and the `ext` package for each, instantiates every candidate that
exists, reads `getContents()`, and merges least-specific-first — the JDK parent chain
flattened into one map, the same shape `build_locale_chain` already uses for
`.properties`.

**Values are decoded into Rust strings and cached that way.** Caching the `ObjectRef`s
would have been a stale pointer one moving collection later; the Java strings are rebuilt
per use.

Six surfaces read it. They are one commit on purpose: W7-44 declined to make
`getNumberPatterns` locale-aware while `getDecimalFormatSymbolsData` stayed en, because
`#,##0.00 ¤` over en's `.`/`,` gives `1,234.50 €` for de — *further* from HotSpot. Landing
them apart would have shipped exactly that.

| surface | before | after |
|---|---|---|
| the synthetic `FormatData` bundle | curated en, and `build_bundle` **was never told the locale** | the requested locale's CLDR, overlaid on the curated en baseline |
| `LocaleResources.getNumberPatterns` | 4 hardcoded strings | `latn.NumberPatterns` for its `locale` field |
| `LocaleResources.getDecimalFormatSymbolsData` | 13 hardcoded strings | `latn.NumberElements` for its `locale` field |
| `DecimalFormatSymbols.initialize` | curated 12-language separator list; `$`/`USD` | the same `NumberElements`; code from `Currency.getInstance(Locale)`, symbol from `CurrencyNames` |
| `Currency.getSymbol(Locale)` and `getSymbol()` | 8-entry / 4-entry code→symbol tables, locale-independent | `CurrencyNames` for the locale |
| `getDateTimePattern` / `getJavaTimeDateTimePattern` | curated en + de arrays | `DatePatterns` / `TimePatterns` / the real combiner |

Three details that are not obvious and are each load-bearing:

1. **The number tables are keyed by numbering system.** The real
   `LocaleResources.getNumberStrings` reads `<DefaultNumberingSystem>.NumberElements`,
   then `latn.NumberElements`, then the bare key. CLDR defines only the prefixed forms, so
   a reader that looked up `NumberElements` would find nothing for **every** locale and
   the whole number surface would have stayed en while looking locale-aware. That is a
   vacuous-green shape: the code would run, the fallback would answer, and the probe would
   show en on a ru host exactly as before.
2. **`NumberPatterns` slot 3 is the ACCOUNTING pattern, not the scientific one.**
   `NumberFormatProviderImpl` has `ACCOUNTINGSTYLE = 3` and consults it only under
   `-u-cf-account`. The `#E0` that stood there was mislabelled; it was harmless because
   nothing could reach it.
3. **The legacy 9-slot `DateTimePatterns`** (4 time + 4 date + 1 combiner) that
   `populate_format_data_en` writes does not exist in CLDR, which splits it into
   `TimePatterns` / `DatePatterns` / a 4-slot combiner. The overlay would have silently
   shrunk that key to four entries, so it is rebuilt from the real per-locale patterns.

### Coverage, measured over all 1157 locales

`probes/CldrChainProbe.java` reimplements the Rust chain in Java and diffs it against the
JDK's own `DateFormatSymbols` / `DecimalFormatSymbols` for every installed locale. This is
the algorithm's test; it does not need a CratonVM build.

```text
locales.total=1157
chain.empty=0                  <- every locale resolves at least one class
miss.months=70                 miss.shortMonths=166
miss.weekdays=60               miss.eras=84
miss.decimalSeparator=31       miss.groupingSeparator=43
```

**Not one locale falls back to en.** Every miss is one of two named, unmodelled things:

* **CLDR's non-truncating parents** — `en_GB` → `en_001` → `en` (chain gives `Sep`, real
  is `Sept`); `es_MX` → `es_419`; `pt_AO`; `zh_TW` → `zh_Hant`. The chain lands on the
  right *language* with a wrong *regional* detail.
* **The script subtag** — `zh_CN_#Latn`, `az__#Cyrl`, `hi__#Latn`, `pa__#Arab`. These are
  the loudest misses (a whole different script) and they are also the locales nothing in
  our suites uses.

Spot-checked exact/diff, same probe:

```text
EXACT en-US ru-RU de-DE tr-TR ja-JP fr-FR pt-BR zh-CN it-IT nl-NL pl-PL ko-KR sv-SE fi-FI cs-CZ uk-UA ar-EG
DIFF  en-GB (Sept/Sep)  es-MX  zh-TW
```

The uncovered path is visible, not silent: `load_cldr_table` emits a `tracing::warn!`
naming the family and locale when **no** candidate class loads, once per (family, locale),
and only for the three families CLDR is expected to answer — warning on `BreakIteratorInfo`,
which legitimately has no `cldr` package, would be crying wolf and is how a real fallback
notice gets filtered out of a log.

### Deliberately NOT overlaid

`CalendarData`. Its CLDR `firstDayOfWeek` is not the plain `"1"` this file's curated table
writes — it is a country-list string that `CLDRCalendarDataProviderImpl` parses per region:

```text
firstDayOfWeek = 1: AG AS BD BR BS BT … ;2: 001 AD AE AI AL … ;6: MV;7: AE AF BH …
```

Overlaying it would replace a value a consumer reads as an integer with one it cannot
parse, and the failure would surface far from here. `TimeZoneNames` is excluded for the
same class of reason (its values are `String[][]`, and it already has a real-class path
via `needs_concrete_bundle_class`). The overlay is a **whitelist** — `FormatData`,
`CurrencyNames`, `LocaleNames` — not "every family CLDR has".

## 3. Proving the RED

`probes/DefaultLocaleProbe.java` (extended, not replaced) now prints, per locale and from
an **explicit `Locale` argument**: months, short months, weekdays, short weekdays, eras,
am/pm, `DateFormat` FULL and MEDIUM on a fixed instant, the literal `SimpleFormatter` head
`%1$tb %1$td, %1$tY %1$tl:%1$tM:%1$tS %1$Tp`, `%1$tb`'s length, number / currency /
negative currency / percent / `%,.2f`, and the currency **symbol and code** — plus the
same row for the default FORMAT locale.

Explicit locales on purpose. `assertEquals("ru_RU", Locale.getDefault().toString())` tests
W7-67's landed reporting fix and says nothing about the data; and a date assertion that
only checks non-emptiness passes against en data on a ru host, because an English date is
perfectly non-empty. Every date row prints rendered text.

Measured, same class file, this host, `-Duser.language=ru -Duser.country=RU` on both:

| observable (explicit `ru_RU`) | HotSpot | CratonVM today |
|---|---|---|
| `months[0]` | января | January |
| `shortMonths[7]` | **авг.** | **Aug** |
| `weekdays[1]` | воскресенье | Sunday |
| `eras` | до н. э., н. э. | BC, AD |
| `date.FULL` | четверг, 6 августа 2026 г. | Thursday, August 6, 2026 |
| `simpleFormatterHead` | `авг. 06, 2026 …` | `Aug 06, 2026 …` |
| `%1$tb`.length | **4** | **3** |
| `number` | 1 234,5 | 1,234.5 |
| `currency` | 1 234,50 ₽ | $1,234.50 |
| `percent` | 76 % | 76% |
| `currency.code` | RUB | **USD** |
| `currency.symbol` | ₽ | **$** |
| `currency.ofLocale` | RUB | **RUB** ← already correct |

The `de_DE` row is the chimera W7-67 predicted, arriving from the other direction:
CratonVM renders `Thursday, 6. August 2026` — de's *pattern* over en's *symbols*.

That last table row is the part worth keeping. `Currency.getInstance(Locale)` — java.base's
own `currency.data` — was **already right**. The currency code was not missing data; it
was never asked for, because `DecimalFormatSymbols.initialize` wrote the literal `"USD"`
over it.

### Why the gap is exactly two bytes

`SimpleFormatter`'s default format opens `%1$tb`. `java.util.Formatter` renders that from
`DateFormatSymbols.getInstance(l).getShortMonths()`. Today, in August, en gives `Aug` (3
chars) and ru gives `авг.` (4). `RJdkLogging.formattedOutputIsRealBytes` logs **two**
records and prints `text.length()`. 175 + 2 = 177.

**So yes: `RJdkLogging` unpinned should reach `streamBytes=177` and the vector should go
green** — provided the `LocaleData.getBundle` native is the path `DateFormatSymbols`
takes, which was measured rather than assumed:

```text
$ CRATONVM_DBG_CATALINA=1 cratonvm --jdk-only -Duser.language=ru -Duser.country=RU … DefaultLocaleProbe
      6 CATALINA-DBG: ResourceBundle.getBundle native — name="sun.text.resources.FormatData"
```

Six calls, one per distinct locale the probe asks for, all through the native this change
rewrites. That is the whole chain from `%tb` to the table.

Note the base name: **`sun.text.resources.FormatData`**, the legacy JRE package, where
HotSpot's CLDR default asks for `sun.text.resources.cldr.FormatData`. CratonVM's
`LocaleProviderAdapter` selection differs from HotSpot's — a separate, smaller defect,
left open. `cldr_packages` therefore maps *both* names onto the CLDR classes: HotSpot's
answer is the oracle and HotSpot answers from CLDR, so resolving to `jdk.localedata`'s
older JRE-package copies would have reproduced the wrong data faithfully.

**The honest caveat on 177.** The number is date-dependent. It is 177 because August's
Russian abbreviation is four characters; in May (`мая`, 3 chars) the same correct VM would
print 175. The vector asserts a length, so its expectation follows the calendar on a
ru_RU host. That is a property of the vector, not of this change, and it is worth knowing
before someone reads a future 175 as a regression.

## 4. What still is not locale-aware

| surface | state after W7-80 |
|---|---|
| `DateFormatSymbols` months / weekdays / eras / am-pm | **CLDR, per locale** |
| `DateFormat` FULL/LONG/MEDIUM/SHORT date and time patterns | **CLDR, per locale** |
| `DecimalFormatSymbols` — all 13 symbols | **CLDR, per locale** |
| `NumberFormat` number / currency / percent patterns | **CLDR, per locale** |
| `NumberFormat` currency code and symbol | **CLDR + `currency.data`, per locale** |
| `Currency.getSymbol()` / `getSymbol(Locale)` | **CLDR, per locale** |
| `getDisplayLanguage()` / `getDisplayCountry()` | **still the raw code.** The `LocaleNames` bundle is now CLDR-populated, but nothing reaches it: `Locale.getDisplayString` goes through `LocaleServiceProviderPool` → `LocaleNameProvider`, and the measured run makes **zero** `LocaleNames` bundle requests. The provider pool, not the data, is the gap |
| CLDR non-truncating parents (`en_GB`→`en_001`, `es_MX`→`es_419`) | not modelled; degrade to the language bundle |
| script subtag (`sr_Latn`, `zh_Hans`, `az_Cyrl`) | not modelled; degrade to the language bundle |
| `TimeZoneNames` | unchanged (real-class path, ROOT + `_en` only) |
| `CalendarData` / `firstDayOfWeek` | unchanged, deliberately (§2) |
| `LocaleProviderAdapter` type selection | still JRE where HotSpot picks CLDR; masked here, not fixed |
| Compact number formats (`CompactNumberPatterns`) | now present in the bundle, but the synthetic factories in `phases_late/text_intl.rs` do not read it |
| synthetic-JDK mode's own `NumberFormat`/`DateFormatSymbols` bodies | untouched — no JDK image, so `load_cldr_table` finds nothing and the curated tables answer, as before |

## 5. Registration ownership, `NativeKind`, env vars

* **Every method changed here was already registered by `locale_resources::register`, and
  that registrar is the only writer of each triple.** No new registrations, so no
  last-write-wins hazard was created. Checked by grepping each method name across the
  tree: the only other `Currency.getSymbol` registration is
  `phases_early.rs::register_currency_natives`, and it owns the **no-arg** descriptor
  (`()Ljava/lang/String;`), a different triple.
* That no-arg registration reaches the registry only through
  `register_synthetic_overrides` → `register_phase51_natives` (lib.rs:23866), i.e.
  **synthetic-JDK mode only**; `locale_resources::register` runs from
  `register_essential_natives` (lib.rs:19113), the real-JDK / `--jdk-only` path. In those
  two modes `Currency.getSymbol()`'s real bytecode delegates to `getSymbol(Locale)` and
  lands on the 1-arg native. Editing the no-arg body therefore moves **synthetic mode
  only**, and moves it toward the other two.
* `NativeKind` is ambient over the whole of `locale_resources::register` — it sets
  `Bridge` at the top and restores at the end. Every edit here is inside an existing
  closure body, so no registration crossed a category boundary.
* **No new `CRATONVM_*` env var**, so no `flag_groups.rs` / `flag-surface.txt` /
  `flag-tokens.md` / `flag-inventory.md` churn and no `cargo test -p cratonvm-types` red.
  The one new diagnostic is a `tracing::warn!`, which needs no flag.

## 6. Compatible mode

`--real-jdk` is contractually frozen except for HotSpot-parity fixes. Every change here is
parity: each one replaces a value that disagreed with HotSpot with the value HotSpot reads,
out of HotSpot's own data files. Per change, what moves and where:

| change | moves on an en_US host (CI) | moves on a non-en host |
|---|---|---|
| `FormatData` months/weekdays/eras | **no** — CLDR `FormatData_en` matches the curated table for all of them | yes, to that locale |
| `DateTimePatternChars` | **yes**: 23 chars → 19. HotSpot returns 19 (`GyMdkHmsSEDFwWahKzZ`), measured. The curated 23 was wrong | same |
| `getNumberPatterns` | **no** for slots 0/1/2 — CLDR en is byte-identical to the hardcoded values. Slot 3 changes from `#E0` to the accounting pattern, which is unreachable without `-u-cf-account` | yes |
| `getDecimalFormatSymbolsData` / `DFS.initialize` | **no** — CLDR en `NumberElements` is byte-identical, including the empty slots 11/12 that fall back to the plain separators | yes |
| currency **code** | **no** — `Currency.getInstance(Locale.US)` is USD, which is what was hardcoded | yes (RUB, EUR, TRY, JPY …) |
| currency **symbol** | **yes**, and this is the one real CI-visible movement — see below | yes |
| date/time patterns | **no** — CLDR en `DatePatterns`/`TimePatterns`/combiner are byte-identical to the curated en arrays, U+202F included | yes |

The currency-symbol row, measured on HotSpot en_US against the two curated tables:

| code | CratonVM before (1-arg / no-arg) | HotSpot en_US | after |
|---|---|---|---|
| USD / EUR / GBP / JPY | `$` `€` `£` `¥` | same | same |
| CNY | `¥` | **`CN¥`** | `CN¥` |
| CAD | `$` | **`CA$`** | `CA$` |
| AUD | `$` | **`A$`** | `A$` |
| INR / BRL / KRW / MXN | the bare code | `₹` `R$` `₩` `MX$` | correct |
| RUB on a ru host | `RUB` | `₽` | `₽` |

So the claim "Linux CI is `LANG=C`/unset → `en_US`, so CI should not move" is **almost**
right and worth stating precisely: CI's *locale* does not move, and neither do dates,
months, patterns or separators. Two en_US-visible things do move, both toward HotSpot —
`DateFormatSymbols.getLocalPatternChars()` (23 → 19 chars) and currency symbols outside
`{USD, EUR, GBP, JPY}`.

## 7. What the orchestrator should expect

* **`RJdkLogging` unpinned: expected to reach `streamBytes=177` and go green**, on both
  arms, closing the last strict red. Pinned runs stay byte-identical, as they were.
* **No test was weakened.** The regression-suite vectors that touch formatting are already
  disciplined — `RStrings`, `RJdkHello`, `RJdkViews`, `RJdkLambdas`, `RJdkSecurity` pin
  `Locale.ROOT` or `Locale.US` on every formatting and case-mapping assertion, and both of
  those resolve through the CLDR ROOT / `_en` bundles to the same values the curated
  tables held. Nothing in the suite asserts a currency symbol.
* **The Rust tests that name `DateFormatSymbols` / `DecimalFormatSymbols` / `Currency` all
  live in `vm/src/vm/tests.rs`** (lines ~37505, ~46230–46360) and exercise the
  **synthetic-JDK** `phases_late/text_intl.rs` bodies, which this change does not touch.
  That module is synthetic-JDK-only and is not compiled by a `--lib` run at all — so a
  green `cargo test` there proves nothing about this change either way. There is no
  existing assertion on `Currency.getSymbol`.
* **On this ru_RU host, expect newly-visible movement in anything unpinned.** Before
  W7-67 the VM lied about the locale; after W7-67 it told the truth over en data; after
  W7-80 it tells the truth over the right data. A test that passed by asserting en output
  on an unpinned run is now asserting en on a Russian host, and should be pinned rather
  than have the VM bent back.
* **Startup cost.** Each `(family, locale)` pair costs one-time class instantiation plus a
  `getContents()` evaluation — 685 rows for ROOT `FormatData`, 415 more for `_ru` — and
  the merged table is then cached in Rust for the process. What is **not** cached is the
  per-call fill of the synthetic bundle's Java map, which is now ~1100 entries instead of
  ~40. `DateFormatSymbols` and `LocaleResources` both cache per locale upstream, and the
  measured probe made six `getBundle` calls in total, so this is expected to be
  unnoticeable — but a workload that calls `ResourceBundle.getBundle` on a locale-data
  base name in a loop would feel it. If a boot-time regression shows up, caching the built
  bundle per `(base name, locale)` in `rb_get_bundle` is the fix, and it is independent of
  everything else here.
* **Synthetic-JDK mode will emit the new fallback warning** — there is no JDK image, so
  `load_cldr_table` finds nothing and warns once per (family, locale) before answering
  from the curated tables. That is the message doing its job, not a defect.

## 8. Re-measuring

Nothing here ran against a CratonVM binary built from this branch. To close:

1. Build and run `probes/DefaultLocaleProbe.java` on both arms with **no** `-Duser.*`,
   against HotSpot on this host. The `ru_RU.*`, `de_DE.*`, `tr_TR.*` and `ja_JP.*` rows
   must match HotSpot exactly; `en_GB` is expected to differ on `Sept`/`Sep` and is
   documented as such in §2.
2. Run the strict corpus unpinned. `RJdkLogging` must print `streamBytes=177`.
3. Grep the run's stderr for `W7-80:`. On a real-JDK image that warning should not appear
   at all; if it does, `ctx.find_resource` is not reaching the jimage classes the way
   `try_class_bundle` does, and the whole change is inert — an inert registration looks
   exactly like a missing feature, so check this before concluding the data is wrong.
4. `probes/CldrChainProbe.java` is the algorithm's regression test and needs only HotSpot;
   rerun it after any JDK bump, since the miss counts are CLDR-version-specific.
