import java.nio.charset.Charset;
import java.util.HashMap;
import java.util.Locale;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;

/** Scaling probe: same shape as TestCharsetCachePerformance's arms, but with a
 *  thread count and iteration count on the command line so contention can be
 *  separated from single-thread code quality. */
public final class ScaleProbe {
    interface CsCache { Charset getCharset(String n); }

    static final class Full implements CsCache {
        static final Map<String,Charset> cache = new HashMap<>();
        static {
            for (Charset cs : Charset.availableCharsets().values()) {
                cache.put(cs.name().toLowerCase(Locale.ENGLISH), cs);
                for (String a : cs.aliases()) cache.put(a.toLowerCase(Locale.ENGLISH), cs);
            }
        }
        public Charset getCharset(String n) { return cache.get(n.toLowerCase(Locale.ENGLISH)); }
    }

    static final class LowerOnly implements CsCache {
        public Charset getCharset(String n) { String s = n.toLowerCase(Locale.ENGLISH); return s.length() > 0 ? null : null; }
    }

    static final class MapOnly implements CsCache {
        static final Map<String,Charset> cache = new HashMap<>();
        static { for (Charset cs : Charset.availableCharsets().values()) cache.put(cs.name().toLowerCase(Locale.ENGLISH), cs); }
        public Charset getCharset(String n) { return cache.get(n); }
    }

    static final class ChmOnly implements CsCache {
        static final Map<String,Charset> cache = new ConcurrentHashMap<>();
        static { for (Charset cs : Charset.availableCharsets().values()) cache.put(cs.name().toLowerCase(Locale.ENGLISH), cs); }
        public Charset getCharset(String n) { return cache.get(n); }
    }

    static final String[] NAMES = {"ISO-8859-1","ISO-8859-2","ISO-8859-3","ISO-8859-4","ISO-8859-5"};
    static final String[] LNAMES = {"iso-8859-1","iso-8859-2","iso-8859-3","iso-8859-4","iso-8859-5"};
    static volatile long sink;

    static double run(CsCache c, int threads, int iters, boolean lower) throws Exception {
        final String[] names = lower ? LNAMES : NAMES;
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) ts[t] = new Thread(() -> {
            long l = 0;
            for (int i = 0; i < iters; i++) { Charset cs = c.getCharset(names[i % names.length]); if (cs != null) l++; }
            sink += l;
        });
        long s = System.nanoTime();
        for (Thread t : ts) t.start();
        for (Thread t : ts) t.join();
        long e = System.nanoTime();
        return (e - s) / (double) iters;   // ns per op per thread
    }

    public static void main(String[] args) throws Exception {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 1_000_000;
        int[] tcounts = {1, 2, 4, 10};
        CsCache full = new Full(), mapOnly = new MapOnly(), chmOnly = new ChmOnly(), lowerOnly = new LowerOnly();
        // warm
        run(full, 1, 200_000, false); run(mapOnly, 1, 200_000, true);
        run(chmOnly, 1, 200_000, true); run(lowerOnly, 1, 200_000, false);
        for (int t : tcounts) {
            System.out.printf("threads=%-3d full=%9.1f  lowerOnly=%9.1f  hashmapOnly=%9.1f  chmOnly=%9.1f  ns/op%n",
                t, run(full, t, iters, false), run(lowerOnly, t, iters, false),
                run(mapOnly, t, iters, true), run(chmOnly, t, iters, true));
        }
        System.out.println("sink=" + sink);
    }
}
