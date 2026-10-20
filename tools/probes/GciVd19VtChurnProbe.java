// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.LockSupport;

/**
 * gci i19/vd: virtual-thread park / yield churn that checks VALUES across every
 * unmount (the precise G1 continuation deposit, CRATONVM_G1_VT_PRECISE_DEPOSIT).
 *
 *   GciVd19VtChurnProbe [THREADS [ITERS [DEPTH]]]   (defaults 300 40 6)
 *
 * Each of THREADS virtual threads runs ITERS iterations. An iteration recurses
 * DEPTH frames; every frame holds a LIVE local `Node` (checked after the
 * unmount below it) and a liveness-DEAD one (`scoped`, last read before the
 * call). At the bottom the thread stores a fresh young chain into its own
 * long-lived `Holder` (an old -> young store from interpreted code), then calls
 * `combine(local, pause(id, it), ...)`: `local` sits on the bottom frame's
 * OPERAND STACK while `pause` unmounts (Thread.yield or a timed park, by
 * parity) and allocates garbage. After the unmount every frame re-checks its
 * local, the caller's local it carries, the holder's young chain and the
 * thread's first (old) node. Each Node carries v, v*3, v*7 in a long[] so a
 * stale or foreign copy is caught.
 *
 * stdout is a pure function of the arguments (the checksum is a sum of
 * per-thread values that do not depend on scheduling):
 *   vd19 churn threads=300 iters=40 depth=6 mismatches=0 checksum=<HotSpot's>
 * Diagnostics on stderr ("[vd19] ..."), at most 20 mismatch lines.
 */
public class GciVd19VtChurnProbe {
    static final AtomicInteger MISMATCH = new AtomicInteger();
    static final AtomicLong CHECKSUM = new AtomicLong();
    static volatile Object sink;

    static final class Node {
        final int v;
        final long[] pad;
        final Node next;

        Node(int v, Node next) {
            this.v = v;
            this.next = next;
            this.pad = new long[] { v, v * 3L, v * 7L };
        }
    }

    static final class Holder {
        final int id;
        Node young;

        Holder(int id) {
            this.id = id;
        }
    }

    static void garbage(int k) {
        for (int i = 0; i < k; i++) {
            sink = new byte[64 + (i * 37 % 512)];
        }
    }

    static boolean good(Node n, int v) {
        return n != null && n.v == v && n.pad != null && n.pad.length == 3 && n.pad[0] == v
                && n.pad[1] == v * 3L && n.pad[2] == v * 7L;
    }

    static void bad(int id, int it, String what) {
        if (MISMATCH.incrementAndGet() <= 20) {
            System.err.println("[vd19] mismatch id=" + id + " it=" + it + " " + what);
        }
    }

    static int base(int id, int it, int depth) {
        return id * 10000 + it * 100 + depth;
    }

    // The unmount. Its result lands on the caller's operand stack above `local`.
    static int pause(int id, int it) {
        if (((id + it) & 1) == 0) {
            Thread.yield();
        } else {
            LockSupport.parkNanos(50_000L);
        }
        garbage(48);
        return it;
    }

    static long combine(Node head, int it, int id, int depth) {
        if (!good(head, base(id, it, depth))) {
            bad(id, it, "operand-stack head");
            return 0;
        }
        return head.pad[2] + it;
    }

    static long deep(Holder h, int depth, Node carry, int id, int it) {
        int b = base(id, it, depth);
        Node local = new Node(b, carry);
        Node scoped = new Node(-b, null);
        long s = scoped.pad[1]; // the last read of `scoped`
        if (depth > 0) {
            s += deep(h, depth - 1, local, id, it);
        } else {
            h.young = new Node(b + 7, local);
            s += combine(local, pause(id, it), id, depth);
            Node y = h.young;
            if (!good(y, b + 7) || y.next != local) {
                bad(id, it, "holder.young");
            } else {
                s += y.pad[1];
            }
        }
        if (!good(local, b)) {
            bad(id, it, "local depth=" + depth);
        } else if (carry != null && (local.next != carry || !good(carry, b + 1))) {
            bad(id, it, "carry depth=" + depth);
        } else {
            s += local.v + (carry == null ? 0L : carry.pad[2]);
        }
        return s;
    }

    static void run(int id, int iters, int depth) {
        Holder h = new Holder(id);
        Node old = new Node(id, null);
        long s = 0;
        for (int it = 0; it < iters; it++) {
            s += deep(h, depth, null, id, it);
            if (!good(old, id)) {
                bad(id, it, "old");
            }
        }
        CHECKSUM.addAndGet(s + old.pad[1] + h.id);
    }

    static void body(int id, int iters, int depth) {
        try {
            run(id, iters, depth);
        } catch (Throwable t) {
            bad(id, -1, "threw " + t.getClass().getName());
        }
    }

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 300;
        int iters = args.length > 1 ? Integer.parseInt(args[1]) : 40;
        int depth = args.length > 2 ? Integer.parseInt(args[2]) : 6;
        long t0 = System.nanoTime();
        List<Thread> ts = new ArrayList<>(n);
        for (int i = 0; i < n; i++) {
            final int id = i;
            ts.add(Thread.ofVirtual().name("vd19-" + i).start(() -> body(id, iters, depth)));
        }
        for (Thread t : ts) {
            t.join();
        }
        System.out.println("vd19 churn threads=" + n + " iters=" + iters + " depth=" + depth
                + " mismatches=" + MISMATCH.get() + " checksum=" + CHECKSUM.get());
        System.err.println("[vd19] ms=" + ((System.nanoTime() - t0) / 1_000_000));
    }
}
