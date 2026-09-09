# `Calendar` reports the same week rules for every locale, because CratonVM synthesises the `CalendarData` bundle with a plain `"1"` where CLDR has a region table

**Status:** open. Cause MEASURED and located to one function; fix deliberately
not attempted this session (see "Scope").
**Applies to:** every mode and both JDK images — `--real-jdk` and `--jdk-only`,
JDK 21 and JDK 25. **Mode-independent**, which is what distinguishes it from the
locale row retired the same day.
**Found:** 2026-09-09 by `probes/LocaleSelect.java`, as a side effect of the
`DecimalFormatSymbols` fix. Diagnosed the same day by `probes/CalWeek.java`.

> **This page has been wrong twice, and both wrong versions are recorded rather
> than deleted.** It was first filed as "the provider lookup answers `null`",
> then revised to "the provider resolves against ROOT resources". Neither is the
> cause. The measurement that settles it is section 2.

## 1. The one wrong answer

```
Calendar.getInstance(loc)      getFirstDayOfWeek()   getMinimalDaysInFirstWeek()
  HotSpot 21    en-US                  1                        1
  HotSpot 21    de-DE                  2                        4
  HotSpot 21    fr-FR                  2                        4
  CratonVM (any mode, either image)
                en-US                  1                        1     correct*
                de-DE                  1                        1     WRONG
                fr-FR                  1                        1     WRONG
```

\* `en-US` is correct **by luck**, and that matters — see section 3.

## 2. The cause, measured layer by layer

`probes/CalWeek.java` goes straight at the CLDR adapter, bypassing both
`LocaleServiceProviderPool` and the shadowed `getLocaleServiceProvider`, and
asks each layer separately. For `de-DE`:

```
                                   HotSpot 21                      CratonVM 21
L2 resources.locale                de_DE                           de_DE     <- SAME
L2 getCalendarData(firstDayOfWeek) "1: AG AS BD BR ... US ...;     "1"       <- DIFFERENT
                                     2: 001 AD ... DE ...;
                                     6: MV;7: AE AF ..."
L1 provider.getFirstDayOfWeek      2                               0
```

Three things follow immediately.

**It is not a selection fault.** The `LocaleResources` handed out carries the
REQUESTED locale (`de_DE`) on both VMs. The previous revision of this page
blamed root resources; that was wrong, and it was wrong because it reasoned by
analogy with the `DecimalFormatSymbols` row instead of asking this question.

**The week rules are not per-locale data at all.** HotSpot returns the SAME
string for `en-US` and `de-DE` — a region-keyed table.
`CLDRCalendarDataProviderImpl` parses it and selects by the locale's COUNTRY. So
correct `LocaleResources` is necessary but nowhere near sufficient.

**CratonVM's value is a plain `"1"`**, which is not a region table. The real
provider finds no region in it, returns `0`, and `CalendarDataUtility`'s
"not in 1..7" guard falls back to its default of `1` — for every locale.

## 3. Why `en-US` looked healthy, and why that is dangerous

`CalendarDataUtility.retrieveFirstDayOfWeek` defaults to `1` (Sunday) and
`retrieveMinimalDaysInFirstWeek` defaults to `1`. For `en-US` those defaults are
the CORRECT answer. The control therefore passed for the wrong reason, and any
change that makes the lookup "work" without supplying a real region table will
BREAK it — which is exactly what happened in section 5.

## 4. The site

`native-builtins/src/locale_resources.rs`. The synthesis arm

```rust
} else if bundle_name.starts_with("sun.util.resources.CalendarData")
    || bundle_name.starts_with("sun.util.resources.cldr.CalendarData")
{
    populate_calendar_data_en(ctx, &mut map_now);
}
```

serves a hand-built bundle whose body is

```rust
put_str(ctx, map_pin, map, "firstDayOfWeek", "1"); // Sunday
put_str(ctx, map_pin, map, "minimalDaysInFirstWeek", "1");
```

**The file already knows this is wrong.** The overlay whitelist ~700 lines later
excludes `CalendarData` for precisely this reason:

> `CalendarData` is the reason: its CLDR `firstDayOfWeek` is not the plain `"1"`
> this file's curated table writes, it is a country-list string — `"1: AG AS BD
> BR …;2: 001 AD AE …;6: MV;7: AE AF BH …"` — which the real
> `CLDRCalendarDataProviderImpl` parses per region. Overlaying it would replace
> a value a consumer reads as an integer with one it cannot parse.

That caution guards the OVERLAY path. It does not guard the SYNTHESIS path
above, which serves the same `"1"` to the same parser. The comment describes the
defect and sits in the same file as it.

## 5. Two fixes that were tried or proposed and do NOT work

**Delegating the `CalendarDataProvider` arm** — written, built, measured
2026-09-09, reverted. Making `getLocaleServiceProvider(CalendarDataProvider.class)`
return the real provider gives `2/1` for EVERY locale and regresses the `en-US`
control:

```
                    HotSpot 21   before      with the arm
  en-US (control)     1 / 1      1 / 1  ok   2 / 1  WRONG (regressed)
  de-DE               2 / 4      1 / 1  bad  2 / 1  still wrong
  fr-FR               2 / 4      1 / 1  bad  2 / 1  still wrong
```

`2` is what CLDR's table gives region `001`, the world default. Section 2
measures the CLDR provider answering `0` when asked DIRECTLY, so the `2` arrives
by some other route through `LocaleServiceProviderPool` — **not explained here,
and not needed**: either way the arm cannot be right while the bundle says `"1"`.
Recorded so nobody re-spends the build.

**The `null` framing.** The blanket `null` from
`JRELocaleProviderAdapter.getLocaleServiceProvider` is real, and is described in
`../internal/retired/jdk-21-strict-mode-answers-a-non-root-locale-with-root-resources-FIXED-20260909.md`.
It is not what makes the week rules wrong: acting on it alone produces the
regression above.

## 6. What a real fix has to do

Supply the CLDR region table for `firstDayOfWeek` / `minimalDaysInFirstWeek`, or
stop synthesising the `CalendarData` bundle at all when the real JDK image can
supply it. The second is the honest repair and is the larger change: the
synthesis exists so locale-sensitive formatting works when the resource chain
cannot be walked, and removing it is a behaviour change for every consumer of
that bundle, not only `Calendar`.

Whichever is chosen, the acceptance test is fixed in advance by section 1:
`en-US` must STAY `1/1` while `de-DE` and `fr-FR` become `2/4`. A change that
moves `en-US` off `1/1` is a regression regardless of what it does to the rest.

## Scope

Not attempted on 2026-09-09: narrowing or retiring a synthetic resource that
shadows real JDK data is the same family as the synthetic-bridge retirement that
was out of scope for that session. This page exists so the next person starts
from the measurement instead of from the two wrong theories above.

## Reproducing

```
JAVA_HOME=<jdk21> cratonvm --add-exports java.base/sun.util.locale.provider=ALL-UNNAMED \
                           --add-opens java.base/sun.util.locale.provider=ALL-UNNAMED \
                           -cp . CalWeek
```

`probes/CalWeek.java` prints the public answer and then both layers, with
`en-US` as the control. Compare against `<jdk21>/bin/java` with the same flags.
The single line that decides it is `L2 getCalendarData(firstDayOfWeek)`: a region
table means the bundle is real, a bare `"1"` means it is the synthesised one.

## Related

- `../internal/retired/jdk-21-strict-mode-answers-a-non-root-locale-with-root-resources-FIXED-20260909.md`
  — the same registration's blanket `null`, and the `DecimalFormatSymbolsProvider`
  arm that WAS landed. That row really was a selection fault; this one is not,
  and the analogy is what made this page wrong twice.
