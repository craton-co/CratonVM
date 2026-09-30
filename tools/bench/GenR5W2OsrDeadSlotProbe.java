// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;

/**
 * gen r5w2/oomjit6 (2026-09-26): does a DEAD value in a LIVE optimizing-tier
 * OSR frame keep a dropped data structure reachable?
 *
 * <p>Each case is a method entered ONCE whose warm-up loop gets it OSR-compiled
 * (since JIT round 11 wave 11 an OSR body with calls goes to the optimizing
 * tier, {@code CRATONVM_JIT_OSR_OPTIMIZING_EXC_EXITS}). After the loop, still
 * inside the OSR'd frame, the case builds 16 MB, keeps only a
 * {@link WeakReference}, drops the data and asks {@code System.gc()} whether
 * it went. The data is never in a live local at the check and no exception is
 * thrown, so the only thing that can hold it is a word of the OSR'd frame
 * itself: a value the IR tier gave a frame home (a pinned one, because a deopt
 * snapshot names it while it is on the operand stack), or the argument
 * staging region of the last dispatched call. HotSpot's oop map at the check
 * names none of them.
 *
 * <ul>
 *   <li>{@code osr-tail-static}: the data only ever passes through the operand
 *       stack ({@code putstatic}/{@code getstatic}) and a constructor
 *       argument.</li>
 *   <li>{@code osr-tail-arg}: as above, plus it is the argument of a returned
 *       call.</li>
 *   <li>{@code osr-tail-local}: it is held in a local, which the program then
 *       sets to {@code null}.</li>
 * </ul>
 *
 * <p>HotSpot ({@code java -XX:+UseSerialGC -Xmx64m -cp tools/bench GenR5W2OsrDeadSlotProbe})
 * prints, in this order:
 * <pre>
 *   PASS osr-tail-static
 *   PASS osr-tail-arg
 *   PASS osr-tail-local
 *   PASS all 3
 * </pre>
 * and exits 0; otherwise the summary is {@code FAIL <n> of 3} and the exit code
 * 1.
 *
 * <p>Commands:
 * <pre>
 *   javac -d tools/bench tools/bench/GenR5W2OsrDeadSlotProbe.java
 *   timeout 300 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx64m -cp tools/bench GenR5W2OsrDeadSlotProbe
 *   # the lever (opt-in): dead reference homes and stale argument staging zeroed at IR call sites
 *   CRATONVM_JIT_IR_CLEAR_DEAD_REF_SLOTS=1 timeout 300 cratonvm ... GenR5W2OsrDeadSlotProbe
 *   # the controls: single-pass OSR bodies, and no JIT
 *   CRATONVM_JIT_OSR_OPTIMIZING=0 timeout 300 cratonvm ... GenR5W2OsrDeadSlotProbe
 *   timeout 300 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx64m --nojit -cp tools/bench GenR5W2OsrDeadSlotProbe
 *   # which frame word holds it, when a case fails
 *   CRATONVM_DBG=oldmark-root-census,root-source timeout 600 cratonvm ... GenR5W2OsrDeadSlotProbe 2>census.log
 * </pre>
 */
public final class GenR5W2OsrDeadSlotProbe {
    static int failures;
    static long sink;

    // One static per case, so a leak in one case cannot pin another's data.
    static List<long[]> staticData;
    static List<long[]> argData;

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

    /** About {@code mb} megabytes of {@code long[128]} blocks in a fresh list. */
    static List<long[]> blocks(int mb) {
        final List<long[]> l = new ArrayList<>();
        for (int i = 0; i < mb * 1024; i++) {
            l.add(new long[128]);
        }
        return l;
    }

    static int consume(List<long[]> l) {
        return l.size();
    }

    /** Entered once; the warm-up loop is what compiles it (OSR). */
    static void osrTailStatic() {
        for (int i = 0; i < 30_000; i++) {
            staticData = new ArrayList<>();
            sink += consume(staticData);
        }
        staticData = blocks(16);
        final WeakReference<List<long[]>> ref = new WeakReference<>(staticData);
        staticData = null;
        verdict("osr-tail-static", cleared(ref));
    }

    static void osrTailArg() {
        for (int i = 0; i < 30_000; i++) {
            argData = new ArrayList<>();
            sink += consume(argData);
        }
        argData = blocks(16);
        final WeakReference<List<long[]>> ref = new WeakReference<>(argData);
        sink += consume(argData);
        argData = null;
        verdict("osr-tail-arg", cleared(ref));
    }

    static void osrTailLocal() {
        List<long[]> l = new ArrayList<>();
        for (int i = 0; i < 30_000; i++) {
            l = new ArrayList<>();
            sink += consume(l);
        }
        l = blocks(16);
        final WeakReference<List<long[]>> ref = new WeakReference<>(l);
        sink += consume(l);
        l = null;
        verdict("osr-tail-local", cleared(ref));
    }

    interface Case {
        void run();
    }

    static void guarded(String name, Case c) {
        try {
            c.run();
        } catch (OutOfMemoryError again) {
            staticData = null;
            argData = null;
            verdict(name, false);
        }
    }

    public static void main(String[] args) {
        guarded("osr-tail-static", GenR5W2OsrDeadSlotProbe::osrTailStatic);
        guarded("osr-tail-arg", GenR5W2OsrDeadSlotProbe::osrTailArg);
        guarded("osr-tail-local", GenR5W2OsrDeadSlotProbe::osrTailLocal);
        if (failures == 0) {
            System.out.println("PASS all 3");
        } else {
            System.out.println("FAIL " + failures + " of 3");
            System.exit(1);
        }
    }
}
