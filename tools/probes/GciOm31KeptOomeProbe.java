// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;

/**
 * gci i31/om: splits {@code GenR4W6JitOomRootProbe}'s pre-existing
 * {@code FAIL oome-throwable-kept} (every arm of Batch 3, Generational and G1)
 * into the three questions its one verdict line hides:
 * <ul>
 *   <li>does the KEPT {@code OutOfMemoryError} matter? ({@code keep-warm} vs
 *       {@code drop-warm}: the same shape, the error dropped before the
 *       check in the second);</li>
 *   <li>does the warm-up (20 000 synthetic errors, so the catching method and
 *       the fill are compiled before the real heap-full run) matter?
 *       ({@code keep-warm} vs {@code keep-cold});</li>
 *   <li>which half fails: the dropped list's {@code WeakReference} not
 *       clearing ({@code cleared=false}) or the heap not usable afterwards
 *       ({@code usable=false}).</li>
 * </ul>
 * Each case has its own statics and methods, so one case's compiled code or
 * leak cannot pin another's data.
 *
 * <p>stdout (HotSpot 25, {@code -Xmx64m}, any collector):
 * <pre>
 *   case keep-cold caught=true cleared=true usable=true
 *   case drop-warm caught=true cleared=true usable=true
 *   case keep-warm caught=true cleared=true usable=true
 *   PROBE-END
 * </pre>
 * An optional first argument runs ONE case (its line, then {@code PROBE-END}).
 * Diagnostics on stderr only.
 *
 * <p>{@code java -XX:+UseSerialGC -Xmx64m GciOm31KeptOomeProbe}
 */
public final class GciOm31KeptOomeProbe {
    static long sink;

    // keep-cold
    static List<long[]> coldData;
    static OutOfMemoryError coldOome;
    // drop-warm
    static List<long[]> dropData;
    static OutOfMemoryError dropOome;
    // keep-warm
    static List<long[]> warmData;
    static OutOfMemoryError warmOome;

    static boolean cleared(WeakReference<?> ref) {
        for (int i = 0; i < 3 && ref.get() != null; i++) {
            System.gc();
        }
        return ref.get() == null;
    }

    /** 24 MB of fresh, short-lived allocation must succeed. */
    static boolean usable() {
        try {
            long s = 0;
            for (int i = 0; i < 24; i++) {
                final long[] chunk = new long[128 * 1024];
                chunk[i] = i;
                s += chunk[i];
            }
            sink += s;
            return true;
        } catch (OutOfMemoryError again) {
            return false;
        }
    }

    static void report(String name, boolean caught, boolean clearedNow, boolean usableNow) {
        System.out.println("case " + name + " caught=" + caught + " cleared=" + clearedNow
                + " usable=" + usableNow);
    }

    // ------------------------------------------------------------------
    // keep-cold: no warm-up; the error is kept across the check.
    // ------------------------------------------------------------------

    static void fillCold(List<long[]> l) {
        while (true) {
            l.add(new long[128]);
        }
    }

    static boolean catchCold() {
        try {
            fillCold(coldData);
        } catch (OutOfMemoryError e) {
            coldOome = e;
            coldData = null;
        }
        return coldOome != null;
    }

    static void caseKeepCold() {
        coldData = new ArrayList<>();
        final WeakReference<List<long[]>> ref = new WeakReference<>(coldData);
        final boolean caught = catchCold();
        final boolean clearedNow = cleared(ref);
        coldOome = null;
        report("keep-cold", caught, clearedNow, usable());
    }

    // ------------------------------------------------------------------
    // drop-warm: warmed like the original case; the error is DROPPED before
    // the check.
    // ------------------------------------------------------------------

    static void fillDrop(List<long[]> l, int cap) {
        int n = 0;
        while (true) {
            l.add(new long[128]);
            if (cap >= 0 && ++n >= cap) {
                throw new OutOfMemoryError("warm-up");
            }
        }
    }

    static boolean catchDrop(int cap) {
        try {
            fillDrop(dropData, cap);
        } catch (OutOfMemoryError e) {
            dropOome = e;
            dropData = null;
        }
        return dropOome != null;
    }

    static void caseDropWarm() {
        for (int i = 0; i < 20_000; i++) {
            dropData = new ArrayList<>();
            catchDrop(4);
            dropOome = null;
        }
        dropData = new ArrayList<>();
        final WeakReference<List<long[]>> ref = new WeakReference<>(dropData);
        final boolean caught = catchDrop(-1);
        dropOome = null;
        final boolean clearedNow = cleared(ref);
        report("drop-warm", caught, clearedNow, usable());
    }

    // ------------------------------------------------------------------
    // keep-warm: the original `oome-throwable-kept` shape.
    // ------------------------------------------------------------------

    static void fillWarm(List<long[]> l, int cap) {
        int n = 0;
        while (true) {
            l.add(new long[128]);
            if (cap >= 0 && ++n >= cap) {
                throw new OutOfMemoryError("warm-up");
            }
        }
    }

    static boolean catchWarm(int cap) {
        try {
            fillWarm(warmData, cap);
        } catch (OutOfMemoryError e) {
            warmOome = e;
            warmData = null;
        }
        return warmOome != null;
    }

    static void caseKeepWarm() {
        for (int i = 0; i < 20_000; i++) {
            warmData = new ArrayList<>();
            catchWarm(4);
            warmOome = null;
        }
        warmData = new ArrayList<>();
        final WeakReference<List<long[]>> ref = new WeakReference<>(warmData);
        final boolean caught = catchWarm(-1);
        final boolean clearedNow = cleared(ref);
        warmOome = null;
        report("keep-warm", caught, clearedNow, usable());
    }

    // ------------------------------------------------------------------

    static void run(String only, String name, Runnable body) {
        if (only != null && !only.equals(name)) {
            return;
        }
        try {
            body.run();
        } catch (OutOfMemoryError escaped) {
            coldData = null;
            coldOome = null;
            dropData = null;
            dropOome = null;
            warmData = null;
            warmOome = null;
            System.err.println("[gci-om31] " + name + ": OutOfMemoryError escaped the case");
            System.out.println("case " + name + " escaped");
        }
        System.gc();
    }

    public static void main(String[] args) {
        final String only = args.length > 0 ? args[0] : null;
        run(only, "keep-cold", GciOm31KeptOomeProbe::caseKeepCold);
        run(only, "drop-warm", GciOm31KeptOomeProbe::caseDropWarm);
        run(only, "keep-warm", GciOm31KeptOomeProbe::caseKeepWarm);
        System.out.println("PROBE-END");
    }
}
