import java.nio.charset.Charset;

/** Byte-for-byte the NoCsCache arm of TestCharsetCachePerformance, alone. */
public final class NoArmOnly {
    private interface CsCache { Charset getCharset(String n); }
    private static class NoCsCache implements CsCache {
        public Charset getCharset(String n) { return Charset.forName(n); }
    }
    private static class W extends Thread {
        private final int iterations; private final CsCache cache; private final String[] names; private final int n;
        W(int i, CsCache c, String[] s) { iterations = i; cache = c; names = s; n = s.length; }
        public void run() { for (int i = 0; i < iterations; i++) { cache.getCharset(names[i % n]); } }
    }
    public static void main(String[] a) throws Exception {
        int iterations = a.length > 0 ? Integer.parseInt(a[0]) : 10000000;
        String[] names = { "ISO-8859-1", "ISO-8859-2", "ISO-8859-3", "ISO-8859-4", "ISO-8859-5" };
        Thread[] t = new Thread[10];
        for (int i = 0; i < 10; i++) t[i] = new W(iterations, new NoCsCache(), names);
        long s = System.nanoTime();
        for (Thread x : t) x.start();
        for (Thread x : t) x.join();
        System.out.println("NoCsCache: " + (System.nanoTime() - s) + "ns");
    }
}
