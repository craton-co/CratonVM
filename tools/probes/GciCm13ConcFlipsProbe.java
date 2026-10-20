// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/**
 * gci i13/cm (2026-10-13): the Generational concurrent old cycle under the
 * user flips (pinned young copy + ledger, OLDGEN_COMPACT and
 * REQUESTED_MAJOR_COMPACTS, the age clock, precise-root promote, P-C true
 * roots). Page:
 * {@code docs/known-issues/gc/gci-i13cm-generational-concurrent-cycle-under-the-user-flips-review-findings-20261013.md}.
 *
 * <p>Each worker owns an OLD ring: a singly linked circular list of
 * {@link Node}s reachable ONLY through the ring itself (the worker keeps the
 * head). Every round it
 * <ul>
 *   <li>churns young garbage, so young pauses (promoting into spans a
 *       concurrent sweep has freed) run while a concurrent cycle is open;</li>
 *   <li>unlinks nodes and re-links them further along the ring: the
 *       overwritten {@code next} is the only path to the moved node at the
 *       snapshot, so only the SATB log keeps it if the slot it is re-linked
 *       into was already scanned;</li>
 *   <li>replaces every other moved node's payload with a fresh young array
 *       (old-to-young edges; promotions mid-cycle);</li>
 *   <li>optionally calls {@code System.gc()} every {@code sysgcEvery}
 *       rounds: a REQUESTED major, compacting by the user flip, through an
 *       open concurrent cycle, which must then be abandoned rather than swept
 *       against the moved layout.</li>
 * </ul>
 * At the end every ring is walked (count, id sum, version sum, payload
 * contents) and weak references to strongly reachable nodes must still name
 * them. A lost or overwritten object shows up as a wrong number, a payload
 * mismatch, a {@code FAIL=} token or a crash. Everything printed on stdout is
 * a function of the arguments only.
 *
 * <p>stdout (HotSpot {@code -XX:+UseSerialGC} prints the same):
 * <pre>
 *   cm13 mode=ring threads=T rounds=R ring=N nodes=T*N idsum=S versions=V payload-ok=true weak-ok=true
 * </pre>
 * stderr (not compared): {@code [gci-cm13] ms=<elapsed> sysgc=<count>}.
 *
 * <pre>
 *   javac -d &lt;classes&gt; tools/probes/GciCm13ConcFlipsProbe.java
 *   java -XX:+UseSerialGC -Xmx64m -cp &lt;classes&gt; GciCm13ConcFlipsProbe ring 4 80 20000 0
 * </pre>
 * Usage: {@code GciCm13ConcFlipsProbe ring [threads] [rounds] [ring] [sysgcEvery]}
 * (defaults 1, 120, 40000, 0).
 */
public final class GciCm13ConcFlipsProbe {

    static final int CHURN_PER_ROUND = 20000;

    static final class Node {
        Node next;
        byte[] payload;
        final int id;
        int ver;

        Node(int id) {
            this.id = id;
        }
    }

    static byte fill(int id, int ver) {
        return (byte) (id * 31 + ver * 7);
    }

    static int payloadLength(int id) {
        return 16 + (id % 48);
    }

    static byte[] payloadFor(int id, int ver) {
        byte[] b = new byte[payloadLength(id)];
        Arrays.fill(b, fill(id, ver));
        return b;
    }

    static final class Worker implements Runnable {
        final int ring;
        final int rounds;
        final int sysgcEvery;
        int rnd;
        Node head;
        final List<WeakReference<Node>> weaks = new ArrayList<>();
        final List<Integer> weakIds = new ArrayList<>();
        long idSum;
        long versions;
        long nodes;
        boolean payloadOk = true;
        boolean weakOk = true;
        int sysgc;
        String failure;
        Object sink;

        Worker(int seed, int ring, int rounds, int sysgcEvery) {
            this.rnd = seed;
            this.ring = ring;
            this.rounds = rounds;
            this.sysgcEvery = sysgcEvery;
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
                    if (sysgcEvery > 0 && (r + 1) % sysgcEvery == 0) {
                        System.gc();
                        sysgc++;
                    }
                }
                sink = null;
                verify();
            } catch (Throwable t) {
                failure = t.getClass().getName();
            }
        }

        void build() {
            Node first = new Node(0);
            first.payload = payloadFor(0, 0);
            Node prev = first;
            for (int i = 1; i < ring; i++) {
                Node n = new Node(i);
                n.payload = payloadFor(i, 0);
                prev.next = n;
                prev = n;
            }
            prev.next = first;
            head = first;
            Node c = head;
            for (int i = 0; i < ring; i++) {
                if (i % 997 == 0) {
                    weaks.add(new WeakReference<>(c));
                    weakIds.add(c.id);
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
                // The only reference to x is overwritten here.
                cur.next = x.next;
                x.next = null;
                Node at = cur;
                for (int s = 0; s < dist; s++) {
                    at = at.next;
                }
                x.next = at.next;
                at.next = x;
                if ((k & 1) == 0) {
                    x.ver++;
                    x.payload = payloadFor(x.id, x.ver);
                }
            }
        }

        void verify() {
            Node c = head;
            long count = 0;
            long sum = 0;
            long vsum = 0;
            do {
                count++;
                sum += c.id;
                vsum += c.ver;
                byte[] p = c.payload;
                if (p == null || p.length != payloadLength(c.id)) {
                    payloadOk = false;
                } else {
                    byte f = fill(c.id, c.ver);
                    for (byte v : p) {
                        if (v != f) {
                            payloadOk = false;
                            break;
                        }
                    }
                }
                c = c.next;
                if (count > (long) ring + 1) {
                    failure = "ring-overrun";
                    break;
                }
            } while (c != head);
            nodes = count;
            idSum = sum;
            versions = vsum;
            for (int i = 0; i < weaks.size(); i++) {
                Node n = weaks.get(i).get();
                if (n == null || n.id != weakIds.get(i).intValue()) {
                    weakOk = false;
                }
            }
        }
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "ring";
        int threads = args.length > 1 ? Integer.parseInt(args[1]) : 1;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 120;
        int ring = args.length > 3 ? Integer.parseInt(args[3]) : 40000;
        int sysgcEvery = args.length > 4 ? Integer.parseInt(args[4]) : 0;
        if (!mode.equals("ring") || threads < 1 || ring < 128 || rounds < 1) {
            System.out.println("cm13 bad-args mode=" + mode);
            return;
        }
        long t0 = System.nanoTime();
        Worker[] ws = new Worker[threads];
        Thread[] ts = new Thread[threads];
        for (int i = 0; i < threads; i++) {
            ws[i] = new Worker(i + 1, ring, rounds, sysgcEvery);
            ts[i] = new Thread(ws[i], "cm13-" + i);
        }
        for (Thread t : ts) {
            t.start();
        }
        for (Thread t : ts) {
            t.join();
        }
        long nodes = 0;
        long idSum = 0;
        long versions = 0;
        boolean payloadOk = true;
        boolean weakOk = true;
        int sysgc = 0;
        String failure = null;
        for (Worker w : ws) {
            nodes += w.nodes;
            idSum += w.idSum;
            versions += w.versions;
            payloadOk &= w.payloadOk;
            weakOk &= w.weakOk;
            sysgc += w.sysgc;
            if (w.failure != null && failure == null) {
                failure = w.failure;
            }
        }
        StringBuilder line = new StringBuilder();
        line.append("cm13 mode=").append(mode)
            .append(" threads=").append(threads)
            .append(" rounds=").append(rounds)
            .append(" ring=").append(ring)
            .append(" nodes=").append(nodes)
            .append(" idsum=").append(idSum)
            .append(" versions=").append(versions)
            .append(" payload-ok=").append(payloadOk)
            .append(" weak-ok=").append(weakOk);
        if (failure != null) {
            line.append(" FAIL=").append(failure);
        }
        System.out.println(line);
        System.err.println("[gci-cm13] ms=" + (System.nanoTime() - t0) / 1_000_000L + " sysgc=" + sysgc);
    }
}
