import java.nio.charset.Charset;
import java.util.HashMap;
import java.util.Locale;
import java.util.Map;

import org.apache.tomcat.util.buf.CharsetCache;

/**
 * The three arms of {@code org.apache.tomcat.util.buf.TestCharsetCachePerformance#testCache},
 * copied verbatim, but with the iteration count on the command line so a run
 * costs seconds instead of the ~4 minutes the real class needs per arm.
 *
 * Usage: Doc23Arms [iterations] [threads] [rounds]
 *
 * Prints one line per round with the three arm times and the two ratios the
 * test actually asserts on (full/none and lazy/none, both must be &lt; 1).
 */
public final class Doc23Arms {

    private interface CsCache {
        Charset getCharset(String charsetName);
    }

    private static class NoCsCache implements CsCache {
        @Override
        public Charset getCharset(String charsetName) {
            return Charset.forName(charsetName);
        }
    }

    private static class FullCsCache implements CsCache {
        private static final Map<String,Charset> cache = new HashMap<>();
        static {
            for (Charset charset : Charset.availableCharsets().values()) {
                cache.put(charset.name().toLowerCase(Locale.ENGLISH), charset);
                for (String alias : charset.aliases()) {
                    cache.put(alias.toLowerCase(Locale.ENGLISH), charset);
                }
            }
        }
        @Override
        public Charset getCharset(String charsetName) {
            return cache.get(charsetName.toLowerCase(Locale.ENGLISH));
        }
    }

    private static class LazyCsCache implements CsCache {
        private CharsetCache cache = new CharsetCache();
        @Override
        public Charset getCharset(String charsetName) {
            return cache.getCharset(charsetName);
        }
    }

    private static class TestCsCacheThread extends Thread {
        private final int iterations;
        private final CsCache cache;
        private final String[] lookupNames;
        private final int lookupNamesCount;

        TestCsCacheThread(int iterations, CsCache cache, String[] lookupNames) {
            this.iterations = iterations;
            this.cache = cache;
            this.lookupNames = lookupNames;
            this.lookupNamesCount = lookupNames.length;
        }

        @Override
        public void run() {
            for (int i = 0; i < iterations; i++) {
                cache.getCharset(lookupNames[i % lookupNamesCount]);
            }
        }
    }

    private static long doTest(CsCache cache, int threadCount, int iterations) throws Exception {
        String[] lookupNames = new String[] {
                "ISO-8859-1", "ISO-8859-2", "ISO-8859-3", "ISO-8859-4", "ISO-8859-5" };

        Thread[] threads = new Thread[threadCount];
        for (int i = 0; i < threadCount; i++) {
            threads[i] = new TestCsCacheThread(iterations, cache, lookupNames);
        }
        long startTime = System.nanoTime();
        for (int i = 0; i < threadCount; i++) {
            threads[i].start();
        }
        for (int i = 0; i < threadCount; i++) {
            threads[i].join();
        }
        return System.nanoTime() - startTime;
    }

    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 1_000_000;
        int threads = args.length > 1 ? Integer.parseInt(args[1]) : 10;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 1;

        for (int r = 0; r < rounds; r++) {
            long none = doTest(new NoCsCache(), threads, iterations);
            long full = doTest(new FullCsCache(), threads, iterations);
            long lazy = doTest(new LazyCsCache(), threads, iterations);
            System.out.printf(
                    "round=%d threads=%d iters=%d  none=%8.3fs full=%8.3fs lazy=%8.3fs  "
                            + "full/none=%6.3f %s  lazy/none=%6.3f %s%n",
                    r, threads, iterations,
                    none / 1e9, full / 1e9, lazy / 1e9,
                    full / (double) none, full < none ? "PASS" : "FAIL",
                    lazy / (double) none, lazy < none ? "PASS" : "FAIL");
        }
    }
}
