import java.nio.charset.StandardCharsets;
import java.text.DateFormatSymbols;
import java.text.DecimalFormatSymbols;
import java.time.LocalDateTime;
import java.util.*;

/**
 * Regression: String / StringBuilder / formatting. Weighted toward UTF-16
 * code-unit indexing (a regressed area): all String indices are over UTF-16
 * code units, so supplementary characters (emoji) occupy two units.
 */
public class RStrings {
    static int checks = 0;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }

    public static void main(String[] a) {
        // ---- UTF-16 code-unit indexing with a supplementary char (U+1F600) ----
        // length() and charAt() are over UTF-16 code units (😀 = 2 units).
        // (NOTE: indexOf(char)/getChars positioning past a supplementary char is
        //  a known partial CratonVM gap — see README. Not asserted here.)
        String emoji = "a😀b";               // 'a', 😀 (2 units), 'b'
        check(emoji.length() == 4, "supplementary length (code units)");
        check(emoji.charAt(0) == 'a' && emoji.charAt(3) == 'b', "charAt past supplementary (code units)");

        String s = "The quick brown fox";
        check(s.indexOf("quick") == 4, "indexOf(str)");
        check(s.indexOf("o", 13) == 17, "indexOf(str,from)");
        check(s.lastIndexOf("o") == 17, "lastIndexOf");
        check(s.substring(4, 9).equals("quick"), "substring");
        check(s.replace("quick", "slow").equals("The slow brown fox"), "replace");
        check("a,b,,c".split(",").length == 4, "split keeps interior empties");
        check(String.join("-", "x", "y", "z").equals("x-y-z"), "join");
        check("  hi  ".trim().equals("hi") && "  hi  ".strip().equals("hi"), "trim/strip");
        check("AbC".equalsIgnoreCase("abc"), "equalsIgnoreCase");
        check("abc".compareTo("abd") < 0, "compareTo");
        check("abcabc".chars().filter(c -> c == 'a').count() == 2, "chars stream");

        // ---- StringBuilder ----
        StringBuilder sb = new StringBuilder("hello");
        sb.append(' ').append(42).append(' ').append(true);
        sb.insert(0, ">>");
        sb.reverse();
        check(sb.toString().equals("eurt 24 olleh>>"), "StringBuilder reverse: " + sb);
        StringBuilder sb2 = new StringBuilder();
        for (int i = 0; i < 5; i++) sb2.append(i);
        sb2.deleteCharAt(2);
        check(sb2.toString().equals("0134"), "deleteCharAt");

        // ---- StringBuilder: the bounds and overloads W7-3 left open ----
        // append(CharSequence,int,int) used to CLAMP its window, so an
        // out-of-range request appended a SHORT slice and reported success.
        // Both polarities, because a native that refuses everything would pass
        // a one-sided check. (JDK: `checkRange` -> IndexOutOfBoundsException,
        // NOT the String subclass — a caller catching SIOOBE must not see it.)
        StringBuilder sbr = new StringBuilder();
        sbr.append((CharSequence) "abcdef", 1, 4);
        check(sbr.toString().equals("bcd"), "append(CharSequence,int,int) window: " + sbr);
        boolean clamped = true;
        try {
            sbr.append((CharSequence) "abc", 0, 100);
            clamped = true;
        } catch (IndexOutOfBoundsException e) {
            clamped = false;
        }
        check(!clamped, "append(CharSequence,0,100) over \"abc\" must throw, not clamp");
        check(sbr.toString().equals("bcd"), "a refused append must not be a partial one: " + sbr);

        // appendCodePoint used to truncate an invalid code point to its low 16
        // bits — a silent wrong character where the caller asked for a refusal.
        // A lone surrogate is a BMP code point and stays legal (that is the JDK
        // rule the old truncation was justified by, misread).
        StringBuilder sbc = new StringBuilder();
        sbc.appendCodePoint(0x41);
        sbc.appendCodePoint(0x1F600);          // supplementary -> 2 code units
        sbc.appendCodePoint(0xD800);           // lone surrogate -> 1 code unit, legal
        check(sbc.length() == 4, "appendCodePoint code-unit accounting: " + sbc.length());
        boolean truncated = true;
        try {
            sbc.appendCodePoint(0x110000);
            truncated = true;
        } catch (IllegalArgumentException e) {
            truncated = false;
        }
        check(!truncated, "appendCodePoint(0x110000) must throw IllegalArgumentException");
        check(sbc.length() == 4, "a refused appendCodePoint must not write: " + sbc.length());

        // insert(int, boolean/long/float/double) had no native at all, so real
        // JDK bytecode ran against the synthetic char[]/count layout. Values
        // chosen so a truncating or byte[]-shaped path cannot produce them.
        check(new StringBuilder("ac").insert(1, true).toString().equals("atruec"), "insert(I,Z)");
        check(new StringBuilder("ac").insert(1, 1234567890123L).toString()
                .equals("a1234567890123c"), "insert(I,J)");
        check(new StringBuilder("ac").insert(1, 1.5f).toString().equals("a1.5c"), "insert(I,F)");
        check(new StringBuilder("ac").insert(1, 2.25d).toString().equals("a2.25c"), "insert(I,D)");
        boolean insertClamped = true;
        try {
            new StringBuilder("ab").insert(99, 1L);
            insertClamped = true;
        } catch (StringIndexOutOfBoundsException e) {
            insertClamped = false;
        }
        check(!insertClamped, "insert past the end must throw, not append");

        // ---- Formatting (pin Locale.US so separators are VM/host-locale independent) ----
        check(String.format(Locale.US, "%d/%05d/%x", 42, 7, 255).equals("42/00007/ff"), "format ints");
        check(String.format(Locale.US, "%.2f|%e", 3.14159, 1234.5).startsWith("3.14|1.23"), "format floats");
        check(String.format(Locale.US, "%s=%b", "ok", true).equals("ok=true"), "format str/bool");
        check(String.format(Locale.US, "%,d", 1234567).equals("1,234,567"), "format grouping");

        // `%a` with the '0' flag and a width. Formatter zero-pads INSIDE the
        // "0x" prefix (and inside the sign, which sits outside it), so the
        // generic "prepend the zeros" padder is wrong here in a way a bare
        // `%a` check cannot see. Measured shape, from
        // FormatSpecifier.print(...HEXADECIMAL_FLOAT): leadingCharacters is 2,
        // or 3 once a sign is present.
        check(String.format(Locale.US, "%a", 1.0).equals("0x1.0p0"), "hex float");
        check(String.format(Locale.US, "%020a", 1.0).equals("0x00000000000001.0p0"),
                "hex float zero-pad goes after 0x: " + String.format(Locale.US, "%020a", 1.0));
        check(String.format(Locale.US, "%+020a", 1.0).equals("+0x0000000000001.0p0"),
                "hex float zero-pad goes after the sign AND the prefix: "
                        + String.format(Locale.US, "%+020a", 1.0));

        // A Formatter built WITH a locale must format with it. `format(String,
        // Object[])` has no locale argument, so the receiver's is the only
        // source — and it used to be discarded, making the two spellings of
        // the same request disagree. Asserted as an equality between them
        // rather than against a fixed "1.234,50" so a platform whose German
        // locale data is unavailable cannot turn this into a false red.
        StringBuilder fsink = new StringBuilder();
        try (Formatter f = new Formatter(fsink, Locale.GERMANY)) {
            f.format("%,.2f", 1234.5);
        }
        check(fsink.toString().equals(String.format(Locale.GERMANY, "%,.2f", 1234.5)),
                "new Formatter(sb, Locale.GERMANY) must honour its locale; got ["
                        + fsink + "] vs [" + String.format(Locale.GERMANY, "%,.2f", 1234.5) + "]");
        // And the locale must actually DO something: an implication, so it is
        // the VM discarding a locale it could resolve — not a platform without
        // the data — that fails here. `contains`-style checks over an
        // all-English rendering cannot see that difference.
        // Written as one unconditional check rather than a guarded block so the
        // reported check COUNT does not depend on the platform's locale data —
        // a count that moves between arms is a diff of its own. The CK line
        // below is the negative control: it says whether the implication had an
        // antecedent at all, so a vacuous pass is visible rather than silent.
        DecimalFormatSymbols de = DecimalFormatSymbols.getInstance(Locale.GERMANY);
        boolean deResolved = de.getDecimalSeparator() == ',' && de.getGroupingSeparator() == '.';
        check(!deResolved || String.format(Locale.GERMANY, "%,.2f", 1234.5).equals("1.234,50"),
                "de_DE symbols resolved, so the rendering must use them; got "
                        + String.format(Locale.GERMANY, "%,.2f", 1234.5));
        System.out.println("CK RStrings deDecimal=" + de.getDecimalSeparator()
                + " deGrouping=" + de.getGroupingSeparator() + " deResolved=" + deResolved);

        // ---- %t/%T renders NAMES from DateFormatSymbols, not from a table ----
        // The six name-bearing date/time conversions — %tB, %tb/%th, %tA, %ta,
        // %tp, and %tc over two of them — used to be answered from four
        // hard-coded English arrays, so every rendering was English whatever
        // the format locale was. `java.util.logging.SimpleFormatter`'s default
        // pattern opens `%1$tb`, which is how this reached a user: every log
        // line named its month in English on a host that is not English.
        //
        // Each check is an EQUALITY AGAINST THE JDK'S OWN TABLE for the same
        // locale, never against a pinned "Mär": both sides read one array, so
        // a platform whose German data is missing cannot manufacture a red.
        // The instant is fixed, so nothing here follows the calendar.
        LocalDateTime tstamp = LocalDateTime.of(2026, 3, 4, 5, 6, 7);   // Wednesday, 05:06 => AM
        DateFormatSymbols deSyms = DateFormatSymbols.getInstance(Locale.GERMANY);
        // Calendar.SUNDAY..SATURDAY is 1..7 with slot 0 unused, while ISO
        // DayOfWeek is 1=Monday..7=Sunday. This is `printDateTime`'s own
        // `DAY_OF_WEEK % 7 + 1`, not a hand-counted index.
        int calDow = tstamp.getDayOfWeek().getValue() % 7 + 1;
        // Whether the implications below have an antecedent at all. Computed
        // first so it can be reported; a `false` here means this platform's
        // German names ARE the English ones, and the checks are then green on
        // the old behaviour too — vacuous, and visible in the CK line.
        boolean deNames = !deSyms.getShortMonths()[2].equals("Mar")
                && !deSyms.getWeekdays()[calDow].equals("Wednesday");
        check(String.format(Locale.GERMANY, "%tb", tstamp).equals(deSyms.getShortMonths()[2]),
                "%tb must be DateFormatSymbols.getShortMonths() for the format locale; got "
                        + String.format(Locale.GERMANY, "%tb", tstamp));
        check(String.format(Locale.GERMANY, "%tB", tstamp).equals(deSyms.getMonths()[2]),
                "%tB must be getMonths(); got " + String.format(Locale.GERMANY, "%tB", tstamp));
        check(String.format(Locale.GERMANY, "%ta", tstamp).equals(deSyms.getShortWeekdays()[calDow]),
                "%ta must be getShortWeekdays(); got " + String.format(Locale.GERMANY, "%ta", tstamp));
        check(String.format(Locale.GERMANY, "%tA", tstamp).equals(deSyms.getWeekdays()[calDow]),
                "%tA must be getWeekdays(); got " + String.format(Locale.GERMANY, "%tA", tstamp));
        check(String.format(Locale.GERMANY, "%tp", tstamp)
                        .equals(deSyms.getAmPmStrings()[0].toLowerCase(Locale.GERMANY)),
                "%tp must be getAmPmStrings(), lower-cased; got "
                        + String.format(Locale.GERMANY, "%tp", tstamp));
        // The overload with NO locale is the one SimpleFormatter uses. A real
        // Formatter built without one carries
        // Locale.getDefault(Locale.Category.FORMAT), which is exactly what
        // DateFormatSymbols.getInstance() resolves — so the two must agree on
        // any host, and on an en_US host they agree in English.
        check(String.format("%tb", tstamp)
                        .equals(DateFormatSymbols.getInstance().getShortMonths()[2]),
                "the no-locale overload must render the default FORMAT locale; got "
                        + String.format("%tb", tstamp));
        // Anti-overshoot: an implementation that answered the DEFAULT locale
        // for an explicit one, or German for every locale, passes everything
        // above and fails here.
        String usMonth = String.format(Locale.US, "%tb", tstamp);
        check(usMonth.equals(DateFormatSymbols.getInstance(Locale.US).getShortMonths()[2])
                        && (!deNames || !usMonth.equals(String.format(Locale.GERMANY, "%tb", tstamp))),
                "an explicit Locale.US must render US English and must not be answered from the"
                        + " German table; got " + usMonth);
        // LENGTHS, not the names themselves: this line goes through the
        // harness's byte-for-byte cross-VM diff, and printing `Mär` there would
        // put the two VMs' stdout ENCODINGS on trial in a row about month
        // names. A data-less platform prints 3/9 (`Mar`/`Wednesday`) with
        // deNames=false, so the vacuous case stays visible either way.
        System.out.println("CK RStrings deMonthAbbrLen=" + deSyms.getShortMonths()[2].length()
                + " deWeekdayLen=" + deSyms.getWeekdays()[calDow].length()
                + " deNames=" + deNames);

        // ---- Charset round-trip ----
        byte[] u8 = "héllo·世界".getBytes(StandardCharsets.UTF_8);
        check(new String(u8, StandardCharsets.UTF_8).equals("héllo·世界"), "UTF-8 round-trip");
        byte[] ascii = "ABC".getBytes(StandardCharsets.US_ASCII);
        check(ascii.length == 3 && ascii[0] == 65, "ASCII bytes");

        // ---- text blocks + repeat/strip ----
        check("ab".repeat(3).equals("ababab"), "repeat");
        check("hello".contains("ell") && "hello".startsWith("he") && "hello".endsWith("lo"), "contains/prefix/suffix");

        System.out.println("PASS RStrings (" + checks + " checks)");
    }
}
