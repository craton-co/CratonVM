# `Calendar` reports root week rules for every locale, because the provider lookup still answers `null` for `CalendarDataProvider`

**Status:** open, cause known, fix known and prescribed. Not a strict-mode row.
**Applies to:** every mode and both JDK images — `--real-jdk` and `--jdk-only`,
JDK 21 and JDK 25. It is **mode-independent**, which is what distinguishes it
from the locale row retired the same day.
**Found:** 2026-09-09, by `probes/LocaleSelect.java` — as a side effect of
fixing the `DecimalFormatSymbolsProvider` arm, not as the thing being looked
for. The probe prints it because section A asks five locale-sensitive services
through public API, not just the one under investigation.

## The one wrong answer

```
Calendar.getInstance(loc)          getFirstDayOfWeek()   getMinimalDaysInFirstWeek()
  HotSpot 21          en-US                 1                        1
  HotSpot 21          de-DE                 2                        4
  HotSpot 21          fr-FR                 2                        4
  CratonVM (any mode, either image)  de-DE  1                        1     WRONG
  CratonVM (any mode, either image)  fr-FR  1                        1     WRONG
```

`en-US` is the control and it agrees everywhere — which it must, because `1/1`
is also what the defect produces. An arm where the control disagrees is a broken
probe, not a finding.

## The cause

The same one that produced the retired locale row, one switch arm over.
`native-builtins/src/locale_bootstrap.rs` registers a native over
`sun/util/locale/provider/JRELocaleProviderAdapter.getLocaleServiceProvider`.
It used to answer `null` for every SPI class; it now delegates for
`java.text.spi.DecimalFormatSymbolsProvider` and still answers `null` for the
rest, `java.util.spi.CalendarDataProvider` among them.

`LocaleProviderAdapter.findAdapter` accepts an adapter only when that lookup is
non-null, so for `CalendarDataProvider` no adapter is ever accepted,
`getAdapter` falls through to `fallbackLocaleProviderAdapter`, and the week
rules come from root data. Nothing throws and nothing is missing — the CLDR
adapter, asked directly, has the right data in every cell.

Why this one is mode-independent while the retired locale row was not: there is
no synthetic `Calendar` stub bypassing the walk in `--real-jdk` the way
`DecimalFormatSymbols.initialize` bypasses it. Both modes take the real path,
so both are wrong.

## The fix

Add the `CalendarDataProvider` arm to the switch in `locale_bootstrap.rs`,
delegating to the receiver's `getCalendarDataProvider()`, exactly as the
`DecimalFormatSymbolsProvider` arm now does.

The wave-4 caution above that registration still governs and should be obeyed
rather than skipped because the first arm was uneventful: `get*Provider()`
instantiates the adapter's inner provider off `LocaleDataMetaInfo` /
`LocaleResources`, which is the resource-bundle chain this whole C20 override
exists to bypass for Jackson / H2 / `Locale.getDefault()` formatting. Delegate
ONE SPI at a time and re-run the formatting suites for each. Do not blanket-
delegate the remaining ~11 on the strength of reading the switch.

## Reproducing

```
JAVA_HOME=<jdk21> cratonvm -cp . LocaleSelect
```

Read the `Calendar firstDay` lines under `[de-DE]` and `[fr-FR]` in section A,
against the same lines from `<jdk21>/bin/java -cp . LocaleSelect`. Section A
needs no `--add-exports`: it is public API only, so no step of it can be mute.

## Related

- `docs/internal/retired/jdk-21-strict-mode-answers-a-non-root-locale-with-root-resources-FIXED-20260909.md`
  — the same registration, the `DecimalFormatSymbolsProvider` arm, and the
  measured four-cell table showing the blanket `null` is present everywhere.
