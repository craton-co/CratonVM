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
        boundsContract();
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

    // ---- 5. the BOUNDS contract --------------------------------------------
    //
    // Every accessor above is exercised only at in-range indices, so none of
    // it says anything about W8-C15-1: CratonVM's atomic-array natives used to
    // decode the index with a bare `as usize` and never compare it against the
    // array length at all, so `ARA.get(-1)` answered `null` instead of
    // throwing, and `AIA.compareAndSet(-1, 0, 9)` answered `true` having
    // written nothing. HotSpot throws `ArrayIndexOutOfBoundsException` from
    // every accessor and `NegativeArraySizeException` from every constructor,
    // for every one of the three classes, and this method is the fixture that
    // says so where the four methods above are silent. An in-range control
    // (below) keeps this from passing by making every call throw, and the
    // neighbour-integrity checks after each class's writes are the
    // memory-safety half: a defect that silently drops a write and one that
    // corrupts a neighbouring slot both leave a value in place that this
    // method compares.

    interface Op {
        void run(int idx) throws Exception;
    }

    static int boundsChecks = 0;

    static void aioobe(String what, int idx, Op op) {
        try {
            op.run(idx);
        } catch (ArrayIndexOutOfBoundsException expected) {
            boundsChecks++;
            checks++;
            return;
        } catch (Exception e) {
            throw new AssertionError("RAtomicArray: " + what + "(" + idx
                    + ") threw " + e.getClass().getName()
                    + ", not ArrayIndexOutOfBoundsException", e);
        }
        throw new AssertionError("RAtomicArray: " + what + "(" + idx
                + ") did not throw ArrayIndexOutOfBoundsException");
    }

    static void negativeArraySize(String what, Op ctor) {
        try {
            ctor.run(-1);
        } catch (NegativeArraySizeException expected) {
            checks++;
            return;
        } catch (Exception e) {
            throw new AssertionError("RAtomicArray: " + what
                    + " threw " + e.getClass().getName()
                    + ", not NegativeArraySizeException", e);
        }
        throw new AssertionError("RAtomicArray: " + what + " did not throw NegativeArraySizeException");
    }

    static final int[] BAD_INDICES = { -1, -5, -32, 3, 100, Integer.MIN_VALUE, Integer.MAX_VALUE };

    static void boundsContract() {
        boundsAia();
        boundsAla();
        boundsAra();

        negativeArraySize("new AtomicIntegerArray(-1)", i -> new AtomicIntegerArray(i));
        negativeArraySize("new AtomicLongArray(-1)", i -> new AtomicLongArray(i));
        negativeArraySize("new AtomicReferenceArray(-1)", i -> new AtomicReferenceArray<String>(i));

        System.out.println("CK RAtomicArray bounds=" + boundsChecks);
    }

    static void boundsAia() {
        AtomicIntegerArray a = new AtomicIntegerArray(3);
        a.set(0, 11);
        a.set(1, 22);
        a.set(2, 33);
        for (int idx : BAD_INDICES) {
            aioobe("AIA.get", idx, i -> a.get(i));
            aioobe("AIA.set", idx, i -> a.set(i, 9));
            aioobe("AIA.lazySet", idx, i -> a.lazySet(i, 9));
            aioobe("AIA.getAndSet", idx, i -> a.getAndSet(i, 9));
            aioobe("AIA.compareAndSet", idx, i -> a.compareAndSet(i, 0, 9));
            aioobe("AIA.weakCompareAndSet", idx, i -> a.weakCompareAndSet(i, 0, 9));
            aioobe("AIA.getAndIncrement", idx, i -> a.getAndIncrement(i));
            aioobe("AIA.getAndDecrement", idx, i -> a.getAndDecrement(i));
            aioobe("AIA.getAndAdd", idx, i -> a.getAndAdd(i, 1));
            aioobe("AIA.incrementAndGet", idx, i -> a.incrementAndGet(i));
            aioobe("AIA.decrementAndGet", idx, i -> a.decrementAndGet(i));
            aioobe("AIA.addAndGet", idx, i -> a.addAndGet(i, 1));
            aioobe("AIA.getAndUpdate", idx, i -> a.getAndUpdate(i, x -> x + 1));
            aioobe("AIA.updateAndGet", idx, i -> a.updateAndGet(i, x -> x + 1));
            aioobe("AIA.getAndAccumulate", idx, i -> a.getAndAccumulate(i, 1, Integer::sum));
            aioobe("AIA.accumulateAndGet", idx, i -> a.accumulateAndGet(i, 1, Integer::sum));
            aioobe("AIA.getPlain", idx, i -> a.getPlain(i));
            aioobe("AIA.setPlain", idx, i -> a.setPlain(i, 9));
            aioobe("AIA.getOpaque", idx, i -> a.getOpaque(i));
            aioobe("AIA.setOpaque", idx, i -> a.setOpaque(i, 9));
            aioobe("AIA.getAcquire", idx, i -> a.getAcquire(i));
            aioobe("AIA.setRelease", idx, i -> a.setRelease(i, 9));
            aioobe("AIA.compareAndExchange", idx, i -> a.compareAndExchange(i, 0, 9));
            aioobe("AIA.weakCompareAndSetPlain", idx, i -> a.weakCompareAndSetPlain(i, 0, 9));
            aioobe("AIA.weakCompareAndSetAcquire", idx, i -> a.weakCompareAndSetAcquire(i, 0, 9));
            aioobe("AIA.weakCompareAndSetRelease", idx, i -> a.weakCompareAndSetRelease(i, 0, 9));
        }
        check(a.get(0) == 11, "AIA neighbour 0 intact after out-of-range writes");
        check(a.get(1) == 22, "AIA neighbour 1 intact after out-of-range writes");
        check(a.get(2) == 33, "AIA neighbour 2 intact after out-of-range writes");
        // In-range control: the accessors above must still work at all, or a
        // body that throws unconditionally would pass every aioobe() call.
        check(a.compareAndSet(0, 11, 111), "AIA in-range compareAndSet must still work");
        check(a.get(0) == 111, "AIA in-range compareAndSet must still write");
    }

    static void boundsAla() {
        AtomicLongArray a = new AtomicLongArray(3);
        a.set(0, 111L);
        a.set(1, 222L);
        a.set(2, 333L);
        for (int idx : BAD_INDICES) {
            aioobe("ALA.get", idx, i -> a.get(i));
            aioobe("ALA.set", idx, i -> a.set(i, 9L));
            aioobe("ALA.lazySet", idx, i -> a.lazySet(i, 9L));
            aioobe("ALA.getAndSet", idx, i -> a.getAndSet(i, 9L));
            aioobe("ALA.compareAndSet", idx, i -> a.compareAndSet(i, 0L, 9L));
            aioobe("ALA.weakCompareAndSet", idx, i -> a.weakCompareAndSet(i, 0L, 9L));
            aioobe("ALA.getAndIncrement", idx, i -> a.getAndIncrement(i));
            aioobe("ALA.getAndDecrement", idx, i -> a.getAndDecrement(i));
            aioobe("ALA.getAndAdd", idx, i -> a.getAndAdd(i, 1L));
            aioobe("ALA.incrementAndGet", idx, i -> a.incrementAndGet(i));
            aioobe("ALA.decrementAndGet", idx, i -> a.decrementAndGet(i));
            aioobe("ALA.addAndGet", idx, i -> a.addAndGet(i, 1L));
            aioobe("ALA.getAndUpdate", idx, i -> a.getAndUpdate(i, x -> x + 1));
            aioobe("ALA.updateAndGet", idx, i -> a.updateAndGet(i, x -> x + 1));
            aioobe("ALA.getAndAccumulate", idx, i -> a.getAndAccumulate(i, 1L, Long::sum));
            aioobe("ALA.accumulateAndGet", idx, i -> a.accumulateAndGet(i, 1L, Long::sum));
            aioobe("ALA.getPlain", idx, i -> a.getPlain(i));
            aioobe("ALA.setPlain", idx, i -> a.setPlain(i, 9L));
            aioobe("ALA.getOpaque", idx, i -> a.getOpaque(i));
            aioobe("ALA.setOpaque", idx, i -> a.setOpaque(i, 9L));
            aioobe("ALA.getAcquire", idx, i -> a.getAcquire(i));
            aioobe("ALA.setRelease", idx, i -> a.setRelease(i, 9L));
            aioobe("ALA.compareAndExchange", idx, i -> a.compareAndExchange(i, 0L, 9L));
            aioobe("ALA.weakCompareAndSetPlain", idx, i -> a.weakCompareAndSetPlain(i, 0L, 9L));
            aioobe("ALA.weakCompareAndSetAcquire", idx, i -> a.weakCompareAndSetAcquire(i, 0L, 9L));
            aioobe("ALA.weakCompareAndSetRelease", idx, i -> a.weakCompareAndSetRelease(i, 0L, 9L));
        }
        check(a.get(0) == 111L, "ALA neighbour 0 intact after out-of-range writes");
        check(a.get(1) == 222L, "ALA neighbour 1 intact after out-of-range writes");
        check(a.get(2) == 333L, "ALA neighbour 2 intact after out-of-range writes");
        check(a.compareAndSet(0, 111L, 1111L), "ALA in-range compareAndSet must still work");
        check(a.get(0) == 1111L, "ALA in-range compareAndSet must still write");
    }

    static void boundsAra() {
        AtomicReferenceArray<String> a = new AtomicReferenceArray<>(3);
        a.set(0, "a");
        a.set(1, "b");
        a.set(2, "c");
        for (int idx : BAD_INDICES) {
            aioobe("ARA.get", idx, i -> a.get(i));
            aioobe("ARA.set", idx, i -> a.set(i, "x"));
            aioobe("ARA.lazySet", idx, i -> a.lazySet(i, "x"));
            aioobe("ARA.getAndSet", idx, i -> a.getAndSet(i, "x"));
            aioobe("ARA.compareAndSet", idx, i -> a.compareAndSet(i, null, "x"));
            aioobe("ARA.weakCompareAndSet", idx, i -> a.weakCompareAndSet(i, null, "x"));
            aioobe("ARA.weakCompareAndSetPlain", idx, i -> a.weakCompareAndSetPlain(i, null, "x"));
            aioobe("ARA.getAndUpdate", idx, i -> a.getAndUpdate(i, e -> e));
            aioobe("ARA.updateAndGet", idx, i -> a.updateAndGet(i, e -> e));
            aioobe("ARA.getAndAccumulate", idx, i -> a.getAndAccumulate(i, "x", (x, y) -> y));
            aioobe("ARA.accumulateAndGet", idx, i -> a.accumulateAndGet(i, "x", (x, y) -> y));
            aioobe("ARA.compareAndExchange", idx, i -> a.compareAndExchange(i, null, "x"));
            aioobe("ARA.getPlain", idx, i -> a.getPlain(i));
            aioobe("ARA.getAcquire", idx, i -> a.getAcquire(i));
            aioobe("ARA.setRelease", idx, i -> a.setRelease(i, "x"));
        }
        check("a".equals(a.get(0)), "ARA neighbour 0 intact after out-of-range writes");
        check("b".equals(a.get(1)), "ARA neighbour 1 intact after out-of-range writes");
        check("c".equals(a.get(2)), "ARA neighbour 2 intact after out-of-range writes");
        check(a.compareAndSet(0, "a", "aa"), "ARA in-range compareAndSet must still work");
        check("aa".equals(a.get(0)), "ARA in-range compareAndSet must still write");
    }
}
