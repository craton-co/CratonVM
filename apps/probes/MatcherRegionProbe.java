import java.util.regex.*;
import java.nio.*;

/** L3 — the region-bounded `Matcher` API, which is the whole blocker to
 *  retiring `java.util.Scanner`.
 *
 *  MEASURED (`apps/probes/ScannerShadowSweep` under
 *  `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/Scanner`): retiring Scanner's 43
 *  shadows takes it from 25 wrong rows of 94 to 12 — it FIXES 24 and BREAKS 11.
 *  The 11 are one family: `nextLine`, `hasNextLine`, `findInLine`,
 *  `findWithinHorizon` and `match`. In the JDK all of them are one mechanism:
 *  `Scanner` searches its buffer through a `Matcher` with an explicit REGION,
 *  transparent bounds, non-anchoring bounds, and `hitEnd`/`requireEnd` to decide
 *  whether to read more input.
 *
 *  So this asks that API directly, without a Scanner in the way. If the region
 *  calls are the gap, the Scanner retirement is downstream of fixing them.
 *
 *  `Scanner` reads from a `CharBuffer`, not a `String`, so the CharSequence rows
 *  are asked both ways: a matcher over a `CharBuffer` is the shape that actually
 *  runs inside a real Scanner.
 */
public class MatcherRegionProbe {
    static int rows = 0;

    interface Val { Object call() throws Throwable; }

    static String esc(String s) {
        return s == null ? "null" : s.replace("\n", "\\n").replace("\r", "\\r");
    }

    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }

    static void tv(String tag, Val c) {
        try { p(tag, c.call()); }
        catch (Throwable e) {
            p(tag, "THREW " + e.getClass().getName() + " msg=" + esc(e.getMessage()));
        }
    }

    public static void main(String[] args) {
        // ---- region basics
        Matcher m = Pattern.compile("b+").matcher("abbbc");
        tv("no region regionStart", () -> m.regionStart());
        tv("no region regionEnd", () -> m.regionEnd());
        tv("find", () -> m.find() + " " + m.group() + " @" + m.start());

        tv("region(1,3) then find", () -> {
            Matcher r = Pattern.compile("b+").matcher("abbbc");
            r.region(1, 3);
            return r.find() + " [" + r.group() + "] start=" + r.start();
        });
        tv("region bounds reported", () -> {
            Matcher r = Pattern.compile("x").matcher("abcdef");
            r.region(2, 4);
            return r.regionStart() + ".." + r.regionEnd();
        });
        tv("region excludes match", () -> {
            Matcher r = Pattern.compile("f").matcher("abcdef");
            r.region(0, 3);
            return r.find();
        });
        tv("region out of range", () -> {
            Matcher r = Pattern.compile("a").matcher("abc");
            r.region(0, 99);
            return "no-throw";
        });
        tv("region resets append pos", () -> {
            Matcher r = Pattern.compile("a").matcher("aaa");
            r.find();
            r.region(0, 3);
            return r.find() + " start=" + r.start();
        });

        // ---- anchoring bounds: does ^/$ see the region edge or the input edge?
        tv("anchoring default", () -> {
            Matcher r = Pattern.compile("^b").matcher("abc");
            r.region(1, 3);
            return r.find();
        });
        tv("useAnchoringBounds(false)", () -> {
            Matcher r = Pattern.compile("^b").matcher("abc");
            r.region(1, 3);
            r.useAnchoringBounds(false);
            return r.find();
        });
        tv("hasAnchoringBounds default", () -> Pattern.compile("a").matcher("a")
                .hasAnchoringBounds());
        tv("hasAnchoringBounds after set", () -> Pattern.compile("a").matcher("a")
                .useAnchoringBounds(false).hasAnchoringBounds());

        // ---- transparent bounds: can lookbehind/lookahead see outside the region?
        tv("transparent default", () -> {
            Matcher r = Pattern.compile("(?<=a)b").matcher("abc");
            r.region(1, 3);
            return r.find();
        });
        tv("useTransparentBounds(true)", () -> {
            Matcher r = Pattern.compile("(?<=a)b").matcher("abc");
            r.region(1, 3);
            r.useTransparentBounds(true);
            return r.find();
        });
        tv("hasTransparentBounds default", () -> Pattern.compile("a").matcher("a")
                .hasTransparentBounds());
        tv("hasTransparentBounds after set", () -> Pattern.compile("a").matcher("a")
                .useTransparentBounds(true).hasTransparentBounds());

        // ---- hitEnd / requireEnd, which is how Scanner decides to read more
        tv("hitEnd after failed partial", () -> {
            Matcher r = Pattern.compile("abcd").matcher("abc");
            boolean f = r.find();
            return f + " hitEnd=" + r.hitEnd();
        });
        tv("hitEnd after full match", () -> {
            Matcher r = Pattern.compile("ab").matcher("abcd");
            boolean f = r.find();
            return f + " hitEnd=" + r.hitEnd();
        });
        tv("hitEnd on no match", () -> {
            Matcher r = Pattern.compile("z").matcher("abc");
            return r.find() + " hitEnd=" + r.hitEnd();
        });
        tv("requireEnd", () -> {
            Matcher r = Pattern.compile("ab$").matcher("ab");
            return r.find() + " requireEnd=" + r.requireEnd();
        });

        // ---- lookingAt / matches within a region, the other Scanner primitives
        tv("lookingAt in region", () -> {
            Matcher r = Pattern.compile("bb").matcher("abbc");
            r.region(1, 4);
            return r.lookingAt() + " [" + r.group() + "]";
        });
        tv("matches in region", () -> {
            Matcher r = Pattern.compile("bb").matcher("abbc");
            r.region(1, 3);
            return r.matches();
        });
        tv("reset clears region", () -> {
            Matcher r = Pattern.compile("f").matcher("abcdef");
            r.region(0, 3);
            r.reset();
            return r.find() + " " + r.regionStart() + ".." + r.regionEnd();
        });

        // ---- a CharBuffer input, which is what a real Scanner matches against
        tv("CharBuffer find", () -> {
            CharBuffer cb = CharBuffer.wrap("hello world");
            Matcher r = Pattern.compile("w\\w+").matcher(cb);
            return r.find() + " [" + r.group() + "]";
        });
        tv("CharBuffer region find", () -> {
            CharBuffer cb = CharBuffer.wrap("hello world");
            Matcher r = Pattern.compile("\\w+").matcher(cb);
            r.region(6, 11);
            return r.find() + " [" + r.group() + "]";
        });
        tv("CharBuffer hitEnd", () -> {
            CharBuffer cb = CharBuffer.wrap("abc");
            Matcher r = Pattern.compile("abcd").matcher(cb);
            return r.find() + " hitEnd=" + r.hitEnd();
        });

        // ---- the line pattern Scanner actually uses for nextLine
        String LINE = ".*(\r\n|[\n\r\u2028\u2029\u0085])|.+$";
        tv("line pattern on one line", () -> {
            Matcher r = Pattern.compile(LINE).matcher("one\ntwo");
            return r.find() + " [" + r.group() + "]";
        });
        tv("line pattern last line", () -> {
            Matcher r = Pattern.compile(LINE).matcher("only");
            return r.find() + " [" + r.group() + "]";
        });
        tv("line pattern crlf", () -> {
            Matcher r = Pattern.compile(LINE).matcher("a\r\nb");
            return r.find() + " [" + esc(r.group()) + "]";
        });
        tv("line pattern empty line", () -> {
            Matcher r = Pattern.compile(LINE).matcher("\nx");
            return r.find() + " [" + esc(r.group()) + "]";
        });
        tv("line pattern in region", () -> {
            Matcher r = Pattern.compile(LINE).matcher("one\ntwo\n");
            r.region(4, 8);
            r.useAnchoringBounds(false);
            r.useTransparentBounds(true);
            return r.find() + " [" + esc(r.group()) + "]";
        });

        // `usePattern` is the FIRST call in `Scanner.findPatternInBuffer`, and
        // the one member of this family the first pass did not ask.
        tv("usePattern then find", () -> {
            Matcher r = Pattern.compile("a").matcher("abc123");
            r.usePattern(Pattern.compile("\\d+"));
            return r.find() + " [" + r.group() + "]";
        });
        tv("usePattern keeps position", () -> {
            Matcher r = Pattern.compile("b").matcher("abcb");
            r.find();
            r.usePattern(Pattern.compile("c"));
            return r.find() + " start=" + r.start();
        });
        tv("usePattern keeps region", () -> {
            Matcher r = Pattern.compile("z").matcher("abcdef");
            r.region(2, 5);
            r.usePattern(Pattern.compile("\\w"));
            return r.find() + " [" + r.group() + "] " + r.regionStart() + ".." + r.regionEnd();
        });
        tv("usePattern null", () -> {
            Matcher r = Pattern.compile("a").matcher("a");
            r.usePattern(null);
            return "no-throw";
        });
        tv("usePattern pattern()", () -> {
            Matcher r = Pattern.compile("a").matcher("a");
            r.usePattern(Pattern.compile("b+"));
            return r.pattern().pattern();
        });
        tv("usePattern then hitEnd", () -> {
            Matcher r = Pattern.compile("a").matcher("abc");
            r.usePattern(Pattern.compile("abcd"));
            return r.find() + " hitEnd=" + r.hitEnd();
        });
        tv("usePattern on CharBuffer in region", () -> {
            CharBuffer cb = CharBuffer.wrap("one\\ntwo\\n");
            Matcher r = Pattern.compile("x").matcher(cb);
            r.usePattern(Pattern.compile(".*(\\r\\n|[\\n\\r])|.+$"));
            r.region(4, 8);
            r.useAnchoringBounds(false);
            r.useTransparentBounds(true);
            return r.find() + " [" + esc(r.group()) + "]";
        });
        tv("toMatchResult after find", () -> {
            Matcher r = Pattern.compile("b+").matcher("abbc");
            r.find();
            MatchResult mr = r.toMatchResult();
            return mr.group() + " " + mr.start() + ".." + mr.end();
        });
        tv("reset(CharSequence)", () -> {
            Matcher r = Pattern.compile("a").matcher("zzz");
            r.reset("aaa");
            return r.find() + " start=" + r.start();
        });

        System.out.println("DONE MatcherRegionProbe");
    }
}
