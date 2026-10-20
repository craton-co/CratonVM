// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 77, lane L6 -- bench for
// docs/internal/fixed-bugs/interpreter-i76-L6-proposal-interrupt-status-field-through-the-memo-20261109-DONE-20261110.md:
//
//   CRATONVM_INTERRUPT_FIELD_MEMO=1  every write of the Java Thread.interrupted
//                                    field (the interrupt's set and every
//                                    consumed interrupt's clear) takes its slot
//                                    from the per-VM thread-layout memo, not a
//                                    by-name lookup under the class-manager
//                                    lock; CRATONVM_DBG_INTERRUPT is read once
//                                    per VM.
//
// A/B, one binary, interleaved, >= 7 reps, medians of the per-run values;
// --jdk-only and --compatible:
//   cratonvm L6W77InterruptFieldBench
//   CRATONVM_INTERRUPT_FIELD_MEMO=1 cratonvm L6W77InterruptFieldBench
//
// Rows (stdout, best of REPS):
//   interrupt-self        ns per Thread.currentThread().interrupt() +
//                         Thread.interrupted() pair (the set through the memo
//                         since wave 76; the clear is bytecode, then the
//                         clearInterruptEvent bridge's is_interrupted(true),
//                         which writes the field again: the switch's path);
//   sleep-throw           ns per interrupt() + Thread.sleep(1) that throws
//                         InterruptedException at once (the native's consumed
//                         interrupt clears the field: the path the switch
//                         moves off the lock);
//   wait-throw            ns per interrupt() + Object.wait(1) that throws at
//                         once (the monitor-wait entry check's clear);
//   sleep-throw-loading   sleep-throw while LOADERS threads loop over
//                         reflective lookups (MethodHandles findStatic,
//                         Class.forName), class-manager lock traffic the
//                         by-name clear shares.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum self=100000 sleep=20000 wait=20000 loading=20000 after=false
// done
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L6W77InterruptFieldBench {
    static final int SELF_OPS = 100_000;
    static final int THROW_OPS = 20_000;
    static final int REPS = 5;
    static final int LOADERS = 2;

    static volatile boolean stop;
    static volatile Object sink;

    static int selfRow() {
        Thread me = Thread.currentThread();
        long best = Long.MAX_VALUE;
        int seen = 0;
        for (int r = 0; r < REPS; r++) {
            int s = 0;
            long t0 = System.nanoTime();
            for (int i = 0; i < SELF_OPS; i++) {
                me.interrupt();
                if (Thread.interrupted()) {
                    s++;
                }
            }
            best = Math.min(best, System.nanoTime() - t0);
            seen = s;
        }
        System.out.printf("%-22s %.1f ns/op%n", "interrupt-self", best / (double) SELF_OPS);
        return seen;
    }

    static int sleepThrow(String row) {
        Thread me = Thread.currentThread();
        long best = Long.MAX_VALUE;
        int seen = 0;
        for (int r = 0; r < REPS; r++) {
            int s = 0;
            long t0 = System.nanoTime();
            for (int i = 0; i < THROW_OPS; i++) {
                me.interrupt();
                try {
                    Thread.sleep(1);
                } catch (InterruptedException e) {
                    s++;
                }
            }
            best = Math.min(best, System.nanoTime() - t0);
            seen = s;
        }
        System.out.printf("%-22s %.1f ns/op%n", row, best / (double) THROW_OPS);
        return seen;
    }

    static int waitThrow() {
        Thread me = Thread.currentThread();
        Object lock = new Object();
        long best = Long.MAX_VALUE;
        int seen = 0;
        for (int r = 0; r < REPS; r++) {
            int s = 0;
            long t0 = System.nanoTime();
            for (int i = 0; i < THROW_OPS; i++) {
                me.interrupt();
                synchronized (lock) {
                    try {
                        lock.wait(1);
                    } catch (InterruptedException e) {
                        s++;
                    }
                }
            }
            best = Math.min(best, System.nanoTime() - t0);
            seen = s;
        }
        System.out.printf("%-22s %.1f ns/op%n", "wait-throw", best / (double) THROW_OPS);
        return seen;
    }

    /** Reflective lookups in a loop: class-manager lock traffic. */
    static Thread loader(int n) {
        Thread t = new Thread(() -> {
            MethodHandles.Lookup lookup = MethodHandles.lookup();
            while (!stop) {
                try {
                    sink = lookup.findStatic(L6W77InterruptFieldBench.class, "selfRow",
                            MethodType.methodType(int.class));
                    sink = Class.forName("java.util.concurrent.ConcurrentSkipListMap");
                } catch (ReflectiveOperationException e) {
                    throw new IllegalStateException(e);
                }
            }
        }, "w77-loader-" + n);
        t.setDaemon(true);
        return t;
    }

    public static void main(String[] args) throws Exception {
        int self = selfRow();
        int sleep = sleepThrow("sleep-throw");
        int waited = waitThrow();
        stop = false;
        Thread[] loaders = new Thread[LOADERS];
        for (int i = 0; i < LOADERS; i++) {
            loaders[i] = loader(i);
            loaders[i].start();
        }
        int loading = sleepThrow("sleep-throw-loading");
        stop = true;
        for (Thread t : loaders) {
            t.join();
        }
        boolean after = Thread.currentThread().isInterrupted();
        System.out.println("checksum self=" + self + " sleep=" + sleep + " wait=" + waited
                + " loading=" + loading + " after=" + after);
        System.out.println("done");
    }
}
