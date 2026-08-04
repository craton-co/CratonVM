import java.util.Arrays;

/**
 * Pin down the JIT defect that BatchTest's unique-constraint violation bisects
 * to: with only jdk/internal/util/ArraysSupport allowed to compile, denying
 * ArraysSupport.mismatch makes the failure vanish, so the compiled bytecode of
 * that method answers wrongly.
 *
 * Every java.util.Arrays.equals / mismatch / compare over a primitive array
 * funnels through it, so a wrong answer means "two different byte arrays are
 * equal" — which is exactly the shape of H2's MVStore key comparisons going
 * wrong and a unique index reporting a collision that does not exist.
 *
 * Checks every overload against a hand-written reference loop that the JIT
 * cannot route through ArraysSupport. Prints the first divergences and exits 1.
 */
public class ArraysMismatchProbe {

    static int refMismatch(byte[] a, byte[] b) {
        int n = Math.min(a.length, b.length);
        for (int i = 0; i < n; i++) {
            if (a[i] != b[i]) {
                return i;
            }
        }
        return a.length == b.length ? -1 : n;
    }

    static int refMismatch(char[] a, char[] b) {
        int n = Math.min(a.length, b.length);
        for (int i = 0; i < n; i++) {
            if (a[i] != b[i]) {
                return i;
            }
        }
        return a.length == b.length ? -1 : n;
    }

    static int refMismatch(int[] a, int[] b) {
        int n = Math.min(a.length, b.length);
        for (int i = 0; i < n; i++) {
            if (a[i] != b[i]) {
                return i;
            }
        }
        return a.length == b.length ? -1 : n;
    }

    static int refMismatch(long[] a, long[] b) {
        int n = Math.min(a.length, b.length);
        for (int i = 0; i < n; i++) {
            if (a[i] != b[i]) {
                return i;
            }
        }
        return a.length == b.length ? -1 : n;
    }

    static long seed = 88172645463325252L;

    static long rnd() {
        seed ^= seed << 13;
        seed ^= seed >>> 7;
        seed ^= seed << 17;
        return seed;
    }

    public static void main(String[] args) {
        final int reps = args.length > 0 ? Integer.parseInt(args[0]) : 200000;
        final int maxLen = args.length > 1 ? Integer.parseInt(args[1]) : 40;
        long bad = 0;
        long checked = 0;

        for (int rep = 0; rep < reps; rep++) {
            int len = 1 + (int) Math.floorMod(rnd(), maxLen);

            byte[] ba = new byte[len];
            byte[] bb = new byte[len];
            for (int i = 0; i < len; i++) {
                ba[i] = (byte) rnd();
                bb[i] = ba[i];
            }
            // Flip exactly one element so the arrays MUST differ at a known index.
            int flip = (int) Math.floorMod(rnd(), len);
            bb[flip] = (byte) (ba[flip] ^ 0x5a);

            char[] ca = new char[len];
            char[] cb = new char[len];
            int[] ia = new int[len];
            int[] ib = new int[len];
            long[] la = new long[len];
            long[] lb = new long[len];
            for (int i = 0; i < len; i++) {
                ca[i] = (char) (ba[i] & 0xff);
                cb[i] = ca[i];
                ia[i] = ba[i];
                ib[i] = ia[i];
                la[i] = ba[i];
                lb[i] = la[i];
            }
            cb[flip] = (char) (ca[flip] ^ 0x5a);
            ib[flip] = ia[flip] ^ 0x5a;
            lb[flip] = la[flip] ^ 0x5aL;

            bad += check("byte[]", rep, len, flip, Arrays.mismatch(ba, bb), refMismatch(ba, bb),
                    Arrays.equals(ba, bb));
            bad += check("char[]", rep, len, flip, Arrays.mismatch(ca, cb), refMismatch(ca, cb),
                    Arrays.equals(ca, cb));
            bad += check("int[]", rep, len, flip, Arrays.mismatch(ia, ib), refMismatch(ia, ib),
                    Arrays.equals(ia, ib));
            bad += check("long[]", rep, len, flip, Arrays.mismatch(la, lb), refMismatch(la, lb),
                    Arrays.equals(la, lb));
            checked += 4;

            // Ranged overload: same arrays, offset windows. ArraysSupport's
            // 5-argument mismatch adds fromIndex to the Unsafe base offset, so
            // a non-zero offset exercises a different argument path.
            if (len > 4) {
                int from = 1 + (int) Math.floorMod(rnd(), len - 3);
                int to = len;
                int got = Arrays.mismatch(ba, from, to, bb, from, to);
                int want = -1;
                for (int i = from; i < to; i++) {
                    if (ba[i] != bb[i]) {
                        want = i - from;
                        break;
                    }
                }
                checked++;
                if (got != want) {
                    bad++;
                    if (bad <= 20) {
                        System.out.println("RANGED-MISMATCH rep=" + rep + " len=" + len
                                + " from=" + from + " flip=" + flip
                                + " got=" + got + " want=" + want);
                    }
                }
            }

            // Equal arrays must report -1 / true. A defect that makes unequal
            // arrays compare equal often also makes equal arrays report a
            // spurious mismatch, so check both directions.
            byte[] bc = ba.clone();
            checked += 2;
            if (Arrays.mismatch(ba, bc) != -1 || !Arrays.equals(ba, bc)) {
                bad++;
                if (bad <= 20) {
                    System.out.println("EQUAL-ARRAYS rep=" + rep + " len=" + len
                            + " mismatch=" + Arrays.mismatch(ba, bc)
                            + " equals=" + Arrays.equals(ba, bc));
                }
            }
        }
        System.out.println("ArraysMismatchProbe reps=" + reps + " checked=" + checked + " bad=" + bad);
        if (bad != 0) {
            System.exit(1);
        }
    }

    static long check(String kind, int rep, int len, int flip, int got, int want, boolean equals) {
        boolean ok = got == want && !equals;
        if (ok) {
            return 0;
        }
        System.out.println("MISMATCH-WRONG " + kind + " rep=" + rep + " len=" + len
                + " flipAt=" + flip + " Arrays.mismatch=" + got + " reference=" + want
                + " Arrays.equals=" + equals);
        return 1;
    }
}
