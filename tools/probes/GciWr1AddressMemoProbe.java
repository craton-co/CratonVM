// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.text.DecimalFormat;
import java.text.DecimalFormatSymbols;
import java.util.Locale;

/**
 * gci i1/wr regression probe for the address-keyed native memos
 * ({@code r15w4-mh2-address-memo-epoch-misses-g1-cleanup-patch}): the
 * MethodHandle lanes and the {@code DecimalFormat} plan stamp now key on
 * {@code NativeContext::gc_address_epoch} instead of the collection count.
 * Each round mints fresh {@code MethodType}s, names and handles for three
 * methods, re-patterns a {@code DecimalFormat}, churns the heap and
 * collects every 25 rounds, so freed addresses are re-used while the memos
 * are warm. A memo answering for the wrong object shows as {@code mhBad} or
 * {@code fmtBad} above 0, or a different sum or hash. stdout is
 * deterministic (HotSpot 25 is the oracle); nothing goes to stderr.
 */
public class GciWr1AddressMemoProbe {
    public static int a(int x) {
        return x + 1;
    }

    public static int b(int x) {
        return x * 2;
    }

    public static int c(int x) {
        return x - 3;
    }

    static Object sink;

    public static void main(String[] args) throws Throwable {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 400;
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        String[] names = {"a", "b", "c"};
        String[] patterns = {"#,##0.00", "0.000", "00000", "#.##"};
        DecimalFormatSymbols sym = DecimalFormatSymbols.getInstance(Locale.ROOT);
        long mhSum = 0;
        int mhBad = 0;
        int fmtBad = 0;
        long fmtHash = 0;
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < names.length; i++) {
                MethodType mt = MethodType.methodType(int.class, int.class);
                MethodHandle mh = lookup.findStatic(GciWr1AddressMemoProbe.class, new String(names[i]), mt);
                int got = (int) mh.invokeExact(r);
                int want = i == 0 ? r + 1 : (i == 1 ? r * 2 : r - 3);
                if (got != want) {
                    mhBad++;
                }
                mhSum += got;
            }
            double v = r * 1234.5678;
            DecimalFormat df = new DecimalFormat(patterns[r % patterns.length], sym);
            String s1 = df.format(v);
            df.applyPattern(new String(patterns[(r + 1) % patterns.length]));
            String s2 = df.format(v);
            DecimalFormat fresh = new DecimalFormat(patterns[(r + 1) % patterns.length], sym);
            if (!s2.equals(fresh.format(v))) {
                fmtBad++;
            }
            fmtHash = fmtHash * 31 + s1.hashCode() * 17L + s2.hashCode();
            byte[][] junk = new byte[64][];
            for (int k = 0; k < junk.length; k++) {
                junk[k] = new byte[1024];
            }
            sink = junk;
            if (r % 25 == 24) {
                System.gc();
            }
        }
        System.out.println("address-memo: rounds=" + rounds + " mhSum=" + mhSum + " mhBad=" + mhBad
                + " fmtBad=" + fmtBad + " fmtHash=" + fmtHash);
    }
}
