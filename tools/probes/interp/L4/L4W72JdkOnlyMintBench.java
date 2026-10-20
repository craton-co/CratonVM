// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 72, lane L4: what a field accessor costs under
// `--jdk-only`, where `Lookup.findGetter` / `asType` are the JDK's own
// bytecode (`bindTo` runs it once per receiver, then mints from a memo) and
// the handles are real `DirectMethodHandle$Accessor` /
// `BoundMethodHandle$Species_*` objects (proposal
// `interpreter-i72-L4-proposal-jdk-only-field-handle-mint-and-converting-invoke-cost-DONE-20261014`).
//
// Wave 71's `L4W71LinkingCallBench` read, `--jdk-only --nojit`:
// `bind-getter-object` 77483 ns (`call` 76416 over its `bind-only`
// control), against a 3759 ns `bind-getter-first-exact` call. Its only
// difference from the exact row is the call site `()Object` over a `()int`
// handle: `invoke` runs the JDK's `asType(siteType)`, and the VM built the
// site's `MethodType` through `MethodType.fromMethodDescriptorString` (a
// `Class.getClassLoader` call, a descriptor parse and a `Class.forName`
// through the application loader per reference component) on EVERY call.
// `CRATONVM_MH_SITE_TYPE_TABLE=1` keeps that type per (descriptor, built-in
// loader). The `getter-warm-object` / `-long` / `static-warm-object` rows
// isolate it: the handle is warm, so the JDK's `asType` cache hits (it
// compares the site type by identity) and the site type is most of what is
// left.
//
// Output, all on STDOUT:
//   "row <name> min <ns> med <ns> ns/op"         every loop, min/median over reps
//   "call <name> med <ns> ns/op"                  median of (row - its control)
//   "checksum <name> <value>"                     deterministic (rep 0)
// Lines containing "ns/" are timings; the others diff against HotSpot.
// Args: [reps] (default 5). Each row-set finishes well under 60 s.
//
// Switch sets (interleave binaries / sets, take the medians), with
// `--jdk-only --nojit` (the rows the switch moves) and `--compatible --nojit`
// (control: this model's carrier handles never build a site type, so no row
// may move there):
//   A: (none)
//   B: CRATONVM_MH_SITE_TYPE_TABLE=1
// Expected under B, `--jdk-only`: `getter-warm-object`, `getter-warm-long`,
// `static-warm-object` and `call bind-getter-object` drop; `find-only`,
// `find-static-only`, `bind-only` and `getter-warm-exact` do not move
// (controls; `find-*` is the open half of the proposal).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum bind-only 20000
// checksum bind-getter-object 60000
// checksum find-only 4000
// checksum find-static-only 4000
// checksum getter-warm-exact 120000
// checksum getter-warm-object 120000
// checksum getter-warm-long 120000
// checksum static-warm-object 80000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.util.Arrays;
import java.util.Locale;

public class L4W72JdkOnlyMintBench {
    public static class Box {
        public static int S = 2;
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

    static double time(Body b, int n, long[] sum) throws Throwable {
        long t0 = System.nanoTime();
        long s = b.run(n);
        long t1 = System.nanoTime();
        sum[0] = s;
        return (t1 - t0) / (double) n;
    }

    /** A control loop and a row loop, interleaved per rep. */
    static void pair(String control, Body c, String name, Body b, int reps, int n) throws Throwable {
        c.run(Math.max(2, n / 10));
        b.run(Math.max(2, n / 10));
        double[] cs = new double[reps];
        double[] bs = new double[reps];
        double[] ds = new double[reps];
        long[] sum = new long[1];
        long cSum = 0;
        long bSum = 0;
        for (int rep = 0; rep < reps; rep++) {
            cs[rep] = time(c, n, sum);
            if (rep == 0) cSum = sum[0];
            bs[rep] = time(b, n, sum);
            if (rep == 0) bSum = sum[0];
            ds[rep] = bs[rep] - cs[rep];
        }
        report(control, cs, cSum);
        report(name, bs, bSum);
        System.out.printf(Locale.ROOT, "call %s med %.1f ns/op%n", name, median(ds));
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
        report(name, xs, first);
    }

    static final java.util.Set<String> REPORTED = new java.util.HashSet<>();

    static void report(String name, double[] xs, long checksum) {
        System.out.printf(Locale.ROOT, "row %s min %.1f med %.1f ns/op%n", name, min(xs), median(xs));
        if (REPORTED.add(name)) {
            System.out.println("checksum " + name + " " + checksum);
        }
    }

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        MethodHandles.Lookup lk = MethodHandles.lookup();
        Box box = new Box();
        MethodHandle get = lk.findGetter(Box.class, "v", int.class);
        MethodHandle sget = lk.findStaticGetter(Box.class, "S", int.class);

        int nb = 20000;
        Body bindOnly = k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (get.bindTo(box) != null) s++;
            return s;
        };
        pair("bind-only", bindOnly, "bind-getter-object", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (Integer) (Object) get.bindTo(box).invoke();
            return s;
        }, reps, nb);

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

        int n = 40000;
        single("getter-warm-exact", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) get.invokeExact(box);
            return s;
        }, reps, n);
        single("getter-warm-object", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (Integer) (Object) get.invoke(box);
            return s;
        }, reps, n);
        single("getter-warm-long", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (long) get.invoke(box);
            return s;
        }, reps, n);
        single("static-warm-object", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (Integer) (Object) sget.invoke();
            return s;
        }, reps, n);
    }
}
