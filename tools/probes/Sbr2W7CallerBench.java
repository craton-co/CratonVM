// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandles;
import java.lang.ref.WeakReference;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import javax.crypto.Mac;
import javax.crypto.spec.SecretKeySpec;

/**
 * SBR round 2, wave 7, lane {@code caller}: the caller-sensitive reflection rows, split so a cost can
 * be placed at a layer, reported as MIN and MEDIAN ns/op over many short reps (the machine swings
 * badly between reps; the min is the reproducible floor).
 *
 * <pre>
 *   cratonvm --jdk-only -cp . Sbr2W7CallerBench [reps] [iters] [rows,comma,separated]
 * </pre>
 *
 * Rows: getInt/setInt/get/set (public field, non-override), getIntAcc/setIntAcc (setAccessible(true):
 * no caller walk, no access check), getIntStatic, lookup ({@code MethodHandles.lookup()}: one
 * {@code getCallerClass}), getClass, refersTo, swCaller, swWalk (depth ~5), macNew
 * ({@code Mac.getInstance}), macInit ({@code init}+{@code doFinal} 64 bytes), and the D20 variants
 * (the same call under a 20-deep recursion).
 */
public class Sbr2W7CallerBench {
    public static class H {
        public int i = 1;
        public Object o = "x";
        public static int si = 2;
    }

    static long sink;

    interface Row {
        void run(int n) throws Throwable;
    }

    static void deep(int d, Row r, int k) throws Throwable {
        if (d == 0) {
            r.run(k);
            return;
        }
        deep(d - 1, r, k);
        sink += d & 1;
    }

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 15;
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 20_000;
        String only = args.length > 2 ? "," + args[2] + "," : null;
        H h = new H();
        Field fi = H.class.getField("i");
        Field fo = H.class.getField("o");
        Field fa = H.class.getField("i");
        fa.setAccessible(true);
        Field fs = H.class.getField("si");
        WeakReference<Object> wr = new WeakReference<>(h);
        Object other = new Object();
        StackWalker sw = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE);
        SecretKeySpec key = new SecretKeySpec(new byte[16], "HmacSHA256");
        Mac mac = Mac.getInstance("HmacSHA256");
        byte[] data = new byte[64];
        String[] names = {
            "getInt", "setInt", "get", "set", "getIntAcc", "setIntAcc", "getIntStatic", "lookup", "getClass",
            "refersTo", "swCaller", "swWalk", "macNew", "macInit", "getIntD20", "swCallerD20"
        };
        Row[] rows = {
            k -> { for (int j = 0; j < k; j++) sink += fi.getInt(h); },
            k -> { for (int j = 0; j < k; j++) fi.setInt(h, j); },
            k -> { for (int j = 0; j < k; j++) sink += fo.get(h).hashCode(); },
            k -> { for (int j = 0; j < k; j++) fo.set(h, other); },
            k -> { for (int j = 0; j < k; j++) sink += fa.getInt(h); },
            k -> { for (int j = 0; j < k; j++) fa.setInt(h, j); },
            k -> { for (int j = 0; j < k; j++) sink += fs.getInt(null); },
            k -> { for (int j = 0; j < k; j++) sink += MethodHandles.lookup().hashCode() & 1; },
            k -> { for (int j = 0; j < k; j++) sink += h.getClass().hashCode() & 1; },
            k -> { for (int j = 0; j < k; j++) sink += wr.refersTo(other) ? 1 : 0; },
            k -> { for (int j = 0; j < k; j++) sink += sw.getCallerClass().hashCode() & 1; },
            k -> { for (int j = 0; j < k; j++) sink += sw.walk(s -> s.limit(5).count()); },
            k -> { for (int j = 0; j < k / 10; j++) sink += Mac.getInstance("HmacSHA256").hashCode() & 1; },
            k -> { for (int j = 0; j < k / 10; j++) { mac.init(key); sink += mac.doFinal(data)[0]; } },
            k -> deep(20, kk -> { for (int j = 0; j < kk; j++) sink += fi.getInt(h); }, k),
            k -> deep(20, kk -> { for (int j = 0; j < kk; j++) sink += sw.getCallerClass().hashCode() & 1; }, k),
        };
        for (int x = 0; x < rows.length; x++) {
            if (only != null && !only.contains("," + names[x] + ",")) continue;
            int div = (names[x].startsWith("mac")) ? 10 : 1;
            long[] t = new long[reps];
            for (int r = 0; r < reps; r++) {
                long s = System.nanoTime();
                rows[x].run(n);
                t[r] = (System.nanoTime() - s) / (n / div);
            }
            long[] w = java.util.Arrays.copyOfRange(t, Math.min(3, reps - 1), reps);
            java.util.Arrays.sort(w);
            System.out.println(names[x] + " min=" + w[0] + " med=" + w[w.length / 2] + " ns");
        }
        System.out.println("sink " + (sink != 0));
    }
}
