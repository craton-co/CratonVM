# BUG-L — `ResourceBundle.getBundle` ignored the locale + `Locale` getters returned "" for JDK constants

**Test:** `org.apache.tomcat.util.res.TestStringManager` (`testFrench`,
`testMissingNullTccl`, `testMissingWithTccl`, `testVersionLoggerListenerAlignment`).
HotSpot: PASS. **Status: FIXED.**

## Symptoms

`StringManager.getManager(pkg, Locale.FRENCH).getLocale()` returned `en` instead
of `fr`; `StringManager.getManager("org.does.not.exist").getLocale()` returned
`en` instead of `null`; and once the real locale content *was* loaded, Spanish
messages mis-aligned (`ú`/`ó` left undecoded).

## Root causes (three, all in the resource-bundle / locale natives)

1. **`Locale.FRENCH.getLanguage()` returned `""`** — the `getLanguage`/
   `getCountry`/`getScript`/`getVariant` natives (`locale_bootstrap.rs`) only
   consulted CratonVM's synthetic side table and returned `""` for any other
   Locale, including the JDK's predefined constants (built by real `<clinit>`
   bytecode). **Fix:** fall back to reading the real
   `sun.util.locale.BaseLocale` (`language`/`region`/`script`/`variant`).
2. **`getBundle` ignored the locale and never threw CNFE** — `rb_get_bundle`
   (`locale_resources.rs`) always built a ROOT/English bundle and always
   returned non-null. **Fix:** read the requested locale (via
   `getLanguage`/`getCountry`), merge the resource chain
   `ROOT → _lang → _lang_country` into one map (flattening the parent-chain
   fallback so inherited keys still resolve), tag the bundle with the
   most-specific matched locale, and throw `MissingResourceException` for a
   genuinely-absent app bundle (JDK-internal `sun.`/`jdk.`/`java.` base names
   keep the synthesized/empty fallback). The `getLocale` native also returned a
   constant empty Locale; it now returns the bundle's own `locale` field.
3. **`.properties` escapes were not decoded** — the parser kept `\uXXXX`
   literal (6 chars), throwing off `testVersionLoggerListenerAlignment`'s width
   math (45 vs 35). **Fix:** decode `\uXXXX`, `\t`/`\n`/`\r`/`\f`, `\<char>`,
   and trailing-backslash line continuation; keep significant internal/trailing
   value whitespace.

Verified: `TestStringManager` 6/6. The `getLanguage` fix is general (any
`Locale.FRENCH.getLanguage()` caller).
