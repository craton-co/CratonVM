// The late-bind shape of DoubleToDecimal.toDecimal, deterministic, on stock dev:
// toDec is compiled while the class of its rare-branch callee (Rare) is not yet
// initialized (DeclaringClassNotInitialized: a late-bind waiter); Rare is then
// initialized and made hot; the next rare-branch call through toDec's dispatch
// site is served a compiled body, which recompiles toDec and makes the displaced
// body not entrant under hot callers that baked a direct CALL to it. Every
// result is compared with a call-free reference.
public class LateBindToDecimalShape {
    static final long C_MIN = 1L << 52;

    static final class MU {
        static final long[] G = new long[1234];
        static { for (int i = 0; i < G.length; i++) G[i] = i * 0x9E3779B97F4A7C15L; }
        static int f10(int e) { return (int) (e * 661_971_961_083L >> 41); }
        static int f2(int e) { return (int) (e * 913_124_641_741L >> 38); }
        static long g1(int k) { return G[k + 324 << 1]; }
        static long g0(int k) { return G[k + 324 << 1 | 1]; }
    }

    static final class Rare {
        static final int BIAS;
        static { BIAS = Integer.getInteger("late.bind.bias", 0); }
        static int f10q(int e) { return (int) (e * 661_971_961_083L + (-274_743_187_321L) >> 41) + BIAS; }
    }

    int calls;

    // Same arity and kinds as toDecimal(byte[], int, int, long, int, FormattedFPDecimal).
    private int toDec(byte[] str, int index, int q, long c, int dk, Object fd) {
        calls++;
        long cb = c << 2;
        int k;
        if (c != C_MIN | q == -1074) {
            k = MU.f10(q);
        } else {
            k = Rare.f10q(q);
        }
        int h = q + MU.f2(-k) + 2;
        long g1 = MU.g1(k);
        long g0 = MU.g0(k);
        long v = (g1 ^ (g0 >>> 3)) + (cb << (h & 7));
        if (dk == 12345) {
            str[0] ^= (byte) (c >>> 0 ^ q * 3);
            str[1] ^= (byte) (c >>> 1 ^ q * 4);
            str[2] ^= (byte) (c >>> 2 ^ q * 5);
            str[3] ^= (byte) (c >>> 3 ^ q * 6);
            str[4] ^= (byte) (c >>> 4 ^ q * 7);
            str[5] ^= (byte) (c >>> 5 ^ q * 8);
            str[6] ^= (byte) (c >>> 6 ^ q * 9);
            str[7] ^= (byte) (c >>> 7 ^ q * 10);
            str[0] ^= (byte) (c >>> 8 ^ q * 11);
            str[1] ^= (byte) (c >>> 9 ^ q * 12);
            str[2] ^= (byte) (c >>> 10 ^ q * 13);
            str[3] ^= (byte) (c >>> 11 ^ q * 14);
            str[4] ^= (byte) (c >>> 12 ^ q * 15);
            str[5] ^= (byte) (c >>> 13 ^ q * 16);
            str[6] ^= (byte) (c >>> 14 ^ q * 17);
            str[7] ^= (byte) (c >>> 15 ^ q * 18);
            str[0] ^= (byte) (c >>> 16 ^ q * 19);
            str[1] ^= (byte) (c >>> 17 ^ q * 20);
            str[2] ^= (byte) (c >>> 18 ^ q * 21);
            str[3] ^= (byte) (c >>> 19 ^ q * 22);
            str[4] ^= (byte) (c >>> 20 ^ q * 23);
            str[5] ^= (byte) (c >>> 21 ^ q * 24);
            str[6] ^= (byte) (c >>> 22 ^ q * 25);
            str[7] ^= (byte) (c >>> 23 ^ q * 26);
            str[0] ^= (byte) (c >>> 24 ^ q * 27);
            str[1] ^= (byte) (c >>> 25 ^ q * 28);
            str[2] ^= (byte) (c >>> 26 ^ q * 29);
            str[3] ^= (byte) (c >>> 27 ^ q * 30);
            str[4] ^= (byte) (c >>> 28 ^ q * 31);
            str[5] ^= (byte) (c >>> 29 ^ q * 32);
            str[6] ^= (byte) (c >>> 30 ^ q * 33);
            str[7] ^= (byte) (c >>> 31 ^ q * 34);
            str[0] ^= (byte) (c >>> 32 ^ q * 35);
            str[1] ^= (byte) (c >>> 33 ^ q * 36);
            str[2] ^= (byte) (c >>> 34 ^ q * 37);
            str[3] ^= (byte) (c >>> 35 ^ q * 38);
            str[4] ^= (byte) (c >>> 36 ^ q * 39);
            str[5] ^= (byte) (c >>> 37 ^ q * 40);
            str[6] ^= (byte) (c >>> 38 ^ q * 41);
            str[7] ^= (byte) (c >>> 39 ^ q * 42);
            str[0] ^= (byte) (c >>> 40 ^ q * 43);
            str[1] ^= (byte) (c >>> 41 ^ q * 44);
            str[2] ^= (byte) (c >>> 42 ^ q * 45);
            str[3] ^= (byte) (c >>> 43 ^ q * 46);
            str[4] ^= (byte) (c >>> 44 ^ q * 47);
            str[5] ^= (byte) (c >>> 45 ^ q * 48);
            str[6] ^= (byte) (c >>> 46 ^ q * 49);
            str[7] ^= (byte) (c >>> 47 ^ q * 50);
            str[0] ^= (byte) (c >>> 48 ^ q * 51);
            str[1] ^= (byte) (c >>> 49 ^ q * 52);
            str[2] ^= (byte) (c >>> 50 ^ q * 53);
            str[3] ^= (byte) (c >>> 51 ^ q * 54);
            str[4] ^= (byte) (c >>> 52 ^ q * 55);
            str[5] ^= (byte) (c >>> 53 ^ q * 56);
            str[6] ^= (byte) (c >>> 54 ^ q * 57);
            str[7] ^= (byte) (c >>> 55 ^ q * 58);
            str[0] ^= (byte) (c >>> 56 ^ q * 59);
            str[1] ^= (byte) (c >>> 57 ^ q * 60);
            str[2] ^= (byte) (c >>> 58 ^ q * 61);
            str[3] ^= (byte) (c >>> 59 ^ q * 62);
            str[4] ^= (byte) (c >>> 0 ^ q * 63);
            str[5] ^= (byte) (c >>> 1 ^ q * 64);
            str[6] ^= (byte) (c >>> 2 ^ q * 65);
            str[7] ^= (byte) (c >>> 3 ^ q * 66);
            str[0] ^= (byte) (c >>> 4 ^ q * 67);
            str[1] ^= (byte) (c >>> 5 ^ q * 68);
            str[2] ^= (byte) (c >>> 6 ^ q * 69);
            str[3] ^= (byte) (c >>> 7 ^ q * 70);
            str[4] ^= (byte) (c >>> 8 ^ q * 71);
            str[5] ^= (byte) (c >>> 9 ^ q * 72);
            str[6] ^= (byte) (c >>> 10 ^ q * 73);
            str[7] ^= (byte) (c >>> 11 ^ q * 74);
            str[0] ^= (byte) (c >>> 12 ^ q * 75);
            str[1] ^= (byte) (c >>> 13 ^ q * 76);
            str[2] ^= (byte) (c >>> 14 ^ q * 77);
            str[3] ^= (byte) (c >>> 15 ^ q * 78);
            str[4] ^= (byte) (c >>> 16 ^ q * 79);
            str[5] ^= (byte) (c >>> 17 ^ q * 80);
            str[6] ^= (byte) (c >>> 18 ^ q * 81);
            str[7] ^= (byte) (c >>> 19 ^ q * 82);
            str[0] ^= (byte) (c >>> 20 ^ q * 83);
            str[1] ^= (byte) (c >>> 21 ^ q * 84);
            str[2] ^= (byte) (c >>> 22 ^ q * 85);
            str[3] ^= (byte) (c >>> 23 ^ q * 86);
            str[4] ^= (byte) (c >>> 24 ^ q * 87);
            str[5] ^= (byte) (c >>> 25 ^ q * 88);
            str[6] ^= (byte) (c >>> 26 ^ q * 89);
            str[7] ^= (byte) (c >>> 27 ^ q * 90);
            str[0] ^= (byte) (c >>> 28 ^ q * 91);
            str[1] ^= (byte) (c >>> 29 ^ q * 92);
        }
        str[index & 7] = (byte) v;
        return (int) (v ^ (v >>> 32)) + index + dk + (fd == null ? 0 : 1);
    }

    static int ref(int index, int q, long c, int dk, Object fd) {
        int k = (c != C_MIN | q == -1074)
                ? (int) (q * 661_971_961_083L >> 41)
                : (int) (q * 661_971_961_083L + (-274_743_187_321L) >> 41);
        int h = q + (int) (-k * 913_124_641_741L >> 38) + 2;
        long g1 = MU.G[k + 324 << 1], g0 = MU.G[k + 324 << 1 | 1];
        long v = (g1 ^ (g0 >>> 3)) + ((c << 2) << (h & 7));
        return (int) (v ^ (v >>> 32)) + index + dk + (fd == null ? 0 : 1);
    }

    final byte[] buf = new byte[8];
    // Hot callers that bake a direct call to toDec.
    int callA(int q, long c) { return toDec(buf, 3, q, c, -1, null); }
    int callB(int q, long c, Object fd) { return toDec(buf, 5, q, c, 2, fd); }

    static long bad;
    static Throwable first;

    static void check(LateBindToDecimalShape s, int q, long c, boolean b) {
        Object fd = b ? "fd" : null;
        try {
            int got = b ? s.callB(q, c, fd) : s.callA(q, c);
            int want = b ? ref(5, q, c, 2, fd) : ref(3, q, c, -1, null);
            if (got != want && bad++ < 4) System.out.println("MISMATCH q=" + q + " c=" + c + " got=" + got + " want=" + want);
        } catch (Throwable t) {
            bad++;
            if (first == null) first = t;
        }
    }

    public static void main(String[] a) throws Exception {
        int warm = a.length > 0 ? Integer.parseInt(a[0]) : 20000;
        LateBindToDecimalShape s = new LateBindToDecimalShape();
        java.util.Random r = new java.util.Random(5);
        // 1. Regular branch only: toDec and its callers compile; Rare is not initialized.
        for (int i = 0; i < warm; i++) check(s, r.nextInt(2000) - 1074, C_MIN + 1 + (r.nextLong() >>> 12), (i & 1) == 0);
        // 2. Initialize Rare and make f10q hot and compiled on its own.
        int acc = 0;
        for (int i = 0; i < 20000; i++) acc += Rare.f10q(i % 2000 - 1074);
        // 3. Rare branch through the compiled toDec, then everything, from 4 threads.
        Thread[] ts = new Thread[4];
        for (int t = 0; t < ts.length; t++) {
            final int tt = t;
            ts[t] = new Thread(() -> {
                LateBindToDecimalShape own = (tt & 1) == 0 ? s : new LateBindToDecimalShape();
                java.util.Random rr = new java.util.Random(17 + tt);
                for (int i = 0; i < 40000; i++) {
                    boolean irregular = (i % 3) == 0;
                    int q = rr.nextInt(2000) - 1073;
                    long c = irregular ? C_MIN : C_MIN + 1 + (rr.nextLong() >>> 12);
                    synchronized (LateBindToDecimalShape.class) { check(own, q, c, (i & 1) == 0); }
                }
            });
            ts[t].start();
        }
        for (Thread t : ts) t.join();
        if (first != null) {
            System.out.println("FAIL bad=" + bad + " first=" + first);
            for (StackTraceElement e : first.getStackTrace()) System.out.println("  at " + e);
        } else System.out.println(bad == 0 ? "PASS" : "FAIL bad=" + bad);
        if (acc == 42) System.out.println();
    }
}
