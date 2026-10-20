// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 76, lane L6 -- bench for
// docs/internal/fixed-bugs/interpreter-i75-L6-proposal-one-lookup-thread-interrupt-20261108-DONE-20261109.md:
//
//   CRATONVM_INTERRUPT_ONE_LOOKUP=1  Thread.interrupt resolves its target from
//                                    Thread.eetop and raises the flag, takes
//                                    the park state and the JMX waiting monitor
//                                    in one registry read.
//
// A/B, one binary, interleaved, medians of the per-run values; --jdk-only and
// --compatible:
//   cratonvm L6W76InterruptBench
//   CRATONVM_INTERRUPT_ONE_LOOKUP=1 cratonvm L6W76InterruptBench
// (and, for the wake shape, CRATONVM_PARK_WAKE_AFTER_UNLOCK=1 with and without).
//
// Rows (stdout, best of REPS):
//   interrupt-running      ns per peer.interrupt() of a running peer that
//                          never parks (the flag is already up after the
//                          first: every call is the whole native);
//   interrupt-self         ns per Thread.currentThread().interrupt();
//                          Thread.interrupted() pair;
//   interrupt-under-unpark ns per peer.interrupt() while UNPARKERS threads
//                          unpark another parked thread in a loop (the
//                          registry locks the interrupt shares with unpark).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum running-seen=true self-seen=200000 under-unpark-seen=true
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.locks.LockSupport;

public class L6W76InterruptBench {
    static final int OPS = 200_000;
    static final int REPS = 5;
    static final int UNPARKERS = 4;

    static volatile boolean stop;
    static volatile long sink;

    static Thread spinner(String name) {
        Thread t = new Thread(() -> {
            long n = 0;
            while (!stop) {
                n++;
                Thread.onSpinWait();
            }
            sink = n;
        }, name);
        t.setDaemon(true);
        return t;
    }

    static boolean runningRow() throws InterruptedException {
        stop = false;
        Thread peer = spinner("w76-int-running");
        peer.start();
        long best = Long.MAX_VALUE;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < OPS; i++) {
                peer.interrupt();
            }
            best = Math.min(best, System.nanoTime() - t0);
        }
        boolean seen = peer.isInterrupted();
        stop = true;
        peer.join();
        System.out.printf("%-24s %.1f ns/op%n", "interrupt-running", best / (double) OPS);
        return seen;
    }

    static int selfRow() {
        Thread me = Thread.currentThread();
        long best = Long.MAX_VALUE;
        int seen = 0;
        for (int r = 0; r < REPS; r++) {
            int s = 0;
            long t0 = System.nanoTime();
            for (int i = 0; i < OPS; i++) {
                me.interrupt();
                if (Thread.interrupted()) {
                    s++;
                }
            }
            best = Math.min(best, System.nanoTime() - t0);
            seen = s;
        }
        System.out.printf("%-24s %.1f ns/op%n", "interrupt-self", best / (double) OPS);
        return seen;
    }

    static boolean underUnparkRow() throws InterruptedException {
        stop = false;
        Thread parked = new Thread(() -> {
            while (!stop) {
                LockSupport.park();
            }
        }, "w76-int-parked");
        parked.setDaemon(true);
        parked.start();
        Thread[] unparkers = new Thread[UNPARKERS];
        for (int u = 0; u < UNPARKERS; u++) {
            unparkers[u] = new Thread(() -> {
                while (!stop) {
                    LockSupport.unpark(parked);
                }
            }, "w76-int-unparker-" + u);
            unparkers[u].setDaemon(true);
            unparkers[u].start();
        }
        Thread peer = spinner("w76-int-target");
        peer.start();
        long best = Long.MAX_VALUE;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < OPS; i++) {
                peer.interrupt();
            }
            best = Math.min(best, System.nanoTime() - t0);
        }
        boolean seen = peer.isInterrupted();
        stop = true;
        LockSupport.unpark(parked);
        for (Thread t : unparkers) {
            t.join();
        }
        parked.join();
        peer.join();
        System.out.printf("%-24s %.1f ns/op%n", "interrupt-under-unpark", best / (double) OPS);
        return seen;
    }

    public static void main(String[] args) throws Exception {
        boolean running = runningRow();
        int self = selfRow();
        boolean under = underUnparkRow();
        System.out.println("checksum running-seen=" + running + " self-seen=" + self
                + " under-unpark-seen=" + under);
        System.out.println("done");
    }
}
