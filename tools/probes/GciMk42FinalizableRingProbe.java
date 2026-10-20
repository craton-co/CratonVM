// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.Reference;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * gci i42/mk (2026-10-16): a finalizable object that stays strongly reachable
 * must never be finalized, and a weak reference to one must never be cleared.
 * Page:
 * {@code docs/known-issues/gc/gci-i42mk-gen-remark-judges-death-before-thread-satb-buckets-20261016.md}.
 *
 * <p>Each worker owns an OLD ring of FINALIZABLE {@link Node}s, reachable only
 * through the ring (the worker keeps the head). Every round it churns young
 * garbage (young pauses, promotions, concurrent old cycles under
 * {@code CRATONVM_GC_CONC_START_PERCENT}) and re-links nodes further along the
 * ring: the overwritten {@code next} is the only path to the moved node at the
 * snapshot, so when the slot it is re-linked into was already scanned only the
 * SATB log keeps it. On the Generational concurrent remark the log entry may
 * still sit in the worker's own SATB bucket if the worker was FROZEN by the
 * take-over rather than arriving at the pause; before the fix the remark judged
 * the node dead at its first fixed point and handed it to {@code finalize()}.
 *
 * <p>Every node is reachable from start to the final print, so on HotSpot (and
 * on any correct collector) {@code finalize()} never runs before it and every
 * sampled weak reference still names its node. A premature finalization shows
 * up as {@code finalized-while-reachable} above 0, a premature weak clear as
 * {@code weak-ok=false}. Everything on stdout is a function of the arguments.
 *
 * <p>stdout (HotSpot {@code -XX:+UseSerialGC} prints the same):
 * <pre>
 *   mk42 threads=T rounds=R ring=N nodes=T*N idsum=S finalized-while-reachable=0 weak-ok=true fail=none
 * </pre>
 * stderr (not compared): {@code [gci-mk42] ms=<elapsed>}.
 *
 * <pre>
 *   javac -d &lt;classes&gt; tools/probes/GciMk42FinalizableRingProbe.java
 *   java -XX:+UseSerialGC -Xmx64m -cp &lt;classes&gt; GciMk42FinalizableRingProbe 4 80 20000
 * </pre>
 * Usage: {@code GciMk42FinalizableRingProbe [threads] [rounds] [ring]} (defaults 4, 80, 20000).
 */
public final class GciMk42FinalizableRingProbe {

    static final int CHURN_PER_ROUND = 20000;
    static final int WEAK_EVERY = 997;

    /** finalize() calls observed before the final print; every node is reachable then. */
    static final AtomicInteger FINALIZED = new AtomicInteger();

    static final class Node {
        Node next;
        final int id;

        Node(int id) {
            this.id = id;
        }

        @SuppressWarnings("removal")
        @Override
        protected void finalize() {
            FINALIZED.incrementAndGet();
        }
    }

    static final class Worker implements Runnable {
        final int ring;
        final int rounds;
        int rnd;
        Node head;
        final List<WeakReference<Node>> weaks = new ArrayList<>();
        long idSum;
        long nodes;
        boolean weakOk = true;
        String failure;
        Object sink;

        Worker(int seed, int ring, int rounds) {
            this.rnd = seed;
            this.ring = ring;
            this.rounds = rounds;
        }

        int nextInt(int bound) {
            rnd = rnd * 1103515245 + 12345;
            return (rnd >>> 1) % bound;
        }

        @Override
        public void run() {
            try {
                build();
                for (int r = 0; r < rounds; r++) {
                    churn(r);
                    rewire();
                    checkWeaks();
                }
                sink = null;
                verify();
            } catch (Throwable t) {
                failure = t.getClass().getName();
            }
        }

        void build() {
            Node first = new Node(0);
            Node prev = first;
            for (int i = 1; i < ring; i++) {
                Node n = new Node(i);
                prev.next = n;
                prev = n;
            }
            prev.next = first;
            head = first;
            Node c = head;
            for (int i = 0; i < ring; i++) {
                if (i % WEAK_EVERY == 0) {
                    weaks.add(new WeakReference<>(c));
                }
                c = c.next;
            }
        }

        void churn(int r) {
            Object[] tmp = new Object[4096];
            for (int k = 0; k < CHURN_PER_ROUND; k++) {
                tmp[k & 4095] = new byte[64 + ((k + r) & 127)];
            }
            sink = tmp;
        }

        void rewire() {
            Node cur = head;
            int ops = Math.max(1, ring / 8);
            for (int k = 0; k < ops; k++) {
                int skip = 1 + nextInt(16);
                for (int s = 0; s < skip; s++) {
                    cur = cur.next;
                }
                Node x = cur.next;
                int dist = 1 + nextInt(64);
                if (x == head) {
                    continue;
                }
                // The only reference to x is overwritten here (SATB logs x).
                cur.next = x.next;
                x.next = null;
                Node at = cur;
                for (int s = 0; s < dist; s++) {
                    at = at.next;
                }
                x.next = at.next;
                at.next = x;
            }
        }

        void checkWeaks() {
            for (int i = 0; i < weaks.size(); i++) {
                if (weaks.get(i).get() == null) {
                    weakOk = false;
                }
            }
        }

        void verify() {
            Node c = head;
            long count = 0;
            long sum = 0;
            do {
                count++;
                sum += c.id;
                c = c.next;
            } while (c != head && count <= ring);
            nodes = count;
            idSum = sum;
            checkWeaks();
        }
    }

    public static void main(String[] args) throws Exception {
        int threads = args.length > 0 ? Integer.parseInt(args[0]) : 4;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 80;
        int ring = args.length > 2 ? Integer.parseInt(args[2]) : 20000;
        long t0 = System.nanoTime();
        Worker[] workers = new Worker[threads];
        Thread[] ts = new Thread[threads];
        for (int i = 0; i < threads; i++) {
            workers[i] = new Worker(0x5eed + i * 7919, ring, rounds);
            ts[i] = new Thread(workers[i], "mk42-worker-" + i);
        }
        for (Thread t : ts) {
            t.start();
        }
        for (Thread t : ts) {
            t.join();
        }
        long nodes = 0;
        long idSum = 0;
        boolean weakOk = true;
        String fail = "none";
        for (Worker w : workers) {
            nodes += w.nodes;
            idSum += w.idSum;
            weakOk &= w.weakOk;
            if (w.failure != null && "none".equals(fail)) {
                fail = w.failure;
            }
        }
        // Read BEFORE the print; every node is still reachable through `workers`.
        int finalized = FINALIZED.get();
        System.out.println("mk42 threads=" + threads + " rounds=" + rounds + " ring=" + ring
                + " nodes=" + nodes + " idsum=" + idSum
                + " finalized-while-reachable=" + finalized
                + " weak-ok=" + weakOk + " fail=" + fail);
        System.err.println("[gci-mk42] ms=" + (System.nanoTime() - t0) / 1_000_000);
        Reference.reachabilityFence(workers);
    }
}
