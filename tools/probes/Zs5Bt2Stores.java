// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zs s5/bt2 (2026-10-15): the compiled reference stores the s3 fused gate did
// not reach -- constant-VarHandle stores and CASes, stores into a FRESH
// receiver (constructor shape), and the same kernels in the single-pass tier --
// what one store costs, and whether a concurrent mark's snapshot stays whole.
//
// Mode `cost` (CPU, min-of-windows, as Zs3BtRefStores): kernels run in
// round-robin windows of thread CPU in ONE process; a window that saw a
// collection is dropped and the per-kernel MINIMUM is reported on stderr.
//
//   vhset   `REF.setRelease(hs[(n + k) & 1023], v)` -- a constant-VarHandle
//           reference store (the shared gate screen)
//   vhcas   `REF.compareAndSet(h, v, w)` then back -- a constant-VarHandle CAS
//   fresh   `ring[(n + k) & 1023] = new Pair(v, w)` -- two stores into a fresh
//           receiver (CRATONVM_JIT_ZGC_FRESH_RECEIVER_STORE) plus the aastore
//   put     `hs[(n + k) & 1023].ref = v` -- the plain putfield (s3 control)
//
// Mode `snap` (soundness): N nodes, each reachable from exactly ONE place at
// any time. Even rounds move a node by building a FRESH wrapper `Box(node,
// next)` in the target table and dropping the old wrapper (the fresh-receiver
// stores); odd rounds relink nodes through a constant VarHandle on
// `Node.next` (`setRelease` to the new home FIRST, then a CAS clearing the old
// edge -- the gate screen's SATB arm). Garbage keeps collections and, with
// CRATONVM_ZGC_CONC_START / CONC_EVERY_TRIGGER, concurrent marks coming. A
// store that skipped a barrier it owed loses or duplicates a node, corrupts a
// payload, or crashes the VM.
//
// stdout: one deterministic `check` line per mode (must equal HotSpot's).
// stderr: `calib:`, `window:` lines and `ns_per_store:` (cost), `moves:` (snap).
//
// Usage: java -Xmx2g Zs5Bt2Stores cost [seconds=10] [bursts_per_window=2000]
//        java -Xmx256m Zs5Bt2Stores snap [rounds=200] [nodes=20000]
// Disassembly kernels: Zs5Bt2Stores.burstVhSet, burstVhCas, burstFresh, burstPut.
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.util.Arrays;
import java.util.List;
import java.util.Locale;

public final class Zs5Bt2Stores {
    static final class Holder {
        Object ref;
    }

    static final class Pair {
        final Object a;
        final Object b;

        Pair(Object a, Object b) {
            this.a = a;
            this.b = b;
        }
    }

    static final class Node {
        final int id;
        final int[] payload;
        Node next;

        Node(int id) {
            this.id = id;
            this.payload = new int[4];
            for (int i = 0; i < 4; i++) {
                payload[i] = id * 31 + i;
            }
        }

        boolean intact() {
            for (int i = 0; i < 4; i++) {
                if (payload[i] != id * 31 + i) {
                    return false;
                }
            }
            return true;
        }
    }

    static final class Box {
        Node node;
        Box next;

        Box(Node node, Box next) {
            this.node = node;
            this.next = next;
        }
    }

    static final VarHandle REF;
    static final VarHandle NEXT;

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            REF = l.findVarHandle(Holder.class, "ref", Object.class);
            NEXT = l.findVarHandle(Node.class, "next", Node.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static final String[] KERNELS = {"vhset", "vhcas", "fresh", "put"};

    static void burstVhSet(Holder[] hs, Object v, long n) {
        for (int k = 0; k < 64; k++) {
            REF.setRelease(hs[(int) ((n + k) & 1023)], v);
        }
    }

    static int burstVhCas(Holder[] hs, Object v, Object w, long n) {
        int ok = 0;
        for (int k = 0; k < 32; k++) {
            Holder h = hs[(int) ((n + k) & 1023)];
            if (REF.compareAndSet(h, v, w)) {
                ok++;
            }
            if (REF.compareAndSet(h, w, v)) {
                ok++;
            }
        }
        return ok;
    }

    static void burstFresh(Object[] ring, Object v, Object w, long n) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = new Pair(v, w);
        }
    }

    static void burstPut(Holder[] hs, Object v, long n) {
        for (int k = 0; k < 64; k++) {
            hs[(int) ((n + k) & 1023)].ref = v;
        }
    }

    static long gcCount(List<GarbageCollectorMXBean> beans) {
        long c = 0;
        for (GarbageCollectorMXBean b : beans) {
            long x = b.getCollectionCount();
            if (x > 0) {
                c += x;
            }
        }
        return c;
    }

    static void cost(String[] args) {
        double seconds = args.length > 1 ? Double.parseDouble(args[1]) : 10.0;
        int bursts = args.length > 2 ? Integer.parseInt(args[2]) : 2000;
        ThreadMXBean tmx = ManagementFactory.getThreadMXBean();
        List<GarbageCollectorMXBean> gcs = ManagementFactory.getGarbageCollectorMXBeans();
        Object[] ring = new Object[1024];
        Holder[] hs = new Holder[1024];
        Holder[] cs = new Holder[1024];
        Object v = new Object();
        Object w = new Object();
        for (int i = 0; i < hs.length; i++) {
            hs[i] = new Holder();
            cs[i] = new Holder();
            cs[i].ref = v;
        }
        for (int i = 0; i < 4; i++) {
            System.gc();
        }
        long casOk = 0;
        for (int r = 0; r < 20000; r++) {
            burstVhSet(hs, v, r);
            casOk += burstVhCas(cs, v, w, r);
            burstFresh(ring, v, w, r);
            burstPut(hs, v, r);
        }
        long step = Long.MAX_VALUE;
        for (int i = 0; i < 5; i++) {
            long c0 = tmx.getCurrentThreadCpuTime();
            long c1;
            while ((c1 = tmx.getCurrentThreadCpuTime()) == c0) {
                burstPut(hs, v, c0);
            }
            step = Math.min(step, c1 - c0);
        }
        long target = Math.max(2_000_000L, 50 * step);
        while (true) {
            long t0 = tmx.getCurrentThreadCpuTime();
            for (int b = 0; b < bursts; b++) {
                burstPut(hs, v, b);
            }
            long dt = tmx.getCurrentThreadCpuTime() - t0;
            if (dt >= target || bursts > (1 << 26)) {
                break;
            }
            bursts *= 2;
        }
        System.err.println("calib: step_ns=" + step + " bursts=" + bursts);
        int nk = KERNELS.length;
        long[][] samples = new long[nk][];
        int[] kept = new int[nk];
        int[] dropped = new int[nk];
        int cap = 1 << 16;
        for (int i = 0; i < nk; i++) {
            samples[i] = new long[cap];
        }
        long deadline = tmx.getCurrentThreadCpuTime() + (long) (seconds * 1e9);
        long n = 0;
        while (tmx.getCurrentThreadCpuTime() < deadline) {
            for (int s = 0; s < nk; s++) {
                long g0 = gcCount(gcs);
                long t0 = tmx.getCurrentThreadCpuTime();
                for (int b = 0; b < bursts; b++) {
                    switch (s) {
                        case 0 -> burstVhSet(hs, v, n);
                        case 1 -> casOk += burstVhCas(cs, v, w, n);
                        case 2 -> burstFresh(ring, v, w, n);
                        default -> burstPut(hs, v, n);
                    }
                    n += 64;
                }
                long t1 = tmx.getCurrentThreadCpuTime();
                if (gcCount(gcs) != g0) {
                    dropped[s]++;
                } else if (kept[s] < cap) {
                    samples[s][kept[s]++] = t1 - t0;
                }
            }
        }
        double perStore = 64.0 * bursts;
        StringBuilder line = new StringBuilder("ns_per_store:");
        for (int s = 0; s < nk; s++) {
            long[] ws = Arrays.copyOf(samples[s], kept[s]);
            Arrays.sort(ws);
            double min = ws.length == 0 ? -1 : ws[0] / perStore;
            double med = ws.length == 0 ? -1 : ws[ws.length / 2] / perStore;
            System.err.printf(Locale.ROOT, "window: %s min=%.3f med=%.3f kept=%d dropped_gc=%d%n",
                    KERNELS[s], min, med, kept[s], dropped[s]);
            line.append(String.format(Locale.ROOT, " %s=%.3f", KERNELS[s], min));
        }
        System.err.println(line);
        System.err.println("cas_ok: " + casOk);
        // Deterministic: what the kernels left behind.
        int heldV = 0;
        for (Holder h : hs) {
            if (REF.getAcquire(h) == v) {
                heldV++;
            }
        }
        int casV = 0;
        for (Holder h : cs) {
            if (h.ref == v) {
                casV++;
            }
        }
        int pairs = 0;
        for (Object o : ring) {
            if (o instanceof Pair p && p.a == v && p.b == w) {
                pairs++;
            }
        }
        System.out.println("check cost holders_v=" + heldV + " cas_v=" + casV + " pairs=" + pairs
                + " ok=" + (heldV == 1024 && casV == 1024 && pairs == 1024));
    }

    // ---- snap -------------------------------------------------------------

    static Object sink;

    // Even rounds: move the node held by box a[i] into a FRESH box at the head
    // of b[j], then drop the old box (a[i] = old.next). The new box's two
    // stores are fresh-receiver stores; the old box leaves through the
    // overwrite of a[i], whose pre-barrier keeps the node in the snapshot.
    static void moveBox(Object[] a, Object[] b, int i, int j) {
        Box old = (Box) a[i];
        if (old == null) {
            return;
        }
        b[j] = new Box(old.node, (Box) b[j]);
        a[i] = old.next;
    }

    // Odd rounds: inside the chain at c[i], move its second node behind the
    // tail of c[j]'s chain through the constant VarHandle: the new edge FIRST
    // (setRelease), then the old one cleared with a CAS.
    static void moveNext(Object[] c, int i, int j) {
        Box bi = (Box) c[i];
        Box bj = (Box) c[j];
        if (bi == null || bj == null || bi == bj) {
            return;
        }
        Node head = bi.node;
        Node x = (Node) NEXT.getAcquire(head);
        if (x == null) {
            return;
        }
        Node tail = bj.node;
        while (tail.next != null) {
            tail = tail.next;
        }
        if (tail == x || tail == head) {
            return;
        }
        Node rest = x.next;
        NEXT.setRelease(tail, x);
        if (!NEXT.compareAndSet(head, x, rest)) {
            throw new IllegalStateException("lost a CAS on an unshared node");
        }
        NEXT.setRelease(x, (Node) null);
    }

    static void snap(String[] args) {
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 200;
        int nodes = args.length > 2 ? Integer.parseInt(args[2]) : 20000;
        int width = Math.max(16, nodes / 4);
        Object[] a = new Object[width];
        Object[] b = new Object[width];
        for (int id = 0; id < nodes; id++) {
            int slot = id % width;
            a[slot] = new Box(new Node(id), (Box) a[slot]);
        }
        long seed = 0x5DEECE66DL;
        long moves = 0;
        for (int r = 0; r < rounds; r++) {
            for (int k = 0; k < width; k++) {
                seed = seed * 6364136223846793005L + 1442695040888963407L;
                int i = (int) ((seed >>> 33) % width);
                int j = (int) ((seed >>> 13) % width);
                switch (r & 3) {
                    case 0 -> moveBox(a, b, i, j);
                    case 1 -> moveNext(b, j, i);
                    case 2 -> moveBox(b, a, j, i);
                    default -> moveNext(a, i, j);
                }
                moves++;
                if ((k & 7) == 0) {
                    sink = new long[64];
                }
            }
        }
        boolean[] seen = new boolean[nodes];
        int found = 0;
        int dup = 0;
        int bad = 0;
        for (Object[] t : new Object[][] {a, b}) {
            for (Object o : t) {
                for (Box bx = (Box) o; bx != null; bx = bx.next) {
                    for (Node nd = bx.node; nd != null; nd = nd.next) {
                        if (nd.id < 0 || nd.id >= nodes || !nd.intact()) {
                            bad++;
                            continue;
                        }
                        if (seen[nd.id]) {
                            dup++;
                        } else {
                            seen[nd.id] = true;
                            found++;
                        }
                    }
                }
            }
        }
        System.err.println("moves: " + moves);
        System.out.println("check snap nodes=" + nodes + " found=" + found + " dup=" + dup + " bad=" + bad
                + " ok=" + (found == nodes && dup == 0 && bad == 0));
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "cost";
        switch (mode) {
            case "cost" -> cost(args);
            case "snap" -> snap(args);
            default -> throw new IllegalArgumentException("mode: cost | snap");
        }
    }
}
