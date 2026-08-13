import io.netty.buffer.ByteBuf;
import io.netty.buffer.Unpooled;
import io.netty.buffer.search.AbstractMultiSearchProcessorFactory;
import io.netty.buffer.search.AbstractSearchProcessorFactory;
import io.netty.buffer.search.SearchProcessorFactory;

import java.util.Arrays;

/**
 * Splits netty's SearchProcessorTest.testUniqueLen64Substrings into its two
 * halves — factory construction and haystack scanning — per algorithm, so a
 * VM-vs-VM ratio names which half is slow instead of the test as a whole.
 */
public final class AcProbe {

    private static final int ALGO_AC = 0;
    private static final int ALGO_KMP = 1;
    private static final int ALGO_BITAP = 2;

    static SearchProcessorFactory factory(int algo, byte[] needle) {
        switch (algo) {
            case ALGO_KMP:
                return AbstractSearchProcessorFactory.newKmpSearchProcessorFactory(needle);
            case ALGO_BITAP:
                return AbstractSearchProcessorFactory.newBitapSearchProcessorFactory(needle);
            default:
                return AbstractMultiSearchProcessorFactory.newAhoCorasicSearchProcessorFactory(needle);
        }
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 2016;
        byte[] haystackBytes = new byte[32 * 65];
        int pos = 0;
        for (int i = 1; i <= 64; i++) {
            for (int j = 0; j < i; j++) {
                haystackBytes[pos++] = (byte) i;
            }
        }
        ByteBuf haystack = Unpooled.copiedBuffer(haystackBytes);
        String[] names = { "AHO_CORASIC", "KMP", "BITAP" };
        for (int algo = 0; algo < 3; algo++) {
            long build = 0;
            long scan = 0;
            for (int start = 0; start < n; start++) {
                byte[] needle = Arrays.copyOfRange(haystackBytes, start, start + 64);
                long t0 = System.nanoTime();
                SearchProcessorFactory f = factory(algo, needle);
                long t1 = System.nanoTime();
                int r = haystack.forEachByte(f.newSearchProcessor());
                long t2 = System.nanoTime();
                if (r != start + 63) {
                    throw new IllegalStateException(names[algo] + " start=" + start + " -> " + r);
                }
                build += t1 - t0;
                scan += t2 - t1;
            }
            System.out.println("@@ " + names[algo] + " n=" + n
                    + " build_ms=" + (build / 1000000L)
                    + " scan_ms=" + (scan / 1000000L));
        }
    }
}
