// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 77, lane L4: the cost of `interpreter::execute`'s
// own entry (the uncached path every native-to-Java re-entry, reflective call
// and Rust-side up-call takes), with and without
//
//   CRATONVM_EXECUTE_SHARED_METHOD_VIEW=1  the skip key and the frame name the
//       method with the class's and the method's own shared `Arc<str>`s (no
//       per-call class-name String, no fresh name `Arc`s), and the source file
//       is copied once.
//
// Rows (each a loop; every answer is checked, so the checksums are the loop
// counts on HotSpot and here):
//   mt-thrash-8192   8192 interned MethodTypes cycling through the
//                    `makeImpl` memo (4096 lines at most): under --jdk-only
//                    --nojit (nearly) every call is a memo miss that re-enters
//                    `makeImpl` through `invoke_special_bytecode_only*` ->
//                    `execute`
//   reflect-static   Method.invoke of a static two-int method
//   reflect-handlers Method.invoke of a static method with two handlers (a
//                    non-empty exception table)
//   user-key-get     HashMap.get with a user key (hashCode / equals up-calls
//                    from a `--compatible` map native, plain bytecode under
//                    --jdk-only)
//
// Output, all on STDOUT:
//   "row <name> min <ns> med <ns> ns/op"   every loop, min/median over reps
//   "checksum <name> <value>"               deterministic (rep 0)
// Lines containing "ns/" are timings; the others diff against HotSpot.
// Args: [reps] (default 5). The row-set finishes well under 60 s.
//
// Switch sets (interleave A and B, three runs each, medians):
//   --jdk-only --nojit, --compatible --nojit, --jdk-only (JIT), --compatible (JIT):
//     A: (none)
//     B: CRATONVM_EXECUTE_SHARED_METHOD_VIEW=1
//   plus, --jdk-only --nojit only, the wave-76 pair on top of B:
//     C: B + CRATONVM_EXECUTE_NO_JIT_GATE_WITHOUT_JIT=1 CRATONVM_NATIVE_OWN_BODY_HINT=1
// Expected: B shaves ~100-300 ns per re-entry (up to eight allocations) off
// mt-thrash-8192 and the reflect rows; JIT rows flat or a little better.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum mt-thrash-8192 40000
// checksum reflect-static 40000
// checksum reflect-handlers 40000
// checksum user-key-get 40000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodType;
import java.lang.reflect.Array;
import java.lang.reflect.Method;
import java.util.Arrays;
import java.util.HashMap;
import java.util.Locale;

public class L4W77ExecuteViewBench {
    interface Body {
        long run(int n) throws Throwable;
    }

    public static int add(int a, int b) {
        return a + b;
    }

    public static int guarded(int a) {
        try {
            if (a < 0) throw new IllegalStateException("negative");
            return a + 1;
        } catch (IllegalStateException e) {
            return -1;
        } catch (RuntimeException e) {
            return -2;
        }
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

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        int n = 40000;

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

        Method addM = L4W77ExecuteViewBench.class.getMethod("add", int.class, int.class);
        single("reflect-static", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if ((Integer) addM.invoke(null, i, 1) == i + 1) s++;
            return s;
        }, reps, n);

        Method guardedM = L4W77ExecuteViewBench.class.getMethod("guarded", int.class);
        single("reflect-handlers", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if ((Integer) guardedM.invoke(null, i) == i + 1) s++;
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
    }
}
