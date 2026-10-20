// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 71, lane L4: the cost of a field accessor's
// LINKING call (its first call), measured against its own control in the
// same process so that the mint (bindTo / findGetter) and host noise cancel.
//
// Why: `L4W69LinkingCallBench` read `CRATONVM_MH_LINKING_CALL_SERVED` as
// "medians lower, mins both ways" (bind-getter-first-exact 4939 -> 5186,
// -invoke 5195 -> 4938). Each of its ops is a ~4.5 us `bindTo` mint plus one
// call, the switch moves only the call, and the two binaries ran minutes apart
// on a host at load ~120: a 5% swing of the whole op is larger than what the
// switch can move. This bench runs, per rep, the CONTROL loop (the mint
// alone) and then the ROW loop (mint + call) back to back, and reports the
// median over reps of (row - control): the linking call's own cost.
//
// Output, all on STDOUT:
//   "row <name> min <ns> med <ns> ns/op"         every loop, min/median over reps
//   "call <name> med <ns> ns/op"                  median of (row - its control)
//   "checksum <name> <value>"                     deterministic (rep 0)
// Lines containing "ns/" are timings; the others diff against HotSpot.
// Args: [reps] (default 5). Each row-set finishes well under 60 s.
//
// Switch sets (interleave binaries / sets, take the "call" medians):
//   A: (none)
//   B: CRATONVM_MH_LINKING_CALL_SERVED=1
//   C: CRATONVM_MH_LINKING_CALL_SERVED=1 CRATONVM_MH_LINKING_CALL_DIRECT_SERVE=1
//   D: C + CRATONVM_MH_FIELD_LINK_DERIVATION_MEMO=1
//   E: CRATONVM_MH_FINDER_SHARED_MEMBER_WORDS=1        (find-* rows; `find-only`
//      itself should drop: no fresh strings / MethodType per mint)
//   F: E + CRATONVM_MH_FIELD_LINK_DERIVATION_MEMO=1     (find-*-first should
//      now hit the memo; compare with D)
// Leave CRATONVM_MH_BINDTO_PRELINK_FIELD unset (a pre-linked bound copy's
// first call is not a linking call). Run `--compatible --nojit` and
// `--jdk-only --nojit`.
//
// Controls that must NOT move under any set: `getter-warm-exact`,
// `bind-getter-object` (an `asType` site, never linked).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum bind-only 40000
// checksum bind-getter-first-exact 120000
// checksum bind-getter-first-invoke 120000
// checksum bind-setter-only 40000
// checksum bind-setter-first-exact 40000
// checksum bind-setter-first-invoke 40000
// checksum bind-getter-object 120000
// checksum find-only 4000
// checksum find-getter-first-exact 12000
// checksum find-getter-first-invoke 12000
// checksum find-setter-only 4000
// checksum find-setter-first-exact 4000
// checksum find-static-only 4000
// checksum find-static-getter-first 8000
// checksum getter-warm-exact 120000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.util.Arrays;
import java.util.Locale;

public class L4W71LinkingCallBench {
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
        MethodHandle set = lk.findSetter(Box.class, "v", int.class);
        int n = 40000;

        Body bindOnly = k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (get.bindTo(box) != null) s++;
            return s;
        };
        pair("bind-only", bindOnly, "bind-getter-first-exact", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) get.bindTo(box).invokeExact();
            return s;
        }, reps, n);
        pair("bind-only", bindOnly, "bind-getter-first-invoke", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) get.bindTo(box).invoke();
            return s;
        }, reps, n);
        Body setOnly = k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (set.bindTo(box) != null) s++;
            return s;
        };
        pair("bind-setter-only", setOnly, "bind-setter-first-exact", k -> {
            for (int i = 0; i < k; i++) set.bindTo(box).invokeExact(3);
            return k;
        }, reps, n);
        pair("bind-setter-only", setOnly, "bind-setter-first-invoke", k -> {
            for (int i = 0; i < k; i++) set.bindTo(box).invoke(3);
            return k;
        }, reps, n);
        pair("bind-only", bindOnly, "bind-getter-object", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (Integer) (Object) get.bindTo(box).invoke();
            return s;
        }, reps, n);

        int m = 4000;
        Body findOnly = k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findGetter(Box.class, "v", int.class) != null) s++;
            return s;
        };
        pair("find-only", findOnly, "find-getter-first-exact", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) lk.findGetter(Box.class, "v", int.class).invokeExact(box);
            return s;
        }, reps, m);
        pair("find-only", findOnly, "find-getter-first-invoke", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) lk.findGetter(Box.class, "v", int.class).invoke(box);
            return s;
        }, reps, m);
        Body findSetOnly = k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findSetter(Box.class, "v", int.class) != null) s++;
            return s;
        };
        pair("find-setter-only", findSetOnly, "find-setter-first-exact", k -> {
            for (int i = 0; i < k; i++) lk.findSetter(Box.class, "v", int.class).invokeExact(box, 3);
            return k;
        }, reps, m);
        Body findStaticOnly = k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findStaticGetter(Box.class, "S", int.class) != null) s++;
            return s;
        };
        pair("find-static-only", findStaticOnly, "find-static-getter-first", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) lk.findStaticGetter(Box.class, "S", int.class).invokeExact();
            return s;
        }, reps, m);

        MethodHandle warm = lk.findGetter(Box.class, "v", int.class);
        single("getter-warm-exact", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) s += (int) warm.invokeExact(box);
            return s;
        }, reps, n);
    }
}
