# The CLDR locale adapter sees 5 locales instead of 1,063, so every non-English locale silently answers US data

**Status:** open, mechanism identified 2026-09-09.
**Applies to:** JDK 21 AND JDK 25, `--real-jdk` AND `--jdk-only`, **measured on
the Linux host**. Not version-dependent there. Whether it is mode-dependent is
NOT settled -- an earlier Windows measurement disagrees; see section 4a, which
is the first thing to resolve.
**Found:** narrowing the `21-windows` strict-corpus `textformat` row, which had
been narrowed on 2026-09-08 to "locale DATA, not locale selection" and left
there.

## 1. The measurement

`DecimalFormatSymbols.getInstance(Locale.GERMANY)` -- asked for **by name**, so
locale *selection* is not the variable -- answers US separators:

```
                        decimal   grouping
HotSpot 21   de-DE      U+002C    U+002E     <- comma / period, correct
CratonVM 21  de-DE      U+002E    U+002C     <- US
CratonVM 21  fr-FR      U+002E    U+002C     <- US
both         en-US      U+002E    U+002C     <- control, identical on every arm
```

The provider chain says why:

```
                                adapter chosen for de-DE
HotSpot  21   sun.util.cldr.CLDRLocaleProviderAdapter
HotSpot  25   sun.util.cldr.CLDRLocaleProviderAdapter
CratonVM 21   --real-jdk   sun.util.locale.provider.FallbackLocaleProviderAdapter
CratonVM 21   --jdk-only   sun.util.locale.provider.FallbackLocaleProviderAdapter
CratonVM 25   --real-jdk   sun.util.locale.provider.FallbackLocaleProviderAdapter
CratonVM 25   --jdk-only   sun.util.locale.provider.FallbackLocaleProviderAdapter
```

`FallbackLocaleProviderAdapter` carries root/English data only, which is exactly
why every locale comes back with US separators.

## 2. It is NOT the adapter failing to load

The obvious theory is that the CLDR adapter throws during construction and
`LocaleProviderAdapter.forType` silently substitutes the fallback. Tested:
constructing `sun.util.cldr.CLDRLocaleProviderAdapter` directly **succeeds** on
CratonVM, and `forType` returns a real instance for every `Type`:

```
CratonVM: forType(JRE)=JRELocaleProviderAdapter  forType(CLDR)=CLDRLocaleProviderAdapter
          forType(SPI)=SPILocaleProviderAdapter  forType(HOST)=HostLocaleProviderAdapter
adapterPreference = [CLDR, JRE]      <- identical to HotSpot
```

So the adapter exists, constructs, and is preferred. No exception is thrown
anywhere, which is why nothing ever appeared in a stack trace.

## 3. The mechanism: its available-locale set is 5 entries

`LocaleProviderAdapter.getAdapter(spi, locale)` picks an adapter by asking each
one whether it SUPPORTS the locale. CratonVM's CLDR adapter claims almost none:

```
                          availableLocales   contains a `de` locale
HotSpot 21                1063               true
CratonVM 21 --jdk-only    5                  false
```

and the five are:

```
[]  [en]  [en_US]  [en_US_#Latn]  [en_US_POSIX]
getLanguageTagSet(AvailableLocales) = [en, , en-Latn-US, en-US-POSIX, en-US]
getLanguageTagSet(FormatData)       = [en, , en-Latn-US, en-US-POSIX, en-US]
```

**Those five are precisely the set that ships inside `java.base`.** The other
~1,058 live in the **`jdk.localedata`** module, whose
`CLDRLocaleDataMetaInfo` supplements the base one. CratonVM is finding
`java.base`'s base metadata and not `jdk.localedata`'s supplement, so the CLDR
adapter honestly reports that it supports only English, `getAdapter` believes
it, and de-DE falls through to `FALLBACK`.

Nothing here is a wrong ANSWER by any component. Every layer behaves correctly
given its inputs; the input -- the locale-data module -- is simply absent.

## 4a. An unresolved conflict with the earlier Windows measurement -- READ THIS FIRST

Everything above is measured on the Linux host, where the CLDR adapter is
bypassed in BOTH modes. The 2026-09-08 Windows narrowing recorded something
different for the same call:

```
                             grouping   decimal
  HotSpot 21                 U+002E     U+002C
  CratonVM --real-jdk  21    U+002E     U+002C   <- CORRECT on Windows
  CratonVM --jdk-only  21    U+002C     U+002E   <- the one wrong cell
```

On Windows `--real-jdk` answered German data correctly and only `--jdk-only` was
wrong. On Linux BOTH modes answer US. Those cannot both be the whole story, and
I have not reconciled them. Candidate explanations, none tested:

* Windows has a real `HostLocaleProviderAdapter` backed by the OS, which Linux
  does not; it may be claiming `de-DE` on the `--real-jdk` arm and masking the
  same underlying `jdk.localedata` gap.
* The two measurements used different builds, on different dates, against
  different JDK 21 images (Temurin 21.0.12.1+1 on Windows, 21.0.12+8 on Linux).
* `--jdk-only` may additionally suppress something `--real-jdk` leaves working,
  making mode a second, independent variable on top of the module gap.

**Do not quote this page as "mode-independent" until that is settled.** The
mechanism in section 3 -- the CLDR adapter honestly reporting 5 supported
locales -- is measured and solid; the SCOPE of it across modes and platforms is
one host wide.

## 4. Why the corpus saw this on Windows and not on Linux

The `21-windows` baseline froze a `textformat` row; `21-linux` did not, and the
21-linux baseline note says so ("that row is locale data and this is a different
host"). This page explains the asymmetry: the defect is invisible wherever the
machine's default locale is already `en-US`, because then the fallback's US data
IS the right answer. The Linux host runs `en_US`; the Windows host did not.

**So the row is not JDK-21-specific**, and the mechanism is present on Linux
too even though no corpus leg there records it. Whether the WINDOWS row has
exactly this cause is subject to the conflict in section 4a. What is safe to
say: reading the frozen row as "a Windows locale quirk" is wrong, because the
same adapter bypass is measurable on Linux where no row exists.

## 5. Reproducing

`probes/LocaleAdapter.java` (chain), `probes/CldrLocales.java` (the count),
`probes/CldrWhich.java` (which five). All three need
`--add-exports java.base/sun.util.locale.provider=ALL-UNNAMED
 --add-exports java.base/sun.util.cldr=ALL-UNNAMED` plus the matching
`--add-opens`.

The one-liner, if you only want the verdict:

```
cratonvm --add-exports java.base/sun.util.locale.provider=ALL-UNNAMED -cp . LocaleAdapter
# HotSpot : adapter for de-DE = sun.util.cldr.CLDRLocaleProviderAdapter
# CratonVM: adapter for de-DE = sun.util.locale.provider.FallbackLocaleProviderAdapter
```

Keep `en-US` in any probe as a control: it prints the SAME values on a healthy
and a broken VM, so a run where the control differs is a broken probe.

## 6. Where to look

Not in `java.text`. The question is how CratonVM enumerates the resources /
service providers of the `jdk.localedata` module -- `CLDRBaseLocaleDataMetaInfo`
(java.base) is being found and `CLDRLocaleDataMetaInfo` (jdk.localedata) is not.
Check first whether `jdk.localedata` is in the resolved module graph at all, and
then whether its `LocaleDataMetaInfo` service provider is visible to
`ServiceLoader` from `java.base`.

Related: `docs/known-issues/jdk-only/the-first-jdk-21-run-found-the-javalangaccess-carrier-is-pinned-to-system-1-20260908.md`
finding #4, which this page supersedes and completes.
