# `Calendar` reports root week rules for every locale

**Title correction, 2026-09-09:** this page was first filed as "...because the
provider lookup still answers `null` for `CalendarDataProvider`". That is the
proximate cause but not the whole one: making the lookup answer non-null
produces CLDR ROOT week data instead, which is worse. The `null` is real, and
behind it sits a root-resource fault in the calendar path.

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

## The obvious fix was TRIED and REFUTED, 2026-09-09

**Do not simply add the `CalendarDataProvider` arm.** It was written, built and
measured, and it is a net REGRESSION. Recorded here so nobody spends the same
build on it twice.

Delegating `getLocaleServiceProvider(CalendarDataProvider.class)` to the
receiver's real `getCalendarDataProvider()` gives:

```
                  HotSpot 21    before the arm    with the arm
  en-US (control)   1 / 1         1 / 1  correct   2 / 1   WRONG (regressed)
  de-DE             2 / 4         1 / 1  wrong     2 / 1   still wrong
  fr-FR             2 / 4         1 / 1  wrong     2 / 1   still wrong
```

Every locale answers `2 / 1`, which is **CLDR root** (`firstDay=mon`,
`minDays=1`). So the delegation does reach the provider -- and the provider
then resolves against ROOT resources, the same root-resource fault as the
`DecimalFormatSymbols` row, in a different service.

The blanket `null` was hiding that behind a piece of luck.
`CalendarDataUtility.retrieveFirstDayOfWeek` falls back to `1` (Sunday) when the
pool yields nothing, and `retrieveMinimalDaysInFirstWeek` falls back to `1`.
For `en-US` those defaults are the CORRECT answer, so the control looked healthy
for the wrong reason. Delegating replaces a lucky default with wrong data, and
breaks the one cell that was right.

**This is why the wave-4 note says one SPI at a time with the suites re-run for
each.** The `DecimalFormatSymbolsProvider` arm was uneventful; this one is not,
and a blanket delegation of all ~11 would have shipped this regression silently
alongside it.

## What the real fix has to do

Make the calendar resource lookup resolve for the REQUESTED locale rather than
root -- i.e. the same question the retired `DecimalFormatSymbols` row answered,
asked of `LocaleResources.getCalendarData` instead. Until that holds, the arm
must stay undelegated: the `null` is wrong, but it is wrong in a way that is
correct for `en-US` and no worse elsewhere.

A useful next measurement: whether
`CLDRLocaleProviderAdapter.getLocaleResources(de_DE)` (which the retired row
proved returns a genuine `de_DE` resources object) yields the right week data
when asked directly. If it does, the fault is again SELECTION -- which pool /
adapter the calendar path ends on -- and not the data.

## The fix that was proposed here first -- SUPERSEDED by the section above

The original text of this page said, and it is left here because it is the
obvious move and the next person will think of it too:

> Add the `CalendarDataProvider` arm to the switch in `locale_bootstrap.rs`,
> delegating to the receiver's `getCalendarDataProvider()`, exactly as the
> `DecimalFormatSymbolsProvider` arm now does.

That was tried on 2026-09-09 and it regresses `en-US`. See "TRIED and REFUTED"
above before writing it again.

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
