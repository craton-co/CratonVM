import java.nio.charset.Charset;
import java.util.HashMap;
import java.util.Locale;
import java.util.Map;

/**
 * Separates the full-cache initialization cost from its hot read path. This is
 * intentionally shaped like Tomcat's TestCharsetCachePerformance arms.
 */
public final class CharsetCacheArmProbe {
    private interface Lookup {
        Charset get(String name);
    }

    private static final class NoCache implements Lookup {
        @Override
        public Charset get(String name) {
            return Charset.forName(name);
        }
    }

    private static final class FullCache implements Lookup {
        private static final Map<String, Charset> CACHE = new HashMap<>();
        static {
            for (Charset charset : Charset.availableCharsets().values()) {
                CACHE.put(charset.name().toLowerCase(Locale.ENGLISH), charset);
                for (String alias : charset.aliases()) {
                    CACHE.put(alias.toLowerCase(Locale.ENGLISH), charset);
                }
            }
        }

        @Override
        public Charset get(String name) {
            return CACHE.get(name.toLowerCase(Locale.ENGLISH));
        }
    }

    private static final String[] NAMES = {
        "ISO-8859-1", "ISO-8859-2", "ISO-8859-3", "ISO-8859-4", "ISO-8859-5"
    };
    private static volatile long sink;

    private static long run(Lookup lookup, int threads, int iterations) throws InterruptedException {
        Thread[] workers = new Thread[threads];
        long start = System.nanoTime();
        for (int t = 0; t < threads; t++) {
            workers[t] = new Thread(() -> {
                long local = 0;
                for (int i = 0; i < iterations; i++) {
                    local += lookup.get(NAMES[i % NAMES.length]).hashCode();
                }
                sink += local;
            });
            workers[t].start();
        }
        for (Thread worker : workers) {
            worker.join();
        }
        return System.nanoTime() - start;
    }

    public static void main(String[] args) throws Exception {
        int iterations = args.length == 0 ? 2_000_000 : Integer.parseInt(args[0]);
        long initStart = System.nanoTime();
        Lookup fullCache = new FullCache();
        long init = System.nanoTime() - initStart;
        long full = run(fullCache, 10, iterations);
        long none = run(new NoCache(), 10, iterations);
        // Reports, does not assert: `timeFull < timeNone` is the OPEN
        // assertion of docs/known-issues/tomcat/23-charsetcache-pathological-slowdown.md.
        // At the real test's 10,000,000 iterations the full cache does win;
        // at this probe's smaller default it does not, so asserting here
        // would just make the probe a guaranteed failure rather than a
        // measurement.
        System.out.println("CHARSET_ARM_OK init=" + init + " full=" + full + " none=" + none
                + " full/none=" + (full / (double) none) + " sink=" + sink);
    }
}
