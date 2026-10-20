// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zs s3/bt (2026-10-14): the compiled reference-store barrier on ZGC --
// what one `putfield` / `aastore` of a reference costs, and whether the
// barrier still keeps a concurrent mark's snapshot whole.
//
// Mode `cost` (CPU, min-of-windows, as Zs2Ca2AllocWindows): three kernels run
// in round-robin windows of thread CPU in ONE process; a window that saw a
// collection is dropped and the per-kernel MINIMUM is reported on stderr.
//
//   aa      `ring[(n + k) & 1023] = v` -- a reference aastore into an Object[]
//   put     `hs[(n + k) & 1023].ref = v` -- a reference putfield
//   aanull  `ring[(n + k) & 1023] = null` -- a null aastore
//
// Mode `snap` (soundness): N nodes, each reachable from exactly ONE slot of
// two Object[] tables or one `Node.next` field at any time. The mutator
// permutes them -- every move is "store the reference somewhere new, then
// overwrite its old slot" -- while it allocates garbage to keep collections
// (and, with CRATONVM_ZGC_CONC_START / CONC_EVERY_TRIGGER, concurrent marks)
// coming. A SATB pre-barrier that is skipped while a mark runs loses a node
// whose only edge was overwritten after the marker scanned its new slot; the
// final census then finds a node missing, duplicated or with a corrupted
// payload, or the VM crashes.
//
// stdout: one deterministic `check` line per mode (must equal HotSpot's).
// stderr: `window:` lines and `ns_per_store:` (cost), `moves:` (snap).
//
// Usage: java -Xmx2g Zs3BtRefStores cost [seconds=10] [bursts_per_window=2000]
//        java -Xmx256m Zs3BtRefStores snap [rounds=200] [nodes=20000]
// Disassembly kernels: Zs3BtRefStores.burstAa, burstPut, burstAaNull, move.
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.util.Arrays;
import java.util.List;
import java.util.Locale;

public final class Zs3BtRefStores {
    static final class Holder {
        Object ref;
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

    static final String[] KERNELS = {"aa", "put", "aanull"};

    static void burstAa(Object[] ring, Object v, long n) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = v;
        }
    }

    static void burstPut(Holder[] hs, Object v, long n) {
        for (int k = 0; k < 64; k++) {
            hs[(int) ((n + k) & 1023)].ref = v;
        }
    }

    static void burstAaNull(Object[] ring, long n) {
        for (int k = 0; k < 64; k++) {
            ring[(int) ((n + k) & 1023)] = null;
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
        for (int i = 0; i < hs.length; i++) {
            hs[i] = new Holder();
        }
        Object v = new Object();
        // Make the tables old on a generational run: a few collections.
        for (int i = 0; i < 4; i++) {
            System.gc();
        }
        // Warm every kernel past tier-up.
        for (int r = 0; r < 20000; r++) {
            burstAa(ring, v, r);
            burstPut(hs, v, r);
            burstAaNull(ring, r);
        }
        // Calibrate: a window must span >= 2 ms and >= 50 clock steps (the
        // Windows thread-CPU clock steps by 15.6 ms; Linux by about 1 us).
        long step = Long.MAX_VALUE;
        for (int i = 0; i < 5; i++) {
            long c0 = tmx.getCurrentThreadCpuTime();
            long c1;
            while ((c1 = tmx.getCurrentThreadCpuTime()) == c0) {
                burstAa(ring, v, c0);
            }
            step = Math.min(step, c1 - c0);
        }
        long target = Math.max(2_000_000L, 50 * step);
        while (true) {
            long t0 = tmx.getCurrentThreadCpuTime();
            for (int b = 0; b < bursts; b++) {
                burstAa(ring, v, b);
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
                        case 0 -> burstAa(ring, v, n);
                        case 1 -> burstPut(hs, v, n);
                        default -> burstAaNull(ring, n);
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
            long[] w = Arrays.copyOf(samples[s], kept[s]);
            Arrays.sort(w);
            double min = w.length == 0 ? -1 : w[0] / perStore;
            double med = w.length == 0 ? -1 : w[w.length / 2] / perStore;
            System.err.printf(Locale.ROOT, "window: %s min=%.3f med=%.3f kept=%d dropped_gc=%d%n",
                    KERNELS[s], min, med, kept[s], dropped[s]);
            line.append(String.format(Locale.ROOT, " %s=%.3f", KERNELS[s], min));
        }
        System.err.println(line);
        // Deterministic: what the last bursts left behind.
        int nonNullRing = 0;
        for (Object o : ring) {
            if (o != null) {
                nonNullRing++;
            }
        }
        int heldV = 0;
        for (Holder h : hs) {
            if (h.ref == v) {
                heldV++;
            }
        }
        System.out.println("check cost ring_nonnull=" + nonNullRing + " holders_v=" + heldV
                + " ok=" + (nonNullRing == 0 && heldV == 1024));
    }

    // ---- snap -------------------------------------------------------------

    static Object sink;

    // One move: take the node out of a, put it in b (or chain it under the
    // node already there), then clear its old slot. The store into its new
    // home comes FIRST and the overwrite of its old one SECOND, so only the
    // pre-barrier on that overwrite keeps it in a snapshot taken before the
    // move whose marker had already passed the new home.
    static void move(Object[] a, Object[] b, int i, int j) {
        Node x = (Node) a[i];
        if (x == null) {
            return;
        }
        Node y = (Node) b[j];
        if (y == null) {
            b[j] = x;
        } else {
            Node tail = y;
            while (tail.next != null) {
                tail = tail.next;
            }
            Node rest = x.next;
            x.next = null;
            tail.next = x;
            a[i] = rest;
            return;
        }
        a[i] = x.next;
        x.next = null;
    }

    static void snap(String[] args) {
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 200;
        int nodes = args.length > 2 ? Integer.parseInt(args[2]) : 20000;
        int width = Math.max(16, nodes / 4);
        Object[] a = new Object[width];
        Object[] b = new Object[width];
        for (int id = 0; id < nodes; id++) {
            int slot = id % width;
            Node nd = new Node(id);
            nd.next = (Node) a[slot];
            a[slot] = nd;
        }
        long seed = 0x5DEECE66DL;
        long moves = 0;
        for (int r = 0; r < rounds; r++) {
            for (int k = 0; k < width; k++) {
                seed = seed * 6364136223846793005L + 1442695040888963407L;
                int i = (int) ((seed >>> 33) % width);
                int j = (int) ((seed >>> 13) % width);
                if ((r & 1) == 0) {
                    move(a, b, i, j);
                } else {
                    move(b, a, j, i);
                }
                moves++;
                // Garbage, to keep collections and marks coming.
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
                for (Node nd = (Node) o; nd != null; nd = nd.next) {
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
