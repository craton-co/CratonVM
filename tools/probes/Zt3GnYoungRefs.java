// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zt t3/gn (page zgc-zt3gn-young-cycles-process-references-and-finalizers-
// 20261019): does a YOUNG (minor) collection clear a WeakReference whose
// referent is young and unreachable?
//
// The page's reading of HotSpot 25's generational ZGC is that it does NOT:
// reference discovery belongs to the old generation's marking, and a young
// marking follows a Reference's referent like any other field, so the
// referent survives minors until a major. CratonVM's generational young cycle
// runs the reference processor and clears it at the first minor. This probe
// decides the reading against the HotSpot twin; it changes nothing.
//
// A window of `mb` MiB of short-lived garbage is allocated after the referent
// is dropped. The verdict is printed only when the window saw at least one
// minor and no major collection (by the GarbageCollectorMXBeans, whose ZGC
// names are `ZGC Minor Cycles` / `ZGC Major Cycles`); otherwise `n/a`.
//
// stdout:
//   young_window kept=<true|false|n/a>
//   after_system_gc cleared=true
// Args: [mb] (default 256). Run at -Xmx1g so the window is minors only.
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.ref.WeakReference;

public class Zt3GnYoungRefs {
    static volatile Object sink;

    static long cycles(String kind) {
        long n = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (b.getName().contains(kind) && b.getName().contains("Cycles")) {
                n += b.getCollectionCount();
            }
        }
        return n;
    }

    static WeakReference<int[]> make() {
        return new WeakReference<>(new int[16]);
    }

    public static void main(String[] args) {
        int mb = args.length > 0 ? Integer.parseInt(args[0]) : 256;
        WeakReference<int[]> w = make();
        long minor0 = cycles("Minor");
        long major0 = cycles("Major");
        for (int i = 0; i < mb * 1024; i++) {
            sink = new byte[1024];
        }
        sink = null;
        long minors = cycles("Minor") - minor0;
        long majors = cycles("Major") - major0;
        String kept = (minors > 0 && majors == 0) ? String.valueOf(w.get() != null) : "n/a";
        System.out.println("young_window kept=" + kept);
        System.gc();
        System.out.println("after_system_gc cleared=" + (w.get() == null));
    }
}
