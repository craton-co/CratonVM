// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 73, lane L4: the `--jdk-only` field finder mint
// and the warm exact call on a REAL field accessor, A/B for two default-off
// switches (extends `L4W72JdkOnlyMintBench`'s `find-*` / `getter-warm-exact`
// rows with the setter, static and unreflect shapes):
//   CRATONVM_MHN_RESOLVE_MEMO=1 -- `MethodHandleNatives.resolve` /
//     `objectFieldOffset` / `staticFieldOffset` / `staticFieldBase` of a field
//     member found before answer from a per-thread memo (the `find-*` and
//     `unreflect-*` rows);
//   CRATONVM_MH_REAL_FIELD_RECORD=1 -- `invokeExact` at the declared type on a
//     real `DirectMethodHandle$Accessor` / `$StaticAccessor` is served from the
//     handle's memoised record (the `*-warm-exact` rows).
//
// Wave 72 medians (`L4W72JdkOnlyMintBench --jdk-only --nojit`): `find-only`
// ~37 000 ns (2 800 under `--compatible`), `find-static-only` ~31 000,
// `getter-warm-exact` ~1 100 (760 under `--compatible`).
//
// Output, all on STDOUT:
//   "row <name> min <ns> med <ns> ns/op"         every loop, min/median over reps
//   "checksum <name> <value>"                     deterministic (rep 0)
// Lines containing "ns/" are timings; the others diff against HotSpot.
// Args: [reps] (default 5). Each row-set finishes well under 60 s.
//
// Switch sets (interleave sets, three runs each, take the medians), with
// `--jdk-only --nojit` (the rows the switches move) and `--compatible --nojit`
// (control: this model's carriers never reach either path, so no row may move):
//   A: (none)
//   B: CRATONVM_MHN_RESOLVE_MEMO=1
//   C: CRATONVM_MH_REAL_FIELD_RECORD=1
//   D: CRATONVM_MHN_RESOLVE_MEMO=1 CRATONVM_MH_REAL_FIELD_RECORD=1
// Expected, `--jdk-only`: B moves `find-*` / `unreflect-getter-only` (by the
// natives' share, unmeasured: a few microseconds is the guess), C moves the
// `*-warm-exact` rows toward the `--compatible` carrier's cost; D both.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum find-only 4000
// checksum find-static-only 4000
// checksum find-setter-only 4000
// checksum unreflect-getter-only 4000
// checksum getter-warm-exact 120000
// checksum long-getter-warm-exact 200000
// checksum static-getter-warm-exact 80000
// checksum setter-warm-exact 40000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Field;
import java.util.Arrays;
import java.util.Locale;

public class L4W73JdkOnlyFieldBench {
    public static class Box {
        public static int S = 2;
        public int v = 3;
        public long w = 5L;
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

    static double time(Body b, int n, long[] sum) throws Throwable {
        long t0 = System.nanoTime();
        long s = b.run(n);
        long t1 = System.nanoTime();
        sum[0] = s;
        return (t1 - t0) / (double) n;
    }

    static void single(String name, Body b, int reps, int n) throws Throwable {
        b.run(Math.max(2, n / 10));
        double[] xs = new double[reps];
        long[] sum = new long[1];
        long first = 0;
        for (int rep = 0; rep < reps; rep++) {
            xs[rep] = time(b, n, sum);
            if (rep == 0) first = sum[0];
        }
        System.out.printf(Locale.ROOT, "row %s min %.1f med %.1f ns/op%n", name, min(xs), median(xs));
        System.out.println("checksum " + name + " " + first);
    }

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        MethodHandles.Lookup lk = MethodHandles.lookup();
        Box box = new Box();
        Field w = Box.class.getDeclaredField("w");

        int m = 4000;
        single("find-only", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findGetter(Box.class, "v", int.class) != null) s++;
            return s;
        }, reps, m);
        single("find-static-only", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findStaticGetter(Box.class, "S", int.class) != null) s++;
            return s;
        }, reps, m);
        single("find-setter-only", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findSetter(Box.class, "v", int.class) != null) s++;
            return s;
        }, reps, m);
        single("unreflect-getter-only", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.unreflectGetter(w) != null) s++;
            return s;
        }, reps, m);

        MethodHandle get = lk.findGetter(Box.class, "v", int.class);
        MethodHandle getLong = lk.findGetter(Box.class, "w", long.class);
        MethodHandle sget = lk.findStaticGetter(Box.class, "S", int.class);
        MethodHandle set = lk.findSetter(Box.class, "v", int.class);
        int n = 40000;
        single("getter-warm-exact", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) get.invokeExact(box);
            return s;
        }, reps, n);
        single("long-getter-warm-exact", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (long) getLong.invokeExact(box);
            return s;
        }, reps, n);
        single("static-getter-warm-exact", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) sget.invokeExact();
            return s;
        }, reps, n);
        single("setter-warm-exact", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                set.invokeExact(box, 1);
                s += box.v;
            }
            return s;
        }, reps, n);
    }
}
