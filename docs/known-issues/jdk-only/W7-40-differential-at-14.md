# W7-40 — the differential at 14, and two instrument holes

**Status: MEASURED 2026-08-12** on the 50-branch binary. HotSpot 25.0.3.9 vs CratonVM
`--real-jdk`, same class files, both sides pinned to UTF-8 / en-US.

**96 → 43 → 14 divergent observables. 0 sections died** (was 2).

## The 14

```diff
< stream.reuseThrows=java.lang.IllegalStateException
> stream.reuseThrows=no-throw
< ArrayDeque.addNull=java.lang.NullPointerException
< ArrayDeque.addFirstNull=java.lang.NullPointerException
< ArrayDeque.offerNull=java.lang.NullPointerException
< ArrayDeque.sizeAfterRefusedNulls=0
< Enum.valueOfBadName=java.lang.IllegalArgumentException: No enum constant ShadowDifferentialProbe.Color.MAUVE
> Enum.valueOfBadName=java.lang.IllegalArgumentException: No enum constant MAUVE
> [SUREFIRE-NPE] Name is null thrown; top Java frames:
> [SUREFIRE-NPE]   #0 ShadowDifferentialProbe.main (ShadowDifferentialProbe.java:237)
> [SUREFIRE-NPE]   #1 ShadowDifferentialProbe.section (ShadowDifferentialProbe.java:2131)
> [SUREFIRE-NPE]   #2 ShadowDifferentialProbe.enumCollections (ShadowDifferentialProbe.java:1244)
> [SUREFIRE-NPE]   #3 ShadowDifferentialProbe.thrownBy (ShadowDifferentialProbe.java:2140)
> [SUREFIRE-NPE]   #4 ShadowDifferentialProbe.lambda$enumCollections$2 (ShadowDifferentialProbe.java:1244)
> [SUREFIRE-NPE]   #5 ShadowDifferentialProbe$Color.valueOf (ShadowDifferentialProbe.java:1289)
< COW.addAllAbsent=1:[a, b, zz, qq]
< format.unknownConversion=java.util.UnknownFormatConversionException: Conversion = 'q'
< format.missingArgument=java.util.MissingFormatArgumentException
< format.wrongArgumentType=java.util.IllegalFormatConversionException
< format.illegalFlagCombination=java.util.IllegalFormatFlagsException
< format.precisionOnInteger=java.util.IllegalFormatPrecisionException
> format.unknownConversion=java.lang.IllegalArgumentException: Conversion = 'q'
> format.missingArgument=java.lang.IllegalArgumentException
> format.wrongArgumentType=java.lang.IllegalArgumentException
> format.illegalFlagCombination=java.lang.IllegalArgumentException
> format.precisionOnInteger=java.lang.IllegalArgumentException
< NumberFormat.currencyNegativeUS=-$1,234.50
> NumberFormat.currencyNegativeUS=($1,234.50)
< Random.nextGaussian=1.1419053154730547
> Random.nextGaussian=1.141905315473055
```

## Two of these are instrument holes, not value divergences

**1. Four observables vanish with no `SECTION-DIED`.** `ArrayDeque.addNull`, `addFirstNull`,
`offerNull` and `sizeAfterRefusedNulls` appear on HotSpot at lines 481-484 and are **absent**
from CratonVM's transcript — while the same section runs to completion on both sides
(`toArray`, `clearThenIsEmpty`, `growsPastInitialCapacity`, `getFirstOnEmpty`, `popOnEmpty` all
present). `COW.addAllAbsent` is missing the same way. Verified against the raw, unfiltered
output, so it is not the `grep -v` that drops them.

The per-section fence exists precisely so that a failure prints one `SECTION-DIED` line instead
of removing every line after it. **Here lines are lost while the fence stays silent and the
section finishes**, which is worse than the case the fence was built for: a truncated transcript
at least ends. This is the campaign's blind-instrument species inside the instrument.
Find out whether the probe has a conditional path or output is being lost, before reading any
of those four rows as a defect.

**2. `[SUREFIRE-NPE]` diagnostic frames leak into stdout** — 7 lines, from `enumCollections`.
A transcript meant for diffing must carry only observables; VM diagnostics belong on stderr or
behind a flag.

## The genuine value divergences

| observable | HotSpot | CratonVM | note |
|---|---|---|---|
| `stream.reuseThrows` | `IllegalStateException` | `no-throw` | **declined deliberately** — needs a `linkedOrConsumed` flag in a funnel with 99 call sites in one file; a flag set once too often turns a working stream into a throw on the most pervasive path in the SB/Tomcat arms |
| `format.*` ×5 | `UnknownFormatConversionException`, `MissingFormatArgumentException`, `IllegalFormatConversionException`, `IllegalFormatFlagsException`, `IllegalFormatPrecisionException` | `java.lang.IllegalArgumentException` | the refusals landed; the **subclass** does not. All five extend `IllegalArgumentException`, so we raise the base. A mistyped refusal sends a caller down the wrong `catch` |
| `Enum.valueOfBadName` | `No enum constant ShadowDifferentialProbe.Color.MAUVE` | `No enum constant MAUVE` | message omits the qualified type |
| `NumberFormat.currencyNegativeUS` | `-$1,234.50` | `($1,234.50)` | US locale renders negative currency with a minus, not parentheses |
| `Random.nextGaussian` | `1.1419053154730547` | `1.141905315473055` | 16 vs 17 significant digits — a `Double.toString` shortest-representation difference, not a different number |
