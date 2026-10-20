import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * SrParity — line-per-case parity battery for the Matcher.find()/group() fast
 * path and the StringBuilder append path. Run on HotSpot and on CratonVM and
 * diff the two outputs; any difference is a regression.
 *
 * Deliberately exercises the shapes the groups[] bulk-write change touches:
 * multiple capture groups, optional groups that do NOT participate (-1 slots),
 * a failing find() after a successful one (the clear path), find(int) (reset +
 * search), region(), zero-width matches, and group state read back through
 * start()/end()/group() rather than through find()'s return value alone.
 */
public class SrParity {
    static int line = 0;

    static void p(String s) {
        System.out.println((++line) + " " + s);
    }

    static String dump(Matcher m) {
        StringBuilder sb = new StringBuilder();
        sb.append("gc=").append(m.groupCount());
        for (int g = 0; g <= m.groupCount(); g++) {
            sb.append(" [").append(g).append(']');
            try {
                sb.append(m.start(g)).append(',').append(m.end(g)).append(',').append(m.group(g));
            } catch (RuntimeException e) {
                sb.append("EX:").append(e.getClass().getSimpleName());
            }
        }
        return sb.toString();
    }

    static void sweep(String pat, String text) {
        try {
            Pattern p = Pattern.compile(pat);
            Matcher m = p.matcher(text);
            int n = 0;
            while (m.find() && n++ < 40) {
                p("find " + pat + " | " + dump(m));
            }
            p("exhausted " + pat + " n=" + n + " hitEndIsBool=" + (m.hitEnd() || true));
            // groups[] must read as cleared after the failing find()
            try {
                p("afterfail start=" + m.start());
            } catch (RuntimeException e) {
                p("afterfail EX:" + e.getClass().getSimpleName());
            }
        } catch (RuntimeException e) {
            p("compile EX:" + e.getClass().getSimpleName() + " for " + pat);
        }
    }

    public static void main(String[] args) {
        // --- multi-group, optional groups, non-participating captures --------
        sweep("(\\d+)", "1 22 333 4444");
        sweep("(a+)(b+)", "aab abb x aaabbb");
        sweep("(x)?(y)", "y xy y");
        sweep("(\\w)(\\w)?(\\w)?", "a bc def");
        sweep("()", "ab");
        sweep("(?:ab)(c)", "abc abc");
        sweep("(\\d+)-(\\d+)-(\\d+)", "1-2-3 and 44-55-66");
        sweep("q", "no match here");
        sweep("(\\d+)", "");

        // --- find(int): reset + search ---------------------------------------
        Matcher m = Pattern.compile("(\\d+)").matcher("11 22 33");
        p("findAt(0)=" + m.find(0) + " " + dump(m));
        p("findAt(3)=" + m.find(3) + " " + dump(m));
        p("findAt(7)=" + m.find(7) + " " + dump(m));
        p("findAt(8)=" + m.find(8));
        p("findAt(4)=" + m.find(4) + " " + dump(m));

        // --- region ----------------------------------------------------------
        Matcher r = Pattern.compile("(\\d+)").matcher("11 22 33");
        r.region(3, 5);
        p("region find=" + r.find() + " " + dump(r));
        p("region find2=" + r.find());

        // --- zero-width + reset ---------------------------------------------
        Matcher z = Pattern.compile("(?=(\\d))").matcher("a1b2");
        int zn = 0;
        while (z.find() && zn++ < 10) {
            p("zw " + dump(z));
        }
        z.reset();
        p("after reset find=" + z.find() + " " + dump(z));

        // --- replaceAll / appendReplacement read the same groups[] -----------
        p("replaceAll=" + Pattern.compile("(\\d+)").matcher("a1b22c").replaceAll("<$1>"));
        Matcher ar = Pattern.compile("(\\d+)").matcher("a1b22c");
        StringBuilder out = new StringBuilder();
        while (ar.find()) {
            ar.appendReplacement(out, "[" + ar.group(1) + "]");
        }
        ar.appendTail(out);
        p("appendReplacement=" + out);

        // --- matches()/lookingAt() populate groups[] without the fast path ---
        Matcher mm = Pattern.compile("(\\d+)-(\\d+)").matcher("12-34");
        p("matches=" + mm.matches() + " " + dump(mm));
        p("lookingAt=" + mm.lookingAt() + " " + dump(mm));

        // --- StringBuilder append surface the render change touches ----------
        StringBuilder sb = new StringBuilder();
        sb.append(0).append(' ').append(-1).append(' ').append(Integer.MIN_VALUE);
        sb.append(' ').append(Integer.MAX_VALUE).append(' ').append(Long.MIN_VALUE);
        sb.append(' ').append(Long.MAX_VALUE).append(' ').append(123456789L);
        sb.append(' ').append(true).append(' ').append('Z').append(' ').append("s");
        sb.append(' ').append((Object) null).append(' ').append(0.5d).append(' ').append(0.5f);
        p("builder=" + sb + " len=" + sb.length() + " cap>=len=" + (sb.capacity() >= sb.length()));

        // long append (past the stack-buffer threshold) and a non-LATIN1 one
        StringBuilder big = new StringBuilder();
        for (int i = 0; i < 8; i++) {
            big.append("0123456789abcdefghij");
        }
        p("bigLen=" + big.length() + " tail=" + big.substring(big.length() - 5));
        StringBuilder u = new StringBuilder();
        u.append("a€b").append(7).append('é').append("😀");
        p("utf16=" + u.length() + " cp=" + u.codePointAt(0) + " s=" + u);

        // StringBuffer's toStringCache must be invalidated by every mutator
        StringBuffer buf = new StringBuffer("abc");
        buf.toString();
        buf.append(7);
        p("bufCacheAppendInt=" + buf);
        buf.toString();
        buf.insert(0, 9);
        p("bufCacheInsertInt=" + buf);
        buf.toString();
        buf.setLength(2);
        p("bufCacheSetLength=" + buf);
    }
}
