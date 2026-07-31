import java.nio.ByteBuffer;
import java.nio.CharBuffer;
import java.nio.charset.Charset;
import java.nio.charset.StandardCharsets;

/**
 * Is `Charset.decode`'s cost per CALL or per BYTE?
 *
 * `native_charset_decode_bytebuf` -> `decode_with_charset` reads the charset
 * object's `name` String out of the heap, converts it to a Rust String,
 * `normalize_charset_name`s it (a second allocation) and then dispatches into
 * the transcoding engine BY NAME -- on every single call, regardless of
 * payload. If that is the dominant term, decoding 3 bytes and decoding 3000
 * costs about the same, and the fix is to memoise the resolved charset on the
 * `Charset` object identity rather than to speed up transcoding.
 *
 * Everything is measured in ONE process so the payload sizes share whatever
 * host load is present; compare rows within a column, not across VMs.
 */
public final class CharsetDecodeShapeProbe {

    private static final Charset UTF8 = StandardCharsets.UTF_8;
    private static long sink;

    private static byte[] payload(int n) {
        byte[] b = new byte[n];
        for (int i = 0; i < n; i++) {
            b[i] = (byte) ('a' + (i % 26));
        }
        return b;
    }

    private static void decodeLoop(byte[] b, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            CharBuffer cb = UTF8.decode(ByteBuffer.wrap(b, 0, b.length));
            s += cb.length();
        }
        sink += s;
    }

    /** Control: same allocation shape, no charset involved. */
    private static void wrapOnly(byte[] b, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += ByteBuffer.wrap(b, 0, b.length).remaining();
        }
        sink += s;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 100_000;
        int[] sizes = {3, 30, 300, 3000};

        // Warm every path first so the numbers below are steady-state.
        for (int size : sizes) {
            decodeLoop(payload(size), 20_000);
            wrapOnly(payload(size), 20_000);
        }

        System.out.printf("%-14s %14s %14s %14s%n",
                "payload bytes", "decode ns/call", "wrap ns/call", "decode ns/byte");
        for (int size : sizes) {
            byte[] b = payload(size);

            long t0 = System.nanoTime();
            decodeLoop(b, n);
            long dec = (System.nanoTime() - t0) / n;

            t0 = System.nanoTime();
            wrapOnly(b, n);
            long wrap = (System.nanoTime() - t0) / n;

            System.out.printf("%-14d %14d %14d %14.2f%n", size, dec, wrap, (double) dec / size);
        }
        System.out.println("sink=" + sink);
    }
}
