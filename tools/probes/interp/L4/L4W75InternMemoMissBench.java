// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 75, lane L4: the miss side of
// CRATONVM_MH_METHOD_TYPE_INTERN_MEMO (a reviewed `Intrinsic` over
// `MethodType.makeImpl` answering a type this thread interned before from a
// per-thread memo). Wave 74's memo (128 direct-mapped lines) won on repeats
// but lost on any working set it could not hold (`mt-intern-varied` +28%
// under --nojit) and on every finder row under the JIT. Wave 75 registers it
// only without a JIT and makes the memo two-way, growing on thrash.
//
// Rows (each a loop of `MethodType.methodType` calls; every answer is checked
// against the instance interned before the loop, so the checksums are the
// loop counts on HotSpot and here):
//   mt-repeat          one type, always the same (the memo's best case)
//   mt-working-64      64 types cycling (fits a fresh memo)
//   mt-working-256     256 types cycling (needs one growth)
//   mt-working-1024    1024 types cycling (the W74 `mt-intern-varied` shape)
//   mt-cold            a type never interned before on every call (32768
//                      distinct types, each used once): the pure miss
//                      penalty, which no memo size answers
//   find-only          lk.findGetter(Box, "v", int): two intern probes
//   unreflect-getter   lk.unreflectGetter(field)
//
// Output, all on STDOUT:
//   "row <name> min <ns> med <ns> ns/op"         every loop, min/median over reps
//   "checksum <name> <value>"                     deterministic (rep 0)
// Lines containing "ns/" are timings; the others diff against HotSpot.
// Args: [reps] (default 5). Each row-set finishes well under 60 s.
//
// Switch sets (interleave A and B, three runs each, medians), with
// `--jdk-only --nojit` (where the memo now registers) and `--jdk-only` with
// the JIT (control: the memo is not registered there, every row must be flat):
//   A: (none)
//   B: CRATONVM_MH_METHOD_TYPE_INTERN_MEMO=1
// Expected under B, --nojit: mt-repeat / mt-working-64 ~10x faster;
// mt-working-256 / -1024 faster after the memo grows (W74: -1024 was 28%
// slower); mt-cold at most one native crossing slower (the irreducible miss
// penalty: a crossing plus a native re-entry into the body).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum mt-repeat 40000
// checksum mt-working-64 40000
// checksum mt-working-256 40000
// checksum mt-working-1024 40000
// checksum mt-cold 5000
// checksum find-only 4000
// checksum unreflect-getter 4000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Array;
import java.lang.reflect.Field;
import java.util.Arrays;
import java.util.Locale;

public class L4W75InternMemoMissBench {
    public static class Box {
        public int v = 3;
    }

    interface Body {
        long run(int n) throws Throwable;
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
        System.out.printf(Locale.ROOT, "row %s min %.1f med %.1f ns/op%n", name, min(xs), median(xs));
        System.out.println("checksum " + name + " " + first);
    }

    static Body working(Class<?>[] pool, int size) {
        MethodType[] want = new MethodType[size];
        for (int j = 0; j < size; j++) want[j] = MethodType.methodType(pool[j & 31], pool[(j >> 5) & 31]);
        int mask = size - 1;
        return k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                int j = i & mask;
                if (MethodType.methodType(pool[j & 31], pool[(j >> 5) & 31]) == want[j]) s++;
            }
            return s;
        };
    }

    static int coldCursor = 0;

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        MethodHandles.Lookup lk = MethodHandles.lookup();
        Field vField = Box.class.getField("v");

        // 32 classes (int and long arrays of 1..16 dimensions).
        Class<?>[] pool = new Class<?>[32];
        for (int d = 1; d <= 16; d++) {
            pool[d - 1] = Array.newInstance(int.class, new int[d]).getClass();
            pool[d + 15] = Array.newInstance(long.class, new int[d]).getClass();
        }
        MethodType one = MethodType.methodType(int.class, Box.class);

        int n = 40000;
        int m = 4000;
        int cold = 5000;
        single("mt-repeat", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (MethodType.methodType(int.class, Box.class) == one) s++;
            return s;
        }, reps, n);
        single("mt-working-64", working(pool, 64), reps, n);
        single("mt-working-256", working(pool, 256), reps, n);
        single("mt-working-1024", working(pool, 1024), reps, n);
        // 32^3 = 32768 three-parameter types; warm-up plus reps use at most
        // cold / 10 + reps * cold of them, each once (reps <= 6).
        single("mt-cold", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                int c = coldCursor++ & 32767;
                MethodType t = MethodType.methodType(void.class, pool[c & 31], pool[(c >> 5) & 31], pool[(c >> 10) & 31]);
                if (t.parameterCount() == 3) s++;
            }
            return s;
        }, reps, cold);
        single("find-only", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findGetter(Box.class, "v", int.class) != null) s++;
            return s;
        }, reps, m);
        single("unreflect-getter", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.unreflectGetter(vField) != null) s++;
            return s;
        }, reps, m);
    }
}
