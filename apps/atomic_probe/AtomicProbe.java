// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicReference;

/**
 * Wave 4 / Task A probe: {@code java.util.concurrent.atomic} compare-and-set.
 *
 * <p>Driven by {@code vm/tests/wave4_a_atomic.rs}. Every value on every printed
 * line is the RESULT of the CAS call or a subsequent {@code get()} — nothing is
 * printed as an expected literal — so a VM whose CAS is non-atomic, or which
 * compares by {@code equals()} instead of reference identity, prints different
 * text and the test goes red.
 *
 * <p>Lines emitted, in order:
 * <ul>
 *   <li>{@code ai.cas1=.. cas2=.. val=..} — sequential {@link AtomicInteger}
 *       CAS: the first swap sees the expected witness and must succeed, the
 *       second offers a stale witness and must fail.</li>
 *   <li>{@code al.cas=.. val=..} — the same for {@link AtomicLong}, which
 *       traverses the 64-bit descriptor branch of the CAS primitive.</li>
 *   <li>{@code ar.rcas1=.. rcas2=.. val=..} — {@link AtomicReference} must
 *       compare by reference identity. {@code rcas2} offers a FRESH
 *       {@code new String("world")}: equal to the value currently held, but not
 *       the same object. An identity CAS refuses it and the held value stays
 *       {@code "world"}; an {@code equals()}-based CAS would accept it and swap
 *       in {@code "hello"}, which is what makes this line load-bearing.</li>
 *   <li>{@code contended.final=.. expected=..} — 8 threads each running 100 000
 *       {@code do { old = get(); } while (!compareAndSet(old, old + 1))} loops.
 *       A non-atomic CAS loses updates and the final count comes in low; a
 *       broken CAS that never publishes livelocks and trips the test's
 *       subprocess timeout.</li>
 *   <li>{@code OK} — reached only if every thread joined and main() returned
 *       normally.</li>
 * </ul>
 */
public final class AtomicProbe {

    private static final int THREADS = 8;
    private static final int ITERATIONS = 100_000;

    public static void main(String[] args) throws InterruptedException {
        sequentialInt();
        sequentialLong();
        referenceIdentity();
        contended();
        System.out.println("OK");
    }

    /** First CAS sees the real witness and wins; the second is stale and loses. */
    private static void sequentialInt() {
        AtomicInteger ai = new AtomicInteger(0);
        boolean cas1 = ai.compareAndSet(0, 1);
        boolean cas2 = ai.compareAndSet(0, 2);
        System.out.println("ai.cas1=" + cas1 + " cas2=" + cas2 + " val=" + ai.get());
    }

    /** Same shape on the 64-bit path. */
    private static void sequentialLong() {
        AtomicLong al = new AtomicLong(0L);
        boolean cas = al.compareAndSet(0L, 100L);
        System.out.println("al.cas=" + cas + " val=" + al.get());
    }

    /**
     * Reference CAS must be {@code ==}, never {@code equals()}.
     *
     * <p>{@code rcas1} passes the very object the reference holds, so it must
     * succeed. {@code rcas2} passes a distinct-but-equal witness and a
     * distinguishable replacement, so an {@code equals()}-based CAS is caught
     * twice over: {@code rcas2} flips to {@code true} AND the trailing
     * {@code val=} changes from {@code world} to {@code hello}.
     */
    private static void referenceIdentity() {
        String s1 = new String("hello");
        String s2 = new String("world");
        AtomicReference<String> ar = new AtomicReference<>(s1);

        boolean rcas1 = ar.compareAndSet(s1, s2);
        boolean rcas2 = ar.compareAndSet(new String("world"), new String("hello"));

        System.out.println("ar.rcas1=" + rcas1 + " rcas2=" + rcas2 + " val=" + ar.get());
    }

    /** 8 threads x 100 000 CAS-loop increments; every store must commit. */
    private static void contended() throws InterruptedException {
        final AtomicInteger counter = new AtomicInteger(0);
        Thread[] workers = new Thread[THREADS];
        for (int t = 0; t < THREADS; t++) {
            workers[t] = new Thread(new Runnable() {
                @Override
                public void run() {
                    for (int i = 0; i < ITERATIONS; i++) {
                        int old;
                        do {
                            old = counter.get();
                        } while (!counter.compareAndSet(old, old + 1));
                    }
                }
            }, "cas-" + t);
        }
        for (Thread w : workers) {
            w.start();
        }
        for (Thread w : workers) {
            w.join();
        }
        System.out.println("contended.final=" + counter.get()
                + " expected=" + (THREADS * ITERATIONS));
    }
}
