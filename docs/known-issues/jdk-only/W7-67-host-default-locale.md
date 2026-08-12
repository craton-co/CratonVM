# W7-67 — the default locale was hardcoded `en_US`, and `Locale.Category` was collapsed

**Status: SOURCE LANDED, NOT RE-MEASURED.** No CratonVM binary was built from this branch —
this lane does not run `cargo build`. Every HotSpot column below is a live run on
jdk-25.0.3.9-hotspot on this host (Windows 11, **ru_RU**). Every "CratonVM today" column is a
live run of `C:/craton/CratonVM/target/release/cratonvm.exe` (built 2026-08-12 04:11, i.e.
current-dev-ish, pre-change). The "post-fix" columns are **measured, not predicted** — see
"Why the post-fix column is a measurement" below.

Found while closing the last failing vector in the strict corpus: `RJdkLogging` passed every
assertion and failed only on a transcript byte count (`streamBytes=175` vs HotSpot's `177`).
Pinning `-Duser.language=en -Duser.country=US` on both sides made the transcripts
byte-identical. The residual was `SimpleFormatter` rendering its date in Russian on HotSpot
and in English on CratonVM. That is not a logging defect. It is a VM initialisation defect.

---

## 1. The defect, and whether the OS was queried at all

**It was not queried.** `vm_init::derive_locale()` read `$LC_ALL` then `$LANG` and nothing
else:

```rust
let raw = runtime_var("LC_ALL").or_else(|_| runtime_var("LANG")).unwrap_or_default();
```

Windows sets neither. So the function fell through to its final line — `("en", "US")` — on
**every Windows host, unconditionally**. The doc comment said so out loud ("Windows has no
direct env equivalent; we default to `en`/`US`"), which is how a hardcoded value survives
review: it was documented as a fallback and was in fact the only outcome.

Everything downstream was already correct and already reading the properties.
`locale_bootstrap::resolve_default_locale` seeds `Locale.getDefault()` from
`user.language`/`user.country`, and the second registrar (`t3_impl::register_t311_i18n`,
synthetic-jdk, last-write-wins for the no-arg overload) calls the *same* helper. So there was
exactly one producer to fix and no inert-registrar hazard: `derive_locale` was the single
source of the wrong answer for both modes and both registrars.

Measured, same class file, same host:

| | HotSpot | CratonVM `--jdk-only` |
|---|---|---|
| `Locale.getDefault()` | `ru_RU` | `en_US` |
| `Locale.getDefault(FORMAT)` | `ru_RU` | `en_US` |
| `Locale.getDefault(DISPLAY)` | `ru_RU` | `en_US` |
| `user.language` / `user.country` | `ru` / `RU` | `en` / `US` |
| `user.script` / `user.variant` | `""` / `""` | **absent (null)** | 

The last row is a second, smaller parity gap: HotSpot publishes all four keys of the family,
empty string included. `System.getProperty("user.variant")` is `""` there, never `null`.

The Unix branch had a real bug too, just a quieter one: the precedence `LC_ALL` ▸ `LANG`
**skips the category variables**. glibc resolves `LC_ALL` ▸ `LC_CTYPE`/`LC_MESSAGES` ▸ `LANG`,
so a host with `LANG=C LC_CTYPE=de_DE.UTF-8` reported `en_US` on CratonVM and `de_DE` on
HotSpot.

## 2. What the fix reads

`java_props_md.c` reads **two** host locales, and the fix mirrors that:

| category | Windows | Unix |
|---|---|---|
| DISPLAY → `user.language` etc. | `GetUserDefaultUILanguage()` (Settings ▸ Language) | `LC_MESSAGES` |
| FORMAT → `user.language.format` etc. | `GetUserDefaultLocaleName()` (Settings ▸ Region ▸ Regional format) | `LC_CTYPE` |

`GetUserDefaultLocaleName` is the modern replacement for the `GetUserDefaultLCID` +
`GetLocaleInfo(LOCALE_SISO639LANGNAME / LOCALE_SISO3166CTRYNAME)` pair `java_props_md.c`
uses. It returns the same locale already assembled as a BCP-47 name, so the script subtag
survives (`zh-Hans-CN`) instead of having to be rebuilt from the LCID.

Verified against the live API on this host, before writing the VM code:

```text
GetUserDefaultLocaleName   written=6 value="ru-RU"
GetSystemDefaultLocaleName written=6 value="ru-RU"
GetUserDefaultUILanguage   langid=0x0419 -> "ru-RU"
```

which is exactly the `ru`/`RU` with **no** `.format` overlay that HotSpot publishes here.

The property publication follows `jdk.internal.util.SystemProps.fillI18nProps` rather than an
invented rule. Three parts of it are load-bearing and none is obvious:

1. **A command-line `-Duser.<base>` returns early**, so it wins *and suppresses the derived
   overlay*. Without this, `-Duser.language=en -Duser.country=US` on this ru_RU host would pin
   the base to `en` and leave `user.language.format=ru` behind — a "pinned locale" run that
   still formats numbers in Russian. This is precisely the regression-suite mechanism, so
   getting it wrong would have been invisible until a suite disagreed with itself.
2. **The base property takes the DISPLAY value**, not the format one.
3. **`.display` is never created from platform values.** The JDK writes it only when it
   differs from the base — and the base has just been set *from* it, so the condition is dead.
   Only `.format` can appear.

## 3. The Category collapse is a separate defect — and it had to move with this one

`Locale.getDefault(Locale$Category)` was registered as:

```rust
|ctx, _args| get_or_create_default(ctx),
```

The argument was discarded. All three defaults were one cached `ObjectRef`, so
`getDefault(FORMAT)` could not disagree with `getDefault()` under any configuration.

It is a **separate** defect — it has its own cause (a registration that ignores its
parameter), it is independently reproducible with `-Duser.language.format=de` on any host, and
it was wrong before W7-67 and would stay wrong after a reporting-only fix. Measured, this
host, `-Duser.language.format=de` on both sides:

| | HotSpot | CratonVM today |
|---|---|---|
| `getDefault()` | `ru_RU` | `en_US` |
| `getDefault(FORMAT)` | `de_RU` | `en_US` |
| `getDefault(DISPLAY)` | `ru_RU` | `en_US` |
| `categories.collapsed` | `false` | `true` |

It is fixed here anyway, because leaving it would have made the reporting fix half-inert: once
`vm_init` publishes `user.language.format` for a host whose UI language and regional format
differ, a property that nothing reads is an inert registration by another name.

Note `format=de_RU`, not `de_DE`. `StaticProperty` defaults **each key independently** to its
base (`USER_COUNTRY_FORMAT = getProperty("user.country.format", USER_COUNTRY)`), so a
`.format` language with no `.format` country really does compose with the base country. The
implementation copies that per-key fallback, and the HotSpot run above is the oracle for it.

Three cache slots replace the one. Two consequences that are easy to miss:

* `gc_scan_locale_roots` / `gc_update_locale_refs` iterate **all three**. A slot missed in the
  root scan is a `Locale` a moving young collection is free to reclaim while the cache keeps
  handing back its address — the "Stale pointer detected in invokevirtual receiver …
  java/util/Locale" shape the single-slot root scan was written for.
* `setDefault(Locale)` writes all three (its real body sets the base *and* both categories);
  `setDefault(Category, Locale)` writes only the named one and must leave the base alone.
  These two used to be the same code.

**Not modelled:** a category whose *script* or *variant* differs from the base. The synthetic
`Locale` this module allocates records `(language, country, tag)` only, so widening that means
widening the side table. No host we run on splits those two subtags across categories.

## 4. The judgement: is reporting the true locale without locale-aware data a net improvement?

**Yes on this host, with a named and bounded exception elsewhere.** This is the part of the
task that mattered, so here is the measurement rather than an argument.

### Why the post-fix column is a measurement

The `-D` override path already works, and after the fix the default *is* what `-D` sets today.
So running today's binary with `-Duser.language=ru -Duser.country=RU` reproduces the post-fix
state of every formatting surface exactly, without a build. That is what the third column is.

### On this host (ru_RU): the fix changes the report and nothing else

| observable | HotSpot ru_RU | CVM today (en_US) | CVM `-Dru` (post-fix) |
|---|---|---|---|
| `dateFormat.FULL` | среда, 31 декабря 1969 г. | Thursday, January 1, 1970 | Thursday, January 1, 1970 |
| `DateFormatSymbols` months[0] | января | January | January |
| `String.format("%,.2f")` | 1 234,50 | 1,234.50 | 1,234.50 |
| `NumberFormat` number | 1 234,5 | 1,234.5 | 1,234.5 |
| `NumberFormat` currency | 1 234,50 ₽ | $1,234.50 | $1,234.50 |
| percent | 76 % | 76% | 76% |
| currency code | RUB | USD | USD |
| `Locale.getDefault()` | ru_RU | **en_US** | **ru_RU** |
| `getDisplayCountry()` | Россия | US | RU |

**Not one formatting surface moves.** `ru` is not in the curated separator list (below) and
the date/currency data is en-only, so everything falls back to the uniform en shape whether
the locale says `en_US` or `ru_RU`. The fix is a pure reporting improvement here, with zero
new inconsistency: the only deltas are `Locale.getDefault()` becoming correct and
`getDisplayCountry()` moving from a wrong `US` to a right-code-wrong-language `RU`.

The formatting surfaces stay wrong. They were already wrong. They were *invisibly* wrong,
because the VM also misreported the locale, so the two errors cancelled into a self-consistent
en_US story. After the fix they are visibly wrong, which is the correct state for a defect
nobody has fixed yet.

### Where the hybrid does appear, and why it is still not a regression

`DecimalFormatSymbols.initialize` carries a **curated language list** — `de es it nl pt da pl
ro el tr id` get `,`/`.`, `fr` gets `,`/U+202F, everything else gets en's `.`/`,` — while the
currency symbol is hardcoded `$`/`USD` and the date symbols are en-only. On a host in that
list, reporting the true locale produces the chimera W7-44 warned about:

| observable | HOTSPOT tr_TR | CVM today (en_US) | CVM `-Dtr` (post-fix on a tr host) |
|---|---|---|---|
| `dateFormat.FULL` | 31 Aralık 1969 Çarşamba | Thursday, January 1, 1970 | Thursday, January 1, 1970 |
| months[0] | Ocak | January | January |
| `String.format("%,.2f")` | 1.234,50 | 1,234.50 | 1,234.50 |
| `NumberFormat` number | 1.234,5 | 1,234.5 | **1.234,5** |
| `NumberFormat` currency | ₺1.234,50 | $1,234.50 | **$1.234,50** |
| percent | %76 | 76% | 76% |
| currency code | TRY | USD | USD |
| `"i".toUpperCase()` | İ | **I** | **İ** |
| `"I".toLowerCase()` | ı | **i** | **ı** |
| `"title".toUpperCase()` | TİTLE | **TITLE** | **TİTLE** |

`$1.234,50` — a dollar sign with Turkish grouping — is a real chimera and matches no locale.
Three things keep it from being a reason to decline:

1. **It is reachable today.** `-Duser.language=tr` produces it on the current binary. The fix
   does not create the hybrid; it changes which hosts reach it by default.
2. **It was introduced deliberately.** The curated separator list exists because
   `NumberFormat.parse("1,1")` returned `11.0` for German (Spring DLBF customEditor/converter).
   The project has already decided locale-aware separators are wanted ahead of locale-aware
   currency data.
3. **It is closer to HotSpot, not further.** On a tr host today we render `$1,234.50` against
   HotSpot's `₺1.234,50` — wrong on symbol, grouping and decimal. After the fix, `$1.234,50` —
   wrong on the symbol only. W7-44's decline was about a change that would have made a
   *previously matching* surface stop matching; this one improves two of three axes and
   worsens none.

And the strongest single argument for landing it is the last three rows. **Locale-sensitive
case mapping is already fully correct** — `"i".toUpperCase()` returns `İ` under `-Dtr` today.
It is simply being fed the wrong locale. This is the highest-damage surface in the whole
locale system: the Turkish dotted-I silently corrupts identifiers, hostnames, header names and
SQL keywords, and it fails *quietly*, producing plausible wrong strings rather than an
exception. A VM that hardcodes `en_US` gets it wrong on every Turkish host today; the fix
makes it right. Trading a cosmetic currency-symbol hybrid for correct case mapping is not a
close call.

### Verdict

Land the reporting fix. The surfaces that remain inconsistent after it, named explicitly:

| surface | state after W7-67 |
|---|---|
| `Locale.getDefault()` / `.getDefault(Category)` | **correct** |
| `user.language`/`script`/`country`/`variant` | **correct** |
| `String.toUpperCase()` / `toLowerCase()` / collation-free case ops | **correct** (was wrong on every non-en host) |
| `DecimalFormatSymbols` separators | correct for the 12 curated languages, en for all others |
| `NumberFormat` currency symbol + code | **hardcoded `$`/`USD` for every locale** |
| `NumberFormat` percent / number patterns | en shape for every locale (`#,##0%`, `#,##0.###`) |
| `DateFormatSymbols` months / weekdays / eras | **en for every locale** |
| `getDisplayLanguage()` / `getDisplayCountry()` | returns the raw code, never a localised name |
| resource-bundle candidate chain | now walks the real host locale's chain first |

The staged plan the two lanes together imply, in order: (1) this reporting fix; (2) make
`getNumberPatterns`, `getDecimalFormatSymbolsData` and the currency symbol table locale-aware
**together**, as W7-44 requires — the currency symbol is the single biggest remaining error and
it is a table, not an algorithm; (3) `DateFormatSymbols` / `FormatData` per locale, which is
the large one and probably wants real CLDR data rather than curation.

## 5. Compatible mode

This is a HotSpot-parity fix on both halves — reporting the host locale, and a
`getDefault(Category)` that reads its argument — so it falls inside the carve-out for the
otherwise-frozen `--real-jdk` contract, and it is landed for both modes. Verified that the
Compatible arm behaves identically to the strict arm under `-Duser.language=ru` (same
formatting output, same `Locale.getDefault()`), so the two modes do not diverge.

The honest caveat: Compatible mode's blast radius is larger, because more real JDK bytecode
runs and more of it branches on the locale. If the orchestrator wants to stage, the split
point is clean — `derive_host_locale` is one function and one call site — but there is no
parity argument for staging, only a risk-appetite one.

## 6. What the orchestrator should expect to change in the regression suite

**This host is ru_RU. Before this change every run was effectively locale-pinned to en_US;
after it, only explicitly pinned runs are.** That is the whole shape of the fallout.

* **Runs pinned with `-Duser.language=… -Duser.country=…`: no change at all.** The `-D` path
  behaved identically before and after (it was already the only thing `resolve_default_locale`
  read); the fix only changes what happens when nothing is pinned. `RJdkLogging`'s pinned
  comparison stays byte-identical.
* **`RJdkLogging` unpinned: still expected RED, and for a *newly correct* reason.** The
  `streamBytes=175` vs `177` gap does not close — `SimpleFormatter`'s date still renders in
  English because `DateFormatSymbols` is en-only (§4). What changes is that CratonVM now agrees
  with HotSpot about *which locale it is in* while disagreeing about the data, instead of
  disagreeing about both and looking self-consistent. Closing that vector needs step (3) of the
  staged plan, not this fix. **Do not read a persisting 175/177 as this change having failed.**
* **Any test that silently depends on the default locale may newly fail on this host.** That is
  a finding, not breakage — such a test was asserting en_US behaviour on a machine that is not
  en_US, and it passed only because the VM lied. The suite has been running on this host with
  an accidental global `-Duser.language=en` for its whole life. Expect the surface to be
  number/date/currency formatting assertions and any `toUpperCase()`/`toLowerCase()` comparison
  in a case-mapping-sensitive test.
* **Linux CI is unaffected.** `LANG=C`/`POSIX`/unset still maps to `en_US`
  (`java_props_md.c`'s own mapping, unit-tested), so the Azure and ubuntu-latest arms report
  exactly what they reported before. The only Unix behaviour change is that `LC_CTYPE` and
  `LC_MESSAGES` now participate, which no CI arm sets.
* **No test was weakened.** The two `assert_eq!(…, "en_US")` in `vm/src/vm/tests.rs` assert on a
  Locale built by `new Locale("en","US")`, not on the default, and are locale-independent —
  checked, left alone. `locale_default_and_getters` already derives its expectation from the
  VM's own properties rather than hardcoding, so it follows the host. New tests were added in
  `vm_init.rs`'s own `#[cfg(test)]` module rather than `vm/src/vm/tests.rs`, which is
  synthetic-jdk-only and never compiled by a `--lib` run.
* **No new `CRATONVM_*` env var**, so no `flag_groups.rs` / `flag-surface.txt` /
  `flag-tokens.md` / `flag-inventory.md` churn.

## 7. Probe

`probes/DefaultLocaleProbe.java`. Prints all three categories, the `user.*` family, and the
locale-sensitive observables above. Run it on HotSpot and CratonVM on the same host and diff;
that comparison *is* the test.

The vacuous shapes this deliberately avoids: `assertNotNull(Locale.getDefault())`, which
cannot fail, and `assertEquals("en_US", Locale.getDefault().toString())`, which passes on this
host **because of** the defect and would have certified it green.
