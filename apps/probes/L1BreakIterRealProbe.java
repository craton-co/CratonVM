import java.text.BreakIterator;
import java.text.spi.BreakIteratorProvider;
import java.util.ArrayList;
import java.util.List;
import java.util.Locale;
import sun.util.locale.provider.LocaleProviderAdapter;

/** L1 section 10 item 5 -- `java/text/BreakIterator`'s LAST MILE, measured on
 *  the provider chain rather than on the pinned public factories.
 *
 *  Wave 5 answered the two `LocaleResources` readers
 *  (`getBreakIteratorInfo` / `getBreakIteratorResources`) out of the jimage,
 *  and then could not claim the family, because nothing had ever run the rest
 *  of the chain: `new sun.text.RuleBasedBreakIterator(name, bytes)` over the
 *  `*BreakIteratorData` blob those readers now hand back. This probe is that
 *  measurement.
 *
 *  IT DOES NOT GO THROUGH `BreakIterator.getWordInstance` for the P rows.
 *  Those four static factories are pinned to this VM's synthetic natives by
 *  the BREAKITER allow-list in `vm/src/vm/vm_exec.rs`, so calling them
 *  measures the pin. The provider is reachable without them and is not
 *  pinned:
 *
 *    LocaleProviderAdapter.forJRE().getBreakIteratorProvider()
 *        -> BreakIteratorProviderImpl.getWordInstance(Locale)
 *        -> getBreakInstance(locale, 0, "WordData", "WordDictionary")
 *        -> LocaleResources.getBreakIteratorInfo("BreakIteratorClasses")
 *           LocaleResources.getBreakIteratorInfo("WordData")
 *           LocaleResources.getBreakIteratorResources("WordData")
 *        -> new sun.text.RuleBasedBreakIterator(String, byte[])
 *
 *  read out of `javap -c sun.util.locale.provider.BreakIteratorProviderImpl`
 *  on the JDK 25.0.3 image this VM boots, not from memory.
 *
 *  So the `P.*` rows say whether the last mile works, and the `F.*` rows say
 *  what the pinned factories answer beside it. On HotSpot both halves agree;
 *  a VM where only `F.*` matches is a VM whose BreakIterator is the pin.
 *
 *  The two readers are PACKAGE-PRIVATE (`javap -p` on `LocaleResources`:
 *  `java.lang.Object getBreakIteratorInfo(java.lang.String)`, no modifier), so
 *  no `--add-exports` reaches them directly and this probe cannot print the
 *  blob it is really about. `P.wordClass` is the proxy: the real chain answers
 *  `sun.text.RuleBasedBreakIterator`, and every failure mode in between -- a
 *  null bundle, a null blob, a byte[] the tables reject -- arrives here as a
 *  throw instead.
 *
 *  Needs `--add-exports java.base/sun.util.locale.provider=ALL-UNNAMED` at
 *  COMPILE and at RUN, on HotSpot. This VM does not enforce the boundary, so
 *  forgetting the run-time half gives HotSpot zero rows and this VM all of
 *  them -- the 33-vs-29 trap of wave 5.
 *
 *  Every non-ASCII character is written as a unicode escape on purpose: this
 *  file travels through a Windows checkout, git, and a heredoc before javac
 *  sees it, and a mangled byte in a string literal reads as a VM difference.
 */
public final class L1BreakIterRealProbe {
    static int rows = 0;

    interface Body {
        Object run() throws Throwable;
    }

    static void row(String tag, Body b) {
        rows++;
        String v;
        try {
            Object o = b.run();
            v = o == null ? "null" : String.valueOf(o);
        } catch (Throwable t) {
            String m = t.getMessage();
            v = "THREW " + t.getClass().getName() + (m == null ? "" : " msg=" + scrub(m));
        }
        System.out.println(tag + " |" + v + "|");
    }

    /** Messages carry temp paths and identity hashes on some VMs; keep the
     *  shape and drop the parts that cannot match across two runs. */
    static String scrub(String m) {
        return m.replaceAll("@[0-9a-f]+", "@X").replaceAll("[0-9]{4,}", "N");
    }

    static final String TEXT = "The quick brown fox. It jumped over 42 lazy dogs, didn't it?";
    static final String WRAP =
            "JUnit Platform's help formatter wraps text with BreakIterator.getLineInstance,"
                    + " which is the tripwire that pinned this family in the first place.";
    /** 'a', 'e' + COMBINING ACUTE, 'x', U+1F600 as a surrogate pair, 'y'. */
    static final String GRAPHEMES = "ae\u0301x\ud83d\ude00y";

    /** Every boundary, as a list -- the shape a wrong iterator gets wrong even
     *  when its first and last answers are right. */
    static List<Integer> walk(BreakIterator bi, String text) {
        bi.setText(text);
        List<Integer> out = new ArrayList<>();
        for (int i = bi.first(); i != BreakIterator.DONE; i = bi.next()) {
            out.add(i);
        }
        return out;
    }

    static String walkOf(BreakIterator bi, String text) {
        return walk(bi, text).toString();
    }

    static BreakIteratorProvider provider() {
        return LocaleProviderAdapter.forJRE().getBreakIteratorProvider();
    }

    public static void main(String[] args) throws Throwable {
        final Locale us = Locale.US;

        // ---- P: the real provider chain, unpinned.
        row("P.adapterClass", () -> LocaleProviderAdapter.forJRE().getClass().getName());
        row("P.providerClass", () -> provider().getClass().getName());

        row("P.wordClass", () -> provider().getWordInstance(us).getClass().getName());
        row("P.lineClass", () -> provider().getLineInstance(us).getClass().getName());
        row("P.sentenceClass", () -> provider().getSentenceInstance(us).getClass().getName());
        // JDK 25 answers this one with an inline GraphemeBreakIterator and
        // never reads the bundle -- of the chain's four factories it is the
        // one that proves nothing about the blob.
        row("P.characterClass", () -> provider().getCharacterInstance(us).getClass().getName());

        row("P.wordWalk", () -> walkOf(provider().getWordInstance(us), TEXT));
        row("P.lineWalk", () -> walkOf(provider().getLineInstance(us), TEXT));
        row("P.sentenceWalk", () -> walkOf(provider().getSentenceInstance(us), TEXT));
        row("P.characterWalk", () -> walkOf(provider().getCharacterInstance(us), GRAPHEMES));
        row("P.lineWrapWalk", () -> walkOf(provider().getLineInstance(us), WRAP));

        // A second locale, because the bundle lookup is per-locale and a
        // root-only answer looks identical until someone asks for one that is
        // not the root.
        row("P.wordWalk.fr", () ->
                walkOf(provider().getWordInstance(Locale.FRANCE), "C'est l'ete, n'est-ce pas?"));
        // Thai is the one locale whose `BreakIteratorInfo_th` names
        // DictionaryBasedBreakIterator, i.e. the OTHER arm of
        // getBreakInstance's switch and a second blob (`WordDictionary`).
        row("P.wordClass.th", () ->
                provider().getWordInstance(Locale.forLanguageTag("th-TH"))
                        .getClass().getName());

        // ---- F: the public factories, which the pin serves today.
        row("F.wordClass", () -> BreakIterator.getWordInstance(us).getClass().getName());
        row("F.lineClass", () -> BreakIterator.getLineInstance(us).getClass().getName());
        row("F.sentenceClass", () -> BreakIterator.getSentenceInstance(us).getClass().getName());
        row("F.characterClass", () -> BreakIterator.getCharacterInstance(us).getClass().getName());
        row("F.wordWalk", () -> walkOf(BreakIterator.getWordInstance(us), TEXT));
        row("F.lineWalk", () -> walkOf(BreakIterator.getLineInstance(us), TEXT));
        row("F.sentenceWalk", () -> walkOf(BreakIterator.getSentenceInstance(us), TEXT));
        row("F.characterWalk", () -> walkOf(BreakIterator.getCharacterInstance(us), GRAPHEMES));
        row("F.lineWrapWalk", () -> walkOf(BreakIterator.getLineInstance(us), WRAP));
        row("F.noArgWord", () -> BreakIterator.getWordInstance().getClass().getName());
        row("F.availableLocalesNonEmpty", () -> BreakIterator.getAvailableLocales().length > 0);

        // ---- I: the instance surface the natives also cover, on whatever the
        // factory handed back. `following` / `preceding` / `current` are three
        // of the 17 and no probe in the tree calls them.
        row("I.following", () -> {
            BreakIterator bi = BreakIterator.getWordInstance(us);
            bi.setText(TEXT);
            return bi.following(10);
        });
        row("I.preceding", () -> {
            BreakIterator bi = BreakIterator.getWordInstance(us);
            bi.setText(TEXT);
            return bi.preceding(10);
        });
        row("I.currentAfterFirst", () -> {
            BreakIterator bi = BreakIterator.getWordInstance(us);
            bi.setText(TEXT);
            bi.first();
            return bi.current();
        });
        row("I.lastIsLength", () -> {
            BreakIterator bi = BreakIterator.getWordInstance(us);
            bi.setText(TEXT);
            return bi.last() == TEXT.length();
        });
        row("I.isBoundary0", () -> {
            BreakIterator bi = BreakIterator.getWordInstance(us);
            bi.setText(TEXT);
            return bi.isBoundary(0);
        });
        row("I.previousFromLast", () -> {
            BreakIterator bi = BreakIterator.getWordInstance(us);
            bi.setText(TEXT);
            bi.last();
            return bi.previous();
        });

        System.out.println("rows=" + rows);
    }
}
