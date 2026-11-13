// Shape of java.nio.charset's sun.nio.cs.ThreadLocalCoders.Cache.forName (a ThreadLocal<Object[]> move-to-front cache).
// On the real class the C2 compile was refused by the post-schedule dominance check, leaving every
// Charset.decode in C1 (docs/known-issues/jit/small-jdk-method-call-overhead-...).
//   cratonvm --cp <dir> -Dj27.timing=true J27TlCacheForName      (CRATONVM_DBG_JITC=1: grep 'schedule rejected')
// Facts: the same names resolve to the same objects, in the same move-to-front order, in every mode.
public class J27TlCacheForName {
    abstract static class Cache {
        private final ThreadLocal<Object[]> cache = new ThreadLocal<>();
        private final int size;

        Cache(int size) { this.size = size; }

        abstract Object create(Object name);

        abstract boolean hasName(Object ob, Object name);

        private void moveToFront(Object[] oa, int i) {
            Object ob = oa[i];
            for (int j = i; j > 0; j--) oa[j] = oa[j - 1];
            oa[0] = ob;
        }

        Object forName(Object name) {
            Object[] oa = cache.get();
            if (oa == null) {
                oa = new Object[size];
                cache.set(oa);
            } else {
                for (int i = 0; i < oa.length; i++) {
                    Object ob = oa[i];
                    if (ob == null) continue;
                    if (hasName(ob, name)) {
                        if (i > 0) moveToFront(oa, i);
                        return ob;
                    }
                }
            }
            Object ob = create(name);
            oa[oa.length - 1] = ob;
            moveToFront(oa, oa.length - 1);
            return ob;
        }
    }

    static final Cache CACHE = new Cache(3) {
        Object create(Object name) { return new StringBuilder((String) name); }
        boolean hasName(Object ob, Object name) { return ob.toString().equals(name); }
    };

    static long run(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += ((StringBuilder) CACHE.forName("ISO-8859-1")).length();
        return s;
    }

    public static void main(String[] a) {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 2_000_000;
        Object first = CACHE.forName("UTF-8");
        Object second = CACHE.forName("ISO-8859-1");
        System.out.println("fact same " + (CACHE.forName("UTF-8") == first) + " " + (CACHE.forName("ISO-8859-1") == second)
                + " distinct " + (first != second));
        long chk = 0;
        for (int r = 0; r < 4; r++) {
            long t0 = System.nanoTime();
            chk += run(n);
            if (Boolean.getBoolean("j27.timing")) System.out.println("time forname-rep" + r + " " + (System.nanoTime() - t0) / (double) n);
        }
        System.out.println("fact checksum " + chk);
    }
}
