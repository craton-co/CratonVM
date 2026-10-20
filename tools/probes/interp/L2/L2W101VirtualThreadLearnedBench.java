// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 101, lane L2 -- the bench of
// CRATONVM_VTHREAD_COMPILED_LEARNED (page
// i91-L6-virtual-thread-code-is-never-compiled): the "ordinary" virtual-thread
// workload, a loop calling NON-leaf helpers (virtual, interface and static
// calls, a String hash), then a park, run on virtual threads and then on
// platform threads. With the switch off only leaf bodies are compiled on a
// virtual thread, so the virtual rows run at interpreter speed; with it on
// the helpers are entered compiled and the task bodies (which park) are
// learned off after one pin each.
//
// Rows (arg 1 picks one, default all):
//   helpers  2000 tasks x 20 rounds of 200 helper calls, parkNanos between rounds
//   osr      8 tasks, each one 300k-iteration loop of helper calls (OSR), then a
//            park, twice (sized so the --nojit arm finishes in the harness timeout)
//
// Timings on lines starting "TIME " (best of `reps`, arg 0, default 5, after
// one warm-up rep each); stdout otherwise only the checksum lines.
//
// A/B (one binary, interleaved, medians; both modes):
//   cratonvm L2W101VirtualThreadLearnedBench 5
//   CRATONVM_VTHREAD_COMPILED_LEARNED=1 cratonvm L2W101VirtualThreadLearnedBench 5
//   (java L2W101VirtualThreadLearnedBench 5 for HotSpot)
// Census: CRATONVM_DBG_VTHREAD_CENSUS=1 -- learned_pins / learned_denied_methods
// must plateau (a handful: the task lambdas and the JDK frames above a park);
// CRATONVM_DBG_JITC=1 shows tiered-enqueue lines for `work` from the virtual rows.
// Expected: off, TIME virtual >> TIME platform; on, they converge.
//
// HOTSPOT_EXPECTED_BEGIN
// checksum helpers match=true
// checksum osr match=true
// HOTSPOT_EXPECTED_END

import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.LockSupport;

public class L2W101VirtualThreadLearnedBench {
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
        for (int round = 0; round < 20; round++) {
            for (int i = 0; i < 200; i++) {
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
            for (int i = 0; i < 300_000; i++) {
                acc += work(m, sh, i);
            }
            LockSupport.parkNanos(1_000);
        }
        return acc;
    }

    static long row(String which, boolean virtual) throws InterruptedException {
        int tasks = which.equals("helpers") ? 2000 : 8;
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
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            long s = row(which, virtual);
            best = Math.min(best, (System.nanoTime() - t0) / 1_000_000);
            if (s != sumOut[0]) {
                sumOut[0] = Long.MIN_VALUE;
            }
        }
        return best;
    }

    public static void main(String[] args) throws Exception {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        String only = args.length > 1 ? args[1] : null;
        for (String which : new String[] {"helpers", "osr"}) {
            if (only != null && !only.equals(which)) {
                continue;
            }
            long[] v = new long[1];
            long[] p = new long[1];
            long vms = best(which, true, reps, v);
            long pms = best(which, false, reps, p);
            System.out.println("TIME " + which + " virtual ms=" + vms);
            System.out.println("TIME " + which + " platform ms=" + pms);
            System.out.println("checksum " + which + " match=" + (v[0] == p[0] && v[0] != Long.MIN_VALUE));
        }
    }
}
