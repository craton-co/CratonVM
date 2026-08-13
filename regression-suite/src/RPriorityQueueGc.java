// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.PriorityBlockingQueue;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * PriorityBlockingQueue of a custom Comparable, offered under GC pressure.
 *
 * CratonVM backs PriorityBlockingQueue with a native sorted-array insert whose
 * binary search dispatches the element's REAL, interpreted Comparable.compareTo
 * for anything that is not a String or a homogeneous primitive wrapper. That is
 * a full re-entry into the VM: it can allocate and trigger a young collection.
 * The native used to capture the backing array and the element being inserted
 * ONCE, before the search, and keep using them afterwards - so a collection in
 * the middle of the search left it writing through relocated references. Two
 * observed shapes:
 *
 *   * the next iteration's array read decoded a non-array ("left: Object,
 *     right: Array" in gen_heap), aborting the VM; and
 *   * the final insert stored a stale element reference, which a later poll()
 *     read back as an unrelated object - H2 MVStore's "java.lang.Object cannot
 *     be cast to org.h2.mvstore.FileStore$RemovedPageInfo", raised from
 *     FileStore.accountForRemovedPage during concurrent compaction.
 *
 * The same natives also blocked on the queue monitor as a *counted* mutator, so
 * a writer waiting for the monitor never arrived at a concurrent stop-the-world
 * barrier and the whole VM wedged.
 *
 * compareTo() allocates deliberately here so the collection lands inside the
 * native, making both shapes reproducible rather than load-dependent.
 *
 * REQUIRED CratonVM ARGUMENTS: --nojit --Xmx 64m (BOTH; see run.sh
 * class_cv_args). --nojit makes the young generation an actual copying
 * collector - a live JIT frame downgrades it to a non-moving sweep, under which
 * the stale reference still resolves and the defect hides. --Xmx 64m is what
 * makes a collection happen inside the native at all; on the default heap the
 * walk finishes without one and the class passes on a broken VM. Both flags
 * together are what 6cd01bcba registered this class with and validated
 * FAIL-then-PASS under; a later run.sh merge dropped the registration and the
 * hook alike, so whoever re-registers it must check that class_cv_args names
 * BOTH and not just --nojit. HotSpot deliberately gets NEITHER - they are
 * CratonVM spellings and the expected output does not depend on them.
 */
public class RPriorityQueueGc {

    static final int SINGLE_ITEMS = 400;
    static final int SINGLE_ROUNDS = 3;
    static final int THREADS = 4;
    static final int PER_THREAD = 200;

    static volatile Object sink;

    /**
     * Count of assertions whose EXECUTION COUNT is deterministic, published on a
     * CK line so the cross-VM diff can see a run that silently asserted fewer
     * things than the oracle (harness guard G3; see
     * regression-suite/harness-guard.sh and
     * docs/known-issues/jdk-only/W7-60-harness-extract-blindness.md).
     */
    static int checks = 0;

    static void check(boolean cond, String what) {
        checks++;
        if (!cond) {
            throw new AssertionError("RPriorityQueueGc: " + what);
        }
    }

    /**
     * Same assertion, deliberately NOT counted. Used only inside the concurrent
     * drain loop, whose trip count is how many of the workers' interleaved
     * poll()s happened to find a non-empty queue — scheduling-dependent, and
     * therefore different on two VMs that are both correct. `checks` is printed
     * on a line the runner diffs against HotSpot, so folding these in would make
     * a CORRECT VM go red at random. The count stays a constant for a healthy
     * run and moves only when an arm stops executing, which is what G3 is for.
     */
    static void checkDyn(boolean cond, String what) {
        if (!cond) {
            throw new AssertionError("RPriorityQueueGc: " + what);
        }
    }

    /** Two long fields: neither a String nor a single-field primitive wrapper. */
    static final class Item implements Comparable<Item> {
        final long key;
        final long payload;

        Item(long key) {
            this.key = key;
            this.payload = key * 3 + 1;
        }

        @Override
        public int compareTo(Item o) {
            Object[] junk = new Object[32];
            for (int i = 0; i < junk.length; i++) {
                junk[i] = new byte[256];
            }
            sink = junk;
            return Long.compare(this.key, o.key);
        }

        @Override
        public String toString() {
            return "Item(" + key + "," + payload + ")";
        }
    }

    public static void main(String[] args) throws Exception {
        singleThreaded();
        concurrent();
        System.out.println("CK RPriorityQueueGc checks=" + checks);
        System.out.println("PASS RPriorityQueueGc (" + checks + " checks)");
    }

    // ---- 1. sorted insert survives a GC inside compareTo -------------------

    static void singleThreaded() {
        long sum = 0;
        for (int round = 0; round < SINGLE_ROUNDS; round++) {
            PriorityBlockingQueue<Item> q = new PriorityBlockingQueue<Item>();
            for (int i = 0; i < SINGLE_ITEMS; i++) {
                q.offer(new Item((i * 7919) % SINGLE_ITEMS));
            }
            check(q.size() == SINGLE_ITEMS, "size after offers: " + q.size());
            long prev = Long.MIN_VALUE;
            for (int i = 0; i < SINGLE_ITEMS; i++) {
                Object o = q.poll();
                check(o instanceof Item,
                        "poll #" + i + " returned "
                                + (o == null ? "null" : o.getClass().getName()));
                Item it = (Item) o;
                check(it.payload == it.key * 3 + 1, "poll #" + i + " corrupted: " + it);
                check(it.key >= prev, "poll #" + i + " out of order: " + it.key + " after " + prev);
                prev = it.key;
                sum += it.key;
            }
            check(q.poll() == null, "queue not empty after draining");
        }
        System.out.println("CK pbq-sorted " + sum);
    }

    // ---- 2. concurrent writers + a stop-the-world collector ----------------

    static void concurrent() throws Exception {
        final PriorityBlockingQueue<Item> q = new PriorityBlockingQueue<Item>();
        final AtomicInteger errors = new AtomicInteger();
        final AtomicInteger net = new AtomicInteger();
        // Counts offers that actually happened. Without it a run in which every
        // worker died on its first statement leaves drained == net == 0 and
        // errors == 0, and this phase reports "ok" having exercised nothing.
        final AtomicInteger offered = new AtomicInteger();
        Thread[] ts = new Thread[THREADS];
        for (int t = 0; t < THREADS; t++) {
            final int base = t * PER_THREAD;
            ts[t] = new Thread(new Runnable() {
                public void run() {
                    for (int i = 0; i < PER_THREAD; i++) {
                        q.offer(new Item((base + i) * 7919L % 100003L));
                        offered.incrementAndGet();
                        net.incrementAndGet();
                        if ((i & 7) == 0) {
                            Object o = q.poll();
                            if (o == null) {
                                continue;
                            }
                            if (!(o instanceof Item)) {
                                errors.incrementAndGet();
                                return;
                            }
                            Item it = (Item) o;
                            if (it.payload != it.key * 3 + 1) {
                                errors.incrementAndGet();
                                return;
                            }
                            net.decrementAndGet();
                        }
                    }
                }
            });
        }
        for (Thread t : ts) {
            t.start();
        }
        for (Thread t : ts) {
            t.join();
        }
        int drained = 0;
        long prev = Long.MIN_VALUE;
        Object o;
        while ((o = q.poll()) != null) {
            checkDyn(o instanceof Item, "drain returned " + o.getClass().getName());
            Item it = (Item) o;
            checkDyn(it.payload == it.key * 3 + 1, "drain corrupted: " + it);
            checkDyn(it.key >= prev, "drain out of order: " + it.key + " after " + prev);
            prev = it.key;
            drained++;
        }
        check(errors.get() == 0, errors.get() + " concurrent reader error(s)");
        // An erroring worker returns early, so this would also be short - but
        // the errors check above has already fired by then, which is why it
        // comes first.
        check(offered.get() == THREADS * PER_THREAD,
                "workers offered " + offered.get() + " of " + (THREADS * PER_THREAD));
        check(drained == net.get(), "drained " + drained + " but net offered " + net.get());
        // Deliberately not the drained count: how many of the interleaved
        // poll()s find a non-empty queue is scheduling-dependent, and this line
        // is diffed against HotSpot's.
        System.out.println("CK pbq-concurrent ok");
    }
}
