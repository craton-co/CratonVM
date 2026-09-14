// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.atomic.AtomicIntegerArray;
import java.util.concurrent.atomic.AtomicLongArray;
import java.util.concurrent.atomic.AtomicReferenceArray;

/**
 * Atomicity of the java.util.concurrent.atomic ARRAY classes.
 *
 * CratonVM backs AtomicIntegerArray/AtomicLongArray/AtomicReferenceArray with
 * a plain Java array and implements every operation as a native. Those natives
 * used to be a bare read followed by a bare write with nothing in between, so
 * compareAndSet was not a compare-and-swap at all: two threads could both read
 * the expected value and both report success.
 *
 * H2's TestFileSystem.testConcurrent uses exactly that idiom as a spin lock
 * ("while (!locks.compareAndSet(pos, 0, 1)) {}"), so a writer and a reader
 * could hold the same lock at once. This class reproduces the defect directly
 * and deterministically enough to be a regression gate:
 *
 *   1. contended compareAndSet-as-a-lock  — a broken CAS lets two threads into
 *      the critical section, which the shared counter detects.
 *   2. contended getAndIncrement          — a broken RMW loses updates.
 *   3. single-threaded semantics          — CAS/RMW return values still match
 *      the JDK spec (this is what HotSpot's output is diffed against).
 */
public class RAtomicArray {

    static final int SLOTS = 8;
    static final int THREADS = 4;
    static final int ITERS = 20000;

    static int checks = 0;

    static void check(boolean cond, String what) {
        checks++;
        if (!cond) {
            throw new AssertionError("RAtomicArray: " + what);
        }
    }

    public static void main(String[] args) throws Exception {
        singleThreaded();
        casMutualExclusion();
        lostUpdates();
        referenceArrayCas();
        System.out.println("PASS RAtomicArray (" + checks + " checks)");
    }

    // ---- 1. single-threaded spec conformance -------------------------------

    static void singleThreaded() {
        AtomicIntegerArray a = new AtomicIntegerArray(4);
        a.set(1, 42);
        check(a.get(1) == 42, "set/get");
        check(a.compareAndSet(1, 42, 100), "cas success");
        check(a.get(1) == 100, "cas wrote");
        check(!a.compareAndSet(1, 42, 7), "cas failure on wrong witness");
        check(a.get(1) == 100, "failed cas did not write");
        check(a.getAndSet(1, 5) == 100, "getAndSet returns previous");
        check(a.get(1) == 5, "getAndSet wrote");
        check(a.getAndIncrement(1) == 5, "getAndIncrement returns previous");
        check(a.get(1) == 6, "getAndIncrement wrote");
        check(a.incrementAndGet(1) == 7, "incrementAndGet returns new");
        check(a.getAndDecrement(1) == 7, "getAndDecrement returns previous");
        check(a.decrementAndGet(1) == 5, "decrementAndGet returns new");
        check(a.getAndAdd(1, 10) == 5, "getAndAdd returns previous");
        check(a.get(1) == 15, "getAndAdd wrote");
        check(a.addAndGet(1, -5) == 10, "addAndGet returns new");
        check(a.length() == 4, "length");
        System.out.println("CK aia-single " + a.get(1) + " " + a.length());

        AtomicLongArray l = new AtomicLongArray(3);
        l.set(0, 1L << 40);
        check(l.get(0) == (1L << 40), "ala set/get keeps 64 bits");
        check(l.compareAndSet(0, 1L << 40, -1L), "ala cas success");
        check(l.get(0) == -1L, "ala cas wrote");
        check(!l.compareAndSet(0, 0L, 9L), "ala cas failure");
        check(l.getAndIncrement(0) == -1L, "ala getAndIncrement");
        check(l.incrementAndGet(0) == 1L, "ala incrementAndGet");
        check(l.getAndAdd(0, 5L) == 1L, "ala getAndAdd");
        check(l.addAndGet(0, 4L) == 10L, "ala addAndGet");
        check(l.getAndSet(0, 77L) == 10L, "ala getAndSet");
        check(l.length() == 3, "ala length");
        System.out.println("CK ala-single " + l.get(0) + " " + l.length());

        AtomicReferenceArray<String> r = new AtomicReferenceArray<>(2);
        String s1 = "one";
        String s2 = "two";
        r.set(0, s1);
        check(r.get(0) == s1, "ara set/get");
        check(r.compareAndSet(0, s1, s2), "ara cas success");
        check(r.get(0) == s2, "ara cas wrote");
        check(!r.compareAndSet(0, s1, null), "ara cas failure");
        check(r.getAndSet(0, null) == s2, "ara getAndSet returns previous");
        check(r.get(0) == null, "ara getAndSet wrote null");
        check(r.compareAndSet(0, null, s1), "ara cas from null");
        check(r.length() == 2, "ara length");
        System.out.println("CK ara-single " + r.get(0) + " " + r.length());
    }

    // ---- 2. compareAndSet used as a mutual-exclusion lock ------------------
    //
    // Each slot is a lock. A thread acquires it with compareAndSet(i, 0, 1) and
    // releases it with set(i, 0). Inside the section it bumps a plain (racy)
    // counter twice and checks the two reads agree; a second thread inside the
    // same section at the same time makes them disagree. A broken CAS fails
    // this within a few thousand iterations; a correct one never does.

    static final int[] guarded = new int[SLOTS];
    static volatile String breach = null;

    static void casMutualExclusion() throws Exception {
        final AtomicIntegerArray locks = new AtomicIntegerArray(SLOTS);
        Thread[] ts = new Thread[THREADS];
        for (int t = 0; t < THREADS; t++) {
            ts[t] = new Thread(() -> {
                for (int i = 0; i < ITERS && breach == null; i++) {
                    int pos = i % SLOTS;
                    while (!locks.compareAndSet(pos, 0, 1)) {
                        Thread.yield();
                    }
                    try {
                        int before = guarded[pos];
                        guarded[pos] = before + 1;
                        // A concurrent holder shows up as a value we did not write.
                        if (guarded[pos] != before + 1) {
                            breach = "two threads inside the section for slot " + pos;
                        }
                        if (locks.get(pos) != 1) {
                            breach = "lock slot " + pos + " not held while inside section";
                        }
                    } finally {
                        locks.set(pos, 0);
                    }
                }
            });
            ts[t].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        check(breach == null, "compareAndSet is not atomic: " + breach);
        // Every increment must have landed exactly once.
        int total = 0;
        for (int v : guarded) {
            total += v;
        }
        check(total == THREADS * ITERS,
                "guarded increments lost: expected " + (THREADS * ITERS) + " got " + total);
        System.out.println("CK cas-mutex " + total);
    }

    // ---- 3. contended read-modify-write ------------------------------------

    static void lostUpdates() throws Exception {
        final AtomicIntegerArray counters = new AtomicIntegerArray(SLOTS);
        final AtomicLongArray lcounters = new AtomicLongArray(SLOTS);
        Thread[] ts = new Thread[THREADS];
        for (int t = 0; t < THREADS; t++) {
            ts[t] = new Thread(() -> {
                for (int i = 0; i < ITERS; i++) {
                    counters.getAndIncrement(i % SLOTS);
                    lcounters.addAndGet(i % SLOTS, 2L);
                }
            });
            ts[t].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        int total = 0;
        long ltotal = 0;
        for (int i = 0; i < SLOTS; i++) {
            total += counters.get(i);
            ltotal += lcounters.get(i);
        }
        check(total == THREADS * ITERS,
                "AtomicIntegerArray.getAndIncrement lost updates: expected "
                        + (THREADS * ITERS) + " got " + total);
        check(ltotal == 2L * THREADS * ITERS,
                "AtomicLongArray.addAndGet lost updates: expected "
                        + (2L * THREADS * ITERS) + " got " + ltotal);
        System.out.println("CK rmw-total " + total + " " + ltotal);
    }

    // ---- 4. AtomicReferenceArray CAS under contention ----------------------
    //
    // Exactly one thread may win each slot's null -> token CAS.

    static void referenceArrayCas() throws Exception {
        final int rounds = 2000;
        final AtomicReferenceArray<Object> cells = new AtomicReferenceArray<>(rounds);
        final AtomicIntegerArray wins = new AtomicIntegerArray(THREADS);
        Thread[] ts = new Thread[THREADS];
        for (int t = 0; t < THREADS; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                Object token = new Object();
                for (int i = 0; i < rounds; i++) {
                    if (cells.compareAndSet(i, null, token)) {
                        wins.getAndIncrement(id);
                    }
                }
            });
            ts[t].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        int totalWins = 0;
        for (int t = 0; t < THREADS; t++) {
            totalWins += wins.get(t);
        }
        check(totalWins == rounds,
                "AtomicReferenceArray.compareAndSet let " + totalWins
                        + " winners claim " + rounds + " slots");
        for (int i = 0; i < rounds; i++) {
            check(cells.get(i) != null, "slot " + i + " never claimed");
        }
        System.out.println("CK ara-cas " + totalWins);
    }
}
