import java.util.Locale;

/**
 * `Locale`'s BCP-47 round-trip: script subtags, extensions, and the `und`
 * language, across `forLanguageTag` / `toString` / `toLanguageTag` /
 * `stripExtensions` / `getExtension`.
 *
 * CratonVM used to override `Locale.forLanguageTag` with a hand-rolled subtag
 * split whose side table had no slot for a script, so `zh-hant-CN` came back as
 * plain `zh_CN` — Tomcat's `TestAcceptLanguage.bug56848`, "expected:
 * <zh_CN_#Hant> but was:<zh_CN>". The same parser filed a BCP-47 extension
 * singleton as a variant (`en-US-u-ca-japanese` -> variant "u") and treated
 * `und` as a language (`und-DE` -> `und_DE`, not `_DE`).
 *
 * Every expectation below is HotSpot's own answer (JDK 25). Prints one line per
 * check plus a summary; `bad=0` is a pass. Run it under HotSpot too — it must
 * print `bad=0` there by construction, so a failure there means the expectation
 * table has drifted from the JDK, not that CratonVM is wrong.
 */
public class LocaleScriptProbe {

    private static int bad = 0;

    private static void eq(String what, Object actual, Object expected) {
        boolean ok = expected == null ? actual == null : expected.equals(actual);
        if (!ok) {
            bad++;
            System.out.println("BAD  " + what + ": expected <" + expected + "> but was <" + actual + ">");
        } else {
            System.out.println("ok   " + what + " = " + actual);
        }
    }

    /** `toString | toLanguageTag | language | script | country | variant`. */
    private static void check(String tag, String expected) {
        Locale l = Locale.forLanguageTag(tag);
        String actual = l.toString() + " | " + l.toLanguageTag() + " | " + l.getLanguage()
                + " | " + l.getScript() + " | " + l.getCountry() + " | " + l.getVariant();
        eq("forLanguageTag(" + tag + ")", actual, expected);
    }

    public static void main(String[] args) {
        // -- script subtags ------------------------------------------------
        check("zh-hant-CN", "zh_CN_#Hant | zh-Hant-CN | zh | Hant | CN | ");
        check("zh-hans-TW", "zh_TW_#Hans | zh-Hans-TW | zh | Hans | TW | ");
        check("zh-Hant", "zh__#Hant | zh-Hant | zh | Hant |  | ");
        check("az-Cyrl", "az__#Cyrl | az-Cyrl | az | Cyrl |  | ");
        check("sr-Latn-RS", "sr_RS_#Latn | sr-Latn-RS | sr | Latn | RS | ");

        // -- extensions ----------------------------------------------------
        check("zh-Hant-TW-x-java", "zh_TW_#Hant_x-java | zh-Hant-TW-x-java | zh | Hant | TW | ");
        check("en-US-u-ca-japanese", "en_US_#u-ca-japanese | en-US-u-ca-japanese | en |  | US | ");
        check("th-TH-u-nu-thai", "th_TH_#u-nu-thai | th-TH-u-nu-thai | th |  | TH | ");

        // -- `und`, variants, and the plain cases --------------------------
        check("und-DE", "_DE | und-DE |  |  | DE | ");
        check("de-DE-1996", "de_DE_1996 | de-DE-1996 | de |  | DE | 1996");
        check("en", "en | en | en |  |  | ");
        check("en-gb", "en_GB | en-GB | en |  | GB | ");

        // -- equality with the Locale.Builder route (bug56848's assertion) --
        Locale.Builder b = new Locale.Builder();
        Locale l1 = b.setLanguage("zh").setRegion("CN").setScript("hant").build();
        Locale l2 = b.clear().setLanguage("zh").setRegion("TW").setScript("hans").build();
        eq("builder(zh,CN,hant).toString", l1.toString(), "zh_CN_#Hant");
        eq("forLanguageTag(zh-hant-CN).equals(builder l1)",
                Locale.forLanguageTag("zh-hant-CN").equals(l1), Boolean.TRUE);
        eq("forLanguageTag(zh-hans-TW).equals(builder l2)",
                Locale.forLanguageTag("zh-hans-TW").equals(l2), Boolean.TRUE);
        eq("hashCode agrees with builder l1",
                Locale.forLanguageTag("zh-hant-CN").hashCode() == l1.hashCode(), Boolean.TRUE);
        eq("Builder.setLocale(forLanguageTag(zh-hant-CN))",
                new Locale.Builder().setLocale(Locale.forLanguageTag("zh-hant-CN")).build().toString(),
                "zh_CN_#Hant");

        // -- stripExtensions / getExtension --------------------------------
        Locale ext = Locale.forLanguageTag("zh-Hant-TW-x-java");
        eq("stripExtensions().toString", ext.stripExtensions().toString(), "zh_TW_#Hant");
        eq("stripExtensions().toLanguageTag", ext.stripExtensions().toLanguageTag(), "zh-Hant-TW");
        eq("getExtension('x')", ext.getExtension('x'), "java");

        // -- constructor-built locales keep no script ----------------------
        eq("new Locale(de,DE,POSIX)", new Locale("de", "DE", "POSIX").toString(), "de_DE_POSIX");
        eq("new Locale(\"\",DE)", new Locale("", "DE").toString(), "_DE");
        eq("Locale.ROOT", Locale.ROOT.toString(), "");
        eq("Locale.TRADITIONAL_CHINESE", Locale.TRADITIONAL_CHINESE.toString(), "zh_TW");
        eq("Locale.SIMPLIFIED_CHINESE", Locale.SIMPLIFIED_CHINESE.toString(), "zh_CN");

        System.out.println("bad=" + bad);
        if (bad != 0) {
            System.exit(1);
        }
    }
}
