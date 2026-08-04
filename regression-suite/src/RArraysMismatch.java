import java.util.Arrays;

/**
 * Regression: {@code java.util.Arrays.mismatch}/{@code equals}/{@code compare}
 * over every primitive array type, asserted only AFTER the helper behind them
 * has been JIT-compiled.
 *
 * These all funnel through {@code jdk.internal.util.ArraysSupport.mismatch},
 * which passes {@code Unsafe.ARRAY_*_BASE_OFFSET} — declared {@code long} in
 * JDK 25 — to the {@code vectorizedMismatch} intrinsic. CratonVM's post-clinit
 * fixup used to inject those nine statics as a 32-bit {@code Value::Int}; the
 * interpreter widened them on read and was fine, while JIT-compiled code
 * lowers {@code getstatic …:J} to a 64-bit load of the slot and so got a
 * garbage offset. The intrinsic's range check then failed, it returned "no
 * mismatch", and {@code Arrays.equals(long[], long[])} answered TRUE for
 * arrays that differ — silently, with no exception anywhere. Downstream that
 * showed up as an H2 unique index reporting a collision that did not exist.
 *
 * Two things this vector deliberately does NOT do, because either one would
 * make it pass on the broken VM:
 *
 *   - assert only that unequal arrays compare unequal. The bug flipped the
 *     mismatch INDEX to -1, and {@code Arrays.equals(int[],int[])} does not
 *     route through the same helper, so an equals-only check stays green.
 *   - run the assertions cold. Interpreted execution is correct; the wrong
 *     answer only appears once the method is compiled, which is what the warm
 *     phase below is for.
 *
 * int[] and long[] are the types that regressed — byte[] and char[] have
 * native overrides and never reach this bytecode — but all six are covered so
 * a future change to the override list cannot open a hole silently.
 */
public class RArraysMismatch {
    static int checks = 0;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    static long seed = 88172645463325252L;

    static long rnd() {
        seed ^= seed << 13;
        seed ^= seed >>> 7;
        seed ^= seed << 17;
        return seed;
    }

    /** Allocation-free warm phase: compiles every mismatch overload used below. */
    static long warm(int iterations) {
        byte[] ba = new byte[24];
        byte[] bb = new byte[24];
        char[] ca = new char[24];
        char[] cb = new char[24];
        short[] sa = new short[24];
        short[] sb = new short[24];
        int[] ia = new int[24];
        int[] ib = new int[24];
        long[] la = new long[24];
        long[] lb = new long[24];
        double[] da = new double[24];
        double[] db = new double[24];
        long acc = 0;
        for (int it = 0; it < iterations; it++) {
            int flip = it % 24;
            for (int i = 0; i < 24; i++) {
                ba[i] = bb[i] = (byte) (i + 1);
                ca[i] = cb[i] = (char) (i + 1);
                sa[i] = sb[i] = (short) (i + 1);
                ia[i] = ib[i] = i + 1;
                la[i] = lb[i] = i + 1;
                da[i] = db[i] = i + 1;
            }
            bb[flip] = (byte) ~ba[flip];
            cb[flip] = (char) (ca[flip] ^ 0x4321);
            sb[flip] = (short) ~sa[flip];
            ib[flip] = ~ia[flip];
            lb[flip] = ~la[flip];
            db[flip] = -da[flip] - 1;
            acc += Arrays.mismatch(ba, bb) + Arrays.mismatch(ca, cb) + Arrays.mismatch(sa, sb)
                    + Arrays.mismatch(ia, ib) + Arrays.mismatch(la, lb) + Arrays.mismatch(da, db)
                    + Arrays.mismatch(ia, 1, 24, ib, 1, 24) + Arrays.mismatch(la, 1, 24, lb, 1, 24);
        }
        return acc;
    }

    public static void main(String[] args) {
        long acc = warm(200_000);

        // Exhaustive scan, now against compiled code. Every length from 1 to 24
        // and every mismatch position: the defect answered -1 for every
        // position except 0, which the `if (a[0] != b[0])` early-out covers, so
        // a scan that only checked position 0 would also pass on the broken VM.
        for (int len = 1; len <= 24; len++) {
            for (int flip = 0; flip < len; flip++) {
                byte[] ba = new byte[len];
                byte[] bb = new byte[len];
                char[] ca = new char[len];
                char[] cb = new char[len];
                short[] sa = new short[len];
                short[] sb = new short[len];
                int[] ia = new int[len];
                int[] ib = new int[len];
                long[] la = new long[len];
                long[] lb = new long[len];
                double[] da = new double[len];
                double[] db = new double[len];
                for (int i = 0; i < len; i++) {
                    ba[i] = bb[i] = (byte) (i + 1);
                    ca[i] = cb[i] = (char) (i + 1);
                    sa[i] = sb[i] = (short) (i + 1);
                    ia[i] = ib[i] = i + 1;
                    la[i] = lb[i] = i + 1;
                    da[i] = db[i] = i + 1;
                }
                bb[flip] = (byte) ~ba[flip];
                cb[flip] = (char) (ca[flip] ^ 0x4321);
                sb[flip] = (short) ~sa[flip];
                ib[flip] = ~ia[flip];
                lb[flip] = ~la[flip];
                db[flip] = -da[flip] - 1;

                check(Arrays.mismatch(ba, bb) == flip,
                        "byte[] len=" + len + " flip=" + flip + " got=" + Arrays.mismatch(ba, bb));
                check(Arrays.mismatch(ca, cb) == flip,
                        "char[] len=" + len + " flip=" + flip + " got=" + Arrays.mismatch(ca, cb));
                check(Arrays.mismatch(sa, sb) == flip,
                        "short[] len=" + len + " flip=" + flip + " got=" + Arrays.mismatch(sa, sb));
                check(Arrays.mismatch(ia, ib) == flip,
                        "int[] len=" + len + " flip=" + flip + " got=" + Arrays.mismatch(ia, ib));
                check(Arrays.mismatch(la, lb) == flip,
                        "long[] len=" + len + " flip=" + flip + " got=" + Arrays.mismatch(la, lb));
                check(Arrays.mismatch(da, db) == flip,
                        "double[] len=" + len + " flip=" + flip + " got=" + Arrays.mismatch(da, db));

                // equals must agree with mismatch, in both directions.
                check(!Arrays.equals(ba, bb), "byte[] equals len=" + len + " flip=" + flip);
                check(!Arrays.equals(ca, cb), "char[] equals len=" + len + " flip=" + flip);
                check(!Arrays.equals(ia, ib), "int[] equals len=" + len + " flip=" + flip);
                check(!Arrays.equals(la, lb), "long[] equals len=" + len + " flip=" + flip);
                check(Arrays.equals(ia, ia.clone()), "int[] self equals len=" + len);
                check(Arrays.equals(la, la.clone()), "long[] self equals len=" + len);
                check(Arrays.mismatch(la, la.clone()) == -1, "long[] self mismatch len=" + len);

                // compare() shares the helper and must order by the first
                // differing element rather than reporting "equal".
                check(Arrays.compare(ia, ib) != 0, "int[] compare len=" + len + " flip=" + flip);
                check(Arrays.compare(la, lb) != 0, "long[] compare len=" + len + " flip=" + flip);

                acc += Arrays.mismatch(ia, ib) + Arrays.mismatch(la, lb);
            }
        }

        // Ranged overload: fromIndex is folded into the Unsafe base offset, so
        // it reaches the intrinsic through a different argument path than the
        // 3-argument form.
        for (int rep = 0; rep < 20_000; rep++) {
            int len = 8 + (int) Math.floorMod(rnd(), 17);
            int[] a = new int[len];
            int[] b = new int[len];
            long[] la = new long[len];
            long[] lb = new long[len];
            for (int i = 0; i < len; i++) {
                a[i] = b[i] = i * 7 + 1;
                la[i] = lb[i] = i * 7L + 1;
            }
            int from = (int) Math.floorMod(rnd(), len - 4);
            int flip = from + 1 + (int) Math.floorMod(rnd(), len - from - 1);
            b[flip] = -1;
            lb[flip] = -1;
            check(Arrays.mismatch(a, from, len, b, from, len) == flip - from,
                    "int[] ranged from=" + from + " flip=" + flip
                            + " got=" + Arrays.mismatch(a, from, len, b, from, len));
            check(Arrays.mismatch(la, from, len, lb, from, len) == flip - from,
                    "long[] ranged from=" + from + " flip=" + flip
                            + " got=" + Arrays.mismatch(la, from, len, lb, from, len));
            acc += flip - from;
        }

        System.out.println("CK acc=" + acc);
        System.out.println("PASS RArraysMismatch (" + checks + " checks)");
    }
}
