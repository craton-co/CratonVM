import java.nio.charset.Charset;
import java.nio.charset.CharsetDecoder;
import java.nio.charset.CharsetEncoder;
import java.nio.charset.UnsupportedCharsetException;
import java.util.HashMap;
import java.util.Locale;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ConcurrentMap;

import org.apache.tomcat.util.buf.CharsetCache;

/**
 * The LazyCsCache arm of TestCharsetCachePerformance, decomposed.
 *
 * Each variant changes exactly ONE thing against the one above it, so the
 * ~8x the lazy arm costs over the full-cache arm can be attributed to a
 * specific construct rather than to "the lazy cache".
 *
 * Usage: LazyArmVariants [iterations] [threads]
 */
public final class LazyArmVariants {

    private static final String[] NAMES =
            { "ISO-8859-1", "ISO-8859-2", "ISO-8859-3", "ISO-8859-4", "ISO-8859-5" };

    interface CsCache { Charset getCharset(String charsetName); }

    /** Placeholder charset, same shape as CharsetCache's own. */
    static final class Dummy extends Charset {
        Dummy() { super("Dummy", null); }
        public boolean contains(Charset cs) { return false; }
        public CharsetDecoder newDecoder() { return null; }
        public CharsetEncoder newEncoder() { return null; }
    }

    /** V0 — the real thing, straight off the Tomcat classpath. */
    static final class V0Real implements CsCache {
        private final CharsetCache cache = new CharsetCache();
        public Charset getCharset(String n) { return cache.getCharset(n); }
    }

    /** Shared population helper: same entry set the real cache builds. */
    private static void populate(Map<String,Charset> into, Charset dummy) {
        for (Charset charset : Charset.availableCharsets().values()) {
            into.put(charset.name().toLowerCase(Locale.ENGLISH), charset);
            for (String alias : charset.aliases()) {
                into.put(alias.toLowerCase(Locale.ENGLISH), charset);
            }
        }
        // A handful of never-looked-up dummy entries, so the DUMMY compare in
        // the variants below is a real (always-false) test rather than dead.
        into.put("craton-dummy-1", dummy);
        into.put("craton-dummy-2", dummy);
    }

    /** V1 — a clone of CharsetCache.getCharset: CHM + toLowerCase + try/catch + DUMMY compare. */
    static final class V1Clone implements CsCache {
        private static final Charset DUMMY = new Dummy();
        private final ConcurrentMap<String,Charset> cache = new ConcurrentHashMap<>();
        V1Clone() { populate(cache, DUMMY); }
        public Charset getCharset(String charsetName) {
            String lcCharsetName = charsetName.toLowerCase(Locale.ENGLISH);
            Charset result = cache.get(lcCharsetName);
            if (result == DUMMY) {
                try {
                    Charset charset = Charset.forName(lcCharsetName);
                    cache.put(lcCharsetName, charset);
                    result = charset;
                } catch (UnsupportedCharsetException e) {
                    cache.remove(lcCharsetName);
                    result = null;
                }
            }
            return result;
        }
    }

    /** V2 — V1 with the try/catch removed (nothing else changes). */
    static final class V2NoTryCatch implements CsCache {
        private static final Charset DUMMY = new Dummy();
        private final ConcurrentMap<String,Charset> cache = new ConcurrentHashMap<>();
        V2NoTryCatch() { populate(cache, DUMMY); }
        public Charset getCharset(String charsetName) {
            String lcCharsetName = charsetName.toLowerCase(Locale.ENGLISH);
            Charset result = cache.get(lcCharsetName);
            if (result == DUMMY) {
                Charset charset = Charset.forName(lcCharsetName);
                cache.put(lcCharsetName, charset);
                result = charset;
            }
            return result;
        }
    }

    /** V3 — V2 with the DUMMY getstatic + compare removed. */
    static final class V3NoDummyCompare implements CsCache {
        private final ConcurrentMap<String,Charset> cache = new ConcurrentHashMap<>();
        V3NoDummyCompare() { populate(cache, new Dummy()); }
        public Charset getCharset(String charsetName) {
            return cache.get(charsetName.toLowerCase(Locale.ENGLISH));
        }
    }

    /** V4 — V3 with a plain HashMap instead of a ConcurrentHashMap. */
    static final class V4HashMap implements CsCache {
        private final Map<String,Charset> cache = new HashMap<>();
        V4HashMap() { populate(cache, new Dummy()); }
        public Charset getCharset(String charsetName) {
            return cache.get(charsetName.toLowerCase(Locale.ENGLISH));
        }
    }

    /** V5 — V4 with the map in a static field rather than an instance field
     *  (this is exactly the shape of the passing FullCsCache arm). */
    static final class V5StaticHashMap implements CsCache {
        private static final Map<String,Charset> cache = new HashMap<>();
        static { populate(cache, new Dummy()); }
        public Charset getCharset(String charsetName) {
            return cache.get(charsetName.toLowerCase(Locale.ENGLISH));
        }
    }

    /** V6 — V3 (CHM) but with the map in a static field. */
    static final class V6StaticChm implements CsCache {
        private static final ConcurrentMap<String,Charset> cache = new ConcurrentHashMap<>();
        static { populate(cache, new Dummy()); }
        public Charset getCharset(String charsetName) {
            return cache.get(charsetName.toLowerCase(Locale.ENGLISH));
        }
    }

    /*
     * V7/V8 exist because V0 (the real arm) delegates through a *second* Java
     * method — `LazyCsCache.getCharset` calls `CharsetCache.getCharset` — while
     * V1..V6 do the work in the interface-dispatched method itself. Comparing
     * V0 against V2 therefore changes two things at once. V7 and V8 have V0's
     * call depth exactly and differ from each other only by a `try`/`catch`
     * that never fires.
     */

    /** Delegate without a try/catch. */
    static final class PlainDelegate {
        private final ConcurrentMap<String,Charset> cache = new ConcurrentHashMap<>();
        PlainDelegate() { populate(cache, new Dummy()); }
        Charset getCharset(String charsetName) {
            return cache.get(charsetName.toLowerCase(Locale.ENGLISH));
        }
    }

    /** The same delegate plus a never-taken try/catch around a call. */
    static final class TryDelegate {
        private static final Charset DUMMY = new Dummy();
        private final ConcurrentMap<String,Charset> cache = new ConcurrentHashMap<>();
        TryDelegate() { populate(cache, DUMMY); }
        Charset getCharset(String charsetName) {
            String lcCharsetName = charsetName.toLowerCase(Locale.ENGLISH);
            Charset result = cache.get(lcCharsetName);
            if (result == DUMMY) {
                try {
                    Charset charset = Charset.forName(lcCharsetName);
                    addToCache(lcCharsetName, charset);
                    result = charset;
                } catch (UnsupportedCharsetException e) {
                    cache.remove(lcCharsetName);
                    result = null;
                }
            }
            return result;
        }
        private void addToCache(String name, Charset charset) { cache.put(name, charset); }
    }

    static final class V7DelegatePlain implements CsCache {
        private final PlainDelegate delegate = new PlainDelegate();
        public Charset getCharset(String n) { return delegate.getCharset(n); }
    }

    static final class V8DelegateTry implements CsCache {
        private final TryDelegate delegate = new TryDelegate();
        public Charset getCharset(String n) { return delegate.getCharset(n); }
    }

    static final class Worker extends Thread {
        private final int iterations; private final CsCache cache;
        Worker(int i, CsCache c) { iterations = i; cache = c; }
        public void run() {
            for (int i = 0; i < iterations; i++) { cache.getCharset(NAMES[i % NAMES.length]); }
        }
    }

    static double time(CsCache cache, int threads, int iterations) throws Exception {
        Thread[] ts = new Thread[threads];
        for (int i = 0; i < threads; i++) ts[i] = new Worker(iterations, cache);
        long s = System.nanoTime();
        for (Thread t : ts) t.start();
        for (Thread t : ts) t.join();
        return (System.nanoTime() - s) / (double) iterations;
    }

    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 300_000;
        int threads = args.length > 1 ? Integer.parseInt(args[1]) : 10;

        String[] names = { "V0 real CharsetCache", "V1 clone (chm+try+dummy)",
                           "V2 no try/catch", "V3 no dummy compare",
                           "V4 HashMap not CHM", "V5 static HashMap", "V6 static CHM",
                           "V7 delegate, no try", "V8 delegate, with try" };
        CsCache[] all = { new V0Real(), new V1Clone(), new V2NoTryCatch(), new V3NoDummyCompare(),
                          new V4HashMap(), new V5StaticHashMap(), new V6StaticChm(),
                          new V7DelegatePlain(), new V8DelegateTry() };

        for (CsCache c : all) time(c, 1, Math.min(iterations, 100_000));   // warm

        System.out.printf("%-28s %11s %11s %8s%n", "variant", "1t ns/op", threads + "t ns/op", "scale");
        for (int i = 0; i < all.length; i++) {
            double one = time(all[i], 1, iterations);
            double many = time(all[i], threads, iterations);
            System.out.printf("%-28s %11.1f %11.1f %7.2fx%n", names[i], one, many, (one * threads) / many);
        }
    }
}
