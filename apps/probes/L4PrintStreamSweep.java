import java.io.*;
import java.nio.charset.*;
import java.util.*;

/** L4 -- `java.io.PrintStream` and `PrintWriter`, asked at their CONTRACT EDGES.
 *
 *  `PrintStream` is the one class in this campaign whose contract runs the
 *  OTHER way from every other family: it must **not** throw. It swallows every
 *  `IOException` from the stream underneath and records the fact in a flag, so
 *  `checkError()` is the observable and "threw" is the defect. A shim that lets
 *  the underlying `IOException` out is wrong even though it is louder and looks
 *  more careful.
 *
 *  Three things it must NOT swallow, and they are the rows a from-memory
 *  implementation folds into the flag:
 *
 *    * a `null` `char[]` is an NPE -- while a `null` `String` prints "null";
 *    * a bad format string is an `IllegalFormatException` subtype, thrown;
 *    * `checkError()` FLUSHES first, so a buffered failure surfaces on the ask
 *      rather than on the next write.
 *
 *  Every stream here writes into a `ByteArrayOutputStream` whose bytes are
 *  printed as hex-escaped text, so the diff is of what was written and not of
 *  either VM's console encoding.
 */
public class L4PrintStreamSweep {
    static int rows = 0;
    static String esc(String s) {
        if (s == null) return "null";
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c == '\n') b.append("\\n");
            else if (c == '\r') b.append("\\r");
            else if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }
    static void p(String tag, Object v) {
        rows++;
        System.out.println(esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }
    static void t(String tag, ThrowingRun r) {
        rows++;
        try { r.run(); System.out.println(esc(tag) + " |no-throw|"); }
        catch (Throwable e) { System.out.println(esc(tag) + " |THREW " + e.getClass().getName() + "|"); }
    }
    interface ThrowingRun { void run() throws Throwable; }

    /** A sink that fails on demand, so the swallow-and-flag contract can be
     *  asked rather than assumed. */
    static class Failing extends OutputStream {
        boolean fail;
        int closed = 0, flushed = 0;
        public void write(int b) throws IOException { if (fail) throw new IOException("boom"); }
        public void write(byte[] b, int o, int l) throws IOException { if (fail) throw new IOException("boom"); }
        public void flush() throws IOException { flushed++; if (fail) throw new IOException("boom"); }
        public void close() throws IOException { closed++; if (fail) throw new IOException("boom"); }
    }

    static ByteArrayOutputStream sink;
    static PrintStream ps;

    static void fresh() {
        sink = new ByteArrayOutputStream();
        ps = new PrintStream(sink, false, StandardCharsets.UTF_8);
    }
    static String drain() {
        ps.flush();
        String s = new String(sink.toByteArray(), StandardCharsets.UTF_8);
        sink.reset();
        return s;
    }
    static void one(String tag, ThrowingRun r) {
        fresh();
        rows++;
        try { r.run(); System.out.println(esc(tag) + " |" + esc(drain()) + "|"); }
        catch (Throwable e) { System.out.println(esc(tag) + " |THREW " + e.getClass().getName() + "|"); }
    }

    // ------------------------------------------------------------ conversion

    static void conversion() {
        one("print(true)", () -> ps.print(true));
        one("print(false)", () -> ps.print(false));
        one("print('c')", () -> ps.print('c'));
        one("print(non-ascii char)", () -> ps.print('é'));
        one("print(int)", () -> ps.print(-42));
        one("print(Integer.MIN_VALUE)", () -> ps.print(Integer.MIN_VALUE));
        one("print(long)", () -> ps.print(Long.MIN_VALUE));
        one("print(float 1.0)", () -> ps.print(1.0f));
        one("print(float NaN)", () -> ps.print(Float.NaN));
        one("print(float Inf)", () -> ps.print(Float.POSITIVE_INFINITY));
        one("print(float -0.0)", () -> ps.print(-0.0f));
        one("print(double 1.0)", () -> ps.print(1.0));
        one("print(double 1e20)", () -> ps.print(1e20));
        one("print(double 1e-7)", () -> ps.print(1e-7));
        one("print(double NaN)", () -> ps.print(Double.NaN));
        one("print(double -0.0)", () -> ps.print(-0.0));
        one("print(String)", () -> ps.print("abc"));
        one("print((String)null)", () -> ps.print((String) null));
        one("print((Object)null)", () -> ps.print((Object) null));
        one("print(Object)", () -> ps.print((Object) Integer.valueOf(7)));
        one("print(char[])", () -> ps.print(new char[]{'x', 'y'}));
        // A null char[] is an NPE. A null String is "null". The pair is the
        // row a shim most often collapses.
        one("print((char[])null)", () -> ps.print((char[]) null));
        one("println()", () -> ps.println());
        one("println(String)", () -> ps.println("abc"));
        one("println((String)null)", () -> ps.println((String) null));
        one("println((Object)null)", () -> ps.println((Object) null));
        one("println((char[])null)", () -> ps.println((char[]) null));
        one("println(char[])", () -> ps.println(new char[]{'x'}));
        one("println(int)", () -> ps.println(1));
        one("println(long)", () -> ps.println(2L));
        one("println(boolean)", () -> ps.println(true));
        one("println(char)", () -> ps.println('z'));
        one("println(float)", () -> ps.println(1.5f));
        one("println(double)", () -> ps.println(2.5));
        // A toString() that answers null: the JDK prints "null", it does not
        // NPE inside String.valueOf.
        one("print(Object with null toString)", () -> ps.print(new Object() { public String toString() { return null; } }));
        one("println(Object with null toString)", () -> ps.println(new Object() { public String toString() { return null; } }));
        // append: a null CharSequence prints "null"; a subrange is honoured.
        one("append(char)", () -> ps.append('a'));
        one("append(CharSequence)", () -> ps.append("bc"));
        one("append((CharSequence)null)", () -> ps.append((CharSequence) null));
        one("append(cs,1,3)", () -> ps.append("abcd", 1, 3));
        one("append(null,1,3)", () -> ps.append((CharSequence) null, 1, 3));
        one("append(cs,3,1) reversed", () -> ps.append("abcd", 3, 1));
        one("append(cs,0,9) past end", () -> ps.append("abcd", 0, 9));
        one("append(cs,-1,2)", () -> ps.append("abcd", -1, 2));
        // write: the int form takes the LOW EIGHT BITS and is not encoded.
        one("write(65)", () -> ps.write(65));
        one("write(0x1FF)", () -> ps.write(0x1FF));
        one("write(-1)", () -> ps.write(-1));
        one("write(byte[])", () -> ps.write(new byte[]{66, 67}));
        one("write(byte[],0,2)", () -> ps.write(new byte[]{68, 69, 70}, 0, 2));
        one("write(byte[],1,2)", () -> ps.write(new byte[]{68, 69, 70}, 1, 2));
        one("write(byte[],-1,1)", () -> ps.write(new byte[]{1}, -1, 1));
        one("write(byte[],0,-1)", () -> ps.write(new byte[]{1}, 0, -1));
        one("write(byte[],0,2) past end", () -> ps.write(new byte[]{1}, 0, 2));
        one("write(null,0,1)", () -> ps.write((byte[]) null, 0, 1));
        one("write((byte[])null)", () -> ps.write((byte[]) null));
        // Non-ASCII goes through the stream's CHARSET, so the bytes differ
        // between a UTF-8 and an ISO-8859-1 stream. Ask both.
        rows++;
        ByteArrayOutputStream b1 = new ByteArrayOutputStream();
        PrintStream u8 = new PrintStream(b1, true, StandardCharsets.UTF_8);
        u8.print("é中");
        u8.flush();
        StringBuilder h1 = new StringBuilder();
        for (byte x : b1.toByteArray()) h1.append(String.format("%02x", x));
        System.out.println("utf8 bytes |" + h1 + "|");
        rows++;
        ByteArrayOutputStream b2 = new ByteArrayOutputStream();
        PrintStream l1 = new PrintStream(b2, true, StandardCharsets.ISO_8859_1);
        l1.print("é中");
        l1.flush();
        StringBuilder h2 = new StringBuilder();
        for (byte x : b2.toByteArray()) h2.append(String.format("%02x", x));
        System.out.println("latin1 bytes |" + h2 + "|");
    }

    // -------------------------------------------------------------- format

    static void formats() {
        one("format simple", () -> ps.format("%d-%s", 1, "x"));
        one("printf simple", () -> ps.printf("%s", "y"));
        one("printf with locale", () -> ps.printf(Locale.ROOT, "%,d", 1234567));
        one("printf null locale", () -> ps.printf((Locale) null, "%d", 1));
        one("format %n", () -> ps.format("a%nb"));
        one("format %%", () -> ps.format("100%%"));
        one("format null arg", () -> ps.format("%s", (Object) null));
        one("format null args array", () -> ps.format("%s", (Object[]) null));
        one("format no args needed", () -> ps.format("plain"));
        one("format width", () -> ps.format("[%5d]", 42));
        one("format left", () -> ps.format("[%-5d]", 42));
        one("format hex", () -> ps.format("%x %X %o", 255, 255, 8));
        one("format float", () -> ps.format(Locale.ROOT, "%.3f %e %g", 1.5, 1.5, 1.5));
        one("format boolean and char", () -> ps.format("%b %c", true, 'z'));
        // Bad format strings throw SPECIFIC IllegalFormatException subtypes.
        one("format missing arg", () -> ps.format("%d %d", 1));
        one("format unknown conversion", () -> ps.format("%q", 1));
        one("format wrong type", () -> ps.format("%d", "x"));
        one("format bad flags", () -> ps.format("%-d", 1));
        one("format bad index", () -> ps.format("%0$s", "a"));
        one("format trailing percent", () -> ps.format("100%"));
        one("format precision on int", () -> ps.format("%.2d", 1));
        one("format(null)", () -> ps.format((String) null, 1));
        one("format extra args ignored", () -> ps.format("%s", "a", "b"));
        one("format positional", () -> ps.format("%2$s%1$s", "a", "b"));
    }

    // ------------------------------------------------- the error-flag contract

    static void errorFlag() throws Exception {
        Failing f = new Failing();
        PrintStream e = new PrintStream(f, false);
        p("checkError clean", e.checkError());
        f.fail = true;
        // A failing write must NOT throw -- it sets the flag.
        t("print on failing stream", () -> e.print("x"));
        p("checkError after failure", e.checkError());
        // Once set, the flag stays set even after the sink recovers.
        f.fail = false;
        e.print("y");
        p("checkError stays set", e.checkError());
        // checkError FLUSHES first: a buffered failure surfaces on the ask.
        Failing f2 = new Failing();
        PrintStream e2 = new PrintStream(new BufferedOutputStream(f2, 4096), false);
        e2.print("buffered");
        f2.fail = true;
        p("checkError flushes", e2.checkError());
        // A closed stream sets the flag on the next write, and does not throw.
        Failing f3 = new Failing();
        PrintStream e3 = new PrintStream(f3, false);
        e3.close();
        p("checkError after close", e3.checkError());
        t("print after close", () -> e3.print("z"));
        p("checkError after print-on-closed", e3.checkError());
        t("close twice", () -> e3.close());
        p("underlying closed count", f3.closed);
        t("flush after close", () -> e3.flush());
        // A failing close sets the flag rather than throwing.
        Failing f4 = new Failing();
        PrintStream e4 = new PrintStream(f4, false);
        f4.fail = true;
        t("close with failing sink", () -> e4.close());
        p("checkError after failing close", e4.checkError());
        // Autoflush: println flushes, print does not.
        Failing f5 = new Failing();
        PrintStream e5 = new PrintStream(f5, true);
        e5.print("a");
        p("autoflush print flush count", f5.flushed);
        e5.println("b");
        p("autoflush println flush count", f5.flushed > 0);
        Failing f6 = new Failing();
        PrintStream e6 = new PrintStream(f6, true);
        e6.write('\n');
        p("autoflush write newline flushed", f6.flushed > 0);
        // An NPE from a null char[] must still escape -- it is not an IO error.
        Failing f7 = new Failing();
        PrintStream e7 = new PrintStream(f7, false);
        t("null char[] still throws", () -> e7.print((char[]) null));
        p("checkError unaffected by NPE", e7.checkError());
    }

    // ------------------------------------------------------ construction

    static void construction() throws Exception {
        t("PrintStream((OutputStream)null)", () -> new PrintStream((OutputStream) null));
        t("PrintStream(null,true)", () -> new PrintStream((OutputStream) null, true));
        t("PrintStream(os,true,(String)null)", () -> new PrintStream(new ByteArrayOutputStream(), true, (String) null));
        t("PrintStream(os,true,(Charset)null)", () -> new PrintStream(new ByteArrayOutputStream(), true, (Charset) null));
        t("PrintStream(os,true,bogus charset)", () -> new PrintStream(new ByteArrayOutputStream(), true, "no-such-charset"));
        t("PrintStream((String)null)", () -> new PrintStream((String) null));
        t("PrintStream((File)null)", () -> new PrintStream((File) null));
        t("PrintStream(missing dir path)", () -> new PrintStream("l4ps-nope/x.txt"));
        File tf = new File("l4ps-out.txt");
        try {
            PrintStream fs = new PrintStream(tf);
            fs.print("file");
            fs.close();
            p("file stream wrote", tf.length());
            PrintStream fs2 = new PrintStream(tf.getPath(), "UTF-8");
            fs2.print("x");
            fs2.close();
            p("named stream truncates", tf.length());
        } finally { tf.delete(); }
        // charset() reports what was configured.
        p("charset of UTF-8 stream", new PrintStream(new ByteArrayOutputStream(), true, StandardCharsets.UTF_8).charset());
        p("charset of latin1 stream", new PrintStream(new ByteArrayOutputStream(), true, StandardCharsets.ISO_8859_1).charset());
        p("charset by name", new PrintStream(new ByteArrayOutputStream(), true, "US-ASCII").charset());
        // Unmappable characters are replaced, not thrown, on a narrow charset.
        ByteArrayOutputStream ab = new ByteArrayOutputStream();
        PrintStream ascii = new PrintStream(ab, true, StandardCharsets.US_ASCII);
        ascii.print("a中b");
        ascii.flush();
        StringBuilder h = new StringBuilder();
        for (byte x : ab.toByteArray()) h.append(String.format("%02x", x));
        p("ascii unmappable bytes", h.toString());
        p("ascii checkError", ascii.checkError());
    }

    // -------------------------------------------------------- PrintWriter

    static void printWriter() {
        StringWriter w = new StringWriter();
        PrintWriter pw = new PrintWriter(w);
        pw.print("a");
        pw.println(1);
        pw.printf("%s-%d", "x", 2);
        pw.flush();
        p("PrintWriter output", w.toString());
        p("PrintWriter checkError", pw.checkError());
        pw.close();
        pw.print("after close");
        p("PrintWriter checkError after close", pw.checkError());
        StringWriter w2 = new StringWriter();
        PrintWriter pw2 = new PrintWriter(w2);
        t("PrintWriter print((char[])null)", () -> pw2.print((char[]) null));
        pw2.print((String) null);
        pw2.print((Object) null);
        pw2.append((CharSequence) null);
        pw2.flush();
        p("PrintWriter nulls", w2.toString());
        t("PrintWriter((Writer)null)", () -> new PrintWriter((Writer) null));
        t("PrintWriter format missing arg", () -> { PrintWriter q = new PrintWriter(new StringWriter()); q.format("%d %d", 1); });
        // A PrintWriter over a PrintStream shares the flag ONLY through IO.
        StringWriter w3 = new StringWriter();
        PrintWriter pw3 = new PrintWriter(w3, true);
        pw3.println("auto");
        p("PrintWriter autoflush", w3.toString());
        pw3.write(65);
        pw3.write("bc");
        pw3.write(new char[]{'d'});
        pw3.write("efgh", 1, 2);
        pw3.flush();
        p("PrintWriter writes", w3.toString());
    }

    public static void main(String[] a) throws Exception {
        conversion();
        formats();
        errorFlag();
        construction();
        printWriter();
        System.out.println("rows " + rows);
        System.out.println("DONE L4PrintStreamSweep");
    }
}
