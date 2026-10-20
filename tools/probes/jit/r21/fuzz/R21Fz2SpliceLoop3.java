// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: spliced-loop promotion against the memory
// model (`CRATONVM_JIT_IR_LOOP_PROMOTE_SPLICED`; promotion keeps a plain
// field in a register and stores it at the loop exit).
//
// Risk targeted: delaying a plain store is legal ONLY until the next
// release. A volatile store, a monitor exit, `Thread.yield`-free spinning on
// a volatile flag, or a lock in the loop body must publish the plain field
// first. The writer runs a hot spliced loop
// (`w.step(n)`: `sum += i; if (...) vcount = i;` -- a volatile store in the
// body) while a reader thread polls `vcount` (volatile) then `sum` (plain):
// by happens-before, every observation must have `sum >= T(vcount)` (the
// triangular number of the published trip). A second shape takes a
// monitor in the loop (`synchronized (lock) { sum += i; }`) and the reader
// reads `sum` under the same monitor: it must never see a value outside
// the set of prefix sums. The probe prints only violation COUNTS (0) and
// the final sums, which are exact. Then a deopt trigger (a subclass
// overriding the step's helper) and again.
//
// Run:
//   javac -d out R21Fz2SpliceLoop3.java
//   java -Xmx256m -cp out R21Fz2SpliceLoop3                              # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz2SpliceLoop3
//   CRATONVM_JIT_IR_LOOP_PROMOTE_SPLICED=1 cratonvm -Xmx256m -cp out R21Fz2SpliceLoop3
//   CRATONVM_JIT_IR_LOOP_PROMOTE_SPLICED=1 CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_METHOD_ENTRY=1 cratonvm ...
//   ... + CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_POLL_WRITEBACK=1
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

public class R21Fz2SpliceLoop3 {
    static final int TRIPS = 4_000;

    static class Worker {
        long sum;
        volatile int vcount;
        volatile int ltrips;
        final Object lock = new Object();
        long locked;

        int weight(int i) {
            return 1;
        }

        void step(int n) {
            for (int i = 1; i <= n; i++) {
                sum += (long) i * weight(i);
                if ((i & 63) == 0) {
                    vcount = i;
                }
            }
            vcount = n;
        }

        void stepLocked(int n) {
            for (int i = 1; i <= n; i++) {
                synchronized (lock) {
                    locked += i;
                }
                ltrips = i;
            }
        }
    }

    static class Heavy extends Worker {
        @Override
        int weight(int i) {
            return 2;
        }
    }

    static long tri(long n) {
        return n * (n + 1) / 2;
    }

    // Drive the step through a caller so it is spliced there.
    static long drive(Worker w, int n) {
        w.step(n);
        return w.sum;
    }

    static long driveLocked(Worker w, int n) {
        w.stepLocked(n);
        return w.locked;
    }

    static volatile boolean done;

    static String race(Worker w, int factor, int rounds) throws InterruptedException {
        done = false;
        final long[] bad = new long[2];
        final long[] seen = new long[1];
        Thread reader = new Thread(() -> {
            while (!done) {
                int c = w.vcount;
                long s = w.sum;
                if (rounds == 1 && c > 0 && s < factor * tri(c)) {
                    bad[0]++;
                }
                int t = w.ltrips;
                long l;
                synchronized (w.lock) {
                    l = w.locked;
                }
                // Round 1 only: `l` is a prefix sum 1..k with k >= the published trip t.
                if (rounds == 1 && l > 0) {
                    long k = (long) Math.floor((Math.sqrt(8.0 * l + 1) - 1) / 2);
                    if (tri(k) != l || l < tri(t)) {
                        bad[1]++;
                    }
                } else if (rounds == 1 && t > 0) {
                    bad[1]++;
                }
                seen[0]++;
            }
        });
        reader.start();
        long a = 0;
        long b = 0;
        for (int r = 0; r < rounds; r++) {
            a = drive(w, TRIPS);
            b = driveLocked(w, TRIPS);
        }
        done = true;
        reader.join();
        // Whether the reader got scheduled before the writers finished is timing, not a fact.
        return "sum=" + a + " locked=" + b + " vcount=" + w.vcount + " violations=" + bad[0] + "/" + bad[1];
    }

    public static void main(String[] args) throws Exception {
        System.out.println("race cold " + race(new Worker(), 1, 1));
        long h = 0;
        for (int r = 0; r < 20_000; r++) {
            Worker w = new Worker();
            h = h * 31 + drive(w, 64) + driveLocked(w, 16);
        }
        System.out.println("warm hash " + h);
        for (int i = 0; i < 3; i++) {
            System.out.println("race hot " + race(new Worker(), 1, 1));
        }
        System.out.println("race many " + race(new Worker(), 1, 50));
        // Deopt trigger: weight() overridden.
        System.out.println("race late " + race(new Heavy(), 2, 1));
        System.out.println("race after " + race(new Worker(), 1, 1));
    }
}
