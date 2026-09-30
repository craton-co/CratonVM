// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;

/**
 * gcd d9/d (2026-09-28): does a call's reference ARGUMENT stay rooted by the
 * CALLER for the whole call, on the call shapes other than the one
 * {@code Gcd1ArgPinProbe} measures?
 *
 * <p>The page
 * {@code docs/known-issues/gc/gcd-d9d-call-argument-copies-still-rooted-on-other-call-shapes-20260928.md}
 * lists the shapes. Each case hands a 16 MB list, which nothing else
 * references (it is built by {@code make}, which parks its
 * {@link WeakReference} in a one-element array the caller passes along), to a
 * callee that drops it and then polls the reference through
 * {@code System.gc()}:
 *
 * <ul>
 *   <li>{@code static-direct}: a warmed caller's {@code invokestatic} -- the
 *       shape {@code Gcd1ArgPinProbe}'s warm case measures, fixed by
 *       {@code CRATONVM_JIT_CALLEE_OWNED_ARGS}; the control;</li>
 *   <li>{@code static-in-loop}: the same call inside a loop of the warmed
 *       caller (a retire-cell site);</li>
 *   <li>{@code interface-ic}: through an interface call the warmed caller made
 *       monomorphic (an inline cache, or a guarded splice);</li>
 *   <li>{@code interp-to-compiled}: a caller that runs once (interpreted,
 *       unless a door compiles it on its first call) into the warmed
 *       callee.</li>
 * </ul>
 *
 * <p>HotSpot ({@code java -XX:+UseSerialGC -Xmx64m -cp tools/bench Gcd1ArgPinShapesProbe})
 * prints, in this order:
 * <pre>
 *   PASS static-direct
 *   PASS static-in-loop
 *   PASS interface-ic
 *   PASS interp-to-compiled
 *   PASS all 4
 * </pre>
 * and exits 0; otherwise the summary is {@code FAIL <n> of 4} and the exit code
 * 1. The holder of a failing case is named by
 * {@code CRATONVM_DBG=oldmark-root-census,root-source}.
 *
 * <p>Commands:
 * <pre>
 *   javac -d tools/bench tools/bench/Gcd1ArgPinShapesProbe.java
 *   P="--java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp tools/bench"
 *   timeout 300 cratonvm $P Gcd1ArgPinShapesProbe
 *   CRATONVM_DBG=oldmark-root-census,root-source \
 *     timeout 600 cratonvm $P Gcd1ArgPinShapesProbe 2>shapes.log
 * </pre>
 */
public final class Gcd1ArgPinShapesProbe {
    static int failures;
    static long sink;

    interface Dropper {
        boolean dropAndCheck(List<long[]> l, Object[] box, boolean check);
    }

    /** The one implementation, so the interface site stays monomorphic. */
    static final class ListDropper implements Dropper {
        @Override
        public boolean dropAndCheck(List<long[]> l, Object[] box, boolean check) {
            sink += l.size();
            l = null;
            final WeakReference<?> ref = (WeakReference<?>) box[0];
            box[0] = null;
            return !check || cleared(ref);
        }
    }

    static final Dropper DROPPER = new ListDropper();

    static boolean cleared(WeakReference<?> ref) {
        for (int i = 0; i < 3 && ref.get() != null; i++) {
            System.gc();
        }
        return ref.get() == null;
    }

    static void verdict(String name, boolean ok) {
        if (!ok) {
            failures++;
        }
        System.out.println((ok ? "PASS " : "FAIL ") + name);
    }

    /** About {@code mb} megabytes of {@code long[128]} blocks; its weak reference goes to {@code box[0]}. */
    static List<long[]> make(int mb, Object[] box) {
        final List<long[]> l = new ArrayList<>();
        for (int i = 0; i < mb * 1024; i++) {
            l.add(new long[128]);
        }
        box[0] = new WeakReference<>(l);
        return l;
    }

    /** Drops its argument, then (when {@code check}) asks whether it went. */
    static boolean dropAndCheck(List<long[]> l, Object[] box, boolean check) {
        sink += l.size();
        l = null;
        final WeakReference<?> ref = (WeakReference<?>) box[0];
        box[0] = null;
        return !check || cleared(ref);
    }

    /** {@code static-direct}: holds nothing but the call's own argument words. */
    static boolean staticCaller(int mb, boolean check) {
        final Object[] box = new Object[1];
        return dropAndCheck(make(mb, box), box, check);
    }

    /** {@code static-in-loop}: the same call, inside a loop. */
    static boolean loopCaller(int mb, boolean check, int reps) {
        boolean ok = true;
        for (int i = 0; i < reps; i++) {
            final Object[] box = new Object[1];
            ok &= dropAndCheck(make(mb, box), box, check);
        }
        return ok;
    }

    /** {@code interface-ic}: through the interface. */
    static boolean interfaceCaller(Dropper d, int mb, boolean check) {
        final Object[] box = new Object[1];
        return d.dropAndCheck(make(mb, box), box, check);
    }

    /** {@code interp-to-compiled}: runs exactly once. */
    static boolean coldCaller(int mb, boolean check) {
        final Object[] box = new Object[1];
        return dropAndCheck(make(mb, box), box, check);
    }

    static boolean run(int shape, int mb) {
        switch (shape) {
            case 0:
                return staticCaller(mb, true);
            case 1:
                return loopCaller(mb, true, 1);
            case 2:
                return interfaceCaller(DROPPER, mb, true);
            default:
                return coldCaller(mb, true);
        }
    }

    static void guarded(String name, int shape) {
        try {
            verdict(name, run(shape, 16));
        } catch (OutOfMemoryError again) {
            verdict(name, false);
        }
    }

    public static void main(String[] args) {
        // Warm every caller but `coldCaller`, and the callees, with empty
        // lists and no collection; only the 16 MB calls below are judged.
        for (int i = 0; i < 20_000; i++) {
            sink += staticCaller(0, false) ? 1 : 0;
            sink += loopCaller(0, false, 1) ? 1 : 0;
            sink += interfaceCaller(DROPPER, 0, false) ? 1 : 0;
        }
        guarded("static-direct", 0);
        guarded("static-in-loop", 1);
        guarded("interface-ic", 2);
        guarded("interp-to-compiled", 3);
        if (failures == 0) {
            System.out.println("PASS all 4");
        } else {
            System.out.println("FAIL " + failures + " of 4");
            System.exit(1);
        }
    }
}
