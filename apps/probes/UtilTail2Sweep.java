import java.util.*;
import java.util.regex.*;

/** L3 coverage sweep, round 2 — the `java.util` families outside collections.
 *
 *  `UtilCoverageSweep` took the corpus from 445 of 602 owning registrations to
 *  507. The 95 still unreached concentrate in five families no lane of the
 *  seven-lane campaign owned: `Arrays` (11 rows), `Locale` (10), `Formatter`
 *  (9), `UUID` (9) and `java.util.regex` (10), plus `TimeZone` (6). This asks
 *  them.
 *
 *  NOTHING NONDETERMINISTIC IS PRINTED. `randomUUID` is asked for its version
 *  and variant, never its value; `Formatter.out()` for its class, not its
 *  identity; the date rows use a fixed epoch. A probe that prints a fresh UUID
 *  diffs against itself.
 */
public class UtilTail2Sweep {
    static int rows = 0;

    interface ThrowingRun { void run() throws Throwable; }
    interface Val { Object call() throws Throwable; }

    static String esc(String s) {
        return s == null ? "null" : s.replace("\n", "\\n").replace("\r", "\\r");
    }

    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }

    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }

    static void tv(String tag, Val c) {
        try { p(tag, c.call()); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }

    static void arrays() {
        int[] a = {5, 3, 9, 1, 7};
        int[] sorted = a.clone();
        Arrays.sort(sorted);
        p("Arrays.sort(int[])", Arrays.toString(sorted));
        p("Arrays.binarySearch hit", Arrays.binarySearch(sorted, 7));
        p("Arrays.binarySearch miss", Arrays.binarySearch(sorted, 4));
        p("Arrays.binarySearch below", Arrays.binarySearch(sorted, 0));
        p("Arrays.binarySearch above", Arrays.binarySearch(sorted, 99));

        long[] l = {9L, 2L, 7L, 4L, 1L};
        Arrays.sort(l, 1, 4);
        p("Arrays.sort(long[],i,j)", Arrays.toString(l));

        int[] f = new int[4];
        Arrays.fill(f, 6);
        p("Arrays.fill(int[])", Arrays.toString(f));

        p("Arrays.copyOf(int[]) grow", Arrays.toString(Arrays.copyOf(a, 7)));
        p("Arrays.copyOf(int[]) shrink", Arrays.toString(Arrays.copyOf(a, 2)));
        p("Arrays.copyOf(int[]) zero", Arrays.toString(Arrays.copyOf(a, 0)));
        tv("Arrays.copyOf(int[]) negative", () -> Arrays.toString(Arrays.copyOf(a, -1)));

        byte[] b = {1, 2, 3};
        p("Arrays.copyOf(byte[])", Arrays.toString(Arrays.copyOf(b, 5)));
        p("Arrays.equals(byte[]) same", Arrays.equals(b, new byte[] {1, 2, 3}));
        p("Arrays.equals(byte[]) diff", Arrays.equals(b, new byte[] {1, 2, 4}));
        p("Arrays.equals(byte[]) len", Arrays.equals(b, new byte[] {1, 2}));
        p("Arrays.equals(int[]) same", Arrays.equals(a, a.clone()));
        p("Arrays.equals(int[]) null both", Arrays.equals((int[]) null, (int[]) null));

        p("Arrays.hashCode(byte[])", Arrays.hashCode(b));
        p("Arrays.hashCode(byte[]) empty", Arrays.hashCode(new byte[0]));
        p("Arrays.hashCode(byte[]) null", Arrays.hashCode((byte[]) null));
        p("Arrays.hashCode(Object[])", Arrays.hashCode(new Object[] {"a", "b"}));
        p("Arrays.hashCode(Object[]) nested null", Arrays.hashCode(new Object[] {"a", null}));

        StringBuilder st = new StringBuilder();
        Arrays.stream(new Object[] {"x", "y", "z"}).forEach(o -> st.append(o).append(','));
        p("Arrays.stream(Object[])", st.toString());
        p("Arrays.stream count", Arrays.stream(new Object[] {"x", "y"}).count());
    }

    static void uuid() {
        UUID u = new UUID(0x0123456789abcdefL, 0xfedcba9876543210L);
        p("UUID ctor toString", u.toString());
        p("UUID msb", u.getMostSignificantBits());
        p("UUID lsb", u.getLeastSignificantBits());
        p("UUID version", u.version());
        p("UUID variant", u.variant());
        p("UUID hashCode", u.hashCode());
        p("UUID equals self", u.equals(new UUID(0x0123456789abcdefL, 0xfedcba9876543210L)));
        p("UUID equals other", u.equals(new UUID(1L, 2L)));
        p("UUID equals non-uuid", u.equals("x"));

        UUID parsed = UUID.fromString("01234567-89ab-cdef-fedc-ba9876543210");
        p("UUID fromString round trip", parsed.toString());
        p("UUID fromString equals ctor", parsed.equals(u));
        tv("UUID fromString bad", () -> UUID.fromString("not-a-uuid"));
        tv("UUID fromString empty", () -> UUID.fromString(""));

        // Value is random; its SHAPE is not.
        UUID r = UUID.randomUUID();
        p("UUID randomUUID version", r.version());
        p("UUID randomUUID variant", r.variant());
        p("UUID randomUUID length", r.toString().length());
        p("UUID randomUUID distinct", !r.equals(UUID.randomUUID()));
    }

    static void formatter() {
        Formatter f = new Formatter();
        f.format("%s=%d", "a", 1);
        p("Formatter() toString", f.toString());
        p("Formatter out class", f.out().getClass().getName());
        p("Formatter locale is default", f.locale() == null ? "null" : "non-null");
        f.flush();
        f.close();
        tv("Formatter after close", () -> { f.format("x"); return "no-throw"; });

        StringBuilder sb = new StringBuilder();
        Formatter fa = new Formatter(sb);
        fa.format("%03d|%s|%.2f", 7, "z", 1.5);
        p("Formatter(Appendable)", sb.toString());
        p("Formatter(Appendable) out is sb", fa.out() == sb);
        fa.close();

        Formatter fl = new Formatter(Locale.ENGLISH);
        fl.format("%,d", 1234567);
        p("Formatter(Locale) grouping", fl.toString());
        p("Formatter(Locale) locale", String.valueOf(fl.locale()));
        fl.close();
    }

    static void regex() {
        Pattern p1 = Pattern.compile("(\\w+)@(\\w+)");
        p("Pattern.compile pattern", p1.pattern());
        p("Pattern.compile flags", p1.flags());
        Pattern p2 = Pattern.compile("ab", Pattern.CASE_INSENSITIVE);
        p("Pattern.compile(flags) flags", p2.flags());
        p("Pattern(flags) matches upper", p2.matcher("AB").matches());
        tv("Pattern.compile bad", () -> Pattern.compile("("));

        Matcher m = p1.matcher("alice@example bob@host");
        p("Matcher find 1", m.find());
        p("Matcher group()", m.group());
        p("Matcher group(1)", m.group(1));
        p("Matcher group(2)", m.group(2));
        p("Matcher start()", m.start());
        p("Matcher end()", m.end());
        p("Matcher start(1)", m.start(1));
        p("Matcher end(1)", m.end(1));
        p("Matcher find 2", m.find());
        p("Matcher group after 2", m.group());
        p("Matcher find 3", m.find());
        tv("Matcher group after exhausted", () -> m.group());

        Matcher m2 = p1.matcher("alice@example bob@host");
        p("Matcher find(int)", m2.find(14));
        p("Matcher group after find(int)", m2.group());
        tv("Matcher group(9)", () -> p1.matcher("a@b").group(9));
    }

    static void dateZone() {
        TimeZone utc = TimeZone.getTimeZone("UTC");
        TimeZone ny = TimeZone.getTimeZone("America/New_York");
        p("TZ utc id", utc.getID());
        p("TZ utc rawOffset", utc.getRawOffset());
        p("TZ ny rawOffset", ny.getRawOffset());
        p("TZ ny useDaylightTime", ny.useDaylightTime());
        p("TZ ny dstSavings", ny.getDSTSavings());
        p("TZ ny inDaylight epoch", ny.inDaylightTime(new Date(0L)));
        p("TZ getAvailableIDs has UTC", Arrays.asList(TimeZone.getAvailableIDs()).contains("UTC"));
        p("TZ unknown id", TimeZone.getTimeZone("No/Such_Zone").getID());

        Locale l = Locale.forLanguageTag("fr-CA");
        p("Locale tag language", l.getLanguage());
        p("Locale tag country", l.getCountry());
        p("Locale toLanguageTag", l.toLanguageTag());
        p("Locale getISO3Language", l.getISO3Language());
        p("Locale getISO3Country", l.getISO3Country());
        p("Locale displayVariant", "[" + l.getDisplayVariant(Locale.ENGLISH) + "]");
        p("Locale toString", l.toString());
        p("Locale equals", l.equals(Locale.forLanguageTag("fr-CA")));
        p("Locale hashCode stable", l.hashCode() == Locale.forLanguageTag("fr-CA").hashCode());
        p("Locale getDefault non-null", Locale.getDefault() != null);
    }

    public static void main(String[] a) {
        arrays();
        uuid();
        formatter();
        regex();
        dateZone();
        System.out.println("DONE UtilTail2Sweep");
    }
}
