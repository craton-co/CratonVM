// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 77, lane L4: the cost of a repeat `Lookup` field
// finder, with and without
//
//   CRATONVM_MH_FINDER_MINT_MEMO=1  (--jdk-only over a real JDK, no JIT) the
//       four field finders are `Intrinsic`s: the first call per key runs the
//       JDK body, a repeat mints a fresh accessor and MemberName from it.
//
// Rows (each a loop; every answer is checked, so the checksums are the loop
// counts on HotSpot and here):
//   find-getter          lk.findGetter(Box, "v", int)
//   find-setter          lk.findSetter(Box, "v", int)
//   find-static-getter   lk.findStaticGetter(S, "s", int)
//   find-static-setter   lk.findStaticSetter(S, "s", int)
//   find-two-lookups     findGetter alternating between two lookups (two keys)
//   find-and-invoke      findGetter then invokeExact (a minted handle runs)
//   find-inherited       findStaticGetter(Q, "p", int), p declared by Q's
//                        superclass (declined: always the body; the control)
//
// Output, all on STDOUT:
//   "row <name> min <ns> med <ns> ns/op"   every loop, min/median over reps
//   "checksum <name> <value>"               deterministic (rep 0)
// Lines containing "ns/" are timings; the others diff against HotSpot.
// Args: [reps] (default 5). The row-set finishes well under 60 s.
//
// Switch sets (interleave A and B, three runs each, medians):
//   --jdk-only --nojit:  A: (none)   B: CRATONVM_MH_FINDER_MINT_MEMO=1
//   --jdk-only (JIT):    A vs B is the control (the switch registers nothing
//                        in a VM with a JIT)
//   --compatible --nojit: A vs B control (nothing registered)
// Expected: B takes the first six rows under --jdk-only --nojit from ~31-37 us
// toward ~2-4 us (two allocations and a dozen field copies); find-inherited
// flat (the decline costs one memo probe).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum find-getter 4000
// checksum find-setter 4000
// checksum find-static-getter 4000
// checksum find-static-setter 4000
// checksum find-two-lookups 4000
// checksum find-and-invoke 4000
// checksum find-inherited 4000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.util.Arrays;
import java.util.Locale;

public class L4W77FinderMintBench {
    public static class Box {
        public int v = 3;
    }

    public static class S {
        public static int s = 5;
    }

    public static class P {
        public static int p = 11;
    }

    public static class Q extends P {
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

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodHandles.Lookup other = MethodHandles.publicLookup();
        Box box = new Box();
        int m = 4000;

        single("find-getter", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findGetter(Box.class, "v", int.class) != null) s++;
            return s;
        }, reps, m);
        single("find-setter", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findSetter(Box.class, "v", int.class) != null) s++;
            return s;
        }, reps, m);
        single("find-static-getter", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findStaticGetter(S.class, "s", int.class) != null) s++;
            return s;
        }, reps, m);
        single("find-static-setter", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findStaticSetter(S.class, "s", int.class) != null) s++;
            return s;
        }, reps, m);
        single("find-two-lookups", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                MethodHandles.Lookup l = (i & 1) == 0 ? lk : other;
                if (l.findGetter(Box.class, "v", int.class) != null) s++;
            }
            return s;
        }, reps, m);
        single("find-and-invoke", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                MethodHandle g = lk.findGetter(Box.class, "v", int.class);
                if ((int) g.invokeExact(box) == 3) s++;
            }
            return s;
        }, reps, m);
        single("find-inherited", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findStaticGetter(Q.class, "p", int.class) != null) s++;
            return s;
        }, reps, m);
    }
}
