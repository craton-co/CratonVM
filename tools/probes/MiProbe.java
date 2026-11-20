// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// One Method.invoke on a static final Method in a small hot loop (`MiProbe <n>`).
import java.lang.reflect.Method;

public class MiProbe {
    public static final class Plain { public int price(int item) { return item * 3; } }
    static final Plain P = new Plain();
    static final Method M;
    static { try { M = Plain.class.getMethod("price", int.class); } catch (Exception e) { throw new Error(e); } }

    static long loop(int n) throws Exception {
        long s = 0;
        for (int i = 0; i < n; i++) s += (Integer) M.invoke(P, i);
        return s;
    }

    public static void main(String[] a) throws Exception {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 20000;
        for (int rep = 0; rep < 6; rep++) {
            long t0 = System.nanoTime(); long s = loop(n); long t = System.nanoTime() - t0;
            System.out.printf("rep%d %.1f ns/op %d%n", rep, t / (double) n, s);
        }
    }
}
