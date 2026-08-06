import java.io.ByteArrayOutputStream;
import java.io.PrintStream;
import java.io.PrintWriter;
import java.io.StringWriter;
import java.nio.CharBuffer;
import java.util.Formatter;
import java.util.Locale;
import java.util.StringJoiner;
import java.util.regex.Pattern;

/**
 * `CharSequence` parameters that are not a `java.lang.String`.
 *
 * A native that reads a `CharSequence` argument with `read_string` alone sees a
 * `java.lang.String` and nothing else, so a `StringBuilder` — or a
 * `StringBuffer`, or a `CharBuffer` — silently becomes the four characters
 * `null`. That is not a corner case: `java.util.Formatter` appends a
 * `StringBuilder` for every numeric conversion and a `String` for `%s`, so a
 * single such native turns `printf(Locale.ROOT, "[%s|%d]", "x", 7)` into
 * `[x|null]` while leaving `%s` looking healthy.
 *
 * That was the whole reason `regression-suite/run.sh` reported 0 passed on
 * every class in both modes: the corpus asserts through `printf`.
 *
 * Each line passes a NON-String CharSequence and prints the resulting text, so
 * a native that drops it shows up as `null` (or as missing characters) rather
 * than as a thrown exception. Every line must match the host JDK under both
 * `cratonvm --real-jdk` and `cratonvm --jdk-only`.
 */
public final class PrintStreamAppendProbe {

    public static void main(String[] args) {
        printStream();
        printWriter();
        builders();
        otherCharSequenceSinks();
        theFormatterPath();
        System.out.println("PrintStreamAppendProbe done");
    }

    static void printStream() {
        p("ps.append.string", () -> cap(ps -> ps.append("7")));
        p("ps.append.sb", () -> cap(ps -> ps.append(new StringBuilder("7"))));
        p("ps.append.sbuf", () -> cap(ps -> ps.append(new StringBuffer("7"))));
        p("ps.append.charbuffer", () -> cap(ps -> ps.append(CharBuffer.wrap("7"))));
        p("ps.append.null", () -> cap(ps -> ps.append(null)));
        p("ps.append.range", () -> cap(ps -> ps.append(new StringBuilder("789"), 0, 2)));
        p("ps.append.chained", () -> cap(ps -> ps.append(new StringBuilder("a")).append("b")));
        p("ps.print.sb", () -> cap(ps -> ps.print(new StringBuilder("7"))));
    }

    static void printWriter() {
        p("pw.append.sb", () -> {
            StringWriter sw = new StringWriter();
            PrintWriter pw = new PrintWriter(sw);
            pw.append(new StringBuilder("7"));
            pw.flush();
            return "[" + sw + "]";
        });
        p("sw.append.sb", () -> {
            StringWriter sw = new StringWriter();
            sw.append(new StringBuilder("7"));
            return "[" + sw + "]";
        });
        p("pw.printf.locale", () -> {
            StringWriter sw = new StringWriter();
            PrintWriter pw = new PrintWriter(sw);
            pw.printf(Locale.ROOT, "[%s|%d|%05.2f]", "x", 7, 1.5);
            pw.flush();
            return sw.toString();
        });
    }

    static void builders() {
        p("sb.append.sb", () -> new StringBuilder("a").append(new StringBuilder("b")).toString());
        p("sb.append.charbuffer", () ->
                new StringBuilder("a").append(CharBuffer.wrap("b")).toString());
        p("sbuf.append.sb", () -> new StringBuffer("a").append(new StringBuilder("b")).toString());
        p("sb.append.null.cs", () -> {
            CharSequence cs = null;
            return new StringBuilder("a").append(cs).toString();
        });
    }

    static void otherCharSequenceSinks() {
        p("joiner.add.sb", () -> {
            StringJoiner j = new StringJoiner(",");
            j.add(new StringBuilder("a"));
            j.add("b");
            return j.toString();
        });
        p("String.join.sb", () -> String.join("-", new StringBuilder("a"), "b"));
        p("String.contains.sb", () -> "" + "abc".contains(new StringBuilder("bc")));
        p("String.contentEquals.sb", () -> "" + "abc".contentEquals(new StringBuilder("abc")));
        p("Pattern.matcher.sb", () -> {
            java.util.regex.Matcher m = Pattern.compile("b+").matcher(new StringBuilder("abbc"));
            return m.find() ? m.group() + "@" + m.start() : "no-match";
        });
        p("CharBuffer.wrap.sb", () -> CharBuffer.wrap(new StringBuilder("xy")).toString());
    }

    /**
     * The path the defect was found through. `printf(Locale, …)` carries no
     * native of its own, so it is the one overload that reaches real
     * `java.util.Formatter` bytecode — and `Formatter` appends a
     * `StringBuilder` for `%d` and `%f` but a `String` for `%s`.
     */
    static void theFormatterPath() {
        p("printf.locale", () -> cap(ps -> ps.printf(Locale.ROOT, "[%s|%d|%05.2f]", "x", 7, 1.5)));
        p("printf.nolocale", () -> cap(ps -> ps.printf("[%s|%d]", "x", 7)));
        p("format.locale", () -> cap(ps -> ps.format(Locale.ROOT, "[%s|%d]", "x", 7)));
        p("printf.locale.allS", () -> cap(ps -> ps.printf(Locale.ROOT, "[%s|%s]", "x", 7)));
        p("printf.locale.width", () -> cap(ps -> ps.printf(Locale.ROOT, "[%5d|%-5d|]", 7, 7)));
        p("printf.locale.hex", () -> cap(ps -> ps.printf(Locale.ROOT, "[%x|%o|%c|%b]", 255, 8, 'z', true)));
        p("formatter.into.ps", () -> cap(ps ->
                new Formatter(ps, Locale.ROOT).format("[%s|%d|%05.2f]", "x", 7, 1.5)));
    }

    interface Use {
        void run(PrintStream ps);
    }

    interface T {
        String get() throws Exception;
    }

    static String cap(Use u) throws Exception {
        ByteArrayOutputStream b = new ByteArrayOutputStream();
        PrintStream ps = new PrintStream(b, true, "UTF-8");
        u.run(ps);
        ps.flush();
        return "[" + b.toString("UTF-8") + "]";
    }

    static void p(String label, T t) {
        try {
            System.out.println(label + "=" + t.get());
        } catch (Throwable e) {
            String msg = e.getMessage();
            System.out.println(label + "=" + e.getClass().getName() + (msg == null ? "" : ": " + msg));
        }
    }
}
