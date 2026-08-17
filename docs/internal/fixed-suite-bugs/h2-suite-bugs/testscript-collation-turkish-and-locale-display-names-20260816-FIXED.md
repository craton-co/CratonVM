# ✅ FIXED — `SET COLLATION TURKISH`: the display-name table and the collation rules were both empty, and both are in the JDK image

## Status
**RESOLVED 2026-08-17** on branch `fix/h2-locale-collation-chm-order-20260817`.
`org.h2.test.scripts.TestScript` now reports **0 errors** under CratonVM, the
same as HotSpot JDK 25 on the identical classpath.

Filed originally as: *`SET COLLATION TURKISH` is rejected, because
`Locale.getDisplayLanguage` answers with the language code.*

The original record refused to fix half of this on purpose, and was right to:
"landing the name lookup alone is a regression in behaviour even though it is a
reduction in error count", because H2 would then *accept* the collation and
apply English rules to it. Both halves land here, in one change, verified
against the three `varchar-ignorecase.sql` rows together — the order the
original record recommended.

## What it was

Five of the fifteen `TestScript` errors as filed (nine remained on `dev` when
this was picked up, four of them the separate `ConcurrentHashMap` cluster). One
real failure and four consequences:

```
ERROR: .../datatypes/varchar-ignorecase.sql line 147
  exp: > ok
  got: > exception INVALID_VALUE_2
  Invalid value "TURKISH" for parameter "collation"
```

`org.h2.value.CompareMode.getName` asks every locale `Collator` offers for its
**English display name** and matches that against the requested collation name.
CratonVM answered every such query with the subtag — `tr`, not `Turkish` — so
nothing in the table ever answered `TURKISH` and *every* `SET COLLATION
<language>` was unresolvable.

That was deliberate and documented, in `locale_bootstrap.rs`:

> Return language/country codes directly — good enough for any caller that just
> wants a non-null human-readable string.

True of a caller that prints one. False of a caller that uses one as a **key**,
which is what H2 does — it round-trips a name through the display-name table to
find a locale.

## Root cause, both halves

**Neither half was a missing feature. Both were data already in the JDK image
that nothing was reading.**

* **Display names.** `sun/util/resources/cldr/ext/LocaleNames_*` — 324 bundles
  in a JDK 25 image, `LocaleNames_tr` among them. `load_cldr_table` already knew
  how to reach that package (it is the same mechanism the German month-name fix
  used for `FormatData`); the `Locale` overrides simply never called it.

* **Collation rules.** This one had a false lead worth recording. The obvious
  reading of `phases_late/text_intl.rs` is that `java.text.Collator` is a
  synthetic stub whose `getInstance(Locale)` **discards its locale argument** —
  and it is. But that stub is not what was answering:

  ```
  Collator.getInstance(new Locale("tr")).getClass()
      HotSpot   java.text.RuleBasedCollator
      CratonVM  java.text.RuleBasedCollator     <- the REAL one, already
  ```

  The real JDK provider was running the whole time. What it could not get was
  its **tailoring**: `is_synthesized_locale_base` routes every
  `sun.text.resources.*` base name to a hand-built bundle, and there was no arm
  for `CollationData`, so `LocaleResources.getCollationData()` read `""` and
  `CollatorProviderImpl` built every locale's collator from
  `CollationRules.DEFAULTRULES` alone. The tailorings are in the image too, at
  `sun/text/resources/ext/CollationData_XX` — 47 of them, `_tr`, `_da`, `_sv`
  among them — outside any `cldr` package, which is why `load_cldr_table` could
  not reach them and a sibling loader was needed.

  `probes/RealCollatorProbe` is what settled this: it builds
  `new RuleBasedCollator(DEFAULTRULES + tailoring)` explicitly and gets
  HotSpot's exact answers on CratonVM. The machinery was fine; only its input
  was missing.

## Verification

`probes/LocaleCollationProbe` prints both halves — 15 display languages, 5
country names, H2's own `getName` round-trip, the collation-locale count, and
the comparisons that Turkish, Danish and Swedish collation are *for*. Against
HotSpot JDK 25 it is now **byte-identical**, where before every row diverged:

| row | before | after / HotSpot |
|---|---|---|
| `displayLanguage(tr, ENGLISH)` | `tr` | `Turkish` |
| `h2Name(en_US)` | `EN_US` | `ENGLISH_UNITED_STATES` |
| collation locales with an English name | 0 / 166 | 165 / 166 |
| H2 resolves `TURKISH` to | *(nothing)* | `tr` |
| `compare[tr](I, i)` | `1` | `-1` |
| `compare[tr](İ, I)` | `-1` | `1` |
| `compare[da](æ, z)` | `-1` | `1` |
| `tr` PRIMARY `I == i` | `true` | `false` |
| `en` PRIMARY `I == i` | `true` | `true` |

Note the last two: the English control has to keep answering `true`, and does.
A collator that made everything case-sensitive would have "fixed" the Turkish
rows and broken `VARCHAR_IGNORECASE` everywhere else.

End to end, `TestScript` on the same classpath, runs serialized so no arm holds
the database file another needs:

| | result | time |
|---|---|---|
| HotSpot JDK 25 | 0 errors | 9s |
| CratonVM, `dev` | **9 errors** | 1083s |
| CratonVM, fixed | **0 errors** | 993s |

Slightly *faster* than the baseline, not slower — worth stating because the
first build of this fix was 1204s: reading a tailoring instantiates the bundle
class and walks its `getContents()`, and the sibling loader was missing the
cache `load_cldr_table` has. Callers that build a collator per comparison are
common enough that the difference is 20%.

## A measurement trap this hit twice

An early "baseline" reading of 99s with 4 errors was **an aborted run**, not a
fast one: a `TestScript` process left over from a previous arm still held
`data/test/script.mv.db`, the next arm died on the file lock in seconds, and
`grep -c '^ERROR'` on a log that stops early returns a small number that looks
like good news. The real baseline is 1083s with 9 errors. Every arm above is
from a driver that waits for the previous `TestScript` to exit and deletes the
data directory before starting the next.

## The transferable part

**"Good enough for any caller that just wants X" is a claim about callers, and
callers are not surveyed.** The comment on the display-name overrides was
accurate about the callers its author had in mind and wrong about the one that
mattered. A stub that returns a plausible value silently redefines what the
method means for everyone who uses it differently.

**Check what class is actually answering before believing a stub is the
problem.** The synthetic `Collator` reads exactly like the cause of this bug —
its `getInstance(Locale)` really does throw the locale away — and it was not
running. One `getClass().getName()` would have saved the detour, and is the same
lesson as the MXBean overlay finding: print what the object IS before theorising
about what it does.
