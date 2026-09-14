import java.io.*;
import java.nio.charset.StandardCharsets;
import java.util.*;

/**
 * `java.io.PrintStream` (30 owned §1.4 shadow rows) and `java.io.PrintWriter`
 * (7) — the second and fourth largest classes in the `java.io` census
 * (`WORKER-4-2` §1.1).
 *
 * Everything is written to a `ByteArrayOutputStream` and the BYTES are printed
 * back, so this checks what was actually emitted rather than that the call
 * returned. The corpus asks nothing about the content of a built string
 * (`WORKER-4` trap 5), and a print stream is nothing but content.
 */
public class W4Print {

    static void ck(String tag, Object got) { System.out.println("CK " + tag + " " + got); }

    /** Render bytes unambiguously: newlines and non-ASCII shown as escapes. */
    static String show(ByteArrayOutputStream bo) {
        String s = new String(bo.toByteArray(), StandardCharsets.UTF_8);
        StringBuilder sb = new StringBuilder("\"");
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c == '\n') sb.append("\\n");
            else if (c == '\r') sb.append("\\r");
            else if (c == '\t') sb.append("\\t");
            else if (c < 0x20 || c > 0x7e) sb.append(String.format("\\u%04x", (int) c));
            else sb.append(c);
        }
        return sb.append('"').toString();
    }

    interface Emit { void run(PrintStream p) throws Exception; }

    static void ps(String tag, Emit e) {
        ByteArrayOutputStream bo = new ByteArrayOutputStream();
        try (PrintStream p = new PrintStream(bo, true, StandardCharsets.UTF_8)) {
            e.run(p);
            p.flush();
            ck(tag, show(bo) + " err=" + p.checkError());
        } catch (Throwable t) {
            ck(tag, "threw:" + t.getClass().getName());
        }
    }

    interface EmitW { void run(PrintWriter p) throws Exception; }

    static void pw(String tag, EmitW e) {
        StringWriter sw = new StringWriter();
        try (PrintWriter p = new PrintWriter(sw)) {
            e.run(p);
            p.flush();
            ByteArrayOutputStream bo = new ByteArrayOutputStream();
            bo.writeBytes(sw.toString().getBytes(StandardCharsets.UTF_8));
            ck(tag, show(bo) + " err=" + p.checkError());
        } catch (Throwable t) {
            ck(tag, "threw:" + t.getClass().getName());
        }
    }

    public static void main(String[] args) throws Exception {
        // ---- PrintStream.print over every overload ----------------------
        ps("ps.print.String", p -> p.print("abc"));
        ps("ps.print.nullString", p -> p.print((String) null));
        ps("ps.print.char", p -> p.print('Z'));
        ps("ps.print.int", p -> p.print(-42));
        ps("ps.print.long", p -> p.print(Long.MIN_VALUE));
        ps("ps.print.float", p -> p.print(1.5f));
        ps("ps.print.double", p -> p.print(-0.0d));
        ps("ps.print.doubleNaN", p -> p.print(Double.NaN));
        ps("ps.print.doubleInf", p -> p.print(Double.POSITIVE_INFINITY));
        ps("ps.print.boolean", p -> p.print(true));
        ps("ps.print.charArray", p -> p.print(new char[] {'x', 'y'}));
        ps("ps.print.Object", p -> p.print(Arrays.asList(1, 2)));
        ps("ps.print.nullObject", p -> p.print((Object) null));

        // ---- println, including the separator ---------------------------
        ps("ps.println.noArg", p -> p.println());
        ps("ps.println.String", p -> p.println("abc"));
        ps("ps.println.nullString", p -> p.println((String) null));
        ps("ps.println.int", p -> p.println(7));
        ps("ps.println.charArray", p -> p.println(new char[] {'q'}));
        ps("ps.println.Object", p -> p.println((Object) "o"));
        ps("ps.println.twice", p -> { p.println("a"); p.println("b"); });

        // ---- write / append ---------------------------------------------
        ps("ps.write.int", p -> p.write(65));
        ps("ps.write.bytes", p -> p.write("hi".getBytes(StandardCharsets.UTF_8), 0, 2));
        ps("ps.append.char", p -> p.append('c'));
        ps("ps.append.CharSequence", p -> p.append("seq"));
        ps("ps.append.nullCharSequence", p -> p.append(null));
        ps("ps.append.subSequence", p -> p.append("abcdef", 1, 4));
        ps("ps.append.chained", p -> p.append('a').append("bc").append('d'));

        // ---- format / printf ---------------------------------------------
        ps("ps.printf", p -> p.printf("%s-%d-%05.2f", "s", 3, 1.5));
        ps("ps.format.pct", p -> p.format("100%%"));
        ps("ps.printf.newline", p -> p.printf("a%nb"));
        ps("ps.printf.hex", p -> p.printf("%x|%X|%08x", 255, 255, 255));
        ps("ps.printf.locale", p -> p.printf(Locale.US, "%,d", 1234567));
        ps("ps.printf.width", p -> p.printf("[%10s][%-10s]", "r", "l"));
        ps("ps.printf.badFormat", p -> p.printf("%d", "not a number"));

        // ---- non-ASCII through the stream's charset ------------------------
        ps("ps.print.unicode", p -> p.print("café ✓ 日本"));
        ps("ps.println.unicode", p -> p.println("naïve"));
        ps("ps.print.surrogatePair", p -> p.print("😀"));

        // ---- checkError and the swallowed-IOException contract -------------
        ByteArrayOutputStream bo = new ByteArrayOutputStream();
        PrintStream broken = new PrintStream(new OutputStream() {
            @Override public void write(int b) throws IOException {
                throw new IOException("nope");
            }
        }, true);
        broken.print("x");
        ck("ps.checkError.afterFailure", broken.checkError());
        broken.println("y");
        ck("ps.checkError.stillSet", broken.checkError());
        ck("ps.emptyStream.checkError", new PrintStream(bo).checkError());

        // ---- PrintWriter ---------------------------------------------------
        pw("pw.print.String", p -> p.print("abc"));
        pw("pw.print.nullString", p -> p.print((String) null));
        pw("pw.println.String", p -> p.println("abc"));
        pw("pw.println.noArg", p -> p.println());
        pw("pw.print.int", p -> p.print(-1));
        pw("pw.print.charArray", p -> p.print(new char[] {'m', 'n'}));
        pw("pw.write.String", p -> p.write("wr"));
        pw("pw.write.substring", p -> p.write("abcdef", 1, 3));
        pw("pw.append.chained", p -> p.append('a').append("bc"));
        pw("pw.printf", p -> p.printf("%s=%d", "k", 9));
        pw("pw.format.locale", p -> p.format(Locale.US, "%,.2f", 1234.5));
        pw("pw.print.unicode", p -> p.print("café"));

        // ---- PrintWriter over a PrintStream, and autoflush ------------------
        ByteArrayOutputStream bo2 = new ByteArrayOutputStream();
        PrintWriter over = new PrintWriter(new PrintStream(bo2, true, StandardCharsets.UTF_8), true);
        over.println("layered");
        over.flush();
        ck("pw.overPrintStream", show(bo2));

        ByteArrayOutputStream bo3 = new ByteArrayOutputStream();
        PrintStream noFlush = new PrintStream(bo3, false, StandardCharsets.UTF_8);
        noFlush.println("buffered");
        ck("ps.noAutoflush.beforeFlush", show(bo3));
        noFlush.flush();
        ck("ps.noAutoflush.afterFlush", show(bo3));

        System.out.println("PASS W4Print");
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
