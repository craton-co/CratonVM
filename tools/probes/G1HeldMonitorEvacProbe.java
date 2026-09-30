// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Threads that hold monitors across G1 young pauses whose parallel evacuation
// runs out of to-space, with a shared (diamond-shaped) survivor graph so two
// evacuation workers race to copy the same object.
//
// Regression probe for
// docs/internal/gc/g1-parallel-evac-pool-exhaustion-loses-a-held-monitor-FIXED-20260930.md:
// the symptom was a thread no longer owning a monitor it held
// (IllegalMonitorStateException at the matching monitorexit, which javac's
// catch-any handler turns into a spin) after such a pause. The defect closed
// with it was a peer's forwarding claim abandoned after a bounded wait when
// its winner was descheduled; `G1 evacuation claim waits` at exit (under
// CRATONVM_DBG_JIT_METHOD_STATS=1) says how often a run reached that window.
//
//   cratonvm -XX:+UseG1GC -Xmx46m -cp tools/probes G1HeldMonitorEvacProbe 60
//
// The heap size matters: too roomy and no pause exhausts its pool, too tight
// and the run thrashes. 46m reached the window on the 2026-09-30 host.
//
// Run several copies at once: the window only opens when evacuation workers
// are descheduled. Prints one `G1HeldMonitorEvacProbe ok ...` line, or FAIL
// lines and a non-zero exit. A hang is also a failure (use a timeout).
public class G1HeldMonitorEvacProbe {
    static final int THREADS = 4;
    static final Object[] LOCKS = new Object[8];
    // Survivors shared by every thread: each slot is referenced from several
    // holders, so a young pause meets the same object from many parents.
    static final Object[][] SHARED = new Object[64][];
    static volatile int failures;

    static final class Node {
        final long id;
        Node next;
        Object shared;
        final long check;

        Node(long id, Node next, Object shared) {
            this.id = id;
            this.next = next;
            this.shared = shared;
            this.check = id * 31 + 7;
        }
    }

    static void fail(String what) {
        failures++;
        System.out.println("FAIL " + Thread.currentThread().getName() + " " + what);
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 400;
        for (int i = 0; i < LOCKS.length; i++) {
            LOCKS[i] = new Object();
        }
        for (int i = 0; i < SHARED.length; i++) {
            SHARED[i] = new Object[] {new long[4], "s" + i};
        }
        Thread[] ts = new Thread[THREADS];
        for (int t = 0; t < THREADS; t++) {
            final int tid = t;
            ts[t] = new Thread(() -> work(tid, rounds), "w" + t);
            ts[t].start();
        }
        for (Thread t : ts) {
            // The heap is kept tight on purpose; an OutOfMemoryError on this
            // thread is not what the probe measures.
            for (;;) {
                try {
                    t.join();
                    break;
                } catch (OutOfMemoryError e) {
                    // retry the join
                }
            }
        }
        if (failures != 0) {
            System.out.println("G1HeldMonitorEvacProbe FAIL failures=" + failures);
            System.exit(1);
        }
        System.out.println("G1HeldMonitorEvacProbe ok threads=" + THREADS + " rounds=" + rounds);
    }

    static void work(int tid, int rounds) {
        // A ring of live rounds, sized so the heap stays close to full and
        // young pauses exhaust their to-space pool. Each round is a chain AND a
        // wide array of the same nodes, and every node also references one of
        // sixteen FRESH (young) objects of its round: many parents per child,
        // met by different evacuation workers in the same pause, is what makes
        // two workers race to copy one object.
        Object[][] ring = new Object[24][];
        long id = tid * 1_000_000_000L;
        for (int r = 0; r < rounds; r++) {
            Object lock = LOCKS[(tid + r) % LOCKS.length];
            try {
                synchronized (lock) {
                    Node head = null;
                    long[][] fresh = new long[16][];
                    for (int k = 0; k < fresh.length; k++) {
                        long v = ((long) r << 8) | k;
                        fresh[k] = new long[] {v, ~v};
                    }
                    Object[] wide = new Object[2001];
                    for (int i = 0; i < 2000; i++) {
                        head = new Node(id++, head, (i & 1) == 0 ? fresh[i & 15] : SHARED[(int) (id % SHARED.length)]);
                        wide[i] = head;
                        // Short-lived garbage between the survivors.
                        byte[] junk = new byte[48 + (i & 63)];
                        junk[0] = (byte) i;
                        if (!Thread.holdsLock(lock)) {
                            fail("lost ownership of lock " + ((tid + r) % LOCKS.length) + " round " + r);
                            return;
                        }
                    }
                    wide[2000] = head;
                    ring[r % ring.length] = wide;
                    // Re-share a survivor, so later pauses meet it from two parents.
                    SHARED[(int) (id % SHARED.length)] = new Object[] {head, new long[2]};
                }
            } catch (IllegalMonitorStateException e) {
                fail("IMSE round " + r + ": " + e.getMessage());
                return;
            } catch (OutOfMemoryError e) {
                // Tolerated: drop the ring and continue; the point is the pauses.
                java.util.Arrays.fill(ring, null);
            }
            if (r % 16 == 0) {
                for (Object[] wide : ring) {
                    if (wide == null) {
                        continue;
                    }
                    int len = 0;
                    for (Node p = (Node) wide[2000]; p != null; p = p.next) {
                        if (p.check != p.id * 31 + 7 || p.shared == null) {
                            fail("corrupt node id=" + p.id + " check=" + p.check);
                            return;
                        }
                        if (p.shared instanceof long[] f && f[1] != ~f[0]) {
                            fail("corrupt shared child of node id=" + p.id);
                            return;
                        }
                        if (wide[1999 - len] != p) {
                            fail("chain and array disagree at " + len);
                            return;
                        }
                        len++;
                    }
                    if (len != 2000) {
                        fail("chain length " + len);
                        return;
                    }
                }
            }
        }
    }
}
