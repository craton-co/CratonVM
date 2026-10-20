// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcf f2/j (2026-10-01): the JNI in-native package's repeated entry
 * ({@code CRATONVM_JNI_IN_NATIVE_REPEAT}, on with
 * {@code CRATONVM_JNI_NATIVE_TRANSITIONS=1}) must publish exactly the roots a
 * full deposit would. A compiled loop calls two natives from two call sites
 * with a fresh array each round; every 512th round it blocks in
 * {@code Object.wait(1)} between two native calls (a blocking deposit in
 * between, which must invalidate the reuse); a second thread allocates so
 * pauses land while the loop's thread is in native, and samples the loop
 * thread's stack trace. Objects held only by the interpreter frames below the
 * loop are checked at the end.
 *
 * <p>Deterministic stdout (HotSpot 25 {@code -XX:+UseSerialGC} prints):
 * <pre>
 *   repeat-sum: ok
 *   repeat-holder: ok
 *   repeat-keep: ok
 *   repeat-trace: ok
 *   PASS
 * </pre>
 * Diagnostics on stderr. With {@code CRATONVM_DBG_JNI_PHASE=1} the
 * {@code [GC] jni_phase:} line's {@code dep_repeat} count is the deciding
 * engagement counter (positive under the package, absent with
 * {@code CRATONVM_JNI_IN_NATIVE_REPEAT=0}); with
 * {@code CRATONVM_DBG_JNI_REPEAT_AUDIT=1} no {@code dep_repeat_mismatch} key
 * and no {@code [jni-repeat] MISMATCH} line may appear.
 *
 * <p>Commands (Linux):
 * <pre>
 *   gcc -O1 -shared -fPIC -I"$JDK/include" -I"$JDK/include/linux" \
 *       -o /tmp/libgcff2jjnirepeat.so tools/probes/jni/GcfF2jJniRepeatProbe.c
 *   javac -d /tmp/gcff2jjnirepeat tools/bench/GcfF2jJniRepeatProbe.java
 *   java -XX:+UseSerialGC -Xmx64m -cp /tmp/gcff2jjnirepeat GcfF2jJniRepeatProbe \
 *       /tmp/libgcff2jjnirepeat.so [rounds] [mode] [progress]
 * </pre>
 * gci i17/jh: a fourth argument {@code progress} adds a daemon thread that
 * prints {@code [repeat-probe-progress]} on stderr every 5 s (see
 * {@code main}); stdout is unchanged.
 */
public final class GcfF2jJniRepeatProbe {

    static native int pick(int[] a, int i);

    static native int nameLength(Object o);

    static final Object LOCK = new Object();
    static volatile boolean done;
    static volatile Object sink;
    /**
     * gci i13/jm: arrays the churn thread allocated. Written by the churn
     * thread only, read after {@code churn.join()} (stderr diagnostics).
     */
    static long churned;
    /**
     * gci i15/jk: bytes the churn thread allocated, by its own
     * {@code getCurrentThreadAllocatedBytes()} (-1 if unavailable). Written by
     * the churn thread only, read after {@code churn.join()} (stderr).
     */
    static volatile long churnBytes = -1;
    /**
     * gci i17/jh: where the loop thread is, for the {@code progress} reporter
     * (args[3]): the rep it runs (-1 before the first) and the last multiple
     * of 512 rounds it reached in that rep (written in the existing
     * every-512th-round branch of {@code run}, beside its {@code wait(1)}).
     * Written by the loop thread only.
     */
    static volatile int progRep = -1;
    static volatile int progRound;

    /**
     * gci i15/jk (gcg-g1j): the calling thread's allocated bytes, or -1 when
     * the platform bean cannot say. Only the calling thread is asked (the one
     * form every VM answers).
     */
    static long threadBytes() {
        try {
            java.lang.management.ThreadMXBean b =
                    java.lang.management.ManagementFactory.getThreadMXBean();
            if (b instanceof com.sun.management.ThreadMXBean) {
                return ((com.sun.management.ThreadMXBean) b).getCurrentThreadAllocatedBytes();
            }
        } catch (Throwable t) {
            // fall through
        }
        return -1;
    }

    static final class Holder {
        long v;
        final String tag;

        Holder(String tag) {
            this.tag = tag;
        }
    }

    /** gcf f5/j: bisection switches (args[2]): see `main`. */
    static boolean allocate = true;
    static final int[] SHARED = new int[3];

    /** The compiled loop. Returns the sum of what the natives answered. */
    static long run(int rounds, Holder h) throws InterruptedException {
        long sum = 0;
        for (int i = 0; i < rounds; i++) {
            int[] a = allocate ? new int[3] : SHARED;
            a[0] = i;
            a[1] = i + 1;
            a[2] = i + 2;
            sum += pick(a, i);
            if ((i & 1) == 0) {
                sum += nameLength(h);
            }
            if ((i & 511) == 0) {
                progRound = i;
                synchronized (LOCK) {
                    LOCK.wait(1);
                }
            }
            h.v += a[2];
        }
        return sum;
    }

    static long expected(int rounds, int nameLen) {
        long sum = 0;
        for (int i = 0; i < rounds; i++) {
            sum += i + (i % 3);
            if ((i & 1) == 0) {
                sum += nameLen;
            }
        }
        return sum;
    }

    public static void main(String[] args) throws Exception {
        System.load(args[0]);
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 200_000;
        int reps = 10;
        // gcf f5/j: `[mode]`, to bisect a slowdown -- `full` (default),
        // `nosample` (the churn thread never asks for the loop thread's
        // stack), `nochurn` (no churn thread at all), `noalloc` (the loop
        // reuses one array: no TLAB refill between native calls). Only `full`
        // checks `repeat-trace`; the other modes print it as `ok`.
        // gcg g1/j: `quiet` -- `noalloc` without the stack samples (the churn
        // thread still allocates). `noalloc` P costs 5.6x A on Generational
        // with equal pause counts (vf5); `quiet` against `noalloc` says
        // whether the samples' frame-trace pauses are that cost.
        String mode = args.length > 2 ? args[2] : "full";
        allocate = !mode.equals("noalloc") && !mode.equals("quiet");
        boolean sample = mode.equals("full") || mode.equals("noalloc");
        boolean churnOn = !mode.equals("nochurn");
        // gci i17/jh: `[progress]` (args[3] = `progress`) -- a daemon thread
        // prints, every 5 s on stderr, `[repeat-probe-progress] t_ms=<ms since
        // the start> rep=<progRep> round=<progRound> churned=<arrays>`, so a
        // run that does not finish can be told apart: the loop's (rep, round)
        // still advancing is a slow run; it frozen while `churned` advances is
        // the loop thread stuck alone; no line at all is the whole VM stuck.
        // Off without the argument (no extra thread).
        boolean progress = args.length > 3 && args[3].equals("progress");
        // Held only by this (interpreter) frame across every native call.
        int[] keep = new int[] {42, 43, 44};
        String keepName = new String("keep-" + rounds);
        Holder h = new Holder(new String("holder"));
        int nameLen = Holder.class.getName().length();

        Thread loop = Thread.currentThread();
        boolean[] sawRun = new boolean[1];
        Thread churn = new Thread(() -> {
            int samples = 0;
            long cb0 = threadBytes();
            while (!done) {
                sink = new byte[32 * 1024];
                churned++;
                if (sample && (++samples & 63) == 0) {
                    for (StackTraceElement e : loop.getStackTrace()) {
                        if (e.getMethodName().equals("run")
                                && e.getClassName().equals("GcfF2jJniRepeatProbe")) {
                            sawRun[0] = true;
                        }
                    }
                }
            }
            long cb1 = threadBytes();
            if (cb0 >= 0 && cb1 >= 0) {
                churnBytes = cb1 - cb0;
            }
        }, "churn");
        churn.setDaemon(true);
        // gci i15/jk: the bean is initialised before the timed loop, so the
        // loop thread's own count below holds only what the loop allocated.
        threadBytes();
        if (progress) {
            final long tStart = System.nanoTime();
            Thread reporter = new Thread(() -> {
                while (!done) {
                    try {
                        Thread.sleep(5000);
                    } catch (InterruptedException e) {
                        return;
                    }
                    if (done) {
                        return;
                    }
                    System.err.println("[repeat-probe-progress] t_ms="
                            + ((System.nanoTime() - tStart) / 1_000_000)
                            + " rep=" + progRep + " round=" + progRound
                            + " churned=" + churned);
                }
            }, "progress");
            reporter.setDaemon(true);
            reporter.start();
        }
        if (churnOn) {
            churn.start();
        }
        long lb0 = threadBytes();
        long t0 = System.nanoTime();

        boolean sumOk = true;
        long hv = 0;
        for (int r = 0; r < reps; r++) {
            progRep = r;
            long got = run(rounds, h);
            long want = expected(rounds, nameLen);
            if (got != want) {
                sumOk = false;
                System.err.println("rep " + r + ": got " + got + " want " + want);
            }
            for (int i = 0; i < rounds; i++) {
                hv += i + 2;
            }
        }
        long ns = System.nanoTime() - t0;
        long lb1 = threadBytes();
        System.err.println("[repeat-probe] mode=" + mode + " rounds=" + rounds + " reps=" + reps
                + " ns_per_round=" + (ns / ((long) rounds * reps)));
        done = true;
        if (churnOn) {
            churn.join();
        }
        if (!sample) {
            sawRun[0] = true;
        }

        System.out.println("repeat-sum: " + (sumOk ? "ok" : "MISMATCH"));
        System.out.println("repeat-holder: " + (h.v == hv ? "ok" : "MISMATCH " + h.v + " vs " + hv));
        boolean keepOk = keep[0] == 42 && keep[1] == 43 && keep[2] == 44
                && keepName.equals("keep-" + rounds) && h.tag.equals("holder");
        System.out.println("repeat-keep: " + (keepOk ? "ok" : "MISMATCH"));
        System.out.println("repeat-trace: " + (sawRun[0] ? "ok" : "no sample named run"));
        System.out.println(sumOk && h.v == hv && keepOk && sawRun[0] ? "PASS" : "FAIL");

        // gci i13/jm (gcg-g1j): the churn's throughput and the collections it
        // cost, on stderr only. churn_arrays / gcs is the eden one collection
        // frees (in 32 KiB arrays); churn_arrays / wall_ms the churn's rate.
        long gcs = 0;
        try {
            for (java.lang.management.GarbageCollectorMXBean b
                    : java.lang.management.ManagementFactory.getGarbageCollectorMXBeans()) {
                long c = b.getCollectionCount();
                if (c > 0) {
                    gcs += c;
                }
            }
        } catch (Throwable t) {
            gcs = -1;
        }
        System.err.println("[repeat-probe-churn] mode=" + mode + " churn_arrays=" + churned
                + " gcs=" + gcs + " wall_ms=" + (ns / 1_000_000));
        // gci i15/jk (gcg-g1j): who allocates. loop_bytes is what the loop
        // thread itself allocated over the timed reps (in `quiet` / `noalloc`
        // the loop allocates nothing in Java: any bytes here are the JNI call
        // path's); churn_bytes the churn thread's. -1 when unavailable.
        long loopBytes = (lb0 >= 0 && lb1 >= 0) ? lb1 - lb0 : -1;
        long loopMilliPerRound = loopBytes < 0 ? -1 : loopBytes * 1000 / ((long) rounds * reps);
        System.err.println("[repeat-probe-alloc] mode=" + mode + " loop_bytes=" + loopBytes
                + " loop_milli_bytes_per_round=" + loopMilliPerRound
                + " churn_bytes=" + churnBytes);
    }
}
