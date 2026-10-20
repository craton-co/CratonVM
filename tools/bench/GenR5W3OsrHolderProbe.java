// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;

/**
 * gen r5w3/live7 (2026-09-26): WHERE is the word that keeps
 * {@code GenR5W2OsrDeadSlotProbe}'s dropped list alive?
 *
 * <p>That probe fails in every arm the orchestrator ran -- default,
 * {@code CRATONVM_JIT_IR_CLEAR_DEAD_REF_SLOTS=1} and
 * {@code CRATONVM_JIT_OSR_OPTIMIZING=0} -- although the clear leaves the
 * optimizing OSR frame with no copy of the list, and
 * {@code GenR4W6JitOomRootProbe}'s {@code catch-staged-arg}, which drops its
 * list from an OSR frame of the same shape, PASSES under the same flag. The
 * two shapes differ in two ways, and each case below removes one of them:
 *
 * <ul>
 *   <li><b>who builds the list</b>: in the failing probe {@code blocks(..)} is
 *       entered for the first time from the OSR frame, so it runs in the
 *       INTERPRETER and OSR-compiles its own loop, directly beneath the frame
 *       that later runs the check. A {@code warm} builder was called 30 000
 *       times in the warm-up loop, so it is an ordinary compiled callee.</li>
 *   <li><b>who calls the check</b>: in the failing probe the OSR frame calls
 *       the never-compiled {@code cleared(..)} directly, through the JIT's
 *       dispatch helper into the interpreter. A {@code warm} check is a
 *       compiled method (warmed with {@code null}) that calls it instead --
 *       the {@code catch-staged-arg} arrangement.</li>
 * </ul>
 *
 * <p>Each case is a method entered ONCE whose warm-up loop gets it
 * OSR-compiled; the tail builds 8 MB, keeps only a {@link WeakReference},
 * drops the data and checks. 8 MB (not 16) so that four failing cases still
 * fit {@code -Xmx64m} and a failure is a retention, never an OOME cascade.
 *
 * <p>HotSpot ({@code java -XX:+UseSerialGC -Xmx64m -cp tools/bench GenR5W3OsrHolderProbe})
 * prints, in this order:
 * <pre>
 *   PASS warm-build-warm-check
 *   PASS warm-build-cold-check
 *   PASS cold-build-warm-check
 *   PASS cold-build-cold-check
 *   PASS all 4
 * </pre>
 * and exits 0; otherwise the summary is {@code FAIL <n> of 4} and the exit code
 * 1. {@code cold-build-cold-check} is {@code GenR5W2OsrDeadSlotProbe}'s
 * {@code osr-tail-static} at half the size.
 *
 * <p>Reading the four verdicts on a failing binary (run each arm below):
 * <ul>
 *   <li>only the {@code cold-build-*} cases fail: the holder is left behind by
 *       the interpreted, OSR-compiled builder that ran beneath the OSR frame
 *       (a stale word of a returned frame at that depth, or state its OSR
 *       entry/exit left on the thread);</li>
 *   <li>only the {@code *-cold-check} cases fail: the holder is on the
 *       dispatch-into-the-interpreter path the check itself takes;</li>
 *   <li>all four fail in the {@code CRATONVM_JIT_IR_CLEAR_DEAD_REF_SLOTS=1}
 *       arm: the holder is outside every compiled frame, and the census
 *       ({@code CRATONVM_DBG=oldmark-root-census,root-source}) names its
 *       root section.</li>
 * </ul>
 *
 * <p>Commands:
 * <pre>
 *   javac -d tools/bench tools/bench/GenR5W3OsrHolderProbe.java
 *   P="--java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp tools/bench"
 *   for env in "" CRATONVM_JIT_IR_CLEAR_DEAD_REF_SLOTS=1 CRATONVM_JIT_PRECISE_FRAME_LIVENESS=1 \
 *              CRATONVM_JIT_OSR_OPTIMIZING=0 CRATONVM_NO_CONSERVATIVE_LOCALS=1; do
 *     echo "== $env"; env $env timeout 300 cratonvm $P GenR5W3OsrHolderProbe
 *   done
 *   timeout 300 cratonvm $P --nojit GenR5W3OsrHolderProbe
 * </pre>
 */
public final class GenR5W3OsrHolderProbe {
    static int failures;
    static long sink;

    // One static per case, so a leak in one case cannot pin another's data.
    static List<long[]> wwData;
    static List<long[]> wcData;
    static List<long[]> cwData;
    static List<long[]> ccData;

    static boolean cleared(WeakReference<?> ref) {
        for (int i = 0; i < 3 && ref.get() != null; i++) {
            System.gc();
        }
        return ref.get() == null;
    }

    /** The compiled check: warmed with {@code null}, so it is compiled. */
    static boolean check(WeakReference<?> ref) {
        return ref == null || cleared(ref);
    }

    static void verdict(String name, boolean ok) {
        if (!ok) {
            failures++;
        }
        System.out.println((ok ? "PASS " : "FAIL ") + name);
    }

    /** 8 MB of {@code long[128]} blocks. */
    static final int BLOCKS = 8 * 1024;

    /**
     * The warm builder: {@code n} blocks in a fresh list. Called 30 000 times
     * with {@code n = 2} in each warm-up, so its loop body is profiled and the
     * body is an ordinary compiled callee by the tail.
     */
    static List<long[]> build(int n) {
        final List<long[]> l = new ArrayList<>();
        for (int i = 0; i < n; i++) {
            l.add(new long[128]);
        }
        return l;
    }

    /**
     * The cold builder: the same body, but entered for the first time from a
     * case's tail, so it runs interpreted and OSR-compiles its own loop.
     */
    static List<long[]> coldBuild(int n) {
        final List<long[]> l = new ArrayList<>();
        for (int i = 0; i < n; i++) {
            l.add(new long[128]);
        }
        return l;
    }

    // Each cold builder is entered by ONE case, so the two cold cases need two
    // of them to keep "entered for the first time" true for both.
    static List<long[]> coldBuild2(int n) {
        final List<long[]> l = new ArrayList<>();
        for (int i = 0; i < n; i++) {
            l.add(new long[128]);
        }
        return l;
    }

    static void warmBuildWarmCheck() {
        for (int i = 0; i < 30_000; i++) {
            wwData = build(2);
            sink += wwData.size();
            if (!check(null)) {
                sink++;
            }
        }
        wwData = build(BLOCKS);
        final WeakReference<List<long[]>> ref = new WeakReference<>(wwData);
        wwData = null;
        verdict("warm-build-warm-check", check(ref));
    }

    static void warmBuildColdCheck() {
        for (int i = 0; i < 30_000; i++) {
            wcData = build(2);
            sink += wcData.size();
        }
        wcData = build(BLOCKS);
        final WeakReference<List<long[]>> ref = new WeakReference<>(wcData);
        wcData = null;
        verdict("warm-build-cold-check", cleared(ref));
    }

    static void coldBuildWarmCheck() {
        for (int i = 0; i < 30_000; i++) {
            cwData = new ArrayList<>();
            sink += cwData.size();
            if (!check(null)) {
                sink++;
            }
        }
        cwData = coldBuild(BLOCKS);
        final WeakReference<List<long[]>> ref = new WeakReference<>(cwData);
        cwData = null;
        verdict("cold-build-warm-check", check(ref));
    }

    static void coldBuildColdCheck() {
        for (int i = 0; i < 30_000; i++) {
            ccData = new ArrayList<>();
            sink += ccData.size();
        }
        ccData = coldBuild2(BLOCKS);
        final WeakReference<List<long[]>> ref = new WeakReference<>(ccData);
        ccData = null;
        verdict("cold-build-cold-check", cleared(ref));
    }

    interface Case {
        void run();
    }

    static void guarded(String name, Case c) {
        try {
            c.run();
        } catch (OutOfMemoryError again) {
            wwData = null;
            wcData = null;
            cwData = null;
            ccData = null;
            verdict(name, false);
        }
    }

    public static void main(String[] args) {
        guarded("warm-build-warm-check", GenR5W3OsrHolderProbe::warmBuildWarmCheck);
        guarded("warm-build-cold-check", GenR5W3OsrHolderProbe::warmBuildColdCheck);
        guarded("cold-build-warm-check", GenR5W3OsrHolderProbe::coldBuildWarmCheck);
        guarded("cold-build-cold-check", GenR5W3OsrHolderProbe::coldBuildColdCheck);
        if (failures == 0) {
            System.out.println("PASS all 4");
        } else {
            System.out.println("FAIL " + failures + " of 4");
            System.exit(1);
        }
    }
}
