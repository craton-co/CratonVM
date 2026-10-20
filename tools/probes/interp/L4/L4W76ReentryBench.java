// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 76, lane L4: the cost of a native re-entering
// Java, and of `--compatible`'s `MethodType.methodType` bridges.
//
//   CRATONVM_EXECUTE_NO_JIT_GATE_WITHOUT_JIT=1  `interpreter::execute` skips
//       its first-call JIT gate block in a VM without a JIT (three `Arc<str>`
//       allocations and two read locks per re-entry).
//   CRATONVM_NATIVE_OWN_BODY_HINT=1  the `makeImpl` memo's miss re-enters the
//       JDK body by its declared method index (no by-name hierarchy walk, no
//       by-name scan in `execute`).
//   CRATONVM_MH_COMPAT_METHOD_TYPE_BYTECODE=1  `--compatible` over a real JDK
//       runs the JDK's four `methodType` factories (one `makeImpl`) instead of
//       a bridge that re-enters Java up to three times.
//
// Rows (each a loop; every answer is checked, so the checksums are the loop
// counts on HotSpot and here):
//   mt-repeat        one type, always the same (memo hit under --jdk-only --nojit)
//   mt-thrash-8192   8192 interned types cycling: more than the memo's 4096
//                    lines, so (nearly) every call misses the memo and
//                    re-enters a `makeImpl` whose intern probe hits -- the
//                    re-entry penalty with a cheap body
//   mt-cold          a type never interned before on every call (the body's
//                    full cost plus one re-entry)
//   mt-array         methodType(Class, Class[]) of one interned type
//   find-only        lk.findGetter(Box, "v", int)
//
// Output, all on STDOUT:
//   "row <name> min <ns> med <ns> ns/op"   every loop, min/median over reps
//   "checksum <name> <value>"               deterministic (rep 0)
// Lines containing "ns/" are timings; the others diff against HotSpot.
// Args: [reps] (default 5). The row-set finishes well under 60 s.
//
// Switch sets (interleave, three runs each, medians):
//   --jdk-only --nojit:
//     A: (none)
//     B: CRATONVM_EXECUTE_NO_JIT_GATE_WITHOUT_JIT=1
//     C: CRATONVM_EXECUTE_NO_JIT_GATE_WITHOUT_JIT=1 CRATONVM_NATIVE_OWN_BODY_HINT=1
//     D: CRATONVM_MH_METHOD_TYPE_INTERN_MEMO=0 (the no-memo baseline: the miss
//        penalty is mt-thrash-8192(A..C) - mt-thrash-8192(D))
//   --compatible --nojit and --compatible (JIT):
//     A: (none)
//     E: CRATONVM_MH_COMPAT_METHOD_TYPE_BYTECODE=1
//     F: E plus CRATONVM_EXECUTE_NO_JIT_GATE_WITHOUT_JIT=1 (--nojit only)
//   --jdk-only (JIT): A vs C is the control (neither switch acts with a JIT).
// Expected: B and C shave mt-thrash-8192 / mt-cold by a few hundred ns each
// (the miss penalty toward one crossing); E takes the --compatible mt-* rows
// from a crossing plus two or three re-entries to one interpreted makeImpl.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum mt-repeat 40000
// checksum mt-thrash-8192 40000
// checksum mt-cold 5000
// checksum mt-array 40000
// checksum find-only 4000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Array;
import java.util.Arrays;
import java.util.Locale;

public class L4W76ReentryBench {
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

    static int coldCursor = 0;

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        MethodHandles.Lookup lk = MethodHandles.lookup();

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

        // 8192 three-parameter types (the last parameter from the first 8
        // classes), interned once before the loop; the loop walks them in a
        // fixed order, so a memo of at most 4096 lines keeps missing.
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

        // Types with a FOURTH parameter, never interned before (32^3 = 32768
        // of them; warm-up plus reps use at most cold / 10 + reps * cold).
        single("mt-cold", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                int c = coldCursor++ & 32767;
                MethodType t = MethodType.methodType(int.class, pool[c & 31], pool[(c >> 5) & 31], pool[(c >> 10) & 31], int.class);
                if (t.parameterCount() == 4) s++;
            }
            return s;
        }, reps, cold);

        Class<?>[] arr = {Box.class};
        single("mt-array", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (MethodType.methodType(int.class, arr) == one) s++;
            return s;
        }, reps, n);

        single("find-only", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findGetter(Box.class, "v", int.class) != null) s++;
            return s;
        }, reps, m);
    }
}
