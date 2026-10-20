import java.nio.ByteBuffer;
import java.nio.CharBuffer;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;

/**
 * Round nio20 wave 2, lane L3: {@code UTF_8$Encoder.encodeBufferLoop} over a {@code StringCharBuffer}
 * ({@code CharBuffer.wrap(CharSequence)}), the {@code R17NioRows utfencode} path, and the receiver
 * switch that refutes the nested trap guard of {@code CRATONVM_JIT_IR_SPLICE_NESTED_PROFILE_TRAP}
 * ({@code StringCharBuffer.get()}'s {@code str.charAt(..)} spliced for {@code String} behind an exact
 * class test whose miss edge deopts).
 *
 * <p>Bench rows print {@code <row> <N> ns/op}; conformance rows print {@code conf <fact>} lines that
 * must match HotSpot byte for byte (no timings in them). {@code -Dnio20.rows=a,b} selects rows
 * (default all), {@code -Dnio20.n=} the timed ops per row (default 20 000), {@code -Dnio20.len=}
 * the text length in chars (default 4 096).
 *
 * <ul>
 *   <li>{@code encode-string}: {@code UTF_8.encode(CharBuffer.wrap(String))}, ASCII with a 2-byte,
 *       3-byte and surrogate-pair char every 64 chars.</li>
 *   <li>{@code encode-ascii}: the same over pure ASCII (the per-char floor of the 1-byte arm).</li>
 *   <li>{@code conf}: warm the {@code String} shape, then encode a {@code StringBuilder}, a
 *       {@code CharBuffer} view of a {@code char[]}-backed {@code CharSequence}, and the
 *       {@code String} again; prints the byte length and a checksum of each result.</li>
 * </ul>
 *
 * <pre>
 *   javac -d /tmp/p20 tools/probes/Nio20EncodeLoop.java
 *   cratonvm -cp /tmp/p20 Nio20EncodeLoop                                   # arm A (default)
 *   CRATONVM_JIT_IR_SPLICE_NESTED_PROFILE_TRAP=1 cratonvm -cp /tmp/p20 Nio20EncodeLoop   # arm B
 *   CRATONVM_DBG_JITC=1 CRATONVM_JIT_IR_SPLICE_NESTED_PROFILE_TRAP=1 cratonvm -cp /tmp/p20 \
 *     Nio20EncodeLoop 2>&1 | grep -E 'nest-profile-trap|nested profile trap guard'
 *   java -cp /tmp/p20 Nio20EncodeLoop | grep '^conf'                        # the HotSpot facts
 * </pre>
 */
public class Nio20EncodeLoop {
    static final String ROWS = System.getProperty("nio20.rows", "all");
    static final int N = Integer.getInteger("nio20.n", 20_000);
    static final int LEN = Integer.getInteger("nio20.len", 4_096);
    static volatile long sink;

    static boolean on(String r) {
        return ROWS.equals("all") || Arrays.asList(ROWS.split(",")).contains(r);
    }

    static String text(boolean mixed) {
        StringBuilder sb = new StringBuilder(LEN + 2);
        for (int i = 0; sb.length() < LEN; i++) {
            if (mixed && i % 64 == 17) {
                sb.append('é');
            } else if (mixed && i % 64 == 33) {
                sb.append('€');
            } else if (mixed && i % 64 == 49) {
                sb.appendCodePoint(0x1F600);
            } else {
                sb.append((char) ('a' + i % 26));
            }
        }
        return sb.toString();
    }

    static long digest(ByteBuffer b) {
        long h = 1125899906842597L;
        for (int i = b.position(); i < b.limit(); i++) {
            h = 31 * h + b.get(i);
        }
        return h;
    }

    static long encodeOnce(CharSequence s) {
        ByteBuffer out = StandardCharsets.UTF_8.encode(CharBuffer.wrap(s));
        return out.remaining() + (out.get(0) & 0xff);
    }

    static void bench(String row, CharSequence s) {
        for (int w = 0; w < N / 2 + 2_000; w++) {
            sink += encodeOnce(s);
        }
        long t0 = System.nanoTime();
        for (int k = 0; k < N; k++) {
            sink += encodeOnce(s);
        }
        long dt = System.nanoTime() - t0;
        System.out.println(row + " " + (dt / N) + " ns/op");
    }

    /** A CharSequence that is neither String nor StringBuilder: a third receiver class. */
    static final class Chars implements CharSequence {
        final char[] a;

        Chars(String s) {
            a = s.toCharArray();
        }

        public int length() {
            return a.length;
        }

        public char charAt(int i) {
            return a[i];
        }

        public CharSequence subSequence(int s, int e) {
            return new String(a, s, e - s);
        }

        public String toString() {
            return new String(a);
        }
    }

    static void fact(String name, CharSequence s) {
        ByteBuffer out = StandardCharsets.UTF_8.encode(CharBuffer.wrap(s));
        System.out.println("conf " + name + " len=" + out.remaining() + " digest=" + digest(out));
    }

    public static void main(String[] args) {
        String mixed = text(true);
        String ascii = text(false);
        if (on("encode-string")) {
            bench("encode-string", mixed);
        }
        if (on("encode-ascii")) {
            bench("encode-ascii", ascii);
        }
        if (on("conf")) {
            // Warm the String shape long enough to compile the loop for it, then
            // hand it receivers its profile never saw.
            for (int w = 0; w < 30_000; w++) {
                sink += encodeOnce(ascii);
            }
            fact("string", mixed);
            fact("builder", new StringBuilder(mixed));
            for (int w = 0; w < 2_000; w++) {
                sink += encodeOnce(new StringBuilder(ascii));
            }
            fact("chars", new Chars(mixed));
            fact("string-again", mixed);
            fact("empty", "");
            fact("lone-surrogate-tail", "ab\ud83d");
        }
    }
}
