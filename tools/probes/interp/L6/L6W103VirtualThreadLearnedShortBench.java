// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 103, lane L6 -- a SHORT bench of
// CRATONVM_VTHREAD_COMPILED_LEARNED (page
// i91-L6-virtual-thread-code-is-never-compiled), so the switch can be A/B
// measured: `L2W101VirtualThreadLearnedBench` runs over 4 minutes per run
// here (8M helper calls per rep in its `helpers` row, 2000 platform threads
// per rep, 12 reps per row). This is the same workload shape -- a loop of
// NON-leaf helpers (virtual, interface and static calls, a String hash), then
// a park -- scaled to well under 20 s per run, the --nojit arm included:
//
//   helpers  256 tasks x 8 rounds of 150 helper calls, parkNanos between rounds
//   osr      4 tasks, each two 80k-iteration loops of helper calls (OSR), a
//            park after each
//
// Each row runs on virtual threads, then on platform threads: one warm-up
// rep, then `reps` timed reps (arg 0, default 3), stopping early once a row's
// timed reps passed 4 s (the TIME line is the best rep either way). Arg 1
// picks one row. Timings on lines starting "TIME "; the checksum lines are
// the deterministic output.
//
// A/B (one binary, interleaved, medians of >= 5 runs; both modes):
//   cratonvm L6W103VirtualThreadLearnedShortBench
//   CRATONVM_VTHREAD_COMPILED_LEARNED=1 cratonvm L6W103VirtualThreadLearnedShortBench
//   (java L6W103VirtualThreadLearnedShortBench for HotSpot)
// Census: add CRATONVM_DBG_VTHREAD_CENSUS=1 -- learned_pins and
// learned_denied_methods must stay a handful.
// Expected: off, TIME virtual >> TIME platform; on, they converge. The rule
// for flipping: the `virtual` rows win and no `platform` row loses.
//
// HOTSPOT_EXPECTED_BEGIN
// checksum helpers match=true
// checksum osr match=true
// HOTSPOT_EXPECTED_END

import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.LockSupport;

public class L6W103VirtualThreadLearnedShortBench {
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

    static long best(String which, boolean virtual, int reps, long[] sumOut)
            throws InterruptedException {
        sumOut[0] = row(which, virtual);
        long best = Long.MAX_VALUE;
        long spent = 0;
        for (int r = 0; r < reps && (r == 0 || spent < 4_000_000_000L); r++) {
            long t0 = System.nanoTime();
            long s = row(which, virtual);
            long dt = System.nanoTime() - t0;
            spent += dt;
            best = Math.min(best, dt / 1_000);
            if (s != sumOut[0]) {
                sumOut[0] = Long.MIN_VALUE;
            }
        }
        return best;
    }

    public static void main(String[] args) throws Exception {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 3;
        String only = args.length > 1 ? args[1] : null;
        for (String which : new String[] {"helpers", "osr"}) {
            if (only != null && !only.equals(which)) {
                continue;
            }
            long[] v = new long[1];
            long[] p = new long[1];
            long vus = best(which, true, reps, v);
            long pus = best(which, false, reps, p);
            System.out.println("TIME " + which + " virtual us=" + vus);
            System.out.println("TIME " + which + " platform us=" + pus);
            System.out.println("checksum " + which + " match=" + (v[0] == p[0] && v[0] != Long.MIN_VALUE));
        }
    }
}
