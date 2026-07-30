import java.nio.charset.Charset;

import org.apache.tomcat.util.buf.CharsetCache;

/** Same as the concurrent probe, but resolves every lazy name before timing. */
public final class CharsetCacheLazyWarmConcurrencyProbe {
    private static final String[] NAMES = {
        "ISO-8859-1", "ISO-8859-2", "ISO-8859-3", "ISO-8859-4", "ISO-8859-5"
    };
    private static volatile long sink;

    public static void main(String[] args) throws Exception {
        int iterations = args.length == 0 ? 100_000 : Integer.parseInt(args[0]);
        CharsetCache cache = new CharsetCache();
        for (String name : NAMES) {
            if (cache.getCharset(name) == null) {
                throw new AssertionError("warmup failed: " + name);
            }
        }
        Thread[] threads = new Thread[10];
        long start = System.nanoTime();
        for (int t = 0; t < threads.length; t++) {
            threads[t] = new Thread(() -> {
                long local = 0;
                for (int i = 0; i < iterations; i++) {
                    Charset charset = cache.getCharset(NAMES[i % NAMES.length]);
                    if (charset == null) {
                        throw new AssertionError("missing charset");
                    }
                    local += charset.hashCode();
                }
                sink += local;
            });
            threads[t].start();
        }
        for (Thread thread : threads) {
            thread.join();
        }
        long elapsed = System.nanoTime() - start;
        System.out.println("CHARSET_LAZY_WARM_CONCURRENT_OK ns=" + elapsed + " sink=" + sink);
    }
}
