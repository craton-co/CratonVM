// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;

/**
 * gci i4/ir (2026-10-12): a dead reference held by an optimizing-tier frame
 * whose thread stops for a collection at a GC point that is NOT a call
 * ({@code gci-i3ir-unarmed-optimizing-gc-points-keep-dead-homes-20261012}).
 *
 * <p>Two cases, both warmed into the optimizing tier first:
 * <ul>
 *   <li>{@code call-site}: the control. The method makes a large array, reads
 *       it, nulls its local and calls {@link #gcAndCheck()}, which collects and
 *       asks whether the array's weak reference was cleared. The call is an
 *       armed site by default, so this passes on the default arm.</li>
 *   <li>{@code monitor-site}: the same shape, but the next GC point after the
 *       local died is a contended {@code monitorenter}. The main thread holds
 *       the lock, waits until the worker has announced it is about to enter,
 *       collects while the worker is parked on the monitor, and asks the same
 *       question. Unarmed (the default), the parked frame's map names the dead
 *       array's home and the array stays reachable.</li>
 * </ul>
 *
 * <p>A third line checks the soundness of the parameter-home zeroing for a
 * static method's parameter 0 ({@code static-param0 sum=51}).
 *
 * <p>Stdout is deterministic: {@code call-site cleared=true},
 * {@code monitor-site cleared=true}, {@code static-param0 sum=51},
 * {@code PROBE-OK} on HotSpot 25
 * ({@code java -XX:+UseSerialGC -Xmx64m GciIr4UnarmedGcPointProbe}).
 * Diagnostics go to stderr.
 */
public final class GciIr4UnarmedGcPointProbe {
    static final Object LOCK = new Object();
    static volatile int phase;
    static WeakReference<byte[]> ref;
    static long sink;

    static byte[] make(int n) {
        byte[] a = new byte[n];
        a[n - 1] = 1;
        ref = new WeakReference<>(a);
        return a;
    }

    static boolean gcAndCheck() {
        for (int i = 0; i < 3 && ref.get() != null; i++) {
            System.gc();
        }
        return ref.get() == null;
    }

    /** Collects and checks only when judged; the branch is here, not in the caller. */
    static boolean check(boolean judged) {
        if (!judged) {
            return true;
        }
        return gcAndCheck();
    }

    /**
     * The control: the GC point after the local died is a call. No branch on
     * {@code judged} in this body, so the judged call runs the warmed code.
     */
    static boolean viaCall(int n, boolean judged) {
        byte[] big = make(n);
        int r = big.length + big[n - 1];
        big = null;
        sink += r;
        return check(judged);
    }

    /**
     * The GC point after the local died is a contended monitorenter. The
     * unconditional volatile store announces the size about to park.
     */
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

    /**
     * Soundness of the parameter-home zeroing for a STATIC method's parameter
     * 0 ({@code CRATONVM_JIT_IR_REF_PARAM_HOME_ZERO}): {@code a} is read again
     * after a GC point. A wrongly zeroed home shows as a
     * {@code NullPointerException} or a different sum.
     */
    static int staticParam0(int[] a, boolean judged) {
        int r = a[0];
        check(judged);
        r += a[1] + a.length;
        return r;
    }

    static void pause(long ms) {
        try {
            Thread.sleep(ms);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }

    public static void main(String[] args) throws Exception {
        long warm = 0;
        int[] small = {5, 7, 9};
        for (int round = 0; round < 2; round++) {
            for (int i = 0; i < 20_000; i++) {
                warm += viaCall(16 + (i & 15), false) ? 1 : 0;
                warm += viaMonitor(16 + (i & 15));
                warm += staticParam0(small, false);
            }
            if (round == 0) {
                pause(1000);
            }
        }
        sink += warm;

        boolean callCleared = viaCall(8 << 20, true);
        System.out.println("call-site cleared=" + callCleared);

        final int big = 8 << 20;
        final int[] result = new int[1];
        Thread worker = new Thread(() -> result[0] = viaMonitor(big), "gci-ir4-worker");
        boolean monitorCleared;
        synchronized (LOCK) {
            phase = 0;
            worker.start();
            long waited = 0;
            while (phase != big && waited < 30_000) {
                pause(5);
                waited += 5;
            }
            // Let the worker reach the monitor and park on it.
            pause(300);
            System.err.println("[probe] worker state at the collection: " + worker.getState());
            monitorCleared = gcAndCheck();
        }
        worker.join();
        System.out.println("monitor-site cleared=" + monitorCleared);
        int p0 = 0;
        for (int k = 0; k < 3; k++) {
            p0 += staticParam0(new int[] {5 + k, 7, 9, 11}, true);
        }
        System.out.println("static-param0 sum=" + p0);
        System.err.println("[probe] sink=" + sink + " result=" + result[0]);
        System.out.println("PROBE-OK");
    }
}
