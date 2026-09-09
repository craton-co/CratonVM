// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.lang.reflect.Field;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.Vector;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicReference;
import java.util.concurrent.atomic.AtomicReferenceArray;
import java.util.concurrent.atomic.AtomicReferenceFieldUpdater;

/**
 * Old-to-young reference-store sweep: every door a live reference can enter a
 * TENURED container through, exercised concurrently against a collector that
 * discovers old-to-young edges from a card table alone.
 *
 * <p>WHY THIS EXISTS. Until 2026-09-02 the generational collector seeded its
 * young mark from a full old-generation walk on every collection
 * ({@code scan_all_old_to_young}), so a reference store that failed to dirty a
 * card was invisible: the belt-and-braces scan found the edge anyway. That walk
 * is now off by default ({@code CRATONVM_GC_FULL_RSET_SCAN}), which makes a
 * missed card a PREMATURE RECLAMATION of a reachable young object -- read back
 * later as an all-zero header, a {@code NoSuchMethodError} on
 * {@code java.lang.Object}, or a bare SIGSEGV once the span is decommitted.
 *
 * <p>WHAT IT ASSERTS, and why every row is exact rather than timing-shaped: a
 * container is filled, aged into the old generation by allocation pressure,
 * then repeatedly overwritten with FRESH young objects and read back. Each
 * payload carries its own identity twice -- an {@code int} field and a
 * {@code String} -- so a reclaimed-and-reused slot fails as a WRONG VALUE and a
 * reclaimed-and-decommitted one fails as a fault or a zeroed header. Both are
 * deterministic failures of an invariant, not schedule sensitivity.
 * {@code System.identityHashCode} is called on every payload because that is
 * the shortest path from a stale reference to a header dereference, and the
 * site four of five crashes landed on in
 * {@code generational-young-sweep-frees-an-interpreter-held-object-20260906.md}.
 *
 * <p>Run it under {@code --XX:UseGc Generational} with
 * {@code CRATONVM_DBG_GC_STRESS} set (a GC every N bytes) so tenuring and
 * reclamation both happen inside one short run, and with
 * {@code CRATONVM_GC_VERIFY_RSET=1} to have the collector itself name any edge
 * the card table did not deliver. {@code CRATONVM_GC_FULL_RSET_SCAN=1} is the
 * A/B arm: if a failure here disappears under it, the defect is a missing card
 * and not anything about the sweep.
 */
public class OldToYoungBarrierSweep {

    /** Payload whose identity is recorded twice, so a stale read is a wrong ANSWER. */
    static final class Payload {
        final int id;
        final String tag;
        Payload next;

        Payload(int id) {
            this.id = id;
            this.tag = "p" + id;
        }
    }

    /** A tenured holder written through {@code putfield} and through a field updater. */
    static final class Holder {
        volatile Payload direct;
        volatile Payload viaUpdater;
        volatile Payload viaVarHandle;
        Payload viaReflection;
    }

    static final AtomicReferenceFieldUpdater<Holder, Payload> UPDATER =
            AtomicReferenceFieldUpdater.newUpdater(Holder.class, Payload.class, "viaUpdater");

    static final VarHandle VH;
    static final Field REFLECTED;

    static {
        try {
            VH = MethodHandles.lookup().findVarHandle(Holder.class, "viaVarHandle", Payload.class);
            REFLECTED = Holder.class.getDeclaredField("viaReflection");
            REFLECTED.setAccessible(true);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static final int THREADS = Integer.getInteger("otyb.threads", 6);
    static final int ROUNDS = Integer.getInteger("otyb.rounds", 400);
    static final int SLOTS = Integer.getInteger("otyb.slots", 512);
    /** Bytes of throwaway allocation per round, to drive tenuring and collection. */
    static final int CHURN = Integer.getInteger("otyb.churn", 256);

    static volatile Object sink;
    static final List<String> failures = Collections.synchronizedList(new ArrayList<>());

    static void check(String door, int round, int slot, Payload got, int wantId) {
        if (got == null) {
            failures.add(door + ": null at round=" + round + " slot=" + slot + " want=" + wantId);
            return;
        }
        // Reads the header first: a reclaimed-then-decommitted span faults here,
        // a reclaimed-then-reused one returns a wrong id below.
        int ih = System.identityHashCode(got);
        if (ih == 0) {
            failures.add(door + ": identityHashCode==0 at round=" + round + " slot=" + slot);
        }
        if (got.id != wantId) {
            failures.add(door + ": id " + got.id + " != " + wantId
                    + " at round=" + round + " slot=" + slot);
            return;
        }
        String want = "p" + wantId;
        if (!want.equals(got.tag)) {
            failures.add(door + ": tag " + got.tag + " != " + want
                    + " at round=" + round + " slot=" + slot);
        }
    }

    /** Allocate garbage so the collector actually runs between store and read. */
    static void churn() {
        Object last = null;
        for (int i = 0; i < CHURN; i++) {
            last = new byte[64];
        }
        sink = last;
    }

    public static void main(String[] args) throws Exception {
        // Every container is filled ONCE up front and then only overwritten, so
        // the container itself ages into old gen while the payloads stay young.
        final Object[] plainArray = new Object[SLOTS];
        final Payload[] typedArray = new Payload[SLOTS];
        final AtomicReferenceArray<Payload> atomicArray = new AtomicReferenceArray<>(SLOTS);
        final ConcurrentHashMap<Integer, Payload> chm = new ConcurrentHashMap<>();
        final Map<Integer, Payload> syncMap = Collections.synchronizedMap(new HashMap<>());
        final Map<Integer, Payload> linked = Collections.synchronizedMap(new LinkedHashMap<>());
        final Map<Integer, Payload> tree = Collections.synchronizedMap(new TreeMap<>());
        final List<Payload> list = Collections.synchronizedList(new ArrayList<>());
        final Vector<Payload> vector = new Vector<>();
        final AtomicReference<Payload> atomicRef = new AtomicReference<>();
        final Holder[] holders = new Holder[SLOTS];
        final Payload[][] bulkCopyArrays = new Payload[THREADS][SLOTS];
        final Payload[][] bulkFillArrays = new Payload[THREADS][SLOTS];

        for (int i = 0; i < SLOTS; i++) {
            Payload seed = new Payload(-1);
            plainArray[i] = seed;
            typedArray[i] = seed;
            atomicArray.set(i, seed);
            chm.put(i, seed);
            syncMap.put(i, seed);
            linked.put(i, seed);
            tree.put(i, seed);
            list.add(seed);
            vector.add(seed);
            holders[i] = new Holder();
            holders[i].direct = seed;
            holders[i].viaUpdater = seed;
            holders[i].viaVarHandle = seed;
            holders[i].viaReflection = seed;
        }
        atomicRef.set(new Payload(-1));
        for (int t = 0; t < THREADS; t++) {
            Payload seed = new Payload(-1);
            Arrays.fill(bulkCopyArrays[t], seed);
            Arrays.fill(bulkFillArrays[t], seed);
        }

        // Age the containers: allocate hard enough that the structures above are
        // promoted before the measured rounds begin.
        for (int i = 0; i < 200; i++) {
            churn();
        }

        final CountDownLatch start = new CountDownLatch(1);
        final CountDownLatch done = new CountDownLatch(THREADS);
        Thread[] threads = new Thread[THREADS];
        for (int t = 0; t < THREADS; t++) {
            final int tid = t;
            threads[t] = new Thread(() -> {
                try {
                    start.await();
                    // Each thread owns a disjoint stripe of slots, so every row is
                    // an exact single-writer invariant and nothing here depends on
                    // an interleaving.
                    for (int round = 0; round < ROUNDS; round++) {
                        for (int slot = tid; slot < SLOTS; slot += THREADS) {
                            int id = round * SLOTS + slot;
                            Payload p = new Payload(id);

                            // aastore into a tenured Object[] / Payload[]
                            plainArray[slot] = p;
                            typedArray[slot] = p;
                            // Unsafe-backed CAS array store
                            atomicArray.set(slot, p);
                            // hash-map doors
                            chm.put(slot, p);
                            syncMap.put(slot, p);
                            linked.put(slot, p);
                            tree.put(slot, p);
                            // list doors (ArrayList element store, Vector element store)
                            list.set(slot, p);
                            vector.set(slot, p);
                            // putfield, field updater CAS, VarHandle, reflection
                            Holder h = holders[slot];
                            h.direct = p;
                            UPDATER.set(h, p);
                            VH.set(h, p);
                            try {
                                REFLECTED.set(h, p);
                            } catch (IllegalAccessException e) {
                                failures.add("reflection: " + e);
                            }
                            // a young-to-young link that must survive with its parent
                            p.next = new Payload(id);

                            if (tid == 0) {
                                atomicRef.set(p);
                            }

                            churn();

                            check("Object[]", round, slot, (Payload) plainArray[slot], id);
                            check("Payload[]", round, slot, typedArray[slot], id);
                            check("AtomicReferenceArray", round, slot, atomicArray.get(slot), id);
                            check("ConcurrentHashMap", round, slot, chm.get(slot), id);
                            check("synchronizedMap(HashMap)", round, slot, syncMap.get(slot), id);
                            check("synchronizedMap(LinkedHashMap)", round, slot, linked.get(slot), id);
                            check("synchronizedMap(TreeMap)", round, slot, tree.get(slot), id);
                            check("ArrayList", round, slot, list.get(slot), id);
                            check("Vector", round, slot, vector.get(slot), id);
                            check("putfield", round, slot, h.direct, id);
                            check("AtomicReferenceFieldUpdater", round, slot, h.viaUpdater, id);
                            check("VarHandle", round, slot, (Payload) VH.get(h), id);
                            check("reflection", round, slot, h.viaReflection, id);
                            check("young->young link", round, slot, p.next, id);
                        }
                        // System.arraycopy and Arrays.fill into a tenured reference
                        // array are their own doors: each writes N slots without N
                        // putfields. Bulk arrays are per-thread, so these rows stay
                        // single-writer like the striped ones above.
                        Payload[] bulkCopy = bulkCopyArrays[tid];
                        Payload[] bulkFill = bulkFillArrays[tid];
                        Payload[] fresh = new Payload[SLOTS];
                        int base = round * 1_000_000 + tid * 100_000;
                        for (int i = 0; i < SLOTS; i++) {
                            fresh[i] = new Payload(base + i);
                        }
                        System.arraycopy(fresh, 0, bulkCopy, 0, SLOTS);
                        Arrays.fill(bulkFill, fresh[0]);
                        churn();
                        for (int i = 0; i < SLOTS; i++) {
                            check("System.arraycopy", round, i, bulkCopy[i], base + i);
                            check("Arrays.fill", round, i, bulkFill[i], base);
                        }
                    }
                } catch (Throwable e) {
                    failures.add("thread " + tid + " threw " + e);
                } finally {
                    done.countDown();
                }
            }, "otyb-" + t);
            threads[t].setDaemon(true);
            threads[t].start();
        }

        long t0 = System.nanoTime();
        start.countDown();
        done.await();
        long ms = (System.nanoTime() - t0) / 1_000_000L;

        if (failures.isEmpty()) {
            System.out.println("OTYB OK threads=" + THREADS + " rounds=" + ROUNDS
                    + " slots=" + SLOTS + " ms=" + ms);
        } else {
            System.out.println("OTYB FAIL count=" + failures.size() + " ms=" + ms);
            int n = 0;
            for (String f : failures) {
                System.out.println("  " + f);
                if (++n >= 40) {
                    System.out.println("  ... " + (failures.size() - n) + " more");
                    break;
                }
            }
        }
        System.out.println("OTYB DONE");
        if (!failures.isEmpty()) {
            System.exit(1);
        }
    }
}
