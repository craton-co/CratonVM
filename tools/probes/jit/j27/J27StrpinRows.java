// j27 wave 5 (lane STRPIN): the small-JDK-call rows of the CharsetDecoder page in
// CHUNKED methods (each row is a method called many times over a short loop, so the
// timed code is a normal method-entry body, not an OSR body), plus the same rows in
// one method that also holds a StringBuilder site (the shape DecLoop.main has: the
// String-intrinsic pin keeps such a method on the single-pass tier).
// Pages: docs/known-issues/jit/small-jdk-method-call-overhead-charsetdecoder-decode-...-20261025.md
//        docs/known-issues/jit/jr21-strintr-string-pin-residual-decision-20261102.md
//
//   java -cp <dir> J27StrpinRows                     (facts only)
//   java -Dj27.timing=true -cp <dir> J27StrpinRows   (adds `time <row> <ns/op>` lines)
//   -Dj27.rows=dec,setters,...   selects timing rows; -Dj27.secs=N loops the selected
//   rows for about N seconds (for a sampler), printing nothing extra.
//
// Expected HotSpot 25 output (facts only):
//   fact dec 22200000
//   fact wrap 600000
//   fact setters 4950000
//   fact bbsetters 750000
//   fact pinned-dec 22200000
//   fact pinned-setters 4950000
//   fact sb 2550000
// HotSpot 25 timing on the Windows box (P-cores): dec 8.0-8.7, wrap 1.3, setters 1.2,
// bbsetters 0.4, pinned-dec 8.0, pinned-setters 1.2, sb 4.6 ns/op.
import java.nio.ByteBuffer;
import java.nio.CharBuffer;
import java.nio.charset.CharacterCodingException;
import java.nio.charset.CharsetDecoder;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;

public class J27StrpinRows {
    static final byte[] IN = {'G', 'E', 'T'};
    static final CharsetDecoder DEC = StandardCharsets.ISO_8859_1.newDecoder();
    static final ByteBuffer HELD = ByteBuffer.allocate(16);
    static final CharBuffer CHELD = CharBuffer.allocate(16);
    static final int CHUNK = 1000;
    static Object osink;

    // ---- chunk bodies (the timed code) ----
    static long decChunk(int from) throws CharacterCodingException {
        long s = 0;
        for (int i = from; i < from + CHUNK; i++) {
            DEC.reset();
            CharBuffer out = DEC.decode(ByteBuffer.wrap(IN, 0, 3));
            s += out.charAt(0) + out.remaining();
        }
        return s;
    }

    static long wrapChunk(int from) {
        long s = 0;
        for (int i = from; i < from + CHUNK; i++) {
            ByteBuffer b = ByteBuffer.wrap(IN, i % 3, 0);
            s += b.position() + b.limit();
        }
        return s;
    }

    static long settersChunk(int from) {
        ByteBuffer b = HELD;
        CharBuffer c = CHELD;
        long s = 0;
        for (int i = from; i < from + CHUNK; i++) {
            b.position(i & 7);
            b.limit(8 + (i & 7));
            c.position(i & 7);
            c.limit(8 + (i & 7));
            s += b.remaining() + c.remaining() + (i & 1);
        }
        return s;
    }

    static long bbSettersChunk(int from) {
        ByteBuffer b = HELD;
        long s = 0;
        for (int i = from; i < from + CHUNK; i++) {
            b.limit(16).position(i & 3);
            s += b.position() + 1;
        }
        return s;
    }

    // The same two rows with a StringBuilder site in the method: the pin shape.
    static long pinnedChunk(int from, boolean dec) throws CharacterCodingException {
        long s = 0;
        if (from < 0) {
            s += new StringBuilder("x").append(from).toString().length();
        }
        for (int i = from; i < from + CHUNK; i++) {
            if (dec) {
                DEC.reset();
                CharBuffer out = DEC.decode(ByteBuffer.wrap(IN, 0, 3));
                s += out.charAt(0) + out.remaining();
            } else {
                HELD.position(i & 7);
                HELD.limit(8 + (i & 7));
                CHELD.position(i & 7);
                CHELD.limit(8 + (i & 7));
                s += HELD.remaining() + CHELD.remaining() + (i & 1);
            }
        }
        return s;
    }

    static long sbChunk(int from) {
        long s = 0;
        for (int i = from; i < from + CHUNK; i++) {
            String t = new StringBuilder("abc").append(i % 10).toString();
            s += t.length() + (t.charAt(3) - '0');
        }
        return s;
    }

    static long row(String name, int n) throws CharacterCodingException {
        long s = 0;
        for (int c = 0; c < n; c += CHUNK) {
            switch (name) {
                case "dec": s += decChunk(c); break;
                case "wrap": s += wrapChunk(c); break;
                case "setters": s += settersChunk(c); break;
                case "bbsetters": s += bbSettersChunk(c); break;
                case "pinned-dec": s += pinnedChunk(c, true); break;
                case "pinned-setters": s += pinnedChunk(c, false); break;
                case "sb": s += sbChunk(c); break;
                default: throw new IllegalArgumentException(name);
            }
        }
        return s;
    }

    static final String[] ROWS = {"dec", "wrap", "setters", "bbsetters", "pinned-dec", "pinned-setters", "sb"};

    public static void main(String[] args) throws Exception {
        int warm = 300_000;
        for (String r : ROWS) {
            System.out.println("fact " + r + " " + row(r, warm));
        }
        if (!Boolean.getBoolean("j27.timing")) {
            return;
        }
        String sel = System.getProperty("j27.rows", "all");
        String[] rows = sel.equals("all") ? ROWS : sel.split(",");
        int secs = Integer.getInteger("j27.secs", 0);
        if (secs > 0) {
            long end = System.nanoTime() + secs * 1_000_000_000L;
            while (System.nanoTime() < end) {
                for (String r : rows) osink = row(r, 1_000_000);
            }
            return;
        }
        final int n = 2_000_000;
        for (String r : rows) {
            long[] ns = new long[5];
            for (int rep = 0; rep < ns.length; rep++) {
                long t0 = System.nanoTime();
                osink = row(r, n);
                ns[rep] = System.nanoTime() - t0;
            }
            Arrays.sort(ns);
            System.out.printf("time %s %.1f ns/op%n", r, ns[2] / (double) n);
        }
    }
}
