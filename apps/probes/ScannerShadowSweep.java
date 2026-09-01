import java.io.*;
import java.math.*;
import java.util.*;
import java.util.regex.*;

/** L3 — `java.util.Scanner`, 43 owning registrations that no probe had asked.
 *
 *  The `java.util` corpus reaches 566 of 586 owning registrations on the classes
 *  it covers, and `Scanner` is not one of them: it is the largest shadowed class
 *  in the package with NO differential coverage at all, ahead of `Random` (32).
 *  A shadow nothing measures is a shadow whose divergences are unknown, not
 *  absent.
 *
 *  DETERMINISM. Every source is a `String` or a `ByteArrayInputStream` — no
 *  stdin, no files, no clock. Number parsing is locale-sensitive, so every
 *  scanner that reads one is pinned with `useLocale(Locale.US)` and one block
 *  deliberately does not, to ask what the DEFAULT locale is.
 *
 *  THE EXCEPTION TYPE IS THE OBSERVABLE, not just "did it throw". `Scanner`
 *  distinguishes `NoSuchElementException` (nothing left),
 *  `InputMismatchException` (something left, wrong shape) and
 *  `IllegalStateException` (closed) — three different bugs in the caller — so
 *  every row prints the type and the message.
 */
public class ScannerShadowSweep {
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

    static Scanner sc(String src) {
        return new Scanner(src).useLocale(Locale.US);
    }

    public static void main(String[] args) {
        // ---- construction and identity
        tv("class", () -> sc("a").getClass().getName());
        tv("default delimiter", () -> esc(sc("a").delimiter().pattern()));
        tv("default radix", () -> sc("a").radix());
        tv("locale after useLocale", () -> sc("a").locale().toString());
        // `tv`, not `p`: this returned NULL on CratonVM and the NPE killed the
        // run at row 5 of 94, so every later row was unmeasured. A probe row
        // that can take the process down has to be caught.
        tv("default locale", () -> new Scanner("a").locale()
                .equals(Locale.getDefault(Locale.Category.FORMAT)));
        tv("toString shape", () -> {
            String s = sc("a b").toString();
            // The identity hash inside it is not comparable; its SHAPE is.
            return s.startsWith("java.util.Scanner[delimiters=") + " haslocale="
                    + s.contains("locale=");
        });

        // ---- the token walk
        Scanner t = sc("alpha beta  gamma");
        tv("hasNext 1", () -> t.hasNext());
        tv("next 1", () -> t.next());
        tv("next 2", () -> t.next());
        tv("next 3", () -> t.next());
        tv("hasNext end", () -> t.hasNext());
        tv("next past end", () -> t.next());

        // ---- whitespace shapes: tabs, newlines, leading and trailing runs
        tv("tabs and newlines", () -> drain(sc("  a\t\tb\n\nc  ")));
        tv("empty source hasNext", () -> sc("").hasNext());
        tv("blank source hasNext", () -> sc("   \n\t ").hasNext());
        tv("empty source next", () -> sc("").next());

        // ---- typed reads, each with its success, its mismatch and its end
        tv("nextInt", () -> sc("42").nextInt());
        tv("nextInt negative", () -> sc("-7").nextInt());
        tv("nextInt plus", () -> sc("+7").nextInt());
        tv("nextInt on word", () -> sc("abc").nextInt());
        tv("nextInt on empty", () -> sc("").nextInt());
        tv("nextInt overflow", () -> sc("99999999999999999999").nextInt());
        tv("nextInt radix 16", () -> sc("ff").nextInt(16));
        tv("nextInt radix 2", () -> sc("1011").nextInt(2));
        tv("nextInt radix bad digit", () -> sc("fg").nextInt(16));
        tv("nextLong", () -> sc("9999999999").nextLong());
        tv("nextShort", () -> sc("12").nextShort());
        tv("nextShort overflow", () -> sc("99999").nextShort());
        tv("nextByte", () -> sc("12").nextByte());
        tv("nextFloat", () -> sc("1.5").nextFloat());
        tv("nextDouble", () -> sc("1.5").nextDouble());
        tv("nextDouble exponent", () -> sc("1.5e3").nextDouble());
        tv("nextDouble on word", () -> sc("abc").nextDouble());
        tv("nextBoolean true", () -> sc("true").nextBoolean());
        tv("nextBoolean TRUE", () -> sc("TRUE").nextBoolean());
        tv("nextBoolean on word", () -> sc("yes").nextBoolean());
        tv("nextBigInteger", () -> sc("123456789012345678901234567890").nextBigInteger());
        tv("nextBigDecimal", () -> sc("1.25").nextBigDecimal());
        tv("nextBigInteger radix", () -> sc("ff").nextBigInteger(16));

        // ---- the grouping separator, which is what `useLocale` is FOR
        tv("US grouped int", () -> sc("1,234").nextInt());
        tv("US grouped double", () -> sc("1,234.5").nextDouble());
        tv("no-locale grouped int", () -> new Scanner("1,234").nextInt());

        // ---- hasNextX must not consume, and must agree with nextX
        Scanner h = sc("42 abc");
        tv("hasNextInt", () -> h.hasNextInt());
        tv("hasNextInt twice", () -> h.hasNextInt());
        tv("hasNextInt then nextInt", () -> h.nextInt());
        tv("hasNextInt on word", () -> h.hasNextInt());
        tv("hasNext on word", () -> h.hasNext());
        tv("hasNextInt radix 16 on abc", () -> sc("abc").hasNextInt(16));
        tv("hasNextLong", () -> sc("9999999999").hasNextLong());
        tv("hasNextDouble", () -> sc("1.5").hasNextDouble());
        tv("hasNextBoolean", () -> sc("true").hasNextBoolean());
        tv("hasNextBigInteger", () -> sc("12").hasNextBigInteger());
        tv("hasNextShort overflow", () -> sc("99999").hasNextShort());
        tv("hasNextByte overflow", () -> sc("999").hasNextByte());

        // ---- lines
        Scanner l = sc("one\ntwo\nthree");
        tv("hasNextLine", () -> l.hasNextLine());
        tv("nextLine 1", () -> l.nextLine());
        tv("nextLine 2", () -> l.nextLine());
        tv("nextLine 3", () -> l.nextLine());
        tv("hasNextLine end", () -> l.hasNextLine());
        tv("nextLine past end", () -> l.nextLine());
        tv("nextLine after next", () -> lineAfterToken());
        tv("nextLine on empty line", () -> sc("\nx").nextLine());
        tv("nextLine crlf", () -> sc("a\r\nb").nextLine());

        // ---- delimiters
        Scanner d = new Scanner("a,b,,c").useDelimiter(",");
        tv("useDelimiter walk", () -> drain(d));
        p("useDelimiter pattern", new Scanner("a1b22c").useDelimiter(Pattern.compile("\\d+"))
                .next());
        p("delimiter after useDelimiter",
                new Scanner("x").useDelimiter(";").delimiter().pattern());
        p("useDelimiter then reset delimiter",
                new Scanner("x").useDelimiter(";").reset().delimiter().pattern());

        // ---- radix and locale state
        Scanner r = sc("10");
        tv("useRadix radix()", () -> r.useRadix(8).radix());
        tv("useRadix value", () -> r.nextInt());
        tv("reset radix", () -> sc("10").useRadix(8).reset().radix());

        // ---- pattern search
        Scanner f = sc("hello world 42");
        tv("findInLine word", () -> f.findInLine("w\\w+"));
        tv("match group after findInLine", () -> f.match().group());
        tv("next after findInLine", () -> f.next());
        tv("findInLine miss", () -> sc("abc").findInLine("z+"));
        tv("findWithinHorizon", () -> sc("abcdef").findWithinHorizon("cd", 6));
        tv("findWithinHorizon short", () -> sc("abcdef").findWithinHorizon("ef", 3));
        tv("findWithinHorizon negative", () -> sc("abc").findWithinHorizon("a", -1));
        tv("skip then next", () -> skipThenNext());
        tv("skip miss", () -> sc("abc").skip("z+"));
        tv("match without search", () -> sc("a").match());

        // ---- streams over a scanner
        tv("tokens count", () -> sc("a b c").tokens().count());
        tv("tokens joined", () -> String.join("|", sc("a b c").tokens().toList()));
        tv("findAll count", () -> sc("a1b2c3").findAll("\\d").count());

        // ---- closed-scanner behaviour: a THIRD exception type
        Scanner c = sc("a b");
        c.close();
        tv("next after close", () -> c.next());
        tv("hasNext after close", () -> c.hasNext());
        tv("nextLine after close", () -> c.nextLine());
        tv("useDelimiter after close", () -> c.useDelimiter(",").toString().isEmpty());
        tv("close twice", () -> { c.close(); return "no-throw"; });
        tv("ioException after close", () -> String.valueOf(c.ioException()));

        // ---- an InputStream source, the other constructor apps use
        Scanner is = new Scanner(new ByteArrayInputStream("7 eight".getBytes()))
                .useLocale(Locale.US);
        tv("stream nextInt", () -> is.nextInt());
        tv("stream next", () -> is.next());
        tv("stream hasNext end", () -> is.hasNext());
        tv("stream ioException", () -> String.valueOf(is.ioException()));

        // ---- a Readable source
        tv("readable walk", () -> drain(new Scanner(new StringReader("p q"))));

        System.out.println("DONE ScannerShadowSweep");
    }

    static String lineAfterToken() {
        Scanner s = sc("one two\nthree");
        s.next();
        return s.nextLine();
    }

    static String skipThenNext() {
        Scanner s = sc("xxabc");
        s.skip("x+");
        return s.next();
    }

    static String drain(Scanner s) {
        StringBuilder sb = new StringBuilder();
        while (s.hasNext()) sb.append('[').append(s.next()).append(']');
        return sb.toString();
    }
}
