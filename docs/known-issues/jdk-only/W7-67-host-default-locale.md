# W7-67 — the default locale was hardcoded `en_US`, and `Locale.Category` was collapsed

**RETIRE-FIXED, with one residual. Source landed 2026-08-12; not re-run.**
Re-adjudicated 2026-08-12 against the tree. Both halves of the headline are in
place and the staged work this record scheduled has since been done by other
records, so what is left of it is a single unmodelled case — stated in §3 and
nowhere else.

## 1. What it was, and what fixed it

`vm_init::derive_locale()` read `$LC_ALL` then `$LANG` and nothing else. Windows
sets neither, so it fell through to its final line — `("en", "US")` — on **every
Windows host, unconditionally**. The doc comment said so out loud, which is how
a hardcoded value survives review: it was documented as a fallback and was in
fact the only outcome. On this ru_RU host, `Locale.getDefault()` answered
`en_US` against HotSpot's `ru_RU`, and `user.script`/`user.variant` were absent
where HotSpot publishes all four keys of the family, empty string included.

The fix mirrors `java_props_md.c`: `GetUserDefaultUILanguage()` for the DISPLAY
category and `GetUserDefaultLocaleName()` for FORMAT on Windows, `LC_MESSAGES`
and `LC_CTYPE` on Unix (the old `LC_ALL` ▸ `LANG` precedence skipped the
category variables, so `LANG=C LC_CTYPE=de_DE.UTF-8` reported `en_US` here and
`de_DE` on HotSpot). Property publication follows
`jdk.internal.util.SystemProps.fillI18nProps`, including the three parts of it
that are load-bearing and not obvious: a command-line `-Duser.<base>` returns
early and therefore **suppresses the derived overlay** (without which
`-Duser.language=en -Duser.country=US` on this host would leave
`user.language.format=ru` behind — the regression-suite's own pinning
mechanism); the base property takes the DISPLAY value; and `.display` is never
created from platform values, because the JDK writes it only when it differs
from the base and the base has just been set *from* it.

`Locale.getDefault(Locale$Category)` was a separate defect that had to move with
it — it was registered as `|ctx, _args| get_or_create_default(ctx)`, discarding
its argument, so all three defaults were one cached `ObjectRef` and
`getDefault(FORMAT)` could not disagree with `getDefault()` under any
configuration. Three cache slots replace the one;
`gc_scan_locale_roots`/`gc_update_locale_refs` iterate all three (a slot missed
in the root scan is a `Locale` a moving young collection reclaims while the
cache keeps handing back its address), `setDefault(Locale)` writes all three and
`setDefault(Category, Locale)` writes only the named one. Measured on HotSpot
with `-Duser.language.format=de` on this host: `getDefault(FORMAT)` is `de_RU`,
not `de_DE` — `StaticProperty` defaults each key independently to its base — and
the implementation copies that per-key fallback.

Both halves are HotSpot-parity fixes, so they are landed for Compatible mode as
well as `--jdk-only`.

## 2. What this record staged, and who did it

Its §4 argued that reporting the true locale over en-only data was a net
improvement — the decisive row being that locale-sensitive case mapping was
*already* correct and merely being fed the wrong locale, so a Turkish host got
`"i".toUpperCase()` wrong on every identifier, hostname and SQL keyword — and it
listed the surfaces that would stay inconsistent, in a staged order.

Stage 2 and stage 3 are **done**, by `W7-80-locale-data-stage-two.md`: number
patterns, `DecimalFormatSymbols`, the currency symbol and code, the date/time
patterns and `DateFormatSymbols` all read the JDK image's own CLDR data per
locale now. The `$1.234,50` chimera this record accepted as the price of landing
does not exist any more. Its `%t`/`%T` half — the one surface CLDR data does not
reach, because `String.format` never asked `DateFormatSymbols` — is
`W7-91-format-date-symbols-hardcoded-english.md`.

Two expectations this record set for the suite are superseded and should not be
worked from. Its §6 predicted `RJdkLogging` would stay at `streamBytes=175`
against HotSpot's `177`; measured on 2026-08-12, HotSpot is **179**, and the
four-character gap is two separate one-character defects, neither of them this
one — see W7-91 §1. What still holds from §6: pinned runs are byte-identical
before and after, Linux CI is unaffected (`LANG=C`/unset still maps to `en_US`),
and no test was weakened.

## 3. The residual

**A category whose *script* or *variant* differs from the base is not
modelled.** The synthetic `Locale` this VM allocates records `(language,
country, tag)` only — `locale_bootstrap::synthetic_locale_data`, and
`resolve_default_locale_for` returns a `(String, String)` pair — so widening it
means widening that side table. No host we run on splits those two subtags
across categories. The constraint is recorded at
`resolve_default_locale_for`'s own doc comment, which cites this record.

That is the whole of what is open here.
