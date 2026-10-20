// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 108, lane L6 -- page
// i91-L6-virtual-thread-code-is-never-compiled, "Wave 108". The workload of
// L6W103VirtualThreadLearnedShortBench (non-leaf helpers between parks on 256
// virtual threads; the `osr` row's long loops), but every rep is printed, not
// only the best, together with the number of live ForkJoinPool worker
// (carrier) threads after the rep. Two questions the best-of-3 TIME line of
// the W103 bench cannot answer:
//
//   * is a `helpers virtual` loss a warm-up cost (the first reps lose, the
//     last ones match the switch-off arm: compiles queued by the learned
//     nominations, OSR'd-then-denied first activations) or a steady-state
//     one (every rep loses)?
//   * did pinned parks take the carrier compensation (`Blocker.begin`)? The
//     JDK's scheduler then adds workers above its parallelism, which shows as
//     `carriers` growing above `parallelism` and staying there (idle workers
//     linger for the pool's keep-alive, 60 s).
//
// A/B (one binary, interleaved, medians per rep index of >= 5 runs; both
// modes; arg 0 = reps, default 6; arg 1 = one row):
//   cratonvm L6W108VirtualThreadLearnedRepsBench
//   CRATONVM_VTHREAD_COMPILED_LEARNED=1 cratonvm L6W108VirtualThreadLearnedRepsBench
//   (java L6W108VirtualThreadLearnedRepsBench for HotSpot)
// Add CRATONVM_DBG_VTHREAD_CENSUS=1 once per arm for the learned_* rows.
// Wave 109: stdout TIME lines are `TIME <which>-<kind> median=<us>` /
// `last=<us>` / `max-carriers=<n>` / `warmup-carriers=<n>`; per-rep lines on
// stderr.
//
// HOTSPOT_EXPECTED_BEGIN
// checksum helpers match=true
// checksum osr match=true
// HOTSPOT_EXPECTED_END

import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.LockSupport;

public class L6W108VirtualThreadLearnedRepsBench {
    interface Shape {
        long area(int k);
    }

    static final class Square implements Shape {
        final int side;

        Square(int side) {
            this.side = side;
        }

        @Override
        public long area(int k) {
            return (long) side * side + k;
        }
    }

    static final class Rect implements Shape {
        final int w;
        final int h;

        Rect(int w, int h) {
            this.w = w;
            this.h = h;
        }

        @Override
        public long area(int k) {
            return (long) w * h - k;
        }
    }

    static final class Mixer {
        long state;

        Mixer(long seed) {
            state = seed;
        }

        long next(int v) {
            state = state * 6364136223846793005L + v;
            return state >>> 33;
        }
    }

    static final String[] KEYS = new String[16];

    static {
        for (int i = 0; i < KEYS.length; i++) {
            KEYS[i] = "key-" + i;
        }
    }

    static long work(Mixer m, Shape[] shapes, int i) {
        long s = m.next(i);
        s += shapes[i & 3].area(i);
        s += mix(KEYS[i & 15], i);
        return s;
    }

    static long mix(String k, int i) {
        return (k.hashCode() ^ i) & 0xffff;
    }

    static Shape[] shapes() {
        return new Shape[] {new Square(3), new Rect(2, 5), new Square(7), new Rect(4, 4)};
    }

    static long helpersTask(int seed) {
        Mixer m = new Mixer(seed);
        Shape[] sh = shapes();
        long acc = 0;
        for (int round = 0; round < 8; round++) {
            for (int i = 0; i < 150; i++) {
                acc += work(m, sh, i);
            }
            LockSupport.parkNanos(1_000);
        }
        return acc;
    }

    static long osrTask(int seed) {
        Mixer m = new Mixer(seed);
        Shape[] sh = shapes();
        long acc = 0;
        for (int pass = 0; pass < 2; pass++) {
            for (int i = 0; i < 80_000; i++) {
                acc += work(m, sh, i);
            }
            LockSupport.parkNanos(1_000);
        }
        return acc;
    }

    static long row(String which, boolean virtual) throws InterruptedException {
        int tasks = which.equals("helpers") ? 256 : 4;
        AtomicLong total = new AtomicLong();
        Thread[] ts = new Thread[tasks];
        for (int k = 0; k < tasks; k++) {
            final int seed = k;
            Runnable r = which.equals("helpers")
                    ? () -> total.addAndGet(helpersTask(seed))
                    : () -> total.addAndGet(osrTask(seed));
            ts[k] = virtual ? Thread.ofVirtual().start(r) : Thread.ofPlatform().start(r);
        }
        for (Thread t : ts) {
            t.join();
        }
        return total.get();
    }

    /** Live threads named like the default scheduler's workers. */
    static int carriers() {
        int n = 0;
        for (Thread t : Thread.getAllStackTraces().keySet()) {
            String name = t.getName();
            if (name.startsWith("ForkJoinPool-") && name.contains("-worker-")) {
                n++;
            }
        }
        return n;
    }

    static boolean run(String which, boolean virtual, int reps) throws InterruptedException {
        String kind = virtual ? "virtual" : "platform";
        // Interpreter round i5 wave 109, lane L6: the A/B script keys a TIME
        // line on the text before its first number and reads the number after
        // it, so every stdout TIME line is `TIME <row> <key>=<value>` with a
        // digit-free row name, one line per (row, key): the median and the
        // last of the reps, the most carriers seen. The per-rep lines go to
        // stderr (still `TIME `-prefixed, so the battery filters them).
        String rowName = which + "-" + kind;
        long first = row(which, virtual);
        boolean match = true;
        System.out.println("TIME " + rowName + " warmup-carriers=" + carriers());
        long[] us = new long[reps];
        int maxCarriers = 0;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            long s = row(which, virtual);
            long dt = System.nanoTime() - t0;
            match &= s == first;
            us[r] = dt / 1_000;
            int c = carriers();
            maxCarriers = Math.max(maxCarriers, c);
            System.err.println("TIME " + rowName + " rep=" + r + " us=" + us[r] + " carriers=" + c);
        }
        if (reps > 0) {
            long last = us[reps - 1];
            long[] sorted = us.clone();
            java.util.Arrays.sort(sorted);
            System.out.println("TIME " + rowName + " median=" + sorted[reps / 2]);
            System.out.println("TIME " + rowName + " last=" + last);
            System.out.println("TIME " + rowName + " max-carriers=" + maxCarriers);
        }
        return match && run1Matches(which, first);
    }

    static long[] expected = new long[2];

    static boolean run1Matches(String which, long sum) {
        int i = which.equals("helpers") ? 0 : 1;
        if (expected[i] == 0) {
            expected[i] = sum;
            return true;
        }
        return expected[i] == sum;
    }

    public static void main(String[] args) throws Exception {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 6;
        String only = args.length > 1 ? args[1] : null;
        String par = System.getProperty("jdk.virtualThreadScheduler.parallelism");
        System.out.println("TIME parallelism="
                + (par != null ? par : String.valueOf(Runtime.getRuntime().availableProcessors())));
        for (String which : new String[] {"helpers", "osr"}) {
            if (only != null && !only.equals(which)) {
                continue;
            }
            boolean v = run(which, true, reps);
            boolean p = run(which, false, reps);
            System.out.println("checksum " + which + " match=" + (v && p));
        }
    }
}
