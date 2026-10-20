// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.Reference;
import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;
import java.util.WeakHashMap;

/**
 * SBR round 4, lane {@code satb}: what retiring {@code Reference.get} costs. Prints
 * {@code <row> <ns/op>} per row (best of the measured rounds, after warm-up); run interleaved
 * against the native arm of the SAME binary ({@code CRATONVM_UNRETIRE_NATIVE_SHADOW=
 * java/lang/ref/Reference.get,java/lang/ref/WeakReference.get,java/lang/ref/SoftReference.get}).
 */
public final class Sbr4SatbBench {
    static final Object K = new Object();
    static volatile long sink;

    interface Row {
        long run(int n);
    }

    static void bench(String name, int n, Row r) {
        for (int i = 0; i < 5; i++) {
            sink += r.run(n);
        }
        double best = Double.MAX_VALUE;
        for (int i = 0; i < 7; i++) {
            long t0 = System.nanoTime();
            sink += r.run(n);
            double ns = (System.nanoTime() - t0) / (double) n;
            best = Math.min(best, ns);
        }
        System.out.printf("%s %.1f%n", name, best);
    }

    public static void main(String[] a) {
        int n = Integer.getInteger("n", 2_000_000);
        WeakReference<Object> w = new WeakReference<>(K);
        SoftReference<Object> s = new SoftReference<>(K);
        Reference<Object> rw = w;
        bench("weak.get", n, k -> {
            long c = 0;
            for (int i = 0; i < k; i++) {
                if (w.get() == K) c++;
            }
            return c;
        });
        bench("soft.get", n, k -> {
            long c = 0;
            for (int i = 0; i < k; i++) {
                if (s.get() == K) c++;
            }
            return c;
        });
        bench("Reference.get", n, k -> {
            long c = 0;
            for (int i = 0; i < k; i++) {
                if (rw.get() == K) c++;
            }
            return c;
        });
        ThreadLocal<Object> tl = new ThreadLocal<>();
        tl.set(K);
        bench("ThreadLocal.get", n, k -> {
            long c = 0;
            for (int i = 0; i < k; i++) {
                if (tl.get() == K) c++;
            }
            return c;
        });
        bench("ThreadLocal.set", n / 4, k -> {
            long c = 0;
            for (int i = 0; i < k; i++) {
                tl.set(K);
                c++;
            }
            return c;
        });
        WeakHashMap<Object, Object> whm = new WeakHashMap<>();
        Object[] keys = new Object[64];
        for (int i = 0; i < keys.length; i++) {
            keys[i] = new Object();
            whm.put(keys[i], keys[i]);
        }
        bench("WeakHashMap.get", n / 2, k -> {
            long c = 0;
            for (int i = 0; i < k; i++) {
                if (whm.get(keys[i & 63]) != null) c++;
            }
            return c;
        });
        bench("new WeakReference+get", n / 2, k -> {
            long c = 0;
            for (int i = 0; i < k; i++) {
                if (new WeakReference<>(K).get() == K) c++;
            }
            return c;
        });
    }
}
