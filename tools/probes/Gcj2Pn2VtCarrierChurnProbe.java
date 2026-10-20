// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.LockSupport;

/**
 * gcj j2/pn2: virtual threads park and resume across moving young
 * collections while their carriers sit idle (blocked in the scheduler's
 * native park) -- the shape of
 * {@code docs/known-issues/gc/gcj-j1pn-raw-root-write-owner-findings-20261019.md}
 * item 1: a parked continuation stayed the newest claimant of its last
 * carrier's OS tid, so the blocked carrier's own captured stack words were
 * routed to nobody ({@code CRATONVM_GEN_CLAIMANT_SKIPS_UNMOUNTED}, default ON,
 * {@code =0} restores the old rule).
 *
 * <p>Each of {@code vthreads} virtual threads runs {@code rounds} rounds: it
 * builds a short young linked list, parks (unmounting), resumes and checks the
 * list's checksum, and once in a while pins its carrier ({@code synchronized}
 * around a sleep) so a carrier also blocks while MOUNTED. The main thread
 * allocates garbage meanwhile, so young collections run while carriers are
 * parked. Every list must read back exactly what was written.
 *
 * <p>stdout (deterministic): {@code PASS vt-carrier-churn} or
 * {@code FAIL vt-carrier-churn bad=<n>}; timings go to stderr. Args:
 * {@code [vthreads] [rounds]} (default 64 200). HotSpot:
 * {@code java -XX:+UseSerialGC -Xmx64m Gcj2Pn2VtCarrierChurnProbe} prints
 * {@code PASS vt-carrier-churn}.
 */
public class Gcj2Pn2VtCarrierChurnProbe {
    static final class Node {
        final long v;
        final Node next;

        Node(long v, Node next) {
            this.v = v;
            this.next = next;
        }
    }

    static final Object LOCK = new Object();
    static volatile long sink;

    static Node build(long seed, int n) {
        Node head = null;
        for (int i = 0; i < n; i++) {
            head = new Node(seed * 31 + i, head);
        }
        return head;
    }

    static long sum(Node n) {
        long s = 0;
        for (; n != null; n = n.next) {
            s += n.v;
        }
        return s;
    }

    static long expected(long seed, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += seed * 31 + i;
        }
        return s;
    }

    public static void main(String[] args) throws Exception {
        final int vthreads = args.length > 0 ? Integer.parseInt(args[0]) : 64;
        final int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 200;
        final AtomicLong bad = new AtomicLong();
        final CountDownLatch finished = new CountDownLatch(vthreads);
        final long t0 = System.nanoTime();
        for (int t = 0; t < vthreads; t++) {
            final int id = t;
            Thread.ofVirtual().start(() -> {
                try {
                    for (int r = 0; r < rounds; r++) {
                        final long seed = (long) id * 100_003L + r;
                        final int n = 8 + (r % 24);
                        Node list = build(seed, n);
                        if (r % 17 == 0) {
                            // Pin the carrier: it blocks while this thread
                            // stays mounted.
                            synchronized (LOCK) {
                                Thread.sleep(1);
                            }
                        } else {
                            LockSupport.parkNanos(200_000L);
                        }
                        if (sum(list) != expected(seed, n)) {
                            bad.incrementAndGet();
                        }
                    }
                } catch (InterruptedException e) {
                    bad.incrementAndGet();
                } finally {
                    finished.countDown();
                }
            });
        }
        // Churn young while the virtual threads park.
        long acc = 0;
        while (finished.getCount() > 0) {
            for (int i = 0; i < 2000; i++) {
                byte[] b = new byte[256];
                acc += b.length;
            }
            Thread.onSpinWait();
        }
        finished.await();
        sink = acc;
        System.err.println("Gcj2Pn2VtCarrierChurnProbe: vthreads=" + vthreads + " rounds=" + rounds
                + " ms=" + (System.nanoTime() - t0) / 1_000_000 + " bad=" + bad.get());
        if (bad.get() == 0) {
            System.out.println("PASS vt-carrier-churn");
        } else {
            System.out.println("FAIL vt-carrier-churn bad=" + bad.get());
        }
    }
}
