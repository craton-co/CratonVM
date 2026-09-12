# RETIRED — FIXED 2026-09-09. `--jdk-only` on JDK 21 no longer answers a non-root locale request with ROOT resources

**Retired from** `docs/known-issues/jdk-only/strict-mode-on-jdk-21-answers-a-non-root-locale-request-with-root-resources-20260909.md`,
written earlier the same day. The page below is kept verbatim: its five
refutations are all still true, and its "where to look" pointed at the right
step. What it could not name is recorded here.

## The cause, in one sentence

CratonVM registers a native over
`sun/util/locale/provider/JRELocaleProviderAdapter.getLocaleServiceProvider`
that answered **`null` for every SPI class**, and `LocaleProviderAdapter.findAdapter`
accepts an adapter only when that call is non-null — so no adapter was ever
accepted, and `getAdapter` fell through to the fallback adapter.

## Why the answer was still correct in three of the four cells

Two variables are stacked, and the page's section 1 table is the projection of
both. The blanket `null` is present in **every** cell — both modes, both images.
It only becomes wrong data when the fallback adapter is also root-only:

| cell | provider lookup | chosen adapter | its `LocaleResources` | answer |
|---|---|---|---|---|
| HotSpot 21 | `DecimalFormatSymbolsProviderImpl` | CLDR | `de_DE` | correct |
| CratonVM 21 `--real-jdk` | `null` | Fallback | ROOT | correct \* |
| CratonVM 21 `--jdk-only` | `null` | Fallback | ROOT | **WRONG** |
| CratonVM 25 `--jdk-only` | `null` | Fallback | `de_DE` | correct |

\* correct only because `--real-jdk` runs a synthetic
`DecimalFormatSymbols.initialize` stub that never walks providers at all.
Strict mode refuses that stub, which is why strict mode is the **only** arm
that consults this native — the defect was equally present in the default mode
and invisible there.

The second variable is a JDK difference, and it is why JDK 25 is spared:

```
JDK 21  FallbackLocaleProviderAdapter   declares getLocaleResources(Locale)
                                        + a private rootLocaleResources field
                                        -> hands back ROOT for ANY locale
JDK 25  FallbackLocaleProviderAdapter   declares NO getLocaleResources
                                        -> inherits the per-locale one
```

Both facts came from `javap -p` on the two images. This is the same species of
finding as the serialization accessor fixed earlier the same day: a VM
behaviour pinned to one JDK's internal class shape.

## What was landed

One arm of the switch the wave-4 comment in `locale_bootstrap.rs` already
prescribed: `java.text.spi.DecimalFormatSymbolsProvider` now delegates to the
receiver's real `getDecimalFormatSymbolsProvider()` bytecode. Every other SPI
class still gets the blanket `null`. That comment's instruction — delegate ONE
SPI at a time, re-run the formatting suites for each, never blanket-delegate on
the strength of reading the switch — is unchanged and still governs the rest.

The native was **not** retired, and no synthetic bridge was removed.

## Measured after the fix

`probes/LocaleSelect.java`, all four cells: the provider lookup answers
`DecimalFormatSymbolsProviderImpl`, `isSupportedLocale(de_DE)` is `true`, the
chosen adapter is `CLDRLocaleProviderAdapter`, and its `LocaleResources` carries
`de_DE`. Separators and `NumberFormat` output in the formerly failing cell are
byte-identical to HotSpot 21 for `de-DE`, `fr-FR` and the `en-US` control. The
`W7-80` warning added earlier that day fired **12 times** in that cell before
and **0** after.

## Two things this does NOT claim

**The corpus cannot see it on Linux.** The `textformat` row exists only in
`jdk-only-strict-corpus-21-windows.txt`; the defect is invisible wherever the
machine default locale is already `en-US`, which is why `21-linux` never froze
it. That row should now read GONE — which the ratchet passes — but re-minting
the Windows baseline needs a Windows JDK 21 image the build host no longer has.
The verification above is therefore the probe, asking for `de-DE` **by name**,
not a corpus arm.

**A sibling SPI is still wrong, and this probe found it.** `Calendar` for
`de-DE` reports `firstDayOfWeek=1, minimalDaysInFirstWeek=1` where HotSpot 21
reports `2` and `4` — the `CalendarDataProvider` arm, still on the blanket
`null`. It is **mode-independent** (it reads the same under `--real-jdk`), so it
is not a strict-mode row, and it is filed separately as
`calendar-week-rules-are-one-region-table-not-a-per-locale-integer-FIXED-20260910.md`
(retired 2026-09-10).
It is the obvious next arm to delegate.

---

*Original page follows, unedited.*

# `--jdk-only` on JDK 21 answers a non-root locale request with ROOT `LocaleResources`, so every locale formats as English

**Status:** open, localised to one step. Not diagnosed further than "adapter /
resource selection", which is where the next person should start.
**Applies to:** `--jdk-only` on **JDK 21 only**. `--real-jdk` on JDK 21 is
correct, and **both** modes on JDK 25 are correct.
**Found:** 2026-09-09, as the residual left after the
`ServiceLoader.loadInstalled` fix
(`serviceloader-loadinstalled-finds-nothing-so-every-platform-loader-service-is-empty-FIXED-20260909.md`)
removed the dominant cause of the same symptom.

## 1. The one wrong cell

```
DecimalFormatSymbols.getInstance(Locale.GERMANY)   decimal   grouping
  HotSpot 21                                       U+002C    U+002E
  CratonVM 21 --real-jdk                           U+002C    U+002E    correct
  CratonVM 21 --jdk-only                           U+002E    U+002C    WRONG (US)
  CratonVM 25 --real-jdk                           U+002C    U+002E    correct
  CratonVM 25 --jdk-only                           U+002C    U+002E    correct
```

`Locale.US` is identical on every arm -- it is the control, and US data is also
what the defect produces, so a run where the control differs is a broken probe.

This is why the `21-windows` strict corpus froze a `textformat` row and
`21-linux` did not: the defect is invisible wherever the machine's default
locale is already `en-US`.

## 2. What it is NOT -- five things, each measured

Every one of these looked like the answer and cost a probe.

**Not the CLDR adapter's supported-locale set.** After the `loadInstalled` fix
it is a full **1063** on JDK 21 -- HotSpot's exact number -- in this cell too.
So the adapter knows `de-DE`.

**Not a missing bundle.** The `W7-80` "no CLDR bundle class in the JDK image"
warning does not fire here.

**Not missing number data.** The `getDecimalFormatSymbolsData` bridge's
en-constant fallback arm is never taken; the lookup SUCCEEDS.

**Not unreachable resource classes.** `sun.text.resources.cldr.ext.FormatData_de`
and `_fr` load from `jdk.localedata` on the platform loader in this cell,
identically to HotSpot.

**Not a broken `Locale` accessor, and not a refused native.**
`Locale.GERMANY.getLanguage()` answers `de` from bytecode in this cell. Asked
from inside the native, `invoke_virtual(loc, "getLanguage")` returns
`Ok(Some(<String>))` and that String is genuinely **empty** -- a real return
value, not the `Ok(None)` that a refused native would hand back.

## 3. What it IS

The `LocaleResources` instance handed to
`sun/util/locale/provider/LocaleResources.getDecimalFormatSymbolsData` is the
**ROOT** one: its `locale` field is a real, non-null `Locale` whose
`getLanguage()` is `""`.

So a request for `de-DE` was answered upstream with root resources. Everything
below that point then behaves correctly on root data and produces English number
symbols -- with nothing missing, nothing throwing, and no fallback arm taken.
That is why five separate probes all reported health.

The VM now says so. `locale_resources.rs`'s `getDecimalFormatSymbolsData` warns
when the receiver yields no language, and reports WHICH fault it is:

```
W7-80: LocaleResources receiver yielded no language ...
    locale_field="getLanguage-returned-empty-string"
```

Measured across the matrix, the warning fires **twice in the failing cell**
(`de-DE` and `fr-FR`) and **zero times** in the three correct cells. Before this
the substitution was completely silent.

## 4. Where to look

Adapter / resource SELECTION, not the bridge and not the CLDR bundles:
whatever hands a `LocaleResources` to `DecimalFormatSymbols.initialize` returns
the root one for a non-root request, in strict mode, on JDK 21.

Note that in `--jdk-only` the two `java/text/DecimalFormatSymbols` natives
(`initialize` and `getInstance`) are `synthetic-stub` and are REFUSED, so the
real JDK bytecode runs and reaches this bridge. In `--real-jdk` the `initialize`
stub runs instead and bypasses the whole provider walk, which is why only strict
mode is affected. That is visible in a `--jdk-only-report`:

```
synthetic-stub native registered: java/text/DecimalFormatSymbols.initialize(Ljava/util/Locale;)V
synthetic-stub native registered: java/text/DecimalFormatSymbols.getInstance(Ljava/util/Locale;)Ljava/text/DecimalFormatSymbols;
bridge-ran-over-bytecode native shadows bytecode of
  sun/util/locale/provider/LocaleResources.getDecimalFormatSymbolsData()[Ljava/lang/Object; [native-won]
```

The same three rows appear on JDK 25, where the answer is CORRECT -- so their
presence is not the defect and retiring them is not obviously the fix.

**A caution about one probe line.** `probes/LocaleAdapter.java` prints
`adapter for de-DE = ...` via a reflective `LocaleProviderAdapter.getAdapter`
call. That line reads `FallbackLocaleProviderAdapter` in ALL FOUR cells,
including the three that are correct, so it does not reflect the path
`DecimalFormatSymbols.getInstance` actually takes and must not be used as the
verdict. Read the separator values and the `availableLocales` count instead.

## 5. Reproducing

```
JAVA_HOME=<jdk21> cratonvm --jdk-only -cp . FmtBundle
```

`probes/FmtBundle.java` prints the separators and the resource-class
reachability together, with `en-US` as a control. The VM's own warning names the
fault.
