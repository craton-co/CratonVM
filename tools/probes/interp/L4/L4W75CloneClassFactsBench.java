// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 75, lane L4: A/B of one default-off switch,
//   CRATONVM_OBJECT_CLONE_CLASS_FACTS=1 -- a plain-object `Object.clone`
//     answers "is the class Cloneable" and "which side-table overlay"
//     (LinkedHashMap / Properties) from a per-thread memo keyed by
//     (VM, collection epoch, ClassId) instead of walking the hierarchy per
//     call (a by-name lookup of Cloneable, interface worklists, one String per
//     superclass up to Object).
//
// Rows:
//   clone-flat        a two-field Cloneable directly under Object
//   clone-deep        six levels deep, Cloneable reached through an
//                     interface chain declared at the top (the longest walks)
//   clone-cnse        a non-Cloneable's super.clone() (the exception path;
//                     the walk is a small part of it, control)
//   lhm-clone         LinkedHashMap.clone() of 4 entries (the overlay arm)
//   find-only         lk.findGetter(Box, "v", int) (--jdk-only clones one
//                     MemberName per finder)
//
// Output, all on STDOUT:
//   "row <name> min <ns> med <ns> ns/op"         every loop, min/median over reps
//   "checksum <name> <value>"                     deterministic (rep 0)
// Lines containing "ns/" are timings; the others diff against HotSpot.
// Args: [reps] (default 5). Each row-set finishes well under 60 s.
//
// Switch sets (interleave A and B, three runs each, medians), with
// `--jdk-only --nojit`, `--compatible --nojit` and `--jdk-only` (JIT on):
//   A: (none)
//   B: CRATONVM_OBJECT_CLONE_CLASS_FACTS=1
// Expected under B: clone-flat / clone-deep by the walks' share (estimated
// 0.3-1 us per clone, more for clone-deep); find-only by one clone's walks;
// clone-cnse and lhm-clone at most slightly.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum clone-flat 120000
// checksum clone-deep 280000
// checksum clone-cnse 4000
// checksum lhm-clone 8000
// checksum find-only 4000
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandles;
import java.util.Arrays;
import java.util.LinkedHashMap;
import java.util.Locale;

public class L4W75CloneClassFactsBench {
    public static class Box {
        public int v = 3;
    }

    static final class Flat implements Cloneable {
        int a = 1;
        int b = 2;

        Flat copy() throws CloneNotSupportedException {
            return (Flat) clone();
        }
    }

    interface Marker extends Cloneable {}
    interface Marker2 extends Marker {}

    static class D0 implements Marker2 { int x0 = 1; }
    static class D1 extends D0 { int x1 = 1; }
    static class D2 extends D1 { int x2 = 1; }
    static class D3 extends D2 { int x3 = 1; }
    static class D4 extends D3 { int x4 = 1; }
    static final class D5 extends D4 {
        int x5 = 2;

        D5 copy() throws CloneNotSupportedException {
            return (D5) clone();
        }
    }

    static final class NotCloneable {
        int a = 1;

        Object copy() throws CloneNotSupportedException {
            return clone();
        }
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
        Flat flat = new Flat();
        D5 deep = new D5();
        NotCloneable nc = new NotCloneable();
        LinkedHashMap<String, Integer> lhm = new LinkedHashMap<>();
        lhm.put("a", 1);
        lhm.put("b", 2);
        lhm.put("c", 3);
        lhm.put("d", 4);

        int n = 40000;
        int m = 4000;
        single("clone-flat", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                Flat c = flat.copy();
                s += c.a + c.b;
            }
            return s;
        }, reps, n);
        single("clone-deep", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                D5 c = deep.copy();
                s += c.x0 + c.x1 + c.x2 + c.x3 + c.x4 + c.x5;
            }
            return s;
        }, reps, n);
        single("clone-cnse", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) {
                try {
                    nc.copy();
                } catch (CloneNotSupportedException e) {
                    if (e.getMessage().endsWith("NotCloneable")) s++;
                }
            }
            return s;
        }, reps, m);
        single("lhm-clone", k -> {
            long s = 0;
            for (int i = 0; i < k / 2; i++) {
                @SuppressWarnings("unchecked")
                LinkedHashMap<String, Integer> c = (LinkedHashMap<String, Integer>) lhm.clone();
                s += c.size();
            }
            return s;
        }, reps, n / 10);
        single("find-only", k -> {
            long s = 0;
            for (int i = 0; i < k; i++) if (lk.findGetter(Box.class, "v", int.class) != null) s++;
            return s;
        }, reps, m);
    }
}
