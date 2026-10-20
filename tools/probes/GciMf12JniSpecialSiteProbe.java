/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 */

import java.util.Arrays;

/**
 * gci i12/mf (2026-10-13): soundness and cost of the compiled statically bound
 * (kind-1) JNI call-site rows ({@code vm_exec} "gci i12/mf" kind-1 block,
 * inside {@code CRATONVM_JNI_DISPATCH_MEMO}). The shapes a JNI library's own
 * code calls through {@code invokespecial} or a devirtualized
 * {@code invokevirtual}:
 *
 * <ul>
 *   <li>{@code private}: a {@code private native} called from its own class;
 *   <li>{@code final}: a {@code final native} of a non-final class, called on
 *       receivers of the class and of a subclass;
 *   <li>{@code super}: {@code super.fin(x)} from the subclass;
 *   <li>{@code sync-private-held}: a {@code private synchronized native} (the
 *       door must hold the receiver's monitor; an upcall asks);
 *   <li>{@code thrown}: a {@code private native} that throws every 16th call;
 *   <li>{@code abstract-private}: a {@code private native} of an abstract class
 *       (the door memo refuses the abstract class, so no row may form);
 *   <li>{@code rebind}: {@code RegisterNatives} replaces the private native
 *       twice mid-run (each replacement must retire every row);
 *   <li>{@code threads}: four threads on the same site (per-thread rows).
 * </ul>
 *
 * <p>stdout is HotSpot's exact answers; the cost is on stderr as
 * {@code [gci-mf12-jni-special] priv median=<n>ns/call reps=[...]}. Usage:
 * {@code GciMf12JniSpecialSiteProbe /abs/path/gcimf12.dll [n]}. Native half:
 * {@code tools/probes/jni/GciMf12JniSpecialSiteProbe.c}.
 */
public final class GciMf12JniSpecialSiteProbe {
    static final int REPS = 7;

    static class P {
        private native int priv(int x);

        final native int fin(int x);

        private synchronized native boolean held();

        private native int thr(int x);

        int callPriv(int x) {
            return priv(x);
        }

        int callThr(int x) {
            return thr(x);
        }

        boolean callHeld() {
            return held();
        }

        boolean heldSelf() {
            return Thread.holdsLock(this);
        }
    }

    static final class Q extends P {
        int callSuperFin(int x) {
            return super.fin(x);
        }
    }

    abstract static class A {
        private native int apriv(int x);

        int callA(int x) {
            return apriv(x);
        }

        abstract int k();
    }

    static final class AC extends A {
        @Override
        int k() {
            return 1;
        }
    }

    /** RegisterNatives on {@code P.priv}: which = 0 -> x + 1, else x + 100. */
    static native int rebind(int which);

    static long privLoop(P p, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += p.callPriv(i);
        }
        return s;
    }

    public static void main(String[] args) throws Exception {
        if (args.length < 1) {
            System.out.println("usage: GciMf12JniSpecialSiteProbe /abs/path/gcimf12.dll [n]");
            return;
        }
        System.load(args[0]);
        final int n = args.length > 1 ? Integer.parseInt(args[1]) : 200000;

        final P p = new P();
        System.out.println("private: " + privLoop(p, n));

        P[] ps = {new P(), new Q()};
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += ps[i & 1].fin(i);
        }
        System.out.println("final: " + s);

        Q q = new Q();
        s = 0;
        for (int i = 0; i < n; i++) {
            s += q.callSuperFin(i);
        }
        System.out.println("super: " + s);

        boolean allHeld = true;
        for (int i = 0; i < 20000; i++) {
            allHeld &= p.callHeld();
        }
        System.out.println("sync-private-held: " + allHeld);

        int caught = 0;
        s = 0;
        for (int i = 0; i < n; i++) {
            try {
                s += p.callThr(i % 16);
            } catch (IllegalStateException e) {
                if ("seven".equals(e.getMessage())) {
                    caught++;
                }
            }
        }
        System.out.println("thrown: " + caught + " sum: " + s);

        A a = new AC();
        s = 0;
        for (int i = 0; i < n; i++) {
            s += a.callA(i) + a.k();
        }
        System.out.println("abstract-private: " + s);

        s = 0;
        System.out.println("rebind-b: " + rebind(1));
        s += privLoop(p, n);
        System.out.println("rebind-a: " + rebind(0));
        s += privLoop(p, n);
        System.out.println("rebind-sum: " + s);

        final int threads = 4;
        final long[] sums = new long[threads];
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int slot = t;
            ts[t] = new Thread(() -> {
                P own = new P();
                sums[slot] = privLoop(own, 50000) + privLoop(p, 50000);
            });
            ts[t].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        long all = 0;
        for (long v : sums) {
            all += v;
        }
        System.out.println("threads: " + all);

        final long expected = (long) n * (n - 1) / 2 + n;
        long[] ns = new long[REPS];
        boolean ok = true;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            long got = privLoop(p, n);
            ns[r] = (System.nanoTime() - t0) / n;
            ok &= got == expected;
        }
        long[] sorted = ns.clone();
        Arrays.sort(sorted);
        System.err.println("[gci-mf12-jni-special] priv median=" + sorted[REPS / 2]
                + "ns/call reps=" + Arrays.toString(ns));
        System.out.println(ok ? "cost-checksum: ok" : "cost-checksum: FAIL");
        System.out.println("done");
    }
}
