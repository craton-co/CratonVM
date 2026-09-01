import java.io.*;
import java.nio.*;

/** L3 — `Readable.read(CharBuffer)`, the loop a retired `Scanner` runs on.
 *
 *  `Scanner.findPatternInBuffer` returns null and sets `needInput` whenever the
 *  match hit the end of the buffer and the source is not closed; the caller then
 *  calls `readInput()` and retries. If `read(CharBuffer)` never reports EOF, or
 *  never advances the buffer, that retry loop cannot terminate in a match — and
 *  `nextLine`, `findInLine`, `findWithinHorizon` and `match` are exactly the four
 *  Scanner methods built on it. They are also exactly the eleven rows that break
 *  when Scanner's 43 shadows are retired.
 *
 *  So this asks the primitive directly. Every row prints the RETURN VALUE and the
 *  buffer's position/limit after the call, because "did it read" and "how far did
 *  it advance" are different questions and the loop depends on both.
 */
public class ReadableProbe {
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

    static String state(int n, CharBuffer b) {
        return "read=" + n + " pos=" + b.position() + " lim=" + b.limit()
                + " content=[" + esc(contentOf(b)) + "]";
    }

    /** What has been written into the buffer so far, without disturbing it. */
    static String contentOf(CharBuffer b) {
        CharBuffer d = b.duplicate();
        d.flip();
        return d.toString();
    }

    /** One full drain, exactly as `Scanner.readInput` drives it. */
    static String drain(Readable r) throws Exception {
        CharBuffer buf = CharBuffer.allocate(8);
        StringBuilder sb = new StringBuilder();
        int guard = 0;
        while (guard++ < 40) {
            buf.clear();
            int n = r.read(buf);
            sb.append('<').append(n).append('>');
            if (n < 0) break;
            if (n == 0) { sb.append("STALL"); break; }
            buf.flip();
            sb.append(esc(buf.toString()));
        }
        if (guard >= 40) sb.append("RUNAWAY");
        return sb.toString();
    }

    public static void main(String[] args) throws Exception {
        // ---- StringReader, which is what `new Scanner(String)` wraps.
        tv("StringReader single read", () -> {
            CharBuffer b = CharBuffer.allocate(16);
            int n = new StringReader("hello").read(b);
            return state(n, b);
        });
        tv("StringReader read at EOF", () -> {
            StringReader r = new StringReader("hi");
            CharBuffer b = CharBuffer.allocate(16);
            int first = r.read(b);
            b.clear();
            int second = r.read(b);
            return "first=" + first + " second=" + second;
        });
        tv("StringReader short buffer", () -> {
            CharBuffer b = CharBuffer.allocate(3);
            int n = new StringReader("abcdef").read(b);
            return state(n, b);
        });
        tv("StringReader full buffer then more", () -> drain(new StringReader("abcdefghijkl")));
        tv("StringReader empty source", () -> drain(new StringReader("")));
        tv("StringReader zero-capacity buffer", () -> {
            CharBuffer b = CharBuffer.allocate(0);
            return "read=" + new StringReader("abc").read(b);
        });
        tv("StringReader with newlines", () -> drain(new StringReader("one\ntwo\nthree")));

        // ---- InputStreamReader, which `new Scanner(InputStream)` wraps.
        tv("InputStreamReader drain", () -> drain(new InputStreamReader(
                new ByteArrayInputStream("alpha beta".getBytes()))));
        tv("InputStreamReader empty", () -> drain(new InputStreamReader(
                new ByteArrayInputStream(new byte[0]))));

        // ---- BufferedReader and CharArrayReader, the other Readables.
        tv("BufferedReader drain", () -> drain(new BufferedReader(new StringReader("x y z"))));
        tv("CharArrayReader drain", () -> drain(new CharArrayReader("abc".toCharArray())));

        // ---- CharBuffer is itself Readable, and Scanner accepts one.
        tv("CharBuffer as Readable", () -> drain(CharBuffer.wrap("buffered source")));

        // ---- A hand-rolled Readable, so no JDK class can be shadowing the answer.
        tv("custom Readable drain", () -> drain(new Readable() {
            private final String s = "custom text here";
            private int i = 0;
            public int read(CharBuffer cb) {
                if (i >= s.length()) return -1;
                int n = Math.min(cb.remaining(), s.length() - i);
                cb.put(s, i, i + n);
                i += n;
                return n;
            }
        }));

        // ---- Reader.read(char[]) beside it: the same source, the other primitive,
        // so a divergence can be attributed to the CharBuffer overload or not.
        tv("StringReader read(char[])", () -> {
            char[] c = new char[8];
            int n = new StringReader("hello").read(c, 0, 8);
            return "read=" + n + " [" + new String(c, 0, Math.max(n, 0)) + "]";
        });
        tv("StringReader read()", () -> {
            StringReader r = new StringReader("hi");
            return "" + (char) r.read() + (char) r.read() + " eof=" + r.read();
        });

        System.out.println("DONE ReadableProbe");
    }
}
