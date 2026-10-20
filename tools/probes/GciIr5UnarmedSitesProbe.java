// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;

/**
 * gci i5/ir (2026-10-12): a dead reference held by an optimizing-tier frame
 * whose thread stops for a collection at a GC point that is NOT a call
 * ({@code gci-i3ir-unarmed-optimizing-gc-points-keep-dead-homes-20261012}).
 *
 * <p>The successor of {@code GciIr4UnarmedGcPointProbe}, whose
 * {@code monitor-site} control never failed (vi4-ir E1). Two changes:
 * <ul>
 *   <li>Every judged call uses a size the warm-up also used (one call in 512
 *       allocates {@link #BIG}), so no judged call meets an allocation shape
 *       the compiled code never saw. In the i4 probe the only large
 *       allocations were the judged ones; the first of them ran
 *       {@code make} on a shape it had not been compiled for, {@code make} was
 *       compiled again, and the monitor case that followed no longer ran
 *       the compiled {@code viaMonitor} it was written for.</li>
 *   <li>A case that needs no second thread: {@code multi-site}, whose GC
 *       points after the local died are {@code multianewarray}s in the same
 *       thread.</li>
 * </ul>
 *
 * <p>Cases, in judged order, all warmed into the optimizing tier first:
 * <ul>
 *   <li>{@code multi-site} (item 1, {@code CRATONVM_JIT_IR_OTHER_GC_POINT_CLEARS}):
 *       the method makes {@code big}, reads it, nulls its local, then
 *       allocates {@code byte[4][1024]} 32768 times (about 138 MB, several
 *       young collections at -Xmx64m), each a {@code multianewarray}, and
 *       asks whether {@code big}'s weak reference was cleared. No call
 *       between the death and the allocations.</li>
 *   <li>{@code poll-site} (item 2, {@code CRATONVM_JIT_IR_POLL_GC_POINT_CLEARS}):
 *       a worker makes {@code big}, nulls it, announces, and walks a
 *       circular two-cell list until told to stop (a pointer-chasing loop,
 *       never a counted one, so every VM polls on its back edge). The main
 *       thread collects while the worker spins.</li>
 *   <li>{@code monitor-site} (item 1): as in the i4 probe, the worker parks on
 *       a contended {@code monitorenter} after {@code big} died.</li>
 *   <li>{@code call-site}: the control. The GC point after the local died is
 *       a call, armed by default.</li>
 * </ul>
 *
 * <p>Stdout is deterministic: {@code multi-site cleared=true},
 * {@code poll-site cleared=true}, {@code monitor-site cleared=true},
 * {@code call-site cleared=true}, {@code PROBE-OK} on HotSpot 25
 * ({@code java -XX:+UseSerialGC -Xmx64m GciIr5UnarmedSitesProbe}).
 * Diagnostics (worker states and frames) go to stderr.
 */
public final class GciIr5UnarmedSitesProbe {
    static final int BIG = 1 << 20;
    static final Object LOCK = new Object();
    static volatile int phase;
    static volatile boolean stop;
    static WeakReference<byte[]> ref;
    static byte[][] junk;
    static long sink;

    static final class Cell {
        Cell next;
    }

    static byte[] make(int n) {
        byte[] a = new byte[n];
        a[n - 1] = 1;
        ref = new WeakReference<>(a);
        return a;
    }

    static boolean gcAndCheck(int rounds) {
        for (int i = 0; i < rounds && ref.get() != null; i++) {
            System.gc();
        }
        return ref.get() == null;
    }

    /** The control: the GC point after the local died is a call. */
    static boolean viaCall(int n, int rounds) {
        byte[] big = make(n);
        int r = big.length + big[n - 1];
        big = null;
        sink += r;
        return gcAndCheck(rounds);
    }

    /** The GC points after the local died are multianewarrays, in this thread. */
    static boolean viaMulti(int n, int k) {
        byte[] big = make(n);
        int r = big.length + big[n - 1];
        big = null;
        sink += r;
        for (int i = 0; i < k; i++) {
            junk = new byte[4][1024];
        }
        return ref.get() == null;
    }

    /** The GC point after the local died is the loop's back-edge poll. */
    static int viaPoll(int n, Cell head) {
        byte[] big = make(n);
        int r = big.length + big[n - 1];
        big = null;
        phase = n;
        Cell p = head;
        while (p != null && !stop) {
            p = p.next;
            r++;
        }
        return r;
    }

    /** The GC point after the local died is a contended monitorenter. */
    static int viaMonitor(int n) {
        byte[] big = make(n);
        int r = big.length + big[n - 1];
        big = null;
        phase = n;
        synchronized (LOCK) {
            r += 1;
        }
        return r;
    }

    static Cell chain(int len, boolean circular) {
        Cell first = new Cell();
        Cell last = first;
        for (int i = 1; i < len; i++) {
            Cell c = new Cell();
            last.next = c;
            last = c;
        }
        if (circular) {
            last.next = first;
        }
        return first;
    }

    static void pause(long ms) {
        try {
            Thread.sleep(ms);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }

    static void waitPhase(int want) {
        long waited = 0;
        while (phase != want && waited < 30_000) {
            pause(5);
            waited += 5;
        }
    }

    static void report(String what, Thread worker, boolean frames) {
        StringBuilder sb = new StringBuilder("[probe] ");
        sb.append(what).append(" worker state after the collection: ").append(worker.getState());
        if (frames) {
            StackTraceElement[] st = worker.getStackTrace();
            for (int i = 0; i < st.length && i < 3; i++) {
                sb.append(" | ").append(st[i].getMethodName()).append(':').append(st[i].getLineNumber());
            }
        }
        System.err.println(sb);
    }

    public static void main(String[] args) throws Exception {
        long warm = 0;
        Cell open = chain(4, false);
        for (int round = 0; round < 2; round++) {
            for (int i = 0; i < 20_000; i++) {
                int n = (i & 511) == 0 ? BIG : 16 + (i & 15);
                warm += viaCall(n, (i & 2047) == 0 ? 1 : 0) ? 1 : 0;
                warm += viaMulti(n, 2) ? 1 : 0;
                warm += viaPoll(n, open);
                warm += viaMonitor(n);
            }
            if (round == 0) {
                pause(1000);
            }
        }
        sink += warm;

        boolean multi = viaMulti(BIG, 32_768);
        System.out.println("multi-site cleared=" + multi);

        final int[] result = new int[2];
        final Cell ring = chain(2, true);
        stop = false;
        phase = 0;
        Thread poller = new Thread(() -> result[0] = viaPoll(BIG, ring), "gci-ir5-poll");
        poller.start();
        waitPhase(BIG);
        // Let the worker reach its loop and spin in it.
        pause(300);
        boolean pollCleared = gcAndCheck(3);
        report("poll-site", poller, false);
        stop = true;
        poller.join();
        System.out.println("poll-site cleared=" + pollCleared);

        phase = 0;
        Thread blocked = new Thread(() -> result[1] = viaMonitor(BIG), "gci-ir5-monitor");
        boolean monitorCleared;
        synchronized (LOCK) {
            blocked.start();
            waitPhase(BIG);
            // Let the worker reach the monitor and park on it.
            pause(300);
            monitorCleared = gcAndCheck(3);
            report("monitor-site", blocked, true);
        }
        blocked.join();
        System.out.println("monitor-site cleared=" + monitorCleared);

        boolean callCleared = viaCall(BIG, 3);
        System.out.println("call-site cleared=" + callCleared);
        System.err.println("[probe] sink=" + sink + " result=" + result[0] + "," + result[1]);
        System.out.println("PROBE-OK");
    }
}
