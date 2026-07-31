import java.nio.ByteBuffer;
import java.nio.CharBuffer;
import java.nio.charset.Charset;
import java.nio.charset.CodingErrorAction;
import java.nio.charset.StandardCharsets;

import org.apache.tomcat.util.buf.ByteChunk;
import org.apache.tomcat.util.buf.MessageBytes;
import org.apache.tomcat.util.buf.StringCache;

/**
 * Decomposes the per-iteration cost of TestMethodPerformance's hot loop
 * (`mb.setBytes(...); mb.toStringType();`) into the frames the chain is
 * actually made of, so the dominant one names itself.
 *
 * The chain is
 *   MessageBytes.toStringType
 *     -> ByteChunk.toString                 (try/catch)
 *       -> StringCache.toString(bc, a, b)   (try/catch + synchronized block;
 *                                            a pass-through when the byte
 *                                            cache is disabled, which is the
 *                                            default)
 *         -> ByteChunk.toStringInternal
 *           -> Charset.decode(ByteBuffer.wrap(...))
 *           -> new String(char[], int, int)
 *
 * Every stage is a plain static method with the loop INLINE -- no lambda, no
 * functional interface. Driving the stages through a `Runnable` measured ~3.4us
 * per call on CratonVM and buried everything it was meant to compare.
 *
 * Each stage runs `blocks` times so a rising row exposes cost that grows with
 * the number of objects already allocated (measured across processes: 15.5us
 * at 100k iterations vs 35.3us at 800k, with no GC in between).
 */
public final class MbChainCostProbe {

    private static final byte[] INPUT =
            "GET /context-path/servlet-path/path-info HTTP/1.1".getBytes(StandardCharsets.UTF_8);
    private static final Charset UTF8 = StandardCharsets.UTF_8;
    private static final CodingErrorAction REPL = CodingErrorAction.REPLACE;
    private static final MessageBytes MB = MessageBytes.newInstance();
    private static final ByteChunk BC = new ByteChunk();

    private static long sink;

    private static void emptyLoop(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += i;
        }
        sink += s;
    }

    private static void byteBufferWrap(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += ByteBuffer.wrap(INPUT, 0, 3).remaining();
        }
        sink += s;
    }

    private static void charsetDecode(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            CharBuffer cb = UTF8.decode(ByteBuffer.wrap(INPUT, 0, 3));
            s += cb.length();
        }
        sink += s;
    }

    private static void newStringFromChars(int n) {
        char[] c = {'G', 'E', 'T'};
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += new String(c, 0, 3).length();
        }
        sink += s;
    }

    private static void setBytesOnly(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            BC.setBytes(INPUT, 0, 3);
            s += BC.getLength();
        }
        sink += s;
    }

    /** decode + new String, done INLINE -- the work `toStringInternal` does. */
    private static void inlineEquivalent(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            BC.setBytes(INPUT, 0, 3);
            CharBuffer cb = UTF8.decode(ByteBuffer.wrap(INPUT, 0, 3));
            s += new String(cb.array(), cb.arrayOffset(), cb.length()).length();
        }
        sink += s;
    }

    private static void toStringInternalDirect(int n) throws Exception {
        long s = 0;
        for (int i = 0; i < n; i++) {
            BC.setBytes(INPUT, 0, 3);
            s += BC.toStringInternal(REPL, REPL).length();
        }
        sink += s;
    }

    private static void stringCacheDirect(int n) throws Exception {
        long s = 0;
        for (int i = 0; i < n; i++) {
            BC.setBytes(INPUT, 0, 3);
            s += StringCache.toString(BC, REPL, REPL).length();
        }
        sink += s;
    }

    private static void byteChunkToString(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            BC.setBytes(INPUT, 0, 3);
            s += BC.toString().length();
        }
        sink += s;
    }

    private static void mbFullChain(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            MB.setBytes(INPUT, 0, 3);
            s += MB.toStringType().length();
        }
        sink += s;
    }

    private static void row(String name, int blocks, int bs, int which) throws Exception {
        StringBuilder sb = new StringBuilder(String.format("%-24s", name));
        for (int b = 0; b < blocks; b++) {
            long t0 = System.nanoTime();
            switch (which) {
                case 0: emptyLoop(bs); break;
                case 1: byteBufferWrap(bs); break;
                case 2: charsetDecode(bs); break;
                case 3: newStringFromChars(bs); break;
                case 4: setBytesOnly(bs); break;
                case 5: inlineEquivalent(bs); break;
                case 6: toStringInternalDirect(bs); break;
                case 7: stringCacheDirect(bs); break;
                case 8: byteChunkToString(bs); break;
                default: mbFullChain(bs); break;
            }
            sb.append(String.format("%9d", (System.nanoTime() - t0) / bs));
        }
        System.out.println(sb);
    }

    public static void main(String[] args) throws Exception {
        int blocks = args.length > 0 ? Integer.parseInt(args[0]) : 6;
        int bs = args.length > 1 ? Integer.parseInt(args[1]) : 100_000;

        System.out.printf("%-24s", "stage (ns/op by block)");
        for (int b = 0; b < blocks; b++) {
            System.out.printf("%9d", b);
        }
        System.out.println();

        String[] names = {"emptyLoop", "byteBufferWrap", "charsetDecode",
                "newStringFromChars", "setBytesOnly", "inlineEquivalent",
                "toStringInternalDirect", "stringCacheDirect", "byteChunkToString",
                "mbFullChain"};
        for (int i = 0; i < names.length; i++) {
            row(names[i], blocks, bs, i);
        }
        System.out.println("sink=" + sink);
    }
}
