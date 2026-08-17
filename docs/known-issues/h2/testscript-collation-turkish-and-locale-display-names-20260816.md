# `SET COLLATION TURKISH` is rejected, because `Locale.getDisplayLanguage` answers with the language code

## Status
**OPEN, root-caused, not fixed (2026-08-16).** 5 of the 15
`org.h2.test.scripts.TestScript` errors on `dev` @ `496bc3c2c` — one real
failure and four consequences of it. HotSpot JDK 25 on the same classpath
reports 0 errors. Part of the census in
[`testscript-sql-divergences-20260816.md`](testscript-sql-divergences-20260816.md).

Deliberately not fixed: the obvious cheap fix closes one of the five errors and
makes the other four *worse* rather than better. See "Fixing it".

Same family as
`fixed-suite-bugs/h2-suite-bugs/bug-h2-testscript-parsedatetime-german-locale-month-name-FIXED-20260816.md`
— that one was CratonVM answering `java.time`'s calendar-field-name queries for
English only; this one is CratonVM answering `Locale`'s display-name queries for
no language at all. That record names these failures as out of its scope; this
is the record it was pointing at.

## The failure

```
ERROR: org/h2/test/scripts/datatypes/varchar-ignorecase.sql
line: 147
exp: > ok
got: > exception INVALID_VALUE_2
------------------------------
ERROR: script org.h2.jdbc.JdbcSQLDataException: Invalid value "TURKISH" for parameter "collation"; SQL statement:
SET COLLATION TURKISH STRENGTH IDENTICAL [90008-249]
	at org.h2.command.Parser.parseSetCollation(Parser.java:7746)
```

and, because the collation never took, the three statements the rest of the
file builds on top of it:

```
line: 153   exp: > update count: 2          got: > exception DUPLICATE_KEY_1
              (INSERT INTO TEST VALUES 'I', 'i' — distinct under Turkish rules)
line: 156   exp: > exception DUPLICATE_KEY_1  got: > update count: 1
              (INSERT INTO TEST VALUES CHAR(0x0130) — dotted capital I, equal to 'I' under Turkish rules)
```

plus `TestScript`'s separate stack-trace record for the `DUPLICATE_KEY_1` at
line 153. Five error rows, one cause.

## What it is

`org.h2.value.CompareMode` resolves a collation name by asking every locale
`Collator` offers what its **English display name** is:

```java
public static String getName(Locale l) {                       // CompareMode.java:173
    Locale english = Locale.ENGLISH;
    String name = l.getDisplayLanguage(english) + ' ' + l.getDisplayCountry(english) + ' ' + l.getVariant();
    return StringUtils.toUpperEnglish(name.trim().replace(' ', '_'));
}
...
for (Locale locale : getCollationLocales(false))               // = Collator.getAvailableLocales()
    if (compareLocaleNames(locale, name)) { result = Collator.getInstance(locale); break; }
```

so `TURKISH` matches `new Locale("tr")` only if
`new Locale("tr").getDisplayLanguage(Locale.ENGLISH)` returns `Turkish`.

CratonVM returns `tr`. Not only for Turkish — for everything:

```
                                    HotSpot        CratonVM
tr.getDisplayLanguage(ENGLISH)      Turkish        tr
de.getDisplayLanguage(ENGLISH)      German         de
en.getDisplayLanguage(ENGLISH)      English        en
H2 CompareMode "TURKISH" resolves   tr             null
collation locales with a real
  English display name              165 / 166      0 / 166
```

This is deliberate and documented in the source. `native-builtins/src/locale_bootstrap.rs`
registers overrides for `Locale.getDisplayName`, `getDisplayLanguage` and
`getDisplayCountry` (both the no-arg and the `(Locale)` overloads), each of
which just returns the language/country code:

```rust
// Display-name overrides — the JDK's real implementations consult
// `sun.util.resources.cldr.LocaleNames` resource bundles that we cannot
// load (no CLDR data, no ServiceLoader for the resource-bundle
// providers). Return language/country codes directly — good enough
// for any caller that just wants a non-null human-readable string.
```

"Good enough for any caller that just wants a non-null human-readable string"
is the assumption that fails here: H2 does not want a human-readable string, it
wants a *key*, and it round-trips a name through the display-name table to find
a locale. The comment has been left as-is and this record is the counterexample.

`Collator.getAvailableLocales()` itself is fine — 166 entries on both VMs, and
`tr` is among them on both. (`Locale.getAvailableLocales()` is not: 151 on
CratonVM vs 1158 on HotSpot. That is a separate, larger gap and no test here
depends on it.)

## The second half, which the first half hides

Even with the name resolved, rows 9 and 11 above would not pass. Turkish
collation is not just a name lookup — it is the rule that dotless `ı` sorts with
`i` and dotted `İ` with `I`. Measured:

```
Collator.getInstance(new Locale("tr")).setStrength(IDENTICAL); compare("I", "i")
    HotSpot   -1
    CratonVM   1
```

So CratonVM's `Collator` for `tr` is not a Turkish collator; it is whatever the
default collator is, answering with the opposite sign. Fixing the name lookup
alone would make H2 *accept* `SET COLLATION TURKISH` and then apply non-Turkish
rules to it.

## Blast radius

Any caller that treats a display name as a lookup key, and any caller that shows
one to a user. Concretely: `SET COLLATION <language>` in H2 for every language
(not just Turkish — `getName` is the only path H2 has), JDBC/ODBC collation
metadata, and anything formatting a locale for display. Callers that only need
*a* non-empty string are genuinely unaffected, which is why this has not
surfaced before.

## Fixing it

Not attempted here, on purpose. The three options:

* **Route the display-name overrides through the existing synthetic English
  table.** `native-builtins/src/phases_late/text_intl.rs`'s
  `populate_locale_names_en` already carries 15 languages and 18 countries —
  including `("tr", "Turkish")`. Wiring `getDisplayLanguage(Locale)` to consult
  it when the display locale is English would be perhaps 30 lines and would fix
  error #7 above. **It would also flip errors #9 and #11 from "collation
  rejected" to "collation silently applied with the wrong rules"** — H2 would
  accept `SET COLLATION TURKISH` and then sort Turkish text as if it were
  English. A loud rejection is better than a quiet wrong answer, so this is not
  an improvement on its own; it is only worth doing together with the next item.
* **Give `Collator` real per-locale rules.** This is what rows 9 and 11 actually
  need, and it is the real work: CLDR collation tailorings, or at minimum the
  handful of locale-specific rules (Turkish dotted/dotless I, Scandinavian
  æ/ø/å, Spanish ch/ll) that the JDK ships.
* **Read the JDK image's own `LocaleNames` bundles.** The
  `bug-h2-testscript-parsedatetime-...` fix showed the CLDR reader can already
  walk `sun.text.resources.cldr.ext.FormatData_*` out of the JDK image; the
  `LocaleNames_*` bundles live in the same place. This would fix the display
  names properly for all languages rather than the 15 in the synthetic table,
  and is the right first half of the pair.

Recommended order: bundles-or-table **and** collator tailorings, in one change,
verified against the three `varchar-ignorecase.sql` rows together. Landing the
name lookup alone is a regression in behaviour even though it is a reduction in
error count.

## Repro

The display-name gap on its own:

```java
System.out.println(new Locale("tr").getDisplayLanguage(Locale.ENGLISH));  // HotSpot: Turkish, CratonVM: tr
Collator c = Collator.getInstance(new Locale("tr"));
c.setStrength(Collator.IDENTICAL);
System.out.println(c.compare("I", "i"));                                  // HotSpot: -1, CratonVM: 1
```

```bash
javac -d /tmp/classes TrProbe.java
java -cp /tmp/classes TrProbe
<cratonvm-bin> --java-home /data/toolchain/jdk-25 --nojit -c /tmp/classes TrProbe
```

In situ, from `apps/h2database/h2` (see the census record for the full command):

```bash
<cratonvm-bin> --java-home /data/toolchain/jdk-25 --nojit --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.scripts.TestScript
```
