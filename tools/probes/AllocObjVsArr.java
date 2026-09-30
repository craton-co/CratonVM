/** AllocObjVsArr — `new Object()` (inline TLAB bump since 2024) beside
 *  `new byte[8]` (helper until now), same loop, same escape shape. If both
 *  land in the same place the cost is not the allocator call. */
public class AllocObjVsArr {
    static long objs(Object[] keep, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) { Object o = new Object(); keep[i & 63] = o; acc += (i & 1); }
        return acc;
    }
    static long arrs(Object[] keep, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) { byte[] b = new byte[8]; keep[i & 63] = b; acc += (i & 1); }
        return acc;
    }
    static long store(Object[] keep, int n) {
        Object o = new Object();
        long acc = 0;
        for (int i = 0; i < n; i++) { keep[i & 63] = o; acc += (i & 1); }
        return acc;
    }
    static void rung(String name, int ops, long t0, long t1, long c) {
        System.out.printf("%-12s %8d ms  %9.1f ns/op   [%d]%n", name, (t1-t0), (t1-t0)*1e6/ops, c);
    }
    public static void main(String[] a) {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 1000000;
        int passes = a.length > 1 ? Integer.parseInt(a[1]) : 3;
        Object[] keep = new Object[64];
        for (int p = 1; p <= passes; p++) {
            System.out.println("-- pass " + p);
            long t0,t1,c;
            t0=System.currentTimeMillis(); c=store(keep,n); t1=System.currentTimeMillis(); rung("store", n,t0,t1,c);
            t0=System.currentTimeMillis(); c=objs(keep,n);  t1=System.currentTimeMillis(); rung("newObject", n,t0,t1,c);
            t0=System.currentTimeMillis(); c=arrs(keep,n);  t1=System.currentTimeMillis(); rung("newByte8", n,t0,t1,c);
        }
    }
}
