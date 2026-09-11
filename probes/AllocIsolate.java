/**
 * AllocIsolate — what `new byte[8]` actually costs, with everything that is
 * not the allocation subtracted rather than assumed.
 *
 *   loopOnly   : the loop and the arithmetic, nothing else
 *   storeOnly  : the same loop, plus the escaping reference store (and its
 *                write barrier) that AllocProbe used to keep the object alive
 *   allocStore : storeOnly + the allocation            -> alloc = this - storeOnly
 *   allocArray : the allocation parked in a rotating local array, so the
 *                object escapes without a STATIC store's barrier
 *
 * The first measurement of this change attributed ~150 ns/op to `new byte[8]`
 * from AllocProbe, which only ever timed `allocStore`. This splits it.
 */
public class AllocIsolate {
    static Object sink;

    static long loopOnly(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += (i ^ (i >>> 3)) & 7;
        }
        return acc;
    }

    static long storeOnly(byte[] b, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            b[0] = (byte) i;
            acc += b[0];
            sink = b;
        }
        return acc;
    }

    static long allocStore(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            byte[] b = new byte[8];
            b[0] = (byte) i;
            acc += b[0];
            sink = b;
        }
        return acc;
    }

    static long allocArray(Object[] keep, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            byte[] b = new byte[8];
            b[0] = (byte) i;
            acc += b[0];
            keep[i & 63] = b;
        }
        return acc;
    }

    static long allocBig(Object[] keep, int n) {
        // 64 elements: past the point where a per-object constant dominates.
        long acc = 0;
        for (int i = 0; i < n; i++) {
            byte[] b = new byte[64];
            b[0] = (byte) i;
            acc += b[0];
            keep[i & 63] = b;
        }
        return acc;
    }

    static long allocChar(Object[] keep, int n) {
        // char[] — a 2-byte element type, so the shift arm of the size
        // computation is exercised rather than the 1-byte one.
        long acc = 0;
        for (int i = 0; i < n; i++) {
            char[] c = new char[16];
            c[0] = (char) i;
            acc += c[0];
            keep[i & 63] = c;
        }
        return acc;
    }

    static void rung(String name, int ops, long t0, long t1, long c) {
        System.out.printf("%-16s %8d ms  %9.1f ns/op   [%d]%n",
                          name, (t1 - t0), (t1 - t0) * 1e6 / ops, c);
    }

    public static void main(String[] a) {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 1000000;
        int passes = a.length > 1 ? Integer.parseInt(a[1]) : 3;
        byte[] fixed = new byte[8];
        Object[] keep = new Object[64];
        for (int p = 1; p <= passes; p++) {
            System.out.println("-- pass " + p);
            long t0, t1, c;
            t0 = System.currentTimeMillis(); c = loopOnly(n);           t1 = System.currentTimeMillis(); rung("loopOnly", n, t0, t1, c);
            t0 = System.currentTimeMillis(); c = storeOnly(fixed, n);   t1 = System.currentTimeMillis(); rung("storeOnly", n, t0, t1, c);
            t0 = System.currentTimeMillis(); c = allocStore(n);         t1 = System.currentTimeMillis(); rung("allocStore", n, t0, t1, c);
            t0 = System.currentTimeMillis(); c = allocArray(keep, n);   t1 = System.currentTimeMillis(); rung("allocArray", n, t0, t1, c);
            t0 = System.currentTimeMillis(); c = allocBig(keep, n);     t1 = System.currentTimeMillis(); rung("allocBig", n, t0, t1, c);
            t0 = System.currentTimeMillis(); c = allocChar(keep, n);    t1 = System.currentTimeMillis(); rung("allocChar", n, t0, t1, c);
        }
    }
}
