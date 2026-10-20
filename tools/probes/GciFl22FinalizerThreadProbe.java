// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.atomic.AtomicInteger;

/**
 * gci i22/fl: on which thread does {@code finalize()} run?
 *
 * <p>HotSpot 25 runs every finalizer on its "Finalizer" daemon thread, never on the
 * thread that allocated the object or called {@code System.gc()}. CratonVM's default
 * delivery ({@code CRATONVM_FINALIZER_THREAD} unset, {@code WhenLockHeld}) drains them
 * inline on the draining thread unless that thread holds a user-visible lock;
 * {@code CRATONVM_FINALIZER_THREAD=1} hands every finalizer to the VM's daemon. This
 * probe makes the difference Java-visible, so the switch's flip is decided by a
 * HotSpot diff (block {@code finalizer-thread} of {@code vi22-fl.list}).
 *
 * <p>Usage: {@code GciFl22FinalizerThreadProbe <thread|lock|churn> <n>} ({@code n} a
 * multiple of 20). {@code thread}: n garbage finalizables, then {@code System.gc()}
 * until all ran (30 s at most). {@code lock}: the same, allocated and collected while
 * the main thread holds a monitor. {@code churn}: n finalizables in 20 rounds of
 * garbage churn (the cost arm; {@code [fl22-fin] ms=} on stderr).
 *
 * <p>Stdout is deterministic (HotSpot prints {@code finalized-all true},
 * {@code ran-on-caller false}, {@code ran-on-daemon-only true},
 * {@code ran-holding-caller-lock false}); counts, the last finalizing thread's name
 * and the wall time go to stderr. {@code System.runFinalization()} is not called: on
 * HotSpot it runs finalizers on a non-daemon "Secondary finalizer" thread.
 */
public class GciFl22FinalizerThreadProbe {
    static final AtomicInteger FINALIZED = new AtomicInteger();
    static final AtomicInteger ON_CALLER = new AtomicInteger();
    static final AtomicInteger ON_DAEMON = new AtomicInteger();
    static final AtomicInteger HELD_LOCK = new AtomicInteger();
    static final Object LOCK = new Object();
    static volatile Thread caller;
    static volatile String lastThreadName = "none";
    static volatile Object sink;

    static final class F {
        final byte[] pad = new byte[64];

        @SuppressWarnings("removal")
        @Override
        protected void finalize() {
            Thread t = Thread.currentThread();
            if (t == caller) {
                ON_CALLER.incrementAndGet();
            }
            if (t.isDaemon()) {
                ON_DAEMON.incrementAndGet();
            }
            if (Thread.holdsLock(LOCK)) {
                HELD_LOCK.incrementAndGet();
            }
            lastThreadName = t.getName();
            FINALIZED.incrementAndGet();
        }
    }

    static void make(int n) {
        for (int i = 0; i < n; i++) {
            new F();
            sink = new byte[256];
        }
    }

    static boolean drain(int n, long limitMs) throws InterruptedException {
        long end = System.nanoTime() + limitMs * 1_000_000L;
        while (FINALIZED.get() < n && System.nanoTime() < end) {
            System.gc();
            Thread.sleep(10);
        }
        return FINALIZED.get() == n;
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "thread";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 2000;
        caller = Thread.currentThread();
        long t0 = System.nanoTime();
        System.out.println("mode " + mode);
        if (mode.equals("lock")) {
            synchronized (LOCK) {
                make(n);
                for (int i = 0; i < 20; i++) {
                    System.gc();
                    sink = new byte[1 << 16];
                }
            }
        } else if (mode.equals("churn")) {
            int perRound = n / 20;
            for (int r = 0; r < 20; r++) {
                make(perRound);
                for (int i = 0; i < 2000; i++) {
                    sink = new byte[1024];
                }
            }
            n = perRound * 20;
        } else {
            make(n);
        }
        boolean all = drain(n, 30_000L);
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.out.println("finalized-all " + all);
        System.out.println("ran-on-caller " + (ON_CALLER.get() > 0));
        System.out.println("ran-on-daemon-only " + (ON_DAEMON.get() == FINALIZED.get()));
        System.out.println("ran-holding-caller-lock " + (HELD_LOCK.get() > 0));
        System.err.println("[fl22-fin] mode=" + mode + " n=" + n + " finalized=" + FINALIZED.get()
                + " on_caller=" + ON_CALLER.get() + " on_daemon=" + ON_DAEMON.get()
                + " held_lock=" + HELD_LOCK.get() + " last_thread=" + lastThreadName + " ms=" + ms);
        System.out.println("PROBE-OK");
    }
}
