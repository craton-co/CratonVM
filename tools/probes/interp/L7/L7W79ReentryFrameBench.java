// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 79, lane L7: the cost of a native-to-Java re-entry
// (`interpreter::execute` and the frame it pushes), with and without
//
//   CRATONVM_EXECUTE_CACHED_REENTRY_FRAME=1  `execute` pushes a CACHED frame
//       from a per-VM method record instead of an OWNED one: no metadata box
//       or name copies, no padded-code memo lock and body compare per entry,
//       the quickened stream read from the record on every return into the
//       frame, and the record's fused-array decline memo.
//
// Rows (each a loop; every answer is checked, so the checksums are the loop
// counts or sums on HotSpot and here):
//   reflect-empty-static   Method.invoke of an empty static method
//   reflect-empty-virtual  Method.invoke of an empty instance method
//   mh-exact-static        MethodHandle.invokeExact of an empty static method
//                          (the invoke door is a native here)
//   mt-thrash-8192         8192 MethodTypes cycling through the `makeImpl`
//                          memo: under --jdk-only --nojit each call is a memo
//                          miss that re-enters `makeImpl` (a native calling a
//                          JDK body)
//   user-key-get           HashMap.get with a user key (a `--compatible` map
//                          native calls hashCode / equals back)
//   main-call              a loop written in `main` itself calling a tiny
//                          static per iteration (every return lands in the
//                          frame `execute` pushed for `main`)
//   main-refused-inc       `a[j] += h.f` in `main` (a fused array increment
//                          group the decoder refuses; the decline memo lives
//                          on a cached method record only)
//
// Output, all on STDOUT:
//   "row <name> min <ns> med <ns> ns/op"   every loop, min/median over reps
//   "checksum <name> <value>"               deterministic (rep 0)
// Lines containing "ns/" are timings; the others diff against HotSpot.
// Args: [reps] (default 5). The row-set finishes well under 60 s.
//
// Switch sets (interleave A and B, three runs each, medians), each of
//   --jdk-only --nojit, --compatible --nojit, --jdk-only, --compatible:
//     A: (none)
//     B: CRATONVM_EXECUTE_CACHED_REENTRY_FRAME=1
// Expected: B shaves 0.3-1 us off the reflect / mh / mt rows without the JIT
// (the re-entry is ~610 ns of an empty Method.invoke's ~2 us), 20-40 ns off
// main-call, main-refused-inc toward the CRATONVM_INTERP_FUSED_ARRAY_INCREMENT=0
// time; JIT rows flat or better (a compiled first-call door pushes no frame).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum reflect-empty-static 40000
// checksum reflect-empty-virtual 40000
// checksum mh-exact-static 40000
// checksum mt-thrash-8192 40000
// checksum user-key-get 40000
// checksum main-call 3500000
// checksum main-refused-inc 3000000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Array;
import java.lang.reflect.Method;
import java.util.Arrays;
import java.util.HashMap;
import java.util.Locale;

public class L7W79ReentryFrameBench {
    interface Body {
        long run(int n) throws Throwable;
    }

    public static void empty() {
    }

    public void emptyVirtual() {
    }

    static int tiny(int i) {
        return i & 7;
    }

    static final class Holder {
        int f = 3;
    }

    static final class Key {
        final int id;

        Key(int id) {
            this.id = id;
        }

        @Override
        public int hashCode() {
            return id * 31;
        }

        @Override
        public boolean equals(Object o) {
            return o instanceof Key k && k.id == id;
        }
    }

    static double median(double[] xs) {
        double[] c = xs.clone();
        Arrays.sort(c);
        int m = c.length / 2;
        return c.length % 2 == 1 ? c[m] : (c[m - 1] + c[m]) / 2;
    }

    static double min(double[] xs) {
        double best = Double.MAX_VALUE;
        for (double x : xs) best = Math.min(best, x);
        return best;
    }

    static void report(String name, double[] xs, long first) {
        System.out.printf(Locale.ROOT, "row %s min %.1f med %.1f ns/op%n", name, min(xs), median(xs));
        System.out.println("checksum " + name + " " + first);
    }

    static void single(String name, Body b, int reps, int n) throws Throwable {
        b.run(Math.max(2, n / 10));
        double[] xs = new double[reps];
        long first = 0;
        for (int rep = 0; rep < reps; rep++) {
            long t0 = System.nanoTime();
            long s = b.run(n);
            long t1 = System.nanoTime();
            xs[rep] = (t1 - t0) / (double) n;
            if (rep == 0) first = s;
        }
        report(name, xs, first);
    }

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        int n = 40000;

        Method emptyM = L7W79ReentryFrameBench.class.getMethod("empty");
        single("reflect-empty-static", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                if (emptyM.invoke(null) == null) s++;
            }
            return s;
        }, reps, n);

        Method emptyV = L7W79ReentryFrameBench.class.getMethod("emptyVirtual");
        L7W79ReentryFrameBench self = new L7W79ReentryFrameBench();
        single("reflect-empty-virtual", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                if (emptyV.invoke(self) == null) s++;
            }
            return s;
        }, reps, n);

        MethodHandle emptyH = MethodHandles.lookup()
                .findStatic(L7W79ReentryFrameBench.class, "empty", MethodType.methodType(void.class));
        single("mh-exact-static", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                emptyH.invokeExact();
                s++;
            }
            return s;
        }, reps, n);

        Class<?>[] pool = new Class<?>[32];
        for (int d = 1; d <= 16; d++) {
            pool[d - 1] = Array.newInstance(int.class, new int[d]).getClass();
            pool[d + 15] = Array.newInstance(long.class, new int[d]).getClass();
        }
        MethodType[] want = new MethodType[8192];
        for (int j = 0; j < 8192; j++) {
            want[j] = MethodType.methodType(void.class, pool[j & 31], pool[(j >> 5) & 31], pool[(j >> 10) & 7]);
        }
        single("mt-thrash-8192", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                int j = i & 8191;
                if (MethodType.methodType(void.class, pool[j & 31], pool[(j >> 5) & 31], pool[(j >> 10) & 7]) == want[j]) s++;
            }
            return s;
        }, reps, n);

        HashMap<Key, Integer> map = new HashMap<>();
        Key[] probes = new Key[256];
        for (int i = 0; i < 256; i++) {
            map.put(new Key(i), i);
            probes[i] = new Key(i);
        }
        single("user-key-get", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                Integer v = map.get(probes[i & 255]);
                if (v != null && v == (i & 255)) s++;
            }
            return s;
        }, reps, n);

        // The two loops below sit in `main` itself on purpose: `main`'s frame is
        // the one `execute` pushed, so these measure that frame's kind.
        int m = 1_000_000;
        {
            double[] xs = new double[reps];
            long first = 0;
            for (int rep = -1; rep < reps; rep++) {
                long t0 = System.nanoTime();
                long s = 0;
                for (int i = 0; i < m; i++) {
                    s += tiny(i);
                }
                long t1 = System.nanoTime();
                if (rep < 0) continue;
                xs[rep] = (t1 - t0) / (double) m;
                if (rep == 0) first = s;
            }
            report("main-call", xs, first);
        }
        {
            double[] xs = new double[reps];
            long first = 0;
            Holder h = new Holder();
            for (int rep = -1; rep < reps; rep++) {
                int[] a = new int[16];
                long t0 = System.nanoTime();
                for (int i = 0; i < m; i++) {
                    int j = i & 15;
                    a[j] += h.f;
                }
                long t1 = System.nanoTime();
                if (rep < 0) continue;
                long s = 0;
                for (int v : a) s += v;
                xs[rep] = (t1 - t0) / (double) m;
                if (rep == 0) first = s;
            }
            report("main-refused-inc", xs, first);
        }
    }
}
